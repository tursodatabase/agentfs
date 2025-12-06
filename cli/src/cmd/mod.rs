#[cfg(target_os = "linux")]
mod mount;
#[cfg(not(target_os = "linux"))]
#[path = "mount_stub.rs"]
mod mount;

#[cfg(target_os = "linux")]
mod run;
#[cfg(not(target_os = "linux"))]
#[path = "run_stub.rs"]
mod run;

use std::path::PathBuf;

// Import MountConfig from the appropriate source
#[cfg(target_os = "linux")]
pub use agentfs_sandbox::MountConfig;

#[cfg(not(target_os = "linux"))]
pub use run::MountConfig;

pub async fn handle_run_command(
    mounts: Vec<MountConfig>,
    strace: bool,
    command: PathBuf,
    args: Vec<String>,
) {
    run::run_sandbox(mounts, strace, command, args).await;
}

pub use mount::{mount, MountArgs};
