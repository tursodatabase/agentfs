//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox (or experimental ptrace)
//! - Darwin: NFS + sandbox-exec

use anyhow::Result;
use std::path::PathBuf;

#[cfg_attr(target_os = "linux", path = "run_linux.rs")]
#[cfg_attr(target_os = "macos", path = "run_darwin.rs")]
#[cfg_attr(target_os = "windows", path = "run_windows.rs")]
mod sys;

/// Handle the `run` command, dispatching to the platform-specific implementation.
pub async fn handle_run_command(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    experimental_sandbox: bool,
    strace: bool,
    session: Option<String>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    sys::run(
        allow,
        no_default_allows,
        experimental_sandbox,
        strace,
        session,
        command,
        args,
    )
    .await
}
