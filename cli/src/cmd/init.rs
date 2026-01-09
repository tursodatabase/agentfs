use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{agentfs_dir, AgentFS, AgentFSOptions, OverlayFS};
use anyhow::{Context, Result as AnyhowResult};
use turso::sync::{PartialBootstrapStrategy, PartialSyncOpts};

use crate::parser::SyncCommandOptions;

pub async fn open_agentfs(
    options: AgentFSOptions,
) -> anyhow::Result<(Option<turso::sync::Database>, AgentFS)> {
    let path = options.db_path()?;
    let meta_path = format!("{path}-info");
    if !std::fs::exists(meta_path)? {
        return Ok((
            None,
            AgentFS::open(options)
                .await
                .context("Failed to open database")?,
        ));
    }
    let mut builder = turso::sync::Builder::new_remote(&options.db_path()?);
    if let Ok(auth_token) = std::env::var("TURSO_DB_AUTH_TOKEN") {
        builder = builder.with_auth_token(auth_token);
    }
    if let Ok(encryption_key) = std::env::var("TURSO_REMOTE_ENCRYPTION_KEY") {
        builder = builder.with_remote_encryption_key(encryption_key);
    }
    let db = builder.build().await?;
    let conn = db.connect().await?;
    let agent = AgentFS::open_with(conn)
        .await
        .context("Failed to open synced database")?;
    Ok((Some(db), agent))
}

pub async fn create_agentfs(
    options: AgentFSOptions,
    sync_options: SyncCommandOptions,
) -> anyhow::Result<(Option<turso::sync::Database>, AgentFS)> {
    if let Some(remote_url) = sync_options.sync_remote_url {
        let mut builder =
            turso::sync::Builder::new_remote(&options.db_path()?).with_remote_url(remote_url);
        if let Ok(auth_token) = std::env::var("TURSO_DB_AUTH_TOKEN") {
            builder = builder.with_auth_token(auth_token);
        }
        if let Ok(encryption_key) = std::env::var("TURSO_REMOTE_ENCRYPTION_KEY") {
            builder = builder.with_remote_encryption_key(encryption_key);
        }
        let mut partial_sync = PartialSyncOpts {
            bootstrap_strategy: Some(PartialBootstrapStrategy::Prefix { length: 128 * 1024 }),
            prefetch: false,
            segment_size: 128 * 1024,
        };
        let mut has_partial_sync = false;
        if let Some(prefetch) = sync_options.sync_partial_prefetch {
            partial_sync.prefetch = prefetch;
            has_partial_sync = true;
        }
        if let Some(segment_size) = sync_options.sync_partial_segment_size {
            partial_sync.segment_size = segment_size;
            has_partial_sync = true;
        }
        if let Some(length) = sync_options.sync_partial_bootstrap_length {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Prefix { length });
            has_partial_sync = true;
        }
        if let Some(query) = sync_options.sync_partial_bootstrap_query {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Query { query });
            has_partial_sync = true;
        }
        if has_partial_sync {
            builder = builder.with_partial_sync_opts_experimental(partial_sync);
        }
        let db = builder.build().await?;
        let conn = db.connect().await?;
        let agent = AgentFS::open_with(conn)
            .await
            .context("Failed to initialize synced database")?;
        Ok((Some(db), agent))
    } else {
        Ok((
            None,
            AgentFS::open(options)
                .await
                .context("Failed to initialize database")?,
        ))
    }
}

pub async fn init_database(
    id: Option<String>,
    sync_options: SyncCommandOptions,
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
    if !AgentFSOptions::validate_agent_id(&id) {
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
    let db_path = agentfs_dir().join(format!("{}.db", &id));
    if db_path.exists() {
        if force {
            for entry in std::fs::read_dir(agentfs_dir())? {
                let entry = entry?;
                let file_name = entry.file_name();
                if file_name.to_string_lossy().starts_with(&id) {
                    std::fs::remove_file(entry.path())
                        .context("Failed to remove existing database file(s)")?;
                }
            }
        } else {
            anyhow::bail!(
                "Agent '{}' already exists at '{}'. Use --force to overwrite.",
                id,
                db_path.display()
            );
        }
    }

    let mut open_options = AgentFSOptions::with_id(&id);
    if let Some(base_path) = base.as_ref() {
        open_options = open_options.with_base(base_path);
    }

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    let (synced_db, agent) = create_agentfs(open_options, sync_options).await?;

    // If base is provided, initialize the overlay schema using the SDK
    if let Some(base_path) = base {
        let base_path_str = base_path
            .canonicalize()
            .context("Failed to canonicalize base path")?
            .to_string_lossy()
            .to_string();

        // Use SDK's OverlayFS::init_schema to ensure schema consistency
        OverlayFS::init_schema(&agent.get_connection(), &base_path_str)
            .await
            .context("Failed to initialize overlay schema")?;

        if let Some(synced_db) = synced_db {
            synced_db.push().await?;
        }

        eprintln!("Created overlay filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        eprintln!("Base: {}", base_path.display());
    } else {
        if let Some(synced_db) = synced_db {
            synced_db.push().await?;
        }

        eprintln!("Created agent filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
    }

    Ok(())
}
