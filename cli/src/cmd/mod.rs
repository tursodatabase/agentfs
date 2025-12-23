pub mod completions;
pub mod fs;
pub mod init;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod mount;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "mount_stub.rs"]
mod mount;

// Run module selection:
// - Linux x86_64: use overlay sandbox (run.rs)
// - macOS: use NFS-based sandbox (run_nfs.rs)
// - Other platforms: use stub (run_stub.rs)

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod run;

#[cfg(target_os = "macos")]
#[path = "run_nfs.rs"]
mod run;

#[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
#[path = "run_stub.rs"]
mod run;

// Standalone NFS server command (Unix only)
#[cfg(unix)]
pub mod nfs;

pub use mount::{mount, MountArgs};
pub use run::handle_run_command;
