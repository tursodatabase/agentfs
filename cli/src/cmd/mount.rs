use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::Result;
use std::{os::unix::fs::MetadataExt, path::PathBuf, sync::Arc};
use turso::Value;

use crate::fuse::FuseMountOptions;

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
pub fn mount(args: MountArgs) -> Result<()> {
    if !supports_fuse() {
        #[cfg(target_os = "macos")]
        {
            anyhow::bail!(
                "macFUSE is not installed. Please install it from https://osxfuse.github.io/ \n\
                 or via Homebrew: brew install --cask macfuse"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!("FUSE is not available on this system");
        }
    }

    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    let fsname = std::fs::canonicalize(&args.id_or_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| args.id_or_path.clone());

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
        let agentfs = rt.block_on(AgentFS::open(opts))?;

        // Check for overlay configuration
        let fs: Arc<dyn FileSystem> = rt.block_on(async {
            let conn = agentfs.get_connection();

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

/// Check if a path is a mountpoint by comparing device IDs
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

/// Check if macOS system supports FUSE.
///
/// The `libfuse` dynamic library is weakly linked so that users who don't have
/// macFUSE installed can still run the other commands.
#[cfg(target_os = "macos")]
fn supports_fuse() -> bool {
    for lib_name in &[
        c"/usr/local/lib/libfuse.2.dylib",
        c"/usr/local/lib/libfuse.dylib",
    ] {
        let handle = unsafe { libc::dlopen(lib_name.as_ptr(), libc::RTLD_LAZY) };
        if !handle.is_null() {
            unsafe { libc::dlclose(handle) };
            return true;
        }
    }
    false
}

/// Check if Linux system supports FUSE.
///
/// The `fuser` crate does not even need `libfuse` so technically it always support FUSE.
/// Of course, if FUSE is disabled in the kernel, we'll get an error, but that's life.
#[cfg(target_os = "linux")]
fn supports_fuse() -> bool {
    true
}
