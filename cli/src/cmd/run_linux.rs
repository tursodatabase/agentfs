//! Linux run command implementation.
//!
//! Dispatches to either the FUSE+namespace sandbox (default) or the experimental
//! ptrace-based sandbox based on command-line flags.

use anyhow::Result;
use std::path::PathBuf;

/// Run the command in a Linux sandbox.
pub async fn run(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    experimental_sandbox: bool,
    strace: bool,
    session: Option<String>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    if experimental_sandbox {
        if !allow.is_empty() || no_default_allows {
            eprintln!("Warning: --allow and --no-default-allows are not supported with --experimental-sandbox, ignoring");
        }
        if session.is_some() {
            eprintln!("Warning: --session is not supported with --experimental-sandbox, ignoring");
        }
        crate::sandbox::linux_ptrace::run_cmd(strace, command, args).await;
    } else {
        if strace {
            eprintln!("Warning: --strace is only supported with --experimental-sandbox, ignoring");
        }
        crate::sandbox::linux::run_cmd(allow, no_default_allows, session, command, args).await?;
    }
    Ok(())
}
