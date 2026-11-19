//! Execute a command with an AgentFS filesystem mounted.
//!
//! This module provides the `agentfs exec` command which mounts an AgentFS
//! filesystem to a temporary directory, runs a command with that as the
//! working directory, and automatically unmounts when done.

use agentfs_sdk::{AgentFSOptions, EncryptionConfig, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use turso::value::Value;

use crate::cmd::init::open_agentfs;
use crate::mount::{mount_fs, MountBackend, MountOpts};

/// Handle the exec command.
///
/// Mounts the specified AgentFS, runs the command, and unmounts on completion.
pub async fn handle_exec_command(
    id_or_path: String,
    command: PathBuf,
    args: Vec<String>,
    backend: MountBackend,
    encryption: Option<(String, String)>,
) -> Result<()> {
    // Resolve AgentFS options
    let mut opts = AgentFSOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        opts = opts.with_encryption(EncryptionConfig {
            hex_key: key,
            cipher,
        });
    }

    // Open AgentFS
    let agentfs = open_agentfs(opts).await?;

    // Query base_path in a separate scope so connection is released before load
    let base_path: Option<String> = {
        let conn = agentfs.get_connection().await?;

        // Check if fs_overlay_config table exists and has base_path
        let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
        match conn.query(query, ()).await {
            Ok(mut rows) => {
                if let Ok(Some(row)) = rows.next().await {
                    row.get_value(0).ok().and_then(|v| {
                        if let Value::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    }; // conn is dropped here

    let fs: Arc<Mutex<dyn FileSystem + Send>> = if let Some(base_path) = base_path {
        eprintln!("Using overlay filesystem with base: {}", base_path);
        let hostfs = HostFS::new(&base_path)?;
        let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);
        overlay.load().await?; // Load persisted whiteouts and origin mappings
        Arc::new(Mutex::new(overlay)) as Arc<Mutex<dyn FileSystem + Send>>
    } else {
        Arc::new(Mutex::new(agentfs.fs)) as Arc<Mutex<dyn FileSystem + Send>>
    };

    // Create a temporary directory for the mount
    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-exec-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| id_or_path.clone())
    );

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname,
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    // Mount the filesystem
    let _mount_handle = mount_fs(fs, mount_opts).await?;

    // Run the command with the mountpoint as working directory
    let status = Command::new(&command)
        .args(&args)
        .current_dir(&mountpoint)
        .status()
        .with_context(|| format!("Failed to execute: {}", command.display()))?;

    // Drop the mount handle to unmount
    drop(_mount_handle);

    // Clean up the temporary directory
    let _ = std::fs::remove_dir_all(&mountpoint);

    // Exit with the command's exit code
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
