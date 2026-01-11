//! Overlay sandbox using FUSE and Linux namespaces.
//!
//! This module provides a sandboxed execution environment where the current
//! working directory becomes a copy-on-write overlay, and the rest of the
//! filesystem is read-only. All modifications are captured in an AgentFS
//! database, leaving the original files untouched.
//!
//! The implementation mounts a FUSE filesystem on a hidden temporary directory,
//! then uses a child process with its own mount namespace to bind-mount the
//! overlay onto the working directory. This isolation ensures the overlay is
//! only visible to the sandboxed process and its children.
//!
//! To avoid a circular reference (FUSE serving from a directory it's mounted
//! on), we open a file descriptor to the working directory before mounting.
//! The HostFS base layer then accesses files through `/proc/self/fd/N`,
//! bypassing the FUSE mount entirely.

use super::group_paths_by_parent;
use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{bail, Context, Result};
use std::{
    cmp::Reverse,
    ffi::CString,
    fs,
    io::BufRead,
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
};

/// Global child PID for signal forwarding.
/// Set by the parent before installing signal handlers.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Counter for termination signals received.
/// First signal forwards to child, second signal sends SIGKILL.
static TERM_SIGNAL_COUNT: AtomicI32 = AtomicI32::new(0);

use crate::fuse::FuseMountOptions;

/// Exit code returned when exec fails (standard shell convention for "command not found")
const EXIT_COMMAND_NOT_FOUND: i32 = 127;

/// Timeout for waiting for FUSE mount to become ready
const FUSE_MOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Virtual filesystems that must remain writable for system operation.
/// These are skipped when remounting the filesystem hierarchy as read-only.
const SKIP_MOUNT_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/tmp"];

/// Default directories that are allowed to be writable.
/// These are common application config/cache directories that many programs need.
const DEFAULT_ALLOWED_DIRS: &[&str] = &[
    ".amp",         // Amp config
    ".cache",       // XDG cache directory (corepack, pip, etc.)
    ".claude",      // Claude Code config
    ".claude.json", // Claude Code config file
    ".codex",       // OpenAI Codex config
    ".gemini",      // Gemini CLI config
    ".local",       // Local data directory
    ".npm",         // npm local registry
];

/// Field index for mount point in /proc/self/mountinfo.
/// Format: ID PARENT_ID MAJOR:MINOR ROOT MOUNT_POINT OPTIONS ...
const MOUNTINFO_MOUNT_POINT_FIELD: usize = 4;

/// Commands to try for FUSE unmounting, in order of preference.
/// fusermount3 is from fuse3 package; fusermount is the legacy fallback.
const FUSERMOUNT_COMMANDS: &[&str] = &["fusermount3", "fusermount"];

