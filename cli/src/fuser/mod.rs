//! FUSE userspace library implementation (Linux-only, pure-Rust)
//!
//! This is an improved rewrite of the FUSE userspace library (lowlevel interface) to fully take
//! advantage of Rust's architecture. This version uses a pure-Rust mounting implementation
//! and does not require libfuse.

#![allow(
    missing_docs,
    missing_debug_implementations,
    dead_code,
    unused_imports,
    unexpected_cfgs,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

use async_trait::async_trait;
use libc::{c_int, ENOSYS, EPERM};
use log::warn;
use mnt::mount_options::parse_options_from_args;
use std::cmp::min;
use std::ffi::OsStr;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::time::SystemTime;

pub use ll::fuse_abi::consts;
pub use ll::fuse_abi::fuse_forget_one;
pub use ll::fuse_abi::FUSE_ROOT_ID;
pub use ll::TimeOrNow;
pub use mnt::mount_options::MountOption;
pub use notify::{Notifier, PollHandle};
pub use reply::ReplyPoll;
pub use reply::ReplyXattr;
pub use reply::{Reply, ReplyAttr, ReplyData, ReplyEmpty, ReplyEntry, ReplyOpen};
pub use reply::{
    ReplyBmap, ReplyCreate, ReplyDirectory, ReplyDirectoryPlus, ReplyIoctl, ReplyLock, ReplyLseek,
    ReplyStatfs, ReplyWrite,
};
pub use request::Request;
pub use session::{BackgroundSession, Session, SessionACL, SessionUnmounter};

use ll::fuse_abi::consts::*;
use mnt::mount_options::check_option_conflicts;
use session::MAX_WRITE_SIZE;

mod channel;
#[allow(
    dead_code,
    unused_imports,
    unexpected_cfgs,
    clippy::manual_is_multiple_of
)]
mod ll;
#[allow(clippy::io_other_error)]
mod mnt;
#[allow(clippy::io_other_error)]
mod notify;
#[allow(unexpected_cfgs)]
mod reply;
#[allow(unused_imports, unexpected_cfgs)]
mod request;
mod session;

/// We generally support async reads (Linux)
const INIT_FLAGS: u64 = FUSE_ASYNC_READ | FUSE_BIG_WRITES;

const fn default_init_flags(#[allow(unused_variables)] capabilities: u64) -> u64 {
    let mut flags = INIT_FLAGS;
    if capabilities & FUSE_MAX_PAGES != 0 {
        flags |= FUSE_MAX_PAGES;
    }
    flags
}

/// File types
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileType {
    /// Named pipe (`S_IFIFO`)
    NamedPipe,
    /// Character device (`S_IFCHR`)
    CharDevice,
    /// Block device (`S_IFBLK`)
    BlockDevice,
    /// Directory (`S_IFDIR`)
    Directory,
    /// Regular file (`S_IFREG`)
    RegularFile,
    /// Symbolic link (`S_IFLNK`)
    Symlink,
    /// Unix domain socket (`S_IFSOCK`)
    Socket,
}

/// File attributes
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileAttr {
    /// Inode number
    pub ino: u64,
    /// Size in bytes
    pub size: u64,
    /// Size in blocks
    pub blocks: u64,
    /// Time of last access
    pub atime: SystemTime,
    /// Time of last modification
    pub mtime: SystemTime,
    /// Time of last change
    pub ctime: SystemTime,
    /// Time of creation (macOS only, but kept for API compatibility)
    pub crtime: SystemTime,
    /// Kind of file (directory, file, pipe, etc)
    pub kind: FileType,
    /// Permissions
    pub perm: u16,
    /// Number of hard links
    pub nlink: u32,
    /// User id
    pub uid: u32,
    /// Group id
    pub gid: u32,
    /// Rdev
    pub rdev: u32,
    /// Block size
    pub blksize: u32,
    /// Flags (macOS only, but kept for API compatibility)
    pub flags: u32,
}

/// Configuration of the fuse kernel module connection
#[derive(Debug)]
pub struct KernelConfig {
    capabilities: u64,
    requested: u64,
    max_readahead: u32,
    max_max_readahead: u32,
    max_background: u16,
    congestion_threshold: Option<u16>,
    max_write: u32,
    time_gran: std::time::Duration,
}

