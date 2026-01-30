//! Snapshot command - create point-in-time copies of agent filesystems.

use agentfs_sdk::{AgentFS, AgentFSOptions, EncryptionConfig};
use anyhow::{Context, Result};
use std::path::Path;

/// Create a snapshot of an agent filesystem.
///
/// This creates a consistent point-in-time copy of the database using
/// SQLite's VACUUM INTO command, which ensures atomicity and consistency.
pub async fn handle_snapshot_command(
    id_or_path: String,
    dest_path: &Path,
    encryption: Option<(&str, &str)>,
) -> Result<()> {
    // Resolve the source database
    let mut options =
        AgentFSOptions::resolve(&id_or_path).context("Failed to resolve agent ID or path")?;

    // Apply encryption if provided
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key.to_string(),
            cipher: cipher.to_string(),
        });
    }

    let agentfs = AgentFS::open(options)
        .await
        .context("Failed to open agent filesystem")?;

    // Convert destination path to string
    let dest_path_str = dest_path
        .to_str()
        .context("Destination path contains non-UTF8 characters")?;

    // Create the snapshot
    agentfs
        .snapshot(dest_path_str)
        .await
        .context("Failed to create snapshot")?;

    eprintln!("Snapshot created: {}", dest_path.display());

    Ok(())
}
