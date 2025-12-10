use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{AgentFS, AgentFSOptions};
use anyhow::{Context, Result as AnyhowResult};

pub async fn init_database(
    id: Option<String>,
    force: bool,
    base: Option<PathBuf>,
) -> AnyhowResult<()> {
    // Generate ID if not provided
    let id = id.unwrap_or_else(|| {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("agent-{}", timestamp)
    });

    // Validate agent ID for safety
    if !AgentFS::validate_agent_id(&id) {
        anyhow::bail!(
            "Invalid agent ID '{}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.",
            id
        );
    }

    // Validate base directory if provided
    if let Some(ref base_path) = base {
        if !base_path.exists() {
            anyhow::bail!("Base directory does not exist: {}", base_path.display());
        }
        if !base_path.is_dir() {
            anyhow::bail!("Base path is not a directory: {}", base_path.display());
        }
    }

    // Check if agent already exists
    let db_path = Path::new(".agentfs").join(format!("{}.db", id));
    if db_path.exists() && !force {
        anyhow::bail!(
            "Agent '{}' already exists at '{}'. Use --force to overwrite.",
            id,
            db_path.display()
        );
    }

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    let agent = AgentFS::open(AgentFSOptions::with_id(&id))
        .await
        .context("Failed to initialize database")?;

    // If base is provided, store the overlay configuration
    if let Some(base_path) = base {
        let conn = agent.get_connection();

        // Create whiteout table for overlay support
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_whiteout (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await
        .context("Failed to create whiteout table")?;

        // Create overlay config table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_overlay_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .context("Failed to create overlay config table")?;

        // Store base path configuration
        let base_path_str = base_path
            .canonicalize()
            .context("Failed to canonicalize base path")?
            .to_string_lossy()
            .to_string();

        conn.execute(
            "INSERT INTO fs_overlay_config (key, value) VALUES ('base_type', 'hostfs')",
            (),
        )
        .await
        .context("Failed to store base type")?;

        conn.execute(
            "INSERT INTO fs_overlay_config (key, value) VALUES ('base_path', ?)",
            (base_path_str.as_str(),),
        )
        .await
        .context("Failed to store base path")?;

        eprintln!("Created overlay filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        eprintln!("Base: {}", base_path_str);
    } else {
        eprintln!("Created agent filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
    }

    Ok(())
}