impl KernelConfig {
    pub(crate) fn new(capabilities: u64, max_readahead: u32) -> Self {
        Self {
            capabilities,
            requested: default_init_flags(capabilities),
            max_readahead,
            max_max_readahead: max_readahead,
            max_background: 16,
            congestion_threshold: None,
            max_write: MAX_WRITE_SIZE as u32,
            time_gran: std::time::Duration::new(0, 1),
        }
    }

    /// Set the timestamp granularity
    pub fn set_time_granularity(
        &mut self,
        value: std::time::Duration,
    ) -> Result<std::time::Duration, std::time::Duration> {
        if value.as_nanos() == 0 {
            return Err(std::time::Duration::new(0, 1));
        }
        if value.as_secs() > 1 || (value.as_secs() == 1 && value.subsec_nanos() > 0) {
            return Err(std::time::Duration::new(1, 0));
        }
        let mut power_of_10 = 1;
        while power_of_10 < value.as_nanos() {
            if value.as_nanos() < power_of_10 * 10 {
                return Err(std::time::Duration::new(0, power_of_10 as u32));
            }
            power_of_10 *= 10;
        }
        let previous = self.time_gran;
        self.time_gran = value;
        Ok(previous)
    }

    /// Set the maximum write size for a single request
    pub fn set_max_write(&mut self, value: u32) -> Result<u32, u32> {
        if value == 0 {
            return Err(1);
        }
        if value > MAX_WRITE_SIZE as u32 {
            return Err(MAX_WRITE_SIZE as u32);
        }
        let previous = self.max_write;
        self.max_write = value;
        Ok(previous)
    }

    /// Set the maximum readahead size
    pub fn set_max_readahead(&mut self, value: u32) -> Result<u32, u32> {
        if value == 0 {
            return Err(1);
        }
        if value > self.max_max_readahead {
            return Err(self.max_max_readahead);
        }
        let previous = self.max_readahead;
        self.max_readahead = value;
        Ok(previous)
    }

    /// Add a set of capabilities
    pub fn add_capabilities(&mut self, capabilities_to_add: u64) -> Result<(), u64> {
        if capabilities_to_add & self.capabilities != capabilities_to_add {
            return Err(capabilities_to_add - (capabilities_to_add & self.capabilities));
        }
        self.requested |= capabilities_to_add;
        Ok(())
    }

    /// Set the maximum number of pending background requests
    pub fn set_max_background(&mut self, value: u16) -> Result<u16, u16> {
        if value == 0 {
            return Err(1);
        }
        let previous = self.max_background;
        self.max_background = value;
        Ok(previous)
    }

    /// Set the congestion threshold
    pub fn set_congestion_threshold(&mut self, value: u16) -> Result<u16, u16> {
        if value == 0 {
            return Err(1);
        }
        let previous = self.congestion_threshold();
        self.congestion_threshold = Some(value);
        Ok(previous)
    }

    pub(crate) fn congestion_threshold(&self) -> u16 {
        match self.congestion_threshold {
            None => (u32::from(self.max_background) * 3 / 4) as u16,
            Some(value) => min(value, self.max_background),
        }
    }

    pub(crate) fn max_pages(&self) -> u16 {
        ((std::cmp::max(self.max_write, self.max_readahead) - 1) / page_size::get() as u32) as u16
            + 1
    }

    pub(crate) fn requested(&self) -> u64 {
        self.requested
    }

    pub(crate) fn max_readahead(&self) -> u32 {
        self.max_readahead
    }

    pub(crate) fn max_background(&self) -> u16 {
        self.max_background
    }

    pub(crate) fn max_write(&self) -> u32 {
        self.max_write
    }

    pub(crate) fn time_gran(&self) -> std::time::Duration {
        self.time_gran
    }
}

