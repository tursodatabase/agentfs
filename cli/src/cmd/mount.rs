use agentfs_sdk::{error::Error as SdkError, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use tokio::sync::Mutex;
use turso::value::Value;

use crate::mount::{mount_fs, MountOpts};
use crate::nfs::AgentNFS;
use crate::nfsserve::tcp::NFSTcp;

#[cfg(target_os = "linux")]
use agentfs_sdk::{get_mounts, Mount};
#[cfg(target_os = "linux")]
use std::{
    io::{self, Write},
    os::unix::fs::MetadataExt,
};

#[cfg(target_os = "linux")]
use crate::cmd::init::open_agentfs;
#[cfg(target_os = "linux")]
use crate::fuse::FuseMountOptions;

pub use crate::opts::MountBackend;

/// Default NFS port to try (use a high port to avoid needing root)
const DEFAULT_NFS_PORT: u32 = 11111;

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
    /// Allow other system users to access the mount.
    pub allow_other: bool,
    /// Run in foreground (don't daemonize).
    pub foreground: bool,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
    /// The mount backend to use (fuse or nfs).
    pub backend: MountBackend,
}

/// Mount the agent filesystem (Linux).
#[cfg(target_os = "linux")]
pub fn mount(args: MountArgs) -> Result<()> {
    match args.backend {
        MountBackend::Fuse => mount_fuse(args),
        MountBackend::Nfs => {
            let rt = crate::get_runtime();
            rt.block_on(mount_nfs_backend(args))
        }
    }
}

/// Mount the agent filesystem (macOS).
#[cfg(target_os = "macos")]
pub fn mount(args: MountArgs) -> Result<()> {
    match args.backend {
        MountBackend::Fuse => {
            anyhow::bail!(
                "FUSE mounting is not supported on macOS.\n\
                 Use --backend nfs (default) or `agentfs nfs` instead."
            );
        }
        MountBackend::Nfs => {
            let rt = crate::get_runtime();
            rt.block_on(mount_nfs_backend(args))
        }
    }
}

/// Mount the agent filesystem using FUSE (Linux only).
#[cfg(target_os = "linux")]
fn mount_fuse(args: MountArgs) -> Result<()> {
    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    // Check schema version before daemonizing. This allows us to show the error
    // message to the user directly, rather than having it appear in daemon logs.
    {
        let rt = crate::get_runtime();
        let db_path = opts.db_path()?;
        let result: Result<(), SdkError> = rt.block_on(async {
            let db = turso::Builder::new_local(&db_path).build().await?;
            let conn = db.connect()?;
            agentfs_sdk::schema::check_schema_version(&conn).await?;
            Ok(())
        });
        if let Err(SdkError::SchemaVersionMismatch { found, expected }) = result {
            exit_schema_version_mismatch(&found, &expected, &args.id_or_path);
        }
    }

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
        use anyhow::Context as _;
        std::fs::metadata(mountpoint.clone())
            .context("Failed to get mountpoint inode")?
            .ino()
    };

    let fuse_opts = FuseMountOptions {
        mountpoint: args.mountpoint.clone(),
        auto_unmount: args.auto_unmount,
        allow_root: args.allow_root,
        allow_other: args.allow_other,
        fsname,
        uid: args.uid,
        gid: args.gid,
    };

    let id_or_path = args.id_or_path.clone();
    let mount = move || {
        let rt = crate::get_runtime();
        let agentfs = match rt.block_on(open_agentfs(opts)) {
            Ok(fs) => fs,
            Err(SdkError::SchemaVersionMismatch { found, expected }) => {
                exit_schema_version_mismatch(&found, &expected, &id_or_path);
            }
            Err(e) => return Err(e.into()),
        };

        // Check for overlay configuration
        let fs: Arc<dyn FileSystem> = rt.block_on(async {
            // Query base_path in a separate scope so connection is released
            let base_path: Option<String> = {
                let conn = agentfs.get_connection().await?;
                let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
                match conn.query(query, ()).await {
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
                }
            }; // conn is dropped here

            if let Some(base_path) = base_path {
                // Create OverlayFS with HostFS base, loading existing whiteouts
                eprintln!("Using overlay filesystem with base: {}", base_path);
                let hostfs = HostFS::new(&base_path)?;
                let hostfs = hostfs.with_fuse_mountpoint(mountpoint_ino);
                let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);
                overlay.load().await?; // Load persisted whiteouts and origin mappings
                Ok::<Arc<dyn FileSystem>, anyhow::Error>(Arc::new(overlay))
            } else {
                // Plain AgentFS
                Ok(Arc::new(agentfs.fs) as Arc<dyn FileSystem>)
            }
        })?;

        crate::fuse::mount(fs, fuse_opts, rt)
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

