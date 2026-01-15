pub mod connection_pool;
pub mod error;
pub mod filesystem;
pub mod kvstore;
pub mod toolcalls;

use error::{Error, Result};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
};
use turso::{Builder, Value};

// Re-export turso sync types for CLI usage
pub use turso::sync::{DatabaseSyncStats, PartialBootstrapStrategy, PartialSyncOpts};

// Re-export filesystem types
#[cfg(unix)]
pub use filesystem::HostFS;
pub use filesystem::{
    BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, OverlayFS, Stats,
    DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
pub use kvstore::KvStore;
pub use toolcalls::{ToolCall, ToolCallStats, ToolCallStatus, ToolCalls};

/// Directory containing agentfs databases
pub fn agentfs_dir() -> &'static std::path::Path {
    std::path::Path::new(".agentfs")
}

/// Information about a mounted agentfs filesystem
#[derive(Debug, Clone)]
pub struct Mount {
    /// The ID (from the mount source, e.g., "agentfs:my-agent" -> "my-agent")
    pub id: String,
    /// The mountpoint path
    pub mountpoint: PathBuf,
}

/// Get all currently mounted agentfs filesystems by parsing /proc/mounts
///
/// This is the authoritative source for mount information - if it's in /proc/mounts,
/// it's mounted. If not, it's not. No stale state possible.
#[cfg(target_os = "linux")]
pub fn get_mounts() -> Vec<Mount> {
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        return vec![];
    };
    contents
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0].starts_with("agentfs:") {
                let agent_id = parts[0].strip_prefix("agentfs:")?.to_string();
                // Skip the internal "fuse" mount used by the daemon
                if agent_id == "fuse" {
                    return None;
                }
                Some(Mount {
                    id: agent_id,
                    mountpoint: PathBuf::from(parts[1]),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Get all currently mounted agentfs filesystems (non-Linux stub)
#[cfg(not(target_os = "linux"))]
pub fn get_mounts() -> Vec<Mount> {
    // On macOS, we could parse the output of `mount` command
    // For now, return empty - can be implemented later
    vec![]
}

/// Configuration options for sync
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Remote URL for syncing
    pub remote_url: Option<String>,
    /// Auth token for remote sync
    pub auth_token: Option<String>,
    /// Partial sync options
    pub partial_sync: Option<PartialSyncOpts>,
}

/// Configuration options for opening an AgentFS instance
#[derive(Debug, Clone, Default)]
pub struct AgentFSOptions {
    /// Optional unique identifier for the agent.
    /// - If Some(id): Creates persistent storage at `.agentfs/{id}.db`
    /// - If None: Uses ephemeral in-memory database
    pub id: Option<String>,
    /// Optional custom path to the database file.
    /// Takes precedence over `id` if both are set.
    pub path: Option<String>,
    /// Optional base directory for overlay filesystem (copy-on-write).
    /// When set, the filesystem operates as an overlay on top of this directory.
    pub base: Option<PathBuf>,
    /// Sync options for remote database synchronization
    pub sync: SyncOptions,
}

impl AgentFSOptions {
    /// Validates an agent ID to prevent path traversal and ensure safe filesystem operations.
    /// Returns true if the ID contains only alphanumeric characters, hyphens, and underscores.
    pub fn validate_agent_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }
    pub fn db_path(&self) -> Result<String> {
        // Determine database path: path takes precedence over id
        if let Some(path) = &self.path {
            // Custom path provided directly
            Ok(path.to_string())
        } else if let Some(id) = &self.id {
            // Validate agent ID to prevent path traversal attacks
            if !Self::validate_agent_id(id) {
                return Err(Error::InvalidAgentId(id.clone()));
            }

            // Ensure .agentfs directory exists
            let agentfs_dir = agentfs_dir();
            if !agentfs_dir.exists() {
                std::fs::create_dir_all(agentfs_dir)?;
            }
            Ok(format!("{}/{}.db", agentfs_dir.display(), id))
        } else {
            // No id or path = ephemeral in-memory database
            Ok(":memory:".to_string())
        }
    }
    /// Create options for a persistent agent with the given ID
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            path: None,
            base: None,
            sync: SyncOptions::default(),
        }
    }

    /// Create options for an ephemeral in-memory agent
    pub fn ephemeral() -> Self {
        Self {
            id: None,
            path: None,
            base: None,
            sync: SyncOptions::default(),
        }
    }

    /// Create options with a custom database path
    pub fn with_path(path: impl Into<String>) -> Self {
        Self {
            id: None,
            path: Some(path.into()),
            base: None,
            sync: SyncOptions::default(),
        }
    }

    /// Set sync options
    pub fn with_sync(mut self, sync: SyncOptions) -> Self {
        self.sync = sync;
        self
    }

    /// Set the base directory for overlay filesystem (copy-on-write)
    pub fn with_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.base = Some(base.into());
        self
    }

    /// Resolve an id-or-path string to AgentFSOptions
    ///
    /// Resolution order (first match wins):
    /// 1. `:memory:` -> ephemeral in-memory database
    /// 2. Valid agent ID with existing `.agentfs/{id}.db` -> uses that agent
    /// 3. Existing file path -> uses that path directly
    ///
    /// Returns an error if neither an agent nor a file exists.
    pub fn resolve(id_or_path: impl Into<String>) -> Result<Self> {
        let id_or_path = id_or_path.into();

        if id_or_path == ":memory:" {
            return Ok(Self::ephemeral());
        }

        // First, check if it's a valid agent ID with an existing database in .agentfs/
        if AgentFSOptions::validate_agent_id(&id_or_path) {
            let db_path = agentfs_dir().join(format!("{}.db", id_or_path));
            if db_path.exists() {
                return Ok(Self::with_path(db_path.to_str().ok_or_else(|| {
                    Error::InvalidUtf8Path(db_path.display().to_string())
                })?));
            }
        }

        // Fall back to treating as a direct file path
        let path = Path::new(&id_or_path);
        if path.is_file() {
            Ok(Self::with_path(id_or_path))
        } else if AgentFSOptions::validate_agent_id(&id_or_path) {
            // Not a valid agent and not an existing file
            Err(Error::AgentNotFound {
                id: id_or_path.clone(),
                path: agentfs_dir()
                    .join(format!("{}.db", id_or_path))
                    .display()
                    .to_string(),
            })
        } else {
            Err(Error::InvalidAgentId(id_or_path))
        }
    }
}