/// Signal handler that forwards signals to the child process.
///
/// When the parent receives SIGTERM or SIGINT, this handler forwards
/// the signal to the child process so it can shut down gracefully.
/// On the second signal, SIGKILL is sent to force termination (handles
/// cases where the child ignores SIGTERM, like interactive bash).
///
/// SAFETY: This is a signal handler. It must only use async-signal-safe functions.
/// kill() and atomic operations are async-signal-safe.
extern "C" fn forward_signal_to_child(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // Increment signal counter (fetch_add returns previous value)
        let count = TERM_SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst);

        // SAFETY: kill() is async-signal-safe
        unsafe {
            if count == 0 {
                // First signal: forward to child gracefully
                libc::kill(pid, sig);
            } else {
                // Second+ signal: force kill the child
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// Install signal handlers to forward SIGTERM and SIGINT to the child process.
///
/// This ensures that when the parent receives a termination signal, it forwards
/// it to the child and waits for it to exit before cleaning up.
fn install_signal_handlers() {
    // Reset the signal counter for fresh signal handling
    TERM_SIGNAL_COUNT.store(0, Ordering::SeqCst);

    // SAFETY: sigaction() and sigprocmask() with valid signal numbers are safe.
    // SA_RESTART ensures most syscalls restart after the handler returns.
    unsafe {
        // Ensure SIGTERM and SIGINT are not blocked (tokio might block them in worker threads)
        let mut sigset: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut sigset);
        libc::sigaddset(&mut sigset, libc::SIGTERM);
        libc::sigaddset(&mut sigset, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &sigset, std::ptr::null_mut());

        let mut sa: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_sigaction = forward_signal_to_child as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;

        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            panic!(
                "failed to install SIGTERM handler: {}",
                std::io::Error::last_os_error()
            );
        }
        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0 {
            panic!(
                "failed to install SIGINT handler: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Run a command in an overlay sandbox.
pub async fn run_cmd(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    session_id: Option<String>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;

    // Build the list of allowed writable paths
    let allowed_paths = build_allowed_paths(&allow, no_default_allows)?;

    // Check if we're joining an existing session
    let session = setup_run_directory(session_id)?;

    // If the FUSE mountpoint is already mounted, join the existing session
    if is_mountpoint(&session.fuse_mountpoint) {
        eprintln!("Joining existing session: {}", session.run_id);
        eprintln!();
        return run_in_existing_session(
            &cwd,
            &session.fuse_mountpoint,
            &allowed_paths,
            command,
            args,
            &session.run_id,
        );
    }

    print_welcome_banner(&cwd, &allowed_paths, &session.run_id);

    // Open the directory BEFORE mounting FUSE on top of it.
    // This fd lets us access the underlying directory through /proc/self/fd/N,
    // bypassing the FUSE mount that will be placed on top.
    let cwd_fd = std::fs::File::open(&cwd).context("Failed to open current directory")?;
    let fd_num = cwd_fd.as_raw_fd();
    let fd_path = format!("/proc/self/fd/{}", fd_num);

    let db_path_str = session
        .db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;
    let agentfs = AgentFS::open(AgentFSOptions::with_path(db_path_str))
        .await
        .context("Failed to create delta AgentFS")?;

    let hostfs = HostFS::new(&fd_path).context("Failed to create HostFS")?;
    #[cfg(target_family = "unix")]
    let hostfs = {
        let mountpoint_inode = fs::metadata(&session.fuse_mountpoint)
            .map(|m| m.ino())
            .context("Failed to get mountpoint inode")?;
        hostfs.with_fuse_mountpoint(mountpoint_inode)
    };

    let base = Arc::new(hostfs);
    let overlay = OverlayFS::new(base, agentfs.fs);

    let cwd_str = cwd
        .to_str()
        .context("Current directory path contains non-UTF8 characters")?;
    overlay
        .init(cwd_str)
        .await
        .context("Failed to initialize overlay")?;

    let overlay: Arc<dyn FileSystem> = Arc::new(overlay);

    // Set up FUSE mount options - mount at hidden temp directory
    // SAFETY: getuid/getgid are always safe, they simply return the current user/group IDs
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let fuse_opts = FuseMountOptions {
        mountpoint: session.fuse_mountpoint.clone(),
        auto_unmount: false,
        allow_root: false,
        fsname: format!("agentfs:{}", session.run_id),
        uid: Some(uid),
        gid: Some(gid),
    };

    // Start FUSE in a separate thread
    let fuse_handle = std::thread::spawn(move || {
        let rt = crate::get_runtime();
        crate::fuse::mount(overlay, fuse_opts, &rt)
    });

    // Wait for FUSE mount to be ready
    if !wait_for_mount(&session.fuse_mountpoint, FUSE_MOUNT_TIMEOUT) {
        bail!(
            "FUSE mount did not become ready within {:?}",
            FUSE_MOUNT_TIMEOUT
        );
    }

    // Create pipes for parent-child coordination.
    // The parent needs to write uid_map/gid_map for the child after unshare.
    let (pipe_to_child, pipe_to_parent) = create_sync_pipes()?;

    // SAFETY: fork() is safe when called from a single-threaded context before
    // the child performs any async-signal-unsafe operations. Our child immediately
    // closes unused fds and calls exec after namespace setup.
    let child_pid = unsafe { libc::fork() };

    if child_pid < 0 {
        bail!("Failed to fork: {}", std::io::Error::last_os_error());
    }

    if child_pid == 0 {
        // SAFETY: Closing unused pipe ends in child; these fds are valid from pipe()
        unsafe {
            libc::close(pipe_to_child[1]); // Close write end
            libc::close(pipe_to_parent[0]); // Close read end
        }

        // Close the fd in child - we don't need it (parent keeps it for FUSE)
        drop(cwd_fd);
        run_child(
            &cwd,
            &session.fuse_mountpoint,
            &allowed_paths,
            command,
            args,
            &session.run_id,
            pipe_to_child[0],
            pipe_to_parent[1],
        );
    } else {
        // SAFETY: Closing unused pipe ends in parent; these fds are valid from pipe()
        unsafe {
            libc::close(pipe_to_child[0]); // Close read end
            libc::close(pipe_to_parent[1]); // Close write end
        }

        // Wait for child to signal it has called unshare
        if !wait_for_pipe_signal(pipe_to_parent[0]) {
            eprintln!("Error: Failed to read sync signal from child process");
            abort_child(pipe_to_child[1], child_pid);
        }

        // Configure user namespace mappings for the child
        write_namespace_mappings(child_pid, uid, gid, pipe_to_child[1]);

        // Signal child that mappings are done
        // SAFETY: Writing to and closing valid pipe fds
        unsafe {
            libc::write(pipe_to_child[1], b"x".as_ptr() as *const libc::c_void, 1);
            libc::close(pipe_to_child[1]);
            libc::close(pipe_to_parent[0]);
        }

        // Write proc file for this session (owner = true)
        if let Err(e) =
            crate::cmd::ps::write_proc_file(&session.run_id, true, &command.to_string_lossy(), &cwd)
        {
            eprintln!("Warning: Failed to write proc file: {}", e);
        }

        // Keep cwd_fd alive - it's needed by HostFS in the FUSE thread
        run_parent(
            child_pid,
            cwd_fd,
            &session.fuse_mountpoint,
            fuse_handle,
            &session.db_path,
            &session.run_id,
        );
    }
}

/// Run a command in an existing session's FUSE mount.
///
/// This is used when joining an existing session that already has a FUSE mount active.
/// We don't need to start a new FUSE server, just run the command in the existing mount.
fn run_in_existing_session(
    cwd: &Path,
    fuse_mountpoint: &Path,
    allowed_paths: &[PathBuf],
    command: PathBuf,
    args: Vec<String>,
    session_id: &str,
) -> Result<()> {
    // SAFETY: getuid/getgid are always safe
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Create pipes for parent-child coordination.
    let (pipe_to_child, pipe_to_parent) = create_sync_pipes()?;

    // SAFETY: fork() is safe here
    let child_pid = unsafe { libc::fork() };

    if child_pid < 0 {
        bail!("Failed to fork: {}", std::io::Error::last_os_error());
    }

    if child_pid == 0 {
        // Child process
        unsafe {
            libc::close(pipe_to_child[1]);
            libc::close(pipe_to_parent[0]);
        }

        run_child(
            cwd,
            fuse_mountpoint,
            allowed_paths,
            command,
            args,
            session_id,
            pipe_to_child[0],
            pipe_to_parent[1],
        );
    } else {
        // Parent process
        unsafe {
            libc::close(pipe_to_child[0]);
            libc::close(pipe_to_parent[1]);
        }

        // Wait for child to signal it has called unshare
        if !wait_for_pipe_signal(pipe_to_parent[0]) {
            eprintln!("Error: Failed to read sync signal from child process");
            abort_child(pipe_to_child[1], child_pid);
        }

        // Configure user namespace mappings for the child
        write_namespace_mappings(child_pid, uid, gid, pipe_to_child[1]);

        // Signal child that mappings are done
        unsafe {
            libc::write(pipe_to_child[1], b"x".as_ptr() as *const libc::c_void, 1);
            libc::close(pipe_to_child[1]);
            libc::close(pipe_to_parent[0]);
        }

        // Write proc file for this joined session (owner = false)
        if let Err(e) =
            crate::cmd::ps::write_proc_file(session_id, false, &command.to_string_lossy(), cwd)
        {
            eprintln!("Warning: Failed to write proc file: {}", e);
        }

        // Store child PID and install signal handlers before waiting
        CHILD_PID.store(child_pid, Ordering::SeqCst);
        install_signal_handlers();

        // Wait for child to exit (don't unmount or cleanup - the original session owns that)
        // Retry on EINTR (signal interruption)
        let exit_code = wait_for_child(child_pid);

        // Clean up proc file
        crate::cmd::ps::remove_proc_file(session_id);

        std::process::exit(exit_code);
    }
}

/// Print the welcome banner showing sandbox configuration.
fn print_welcome_banner(cwd: &Path, allowed_paths: &[PathBuf], session_id: &str) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();
    eprintln!("The following directories are writable:");
    eprintln!();
    eprintln!("  - {} (copy-on-write)", cwd.display());
    for grouped_path in group_paths_by_parent(allowed_paths) {
        eprintln!("  - {}", grouped_path);
    }
    eprintln!();
    eprintln!("🔒 Everything else is read-only.");
    eprintln!();
    eprintln!("To join this session from another terminal:");
    eprintln!();
    eprintln!("  agentfs run --session {} <command>", session_id);
    eprintln!();
}

/// Configuration for a sandbox run session.
struct RunSession {
    /// Unique identifier for this run.
    run_id: String,
    /// Path to the delta database.
    db_path: PathBuf,
    /// Path where FUSE filesystem will be mounted.
    fuse_mountpoint: PathBuf,
}

/// Create a run directory with database and mountpoint paths.
///
/// If `session_id` is provided, uses that as the run ID (allowing multiple
/// runs to share the same delta layer). Otherwise generates a unique UUID.
fn setup_run_directory(session_id: Option<String>) -> Result<RunSession> {
    let run_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let home_dir = dirs::home_dir().context("Failed to get home directory")?;
    let run_dir = home_dir.join(".agentfs").join("run").join(&run_id);
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;

    let db_path = run_dir.join("delta.db");
    let fuse_mountpoint = run_dir.join("mnt");
    std::fs::create_dir_all(&fuse_mountpoint).context("Failed to create FUSE mountpoint")?;

    Ok(RunSession {
        run_id,
        db_path,
        fuse_mountpoint,
    })
}

/// Create a pair of pipes for parent-child synchronization.
///
/// Returns (child_pipe, parent_pipe) where each is [read_fd, write_fd].
fn create_sync_pipes() -> Result<([libc::c_int; 2], [libc::c_int; 2])> {
    let mut child_pipe: [libc::c_int; 2] = [0; 2];
    let mut parent_pipe: [libc::c_int; 2] = [0; 2];

    if unsafe { libc::pipe(child_pipe.as_mut_ptr()) } != 0 {
        bail!("Failed to create pipe: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::pipe(parent_pipe.as_mut_ptr()) } != 0 {
        // Clean up first pipe on failure
        unsafe {
            libc::close(child_pipe[0]);
            libc::close(child_pipe[1]);
        }
        bail!("Failed to create pipe: {}", std::io::Error::last_os_error());
    }

    Ok((child_pipe, parent_pipe))
}

/// Wait for a single-byte synchronization signal on a pipe.
///
/// Returns true if signal received, false on error or pipe closed.
fn wait_for_pipe_signal(fd: libc::c_int) -> bool {
    let mut buf = [0u8; 1];
    // SAFETY: Reading into valid buffer from valid fd
    let result = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    result > 0
}

/// Terminate child process coordination and exit with failure.
///
/// Closes the pipe to signal the child, waits for it to exit, then exits.
fn abort_child(pipe_write_fd: libc::c_int, child_pid: libc::pid_t) -> ! {
    // SAFETY: Closing valid fd and waiting for valid child pid
    unsafe {
        libc::close(pipe_write_fd);
        let mut status: libc::c_int = 0;
        libc::waitpid(child_pid, &mut status, 0);
    }
    std::process::exit(1)
}

/// Write uid_map, gid_map, and setgroups for a child's user namespace.
///
/// Maps the real uid/gid to itself inside the namespace, so the user appears
/// as themselves (not root) inside the sandbox.
/// On failure, aborts the child and exits.
fn write_namespace_mappings(
    child_pid: libc::pid_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    pipe_write_fd: libc::c_int,
) {
    let uid_map_path = format!("/proc/{}/uid_map", child_pid);
    let gid_map_path = format!("/proc/{}/gid_map", child_pid);
    let setgroups_path = format!("/proc/{}/setgroups", child_pid);

    // Map the user's UID to itself (inside_uid outside_uid count)
    if let Err(e) = std::fs::write(&uid_map_path, format!("{} {} 1\n", uid, uid)) {
        eprintln!("Error: Could not write uid_map: {}", e);
        eprintln!("This may indicate missing unprivileged user namespace support.");
        abort_child(pipe_write_fd, child_pid);
    }

    // Disable setgroups (required before writing gid_map on unprivileged systems)
    if let Err(e) = std::fs::write(&setgroups_path, "deny") {
        eprintln!("Error: Could not write setgroups: {}", e);
        abort_child(pipe_write_fd, child_pid);
    }

    // Map the user's GID to itself (inside_gid outside_gid count)
    if let Err(e) = std::fs::write(&gid_map_path, format!("{} {} 1\n", gid, gid)) {
        eprintln!("Error: Could not write gid_map: {}", e);
        abort_child(pipe_write_fd, child_pid);
    }
}

/// Convert a path to a CString, exiting the child process on failure.
///
/// Used in the child process context where we cannot return errors normally.
fn path_to_cstring(path: &Path, description: &str) -> CString {
    match CString::new(path.as_os_str().as_bytes()) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Invalid {} (contains NUL byte): {}",
                description,
                path.display()
            );
            // SAFETY: In forked child, must use _exit() to avoid running atexit
            // handlers and flushing stdio buffers that belong to the parent.
            unsafe { libc::_exit(1) }
        }
    }
}

/// Exit the child process with an error message and exit code.
///
/// Uses _exit() instead of exit() to avoid running atexit handlers in the
/// forked child, which could corrupt parent state.
fn child_exit_with_code(msg: &str, code: i32) -> ! {
    eprintln!("{}", msg);
    // SAFETY: In forked child, _exit() is the correct way to terminate.
    unsafe { libc::_exit(code) }
}

/// Exit the child process with an error message (exit code 1).
fn child_exit(msg: &str) -> ! {
    child_exit_with_code(msg, 1)
}

/// Child process: set up namespace isolation and execute the command.
#[allow(clippy::too_many_arguments)]
fn run_child(
    cwd: &Path,
    fuse_mountpoint: &Path,
    allowed_paths: &[PathBuf],
    command: PathBuf,
    args: Vec<String>,
    session_id: &str,
    pipe_from_parent: libc::c_int,
    pipe_to_parent: libc::c_int,
) -> ! {
    // Step 1: Create new user + mount namespaces for unprivileged isolation.
    // User namespace gives us CAP_SYS_ADMIN within the namespace to manipulate mounts.
    // SAFETY: unshare() with valid flags is safe; we handle the error case.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        child_exit(&format!(
            "Failed to unshare namespaces: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Step 2: Signal parent that unshare is complete so it can write uid_map/gid_map.
    // SAFETY: Writing to and closing valid pipe fd from create_sync_pipes().
    unsafe {
        libc::write(pipe_to_parent, b"x".as_ptr() as *const libc::c_void, 1);
        libc::close(pipe_to_parent);
    }

    // Step 3: Wait for parent to finish writing namespace mappings.
    if !wait_for_pipe_signal(pipe_from_parent) {
        child_exit("Failed to read sync signal from parent: pipe closed unexpectedly");
    }
    // SAFETY: Closing valid pipe fd.
    unsafe { libc::close(pipe_from_parent) };

    // Step 4: Make all mounts private to prevent propagation to parent namespace.
    let root = CString::new("/").unwrap();
    // SAFETY: mount() with MS_PRIVATE on "/" is safe; changes only affect this namespace.
    if unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        child_exit(&format!(
            "Failed to make mounts private: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Step 5: Bind mount the FUSE overlay from temp dir onto cwd.
    // This is only visible in this namespace, not to other processes.
    let fuse_cstr = path_to_cstring(fuse_mountpoint, "FUSE mountpoint path");
    let cwd_cstr = path_to_cstring(cwd, "working directory path");

    // SAFETY: mount() with MS_BIND and valid paths is safe.
    if unsafe {
        libc::mount(
            fuse_cstr.as_ptr(),
            cwd_cstr.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        child_exit(&format!(
            "Failed to bind mount FUSE overlay: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Step 6: Change to cwd to ensure we're using the overlay.
    if std::env::set_current_dir(cwd).is_err() {
        child_exit("Failed to change to working directory");
    }

    // Step 7: Remount all other filesystems as read-only.
    if let Err(e) = remount_all_readonly_except(cwd, allowed_paths) {
        child_exit(&format!("Failed to remount filesystems read-only: {}", e));
    }

    // Step 8: Execute the command (does not return).
    exec_command(command, args, session_id);
}

/// Remount all filesystems as read-only, except for the specified paths.
///
/// The correct sequence to keep allowed paths writable:
/// 1. Bind-mount each allowed path to itself (creates new mountpoint)
/// 2. Remount each with explicit rw,bind to lock in the rw flag
/// 3. THEN remount / and other mounts as read-only
///
/// This works because bind mounts established before the ro remount
/// retain their own mount options.
fn remount_all_readonly_except(
    writable_path: &Path,
    allowed_paths: &[PathBuf],
) -> std::io::Result<()> {
    // Step 1: Bind-mount allowed paths to themselves FIRST
    // This creates independent mountpoints that will survive the ro remount
    for allowed in allowed_paths {
        let path_cstr = match CString::new(allowed.as_os_str().as_bytes()) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Bind mount to itself to establish new mountpoint (inherits rw)
        // SAFETY: mount() with valid paths
        let bind_result = unsafe {
            libc::mount(
                path_cstr.as_ptr(),
                path_cstr.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };

        if bind_result == 0 {
            // Step 2: Explicitly remount with rw,bind to lock in the rw flag
            // SAFETY: mount() with valid path
            let _ = unsafe {
                libc::mount(
                    std::ptr::null(),
                    path_cstr.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT,
                    std::ptr::null(),
                )
            };
        }
    }

    // Step 3: Now remount everything else as read-only
    let mountinfo = std::fs::File::open("/proc/self/mountinfo")?;
    let reader = std::io::BufReader::new(mountinfo);

    // Collect all mount points
    let mut mounts: Vec<PathBuf> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > MOUNTINFO_MOUNT_POINT_FIELD {
            let mount_point = unescape_mountinfo(fields[MOUNTINFO_MOUNT_POINT_FIELD]);
            mounts.push(PathBuf::from(mount_point));
        }
    }

    // Sort by path length (longest first) to handle nested mounts correctly
    mounts.sort_by_key(|b| Reverse(b.as_os_str().len()));

    // Canonicalize the writable path for comparison
    let writable_canonical = writable_path
        .canonicalize()
        .unwrap_or_else(|_| writable_path.to_path_buf());

    // Canonicalize allowed paths for comparison
    let allowed_canonical: Vec<PathBuf> = allowed_paths
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    for mount_point in &mounts {
        let mount_canonical = mount_point
            .canonicalize()
            .unwrap_or_else(|_| mount_point.clone());

        // Skip the writable path (our FUSE overlay)
        if mount_canonical == writable_canonical {
            continue;
        }

        // Skip allowed paths (they're already bind-mounted as rw)
        if allowed_canonical.contains(&mount_canonical) {
            continue;
        }

        // Skip virtual filesystems that shouldn't be remounted
        if skip_mount(mount_point) {
            continue;
        }

        // Try to remount as read-only (bind + remount + rdonly)
        let mount_cstr = match CString::new(mount_point.as_os_str().as_bytes()) {
            Ok(s) => s,
            Err(_) => continue, // Path contains NUL byte, skip it
        };

        // First bind mount on itself to create a distinct mount point.
        // SAFETY: mount() with valid CString path; failures are expected and handled.
        let bind_result = unsafe {
            libc::mount(
                mount_cstr.as_ptr(),
                mount_cstr.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        };

        if bind_result != 0 {
            // Some mounts can't be bind-mounted (e.g., already bind mounts), skip them
            continue;
        }

        // Remount the bind mount as read-only.
        // SAFETY: mount() with valid path; failures silently ignored as some
        // filesystems (e.g., tmpfs with running processes) cannot be remounted.
        let _ = unsafe {
            libc::mount(
                std::ptr::null(),
                mount_cstr.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null(),
            )
        };
    }

    Ok(())
}

/// Check if a mount point should be skipped during read-only remounting.
///
/// Virtual filesystems like /proc, /sys, and /dev must remain writable
/// for the system to function correctly.
fn skip_mount(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    SKIP_MOUNT_PREFIXES
        .iter()
        .any(|prefix| path_str.starts_with(prefix))
}

/// Build the list of allowed writable paths from user input and defaults.
fn build_allowed_paths(user_allowed: &[PathBuf], no_default_allows: bool) -> Result<Vec<PathBuf>> {
    let mut allowed = Vec::new();

    // Add default allowed directories unless disabled
    if !no_default_allows {
        if let Some(home) = dirs::home_dir() {
            for dir in DEFAULT_ALLOWED_DIRS {
                let path = home.join(dir);
                // Only add if the path exists
                if path.exists() {
                    allowed.push(path);
                }
            }
        }
    }

    // Add user-specified paths
    for path in user_allowed {
        // Canonicalize user paths to resolve symlinks and relative paths
        let canonical = path.canonicalize().with_context(|| {
            format!(
                "Failed to canonicalize allowed path '{}'. Does it exist?",
                path.display()
            )
        })?;
        allowed.push(canonical);
    }

    Ok(allowed)
}

/// Unescape mount point from mountinfo format.
/// Spaces are encoded as \040, tabs as \011, etc.
fn unescape_mountinfo(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            // Try to read octal escape sequence (digits 0-7 only)
            let mut octal = String::new();
            for _ in 0..3 {
                if let Some(&next) = chars.peek() {
                    if ('0'..='7').contains(&next) {
                        octal.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
            }
            if octal.len() == 3 {
                // Use u32 to handle values > 255 (max octal 777 = 511)
                if let Ok(code) = u32::from_str_radix(&octal, 8) {
                    if code <= 255 {
                        result.push(code as u8 as char);
                        continue;
                    }
                }
            }
            // Not a valid escape, keep the backslash and octal chars
            result.push(c);
            result.push_str(&octal);
        } else {
            result.push(c);
        }
    }

    result
}

/// Attempt to unmount a FUSE filesystem using fusermount.
///
/// Tries commands from FUSERMOUNT_COMMANDS in order until one succeeds.
/// Uses lazy unmount (-uz) to handle lingering references from the FUSE thread.
///
/// Returns true if unmount succeeded or mount was already gone.
fn unmount_fuse(mountpoint: &Path) -> bool {
    for cmd in FUSERMOUNT_COMMANDS {
        let success = std::process::Command::new(cmd)
            .args(["-uz"])
            .arg(mountpoint.as_os_str())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if success {
            return true;
        }
    }

    // Check if it's actually still mounted
    !is_mountpoint(mountpoint)
}

/// Parent process: wait for child to exit, then clean up.
///
/// The FUSE thread handle is intentionally dropped without joining. We perform
/// a lazy unmount (fusermount -uz) which safely detaches the filesystem even
/// while the FUSE thread may still be processing requests. The thread will
/// terminate naturally when the mount is gone.
fn run_parent(
    child_pid: i32,
    cwd_fd: std::fs::File,
    fuse_mountpoint: &Path,
    _fuse_handle: std::thread::JoinHandle<anyhow::Result<()>>,
    db_path: &Path,
    session_id: &str,
) -> ! {
    // Store child PID and install signal handlers before waiting
    CHILD_PID.store(child_pid, Ordering::SeqCst);
    install_signal_handlers();

    // Wait for child process to exit, retrying on EINTR (signal interruption)
    let exit_code = wait_for_child(child_pid);

    // Clean up proc file
    crate::cmd::ps::remove_proc_file(session_id);

    // Move away from mountpoint before unmounting to avoid EBUSY
    let _ = std::env::set_current_dir("/");

    // Release the underlying directory fd (was kept alive for HostFS)
    drop(cwd_fd);

    // Unmount the FUSE filesystem
    if !unmount_fuse(fuse_mountpoint) {
        eprintln!(
            "Warning: Failed to unmount FUSE filesystem at {}",
            fuse_mountpoint.display()
        );
        eprintln!(
            "You may need to manually unmount with: fusermount -uz {}",
            fuse_mountpoint.display()
        );
        std::process::exit(exit_code);
    }

    // Clean up the FUSE mountpoint directory (but keep the delta database)
    if let Err(e) = std::fs::remove_dir_all(fuse_mountpoint) {
        eprintln!(
            "Warning: Failed to clean up mountpoint {}: {}",
            fuse_mountpoint.display(),
            e
        );
    }

    // Clean up procs directory if empty
    let procs_dir = crate::cmd::ps::procs_dir(session_id);
    let _ = std::fs::remove_dir(&procs_dir);

    // Print the location of the delta layer for the user
    eprintln!();
    eprintln!("Delta layer saved to: {}", db_path.display());
    eprintln!();
    eprintln!("To see what changed:");
    eprintln!("  agentfs diff {}", db_path.display());

    std::process::exit(exit_code);
}

/// Wait for a path to become a mountpoint
fn wait_for_mount(path: &Path, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    let interval = std::time::Duration::from_millis(50);

    while start.elapsed() < timeout {
        if is_mountpoint(path) {
            return true;
        }
        std::thread::sleep(interval);
    }
    false
}

/// Check if a path is a mountpoint by comparing device IDs with parent.
fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let path_meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("/"),
    };

    let parent_meta = match std::fs::metadata(parent) {
        Ok(m) => m,
        Err(_) => return false,
    };

    path_meta.dev() != parent_meta.dev()
}

/// Execute the command, replacing the current process.
fn exec_command(command: PathBuf, args: Vec<String>, session_id: &str) -> ! {
    setup_env_vars(session_id);

    let cmd_cstr = match CString::new(command.as_os_str().as_bytes()) {
        Ok(s) => s,
        Err(_) => {
            child_exit_with_code(
                &format!("Invalid command (contains NUL byte): {}", command.display()),
                EXIT_COMMAND_NOT_FOUND,
            );
        }
    };

    let mut argv: Vec<CString> = vec![cmd_cstr.clone()];
    for arg in &args {
        match CString::new(arg.as_str()) {
            Ok(s) => argv.push(s),
            Err(_) => {
                child_exit_with_code(
                    &format!("Invalid argument (contains NUL byte): {}", arg),
                    EXIT_COMMAND_NOT_FOUND,
                );
            }
        }
    }

    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    unsafe {
        libc::execvp(cmd_cstr.as_ptr(), argv_ptrs.as_ptr());
    }

    child_exit_with_code(
        &format!(
            "Failed to execute {}: {}",
            command.display(),
            std::io::Error::last_os_error()
        ),
        EXIT_COMMAND_NOT_FOUND,
    );
}

/// Setup environment variables for the sandbox.
fn setup_env_vars(session_id: &str) {
    std::env::set_var("AGENTFS", "1");
    std::env::set_var("AGENTFS_SANDBOX", "linux-namespace");
    std::env::set_var("AGENTFS_SESSION", session_id);
    std::env::set_var("PS1", "🤖 \\u@\\h:\\w\\$ ");

    // Configure SSH to skip system config files.
    // Inside the user namespace, root-owned files in /etc/ssh/ssh_config.d/
    // appear with invalid ownership (unmapped uid), causing SSH to reject them.
    // Using only ~/.ssh/config avoids this issue while preserving user settings.
    if let Some(home) = dirs::home_dir() {
        let user_ssh_config = home.join(".ssh/config");
        // Use user's config if it exists, otherwise use /dev/null (no config)
        let config_path = if user_ssh_config.exists() {
            user_ssh_config.to_string_lossy().to_string()
        } else {
            "/dev/null".to_string()
        };
        std::env::set_var("GIT_SSH_COMMAND", format!("ssh -F {}", config_path));
    }
}

/// Wait for a child process to exit, retrying on EINTR.
///
/// Returns the exit code of the child process, or 1 if waitpid fails.
fn wait_for_child(child_pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid with valid child pid is safe
        let result = unsafe { libc::waitpid(child_pid, &mut status, 0) };
        if result == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                // Interrupted by signal, retry
                continue;
            }
            // Other error, return exit code 1
            return 1;
        }
        break;
    }
    wait_status_to_exit_code(status)
}

/// Extract exit code from wait status.
fn wait_status_to_exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}
