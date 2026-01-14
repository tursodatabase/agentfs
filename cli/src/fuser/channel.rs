use std::{
    fs::File,
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd},
        unix::prelude::OwnedFd,
    },
    sync::Arc,
};

use libc::{c_int, c_void, size_t};
use tokio::io::unix::AsyncFd;

use super::reply::ReplySender;

/// Size of the buffer for reading a request from the kernel.
pub const BUFFER_SIZE: usize = super::session::MAX_WRITE_SIZE + 4096;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug)]
pub struct Channel(Arc<File>);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<File>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let rc = unsafe {
            libc::read(
                self.0.as_raw_fd(),
                buffer.as_ptr() as *mut c_void,
                buffer.len() as size_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc as usize)
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone())
    }

    /// Create an async channel from this channel for use with tokio.
    /// This duplicates the underlying file descriptor so the original channel
    /// remains valid.
    pub fn to_async(&self) -> io::Result<AsyncChannel> {
        let fd = self.0.as_raw_fd();
        unsafe {
            // Duplicate the fd for async use
            let dup_fd = libc::dup(fd);
            if dup_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // Set non-blocking mode on the duplicated fd
            let flags = libc::fcntl(dup_fd, libc::F_GETFL);
            if flags < 0 {
                let err = io::Error::last_os_error();
                libc::close(dup_fd);
                return Err(err);
            }
            if libc::fcntl(dup_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                let err = io::Error::last_os_error();
                libc::close(dup_fd);
                return Err(err);
            }
            let async_fd = AsyncFd::new(OwnedFd::from_raw_fd(dup_fd))?;
            Ok(AsyncChannel { inner: async_fd })
        }
    }
}

/// An async communication channel to the FUSE kernel driver.
/// Uses tokio's AsyncFd for non-blocking I/O.
#[derive(Debug)]
pub struct AsyncChannel {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncChannel {
    /// Receives data asynchronously, returning the size read and a buffer containing the data.
    pub async fn receive(&self) -> io::Result<(usize, Vec<u8>)> {
        let mut buffer = vec![0u8; BUFFER_SIZE];

        loop {
            let mut guard = self.inner.readable().await?;

            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let rc = unsafe {
                    libc::read(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len() as size_t)
                };
                if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(rc as usize)
                }
            }) {
                Ok(Ok(size)) => return Ok((size, buffer)),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Returns a sender object for this channel.
    pub fn sender(&self) -> ChannelSender {
        let fd = self.inner.get_ref().as_raw_fd();
        // Create a new Arc<File> from the fd (note: this doesn't duplicate the fd,
        // but since writev is thread-safe and we're only writing, this is safe)
        let file = unsafe { Arc::new(File::from_raw_fd(libc::dup(fd))) };
        ChannelSender(file)
    }
}

#[derive(Clone, Debug)]
pub struct ChannelSender(Arc<File>);

impl ReplySender for ChannelSender {
    fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let rc = unsafe {
            libc::writev(
                self.0.as_raw_fd(),
                bufs.as_ptr() as *const libc::iovec,
                bufs.len() as c_int,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc as usize);
            Ok(())
        }
    }
}