/// Mount the agent filesystem using NFS over localhost.
async fn mount_nfs_backend(args: MountArgs) -> Result<()> {
    use crate::cmd::init::open_agentfs;

    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    if !args.mountpoint.exists() {
        anyhow::bail!("Mountpoint does not exist: {}", args.mountpoint.display());
    }

    let mountpoint = std::fs::canonicalize(args.mountpoint.clone())?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&args.id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| args.id_or_path.clone())
    );

    // Open AgentFS
    let agentfs = match open_agentfs(opts).await {
        Ok(fs) => fs,
        Err(SdkError::SchemaVersionMismatch { found, expected }) => {
            exit_schema_version_mismatch(&found, &expected, &args.id_or_path);
        }
        Err(e) => return Err(e.into()),
    };

    // Check for overlay configuration
    // Query base_path in a separate scope so connection is released before load_whiteouts
    let base_path: Option<String> = {
        let conn = agentfs.get_connection().await?;
        let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
        match conn.query(query, ()).await {
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
        }
    }; // conn is dropped here

    let fs: Arc<Mutex<dyn FileSystem + Send>> = if let Some(base_path) = base_path {
        // Create OverlayFS with HostFS base, loading existing whiteouts
        eprintln!("Using overlay filesystem with base: {}", base_path);
        let hostfs = HostFS::new(&base_path)?;
        let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);
        overlay.load().await?; // Load persisted whiteouts and origin mappings
        Arc::new(Mutex::new(overlay)) as Arc<Mutex<dyn FileSystem + Send>>
    } else {
        // Plain AgentFS
        Arc::new(Mutex::new(agentfs.fs)) as Arc<Mutex<dyn FileSystem + Send>>
    };

    if args.foreground {
        // Use the unified mount API for foreground mode
        let mount_opts = MountOpts {
            mountpoint: mountpoint.clone(),
            backend: MountBackend::Nfs,
            fsname,
            uid: args.uid,
            gid: args.gid,
            allow_other: args.allow_other,
            allow_root: args.allow_root,
            auto_unmount: args.auto_unmount,
            lazy_unmount: true,
            timeout: std::time::Duration::from_secs(10),
        };

        let _mount_handle = mount_fs(fs, mount_opts).await?;

        eprintln!("Mounted at {}", mountpoint.display());
        eprintln!("Press Ctrl+C to unmount and exit.");
        tokio::signal::ctrl_c().await?;

        // Handle drops automatically when we exit this scope
    } else {
        // Daemon mode: use manual NFS server setup for persistent background operation
        let nfs = AgentNFS::new(fs);
        let port = find_available_port(DEFAULT_NFS_PORT)?;

        let bind_addr = format!("127.0.0.1:{}", port);
        let listener = crate::nfsserve::tcp::NFSTcpListener::bind(&bind_addr, nfs)
            .await
            .context("Failed to bind NFS server")?;

        eprintln!("Starting NFS server on 127.0.0.1:{}", port);

        tokio::spawn(async move {
            if let Err(e) = listener.handle_forever().await {
                eprintln!("NFS server error: {}", e);
            }
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        nfs_mount(port, &mountpoint)?;

        eprintln!("Mounted at {}", mountpoint.display());
        eprintln!(
            "Running in background. Use 'umount {}' to unmount.",
            mountpoint.display()
        );

        // Block forever (server runs in background task)
        std::future::pending::<()>().await;
    }

    Ok(())
}

/// Find an available TCP port starting from the given port.
fn find_available_port(start_port: u32) -> Result<u32> {
    for port in start_port..start_port + 100 {
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "Could not find an available port in range {}-{}",
        start_port,
        start_port + 100
    );
}

/// Mount the NFS filesystem (Linux version).
#[cfg(target_os = "linux")]
fn nfs_mount(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("mount")
        .args([
            "-t",
            "nfs",
            "-o",
            &format!(
                "vers=3,tcp,port={},mountport={},nolock,soft,timeo=10,retrans=2",
                port, port
            ),
            "127.0.0.1:/",
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to mount NFS: {}. Make sure NFS client tools are installed (nfs-common on Debian/Ubuntu, nfs-utils on Fedora/RHEL) and you have permission to mount (try running with sudo).",
            stderr.trim()
        );
    }

    Ok(())
}

/// Mount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn nfs_mount(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!(
                "locallocks,vers=3,tcp,port={},mountport={},soft,timeo=10,retrans=2",
                port, port
            ),
            "127.0.0.1:/",
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount_nfs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to mount NFS: {}", stderr.trim());
    }

    Ok(())
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

/// List all currently mounted agentfs filesystems (Linux)
#[cfg(target_os = "linux")]
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

/// List all currently mounted agentfs filesystems (macOS stub)
#[cfg(target_os = "macos")]
pub fn list_mounts<W: std::io::Write>(out: &mut W) {
    let _ = writeln!(out, "Mount listing is only available on Linux.");
}

/// Check if a mount point is in use by any process.
///
/// Scans /proc to find processes with open files or current working directory
/// on the given mountpoint.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

/// Prune unused agentfs mount points (macOS stub).
#[cfg(target_os = "macos")]
pub fn prune_mounts(_force: bool) -> Result<()> {
    anyhow::bail!("Mount pruning is only available on Linux")
}

/// Print schema version mismatch error and exit.
fn exit_schema_version_mismatch(found: &str, expected: &str, id_or_path: &str) -> ! {
    eprintln!("Error: Filesystem `{}` requires migration", id_or_path);
    eprintln!();
    eprintln!(
        "Found schema version {}, but this version of agentfs requires {}.",
        found, expected
    );
    eprintln!();
    eprintln!("To upgrade, run:");
    eprintln!();
    eprintln!("    agentfs migrate {}", id_or_path);
    eprintln!();
    std::process::exit(1);
}
