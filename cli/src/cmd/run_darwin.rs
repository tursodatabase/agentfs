//! Darwin (macOS) run command implementation.
//!
//! This module provides a sandboxed execution environment using NFS for
//! filesystem mounting. The current working directory becomes a
//! copy-on-write overlay backed by AgentFS, mounted via a localhost NFS server.
//!
//! Sandboxing is enforced using macOS sandbox-exec with dynamically generated
//! profiles that restrict file writes to the NFS mountpoint and allowed paths.

#![cfg(unix)]

use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use nfsserve::tcp::NFSTcp;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::nfs::AgentNFS;

#[cfg(target_os = "macos")]
use crate::sandbox::darwin::{generate_sandbox_profile, SandboxConfig};

/// Default NFS port to try (use a high port to avoid needing root)
const DEFAULT_NFS_PORT: u32 = 11111;

/// Run the command in a Darwin sandbox.
pub async fn run(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    _experimental_sandbox: bool,
    _strace: bool,
    session_id: Option<String>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let home = dirs::home_dir().context("Failed to get home directory")?;

    let session = setup_run_directory(session_id, allow, no_default_allows, &cwd, &home)?;

    // Initialize the AgentFS database
    let db_path_str = session
        .db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;

    let agentfs = AgentFS::open(AgentFSOptions::with_path(db_path_str))
        .await
        .context("Failed to create AgentFS")?;

    // Create overlay filesystem with CWD as base
    let base_str = cwd.to_string_lossy().to_string();
    let hostfs = HostFS::new(&base_str).context("Failed to create HostFS")?;
    let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);

    // Initialize the overlay (copies directory structure)
    overlay
        .init(&base_str)
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

    print_welcome_banner(&session);

    // Run the command
    let exit_code = run_command_in_mount(&session, command, args)?;

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

/// Print the welcome banner showing sandbox configuration (macOS).
#[cfg(target_os = "macos")]
fn print_welcome_banner(session: &RunSession) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();
    eprintln!("The following directories are writable:");
    eprintln!();
    eprintln!("  - {} (copy-on-write)", session.cwd.display());
    eprintln!("  - /tmp");
    for path in &session.allow_paths {
        eprintln!("  - {}", path.display());
    }
    eprintln!();
    eprintln!("🔒 Everything else is read-only.");
    eprintln!();
    eprintln!("To join this session from another terminal:");
    eprintln!();
    eprintln!("  agentfs run --session {} <command>", session.session_id);
    eprintln!();
}

/// Print the welcome banner showing sandbox configuration (Linux).
#[cfg(target_os = "linux")]
fn print_welcome_banner(session: &RunSession) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();
    eprintln!("  {} (copy-on-write)", session.cwd.display());
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
    /// Session ID for the sandbox profile.
    session_id: String,
    /// Additional paths to allow write access.
    allow_paths: Vec<PathBuf>,
    /// Original working directory.
    cwd: PathBuf,
}

/// Default directories in HOME that are allowed to be writable.
/// These are common application config/cache directories that many programs need.
const DEFAULT_ALLOWED_DIRS: &[&str] = &[
    ".claude",      // Claude Code config
    ".claude.json", // Claude Code config file
    ".local",       // Local data directory
    ".npm",         // npm local registry
    ".config",      // XDG config directory
    ".cache",       // XDG cache directory
    ".bun",         // Used by opencode to install packages at runtime
];

/// Create a run directory with database and mountpoint paths.
///
/// If `session_id` is provided, uses that as the run ID (allowing multiple
/// runs to share the same delta layer). Otherwise generates a unique UUID.
fn setup_run_directory(
    session_id: Option<String>,
    user_allow_paths: Vec<PathBuf>,
    no_default_allows: bool,
    cwd: &Path,
    home: &Path,
) -> Result<RunSession> {
    let run_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let run_dir = home.join(".agentfs").join("run").join(&run_id);
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;

    let db_path = run_dir.join("delta.db");
    let mountpoint = run_dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).context("Failed to create mountpoint")?;

    // Build allowed paths list
    let mut allow_paths = user_allow_paths;
    if !no_default_allows {
        for dir in DEFAULT_ALLOWED_DIRS {
            let path = home.join(dir);
            if path.exists() {
                allow_paths.push(path);
            }
        }
    }

    // Create zsh config directory with custom prompt
    let zsh_dir = run_dir.join("zsh");
    std::fs::create_dir_all(&zsh_dir).context("Failed to create zsh config directory")?;
    std::fs::write(zsh_dir.join(".zshrc"), "PROMPT='🤖 %~%# '\n")
        .context("Failed to write zsh config")?;

    Ok(RunSession {
        run_dir,
        db_path,
        mountpoint,
        session_id: run_id,
        allow_paths,
        cwd: cwd.to_path_buf(),
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

/// Run a command with the working directory set to the mounted filesystem (macOS).
///
/// On macOS, the command is wrapped with sandbox-exec using a Sandbox profile
/// that restricts file writes to the NFS mountpoint and allowed paths.
/// The mountpoint overlays CWD, and additional paths in HOME are made writable
/// through the allow_paths configuration.
#[cfg(target_os = "macos")]
fn run_command_in_mount(session: &RunSession, command: PathBuf, args: Vec<String>) -> Result<i32> {
    // Generate the Sandbox profile
    let config = SandboxConfig {
        mountpoint: session.mountpoint.clone(),
        allow_paths: session.allow_paths.clone(),
        allow_read_paths: Vec::new(),
        allow_network: true,
        session_id: session.session_id.clone(),
    };
    let profile = generate_sandbox_profile(&config);

    // Wrap the command with sandbox-exec
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(&profile)
        .arg(&command)
        .args(&args)
        .current_dir(&session.mountpoint)
        .env("AGENTFS", "1")
        .env("AGENTFS_SANDBOX", "macos-sandbox")
        // Bash prompt - show full path since we're not changing HOME
        .env("PS1", "🤖 \\w\\$ ")
        // Zsh: use custom ZDOTDIR to override prompt
        .env("ZDOTDIR", session.run_dir.join("zsh"));

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute command: {}", command.display()))?;

    Ok(status.code().unwrap_or(1))
}

/// Run a command with the working directory set to the mounted filesystem (Linux).
///
/// On Linux, the command runs without additional sandboxing (NFS provides
/// copy-on-write for the working directory).
#[cfg(target_os = "linux")]
fn run_command_in_mount(session: &RunSession, command: PathBuf, args: Vec<String>) -> Result<i32> {
    let mut cmd = Command::new(&command);
    cmd.args(&args)
        .current_dir(&session.mountpoint)
        .env("AGENTFS", "1")
        // Bash prompt
        .env("PS1", "🤖 \\u@\\h:\\w\\$ ")
        // Zsh: use custom ZDOTDIR to override prompt
        .env("ZDOTDIR", session.run_dir.join("zsh"));

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
