use agentfs_sdk::{AgentFS, AgentFSOptions};
use anyhow::Result;
use std::{os::unix::fs::MetadataExt, path::PathBuf};

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
}

/// Mount the agent filesystem using FUSE.
pub fn mount(args: MountArgs) -> Result<()> {
    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    let fsname = std::fs::canonicalize(&args.id_or_path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| args.id_or_path.clone());

    if !args.mountpoint.exists() {
        anyhow::bail!("Mountpoint does not exist: {}", args.mountpoint.display());
    }

    let mountpoint = args.mountpoint.clone();

    let fuse_opts = FuseMountOptions {
        mountpoint: args.mountpoint,
        auto_unmount: args.auto_unmount,
        allow_root: args.allow_root,
        fsname,
    };

    let mount = move || {
        let rt = tokio::runtime::Runtime::new()?;
        let agentfs = rt.block_on(AgentFS::open(opts))?;
        crate::fuse::mount(agentfs, fuse_opts, rt)
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
