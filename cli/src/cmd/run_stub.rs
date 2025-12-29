//! Stub implementations for non-Linux platforms.

use anyhow::Result;
use std::path::PathBuf;

/// Handle the `run` command - stub for non-Linux platforms.
pub async fn handle_run_command(
    _allow: Vec<PathBuf>,
    _no_default_allows: bool,
    _experimental_sandbox: bool,
    _strace: bool,
    _session: Option<String>,
    _command: PathBuf,
    _args: Vec<String>,
) -> Result<()> {
    eprintln!(
        "Error: The 'run' command is only available on Linux (on ARM requires libunwind-dev)."
    );
    eprintln!();
    eprintln!("However, you can still use the other AgentFS commands:");
    eprintln!("  - 'agentfs init' to create a new agent filesystem");
    eprintln!("  - 'agentfs fs ls' to list files");
    eprintln!("  - 'agentfs fs cat' to view file contents");
    std::process::exit(1);
}
