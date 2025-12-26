use crate::{sandbox, sandbox::Sandbox, vfs::fdtable::FdTable};
use reverie::{syscalls::Syscall, Error, Guest};

/// The `fork` system call.
///
/// This intercepts `fork` system calls to properly handle FD table inheritance.
/// The child process gets a deep copy of the parent's FD table.
#[cfg(not(target_arch = "aarch64"))]
pub async fn handle_fork<T: Guest<Sandbox>>(
    guest: &mut T,
    _args: &reverie::syscalls::Fork,
    parent_fd_table: &FdTable,
) -> Result<Option<i64>, Error> {
    // Execute the fork syscall
    let result = guest
        .inject(Syscall::Fork(reverie::syscalls::Fork::new()))
        .await?;

    if result > 0 {
        // We are in the parent process - result is the child PID
        // Create a deep copy of our FD table for the child
        let child_fd_table = parent_fd_table.deep_clone();
        sandbox::insert_fd_table(result as i32, child_fd_table);
    }
    // If result == 0, we're in the child - the FD table was already set up by the parent
    // If result < 0, fork failed - no action needed

    Ok(Some(result))
}

/// The `vfork` system call.
///
/// This intercepts `vfork` system calls. vfork shares the address space with the parent
/// until exec, so we share the FD table (shallow copy) rather than deep copying.
///
/// Note: In practice, modern Linux kernels implement vfork identically to fork with
/// copy-on-write, so we treat it the same as fork for FD table purposes.
#[cfg(not(target_arch = "aarch64"))]
pub async fn handle_vfork<T: Guest<Sandbox>>(
    guest: &mut T,
    _args: &reverie::syscalls::Vfork,
    parent_fd_table: &FdTable,
) -> Result<Option<i64>, Error> {
    // Execute the vfork syscall
    let result = guest
        .inject(Syscall::Vfork(reverie::syscalls::Vfork::new()))
        .await?;

    if result > 0 {
        // Parent process - create a deep copy for the child
        // (Even though vfork semantics suggest sharing, it's safer to deep copy
        // since the child will exec or exit, and we need independent FD tracking)
        let child_fd_table = parent_fd_table.deep_clone();
        sandbox::insert_fd_table(result as i32, child_fd_table);
    }

    Ok(Some(result))
}

/// The `clone` system call.
///
/// This intercepts `clone` system calls. Clone behavior depends on flags:
/// - CLONE_FILES: child shares FD table with parent (shallow copy)
/// - No CLONE_FILES: child gets independent FD table (deep copy, like fork)
///
/// Note: CLONE_FILES is typically used for threads, while process clones
/// without CLONE_FILES behave like fork.
pub async fn handle_clone<T: Guest<Sandbox>>(
    guest: &mut T,
    args: &reverie::syscalls::Clone,
    parent_fd_table: &FdTable,
) -> Result<Option<i64>, Error> {
    // Execute the clone syscall
    let result = guest.inject(Syscall::Clone(*args)).await?;

    if result > 0 {
        // We are in the parent process - result is the child PID/TID
        // Check if CLONE_FILES flag is set (0x00000400)
        const CLONE_FILES: i32 = 0x00000400;

        let flags = args.flags();
        let share_fds = flags.bits() & CLONE_FILES != 0;

        if share_fds {
            // CLONE_FILES set - share the FD table (shallow copy)
            // Both parent and child will see the same FdTable Arc
            sandbox::insert_fd_table(result as i32, parent_fd_table.clone());
        } else {
            // CLONE_FILES not set - create independent FD table (deep copy)
            let child_fd_table = parent_fd_table.deep_clone();
            sandbox::insert_fd_table(result as i32, child_fd_table);
        }
    }
    // If result == 0, we're in the child - FD table already set up by parent
    // If result < 0, clone failed

    Ok(Some(result))
}

/// The `clone3` system call.
///
/// This is the modern clone interface. We need to parse the clone_args structure
/// to determine the flags.
pub async fn handle_clone3<T: Guest<Sandbox>>(
    guest: &mut T,
    args: &reverie::syscalls::Clone3,
    parent_fd_table: &FdTable,
) -> Result<Option<i64>, Error> {
    // Execute the clone3 syscall
    let result = guest.inject(Syscall::Clone3(*args)).await?;

    if result > 0 {
        // Parent process - result is child PID/TID
        // For clone3, we'd need to read the clone_args structure from memory
        // to get the flags. For now, we default to deep copy (safer).
        // TODO: Parse clone_args to check CLONE_FILES flag
        let child_fd_table = parent_fd_table.deep_clone();
        sandbox::insert_fd_table(result as i32, child_fd_table);
    }

    Ok(Some(result))
}
