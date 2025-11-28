use anyhow::Result;
use std::path::PathBuf;

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
pub fn mount(_args: MountArgs) -> Result<()> {
    anyhow::bail!("FUSE mount is only available on Linux")
}
