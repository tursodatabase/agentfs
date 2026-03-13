pub mod completions;
pub mod fs;
pub mod init;
pub mod mcp_server;
pub mod migrate;
pub mod ps;
pub mod sync;
pub mod timeline;

#[cfg(any(unix, target_os = "windows"))]
pub mod mount;
#[cfg(not(any(unix, target_os = "windows")))]
#[path = "mount_stub.rs"]
pub mod mount;

mod run;

// Standalone NFS server command (Unix only)
#[cfg(unix)]
pub mod nfs;

// Exec command (Unix only)
#[cfg(unix)]
pub mod exec;

pub use mount::{mount, MountArgs, MountBackend};
pub use run::handle_run_command;
