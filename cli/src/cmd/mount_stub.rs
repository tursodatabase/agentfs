use anyhow::Result;
use std::{io::Write, path::PathBuf};

/// Arguments for the mount command.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
}

/// List all currently mounted agentfs filesystems
pub fn list_mounts<W: Write>(out: &mut W) {
    let _ = writeln!(out, "Mount listing is only available on Linux.");
}

/// Mount the agent filesystem using FUSE.
pub fn mount(_args: MountArgs) -> Result<()> {
    anyhow::bail!("FUSE mount is only available on Linux")
}

/// Prune unused agentfs mount points.
pub fn prune_mounts(_force: bool) -> Result<()> {
    anyhow::bail!("Mount pruning is only available on Linux")
}
