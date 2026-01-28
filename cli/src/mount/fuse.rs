//! FUSE backend implementation for the mount infrastructure.

use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{wait_for_mount, MountBackend, MountHandle, MountHandleInner, MountOpts};

/// FUSE unmount implementation using fusermount.
pub(super) fn unmount_fuse(mountpoint: &Path, lazy: bool) -> Result<()> {
    const FUSERMOUNT_COMMANDS: &[&str] = &["fusermount3", "fusermount"];
    let args: &[&str] = if lazy { &["-uz"] } else { &["-u"] };

    for cmd in FUSERMOUNT_COMMANDS {
        let result = Command::new(cmd)
            .args(args)
            .arg(mountpoint.as_os_str())
            .status();

        match result {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }

    anyhow::bail!(
        "Failed to unmount {}. You may need to unmount manually with: fusermount -u {}",
        mountpoint.display(),
        mountpoint.display()
    )
}

/// Internal FUSE mount implementation.
pub(super) fn mount_fuse(
    fs: Arc<Mutex<dyn agentfs_sdk::FileSystem + Send>>,
    opts: MountOpts,
) -> Result<MountHandle> {
    use crate::fuse::FuseMountOptions;

    let fuse_opts = FuseMountOptions {
        mountpoint: opts.mountpoint.clone(),
        auto_unmount: opts.auto_unmount,
        allow_root: opts.allow_root,
        allow_other: opts.allow_other,
        fsname: opts.fsname.clone(),
        uid: opts.uid,
        gid: opts.gid,
    };

    let mountpoint = opts.mountpoint.clone();
    let timeout = opts.timeout;
    let lazy_unmount = opts.lazy_unmount;

    let fs_adapter = MutexFsAdapter { inner: fs };
    let fs_arc: Arc<dyn agentfs_sdk::FileSystem> = Arc::new(fs_adapter);

    let fuse_handle = std::thread::spawn(move || {
        let rt = crate::get_runtime();
        crate::fuse::mount(fs_arc, fuse_opts, rt)
    });

    if !wait_for_mount(&mountpoint, timeout) {
        anyhow::bail!("FUSE mount did not become ready within {:?}", timeout);
    }

    Ok(MountHandle {
        mountpoint,
        backend: MountBackend::Fuse,
        lazy_unmount,
        inner: MountHandleInner::Fuse {
            _thread: fuse_handle,
        },
    })
}

/// Adapter to use `Arc<Mutex<dyn FileSystem>>` as `Arc<dyn FileSystem>`.
struct MutexFsAdapter {
    inner: Arc<Mutex<dyn agentfs_sdk::FileSystem + Send>>,
}

#[async_trait::async_trait]
impl agentfs_sdk::FileSystem for MutexFsAdapter {
    async fn lookup(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<Option<agentfs_sdk::Stats>, agentfs_sdk::error::Error> {
        self.inner.lock().await.lookup(parent_ino, name).await
    }

    async fn getattr(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<agentfs_sdk::Stats>, agentfs_sdk::error::Error> {
        self.inner.lock().await.getattr(ino).await
    }

    async fn readlink(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<String>, agentfs_sdk::error::Error> {
        self.inner.lock().await.readlink(ino).await
    }

    async fn readdir(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<String>>, agentfs_sdk::error::Error> {
        self.inner.lock().await.readdir(ino).await
    }

    async fn readdir_plus(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<agentfs_sdk::DirEntry>>, agentfs_sdk::error::Error> {
        self.inner.lock().await.readdir_plus(ino).await
    }

    async fn chmod(
        &self,
        ino: i64,
        mode: u32,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner.lock().await.chmod(ino, mode).await
    }

    async fn chown(
        &self,
        ino: i64,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner.lock().await.chown(ino, uid, gid).await
    }

    async fn utimens(
        &self,
        ino: i64,
        atime: agentfs_sdk::TimeChange,
        mtime: agentfs_sdk::TimeChange,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner.lock().await.utimens(ino, atime, mtime).await
    }

    async fn open(
        &self,
        ino: i64,
    ) -> std::result::Result<agentfs_sdk::BoxedFile, agentfs_sdk::error::Error> {
        self.inner.lock().await.open(ino).await
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        self.inner
            .lock()
            .await
            .mkdir(parent_ino, name, mode, uid, gid)
            .await
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<(agentfs_sdk::Stats, agentfs_sdk::BoxedFile), agentfs_sdk::error::Error>
    {
        self.inner
            .lock()
            .await
            .create_file(parent_ino, name, mode, uid, gid)
            .await
    }

    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        self.inner
            .lock()
            .await
            .mknod(parent_ino, name, mode, rdev, uid, gid)
            .await
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        self.inner
            .lock()
            .await
            .symlink(parent_ino, name, target, uid, gid)
            .await
    }

    async fn unlink(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner.lock().await.unlink(parent_ino, name).await
    }

    async fn rmdir(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner.lock().await.rmdir(parent_ino, name).await
    }

    async fn link(
        &self,
        ino: i64,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        self.inner
            .lock()
            .await
            .link(ino, newparent_ino, newname)
            .await
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        self.inner
            .lock()
            .await
            .rename(oldparent_ino, oldname, newparent_ino, newname)
            .await
    }

    async fn statfs(
        &self,
    ) -> std::result::Result<agentfs_sdk::FilesystemStats, agentfs_sdk::error::Error> {
        self.inner.lock().await.statfs().await
    }
}
