//! Generic mount infrastructure for AgentFS.
//!
//! This module provides a unified mount API that abstracts over FUSE and NFS backends.
//! The `mount_fs()` function returns a `MountHandle` that automatically unmounts when dropped.
//!
//! # Example
//!
//! ```ignore
//! use agentfs_cli::mount::{mount_fs, MountOpts, MountBackend};
//!
//! let opts = MountOpts::new(PathBuf::from("/mnt/agent"), MountBackend::Fuse);
//! let handle = mount_fs(Arc::new(Mutex::new(my_fs)), opts).await?;
//! // ... use the mounted filesystem ...
//! drop(handle); // auto-unmounts
//! ```

use crate::opts::MountBackend;

#[cfg(target_os = "windows")]
pub mod winfsp;

#[cfg(target_os = "linux")]
mod fuse;

#[cfg(unix)]
use super::nfs;
#[cfg(unix)]
use tokio::sync::Mutex;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;

/// Default timeout for mount to become ready.
const DEFAULT_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// Options for mounting a filesystem.
///
/// This struct provides a unified configuration for both FUSE and NFS backends.
/// Use `MountOpts::new()` to create default options, then customize as needed.
#[derive(Debug, Clone)]
pub struct MountOpts {
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Mount backend to use.
    pub backend: MountBackend,
    /// Filesystem name shown in mount output.
    pub fsname: String,
    /// User ID to report for all files.
    pub uid: Option<u32>,
    /// Group ID to report for all files.
    pub gid: Option<u32>,
    /// Allow other system users to access the mount.
    pub allow_other: bool,
    /// Allow root to access the mount (FUSE only).
    pub allow_root: bool,
    /// Auto unmount when process exits (FUSE only).
    pub auto_unmount: bool,
    /// Use lazy unmount on cleanup.
    pub lazy_unmount: bool,
    /// Timeout for mount to become ready.
    pub timeout: Duration,
}

impl MountOpts {
    /// Create default options for the given mountpoint and backend.
    pub fn new(mountpoint: PathBuf, backend: MountBackend) -> Self {
        Self {
            mountpoint,
            backend,
            fsname: "agentfs".to_string(),
            uid: None,
            gid: None,
            allow_other: false,
            allow_root: false,
            auto_unmount: false,
            lazy_unmount: false,
            timeout: DEFAULT_MOUNT_TIMEOUT,
        }
    }
}

impl Default for MountOpts {
    fn default() -> Self {
        Self::new(PathBuf::new(), MountBackend::default())
    }
}

/// A mounted filesystem handle. Automatically unmounts when dropped.
///
/// This handle represents an active mount and provides RAII-style cleanup.
/// When the handle is dropped, the filesystem is automatically unmounted.
pub struct MountHandle {
    mountpoint: PathBuf,
    backend: MountBackend,
    lazy_unmount: bool,
    inner: MountHandleInner,
}

pub(crate) enum MountHandleInner {
    #[cfg(target_os = "linux")]
    Fuse {
        _thread: std::thread::JoinHandle<anyhow::Result<()>>,
    },
    #[cfg(unix)]
    Nfs {
        shutdown: CancellationToken,
        _server_handle: tokio::task::JoinHandle<()>,
    },
    #[cfg(target_os = "windows")]
    WinFsp {
        host_ptr: *mut (),
    },
}

impl MountHandle {
    /// Get the mountpoint path.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // Move away from mountpoint before unmounting to avoid EBUSY
        #[cfg(unix)]
        let _ = std::env::set_current_dir("/");

        match &self.inner {
            #[cfg(target_os = "linux")]
            MountHandleInner::Fuse { .. } => {
                if let Err(e) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                    eprintln!(
                        "Warning: Failed to unmount FUSE filesystem at {}: {}",
                        self.mountpoint.display(),
                        e
                    );
                }
            }
            #[cfg(unix)]
            MountHandleInner::Nfs { shutdown, .. } => {
                // Signal the NFS server to shut down
                shutdown.cancel();

                // Unmount the NFS filesystem
                if let Err(e) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                    eprintln!(
                        "Warning: Failed to unmount NFS filesystem at {}: {}",
                        self.mountpoint.display(),
                        e
                    );
                }
            }
            #[cfg(target_os = "windows")]
            MountHandleInner::WinFsp { host_ptr } => {
                // WinFsp filesystem stops and unmounts when we reconstruct the Box and drop it
                if let Err(e) = winfsp::unmount_winfsp(*host_ptr) {
                    eprintln!(
                        "Warning: Failed to unmount WinFsp filesystem at {}: {}",
                        self.mountpoint.display(),
                        e
                    );
                }
            }
        }
    }
}

