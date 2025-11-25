use anyhow::Result;
use std::time::Duration;

/// Daemonize the current process and run a function in the daemon.
///
/// This function forks the process, detaches from the terminal, and runs the
/// provided `daemon_fn` in the child process. The parent process waits for
/// the daemon to signal readiness before returning.
///
/// # Arguments
/// * `daemon_fn` - The function to run in the daemon process (should block until done)
/// * `ready_check` - A function that polls for readiness (returns true when ready)
/// * `timeout` - How long to wait for the ready_check to succeed
///
/// # Returns
/// * `Ok(())` in the parent process if the daemon started successfully
/// * Never returns in the child process (exits with appropriate code)
pub fn daemonize<F, R>(daemon_fn: F, ready_check: R, timeout: Duration) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
    R: Fn() -> bool,
{
    // Create pipe for child->parent signaling
    let mut pipe_fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("Failed to create pipe: {}", std::io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            anyhow::bail!("Fork failed: {}", std::io::Error::last_os_error());
        }
        0 => {
            // Child process
            unsafe { libc::close(read_fd) };

            // Create new session (detach from terminal)
            if unsafe { libc::setsid() } == -1 {
                let _ = signal_parent(write_fd, false);
                std::process::exit(1);
            }

            // Redirect stdin/stdout/stderr to /dev/null
            redirect_stdio_to_devnull();

            // Run the daemon function in a separate thread
            let daemon_thread = std::thread::spawn(daemon_fn);

            // Wait for readiness, but fail early if daemon thread exits
            let start = std::time::Instant::now();
            let ready = loop {
                if ready_check() {
                    break true;
                }
                if daemon_thread.is_finished() {
                    break false;
                }
                if start.elapsed() >= timeout {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };

            // Signal parent
            let _ = signal_parent(write_fd, ready);
            unsafe { libc::close(write_fd) };

            if !ready {
                std::process::exit(1);
            }

            // Wait for daemon thread (blocks until done)
            match daemon_thread.join() {
                Ok(Ok(())) => std::process::exit(0),
                _ => std::process::exit(1),
            }
        }
        _child_pid => {
            // Parent process
            unsafe { libc::close(write_fd) };

            // Wait for child to signal readiness
            let success = wait_for_signal(read_fd);
            unsafe { libc::close(read_fd) };

            if success {
                Ok(())
            } else {
                anyhow::bail!("Daemon failed to start")
            }
        }
    }
}

/// Signal parent process via pipe
fn signal_parent(fd: libc::c_int, success: bool) -> Result<()> {
    let buf = [if success { 0u8 } else { 1u8 }];
    let written = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, 1) };
    if written != 1 {
        anyhow::bail!("Failed to signal parent");
    }
    Ok(())
}

/// Wait for signal from child process
fn wait_for_signal(fd: libc::c_int) -> bool {
    let mut buf = [0u8; 1];
    let read = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    read == 1 && buf[0] == 0
}

/// Redirect stdio to /dev/null for daemon
fn redirect_stdio_to_devnull() {
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }
}
