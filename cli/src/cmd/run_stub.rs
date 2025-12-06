use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MountType {
    Bind { src: PathBuf },
    Sqlite { src: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    pub mount_type: MountType,
    pub dst: PathBuf,
}

impl std::str::FromStr for MountConfig {
    type Err = String;

    fn from_str(_s: &str) -> Result<Self, Self::Err> {
        Err("Mount configuration is only supported on Linux".to_string())
    }
}

pub async fn run_sandbox(
    mounts: Vec<MountConfig>,
    strace: bool,
    command: PathBuf,
    args: Vec<String>,
) {
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