/// Unmount a filesystem at the given mountpoint.
///
/// This function handles unmounting for both FUSE and NFS backends.
/// If `lazy` is true, uses lazy unmount which detaches immediately even if busy.
pub fn unmount(mountpoint: &Path, backend: MountBackend, lazy: bool) -> Result<()> {
    match backend {
        #[cfg(target_os = "linux")]
        MountBackend::Fuse => fuse::unmount_fuse(mountpoint, lazy),
        #[cfg(not(target_os = "linux"))]
        MountBackend::Fuse => anyhow::bail!("FUSE is not supported on this platform"),
        #[cfg(target_os = "windows")]
        MountBackend::Nfs => anyhow::bail!("NFS mounting is not supported on Windows. Use --backend winfsp instead."),
        #[cfg(all(unix, not(target_os = "linux")))]
        MountBackend::Nfs => nfs::unmount_nfs(mountpoint, lazy),
        #[cfg(target_os = "windows")]
        MountBackend::Winfsp => {
            // WinFsp unmounting is handled by dropping the MountHandle
            // External unmount is not supported - use drop(handle) instead
            anyhow::bail!(
                "WinFsp external unmount is not supported. \
                 Drop the MountHandle to unmount the filesystem."
            );
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        _ => anyhow::bail!("Unsupported mount backend for this platform"),
    }
}

/// Mount a filesystem with the given options.
///
/// Returns a handle that automatically unmounts when dropped.
/// The filesystem must be wrapped in `Arc<Mutex<dyn FileSystem + Send>>`.
#[cfg(target_os = "linux")]
pub async fn mount_fs(
    fs: Arc<Mutex<dyn agentfs_sdk::FileSystem + Send>>,
    opts: MountOpts,
) -> Result<MountHandle> {
    match opts.backend {
        MountBackend::Fuse => fuse::mount_fuse(fs, opts),
        MountBackend::Nfs => nfs::mount_nfs(fs, opts).await,
    }
}

/// Mount a filesystem with the given options (macOS version).
#[cfg(target_os = "macos")]
pub async fn mount_fs(
    fs: Arc<Mutex<dyn agentfs_sdk::FileSystem + Send>>,
    opts: MountOpts,
) -> Result<MountHandle> {
    match opts.backend {
        MountBackend::Fuse => {
            anyhow::bail!(
                "FUSE mounting is not supported on macOS.\n\
                 Use --backend nfs (default) instead."
            );
        }
        MountBackend::Nfs => nfs::mount_nfs(fs, opts).await,
    }
}

/// Mount a filesystem with the given options (Windows version).
#[cfg(target_os = "windows")]
pub async fn mount_fs(
    fs: Arc<parking_lot::Mutex<dyn agentfs_sdk::FileSystem + Send>>,
    opts: MountOpts,
) -> Result<MountHandle> {
    match opts.backend {
        MountBackend::Fuse => {
            anyhow::bail!(
                "FUSE mounting is not supported on Windows.\n\
                 Use --backend winfsp instead."
            );
        }
        MountBackend::Nfs => {
            anyhow::bail!(
                "NFS mounting is not supported on Windows.\n\
                 Use --backend winfsp instead."
            );
        }
        MountBackend::Winfsp => winfsp::mount_winfsp(fs, opts).await,
    }
}

/// Wait for a path to become a mountpoint.
pub fn wait_for_mount(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let interval = Duration:: from_millis(50);

    while start.elapsed() < timeout {
        if is_mountpoint(path) {
            return true;
        }
        std::thread::sleep(interval);
    }
    false
}

/// Check if a path is a mountpoint by comparing device IDs with parent.
pub fn is_mountpoint(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let path_meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };

        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("/"),
        };

        let parent_meta = match std::fs::metadata(parent) {
            Ok(m) => m,
            Err(_) => return false,
        };

        path_meta.dev() != parent_meta.dev()
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}
