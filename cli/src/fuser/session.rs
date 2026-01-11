//! Filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use libc::{EAGAIN, EINTR, ENODEV, ENOENT};
use log::{debug, info, warn};
use nix::unistd::geteuid;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use tokio::runtime::Runtime;

use super::ll::fuse_abi as abi;
use super::request::{dispatch_request, Request};
use super::Filesystem;
use super::MountOption;
use super::{channel::Channel, mnt::Mount};
use super::{channel::ChannelSender, notify::Notifier};

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. The FUSE default is 16M on macOS
/// and 128k on other systems.
pub const MAX_WRITE_SIZE: usize = 16 * 1024 * 1024;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to `MAX_WRITE_SIZE` bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: usize = MAX_WRITE_SIZE + 4096;

#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
/// How requests should be filtered based on the calling UID.
pub enum SessionACL {
    /// Allow requests from any user. Corresponds to the `allow_other` mount option.
    All,
    /// Allow requests from root. Corresponds to the `allow_root` mount option.
    RootAndOwner,
    /// Allow requests from the owning UID. This is FUSE's default mode of operation.
    #[default]
    Owner,
}

/// Shared session state for concurrent request handling.
/// This allows multiple async tasks to access session state safely.
pub struct SharedSessionState<FS: Filesystem> {
    /// Filesystem operation implementations (shared via Arc)
    pub(crate) filesystem: Arc<FS>,
    /// Channel sender for replies (cloneable)
    pub(crate) sender: ChannelSender,
    /// Whether to restrict access to owner, root + owner, or unrestricted
    pub(crate) allowed: SessionACL,
    /// User that launched the fuser process
    pub(crate) session_owner: u32,
    /// FUSE protocol major version
    pub(crate) proto_major: AtomicU32,
    /// FUSE protocol minor version
    pub(crate) proto_minor: AtomicU32,
    /// True if the filesystem is initialized (init operation done)
    pub(crate) initialized: AtomicBool,
    /// True if the filesystem was destroyed (destroy operation done)
    pub(crate) destroyed: AtomicBool,
}

/// The session data structure
#[derive(Debug)]
pub struct Session<FS: Filesystem> {
    /// Filesystem operation implementations
    pub(crate) filesystem: FS,
    /// Communication channel to the kernel driver
    pub(crate) ch: Channel,
    /// Handle to the mount.  Dropping this unmounts.
    mount: Arc<Mutex<Option<(PathBuf, Mount)>>>,
    /// Whether to restrict access to owner, root + owner, or unrestricted
    /// Used to implement `allow_root` and `auto_unmount`
    pub(crate) allowed: SessionACL,
    /// User that launched the fuser process
    pub(crate) session_owner: u32,
    /// FUSE protocol major version
    pub(crate) proto_major: u32,
    /// FUSE protocol minor version
    pub(crate) proto_minor: u32,
    /// True if the filesystem is initialized (init operation done)
    pub(crate) initialized: bool,
    /// True if the filesystem was destroyed (destroy operation done)
    pub(crate) destroyed: bool,
}

impl<FS: Filesystem> AsFd for Session<FS> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.ch.as_fd()
    }
}

