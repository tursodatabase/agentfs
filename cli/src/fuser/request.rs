//! Filesystem operation request
//!
//! A request represents information about a filesystem operation the kernel driver wants us to
//! perform.
//!
//! TODO: This module is meant to go away soon in favor of `ll::Request`.

use super::ll::{fuse_abi as abi, Errno, Response};
use log::{debug, error, warn};
use std::convert::TryFrom;
use std::convert::TryInto;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::channel::ChannelSender;
use super::ll::Request as _;
use super::reply::ReplyDirectoryPlus;
use super::reply::{Reply, ReplyDirectory, ReplySender};
use super::session::{Session, SessionACL, SharedSessionState};
use super::Filesystem;
use super::PollHandle;
use super::{ll, KernelConfig};

/// Request data structure
#[derive(Debug)]
pub struct Request<'a> {
    /// Channel sender for sending the reply
    ch: ChannelSender,
    /// Request raw data
    #[allow(unused)]
    data: &'a [u8],
    /// Parsed request
    request: ll::AnyRequest<'a>,
}

impl<'a> Request<'a> {
    /// Create a new request from the given data
    pub(crate) fn new(ch: ChannelSender, data: &'a [u8]) -> Option<Request<'a>> {
        let request = match ll::AnyRequest::try_from(data) {
            Ok(request) => request,
            Err(err) => {
                error!("{err}");
                return None;
            }
        };

        Some(Self { ch, data, request })
    }

    /// Dispatch request to the given filesystem.
    /// This calls the appropriate filesystem operation method for the
    /// request and sends back the returned reply to the kernel
    pub(crate) async fn dispatch<FS: Filesystem>(&self, se: &mut Session<FS>) {
        debug!("{}", self.request);
        let unique = self.request.unique();

        let res = match self.dispatch_req(se).await {
            Ok(Some(resp)) => resp,
            Ok(None) => return,
            Err(errno) => self.request.reply_err(errno),
        }
        .with_iovec(unique, |iov| self.ch.send(iov));

        if let Err(err) = res {
            warn!("Request {unique:?}: Failed to send reply: {err}");
        }
    }

    async fn dispatch_req<FS: Filesystem>(
        &self,
        se: &mut Session<FS>,
    ) -> Result<Option<Response<'_>>, Errno> {
        let op = self.request.operation().map_err(|_| Errno::ENOSYS)?;
        // Implement allow_root & access check for auto_unmount
        if (se.allowed == SessionACL::RootAndOwner
            && self.request.uid() != se.session_owner
            && self.request.uid() != 0)
            || (se.allowed == SessionACL::Owner && self.request.uid() != se.session_owner)
        {
            #[cfg(feature = "abi-7-21")]
            {
                match op {
                    // Only allow operations that the kernel may issue without a uid set
                    ll::Operation::Init(_)
                    | ll::Operation::Destroy(_)
                    | ll::Operation::Read(_)
                    | ll::Operation::ReadDir(_)
                    | ll::Operation::ReadDirPlus(_)
                    | ll::Operation::BatchForget(_)
                    | ll::Operation::Forget(_)
                    | ll::Operation::Write(_)
                    | ll::Operation::FSync(_)
                    | ll::Operation::FSyncDir(_)
                    | ll::Operation::Release(_)
                    | ll::Operation::ReleaseDir(_) => {}
                    _ => {
                        return Err(Errno::EACCES);
                    }
                }
            }
            #[cfg(not(feature = "abi-7-21"))]
            {
                match op {
                    // Only allow operations that the kernel may issue without a uid set
                    ll::Operation::Init(_)
                    | ll::Operation::Destroy(_)
                    | ll::Operation::Read(_)
                    | ll::Operation::ReadDir(_)
                    | ll::Operation::BatchForget(_)
                    | ll::Operation::Forget(_)
                    | ll::Operation::Write(_)
                    | ll::Operation::FSync(_)
                    | ll::Operation::FSyncDir(_)
                    | ll::Operation::Release(_)
                    | ll::Operation::ReleaseDir(_) => {}
                    _ => {
                        return Err(Errno::EACCES);
                    }
                }
            }
        }
        match op {
            // Filesystem initialization
            ll::Operation::Init(x) => {
                // We don't support ABI versions before 7.6
                let v = x.version();
                if v < ll::Version(7, 6) {
                    error!("Unsupported FUSE ABI version {v}");
                    return Err(Errno::EPROTO);
                }
                // Remember ABI version supported by kernel
                se.proto_major = v.major();
                se.proto_minor = v.minor();

                let mut config = KernelConfig::new(x.capabilities(), x.max_readahead());
                // Call filesystem init method and give it a chance to return an error
                se.filesystem
                    .init(self, &mut config)
                    .await
                    .map_err(Errno::from_i32)?;

                // Reply with our desired version and settings. If the kernel supports a
                // larger major version, it'll re-send a matching init message. If it
                // supports only lower major versions, we replied with an error above.
                debug!(
                    "INIT response: ABI {}.{}, flags {:#x}, max readahead {}, max write {}",
                    abi::FUSE_KERNEL_VERSION,
                    abi::FUSE_KERNEL_MINOR_VERSION,
                    x.capabilities() & config.requested,
                    config.max_readahead,
                    config.max_write
                );
                se.initialized = true;
                return Ok(Some(x.reply(&config)));
            }
            // Any operation is invalid before initialization
            _ if !se.initialized => {
                warn!("Ignoring FUSE operation before init: {}", self.request);
                return Err(Errno::EIO);
            }
            // Filesystem destroyed
            ll::Operation::Destroy(x) => {
                se.filesystem.destroy();
                se.destroyed = true;
                return Ok(Some(x.reply()));
            }
            // Any operation is invalid after destroy
            _ if se.destroyed => {
                warn!("Ignoring FUSE operation after destroy: {}", self.request);
                return Err(Errno::EIO);
            }

            ll::Operation::Interrupt(_) => {
                // TODO: handle FUSE_INTERRUPT
                return Err(Errno::ENOSYS);
            }

            ll::Operation::Lookup(x) => {
                se.filesystem.lookup(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Forget(x) => {
                se.filesystem
                    .forget(self, self.request.nodeid().into(), x.nlookup()).await; // no reply
            }
            ll::Operation::GetAttr(_attr) => {
                se.filesystem.getattr(
                    self,
                    self.request.nodeid().into(),
                    _attr.file_handle().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::SetAttr(x) => {
                se.filesystem.setattr(
                    self,
                    self.request.nodeid().into(),
                    x.mode(),
                    x.uid(),
                    x.gid(),
                    x.size(),
                    x.atime(),
                    x.mtime(),
                    x.ctime(),
                    x.file_handle().map(std::convert::Into::into),
                    x.crtime(),
                    x.chgtime(),
                    x.bkuptime(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::ReadLink(_) => {
                se.filesystem
                    .readlink(self, self.request.nodeid().into(), self.reply()).await;
            }
            ll::Operation::MkNod(x) => {
                se.filesystem.mknod(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.rdev(),
                    self.reply(),
                ).await;
            }
            ll::Operation::MkDir(x) => {
                se.filesystem.mkdir(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Unlink(x) => {
                se.filesystem.unlink(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::RmDir(x) => {
                se.filesystem.rmdir(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::SymLink(x) => {
                se.filesystem.symlink(
                    self,
                    self.request.nodeid().into(),
                    x.link_name().as_ref(),
                    Path::new(x.target()),
                    self.reply(),
                ).await;
            }
            ll::Operation::Rename(x) => {
                se.filesystem.rename(
                    self,
                    self.request.nodeid().into(),
                    x.src().name.as_ref(),
                    x.dest().dir.into(),
                    x.dest().name.as_ref(),
                    0,
                    self.reply(),
                ).await;
            }
            ll::Operation::Link(x) => {
                se.filesystem.link(
                    self,
                    x.inode_no().into(),
                    self.request.nodeid().into(),
                    x.dest().name.as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Open(x) => {
                se.filesystem
                    .open(self, self.request.nodeid().into(), x.flags(), self.reply()).await;
            }
            ll::Operation::Read(x) => {
                se.filesystem.read(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.size(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::Write(x) => {
                se.filesystem.write(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.data(),
                    x.write_flags(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::Flush(x) => {
                se.filesystem.flush(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Release(x) => {
                se.filesystem.release(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    x.flush(),
                    self.reply(),
                ).await;
            }
            ll::Operation::FSync(x) => {
                se.filesystem.fsync(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                ).await;
            }
            ll::Operation::OpenDir(x) => {
                se.filesystem
                    .opendir(self, self.request.nodeid().into(), x.flags(), self.reply()).await;
            }
            ll::Operation::ReadDir(x) => {
                se.filesystem.readdir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectory::new(
                        self.request.unique().into(),
                        self.ch.clone(),
                        x.size() as usize,
                    ),
                ).await;
            }
            ll::Operation::ReleaseDir(x) => {
                se.filesystem.releasedir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::FSyncDir(x) => {
                se.filesystem.fsyncdir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                ).await;
            }
            ll::Operation::StatFs(_) => {
                se.filesystem
                    .statfs(self, self.request.nodeid().into(), self.reply()).await;
            }
            ll::Operation::SetXAttr(x) => {
                se.filesystem.setxattr(
                    self,
                    self.request.nodeid().into(),
                    x.name(),
                    x.value(),
                    x.flags(),
                    x.position(),
                    self.reply(),
                ).await;
            }
            ll::Operation::GetXAttr(x) => {
                se.filesystem.getxattr(
                    self,
                    self.request.nodeid().into(),
                    x.name(),
                    x.size_u32(),
                    self.reply(),
                ).await;
            }
            ll::Operation::ListXAttr(x) => {
                se.filesystem
                    .listxattr(self, self.request.nodeid().into(), x.size(), self.reply()).await;
            }
            ll::Operation::RemoveXAttr(x) => {
                se.filesystem.removexattr(
                    self,
                    self.request.nodeid().into(),
                    x.name(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Access(x) => {
                se.filesystem
                    .access(self, self.request.nodeid().into(), x.mask(), self.reply()).await;
            }
            ll::Operation::Create(x) => {
                se.filesystem.create(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::GetLk(x) => {
                se.filesystem.getlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    self.reply(),
                ).await;
            }
            ll::Operation::SetLk(x) => {
                se.filesystem.setlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    false,
                    self.reply(),
                ).await;
            }
            ll::Operation::SetLkW(x) => {
                se.filesystem.setlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    true,
                    self.reply(),
                ).await;
            }
            ll::Operation::BMap(x) => {
                se.filesystem.bmap(
                    self,
                    self.request.nodeid().into(),
                    x.block_size(),
                    x.block(),
                    self.reply(),
                ).await;
            }

            ll::Operation::IoCtl(x) => {
                if x.unrestricted() {
                    return Err(Errno::ENOSYS);
                }
                se.filesystem.ioctl(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.command(),
                    x.in_data(),
                    x.out_size(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Poll(x) => {
                let ph = PollHandle::new(se.ch.sender(), x.kernel_handle());

                se.filesystem.poll(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    ph,
                    x.events(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::NotifyReply(_) => {
                // TODO: handle FUSE_NOTIFY_REPLY
                return Err(Errno::ENOSYS);
            }
            ll::Operation::BatchForget(x) => {
                se.filesystem.batch_forget(self, x.nodes()).await; // no reply
            }
            #[cfg(feature = "abi-7-19")]
            ll::Operation::FAllocate(x) => {
                se.filesystem.fallocate(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.len(),
                    x.mode(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-21")]
            ll::Operation::ReadDirPlus(x) => {
                se.filesystem.readdirplus(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectoryPlus::new(
                        self.request.unique().into(),
                        self.ch.clone(),
                        x.size() as usize,
                    ),
                ).await;
            }
            #[cfg(feature = "abi-7-23")]
            ll::Operation::Rename2(x) => {
                se.filesystem.rename(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-24")]
            ll::Operation::Lseek(x) => {
                se.filesystem.lseek(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.whence(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-28")]
            ll::Operation::CopyFileRange(x) => {
                let (i, o) = (x.src(), x.dest());
                se.filesystem.copy_file_range(
                    self,
                    i.inode.into(),
                    i.file_handle.into(),
                    i.offset,
                    o.inode.into(),
                    o.file_handle.into(),
                    o.offset,
                    x.len(),
                    x.flags().try_into().unwrap(),
                    self.reply(),
                ).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::SetVolName(x) => {
                se.filesystem.setvolname(self, x.name(), self.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::GetXTimes(x) => {
                se.filesystem
                    .getxtimes(self, x.nodeid().into(), self.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::Exchange(x) => {
                se.filesystem.exchange(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.options(),
                    self.reply(),
                ).await;
            }

            ll::Operation::CuseInit(_) => {
                // TODO: handle CUSE_INIT
                return Err(Errno::ENOSYS);
            }
        }
        Ok(None)
    }

    /// Create a reply object for this request that can be passed to the filesystem
    /// implementation and makes sure that a request is replied exactly once
    fn reply<T: Reply>(&self) -> T {
        Reply::new(self.request.unique().into(), self.ch.clone())
    }

    /// Returns the unique identifier of this request
    #[inline]
    pub fn unique(&self) -> u64 {
        self.request.unique().into()
    }

    /// Returns the uid of this request
    #[inline]
    pub fn uid(&self) -> u32 {
        self.request.uid()
    }

    /// Returns the gid of this request
    #[inline]
    pub fn gid(&self) -> u32 {
        self.request.gid()
    }

    /// Returns the pid of this request
    #[inline]
    pub fn pid(&self) -> u32 {
        self.request.pid()
    }

    /// Dispatch request to the given shared session state.
    /// This is used for parallel request processing.
    pub(crate) async fn dispatch_shared<FS: Filesystem>(&self, shared: &Arc<SharedSessionState<FS>>) -> bool {
        debug!("{}", self.request);
        let unique = self.request.unique();

        let op = match self.request.operation() {
            Ok(op) => op,
            Err(_) => {
                let _ = self.request.reply_err(Errno::ENOSYS)
                    .with_iovec(unique, |iov| self.ch.send(iov));
                return false;
            }
        };

        // Check if this is an INIT request
        let is_init = matches!(op, ll::Operation::Init(_));

        let res = match self.dispatch_req_shared(shared).await {
            Ok(Some(resp)) => resp,
            Ok(None) => return is_init,
            Err(errno) => self.request.reply_err(errno),
        }
        .with_iovec(unique, |iov| self.ch.send(iov));

        if let Err(err) = res {
            warn!("Request {unique:?}: Failed to send reply: {err}");
        }

        is_init
    }

    async fn dispatch_req_shared<FS: Filesystem>(
        &self,
        shared: &Arc<SharedSessionState<FS>>,
    ) -> Result<Option<Response<'_>>, Errno> {
        let op = self.request.operation().map_err(|_| Errno::ENOSYS)?;

        // Implement allow_root & access check for auto_unmount
        if (shared.allowed == SessionACL::RootAndOwner
            && self.request.uid() != shared.session_owner
            && self.request.uid() != 0)
            || (shared.allowed == SessionACL::Owner && self.request.uid() != shared.session_owner)
        {
            #[cfg(feature = "abi-7-21")]
            {
                match op {
                    // Only allow operations that the kernel may issue without a uid set
                    ll::Operation::Init(_)
                    | ll::Operation::Destroy(_)
                    | ll::Operation::Read(_)
                    | ll::Operation::ReadDir(_)
                    | ll::Operation::ReadDirPlus(_)
                    | ll::Operation::BatchForget(_)
                    | ll::Operation::Forget(_)
                    | ll::Operation::Write(_)
                    | ll::Operation::FSync(_)
                    | ll::Operation::FSyncDir(_)
                    | ll::Operation::Release(_)
                    | ll::Operation::ReleaseDir(_) => {}
                    _ => {
                        return Err(Errno::EACCES);
                    }
                }
            }
            #[cfg(not(feature = "abi-7-21"))]
            {
                match op {
                    // Only allow operations that the kernel may issue without a uid set
                    ll::Operation::Init(_)
                    | ll::Operation::Destroy(_)
                    | ll::Operation::Read(_)
                    | ll::Operation::ReadDir(_)
                    | ll::Operation::BatchForget(_)
                    | ll::Operation::Forget(_)
                    | ll::Operation::Write(_)
                    | ll::Operation::FSync(_)
                    | ll::Operation::FSyncDir(_)
                    | ll::Operation::Release(_)
                    | ll::Operation::ReleaseDir(_) => {}
                    _ => {
                        return Err(Errno::EACCES);
                    }
                }
            }
        }

        let fs = &shared.filesystem;

        match op {
            // Filesystem initialization
            ll::Operation::Init(x) => {
                // We don't support ABI versions before 7.6
                let v = x.version();
                if v < ll::Version(7, 6) {
                    error!("Unsupported FUSE ABI version {v}");
                    return Err(Errno::EPROTO);
                }
                // Remember ABI version supported by kernel
                shared.proto_major.store(v.major(), Ordering::Release);
                shared.proto_minor.store(v.minor(), Ordering::Release);

                let mut config = KernelConfig::new(x.capabilities(), x.max_readahead());
                // Call filesystem init method and give it a chance to return an error
                fs.init(self, &mut config)
                    .await
                    .map_err(Errno::from_i32)?;

                // Reply with our desired version and settings. If the kernel supports a
                // larger major version, it'll re-send a matching init message. If it
                // supports only lower major versions, we replied with an error above.
                debug!(
                    "INIT response: ABI {}.{}, flags {:#x}, max readahead {}, max write {}",
                    abi::FUSE_KERNEL_VERSION,
                    abi::FUSE_KERNEL_MINOR_VERSION,
                    x.capabilities() & config.requested,
                    config.max_readahead,
                    config.max_write
                );
                shared.initialized.store(true, Ordering::Release);
                return Ok(Some(x.reply(&config)));
            }
            // Any operation is invalid before initialization
            _ if !shared.initialized.load(Ordering::Acquire) => {
                warn!("Ignoring FUSE operation before init: {}", self.request);
                return Err(Errno::EIO);
            }
            // Filesystem destroyed
            ll::Operation::Destroy(x) => {
                fs.destroy();
                shared.destroyed.store(true, Ordering::Release);
                return Ok(Some(x.reply()));
            }
            // Any operation is invalid after destroy
            _ if shared.destroyed.load(Ordering::Acquire) => {
                warn!("Ignoring FUSE operation after destroy: {}", self.request);
                return Err(Errno::EIO);
            }

            ll::Operation::Interrupt(_) => {
                // TODO: handle FUSE_INTERRUPT
                return Err(Errno::ENOSYS);
            }

            ll::Operation::Lookup(x) => {
                fs.lookup(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Forget(x) => {
                fs.forget(self, self.request.nodeid().into(), x.nlookup()).await; // no reply
            }
            ll::Operation::GetAttr(_attr) => {
                fs.getattr(
                    self,
                    self.request.nodeid().into(),
                    _attr.file_handle().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::SetAttr(x) => {
                fs.setattr(
                    self,
                    self.request.nodeid().into(),
                    x.mode(),
                    x.uid(),
                    x.gid(),
                    x.size(),
                    x.atime(),
                    x.mtime(),
                    x.ctime(),
                    x.file_handle().map(std::convert::Into::into),
                    x.crtime(),
                    x.chgtime(),
                    x.bkuptime(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::ReadLink(_) => {
                fs.readlink(self, self.request.nodeid().into(), self.reply()).await;
            }
            ll::Operation::MkNod(x) => {
                fs.mknod(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.rdev(),
                    self.reply(),
                ).await;
            }
            ll::Operation::MkDir(x) => {
                fs.mkdir(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Unlink(x) => {
                fs.unlink(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::RmDir(x) => {
                fs.rmdir(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::SymLink(x) => {
                fs.symlink(
                    self,
                    self.request.nodeid().into(),
                    x.link_name().as_ref(),
                    Path::new(x.target()),
                    self.reply(),
                ).await;
            }
            ll::Operation::Rename(x) => {
                fs.rename(
                    self,
                    self.request.nodeid().into(),
                    x.src().name.as_ref(),
                    x.dest().dir.into(),
                    x.dest().name.as_ref(),
                    0,
                    self.reply(),
                ).await;
            }
            ll::Operation::Link(x) => {
                fs.link(
                    self,
                    x.inode_no().into(),
                    self.request.nodeid().into(),
                    x.dest().name.as_ref(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Open(x) => {
                fs.open(self, self.request.nodeid().into(), x.flags(), self.reply()).await;
            }
            ll::Operation::Read(x) => {
                fs.read(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.size(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::Write(x) => {
                fs.write(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.data(),
                    x.write_flags(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    self.reply(),
                ).await;
            }
            ll::Operation::Flush(x) => {
                fs.flush(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Release(x) => {
                fs.release(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.lock_owner().map(std::convert::Into::into),
                    x.flush(),
                    self.reply(),
                ).await;
            }
            ll::Operation::FSync(x) => {
                fs.fsync(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                ).await;
            }
            ll::Operation::OpenDir(x) => {
                fs.opendir(self, self.request.nodeid().into(), x.flags(), self.reply()).await;
            }
            ll::Operation::ReadDir(x) => {
                fs.readdir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectory::new(self.request.unique().into(), self.ch.clone(), x.size() as usize),
                ).await;
            }
            ll::Operation::ReleaseDir(x) => {
                fs.releasedir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::FSyncDir(x) => {
                fs.fsyncdir(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.fdatasync(),
                    self.reply(),
                ).await;
            }
            ll::Operation::StatFs(_) => {
                fs.statfs(self, self.request.nodeid().into(), self.reply()).await;
            }
            ll::Operation::SetXAttr(x) => {
                fs.setxattr(
                    self,
                    self.request.nodeid().into(),
                    x.name(),
                    x.value(),
                    x.flags(),
                    x.position(),
                    self.reply(),
                ).await;
            }
            ll::Operation::GetXAttr(x) => {
                fs.getxattr(
                    self,
                    self.request.nodeid().into(),
                    x.name(),
                    x.size_u32(),
                    self.reply(),
                ).await;
            }
            ll::Operation::ListXAttr(x) => {
                fs.listxattr(self, self.request.nodeid().into(), x.size(), self.reply()).await;
            }
            ll::Operation::RemoveXAttr(x) => {
                fs.removexattr(self, self.request.nodeid().into(), x.name(), self.reply()).await;
            }
            ll::Operation::Access(x) => {
                fs.access(self, self.request.nodeid().into(), x.mask(), self.reply()).await;
            }
            ll::Operation::Create(x) => {
                fs.create(
                    self,
                    self.request.nodeid().into(),
                    x.name().as_ref(),
                    x.mode(),
                    x.umask(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::GetLk(x) => {
                fs.getlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    self.reply(),
                ).await;
            }
            ll::Operation::SetLk(x) => {
                fs.setlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    false,
                    self.reply(),
                ).await;
            }
            ll::Operation::SetLkW(x) => {
                fs.setlk(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.lock_owner().into(),
                    x.lock().range.0,
                    x.lock().range.1,
                    x.lock().typ,
                    x.lock().pid,
                    true,
                    self.reply(),
                ).await;
            }
            ll::Operation::BMap(x) => {
                fs.bmap(
                    self,
                    self.request.nodeid().into(),
                    x.block_size(),
                    x.block(),
                    self.reply(),
                ).await;
            }
            ll::Operation::IoCtl(x) => {
                if x.unrestricted() {
                    return Err(Errno::ENOSYS);
                }
                fs.ioctl(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.flags(),
                    x.command(),
                    x.in_data(),
                    x.out_size(),
                    self.reply(),
                ).await;
            }
            ll::Operation::Poll(x) => {
                let ph = PollHandle::new(self.ch.clone(), x.kernel_handle());

                fs.poll(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    ph,
                    x.events(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            ll::Operation::BatchForget(x) => {
                fs.batch_forget(self, x.nodes()).await; // no reply
            }
            ll::Operation::NotifyReply(_) => {
                // TODO: handle FUSE_NOTIFY_REPLY
                return Err(Errno::ENOSYS);
            }

            #[cfg(feature = "abi-7-19")]
            ll::Operation::FAllocate(x) => {
                fs.fallocate(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.len(),
                    x.mode(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-21")]
            ll::Operation::ReadDirPlus(x) => {
                fs.readdirplus(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    ReplyDirectoryPlus::new(
                        self.request.unique().into(),
                        self.ch.clone(),
                        x.size() as usize,
                    ),
                ).await;
            }
            #[cfg(feature = "abi-7-23")]
            ll::Operation::Rename2(x) => {
                fs.rename(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.flags(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-24")]
            ll::Operation::Lseek(x) => {
                fs.lseek(
                    self,
                    self.request.nodeid().into(),
                    x.file_handle().into(),
                    x.offset(),
                    x.whence(),
                    self.reply(),
                ).await;
            }
            #[cfg(feature = "abi-7-28")]
            ll::Operation::CopyFileRange(x) => {
                let (i, o) = (x.src(), x.dest());
                fs.copy_file_range(
                    self,
                    i.inode.into(),
                    i.file_handle.into(),
                    i.offset,
                    o.inode.into(),
                    o.file_handle.into(),
                    o.offset,
                    x.len(),
                    x.flags().try_into().unwrap(),
                    self.reply(),
                ).await;
            }

            #[cfg(target_os = "macos")]
            ll::Operation::SetVolName(x) => {
                fs.setvolname(self, x.name(), self.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::GetXTimes(_) => {
                fs.getxtimes(self, self.request.nodeid().into(), self.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::Exchange(x) => {
                fs.exchange(
                    self,
                    x.from().dir.into(),
                    x.from().name.as_ref(),
                    x.to().dir.into(),
                    x.to().name.as_ref(),
                    x.options(),
                    self.reply(),
                ).await;
            }

            ll::Operation::CuseInit(_) => {
                // TODO: handle CUSE_INIT
                return Err(Errno::ENOSYS);
            }
        }
        Ok(None)
    }
}

/// Dispatch a request using shared session state and owned buffer data.
/// This enables parallel request processing by spawning async tasks.
/// Returns Some(true) if this was an INIT request, Some(false) for other requests,
/// or None if the request was invalid.
pub(crate) async fn dispatch_request<FS: Filesystem + 'static>(
    shared: Arc<SharedSessionState<FS>>,
    data: Vec<u8>,
) -> Option<bool> {
    // Create the request from owned data - the data will live for the duration of this function
    let request = match ll::AnyRequest::try_from(data.as_slice()) {
        Ok(request) => request,
        Err(err) => {
            error!("{err}");
            return None;
        }
    };

    // Create a Request that borrows from the data
    let req = Request {
        ch: shared.sender.clone(),
        data: data.as_slice(),
        request,
    };

    // Dispatch and return whether this was init
    let is_init = req.dispatch_shared(&shared).await;
    Some(is_init)
}
