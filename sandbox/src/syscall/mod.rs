pub mod file;
pub mod process;
pub mod stat;
pub mod xattr;

use crate::{
    sandbox::Sandbox,
    vfs::{fdtable::FdTable, mount::MountTable},
};
use reverie::{
    syscalls::{MemoryAccess, PathPtr, ReadAddr, Syscall},
    Error, Guest, Stack,
};
use std::{ffi::CString, path::PathBuf};

/// Common path translation logic for syscalls.
///
/// This function encapsulates the common pattern used by all path-based syscall
/// handlers. It performs the following operations:
///
/// 1. Reads the original path from guest memory
/// 2. Resolves the path through the mount table to get the translated host path
/// 3. Converts the translated path to a C string
/// 4. Allocates space on the guest stack for the new path
/// 5. Writes the translated path to guest memory
/// 6. Returns the new path address for use in the modified syscall
///
/// # Arguments
/// * `guest` - The guest process being traced
/// * `path_addr` - The address of the original path in guest memory
/// * `mount_table` - The mount table for path resolution
///
/// # Returns
/// * `Ok(Some(addr))` - Path was successfully translated, returns new address
/// * `Ok(None)` - Path does not need translation (no matching mount point)
/// * `Err(e)` - An error occurred during translation
///
/// # Safety
/// This function allocates memory on the guest stack and writes to guest memory.
/// The allocated memory is automatically cleaned up when the syscall returns,
/// as the guest process unwinds its own stack frame.
pub(crate) async fn translate_path<'a, T: Guest<Sandbox>>(
    guest: &'a mut T,
    path_addr: PathPtr<'a>,
    mount_table: &MountTable,
) -> Result<Option<PathPtr<'a>>, Error> {
    // Read the original path from guest memory
    let path: PathBuf = path_addr.read(&guest.memory())?;

    // Only process valid UTF-8 paths
    if path.to_str().is_none() {
        return Ok(None);
    }

    // Resolve through mount table to get the translated host path
    let (_vfs, translated_path) = match mount_table.resolve(&path) {
        Some(result) => result,
        None => return Ok(None), // No mount point matches, use original path
    };

    // Convert translated path to a C string for the syscall
    let new_path_str = translated_path.to_string_lossy().to_string();
    let new_path_cstr = CString::new(new_path_str).map_err(|_| reverie::syscalls::Errno::EINVAL)?;

    // Allocate space on the guest stack and write the new path
    let bytes = new_path_cstr.as_bytes_with_nul();
    let mut stack = guest.stack().await;
    let addr: reverie::syscalls::AddrMut<PathBuf> = stack.reserve();
    stack.commit()?;

    let byte_addr = addr.cast::<u8>();
    guest.memory().write_exact(byte_addr, bytes)?;

    // SAFETY: The transmute converts AddrMut<u8> to PathPtr<'a>.
    // This is safe because:
    // 1. We just allocated `byte_addr` from the guest stack via stack.reserve()
    // 2. We wrote a valid null-terminated C string to this address
    // 3. Reverie treats these pointer types as thin wrappers around raw pointers
    // 4. PathPtr is a newtype around CStrPtr, which is compatible with a char* pointer
    // 5. The guest will read this as a const char* pointer for the syscall path argument
    Ok(Some(unsafe {
        std::mem::transmute::<reverie::syscalls::AddrMut<'_, u8>, reverie::syscalls::PathPtr<'_>>(
            byte_addr,
        )
    }))
}