/// The main AgentFS SDK struct
///
/// This provides a unified interface to the filesystem, key-value store,
/// and tool calls tracking backed by a SQLite database.
pub struct AgentFS {
    pool: connection_pool::ConnectionPool,
    sync_db: Option<turso::sync::Database>,
    pub kv: KvStore,
    pub fs: filesystem::AgentFS,
    pub tools: ToolCalls,
}

impl AgentFS {
    /// Open an AgentFS instance
    ///
    /// # Arguments
    /// * `options` - Configuration options (use Default::default() for ephemeral)
    ///
    /// # Examples
    /// ```no_run
    /// use agentfs_sdk::{AgentFS, AgentFSOptions};
    ///
    /// # async fn example() -> agentfs_sdk::error::Result<()> {
    /// // Persistent storage
    /// let agent = AgentFS::open(AgentFSOptions::with_id("my-agent")).await?;
    ///
    /// // Ephemeral in-memory
    /// let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(options: AgentFSOptions) -> Result<Self> {
        // Validate base directory if provided
        if let Some(ref path) = options.base {
            if !path.exists() {
                return Err(Error::BaseDirectoryNotFound(path.display().to_string()));
            }
            if !path.is_dir() {
                return Err(Error::NotADirectory(path.display().to_string()));
            }
        }

        let db_path = options.db_path()?;
        let meta_path = format!("{db_path}-info");

