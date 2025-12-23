//! Standalone NFS server command.
//!
//! This module provides a standalone NFS server that exports an AgentFS
//! filesystem over the network, allowing remote systems (like VMs) to mount
//! it as their root filesystem.

use agentfs_sdk::{agentfs_dir, AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use nfsserve::tcp::NFSTcp;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::Mutex;

use crate::nfs::AgentNFS;

/// Handle the `nfs` command - start a standalone NFS server.
pub async fn handle_nfs_command(id_or_path: String, bind: String, port: u32) -> Result<()> {
    // Resolve database path
    let db_path = resolve_db_path(&id_or_path)?;

    // Open or create the AgentFS database
    let db_path_str = db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;

    let agentfs = AgentFS::open(AgentFSOptions::with_path(db_path_str))
        .await
        .context("Failed to open AgentFS database")?;

    // Check if overlay is configured in the database
    let base_path = agentfs
        .is_overlay_enabled()
        .await
        .context("Failed to check overlay config")?;

    // Create filesystem - either direct AgentFS or overlay with base
    let fs: Arc<Mutex<dyn FileSystem>> = if let Some(base_str) = base_path {
        let hostfs = HostFS::new(&base_str).context("Failed to create HostFS")?;
        let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);

        eprintln!("Mode: overlay (base: {})", base_str);
        Arc::new(Mutex::new(overlay))
    } else {
        eprintln!("Mode: direct AgentFS");
        Arc::new(Mutex::new(agentfs.fs))
    };

    // Get current user/group for NFS file ownership
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Create NFS adapter
    let nfs = AgentNFS::new(fs, uid, gid);

    // Bind NFS server
    let bind_addr = format!("{}:{}", bind, port);
    let listener = nfsserve::tcp::NFSTcpListener::bind(&bind_addr, nfs)
        .await
        .with_context(|| format!("Failed to bind NFS server to {}", bind_addr))?;

    // Print server info
    eprintln!();
    eprintln!("AgentFS NFS Server");
    eprintln!("  Database: {}", db_path.display());
    eprintln!("  Listening: {}", bind_addr);
    eprintln!("  Export: /");
    eprintln!();
    eprintln!("Mount from client:");
    eprintln!(
        "  mount -t nfs -o vers=3,tcp,port={},mountport={},nolock {}:/ /mnt",
        port, port, bind
    );
    eprintln!();
    eprintln!("Press Ctrl+C to stop.");
    eprintln!();

    // Spawn the NFS server task
    let server_handle = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            eprintln!("NFS server error: {}", e);
        }
    });

    // Wait for Ctrl+C
    signal::ctrl_c()
        .await
        .context("Failed to listen for ctrl+c")?;

    eprintln!();
    eprintln!("Shutting down...");

    // Stop the server
    server_handle.abort();

    Ok(())
}

/// Resolve an agent ID or path to a database path.
fn resolve_db_path(id_or_path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(id_or_path);

    // If it looks like a path (contains / or ends with .db), use it directly
    if id_or_path.contains('/') || id_or_path.ends_with(".db") {
        return Ok(path);
    }

    // Otherwise, treat it as an agent ID and look in .agentfs/
    let agentfs_dir = agentfs_dir();
    let db_path = agentfs_dir.join(format!("{}.db", id_or_path));

    if db_path.exists() {
        Ok(db_path)
    } else {
        // If it doesn't exist, still return the path - AgentFS will create it
        Ok(db_path)
    }
}
