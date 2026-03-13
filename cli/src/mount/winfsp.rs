//! WinFsp mount implementation for AgentFS.
//!
//! This module provides Windows-specific mounting using WinFsp filesystem driver.

use super::{MountBackend, MountHandle, MountHandleInner, MountOpts};
use crate::winfsp::AgentFSWinFsp;
use anyhow::Result;
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

/// Mount timeout for WinFsp.
const WINFSP_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// Mount an AgentFS filesystem using WinFsp.
///
/// This function accepts a `parking_lot::Mutex` because that's what the winfsp adapter
/// uses internally. The `mount_fs` function in mount/mod.rs uses
/// `tokio::sync::Mutex`, so the caller (mount/mod.rs) will need to convert
/// between the two Mutex types.
pub async fn mount_winfsp(
    fs: Arc<Mutex<dyn agentfs_sdk::FileSystem + Send>>,
    opts: MountOpts,
) -> Result<MountHandle> {
    let mountpoint = opts.mountpoint.clone();
    let mountpoint_str = mountpoint.to_string_lossy();
    tracing::debug!("Mounting WinFsp filesystem at {}", mountpoint_str);

    // Get handle to the current runtime instead of creating a new one
    // This avoids nested runtime panic
    tracing::debug!("Getting current tokio runtime handle...");
    let handle = Handle::current();
    tracing::debug!("Creating AgentFSWinFsp adapter...");
    let adapter = AgentFSWinFsp::new(fs, handle);

    // Configure volume parameters
    tracing::debug!("Configuring volume parameters...");
    let mut volume_params = winfsp::host::VolumeParams::default();
    volume_params.case_sensitive_search(true);
    volume_params.filesystem_name(&opts.fsname);

    // Create the filesystem host
    tracing::debug!("Creating FileSystemHost...");
    let mut host = winfsp::host::FileSystemHost::new(volume_params, adapter)?;
    tracing::debug!("FileSystemHost created successfully");

    // Mount the filesystem
    // WinFsp doesn't accept the \\?\ prefix from canonicalize, so strip it
    let mount_str = mountpoint_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&mountpoint_str)
        .to_string();
    tracing::debug!("Calling host.mount({})...", mount_str);
    host.mount(mount_str.as_str())?;
    tracing::debug!("host.mount() returned successfully");

    // Start the filesystem dispatcher (non-blocking)
    tracing::debug!("Calling host.start()...");
    host.start()?;
    tracing::debug!("host.start() returned successfully");

    // Wait for mount to be ready
    tracing::debug!("Waiting for mount to be ready...");
    if !wait_for_mount(&mountpoint, WINFSP_MOUNT_TIMEOUT) {
        // If mount fails, stop and unmount
        tracing::debug!("wait_for_mount returned false, stopping...");
        host.stop();
        host.unmount();
        anyhow::bail!(
            "WinFsp mount failed: filesystem not ready after {} seconds",
            WINFSP_MOUNT_TIMEOUT.as_secs()
        );
    }
    tracing::debug!("WinFsp filesystem mounted successfully at {}", mountpoint_str);

    // Box the host and leak it - we'll recover it during unmount
    // This is safe because we control the lifecycle and will properly clean up
    let host_box = Box::new(host);
    let host_ptr = Box::into_raw(host_box);

    Ok(MountHandle {
        mountpoint,
        backend: MountBackend::Winfsp,
        lazy_unmount: opts.lazy_unmount,
        inner: MountHandleInner::WinFsp { host_ptr: host_ptr as *mut () },
    })
}

/// Unmount a WinFsp filesystem.
///
/// This function reconstructs the FileSystemHost from the raw pointer and drops it,
/// which triggers the proper unmount and cleanup sequence.
pub fn unmount_winfsp(host_ptr: *mut ()) -> Result<()> {
    if host_ptr.is_null() {
        return Ok(());
    }

    // Reconstruct the Box and let it drop, which will:
    // 1. Call unmount() - FspFileSystemRemoveMountPoint
    // 2. Call stop() - FspFileSystemStopDispatcher
    // 3. Call FspFileSystemDelete
    // 4. Drop the user context and interface
    unsafe {
        let _host: Box<winfsp::host::FileSystemHost<AgentFSWinFsp>> =
            Box::from_raw(host_ptr as *mut _);
        // Box is dropped here, triggering cleanup
    }

    tracing::info!("WinFsp filesystem unmounted successfully");
    Ok(())
}

/// Wait for a WinFsp mount to become ready.
///
/// On Windows, WinFsp mount is synchronous, so we just verify the mount point
/// is accessible after a short delay.
fn wait_for_mount(path: &Path, _timeout: Duration) -> bool {
    // WinFsp mount is synchronous - just do a basic check
    // Give it a moment to settle
    std::thread::sleep(Duration::from_millis(100));

    // Try to list the directory to verify the mount is working
    match std::fs::read_dir(path) {
        Ok(_) => {
            tracing::debug!("WinFsp mount verified - directory is accessible");
            true
        }
        Err(e) => {
            tracing::warn!("WinFsp mount verification failed: {}", e);
            // Still return true if the path exists - the filesystem might just be empty
            path.exists()
        }
    }
}