use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{AgentFS, AgentFSOptions};
use anyhow::{Context, Result as AnyhowResult};

pub async fn init_database(id: Option<String>, force: bool) -> AnyhowResult<()> {
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
    AgentFS::open(AgentFSOptions::with_id(&id))
        .await
        .context("Failed to initialize database")?;

    eprintln!("Created agent filesystem: {}", db_path.display());
    eprintln!("Agent ID: {}", id);

    Ok(())
}
