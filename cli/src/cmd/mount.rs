use agentfs_sdk::{get_mounts, Mount};
use anyhow::Result;
use std::{
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use agentfs_sdk::{AgentFSOptions, FileSystem, HostFS, OverlayFS};
use turso::value::Value;

#[cfg(target_os = "linux")]
use crate::{cmd::init::open_agentfs, fuse::FuseMountOptions};

/// Arguments for the mount command.
#[derive(Debug, Clone)]
pub struct MountArgs {
    /// The agent filesystem ID or path.
    pub id_or_path: String,
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Automatically unmount when the process exits.
    pub auto_unmount: bool,
    /// Allow root to access the mount.
    pub allow_root: bool,
    /// Run in foreground (don't daemonize).
    pub foreground: bool,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
}

/// Mount the agent filesystem using FUSE.
#[cfg(target_os = "linux")]
pub fn mount(args: MountArgs) -> Result<()> {
    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&args.id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| args.id_or_path.clone())
    );

    if !args.mountpoint.exists() {
        anyhow::bail!("Mountpoint does not exist: {}", args.mountpoint.display());
    }

    let mountpoint = std::fs::canonicalize(args.mountpoint.clone())?;
    let mountpoint_ino = {
        #[cfg(target_family = "unix")]
        {
            use anyhow::Context as _;
            std::fs::metadata(mountpoint.clone())
                .context("Failed to get mountpoint inode")?
                .ino()
        }
        #[cfg(not(target_family = "unix"))]
        {
            // Should be impossible to reach this path
            return Err(anyhow::anyhow!(
                "FUSE mountpoint inode is not supported on this platform"
            ));
        }
    };

    let fuse_opts = FuseMountOptions {
        mountpoint: args.mountpoint,
        auto_unmount: args.auto_unmount,
        allow_root: args.allow_root,
        fsname,
        uid: args.uid,
        gid: args.gid,
    };

    let mount = move || {
        let rt = crate::get_runtime();
        let (_db, agentfs) = rt.block_on(open_agentfs(opts))?;

        // Check for overlay configuration
        let fs: Arc<dyn FileSystem> = rt.block_on(async {
            let conn = agentfs.get_conn().await?;

            // Check if fs_overlay_config table exists and has base_path
            let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
            let base_path: Option<String> = match conn.query(query, ()).await {
                Ok(mut rows) => {
                    if let Ok(Some(row)) = rows.next().await {
                        row.get_value(0).ok().and_then(|v| {
                            if let Value::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    }
                }
                Err(_) => None, // Table doesn't exist or query failed
            };

            if let Some(base_path) = base_path {
                // Create OverlayFS with HostFS base
                eprintln!("Using overlay filesystem with base: {}", base_path);
                let hostfs = HostFS::new(&base_path)?;
                #[cfg(target_family = "unix")]
                let hostfs = { hostfs.with_fuse_mountpoint(mountpoint_ino) };
                let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);
                Ok::<Arc<dyn FileSystem>, anyhow::Error>(Arc::new(overlay))
            } else {
                // Plain AgentFS
                Ok(Arc::new(agentfs.fs) as Arc<dyn FileSystem>)
            }
        })?;

        crate::fuse::mount(fs, fuse_opts, &rt)
    };

    if args.foreground {
        mount()
    } else {
        crate::daemon::daemonize(
            mount,
            move || is_mounted(&mountpoint),
            std::time::Duration::from_secs(10),
        )
    }
}

/// Mount the agent filesystem using FUSE (macOS - not supported).
#[cfg(target_os = "macos")]
pub fn mount(_args: MountArgs) -> Result<()> {
    anyhow::bail!(
        "FUSE mounting is not supported on macOS in this version.\n\
         Use `agentfs nfs` to mount via NFS instead."
    );
}

/// Check if a path is a mountpoint by comparing device IDs
#[cfg(target_os = "linux")]
fn is_mounted(path: &std::path::Path) -> bool {
    let path_meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("/"),
    };

    let parent_meta = match std::fs::metadata(parent) {
        Ok(m) => m,
        Err(_) => return false,
    };

    // Different device IDs means it's a mountpoint
    path_meta.dev() != parent_meta.dev()
}

