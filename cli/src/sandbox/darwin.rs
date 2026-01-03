//! macOS Sandbox sandbox support for AgentFS.
//!
//! This module provides kernel-enforced sandboxing using macOS's sandbox-exec
//! with dynamically generated Sandbox profiles. When enabled, the spawned
//! process can only access the NFS mountpoint and explicitly allowed paths.
//!
//! # Example
//!
//! ```ignore
//! let config = SandboxConfig {
//!     mountpoint: PathBuf::from("/Users/me/.agentfs/run/abc/mnt"),
//!     allow_paths: vec![PathBuf::from("/tmp")],
//!     allow_network: false,
//!     session_id: "abc123".to_string(),
//! };
//! let profile = generate_sandbox_profile(&config);
//! let wrapped = wrap_command_with_sandbox(&config, "zsh", &[]);
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

/// Configuration for the Sandbox sandbox.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// The NFS mountpoint (primary read/write location).
    pub mountpoint: PathBuf,

    /// Additional paths to allow read/write access.
    pub allow_paths: Vec<PathBuf>,

    /// Additional paths to allow read-only access.
    pub allow_read_paths: Vec<PathBuf>,

    /// Whether to allow network access.
    pub allow_network: bool,

    /// Session ID for log filtering (used in violation messages).
    pub session_id: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mountpoint: PathBuf::new(),
            allow_paths: Vec::new(),
            allow_read_paths: Vec::new(),
            allow_network: false,
            session_id: String::new(),
        }
    }
}