/// System call dispatch.
///
/// This function dispatches a system call to the appropriate handler if the
/// system call needs to be intercepted.
///
/// Returns either:
/// - `SyscallResult::Value(i64)` if the handler executed the syscall and has a return value
/// - `SyscallResult::Syscall(Syscall)` if a syscall should be injected (either modified or original)
pub async fn dispatch_syscall<T: Guest<Sandbox>>(
    guest: &mut T,
    syscall: Syscall,
    mount_table: &MountTable,
    fd_table: &FdTable,
) -> Result<SyscallResult, Error> {
    // FIXME: We need to intercept all system calls that use a path or file descriptor.
    match &syscall {
        Syscall::Openat(args) => {
            if let Some(result) = file::handle_openat(guest, args, mount_table, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Read(args) => file::handle_read(guest, syscall, args, fd_table).await,
        Syscall::Write(args) => file::handle_write(guest, syscall, args, fd_table).await,
        Syscall::Close(args) => file::handle_close(guest, syscall, args, fd_table).await,
        Syscall::Dup(args) => {
            if let Some(result) = file::handle_dup(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Dup2(args) => {
            if let Some(result) = file::handle_dup2(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Dup3(args) => {
            if let Some(result) = file::handle_dup3(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Rmdir(args) => {
            if let Some(modified) = file::handle_rmdir(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(target_arch = "aarch64")]
        Syscall::Unlinkat(args) => {
            if let Some(modified) = file::handle_unlinkat(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Fork(args) => {
            if let Some(result) = process::handle_fork(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Vfork(args) => {
            if let Some(result) = process::handle_vfork(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Clone(args) => {
            if let Some(result) = process::handle_clone(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Clone3(args) => {
            if let Some(result) = process::handle_clone3(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Statx(args) => {
            if let Some(result) = stat::handle_statx(guest, args, mount_table, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Newfstatat(args) => {
            if let Some(result) =
                stat::handle_newfstatat(guest, args, mount_table, fd_table).await?
            {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Statfs(args) => {
            if let Some(modified) = stat::handle_statfs(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Readlink(args) => {
            if let Some(result) = stat::handle_readlink(guest, args, mount_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Readlinkat(args) => {
            if let Some(result) =
                stat::handle_readlinkat(guest, args, mount_table, fd_table).await?
            {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Symlink(args) => {
            if let Some(result) = stat::handle_symlink(guest, args, mount_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Symlinkat(args) => {
            if let Some(result) = stat::handle_symlinkat(guest, args, mount_table, fd_table).await?
            {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Llistxattr(args) => {
            if let Some(modified) = xattr::handle_llistxattr(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Lgetxattr(args) => {
            if let Some(modified) = xattr::handle_lgetxattr(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Ioctl(args) => {
            if let Some(result) = file::handle_ioctl(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Fcntl(args) => {
            if let Some(result) = file::handle_fcntl(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Pselect6(args) => {
            if let Some(result) = file::handle_pselect6(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Poll(args) => {
            if let Some(result) = file::handle_poll(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Getdents64(args) => file::handle_getdents64(guest, syscall, args, fd_table).await,
        Syscall::Fstat(args) => file::handle_fstat(guest, syscall, args, fd_table).await,
        #[cfg(target_arch = "aarch64")]
        Syscall::Fstatat(args) => {
            file::handle_fstatat(guest, syscall, args, fd_table, mount_table).await
        }
        Syscall::Pread64(args) => {
            if let Some(result) = file::handle_pread64(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Pwrite64(args) => {
            if let Some(result) = file::handle_pwrite64(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Lseek(args) => file::handle_lseek(guest, syscall, args, fd_table).await,
        Syscall::Readv(args) => {
            if let Some(result) = file::handle_readv(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Writev(args) => {
            if let Some(result) = file::handle_writev(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Pipe2(args) => {
            if let Some(result) = file::handle_pipe2(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Socket(args) => {
            if let Some(result) = file::handle_socket(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Sendto(args) => {
            if let Some(result) = file::handle_sendto(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Connect(args) => {
            if let Some(result) = file::handle_connect(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Getpeername(args) => {
            if let Some(result) = file::handle_getpeername(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        // Signal-related syscalls - passthrough (no fd/path interception needed)
        Syscall::RtSigaction(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::RtSigprocmask(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::RtSigreturn(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Sigaltstack(_) => Ok(SyscallResult::Syscall(syscall)),
        // Process execution and termination - passthrough
        Syscall::Execve(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Execveat(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Exit(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::ExitGroup(_) => Ok(SyscallResult::Syscall(syscall)),
        // Process information - passthrough
        Syscall::Getpid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getppid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Gettid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getuid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Geteuid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getgid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getegid(_) => Ok(SyscallResult::Syscall(syscall)),
        // Wait syscalls - passthrough
        Syscall::Wait4(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Waitid(_) => Ok(SyscallResult::Syscall(syscall)),
        // Memory management
        Syscall::Brk(_) => Ok(SyscallResult::Syscall(syscall)),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::ArchPrctl(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Mmap(args) => {
            if let Some(result) = file::handle_mmap(guest, args, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Munmap(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Mprotect(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Mremap(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Madvise(_) => Ok(SyscallResult::Syscall(syscall)),
        // Path-based file operations
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Access(args) => {
            if let Some(modified) = file::handle_access(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Faccessat(args) => {
            if let Some(result) = file::handle_faccessat(guest, args, mount_table, fd_table).await?
            {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Rename(args) => {
            if let Some(modified) = file::handle_rename(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Unlink(args) => {
            if let Some(modified) = file::handle_unlink(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Chdir(args) => {
            if let Some(modified) = file::handle_chdir(guest, args, mount_table).await? {
                Ok(SyscallResult::Syscall(modified))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        Syscall::Fchownat(args) => {
            if let Some(result) = file::handle_fchownat(guest, args, mount_table, fd_table).await? {
                Ok(SyscallResult::Value(result))
            } else {
                Ok(SyscallResult::Syscall(syscall))
            }
        }
        // Threading and synchronization - passthrough
        Syscall::SetTidAddress(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::SetRobustList(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Futex(_) => Ok(SyscallResult::Syscall(syscall)),
        // Time - passthrough
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Time(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::ClockGettime(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::ClockGetres(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Gettimeofday(_) => Ok(SyscallResult::Syscall(syscall)),
        // Random - passthrough
        Syscall::Getrandom(_) => Ok(SyscallResult::Syscall(syscall)),
        // Resource limits - passthrough
        Syscall::Prlimit64(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getrlimit(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Setrlimit(_) => Ok(SyscallResult::Syscall(syscall)),
        // Signals - passthrough
        Syscall::Tgkill(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Tkill(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Kill(_) => Ok(SyscallResult::Syscall(syscall)),
        // System information - passthrough
        Syscall::Uname(_) => Ok(SyscallResult::Syscall(syscall)),
        #[cfg(not(target_arch = "aarch64"))]
        Syscall::Getpgrp(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Getpgid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Setpgid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Setsid(_) => Ok(SyscallResult::Syscall(syscall)),
        // Permission management - passthrough
        Syscall::Setfsuid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Setfsgid(_) => Ok(SyscallResult::Syscall(syscall)),
        Syscall::Umask(_) => Ok(SyscallResult::Syscall(syscall)),
        // Process control - passthrough
        Syscall::Prctl(_) => Ok(SyscallResult::Syscall(syscall)),
        // Handle specific "Other" syscalls by syscall number
        Syscall::Other(num, args) => {
            use reverie::syscalls::Sysno;
            match *num {
                Sysno::rseq => Ok(SyscallResult::Syscall(syscall)), // rseq - passthrough
                Sysno::lseek => Ok(SyscallResult::Syscall(syscall)),
                Sysno::fchmodat => {
                    if let Some(result) = file::handle_chmod(guest, args, mount_table).await? {
                        Ok(SyscallResult::Value(result))
                    } else {
                        Ok(SyscallResult::Syscall(syscall))
                    }
                }
                Sysno::faccessat2 => {
                    if let Some(result) =
                        file::handle_faccessat2(guest, args, mount_table, fd_table).await?
                    {
                        Ok(SyscallResult::Value(result))
                    } else {
                        Ok(SyscallResult::Syscall(syscall))
                    }
                }
                _ => {
                    eprintln!("WARNING: Unsupported syscall: {:?}", num);
                    Err(Error::Errno(reverie::syscalls::Errno::ENOSYS))
                }
            }
        }
        _ => {
            eprintln!("WARNING: Unsupported syscall: {:?}", syscall);
            Err(Error::Errno(reverie::syscalls::Errno::ENOSYS))
        }
    }
}

/// Result of a syscall handler
pub enum SyscallResult {
    /// Handler executed the syscall and returned a value
    Value(i64),
    /// Handler modified the syscall, which should be tail-injected
    Syscall(Syscall),
}
