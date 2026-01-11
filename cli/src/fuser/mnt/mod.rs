//! FUSE kernel driver communication
//!
//! Raw communication channel to the FUSE kernel driver.
//! This is a Linux-only pure-Rust implementation.

mod fuse_pure;
pub mod mount_options;

use std::fs::File;
use std::io;

pub use fuse_pure::Mount;
use std::ffi::CStr;

#[inline]
fn libc_umount(mnt: &CStr) -> io::Result<()> {
    let r = unsafe { libc::umount(mnt.as_ptr()) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Warning: This will return true if the filesystem has been detached (lazy unmounted), but not
/// yet destroyed by the kernel.
pub(crate) fn is_mounted(fuse_device: &File) -> bool {
    use libc::{poll, pollfd};
    use std::os::unix::prelude::AsRawFd;

    let mut poll_result = pollfd {
        fd: fuse_device.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    loop {
        let res = unsafe { poll(&mut poll_result, 1, 0) };
        break match res {
            0 => true,
            1 => (poll_result.revents & libc::POLLERR) != 0,
            -1 => {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("Poll failed with error {err}")
            }
            _ => unreachable!(),
        };
    }
}