/// Generate a Sandbox profile for AgentFS.
///
/// The profile allows most operations but restricts file writes to:
/// - The NFS mountpoint (which overlays CWD)
/// - Temp directories (/tmp, /var/folders)
/// - Explicitly allowed paths (e.g., ~/.claude, ~/.config, etc.)
///
/// This approach is simpler and more reliable than trying to enumerate
/// all paths a process might need to read.
pub fn generate_sandbox_profile(config: &SandboxConfig) -> String {
    let mut profile = Vec::new();
    let log_tag = format!("agentfs-{}", config.session_id);

    // Version and deny by default for file writes
    profile.push("(version 1)".to_string());
    profile.push(format!(
        r#"(deny default (with message "agentfs-{}: write denied"))"#,
        config.session_id
    ));
    profile.push(format!("; Log tag: {}", log_tag));

    // =========================================================================
    // Allow most operations - we only want to restrict file writes
    // =========================================================================
    profile.push("; Allow most operations".to_string());
    profile.push("(allow process*)".to_string());
    profile.push("(allow signal)".to_string());
    profile.push("(allow mach*)".to_string());
    profile.push("(allow sysctl*)".to_string());
    profile.push("(allow system*)".to_string());
    profile.push("(allow ipc*)".to_string());
    profile.push("(allow pseudo-tty)".to_string());

    // =========================================================================
    // Allow all file reads - the overlay handles copy-on-write
    // =========================================================================
    profile.push("; Allow all file reads".to_string());
    profile.push("(allow file-read*)".to_string());

    // =========================================================================
    // Writable paths - these are the only places writes are allowed
    // =========================================================================
    profile.push("; Writable paths".to_string());

    // The NFS mountpoint - primary workspace (overlays CWD)
    let mountpoint_str = config.mountpoint.to_string_lossy();
    profile.push(format!(
        r#"(allow file-write* (subpath "{}"))"#,
        mountpoint_str
    ));

    // The run directory (for zsh config, etc.)
    if let Some(parent) = config.mountpoint.parent() {
        let run_dir_str = parent.to_string_lossy();
        profile.push(format!(
            r#"(allow file-write* (subpath "{}"))"#,
            run_dir_str
        ));
    }

    // Temp directories (many tools require these)
    profile.push(r#"(allow file-write* (subpath "/private/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/var/tmp"))"#.to_string());

    // Private var folders (per-user temp space)
    profile.push(r#"(allow file-write* (subpath "/private/var/folders"))"#.to_string());

    // Device files (terminals, etc.)
    profile.push(r#"(allow file-write* (subpath "/dev"))"#.to_string());
    profile.push(r#"(allow file-ioctl (subpath "/dev"))"#.to_string());

    // Additional writable paths from config
    for path in &config.allow_paths {
        let path_str = path.to_string_lossy();
        profile.push(format!(r#"(allow file-write* (subpath "{}"))"#, path_str));
    }

    // =========================================================================
    // Network access
    // =========================================================================
    profile.push("; Network".to_string());
    if config.allow_network {
        profile.push("(allow network*)".to_string());
    } else {
        // Only allow localhost for NFS
        profile.push(r#"(allow network* (remote ip "localhost:*"))"#.to_string());
        profile.push(r#"(allow network* (local ip "localhost:*"))"#.to_string());
    }

    // =========================================================================
    // Security and Keychain - needed for credential storage
    // =========================================================================
    profile.push("; Security and Keychain".to_string());
    profile.push(r#"(allow file-write* (subpath "/private/var/db/mds"))"#.to_string());
    profile.push(
        r#"(allow file-write* (regex #"^/private/var/folders/[^/]+/[^/]+/C/mds/"))"#.to_string(),
    );
    profile
        .push(r#"(allow file-write* (regex #"^/private/var/folders/[^/]+/[^/]+/T/"))"#.to_string());
    // User Library paths for Keychain and security services
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        profile.push(format!(
            r#"(allow file-write* (subpath "{}/Library"))"#,
            home_str
        ));
    }
    // System Library for preferences
    profile.push(r#"(allow file-write* (subpath "/Library/Preferences"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/Library/Keychains"))"#.to_string());
    // Authorization and user preference operations
    profile.push("(allow authorization-right-obtain)".to_string());
    profile.push("(allow user-preference-write)".to_string());
    profile.push("(allow user-preference-read)".to_string());

    profile.join("\n")
}

/// Wrap a command with sandbox-exec.
///
/// Returns a Command configured to run the given program inside the sandbox.
pub fn wrap_command_with_sandbox(
    config: &SandboxConfig,
    program: &Path,
    args: &[String],
) -> Command {
    let profile = generate_sandbox_profile(config);

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p").arg(&profile);
    cmd.arg(program);
    cmd.args(args);
    cmd.current_dir(&config.mountpoint);

    // Set environment variables
    cmd.env("AGENTFS", "1");
    cmd.env("AGENTFS_SANDBOX", "macos-sandbox");

    cmd
}

/// Generate a minimal Sandbox profile for testing.
///
/// This profile is more permissive and useful for debugging sandbox issues.
pub fn generate_permissive_profile(config: &SandboxConfig) -> String {
    let mut profile = Vec::new();
    let log_tag = format!("agentfs-{}", config.session_id);

    profile.push("(version 1)".to_string());

    // Log denials but don't block most things
    profile.push(format!("(deny default (with message \"{}\")))", log_tag));

    // Allow almost everything for debugging
    profile.push("(allow process*)".to_string());
    profile.push("(allow file-read*)".to_string());
    profile.push("(allow mach*)".to_string());
    profile.push("(allow sysctl*)".to_string());
    profile.push("(allow signal)".to_string());
    profile.push("(allow ipc*)".to_string());
    profile.push("(allow pseudo-tty)".to_string());
    profile.push("(allow system*)".to_string());

    // Only restrict writes to outside mountpoint
    let mountpoint_str = config.mountpoint.to_string_lossy();
    profile.push(format!(
        r#"(allow file-write* (subpath "{}"))"#,
        mountpoint_str
    ));
    profile.push(r#"(allow file-write* (subpath "/private/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/private/var/folders"))"#.to_string());

    // Network
    if config.allow_network {
        profile.push("(allow network*)".to_string());
    } else {
        profile.push("(allow network* (remote ip \"localhost:*\"))".to_string());
        profile.push("(allow network* (local ip \"localhost:*\"))".to_string());
    }

    profile.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_profile() {
        let config = SandboxConfig {
            mountpoint: PathBuf::from("/Users/test/.agentfs/run/abc/mnt"),
            allow_paths: vec![],
            allow_read_paths: vec![],
            allow_network: false,
            session_id: "test123".to_string(),
        };

        let profile = generate_sandbox_profile(&config);

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default"));
        assert!(profile.contains("agentfs-test123: write denied"));
        assert!(profile.contains("/Users/test/.agentfs/run/abc/mnt"));
    }

    #[test]
    fn test_profile_with_network() {
        let config = SandboxConfig {
            mountpoint: PathBuf::from("/mnt"),
            allow_network: true,
            ..Default::default()
        };

        let profile = generate_sandbox_profile(&config);
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn test_profile_with_custom_paths() {
        let config = SandboxConfig {
            mountpoint: PathBuf::from("/mnt"),
            allow_paths: vec![PathBuf::from("/custom/writable")],
            allow_read_paths: vec![PathBuf::from("/custom/readonly")],
            ..Default::default()
        };

        let profile = generate_sandbox_profile(&config);
        // Writable paths should be included
        assert!(profile.contains("/custom/writable"));
        // Note: allow_read_paths is not used since we allow all reads by default
    }
}
