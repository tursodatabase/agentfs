#[cfg(target_os = "linux")]
mod mount;
#[cfg(not(target_os = "linux"))]
#[path = "mount_stub.rs"]
mod mount;

#[cfg(target_os = "linux")]
mod run_linux;

use std::path::PathBuf;

// Import MountConfig from the appropriate source
#[cfg(target_os = "linux")]
pub use agentfs_sandbox::MountConfig;

#[cfg(not(target_os = "linux"))]
pub use crate::non_linux::MountConfig;

pub async fn handle_run_command(
    mounts: Vec<MountConfig>,
    strace: bool,
    command: PathBuf,
    args: Vec<String>,
) {
    #[cfg(target_os = "linux")]
    {
        run_linux::run_sandbox(mounts, strace, command, args).await;
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Suppress unused variable warnings on non-Linux platforms
        let _ = (mounts, strace, command, args);

        eprintln!("Error: Sandbox is available only on Linux.");
        eprintln!();
        eprintln!("The 'run' command uses ptrace-based system call interception,");
        eprintln!("which is only supported on Linux.");
        eprintln!();
        eprintln!("However, you can still use the other AgentFS commands:");
        eprintln!("  - 'agentfs init' to create a new agent filesystem");
        eprintln!("  - 'agentfs fs ls' to list files");
        eprintln!("  - 'agentfs fs cat' to view file contents");
        std::process::exit(1);
    }
}

pub use mount::{mount, MountArgs};
