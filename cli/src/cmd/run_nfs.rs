//! NFS-based run command for macOS and Linux.
//!
//! This module provides a sandboxed execution environment using NFS for
//! filesystem mounting. The current working directory becomes a
//! copy-on-write overlay backed by AgentFS, mounted via a localhost NFS server.
//!
//! This approach works without requiring FUSE or other system extensions.

#![cfg(unix)]

use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use nfsserve::tcp::NFSTcp;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::nfs::AgentNFS;

/// Default NFS port to try (use a high port to avoid needing root)
const DEFAULT_NFS_PORT: u32 = 11111;

/// Handle the `run` command using NFS on macOS.
pub async fn handle_run_command(
    _allow: Vec<PathBuf>,
    _no_default_allows: bool,
    _experimental_sandbox: bool,
    _strace: bool,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;

    let session = setup_run_directory()?;

    // Initialize the AgentFS database
    let db_path_str = session
        .db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;

    let agentfs = AgentFS::open(AgentFSOptions::with_path(db_path_str))
        .await
        .context("Failed to create AgentFS")?;

    // Create overlay filesystem with cwd as base
    let cwd_str = cwd.to_string_lossy().to_string();
    let hostfs = HostFS::new(&cwd_str).context("Failed to create HostFS")?;
    let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);

    // Initialize the overlay (copies directory structure)
    overlay
        .init(&cwd_str)
        .await
        .context("Failed to initialize overlay")?;

    let fs: Arc<Mutex<dyn FileSystem>> = Arc::new(Mutex::new(overlay));

    // Get current user/group
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Create NFS adapter
    let nfs = AgentNFS::new(fs, uid, gid);

    // Find an available port
    let port = find_available_port(DEFAULT_NFS_PORT)?;

    // Start NFS server in background
    let listener = nfsserve::tcp::NFSTcpListener::bind(&format!("127.0.0.1:{}", port), nfs)
        .await
        .context("Failed to bind NFS server")?;

    // Spawn the NFS server task
    let server_handle = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            eprintln!("NFS server error: {}", e);
        }
    });

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Mount the NFS filesystem
    mount_nfs(port, &session.mountpoint)?;

    print_welcome_banner(&session.mountpoint);

    // Run the command
    let exit_code = run_command_in_mount(&session.mountpoint, &session.run_dir, command, args)?;

    // Unmount
    unmount(&session.mountpoint)?;

    // Stop the server
    server_handle.abort();

    // Clean up mountpoint directory (but keep the delta database)
    if let Err(e) = std::fs::remove_dir(&session.mountpoint) {
        eprintln!(
            "Warning: Failed to clean up mountpoint {}: {}",
            session.mountpoint.display(),
            e
        );
    }

    // Print the location of the delta layer for the user
    eprintln!();
    eprintln!("Delta layer saved to: {}", session.db_path.display());
    eprintln!();
    eprintln!("To see what changed:");
    eprintln!("  agentfs diff {}", session.db_path.display());

    std::process::exit(exit_code);
}

/// Print the welcome banner showing sandbox configuration.
fn print_welcome_banner(cwd: &Path) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();
    eprintln!("  {} (copy-on-write)", cwd.display());
    eprintln!("  ⚠️  Everything else is WRITABLE.");
    eprintln!();
}

/// Configuration for a sandbox run session.
struct RunSession {
    /// Directory containing session artifacts.
    run_dir: PathBuf,
    /// Path to the delta database.
    db_path: PathBuf,
    /// Path where NFS filesystem will be mounted.
    mountpoint: PathBuf,
}

/// Create a unique run directory with database and mountpoint paths.
fn setup_run_directory() -> Result<RunSession> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let home_dir = dirs::home_dir().context("Failed to get home directory")?;
    let run_dir = home_dir.join(".agentfs").join("run").join(&run_id);
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;

    let db_path = run_dir.join("delta.db");
    let mountpoint = run_dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).context("Failed to create mountpoint")?;

    // Create zsh config directory with custom prompt
    let zsh_dir = run_dir.join("zsh");
    std::fs::create_dir_all(&zsh_dir).context("Failed to create zsh config directory")?;
    std::fs::write(zsh_dir.join(".zshrc"), "PROMPT='🤖 %n@%m:%~%# '\n")
        .context("Failed to write zsh config")?;

    Ok(RunSession {
        run_dir,
        db_path,
        mountpoint,
    })
}

/// Find an available TCP port starting from the given port.
fn find_available_port(start_port: u32) -> Result<u32> {
    for port in start_port..start_port + 100 {
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "Could not find an available port in range {}-{}",
        start_port,
        start_port + 100
    );
}

/// Mount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn mount_nfs(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!(
                "nolocks,vers=3,tcp,port={},mountport={},soft,timeo=10,retrans=2",
                port, port
            ),
            &format!("127.0.0.1:/"),
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount_nfs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to mount NFS: {}", stderr.trim());
    }

    Ok(())
}

/// Mount the NFS filesystem (Linux version).
#[cfg(target_os = "linux")]
fn mount_nfs(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("mount")
        .args([
            "-t",
            "nfs",
            "-o",
            &format!(
                "vers=3,tcp,port={},mountport={},nolock,soft,timeo=10,retrans=2",
                port, port
            ),
            "127.0.0.1:/",
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount. Make sure nfs-common is installed.")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to mount NFS: {}. Make sure nfs-common is installed (apt-get install nfs-common).",
            stderr.trim()
        );
    }

    Ok(())
}

/// Run a command with the working directory set to the mounted filesystem.
fn run_command_in_mount(
    mountpoint: &Path,
    run_dir: &Path,
    command: PathBuf,
    args: Vec<String>,
) -> Result<i32> {
    let mut cmd = Command::new(&command);
    cmd.args(&args)
        .current_dir(mountpoint)
        .env("AGENTFS", "1")
        // Bash prompt
        .env("PS1", "🤖 \\u@\\h:\\w\\$ ")
        // Zsh: use custom ZDOTDIR to override prompt
        .env("ZDOTDIR", run_dir.join("zsh"));

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute command: {}", command.display()))?;

    Ok(status.code().unwrap_or(1))
}

/// Unmount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn unmount(mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/umount")
        .arg(mountpoint)
        .output()
        .context("Failed to execute umount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Try force unmount
        let output2 = Command::new("/sbin/umount")
            .arg("-f")
            .arg(mountpoint)
            .output()?;

        if !output2.status.success() {
            anyhow::bail!(
                "Failed to unmount: {}. You may need to manually unmount with: umount -f {}",
                stderr.trim(),
                mountpoint.display()
            );
        }
    }

    Ok(())
}

/// Unmount the NFS filesystem (Linux version).
#[cfg(target_os = "linux")]
fn unmount(mountpoint: &Path) -> Result<()> {
    let output = Command::new("umount")
        .arg(mountpoint)
        .output()
        .context("Failed to execute umount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Try lazy unmount
        let output2 = Command::new("umount").arg("-l").arg(mountpoint).output()?;

        if !output2.status.success() {
            anyhow::bail!(
                "Failed to unmount: {}. You may need to manually unmount with: umount -l {}",
                stderr.trim(),
                mountpoint.display()
            );
        }
    }

    Ok(())
}
