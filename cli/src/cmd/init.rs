use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{
    agentfs_dir, AgentFS, AgentFSOptions, EncryptionConfig, OverlayFS, PartialBootstrapStrategy,
    PartialSyncOpts, SyncOptions,
};
use anyhow::{Context, Result as AnyhowResult};

use crate::opts::{MountBackend, SyncCommandOptions};

pub struct EncryptionOptions {
    /// Hex-encoded encryption key
    pub key: String,
    /// Cipher algorithm
    pub cipher: String,
}

pub async fn open_agentfs(options: AgentFSOptions) -> Result<AgentFS, agentfs_sdk::error::Error> {
    let mut options = options;
    // CLI handles env var fallback for auth token
    if options.sync.auth_token.is_none() {
        if let Ok(auth_token) = std::env::var("TURSO_DB_AUTH_TOKEN") {
            options.sync.auth_token = Some(auth_token);
        }
    }
    AgentFS::open(options).await
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
    encryption: Option<EncryptionOptions>,
    command: Option<String>,
    backend: MountBackend,
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

    let mut open_options =
        AgentFSOptions::with_id(&id).with_sync(build_sync_options(&sync_options));
    if let Some(base_path) = base.as_ref() {
        open_options = open_options.with_base(base_path);
    }

    let encrypted = if let Some(enc_opts) = encryption {
        if sync_options.sync_remote_url.is_some() {
            anyhow::bail!("Local encryption is not supported with cloud sync");
        }
        if !enc_opts.key.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("Encryption key must be a valid hex string");
        }
        open_options = open_options.with_encryption(EncryptionConfig {
            hex_key: enc_opts.key,
            cipher: enc_opts.cipher,
        });
        true
    } else {
        false
    };

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    let agent = AgentFS::open(open_options)
        .await
        .context("Failed to initialize database")?;

    // If base is provided, initialize the overlay schema using the SDK
    if let Some(ref base_path) = base {
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
        if encrypted {
            eprintln!("Encryption: enabled");
        }
    } else {
        if agent.is_synced() {
            agent.push().await?;
        }

        eprintln!("Created agent filesystem: {}", db_path.display());
        eprintln!("Agent ID: {}", id);
        if encrypted {
            eprintln!("Encryption: enabled");
        }
    }

    // If a command was provided, mount the filesystem and execute it
    if let Some(cmd_str) = command {
        run_init_cmd(&id, cmd_str, backend, base, agent).await?;
    }

    Ok(())
}

#[cfg(unix)]
async fn run_init_cmd(
    id: &str,
    cmd_str: String,
    backend: MountBackend,
    base: Option<PathBuf>,
    agent: AgentFS,
) -> AnyhowResult<()> {
    use crate::mount::{mount_fs, MountOpts};
    use agentfs_sdk::{FileSystem, HostFS};
    use std::process::Command;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let fs: Arc<Mutex<dyn FileSystem + Send>> = if let Some(ref base_path) = base {
        let canonical = base_path
            .canonicalize()
            .context("Failed to canonicalize base path")?;
        let hostfs = HostFS::new(&canonical)?;
        let overlay = OverlayFS::new(Arc::new(hostfs), agent.fs);
        Arc::new(Mutex::new(overlay)) as Arc<Mutex<dyn FileSystem + Send>>
    } else {
        Arc::new(Mutex::new(agent.fs)) as Arc<Mutex<dyn FileSystem + Send>>
    };

    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-init-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname: format!("agentfs:{}", id),
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    let mount_handle = mount_fs(fs, mount_opts).await?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .current_dir(&mountpoint)
        .status()
        .with_context(|| format!("Failed to execute: {}", cmd_str))?;

    drop(mount_handle);

    let _ = std::fs::remove_dir_all(&mountpoint);

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

#[cfg(not(unix))]
async fn run_init_cmd(
    _id: &str,
    _cmd_str: String,
    _backend: MountBackend,
    _base: Option<PathBuf>,
    _agent: AgentFS,
) -> AnyhowResult<()> {
    anyhow::bail!("The -c option is not supported on Windows")
}