        // Determine if this is a synced database:
        // 1. If sync.remote_url is set, create a new synced database
        // 2. If {path}-info file exists, open as existing synced database
        // 3. Otherwise, open as local database
        let (sync_db, pool) = if let Some(remote_url) = options.sync.remote_url {
            // Creating a new synced database
            let mut builder =
                turso::sync::Builder::new_remote(&db_path).with_remote_url(remote_url);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            if let Some(partial_sync) = options.sync.partial_sync {
                builder = builder.with_partial_sync_opts_experimental(partial_sync);
            }
            let db = builder.build().await?;
            let pool = connection_pool::ConnectionPool::new_sync(db.clone());
            (Some(db), pool)
        } else if std::fs::exists(&meta_path).unwrap_or(false) {
            // Opening existing synced database
            let mut builder = turso::sync::Builder::new_remote(&db_path);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            let db = builder.build().await?;
            let pool = connection_pool::ConnectionPool::new_sync(db.clone());
            (Some(db), pool)
        } else {
            // Local database
            let db = Builder::new_local(&db_path).build().await?;
            let pool = connection_pool::ConnectionPool::new(db);
            (None, pool)
        };

        // Initialize overlay schema if base is provided
        if let Some(base_path) = options.base {
            let canonical_base = std::fs::canonicalize(base_path)?;
            let base_path_str = canonical_base.to_string_lossy().to_string();
            let conn = pool.get_connection().await?;
            OverlayFS::init_schema(&conn, &base_path_str).await?;
        }