/// Filesystem trait.
///
/// This trait must be implemented to provide a userspace filesystem via FUSE.
/// All methods are async to allow for non-blocking I/O operations.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait Filesystem: Send + Sync {
    /// Initialize filesystem.
    async fn init(&self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
        Ok(())
    }

    /// Clean up filesystem.
    fn destroy(&self) {}

    /// Look up a directory entry by name and get its attributes.
    async fn lookup(&self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        warn!("[Not Implemented] lookup(parent: {parent:#x?}, name {name:?})");
        reply.error(ENOSYS);
    }

    /// Forget about an inode.
    async fn forget(&self, _req: &Request<'_>, _ino: u64, _nlookup: u64) {}

    /// Like forget, but take multiple forget requests at once for performance.
    async fn batch_forget(&self, req: &Request<'_>, nodes: &[fuse_forget_one]) {
        for node in nodes {
            self.forget(req, node.nodeid, node.nlookup).await;
        }
    }

    /// Get file attributes.
    async fn getattr(&self, _req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        warn!("[Not Implemented] getattr(ino: {ino:#x?}, fh: {fh:#x?})");
        reply.error(ENOSYS);
    }

    /// Set file attributes.
    async fn setattr(
        &self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        warn!(
            "[Not Implemented] setattr(ino: {ino:#x?}, mode: {mode:?}, uid: {uid:?}, \
            gid: {gid:?}, size: {size:?}, fh: {fh:?}, flags: {flags:?})"
        );
        reply.error(ENOSYS);
    }

    /// Read symbolic link.
    async fn readlink(&self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        warn!("[Not Implemented] readlink(ino: {ino:#x?})");
        reply.error(ENOSYS);
    }

    /// Create file node.
    async fn mknod(
        &self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        warn!(
            "[Not Implemented] mknod(parent: {parent:#x?}, name: {name:?}, \
            mode: {mode}, umask: {umask:#x?}, rdev: {rdev})"
        );
        reply.error(ENOSYS);
    }

    /// Create a directory.
    async fn mkdir(
        &self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        warn!(
            "[Not Implemented] mkdir(parent: {parent:#x?}, name: {name:?}, mode: {mode}, umask: {umask:#x?})"
        );
        reply.error(ENOSYS);
    }

    /// Remove a file.
    async fn unlink(&self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        warn!("[Not Implemented] unlink(parent: {parent:#x?}, name: {name:?})",);
        reply.error(ENOSYS);
    }

    /// Remove a directory.
    async fn rmdir(&self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        warn!("[Not Implemented] rmdir(parent: {parent:#x?}, name: {name:?})",);
        reply.error(ENOSYS);
    }

    /// Create a symbolic link.
    async fn symlink(
        &self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        warn!(
            "[Not Implemented] symlink(parent: {parent:#x?}, link_name: {link_name:?}, target: {target:?})",
        );
        reply.error(EPERM);
    }

    /// Rename a file.
    async fn rename(
        &self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        warn!(
            "[Not Implemented] rename(parent: {parent:#x?}, name: {name:?}, \
            newparent: {newparent:#x?}, newname: {newname:?}, flags: {flags})",
        );
        reply.error(ENOSYS);
    }

    /// Create a hard link.
    async fn link(
        &self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        warn!(
            "[Not Implemented] link(ino: {ino:#x?}, newparent: {newparent:#x?}, newname: {newname:?})"
        );
        reply.error(EPERM);
    }

    /// Open a file.
    async fn open(&self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    /// Read data.
    async fn read(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        flags: i32,
        lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        warn!(
            "[Not Implemented] read(ino: {ino:#x?}, fh: {fh}, offset: {offset}, \
            size: {size}, flags: {flags:#x?}, lock_owner: {lock_owner:?})"
        );
        reply.error(ENOSYS);
    }

    /// Write data.
    async fn write(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
        lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        warn!(
            "[Not Implemented] write(ino: {ino:#x?}, fh: {fh}, offset: {offset}, \
            data.len(): {}, write_flags: {write_flags:#x?}, flags: {flags:#x?}, \
            lock_owner: {lock_owner:?})",
            data.len()
        );
        reply.error(ENOSYS);
    }

    /// Flush method.
    async fn flush(&self, _req: &Request<'_>, ino: u64, fh: u64, lock_owner: u64, reply: ReplyEmpty) {
        warn!("[Not Implemented] flush(ino: {ino:#x?}, fh: {fh}, lock_owner: {lock_owner:?})");
        reply.error(ENOSYS);
    }

    /// Release an open file.
    async fn release(
        &self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Synchronize file contents.
    async fn fsync(&self, _req: &Request<'_>, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        warn!("[Not Implemented] fsync(ino: {ino:#x?}, fh: {fh}, datasync: {datasync})");
        reply.error(ENOSYS);
    }

    /// Open a directory.
    async fn opendir(&self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    /// Read directory.
    async fn readdir(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        reply: ReplyDirectory,
    ) {
        warn!("[Not Implemented] readdir(ino: {ino:#x?}, fh: {fh}, offset: {offset})");
        reply.error(ENOSYS);
    }

    /// Read directory with attributes.
    async fn readdirplus(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        reply: ReplyDirectoryPlus,
    ) {
        warn!("[Not Implemented] readdirplus(ino: {ino:#x?}, fh: {fh}, offset: {offset})");
        reply.error(ENOSYS);
    }

    /// Release an open directory.
    async fn releasedir(
        &self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// Synchronize directory contents.
    async fn fsyncdir(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        warn!("[Not Implemented] fsyncdir(ino: {ino:#x?}, fh: {fh}, datasync: {datasync})");
        reply.error(ENOSYS);
    }

    /// Get file system statistics.
    async fn statfs(&self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    /// Set an extended attribute.
    async fn setxattr(
        &self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        _value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        warn!(
            "[Not Implemented] setxattr(ino: {ino:#x?}, name: {name:?}, \
            flags: {flags:#x?}, position: {position})"
        );
        reply.error(ENOSYS);
    }

    /// Get an extended attribute.
    async fn getxattr(
        &self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        warn!("[Not Implemented] getxattr(ino: {ino:#x?}, name: {name:?}, size: {size})");
        reply.error(ENOSYS);
    }

    /// List extended attribute names.
    async fn listxattr(&self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        warn!("[Not Implemented] listxattr(ino: {ino:#x?}, size: {size})");
        reply.error(ENOSYS);
    }

    /// Remove an extended attribute.
    async fn removexattr(&self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        warn!("[Not Implemented] removexattr(ino: {ino:#x?}, name: {name:?})");
        reply.error(ENOSYS);
    }

    /// Check file access permissions.
    async fn access(&self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        warn!("[Not Implemented] access(ino: {ino:#x?}, mask: {mask})");
        reply.error(ENOSYS);
    }

    /// Create and open a file.
    async fn create(
        &self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        warn!(
            "[Not Implemented] create(parent: {parent:#x?}, name: {name:?}, mode: {mode}, \
            umask: {umask:#x?}, flags: {flags:#x?})"
        );
        reply.error(ENOSYS);
    }

    /// Test for a POSIX file lock.
    async fn getlk(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        warn!(
            "[Not Implemented] getlk(ino: {ino:#x?}, fh: {fh}, lock_owner: {lock_owner}, \
            start: {start}, end: {end}, typ: {typ}, pid: {pid})"
        );
        reply.error(ENOSYS);
    }

    /// Acquire, modify or release a POSIX file lock.
    async fn setlk(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        warn!(
            "[Not Implemented] setlk(ino: {ino:#x?}, fh: {fh}, lock_owner: {lock_owner}, \
            start: {start}, end: {end}, typ: {typ}, pid: {pid}, sleep: {sleep})"
        );
        reply.error(ENOSYS);
    }

    /// Map block index within file to block index within device.
    async fn bmap(&self, _req: &Request<'_>, ino: u64, blocksize: u32, idx: u64, reply: ReplyBmap) {
        warn!("[Not Implemented] bmap(ino: {ino:#x?}, blocksize: {blocksize}, idx: {idx})",);
        reply.error(ENOSYS);
    }

    /// Control device.
    async fn ioctl(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        flags: u32,
        cmd: u32,
        in_data: &[u8],
        out_size: u32,
        reply: ReplyIoctl,
    ) {
        warn!(
            "[Not Implemented] ioctl(ino: {ino:#x?}, fh: {fh}, flags: {flags}, \
            cmd: {cmd}, in_data.len(): {}, out_size: {out_size})",
            in_data.len()
        );
        reply.error(ENOSYS);
    }

    /// Poll for events.
    async fn poll(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        ph: PollHandle,
        events: u32,
        flags: u32,
        reply: ReplyPoll,
    ) {
        warn!(
            "[Not Implemented] poll(ino: {ino:#x?}, fh: {fh}, \
            ph: {ph:?}, events: {events}, flags: {flags})"
        );
        reply.error(ENOSYS);
    }

    /// Preallocate or deallocate space to a file.
    async fn fallocate(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        warn!(
            "[Not Implemented] fallocate(ino: {ino:#x?}, fh: {fh}, \
            offset: {offset}, length: {length}, mode: {mode})"
        );
        reply.error(ENOSYS);
    }

    /// Reposition read/write file offset.
    async fn lseek(
        &self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        warn!(
            "[Not Implemented] lseek(ino: {ino:#x?}, fh: {fh}, \
            offset: {offset}, whence: {whence})"
        );
        reply.error(ENOSYS);
    }

    /// Copy the specified range from the source inode to the destination inode.
    async fn copy_file_range(
        &self,
        _req: &Request<'_>,
        ino_in: u64,
        fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        fh_out: u64,
        offset_out: i64,
        len: u64,
        flags: u32,
        reply: ReplyWrite,
    ) {
        warn!(
            "[Not Implemented] copy_file_range(ino_in: {ino_in:#x?}, fh_in: {fh_in}, \
            offset_in: {offset_in}, ino_out: {ino_out:#x?}, fh_out: {fh_out}, \
            offset_out: {offset_out}, len: {len}, flags: {flags})"
        );
        reply.error(ENOSYS);
    }
}

/// Mount the given filesystem to the given mountpoint. This function will
/// not return until the filesystem is unmounted.
#[deprecated(note = "use mount2() instead")]
pub fn mount<FS: Filesystem + 'static, P: AsRef<Path>>(
    filesystem: FS,
    mountpoint: P,
    options: &[&OsStr],
    runtime: &tokio::runtime::Runtime,
) -> io::Result<()> {
    let options = parse_options_from_args(options)?;
    mount2(filesystem, mountpoint, options.as_ref(), runtime)
}

/// Mount the given filesystem to the given mountpoint. This function will
/// not return until the filesystem is unmounted.
pub fn mount2<FS: Filesystem + 'static, P: AsRef<Path>>(
    filesystem: FS,
    mountpoint: P,
    options: &[MountOption],
    runtime: &tokio::runtime::Runtime,
) -> io::Result<()> {
    check_option_conflicts(options)?;
    Session::new(filesystem, mountpoint.as_ref(), options).and_then(|mut se| se.run(runtime))
}

/// Mount the given filesystem to the given mountpoint. This function spawns
/// a background thread to handle filesystem operations while being mounted
/// and therefore returns immediately.
#[deprecated(note = "use spawn_mount2() instead")]
pub fn spawn_mount<'a, FS: Filesystem + Send + 'static + 'a, P: AsRef<Path>>(
    filesystem: FS,
    mountpoint: P,
    options: &[&OsStr],
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
) -> io::Result<BackgroundSession> {
    let options: Option<Vec<_>> = options
        .iter()
        .map(|x| Some(MountOption::from_str(x.to_str()?)))
        .collect();
    let options = options.ok_or(ErrorKind::InvalidData)?;
    Session::new(filesystem, mountpoint.as_ref(), options.as_ref())
        .and_then(|se| se.spawn(runtime))
}

/// Mount the given filesystem to the given mountpoint. This function spawns
/// a background thread to handle filesystem operations while being mounted
/// and therefore returns immediately.
pub fn spawn_mount2<'a, FS: Filesystem + Send + 'static + 'a, P: AsRef<Path>>(
    filesystem: FS,
    mountpoint: P,
    options: &[MountOption],
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
) -> io::Result<BackgroundSession> {
    check_option_conflicts(options)?;
    Session::new(filesystem, mountpoint.as_ref(), options).and_then(|se| se.spawn(runtime))
}
