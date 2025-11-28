pub mod filesystem;
pub mod kvstore;
pub mod toolcalls;

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use turso::{Builder, Connection};

pub use filesystem::{Filesystem, FilesystemStats, FsError, Stats};
pub use kvstore::KvStore;
pub use toolcalls::{ToolCall, ToolCallStats, ToolCallStatus, ToolCalls};

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
}

impl AgentFSOptions {
    /// Create options for a persistent agent with the given ID
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            path: None,
        }
    }

    /// Create options for an ephemeral in-memory agent
    pub fn ephemeral() -> Self {
        Self {
            id: None,
            path: None,
        }
    }

    /// Create options with a custom database path
    pub fn with_path(path: impl Into<String>) -> Self {
        Self {
            id: None,
            path: Some(path.into()),
        }
    }

    /// Resolve an id-or-path string to AgentFSOptions
    ///
    /// - `:memory:` -> ephemeral in-memory database
    /// - Existing file path -> uses that path directly
    /// - Otherwise -> treats as agent ID, looks for `.agentfs/{id}.db`
    ///
    /// Returns an error if the agent ID is invalid or the database doesn't exist.
    pub fn resolve(id_or_path: impl Into<String>) -> Result<Self> {
        let id_or_path = id_or_path.into();

        if id_or_path == ":memory:" {
            return Ok(Self::ephemeral());
        }

        let path = Path::new(&id_or_path);

        if path.exists() {
            Ok(Self::with_path(id_or_path))
        } else {
            // Treat as an agent ID - validate for safety
            if !AgentFS::validate_agent_id(&id_or_path) {
                anyhow::bail!(
                    "Invalid agent ID '{}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.",
                    id_or_path
                );
            }

            let db_path = Path::new(".agentfs").join(format!("{}.db", id_or_path));
            if !db_path.exists() {
                anyhow::bail!(
                    "Agent '{}' not found at '{}'",
                    id_or_path,
                    db_path.display()
                );
            }
            Ok(Self::with_path(db_path.to_str().ok_or_else(|| {
                anyhow::anyhow!("Database path '{}' is not valid UTF-8", db_path.display())
            })?))
        }
    }
}

/// The main AgentFS SDK struct
///
/// This provides a unified interface to the filesystem, key-value store,
/// and tool calls tracking backed by a SQLite database.
pub struct AgentFS {
    conn: Arc<Connection>,
    pub kv: KvStore,
    pub fs: Filesystem,
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
    /// # async fn example() -> anyhow::Result<()> {
    /// // Persistent storage
    /// let agent = AgentFS::open(AgentFSOptions::with_id("my-agent")).await?;
    ///
    /// // Ephemeral in-memory
    /// let agent = AgentFS::open(AgentFSOptions::ephemeral()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(options: AgentFSOptions) -> Result<Self> {
        // Determine database path: path takes precedence over id
        let db_path = if let Some(path) = options.path {
            // Custom path provided directly
            path
        } else if let Some(id) = options.id {
            // Validate agent ID to prevent path traversal attacks
            if !Self::validate_agent_id(&id) {
                anyhow::bail!(
                    "Invalid agent ID '{}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.",
                    id
                );
            }

            // Ensure .agentfs directory exists
            let agentfs_dir = Path::new(".agentfs");
            if !agentfs_dir.exists() {
                std::fs::create_dir_all(agentfs_dir)?;
            }
            format!(".agentfs/{}.db", id)
        } else {
            // No id or path = ephemeral in-memory database
            ":memory:".to_string()
        };

        let db = Builder::new_local(&db_path).build().await?;
        let conn = db.connect()?;
        let conn = Arc::new(conn);

        let kv = KvStore::from_connection(conn.clone()).await?;
        let fs = Filesystem::from_connection(conn.clone()).await?;
        let tools = ToolCalls::from_connection(conn.clone()).await?;

        Ok(Self {
            conn,
            kv,
            fs,
            tools,
        })
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
        let conn = db.connect()?;
        let conn = Arc::new(conn);

        let kv = KvStore::from_connection(conn.clone()).await?;
        let fs = Filesystem::from_connection(conn.clone()).await?;
        let tools = ToolCalls::from_connection(conn.clone()).await?;

        Ok(Self {
            conn,
            kv,
            fs,
            tools,
        })
    }

    /// Get the underlying database connection
    pub fn get_connection(&self) -> Arc<Connection> {
        self.conn.clone()
    }

    /// Validates an agent ID to prevent path traversal and ensure safe filesystem operations.
    /// Returns true if the ID contains only alphanumeric characters, hyphens, and underscores.
    pub fn validate_agent_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agentfs_creation() {
        let agentfs = AgentFS::open(AgentFSOptions::ephemeral()).await.unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection();
    }

    #[tokio::test]
    async fn test_agentfs_with_id() {
        let agentfs = AgentFS::open(AgentFSOptions::with_id("test-agent"))
            .await
            .unwrap();
        // Just verify we can get the connection
        let _conn = agentfs.get_connection();

        // Cleanup
        let _ = std::fs::remove_file(".agentfs/test-agent.db");
        let _ = std::fs::remove_file(".agentfs/test-agent.db-shm");
        let _ = std::fs::remove_file(".agentfs/test-agent.db-wal");
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
        agentfs.fs.mkdir("/test_dir").await.unwrap();

        // Check directory exists
        let stats = agentfs.fs.stat("/test_dir").await.unwrap();
        assert!(stats.is_some());
        assert!(stats.unwrap().is_directory());

        // Write a file
        let data = b"Hello, AgentFS!";
        agentfs
            .fs
            .write_file("/test_dir/test.txt", data)
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
        let _ = std::fs::create_dir_all(".agentfs");
        let db_path = std::path::Path::new(".agentfs").join("test-resolve-agent.db");
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
        assert!(result.unwrap_err().to_string().contains("Invalid agent ID"));

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
        let _ = std::fs::create_dir_all(".agentfs");

        // Test various valid ID formats
        let valid_ids = ["my-agent", "my_agent", "MyAgent123", "agent-123_test"];

        for id in valid_ids {
            let db_path = std::path::Path::new(".agentfs").join(format!("{}.db", id));
            std::fs::write(&db_path, b"test").unwrap();

            let opts = AgentFSOptions::resolve(id).unwrap();
            assert!(opts.id.is_none());
            assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

            // Cleanup
            let _ = std::fs::remove_file(&db_path);
        }
    }
}