        Self::open_with_pool(pool, sync_db).await
    }

    /// Open an AgentFS instance from a connection pool
    pub async fn open_with_pool(
        pool: connection_pool::ConnectionPool,
        sync_db: Option<turso::sync::Database>,
    ) -> Result<Self> {
        let kv = KvStore::from_pool(pool.clone()).await?;
        let fs = filesystem::AgentFS::from_pool(pool.clone()).await?;
        let tools = ToolCalls::from_pool(pool.clone()).await?;

        Ok(Self {
            pool,
            sync_db,
            kv,
            fs,
            tools,
        })
    }

    /// Open an AgentFS instance from a sync database
    pub async fn open_with_sync_db(db: turso::sync::Database) -> Result<Self> {
        let pool = connection_pool::ConnectionPool::new_sync(db.clone());
        Self::open_with_pool(pool, Some(db)).await
    }

    /// Create a new AgentFS instance (deprecated, use `open` instead)
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file (use ":memory:" for in-memory database)
    #[deprecated(
        since = "0.2.0",
        note = "Use AgentFS::open with AgentFSOptions instead"
    )]
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = connection_pool::ConnectionPool::new(db);
        Self::open_with_pool(pool, None).await
    }

    /// Get a connection from the pool
    pub async fn get_connection(&self) -> Result<connection_pool::PooledConnection> {
        self.pool.get_connection().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> connection_pool::ConnectionPool {
        self.pool.clone()
    }

    /// Check if sync is enabled for this database
    pub fn is_synced(&self) -> bool {
        self.sync_db.is_some()
    }

    /// Get the underlying sync database (if sync is enabled)
    pub fn get_sync_db(&self) -> Option<&turso::sync::Database> {
        self.sync_db.as_ref()
    }

    /// Pull changes from remote database
    pub async fn pull(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.pull().await?;
        Ok(())
    }

    /// Push local changes to remote database
    pub async fn push(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.push().await?;
        Ok(())
    }

    /// Checkpoint the local database
    pub async fn checkpoint(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.checkpoint().await?;
        Ok(())
    }

    /// Get sync statistics
    pub async fn sync_stats(&self) -> Result<DatabaseSyncStats> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        let stats = db.stats().await?;
        Ok(stats)
    }

    /// Get all paths in the delta layer (files in fs_dentry)
    ///
    /// This returns all file and directory paths that exist in the overlay's
    /// delta layer, which represents files that have been added or modified.
    pub async fn get_delta_paths(&self) -> Result<HashSet<String>> {
        const ROOT_INO: i64 = 1;
        let conn = self.pool.get_connection().await?;

        let mut paths = HashSet::new();
        let mut queue: VecDeque<(i64, String)> = VecDeque::new();
        queue.push_back((ROOT_INO, String::new()));

        while let Some((parent_ino, prefix)) = queue.pop_front() {
            let query = format!(
                "SELECT d.name, d.ino, i.mode FROM fs_dentry d
                 JOIN fs_inode i ON d.ino = i.ino
                 WHERE d.parent_ino = {}
                 ORDER BY d.name",
                parent_ino
            );

            let mut rows = conn.query(&query, ()).await?;

            while let Some(row) = rows.next().await? {
                let name: String = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| {
                        if let Value::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let ino: i64 = row
                    .get_value(1)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);

                let mode: u32 = row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                let full_path = if prefix.is_empty() {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", prefix, name)
                };

                paths.insert(full_path.clone());

                let is_dir = mode & S_IFMT == S_IFDIR;
                if is_dir {
                    queue.push_back((ino, full_path));
                }
            }
        }

        Ok(paths)
    }

    /// Get the file mode for a path in the delta layer
    ///
    /// Returns the mode (file type and permissions) for a path, or None if
    /// the path doesn't exist in the delta layer.
    pub async fn get_file_mode(&self, path: &str) -> Result<Option<u32>> {
        const ROOT_INO: i64 = 1;
        let conn = self.pool.get_connection().await?;

        // Resolve path to inode
        let components: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        if components.is_empty() {
            // Root directory
            let mut rows = conn
                .query("SELECT mode FROM fs_inode WHERE ino = ?", (ROOT_INO,))
                .await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;
                return Ok(Some(mode));
            }
            return Ok(None);
        }

        let mut current_ino = ROOT_INO;
        for component in &components {
            let query = format!(
                "SELECT ino FROM fs_dentry WHERE parent_ino = {} AND name = '{}'",
                current_ino, component
            );

            let mut rows = conn.query(&query, ()).await?;

            if let Some(row) = rows.next().await? {
                current_ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);
            } else {
                return Ok(None);
            }
        }

        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (current_ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            return Ok(Some(mode));
        }

        Ok(None)
    }

    /// Get all whiteouts (deleted paths from base layer)
    ///
    /// Whiteouts mark paths that existed in the base layer but have been
    /// deleted in the overlay.
    pub async fn get_whiteouts(&self) -> Result<HashSet<String>> {
        let conn = self.pool.get_connection().await?;
        let mut whiteouts = HashSet::new();

        let result = conn.query("SELECT path FROM fs_whiteout", ()).await;

        if let Ok(mut rows) = result {
            while let Some(row) = rows.next().await? {
                if let Ok(Value::Text(path)) = row.get_value(0) {
                    whiteouts.insert(path.clone());
                }
            }
        } // Err case: Table doesn't exist, return empty set

        Ok(whiteouts)
    }

    /// Check if overlay is enabled for this filesystem
    ///
    /// Returns the base path if overlay is enabled, None otherwise.
    pub async fn is_overlay_enabled(&self) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;
        // Check if fs_overlay_config table exists and has base_path
        let result = conn
            .query(
                "SELECT value FROM fs_overlay_config WHERE key = 'base_path'",
                (),
            )
            .await;

        match result {
            Ok(mut rows) => {
                if let Some(row) = rows.next().await? {
                    let base_path: String = row
                        .get_value(0)
                        .ok()
                        .and_then(|v| {
                            if let Value::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    Ok(Some(base_path))
                } else {
                    Ok(None)
                }
            }
            Err(_) => Ok(None), // Table doesn't exist
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agentfs_creation() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection().await.unwrap();
    }

    #[tokio::test]
    async fn test_agentfs_with_id() {
        let agentfs = AgentFS::open(AgentFSOptions::with_id("test-agent"))
            .await
            .unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection().await.unwrap();

        // Cleanup
        let agentfs_dir = agentfs_dir();
        let file_names = ["test-agent.db", "test-agent.db-shm", "test-agent.db-wal"];
        for file_name in file_names {
            let _ = std::fs::remove_file(agentfs_dir.join(file_name));
        }
    }

    #[tokio::test]
    async fn test_kv_operations() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Set a value
        agentfs.kv.set("test_key", &"test_value").await.unwrap();

        // Get the value
        let value: Option<String> = agentfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, Some("test_value".to_string()));

        // Delete the value
        agentfs.kv.delete("test_key").await.unwrap();

        // Verify deletion
        let value: Option<String> = agentfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_filesystem_operations() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Create a directory
        agentfs.fs.mkdir("/test_dir", 0, 0).await.unwrap();

        // Check directory exists
        let stats = agentfs.fs.stat("/test_dir").await.unwrap();
        assert!(stats.is_some());
        assert!(stats.unwrap().is_directory());

        // Write a file
        let data = b"Hello, AgentFS!";
        agentfs
            .fs
            .write_file("/test_dir/test.txt", data, 0, 0)
            .await
            .unwrap();

        // Read the file
        let read_data = agentfs
            .fs
            .read_file("/test_dir/test.txt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_data, data);

        // List directory
        let entries = agentfs.fs.readdir("/test_dir").await.unwrap().unwrap();
        assert_eq!(entries, vec!["test.txt"]);
    }

    #[tokio::test]
    async fn test_tool_calls() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();

        // Start a tool call
        let id = agentfs
            .tools
            .start("test_tool", Some(serde_json::json!({"param": "value"})))
            .await
            .unwrap();

        // Mark it as successful
        agentfs
            .tools
            .success(id, Some(serde_json::json!({"result": "success"})))
            .await
            .unwrap();

        // Get the tool call
        let call = agentfs.tools.get(id).await.unwrap().unwrap();
        assert_eq!(call.name, "test_tool");
        assert_eq!(call.status, ToolCallStatus::Success);

        // Get stats
        let stats = agentfs.tools.stats_for("test_tool").await.unwrap().unwrap();
        assert_eq!(stats.total_calls, 1);
        assert_eq!(stats.successful, 1);
    }

    #[test]
    fn test_resolve_memory() {
        let opts = AgentFSOptions::resolve(":memory:").unwrap();
        assert!(opts.id.is_none());
        assert!(opts.path.is_none());
    }

    #[test]
    fn test_resolve_existing_file_path() {
        // Create a temporary file to test with
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_resolve_existing.db");
        std::fs::write(&temp_file, b"test").unwrap();

        let opts = AgentFSOptions::resolve(temp_file.to_str().unwrap()).unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(temp_file.to_str().unwrap().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_resolve_valid_agent_id_with_existing_db() {
        // Setup: create .agentfs directory and a test database
        let agentfs_dir = agentfs_dir();
        let _ = std::fs::create_dir_all(agentfs_dir);
        let db_path = agentfs_dir.join("test-resolve-agent.db");
        std::fs::write(&db_path, b"test").unwrap();

        let opts = AgentFSOptions::resolve("test-resolve-agent").unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_resolve_invalid_agent_id() {
        // Agent IDs with path traversal should be rejected
        let result = AgentFSOptions::resolve("../evil");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid agent ID"));

        // Agent IDs with spaces should be rejected
        let result = AgentFSOptions::resolve("invalid agent");
        assert!(result.is_err());

        // Agent IDs with special characters should be rejected
        let result = AgentFSOptions::resolve("agent@test");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_nonexistent_agent() {
        let result = AgentFSOptions::resolve("nonexistent-agent-12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_valid_agent_id_formats() {
        // Setup: create .agentfs directory and test databases
        let agentfs_dir = agentfs_dir();
        let _ = std::fs::create_dir_all(agentfs_dir);

        // Test various valid ID formats
        let valid_ids = ["my-agent", "my_agent", "MyAgent123", "agent-123_test"];

        for id in valid_ids {
            let db_path = agentfs_dir.join(format!("{}.db", id));
            std::fs::write(&db_path, b"test").unwrap();

            let opts = AgentFSOptions::resolve(id).unwrap();
            assert!(opts.id.is_none());
            assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

            // Cleanup
            let _ = std::fs::remove_file(&db_path);
        }
    }
}
