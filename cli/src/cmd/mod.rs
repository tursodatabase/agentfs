pub mod fs;
pub mod init;

#[cfg(target_os = "linux")]
mod mount;
#[cfg(not(target_os = "linux"))]
#[path = "mount_stub.rs"]
mod mount;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod run;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[path = "run_stub.rs"]
mod run;

use std::path::PathBuf;

// Import MountConfig from the appropriate source
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use agentfs_sandbox::MountConfig;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
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