/// List all currently mounted agentfs filesystems
pub fn list_mounts<W: Write>(out: &mut W) {
    let mounts = get_mounts();

    if mounts.is_empty() {
        let _ = writeln!(out, "No agentfs filesystems mounted.");
        return;
    }

    // Calculate column widths
    let id_width = mounts.iter().map(|m| m.id.len()).max().unwrap_or(2).max(2);
    let mount_width = mounts
        .iter()
        .map(|m| m.mountpoint.to_string_lossy().len())
        .max()
        .unwrap_or(10)
        .max(10);

    // Print header
    let _ = writeln!(
        out,
        "{:<id_width$}  {:<mount_width$}",
        "ID",
        "MOUNTPOINT",
        id_width = id_width,
        mount_width = mount_width
    );

    // Print mounts
    for mount in &mounts {
        let _ = writeln!(
            out,
            "{:<id_width$}  {:<mount_width$}",
            mount.id,
            mount.mountpoint.display(),
            id_width = id_width,
            mount_width = mount_width
        );
    }
}

/// Check if a mount point is in use by any process.
///
/// Scans /proc to find processes with open files or current working directory
/// on the given mountpoint.
fn is_mount_in_use(mountpoint: &Path) -> bool {
    let mountpoint = match mountpoint.canonicalize() {
        Ok(p) => p,
        Err(_) => return false, // Can't check, assume not in use
    };

    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(dir) => dir,
        Err(_) => return false,
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only check numeric directories (PIDs)
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let pid_path = entry.path();

        // Check cwd
        if let Ok(cwd) = std::fs::read_link(pid_path.join("cwd")) {
            if cwd.starts_with(&mountpoint) {
                return true;
            }
        }

        // Check open file descriptors
        let fd_dir = pid_path.join("fd");
        if let Ok(fds) = std::fs::read_dir(&fd_dir) {
            for fd_entry in fds.flatten() {
                if let Ok(target) = std::fs::read_link(fd_entry.path()) {
                    if target.starts_with(&mountpoint) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Unmount a FUSE filesystem.
///
/// Tries fusermount3 first, then falls back to fusermount.
fn unmount_fuse(mountpoint: &Path) -> Result<()> {
    const FUSERMOUNT_COMMANDS: &[&str] = &["fusermount3", "fusermount"];

    for cmd in FUSERMOUNT_COMMANDS {
        let result = std::process::Command::new(cmd)
            .args(["-u"])
            .arg(mountpoint.as_os_str())
            .status();

        match result {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => continue,  // Command ran but failed, try next
            Err(_) => continue, // Command not found, try next
        }
    }

    anyhow::bail!(
        "Failed to unmount {}. You may need to unmount manually with: fusermount -u {}",
        mountpoint.display(),
        mountpoint.display()
    )
}

/// Ask for user confirmation.
fn confirm(prompt: &str) -> bool {
    eprint!("{} ", prompt);
    let _ = io::stderr().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Prune unused agentfs mount points.
///
/// Finds all mounted agentfs filesystems that are not in use by any process
/// and unmounts them.
pub fn prune_mounts(force: bool) -> Result<()> {
    let mounts = get_mounts();

    // Get active session IDs to exclude from pruning
    let active_sessions = super::ps::active_session_ids();

    // Find unused mounts (not in use by any process and no active session)
    let unused_mounts: Vec<&Mount> = mounts
        .iter()
        .filter(|m| !is_mount_in_use(&m.mountpoint) && !active_sessions.contains(&m.id))
        .collect();

    if unused_mounts.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // Display what will be unmounted
    println!("The following unused mount points will be unmounted:");
    println!();
    for mount in &unused_mounts {
        println!("  {} -> {}", mount.id, mount.mountpoint.display());
    }
    println!();

    // Ask for confirmation unless --force
    if !force && !confirm("Are you sure? (y/N)") {
        println!("Aborted.");
        return Ok(());
    }

    // Unmount each unused mount
    let mut errors = Vec::new();
    for mount in &unused_mounts {
        print!("Unmounting {}... ", mount.mountpoint.display());
        let _ = io::stdout().flush();

        match unmount_fuse(&mount.mountpoint) {
            Ok(()) => println!("done"),
            Err(e) => {
                println!("failed");
                errors.push(format!("{}: {}", mount.mountpoint.display(), e));
            }
        }
    }

    if !errors.is_empty() {
        eprintln!();
        eprintln!("Some mounts could not be unmounted:");
        for error in &errors {
            eprintln!("  {}", error);
        }
        anyhow::bail!("Failed to unmount {} mount(s)", errors.len());
    }

    Ok(())
}
