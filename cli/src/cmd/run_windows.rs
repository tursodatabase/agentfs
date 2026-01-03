//! Windows run command implementation.
//!
//! The `run` command is not supported on Windows.

use anyhow::{bail, Result};
use std::path::PathBuf;

/// Run the command in a Windows sandbox.
pub async fn run(
    _allow: Vec<PathBuf>,
    _no_default_allows: bool,
    _experimental_sandbox: bool,
    _strace: bool,
    _session: Option<String>,
    _command: PathBuf,
    _args: Vec<String>,
) -> Result<()> {
    bail!("The `run` command is not supported on Windows")
}
