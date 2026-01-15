use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{
    agentfs_dir, AgentFS, AgentFSOptions, CompressionMode, OverlayFS, PartialBootstrapStrategy,
    PartialSyncOpts, SyncOptions,
};
use anyhow::{Context, Result as AnyhowResult};

use crate::parser::SyncCommandOptions;

pub async fn open_agentfs(options: AgentFSOptions) -> anyhow::Result<AgentFS> {
    let mut options = options;
    // CLI handles env var fallback for auth token
    if options.sync.auth_token.is_none() {
        if let Ok(auth_token) = std::env::var("TURSO_DB_AUTH_TOKEN") {
            options.sync.auth_token = Some(auth_token);
        }
    }
    AgentFS::open(options)
        .await
        .context("Failed to open database")
}

pub fn build_sync_options(sync_cmd_options: &SyncCommandOptions) -> SyncOptions {
    let mut sync = SyncOptions {
        remote_url: sync_cmd_options.sync_remote_url.clone(),
        auth_token: std::env::var("TURSO_DB_AUTH_TOKEN").ok(),
        partial_sync: None,
    };

    if sync_cmd_options.sync_remote_url.is_some() {
        let mut partial_sync = PartialSyncOpts {
            bootstrap_strategy: Some(PartialBootstrapStrategy::Prefix { length: 128 * 1024 }),
            prefetch: false,
            segment_size: 128 * 1024,
        };
        let mut has_partial_sync = false;

        if let Some(prefetch) = sync_cmd_options.sync_partial_prefetch {
            partial_sync.prefetch = prefetch;
            has_partial_sync = true;
        }
        if let Some(segment_size) = sync_cmd_options.sync_partial_segment_size {
            partial_sync.segment_size = segment_size;
            has_partial_sync = true;
        }
        if let Some(length) = sync_cmd_options.sync_partial_bootstrap_length {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Prefix { length });
            has_partial_sync = true;
        }
        if let Some(ref query) = sync_cmd_options.sync_partial_bootstrap_query {
            partial_sync.bootstrap_strategy = Some(PartialBootstrapStrategy::Query {
                query: query.clone(),
            });
            has_partial_sync = true;
        }

        if has_partial_sync {
            sync.partial_sync = Some(partial_sync);
        }
    }

    sync
}

pub async fn init_database(
    id: Option<String>,
    sync_options: SyncCommandOptions,
    force: bool,
    base: Option<PathBuf>,
    compression: String,
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

    // Parse compression mode
    let compression_mode = CompressionMode::parse(&compression).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid compression mode '{}'. Must be 'zstd' or 'none'",
            compression
        )
    })?;

    let mut open_options = AgentFSOptions::with_id(&id)
        .with_sync(build_sync_options(&sync_options))
        .with_compression(compression_mode);
    if let Some(base_path) = base.as_ref() {
        open_options = open_options.with_base(base_path);
    }

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    let agent = AgentFS::open(open_options)
        .await
        .context("Failed to initialize database")?;

    // If base is provided, initialize the overlay schema using the SDK
    if let Some(base_path) = base {
        let base_path_str = base_path
            .canonicalize()
            .context("Failed to canonicalize base path")?
            .to_string_lossy()
            .to_string();

        // Use SDK's OverlayFS::init_schema to ensure schema consistency
        let conn = agent.get_connection().await?;
        OverlayFS::init_schema(&conn, &base_path_str)
            .await
            .context("Failed to initialize overlay schema")?;

        if agent.is_synced() {
            agent.push().await?;
        }

        eprintln!("Created overlay filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        eprintln!("Base: {}", base_path.display());
    } else {
        if agent.is_synced() {
            agent.push().await?;
        }

        eprintln!("Created agent filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
    }

    Ok(())
}
