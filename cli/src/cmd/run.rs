//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox (or experimental ptrace)
//! - Darwin: NFS + sandbox-exec

use anyhow::Result;
use std::path::PathBuf;

#[cfg_attr(all(target_os = "linux", feature = "sandbox"), path = "run_linux.rs")]
#[cfg_attr(all(target_os = "macos", feature = "sandbox"), path = "run_darwin.rs")]
#[cfg_attr(
    all(target_os = "windows", feature = "sandbox"),
    path = "run_windows.rs"
)]
#[cfg_attr(not(feature = "sandbox"), path = "run_not_supported.rs")]
mod sys;

/// Handle the `run` command, dispatching to the platform-specific implementation.
#[allow(clippy::too_many_arguments)]
pub async fn handle_run_command(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    experimental_sandbox: bool,
    strace: bool,
    session: Option<String>,
    system: bool,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    sys::run(
        allow,
        no_default_allows,
        experimental_sandbox,
        strace,
        session,
        system,
        command,
        args,
    )
    .await
}