impl<FS: Filesystem + 'static> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    /// # Errors
    /// Returns an error if the options are incorrect, or if the fuse device can't be mounted.
    pub fn new<P: AsRef<Path>>(
        filesystem: FS,
        mountpoint: P,
        options: &[MountOption],
    ) -> io::Result<Session<FS>> {
        let mountpoint = mountpoint.as_ref();
        info!("Mounting {}", mountpoint.display());
        // If AutoUnmount is requested, but not AllowRoot or AllowOther we enforce the ACL
        // ourself and implicitly set AllowOther because fusermount needs allow_root or allow_other
        // to handle the auto_unmount option
        let (file, mount) = if options.contains(&MountOption::AutoUnmount)
            && !(options.contains(&MountOption::AllowRoot)
                || options.contains(&MountOption::AllowOther))
        {
            warn!(
                "Given auto_unmount without allow_root or allow_other; adding allow_other, with userspace permission handling"
            );
            let mut modified_options = options.to_vec();
            modified_options.push(MountOption::AllowOther);
            Mount::new(mountpoint, &modified_options)?
        } else {
            Mount::new(mountpoint, options)?
        };

        let ch = Channel::new(file);
        let allowed = if options.contains(&MountOption::AllowRoot) {
            SessionACL::RootAndOwner
        } else if options.contains(&MountOption::AllowOther) {
            SessionACL::All
        } else {
            SessionACL::Owner
        };

        Ok(Session {
            filesystem,
            ch,
            mount: Arc::new(Mutex::new(Some((mountpoint.to_owned(), mount)))),
            allowed,
            session_owner: geteuid().as_raw(),
            proto_major: 0,
            proto_minor: 0,
            initialized: false,
            destroyed: false,
        })
    }

    /// Wrap an existing /dev/fuse file descriptor. This doesn't mount the
    /// filesystem anywhere; that must be done separately.
    pub fn from_fd(filesystem: FS, fd: OwnedFd, acl: SessionACL) -> Self {
        let ch = Channel::new(Arc::new(fd.into()));
        Session {
            filesystem,
            ch,
            mount: Arc::new(Mutex::new(None)),
            allowed: acl,
            session_owner: geteuid().as_raw(),
            proto_major: 0,
            proto_minor: 0,
            initialized: false,
            destroyed: false,
        }
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. Requests are processed in parallel by spawning async
    /// tasks for each incoming request.
    /// # Errors
    /// Returns any final error when the session comes to an end.
    pub fn run(&mut self, runtime: &Runtime) -> io::Result<()> {
        // Create shared state for concurrent request handling
        let shared = Arc::new(SharedSessionState {
            filesystem: Arc::new(unsafe {
                // SAFETY: We're moving the filesystem into Arc. The Session will not
                // be used after run() returns, and we reconstruct it for destroy().
                std::ptr::read(&self.filesystem)
            }),
            sender: self.ch.sender(),
            allowed: self.allowed.clone(),
            session_owner: self.session_owner,
            proto_major: AtomicU32::new(0),
            proto_minor: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
        });

        // Process the first request (INIT) synchronously to establish protocol version
        let mut buffer = vec![0; BUFFER_SIZE];
        let buf = aligned_sub_buf(&mut buffer, std::mem::align_of::<abi::fuse_in_header>());

        // Handle INIT synchronously - must complete before parallel processing
        loop {
            match self.ch.receive(buf) {
                Ok(size) => {
                    let data = buf[..size].to_vec(); // Copy for owned request
                    if let Some(is_init) = runtime.block_on(dispatch_request(shared.clone(), data)) {
                        if is_init {
                            // INIT completed, now we can process requests in parallel
                            break;
                        }
                    } else {
                        // Invalid request before init
                        continue;
                    }
                }
                Err(err) => match err.raw_os_error() {
                    Some(ENOENT | EINTR | EAGAIN) => continue,
                    Some(ENODEV) => return Ok(()),
                    _ => return Err(err),
                },
            }
        }

        debug!("FUSE initialized, entering parallel request processing");

        // Main loop: spawn tasks for parallel request handling
        loop {
            match self.ch.receive(buf) {
                Ok(size) => {
                    // Copy buffer data so the task owns it
                    let data = buf[..size].to_vec();
                    let shared = shared.clone();

                    // Spawn async task to handle this request
                    runtime.spawn(async move {
                        dispatch_request(shared, data).await;
                    });
                }
                Err(err) => match err.raw_os_error() {
                    Some(ENOENT | EINTR | EAGAIN) => continue,
                    Some(ENODEV) => break,
                    _ => return Err(err),
                },
            }
        }

        // Wait for any in-flight requests and call destroy
        // Note: In practice, ENODEV means the mount is gone, so requests should complete quickly
        if !shared.destroyed.load(Ordering::Acquire) {
            shared.filesystem.destroy();
            shared.destroyed.store(true, Ordering::Release);
        }

        // Prevent double-drop of filesystem
        std::mem::forget(unsafe { std::ptr::read(&self.filesystem) });
        self.destroyed = true;

        Ok(())
    }

    /// Unmount the filesystem
    pub fn unmount(&mut self) {
        drop(std::mem::take(&mut *self.mount.lock().unwrap()));
    }

    /// Returns a thread-safe object that can be used to unmount the Filesystem
    pub fn unmount_callable(&mut self) -> SessionUnmounter {
        SessionUnmounter {
            mount: self.mount.clone(),
        }
    }

    /// Returns an object that can be used to send notifications to the kernel
    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.ch.sender())
    }
}

#[derive(Debug)]
/// A thread-safe object that can be used to unmount a Filesystem
pub struct SessionUnmounter {
    mount: Arc<Mutex<Option<(PathBuf, Mount)>>>,
}

impl SessionUnmounter {
    /// Unmount the filesystem
    pub fn unmount(&mut self) -> io::Result<()> {
        drop(std::mem::take(&mut *self.mount.lock().unwrap()));
        Ok(())
    }
}

fn aligned_sub_buf(buf: &mut [u8], alignment: usize) -> &mut [u8] {
    let off = alignment - (buf.as_ptr() as usize) % alignment;
    if off == alignment {
        buf
    } else {
        &mut buf[off..]
    }
}

impl<FS: 'static + Filesystem + Send> Session<FS> {
    /// Run the session loop in a background thread
    pub fn spawn(self, runtime: Arc<Runtime>) -> io::Result<BackgroundSession> {
        BackgroundSession::new(self, runtime)
    }
}

impl<FS: Filesystem> Drop for Session<FS> {
    fn drop(&mut self) {
        if !self.destroyed {
            self.filesystem.destroy();
            self.destroyed = true;
        }

        if let Some((mountpoint, _mount)) = std::mem::take(&mut *self.mount.lock().unwrap()) {
            info!("unmounting session at {}", mountpoint.display());
        }
    }
}

/// The background session data structure
#[derive(Debug)]
pub struct BackgroundSession {
    /// Thread guard of the background session
    pub guard: JoinHandle<io::Result<()>>,
    /// Object for creating Notifiers for client use
    sender: ChannelSender,
    /// Ensures the filesystem is unmounted when the session ends
    _mount: Option<Mount>,
}

impl BackgroundSession {
    /// Create a new background session for the given session by running its
    /// session loop in a background thread. If the returned handle is dropped,
    /// the filesystem is unmounted and the given session ends.
    pub fn new<FS: Filesystem + Send + 'static>(se: Session<FS>, runtime: Arc<Runtime>) -> io::Result<BackgroundSession> {
        let sender = se.ch.sender();
        // Take the fuse_session, so that we can unmount it
        let mount = std::mem::take(&mut *se.mount.lock().unwrap()).map(|(_, mount)| mount);
        let guard = thread::spawn(move || {
            let mut se = se;
            se.run(&runtime)
        });
        Ok(BackgroundSession {
            guard,
            sender,
            _mount: mount,
        })
    }
    /// Unmount the filesystem and join the background thread.
    /// # Panics
    /// Panics if the background thread can't be recovered (e.g., because it panicked).
    pub fn join(self) {
        let Self {
            guard,
            sender: _,
            _mount,
        } = self;
        drop(_mount);
        guard.join().unwrap().unwrap();
    }

    /// Returns an object that can be used to send notifications to the kernel
    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.sender.clone())
    }
}
