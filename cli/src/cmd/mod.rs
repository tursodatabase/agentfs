#[cfg(target_os = "linux")]
mod mount;
#[cfg(not(target_os = "linux"))]
#[path = "mount_stub.rs"]
mod mount;

#[cfg(feature = "sandbox")]
mod run;
#[cfg(not(feature = "sandbox"))]
#[path = "run_stub.rs"]
mod run;

use std::path::PathBuf;

// Import MountConfig from the appropriate source
#[cfg(feature = "sandbox")]
pub use agentfs_sandbox::MountConfig;

#[cfg(not(feature = "sandbox"))]
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
