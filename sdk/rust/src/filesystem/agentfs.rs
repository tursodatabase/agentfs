use anyhow::Result;
use async_trait::async_trait;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use turso::{Builder, Connection, Value};

use super::{
    BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats, DEFAULT_DIR_MODE,
    DEFAULT_FILE_MODE, S_IFLNK, S_IFMT, S_IFREG,
};
use crate::connection_pool::ConnectionPool;

const ROOT_INO: i64 = 1;
const DEFAULT_CHUNK_SIZE: usize = 4096;
const DENTRY_CACHE_MAX_SIZE: usize = 10000;

/// LRU cache for directory entry lookups.
///
/// Maps (parent_ino, name) -> child_ino to avoid repeated database queries
/// during path resolution. For a path like `/a/b/c/d`, this reduces queries
/// from 4 to potentially 0 on cache hits.
struct DentryCache {
    // Mutex required because LruCache::get() mutates internal order
    entries: Mutex<LruCache<(i64, String), i64>>,
}

impl DentryCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    /// Look up a cached entry (updates LRU order)
    fn get(&self, parent_ino: i64, name: &str) -> Option<i64> {
        self.entries
            .lock()
            .unwrap()
            .get(&(parent_ino, name.to_string()))
            .copied()
    }

    /// Insert an entry into the cache (evicts LRU entry if full)
    fn insert(&self, parent_ino: i64, name: &str, child_ino: i64) {
        self.entries
            .lock()
            .unwrap()
            .put((parent_ino, name.to_string()), child_ino);
    }

    /// Remove an entry from the cache
    fn remove(&self, parent_ino: i64, name: &str) {
        self.entries
            .lock()
            .unwrap()
            .pop(&(parent_ino, name.to_string()));
    }
}

/// A filesystem backed by SQLite
#[derive(Clone)]
pub struct AgentFS {
    pool: ConnectionPool,
    chunk_size: usize,
    /// Cache for directory entry lookups (shared across clones)
    dentry_cache: Arc<DentryCache>,
}

/// An open file handle for AgentFS.
///
/// This struct holds the inode number resolved at open time, allowing
/// efficient read/write/fsync operations without path lookups.
pub struct AgentFSFile {
    pool: ConnectionPool,
    ino: i64,
    chunk_size: usize,
}

#[async_trait]
impl File for AgentFSFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let conn = self.pool.get_conn().await?;
        let chunk_size = self.chunk_size as u64;
        let start_chunk = offset / chunk_size;
        let end_chunk = (offset + size).saturating_sub(1) / chunk_size;

        let mut stmt = conn
            .prepare_cached("SELECT chunk_index, data FROM fs_data WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ? ORDER BY chunk_index")
            .await?;
        let mut rows = stmt
            .query((self.ino, start_chunk as i64, end_chunk as i64))
            .await?;

        let mut result = Vec::with_capacity(size as usize);
        let start_offset_in_chunk = (offset % chunk_size) as usize;
        let mut next_expected_chunk = start_chunk;

        while let Some(row) = rows.next().await? {
            let chunk_index = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64;

            // Fill gaps with zeros for sparse files
            while next_expected_chunk < chunk_index && result.len() < size as usize {
                let skip = if next_expected_chunk == start_chunk {
                    start_offset_in_chunk
                } else {
                    0
                };
                let zeros_needed =
                    std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
                result.extend(std::iter::repeat_n(0u8, zeros_needed));
                next_expected_chunk += 1;
            }

            if let Ok(Value::Blob(chunk_data)) = row.get_value(1) {
                let skip = if chunk_index == start_chunk {
                    start_offset_in_chunk
                } else {
                    0
                };
                if skip >= chunk_data.len() {
                    // Chunk is smaller than skip offset, fill with zeros
                    let zeros_needed =
                        std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
                    result.extend(std::iter::repeat_n(0u8, zeros_needed));
                } else {
                    let remaining = size as usize - result.len();
                    let take = std::cmp::min(chunk_data.len() - skip, remaining);
                    result.extend_from_slice(&chunk_data[skip..skip + take]);

                    // If chunk is smaller than chunk_size, pad with zeros
                    let chunk_end = skip + take;
                    if chunk_end < chunk_size as usize && result.len() < size as usize {
                        let zeros_needed = std::cmp::min(
                            chunk_size as usize - chunk_end,
                            size as usize - result.len(),
                        );
                        result.extend(std::iter::repeat_n(0u8, zeros_needed));
                    }
                }
            }
            next_expected_chunk = chunk_index + 1;
        }

        // Fill any remaining space with zeros (for sparse file tail or missing chunks at end)
        if result.len() < size as usize {
            result.resize(size as usize, 0);
        }

        Ok(result)
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let conn = self.pool.get_conn().await?;

        // Get current file size
        let mut stmt = conn
            .prepare_cached("SELECT size FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;
        let current_size = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        // If writing beyond current size, extend with zeros first
        if offset > current_size {
            let zeros = vec![0u8; (offset - current_size) as usize];
            self.write_data_at_offset_with_conn(&conn, current_size, &zeros)
                .await?;
        }

        // Write the actual data
        self.write_data_at_offset_with_conn(&conn, offset, data)
            .await?;

        // Update file size and mtime
        let new_size = std::cmp::max(current_size, offset + data.len() as u64);
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?")
            .await?;
        stmt.execute((new_size as i64, now, self.ino)).await?;

        Ok(())
    }

    async fn truncate(&self, new_size: u64) -> Result<()> {
        let conn = self.pool.get_conn().await?;

        // Get current size
        let mut stmt = conn
            .prepare_cached("SELECT size FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;
        let current_size = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        let chunk_size = self.chunk_size as u64;

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let result: Result<()> = async {
            if new_size == 0 {
                // Special case: truncate to zero - just delete all chunks
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                    .await?;
                stmt.execute((self.ino,)).await?;
            } else if new_size < current_size {
                // Shrinking: delete excess chunks and truncate last chunk if needed
                let last_chunk_idx = (new_size - 1) / chunk_size;

                // Delete all chunks beyond the last one we need
                conn.execute(
                    "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
                    (self.ino, last_chunk_idx as i64),
                )
                .await?;

                // Truncate the last chunk if needed
                let offset_in_chunk = (new_size % chunk_size) as usize;
                if offset_in_chunk > 0 {
                    let mut stmt = conn
                        .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                        .await?;
                    let mut rows = stmt.query((self.ino, last_chunk_idx as i64)).await?;

                    if let Some(row) = rows.next().await? {
                        if let Ok(Value::Blob(mut chunk_data)) = row.get_value(0) {
                            if chunk_data.len() > offset_in_chunk {
                                chunk_data.truncate(offset_in_chunk);
                                let mut stmt = conn
                                    .prepare_cached("UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?")
                                    .await?;
                                stmt.execute((Value::Blob(chunk_data), self.ino, last_chunk_idx as i64)).await?;
                            }
                        }
                    }
                }
            }
            // For extending (new_size > current_size), we just update the size
            // The sparse regions will be handled by pread returning zeros

            // Update the inode size and mtime
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?")
                .await?;
            stmt.execute((new_size as i64, now, self.ino)).await?;

            Ok(())
        }
        .await;

        if result.is_err() {
            let _ = conn
                .prepare_cached("ROLLBACK")
                .await?
                .execute(())
                .await;
            return result;
        }

        conn.prepare_cached("COMMIT")
            .await?
            .execute(())
            .await?;
        Ok(())
    }

    async fn fsync(&self) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        conn.prepare_cached("PRAGMA synchronous = FULL")
            .await?
            .execute(())
            .await?;
        conn.prepare_cached("BEGIN").await?.execute(()).await?;
        conn.prepare_cached("COMMIT")
            .await?
            .execute(())
            .await?;
        conn.prepare_cached("PRAGMA synchronous = OFF")
            .await?
            .execute(())
            .await?;
        Ok(())
    }

    async fn fstat(&self) -> Result<Stats> {
        let conn = self.pool.get_conn().await?;
        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;

        if let Some(row) = rows.next().await? {
            AgentFS::build_stats_from_row(&row)
        } else {
            Err(FsError::NotFound.into())
        }
    }
}

impl AgentFSFile {
    /// Write data at a specific offset, handling chunk boundaries.
    /// Uses a provided connection to allow reuse within a transaction.
    async fn write_data_at_offset_with_conn(
        &self,
        conn: &Connection,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let chunk_size = self.chunk_size as u64;
        let mut written = 0usize;

        while written < data.len() {
            let current_offset = offset + written as u64;
            let chunk_index = (current_offset / chunk_size) as i64;
            let offset_in_chunk = (current_offset % chunk_size) as usize;

            // How much can we write in this chunk?
            let remaining_in_chunk = self.chunk_size - offset_in_chunk;
            let remaining_data = data.len() - written;
            let to_write = std::cmp::min(remaining_in_chunk, remaining_data);

            // Get existing chunk data (if any)
            let mut stmt = conn
                .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                .await?;
            let mut rows = stmt.query((self.ino, chunk_index)).await?;

            let mut chunk_data = if let Some(row) = rows.next().await? {
                row.get_value(0)
                    .ok()
                    .and_then(|v| {
                        if let Value::Blob(b) = v {
                            Some(b.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            // Extend chunk if needed
            if chunk_data.len() < offset_in_chunk + to_write {
                chunk_data.resize(offset_in_chunk + to_write, 0);
            }

            // Write data into chunk
            chunk_data[offset_in_chunk..offset_in_chunk + to_write]
                .copy_from_slice(&data[written..written + to_write]);

            // Save chunk
            let mut stmt = conn
                .prepare_cached(
                    "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                )
                .await?;
            stmt.execute((self.ino, chunk_index, Value::Blob(chunk_data)))
                .await?;

            written += to_write;
        }

        Ok(())
    }
}

impl AgentFS {
    /// Create a new filesystem
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        Self::from_pool(ConnectionPool::new(db)).await
    }

    /// Create a filesystem from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        let conn = pool.get_conn().await?;

        // Initialize schema first
        Self::initialize_schema(&conn).await?;

        // Disable synchronous mode for filesystem fsync() semantics.
        conn.execute("PRAGMA synchronous = OFF", ()).await?;

        // Set busy timeout to handle concurrent access gracefully.
        // Without this, concurrent transactions fail immediately with SQLITE_BUSY.
        conn.execute("PRAGMA busy_timeout = 5000", ()).await?;

        // Get chunk_size from config (or use default)
        let chunk_size = Self::read_chunk_size(&conn).await?;

        let fs = Self {
            pool,
            chunk_size,
            dentry_cache: Arc::new(DentryCache::new(DENTRY_CACHE_MAX_SIZE)),
        };
        Ok(fs)
    }

    /// Get the configured chunk size
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Get a database connection from the pool
    pub async fn get_conn(&self) -> anyhow::Result<crate::connection_pool::PooledConnection> {
        self.pool.get_conn().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> ConnectionPool {
        self.pool.clone()
    }

    /// Initialize the database schema
    pub async fn initialize_schema(conn: &Connection) -> Result<()> {
        // Create config table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await?;

        // Create inode table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_inode (
                ino INTEGER PRIMARY KEY AUTOINCREMENT,
                mode INTEGER NOT NULL,
                nlink INTEGER NOT NULL DEFAULT 0,
                uid INTEGER NOT NULL DEFAULT 0,
                gid INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL DEFAULT 0,
                atime INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                ctime INTEGER NOT NULL
            )",
            (),
        )
        .await?;

        // Create directory entry table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_dentry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parent_ino INTEGER NOT NULL,
                ino INTEGER NOT NULL,
                UNIQUE(parent_ino, name)
            )",
            (),
        )
        .await?;

        // Create index for efficient path lookups
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
            ON fs_dentry(parent_ino, name)",
            (),
        )
        .await?;

        // Create data chunks table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_data (
                ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (ino, chunk_index)
            )",
            (),
        )
        .await?;

        // Create symlink table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_symlink (
                ino INTEGER PRIMARY KEY,
                target TEXT NOT NULL
            )",
            (),
        )
        .await?;

        // Ensure chunk_size config exists
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        if rows.next().await?.is_none() {
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)",
                (DEFAULT_CHUNK_SIZE.to_string(),),
            )
            .await?;
        }

        // Ensure root directory exists
        let mut rows = conn
            .query("SELECT ino FROM fs_inode WHERE ino = ?", (ROOT_INO,))
            .await?;

        if rows.next().await?.is_none() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            conn.execute(
                "INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
                VALUES (?, ?, 1, 0, 0, 0, ?, ?, ?)",
                (ROOT_INO, DEFAULT_DIR_MODE as i64, now, now, now),
            )
            .await?;
        }

        Ok(())
    }

    /// Read chunk size from config
    async fn read_chunk_size(conn: &Connection) -> Result<usize> {
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        if let Some(row) = rows.next().await? {
            let value = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => s.parse::<usize>().ok(),
                    Value::Integer(i) => Some(i as usize),
                    _ => None,
                })
                .unwrap_or(DEFAULT_CHUNK_SIZE);
            Ok(value)
        } else {
            Ok(DEFAULT_CHUNK_SIZE)
        }
    }

    /// Normalize a path
    fn normalize_path(&self, path: &str) -> String {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() {
            "/"
        } else if normalized.starts_with('/') {
            normalized
        } else {
            return format!("/{}", normalized);
        };

        // Handle . and .. components
        let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let mut result = Vec::new();

        for component in components {
            match component {
                "." => {
                    // Current directory - skip it
                    continue;
                }
                ".." => {
                    // Parent directory - only pop if there is a component to pop (don't traverse above root)
                    if !result.is_empty() {
                        result.pop();
                    }
                }
                _ => {
                    result.push(component);
                }
            }
        }

        if result.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", result.join("/"))
        }
    }

    /// Split path into components
    fn split_path(&self, path: &str) -> Vec<String> {
        let normalized = self.normalize_path(path);
        if normalized == "/" {
            return vec![];
        }
        normalized
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// Look up a child entry by parent inode and name using a provided connection.
    ///
    /// This is more efficient than `resolve_path` when you already have the parent inode,
    /// as it avoids re-resolving all parent path components.
    async fn lookup_child(
        &self,
        conn: &Connection,
        parent_ino: i64,
        name: &str,
    ) -> Result<Option<i64>> {
        let mut rows = conn
            .query(
                "SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?",
                (parent_ino, name),
            )
            .await?;

        let mut found_ino = None;
        let mut row_count = 0;

        while let Some(row) = rows.next().await? {
            found_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
            row_count += 1;
        }

        if row_count > 1 {
            return Err(FsError::InvalidPath.into());
        }

        Ok(found_ino)
    }

    /// Get link count for an inode
    async fn get_link_count(&self, conn: &Connection, ino: i64) -> Result<u32> {
        let mut stmt = conn
            .prepare_cached("SELECT nlink FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let nlink = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            Ok(nlink as u32)
        } else {
            Ok(0)
        }
    }

    /// Build a Stats object from a database row
    ///
    /// The row should contain columns in this order:
    /// ino, mode, nlink, uid, gid, size, atime, mtime, ctime
    fn build_stats_from_row(row: &turso::Row) -> Result<Stats> {
        Ok(Stats {
            ino: row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            mode: row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            nlink: row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(1) as u32,
            uid: row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            gid: row
                .get_value(4)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            size: row
                .get_value(5)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            atime: row
                .get_value(6)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            mtime: row
                .get_value(7)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            ctime: row
                .get_value(8)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
        })
    }

    /// Resolve a path to an inode number
    async fn resolve_path(&self, path: &str) -> Result<Option<i64>> {
        let conn = self.pool.get_conn().await?;
        self.resolve_path_with_conn(&conn, path).await
    }

    /// Resolve a path to an inode number using a provided connection
    async fn resolve_path_with_conn(
        &self,
        conn: &Connection,
        path: &str,
    ) -> Result<Option<i64>> {
        let components = self.split_path(path);
        if components.is_empty() {
            return Ok(Some(ROOT_INO));
        }

        let mut current_ino = ROOT_INO;
        for component in components {
            // Check cache first
            if let Some(cached_ino) = self.dentry_cache.get(current_ino, &component) {
                current_ino = cached_ino;
                continue;
            }

            // Cache miss - query database
            let mut statement = conn
                .prepare_cached("SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?")
                .await?;
            let mut rows = statement.query((current_ino, component.as_str())).await?;

            let mut found_row = None;
            let mut row_count = 0;

            while let Some(row) = rows.next().await? {
                found_row = Some(row);
                row_count += 1;
            }

            if row_count > 1 {
                return Err(FsError::InvalidPath.into());
            }

            if let Some(row) = found_row {
                let child_ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);

                // Populate cache
                self.dentry_cache.insert(current_ino, &component, child_ino);
                current_ino = child_ino;
            } else {
                return Ok(None);
            }
        }

        Ok(Some(current_ino))
    }

    /// Get file statistics without following symlinks
    pub async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let ino = match self.resolve_path_with_conn(&conn, &path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = conn
            .query(
                "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?",
                (ino,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let stats = Self::build_stats_from_row(&row)?;
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    /// Get file statistics, following symlinks
    pub async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);

        // Follow symlinks with a maximum depth to prevent infinite loops
        let mut current_path = path;
        let max_symlink_depth = 40; // Standard limit for symlink following

        for _ in 0..max_symlink_depth {
            let ino = match self.resolve_path_with_conn(&conn, &current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };

            let mut rows = conn
                .query(
                    "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?",
                    (ino,),
                )
                .await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(1)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                // Check if this is a symlink
                if (mode & S_IFMT) == S_IFLNK {
                    // Read the symlink target
                    let target = self
                        .readlink_with_conn(&conn, &current_path)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Symlink has no target"))?;

                    // Resolve target path (handle both absolute and relative paths)
                    current_path = if target.starts_with('/') {
                        target
                    } else {
                        // Relative path - resolve relative to the symlink's directory
                        let base_path = Path::new(&current_path);
                        let parent = base_path.parent().unwrap_or(Path::new("/"));
                        let joined = parent.join(&target);
                        joined.to_string_lossy().into_owned()
                    };
                    current_path = self.normalize_path(&current_path);
                    continue; // Follow the symlink
                }

                // Not a symlink, return the stats
                let stats = Self::build_stats_from_row(&row)?;
                return Ok(Some(stats));
            } else {
                return Ok(None);
            }
        }

        // Too many symlinks
        return Err(FsError::SymlinkLoop.into())
    }

    /// Get file statistics, following symlinks (using provided connection)
    async fn stat_with_conn(&self, conn: &Connection, path: &str) -> Result<Option<Stats>> {
        let path = self.normalize_path(path);

        // Follow symlinks with a maximum depth to prevent infinite loops
        let mut current_path = path;
        let max_symlink_depth = 40; // Standard limit for symlink following

        for _ in 0..max_symlink_depth {
            let ino = match self.resolve_path_with_conn(conn, &current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };

            let mut rows = conn
                .query(
                    "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?",
                    (ino,),
                )
                .await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(1)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                // Check if this is a symlink
                if (mode & S_IFMT) == S_IFLNK {
                    // Read the symlink target
                    let target = self
                        .readlink_with_conn(conn, &current_path)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Symlink has no target"))?;

                    // Resolve target path (handle both absolute and relative paths)
                    current_path = if target.starts_with('/') {
                        target
                    } else {
                        // Relative path - resolve relative to the symlink's directory
                        let base_path = Path::new(&current_path);
                        let parent = base_path.parent().unwrap_or(Path::new("/"));
                        let joined = parent.join(&target);
                        joined.to_string_lossy().into_owned()
                    };
                    current_path = self.normalize_path(&current_path);
                    continue; // Follow the symlink
                }

                // Not a symlink, return the stats
                let stats = Self::build_stats_from_row(&row)?;
                return Ok(Some(stats));
            } else {
                return Ok(None);
            }
        }

        // Too many symlinks
        return Err(FsError::SymlinkLoop.into())
    }

    /// Create a directory
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create inode
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let mut stmt = conn
            .prepare(
                "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                VALUES (?, 0, 0, 0, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let row = stmt
            .query_row((DEFAULT_DIR_MODE as i64, now, now, now))
            .await?;

        let ino = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?;

        // Create directory entry
        let mut stmt = conn
            .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
            .await?;
        stmt.execute((name.as_str(), parent_ino, ino)).await?;

        // Increment link count
        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
            .await?;
        stmt.execute((ino,)).await?;

        // Populate dentry cache
        self.dentry_cache.insert(parent_ino, name, ino);

        Ok(())
    }

    /// Write data to a file
    pub async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let result: Result<()> = async {
            // Check if file exists (single query using parent_ino we already have)
            let ino = if let Some(ino) = self.lookup_child(&conn, parent_ino, name).await? {
                // Delete existing data
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;
                ino
            } else {
                // Create new inode
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
                let mut stmt = conn
                    .prepare(
                        "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                        VALUES (?, 0, 0, ?, ?, ?, ?) RETURNING ino",
                    )
                    .await?;
                let row = stmt
                    .query_row((DEFAULT_FILE_MODE as i64, data.len() as i64, now, now, now))
                    .await?;

                let ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?;

                // Create directory entry
                let mut stmt = conn
                    .prepare_cached(
                        "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                    )
                    .await?;
                stmt.execute((name.as_str(), parent_ino, ino)).await?;

                // Increment link count
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;

                // Populate dentry cache for new file
                self.dentry_cache.insert(parent_ino, name, ino);

                ino
            };

            // Write data in chunks
            for (chunk_index, chunk) in data.chunks(self.chunk_size).enumerate() {
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                    (ino, chunk_index as i64, chunk),
                )
                .await?;
            }

            // Update mode (to regular file), size and mtime
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET mode = ?, size = ?, mtime = ? WHERE ino = ?")
                .await?;
            stmt.execute((DEFAULT_FILE_MODE as i64, data.len() as i64, now, ino))
                .await?;

            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                conn.prepare_cached("COMMIT")
                    .await?
                    .execute(())
                    .await?;
                Ok(())
            }
            Err(e) => {
                let _ = conn
                    .prepare_cached("ROLLBACK")
                    .await?
                    .execute(())
                    .await;
                Err(e)
            }
        }
    }

    /// Create a new empty file with the specified mode.
    ///
    /// This is an optimized path for FUSE create() that combines inode creation,
    /// dentry creation, and file handle opening in a single operation.
    /// Returns both Stats and an open file handle.
    pub async fn create_file(&self, path: &str, mode: u32) -> Result<(Stats, BoxedFile)> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = match components.len() {
            1 => "/".to_string(),
            _ => format!("/{}", components[..components.len() - 1].join("/")),
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Prepare statements before starting the transaction
        let mut inode_stmt = conn
            .prepare_cached(
                "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime)
                 VALUES (?, 1, 0, 0, 0, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let mut dentry_stmt = conn
            .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
            .await?;

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let file_mode = S_IFREG | (mode & 0o7777);

        let row = inode_stmt
            .query_row((file_mode as i64, now, now, now))
            .await?;

        let ino = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?;

        dentry_stmt
            .execute((name.as_str(), parent_ino, ino))
            .await?;

        conn.prepare_cached("COMMIT")
            .await?
            .execute(())
            .await?;

        self.dentry_cache.insert(parent_ino, name, ino);

        let stats = Stats {
            ino,
            mode: file_mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
        };

        let file: BoxedFile = Arc::new(AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
        });

        Ok((stats, file))
    }

    /// Read data from a file
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.pool.get_conn().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = conn
            .query(
                "SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index",
                (ino,),
            )
            .await?;

        let mut data = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Ok(Value::Blob(chunk)) = row.get_value(0) {
                data.extend_from_slice(&chunk);
            }
        }

        Ok(Some(data))
    }

    /// Reads from a file at a given offset.
    ///
    /// Similar to POSIX `pread`, this reads up to `size` bytes from the file
    /// starting at `offset`, without modifying any file cursor.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    pub async fn pread(&self, path: &str, offset: u64, size: u64) -> Result<Option<Vec<u8>>> {
        let conn = self.pool.get_conn().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        // Calculate which chunks we need
        let chunk_size = self.chunk_size as u64;
        let start_chunk = offset / chunk_size;
        let end_chunk = (offset + size).saturating_sub(1) / chunk_size;

        let mut rows = conn
            .query(
                "SELECT chunk_index, data FROM fs_data WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ? ORDER BY chunk_index",
                (ino, start_chunk as i64, end_chunk as i64),
            )
            .await?;

        let mut result = Vec::with_capacity(size as usize);
        let start_offset_in_chunk = (offset % chunk_size) as usize;

        while let Some(row) = rows.next().await? {
            if let Ok(Value::Blob(chunk_data)) = row.get_value(1) {
                let skip = if result.is_empty() {
                    start_offset_in_chunk
                } else {
                    0
                };
                if skip >= chunk_data.len() {
                    continue;
                }
                let remaining = size as usize - result.len();
                let take = std::cmp::min(chunk_data.len() - skip, remaining);
                result.extend_from_slice(&chunk_data[skip..skip + take]);
            }
        }

        Ok(Some(result))
    }

    /// Writes to a file at a given offset.
    ///
    /// Similar to POSIX `pwrite`, this writes `data` to the file starting at
    /// `offset`, without modifying any file cursor.
    ///
    /// If the offset is beyond the current file size, the file is extended with zeros.
    /// If the file does not exist, it will be created.
    pub async fn pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let result: Result<()> = async {
            // Get or create the inode
            let (ino, current_size) = if let Some(ino) = self.resolve_path_with_conn(&conn, &path).await? {
                // Get current file size
                let mut stmt = conn
                    .prepare_cached("SELECT size FROM fs_inode WHERE ino = ?")
                    .await?;
                let mut rows = stmt.query((ino,)).await?;
                let size = if let Some(row) = rows.next().await? {
                    row.get_value(0)
                        .ok()
                        .and_then(|v| v.as_integer().copied())
                        .unwrap_or(0) as u64
                } else {
                    0
                };
                (ino, size)
            } else {
                // Create new inode
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
                let mut stmt = conn
                    .prepare(
                        "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                        VALUES (?, 0, 0, 0, ?, ?, ?) RETURNING ino",
                    )
                    .await?;
                let row = stmt
                    .query_row((DEFAULT_FILE_MODE as i64, now, now, now))
                    .await?;

                let ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?;

                // Create directory entry
                let mut stmt = conn
                    .prepare_cached(
                        "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                    )
                    .await?;
                stmt.execute((name.as_str(), parent_ino, ino)).await?;

                // Increment link count
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;

                (ino, 0)
            };

            // Handle empty writes - just update mtime
            if data.is_empty() {
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
                conn.prepare_cached("UPDATE fs_inode SET mtime = ? WHERE ino = ?")
                    .await?
                    .execute((now, ino))
                    .await?;
                return Ok(());
            }

            let chunk_size = self.chunk_size as u64;
            let write_end = offset + data.len() as u64;

            // Calculate affected chunk range
            let start_chunk = offset / chunk_size;
            let end_chunk = (write_end - 1) / chunk_size;

            // Process each affected chunk
            for chunk_idx in start_chunk..=end_chunk {
                let chunk_start = chunk_idx * chunk_size;

                // Calculate what part of data goes into this chunk
                let data_start = if offset > chunk_start {
                    (offset - chunk_start) as usize
                } else {
                    0
                };
                let data_end =
                    std::cmp::min(chunk_size as usize, (write_end - chunk_start) as usize);

                // Calculate what part of data to copy
                let src_start = if chunk_start > offset {
                    (chunk_start - offset) as usize
                } else {
                    0
                };
                let src_end = std::cmp::min(data.len(), src_start + (data_end - data_start));

                // Read existing chunk if we need to preserve some data
                let needs_read = data_start > 0 || data_end < chunk_size as usize;
                let mut chunk_data = if needs_read {
                    let mut rows = conn
                        .query(
                            "SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?",
                            (ino, chunk_idx as i64),
                        )
                        .await?;
                    if let Some(row) = rows.next().await? {
                        if let Ok(Value::Blob(data)) = row.get_value(0) {
                            let mut v = data.clone();
                            v.resize(chunk_size as usize, 0);
                            v
                        } else {
                            vec![0u8; chunk_size as usize]
                        }
                    } else {
                        vec![0u8; chunk_size as usize]
                    }
                } else {
                    vec![0u8; chunk_size as usize]
                };

                // Copy the new data into the chunk
                chunk_data[data_start..data_end].copy_from_slice(&data[src_start..src_end]);

                // Trim trailing zeros for the last chunk
                let actual_len = if chunk_idx == end_chunk {
                    let file_end_in_chunk = (write_end - chunk_start) as usize;
                    let old_end_in_chunk = if current_size > chunk_start {
                        std::cmp::min((current_size - chunk_start) as usize, chunk_size as usize)
                    } else {
                        0
                    };
                    std::cmp::max(file_end_in_chunk, old_end_in_chunk)
                } else {
                    chunk_size as usize
                };

                // Write the chunk - delete existing then insert
                conn.execute(
                    "DELETE FROM fs_data WHERE ino = ? AND chunk_index = ?",
                    (ino, chunk_idx as i64),
                )
                .await?;
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                    (ino, chunk_idx as i64, &chunk_data[..actual_len]),
                )
                .await?;
            }

            // Update size and mtime
            let new_size = std::cmp::max(current_size, write_end);
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?")
                .await?;
            stmt.execute((new_size as i64, now, ino)).await?;

            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                conn.prepare_cached("COMMIT")
                    .await?
                    .execute(())
                    .await?;
                Ok(())
            }
            Err(e) => {
                let _ = conn
                    .prepare_cached("ROLLBACK")
                    .await?
                    .execute(())
                    .await;
                Err(e)
            }
        }
    }

    /// Truncate a file to a specific size.
    ///
    /// This operates directly on chunks without loading the entire file into memory:
    /// - Shrinking: deletes chunks beyond new size, truncates the last chunk if needed
    /// - Extending: pads with zeros up to the new size
    pub async fn truncate(&self, path: &str, new_size: u64) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let ino = self.resolve_path_with_conn(&conn, &path).await?.ok_or(FsError::NotFound)?;

        // Get current size
        let mut stmt = conn
            .prepare_cached("SELECT size FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;
        let current_size = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        let chunk_size = self.chunk_size as u64;

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let result: Result<()> = async {
            if new_size == 0 {
                // Special case: truncate to zero - just delete all chunks
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;
            } else if new_size < current_size {
                // Shrinking: delete excess chunks and truncate last chunk if needed
                let last_chunk_idx = (new_size - 1) / chunk_size;

                // Delete all chunks beyond the last one we need
                conn.execute(
                    "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
                    (ino, last_chunk_idx as i64),
                )
                .await?;

                // Calculate where in the last chunk the file should end
                let end_in_last_chunk = ((new_size - 1) % chunk_size) + 1;

                // If the last chunk needs to be truncated (not a full chunk),
                // read it, truncate, and rewrite
                if end_in_last_chunk < chunk_size {
                    let mut stmt = conn
                        .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                        .await?;
                    let mut rows = stmt.query((ino, last_chunk_idx as i64)).await?;

                    if let Some(row) = rows.next().await? {
                        if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                            if chunk_data.len() > end_in_last_chunk as usize {
                                let truncated = &chunk_data[..end_in_last_chunk as usize];
                                let mut stmt = conn
                                    .prepare_cached("UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?")
                                    .await?;
                                stmt.execute((truncated, ino, last_chunk_idx as i64)).await?;
                            }
                        }
                    }
                }
            } else if new_size > current_size {
                // Extending: pad last existing chunk and add zero chunks as needed
                let last_existing_chunk = if current_size == 0 {
                    None
                } else {
                    Some((current_size - 1) / chunk_size)
                };
                let last_new_chunk = (new_size - 1) / chunk_size;

                // Pad the last existing chunk with zeros if it's not full
                if let Some(last_idx) = last_existing_chunk {
                    let mut stmt = conn
                        .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                        .await?;
                    let mut rows = stmt.query((ino, last_idx as i64)).await?;

                    if let Some(row) = rows.next().await? {
                        if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                            let current_chunk_len = chunk_data.len();
                            let needed_len = if last_idx == last_new_chunk {
                                // Last existing chunk is also the last new chunk
                                ((new_size - 1) % chunk_size + 1) as usize
                            } else {
                                // Need to fill this chunk completely
                                chunk_size as usize
                            };

                            if needed_len > current_chunk_len {
                                let mut padded = chunk_data.clone();
                                padded.resize(needed_len, 0);
                                let mut stmt = conn
                                    .prepare_cached("UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?")
                                    .await?;
                                stmt.execute((&padded[..], ino, last_idx as i64)).await?;
                            }
                        }
                    }
                }

                // Add new zero-filled chunks if needed
                let start_new_chunk = last_existing_chunk.map(|i| i + 1).unwrap_or(0);
                for chunk_idx in start_new_chunk..=last_new_chunk {
                    let chunk_len = if chunk_idx == last_new_chunk {
                        ((new_size - 1) % chunk_size + 1) as usize
                    } else {
                        chunk_size as usize
                    };
                    let zeros = vec![0u8; chunk_len];
                    conn.execute(
                        "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                        (ino, chunk_idx as i64, &zeros[..]),
                    )
                    .await?;
                }
            }
            // else: new_size == current_size, nothing to do for data

            // Update size and mtime
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?")
                .await?;
            stmt.execute((new_size as i64, now, ino)).await?;

            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                conn.prepare_cached("COMMIT")
                    .await?
                    .execute(())
                    .await?;
                Ok(())
            }
            Err(e) => {
                let _ = conn
                    .prepare_cached("ROLLBACK")
                    .await?
                    .execute(())
                    .await;
                Err(e)
            }
        }
    }

    /// List directory contents
    pub async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let conn = self.pool.get_conn().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = conn
            .query(
                "SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name",
                (ino,),
            )
            .await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
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
            if !name.is_empty() {
                entries.push(name);
            }
        }

        Ok(Some(entries))
    }

    /// List directory contents with full statistics (optimized batch query)
    ///
    /// Returns entries with their stats in a single JOIN query, avoiding N+1 queries.
    pub async fn readdir_plus(&self, path: &str) -> Result<Option<Vec<DirEntry>>> {
        let conn = self.pool.get_conn().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        // Single JOIN query to get all entry names and their stats (including link count)
        let mut rows = conn
            .query(
                "SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime
                 FROM fs_dentry d
                 JOIN fs_inode i ON d.ino = i.ino
                 WHERE d.parent_ino = ?
                 ORDER BY d.name",
                (ino,),
            )
            .await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
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

            if name.is_empty() {
                continue;
            }

            let entry_ino = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let nlink = row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(1) as u32;

            let stats = Stats {
                ino: entry_ino,
                mode: row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                nlink,
                uid: row
                    .get_value(4)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                gid: row
                    .get_value(5)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                size: row
                    .get_value(6)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                atime: row
                    .get_value(7)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                mtime: row
                    .get_value(8)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                ctime: row
                    .get_value(9)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
            };

            entries.push(DirEntry { name, stats });
        }

        Ok(Some(entries))
    }

    /// Create a symbolic link
    pub async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let linkpath = self.normalize_path(linkpath);
        let components = self.split_path(&linkpath);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        // Get parent directory
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if entry already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create inode for symlink
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mode = S_IFLNK | 0o777; // Symlinks typically have 777 permissions
        let size = target.len() as i64;

        let mut stmt = conn
            .prepare(
                "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                 VALUES (?, 0, 0, ?, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let row = stmt.query_row((mode, size, now, now, now)).await?;

        // Get the newly created inode
        let ino = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        // Store symlink target
        conn.execute(
            "INSERT INTO fs_symlink (ino, target) VALUES (?, ?)",
            (ino, target),
        )
        .await?;

        // Create directory entry
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
            (name.as_str(), parent_ino, ino),
        )
        .await?;

        // Increment link count
        conn.execute(
            "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
            (ino,),
        )
        .await?;

        // Populate dentry cache
        self.dentry_cache.insert(parent_ino, name, ino);

        Ok(())
    }

    /// Create a hard link
    ///
    /// Creates a new directory entry `newpath` that refers to the same inode as `oldpath`.
    /// Both paths will share the same file data and metadata (except for the name).
    /// The link count (nlink) of the inode is incremented.
    pub async fn link(&self, oldpath: &str, newpath: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let oldpath = self.normalize_path(oldpath);
        let newpath = self.normalize_path(newpath);
        let components = self.split_path(&newpath);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        // Resolve old path to get its inode
        let ino = self
            .resolve_path_with_conn(&conn, &oldpath)
            .await?
            .ok_or(FsError::NotFound)?;

        // Check if source is a directory (hard links to directories are not allowed)
        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            if (mode & S_IFMT) == super::S_IFDIR {
                return Err(FsError::IsADirectory.into());
            }
        } else {
            return Err(FsError::NotFound.into());
        }

        // Get parent directory of new path
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if new path already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create directory entry pointing to the same inode
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
            (name.as_str(), parent_ino, ino),
        )
        .await?;

        // Increment link count
        conn.execute(
            "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
            (ino,),
        )
        .await?;

        // Populate dentry cache
        self.dentry_cache.insert(parent_ino, name, ino);

        Ok(())
    }

    /// Read the target of a symbolic link
    pub async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let conn = self.pool.get_conn().await?;
        self.readlink_with_conn(&conn, path).await
    }

    /// Read the target of a symbolic link using a provided connection
    async fn readlink_with_conn(
        &self,
        conn: &Connection,
        path: &str,
    ) -> Result<Option<String>> {
        let path = self.normalize_path(path);

        let ino = match self.resolve_path_with_conn(conn, &path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        // Check if it's a symlink by querying the inode
        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            // Check if it's a symlink
            if (mode & S_IFMT) != S_IFLNK {
                return Err(FsError::NotASymlink.into());
            }
        } else {
            return Ok(None);
        }

        // Read target from fs_symlink table
        let mut rows = conn
            .query("SELECT target FROM fs_symlink WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let target = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => Some(s.to_string()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("Invalid symlink target"))?;
            Ok(Some(target))
        } else {
            Ok(None)
        }
    }

    /// Remove a file or empty directory
    pub async fn remove(&self, path: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let ino = self
            .resolve_path_with_conn(&conn, &path)
            .await?
            .ok_or(FsError::NotFound)?;

        if ino == ROOT_INO {
            return Err(FsError::RootOperation.into());
        }

        // Check if directory is empty
        let mut stmt = conn
            .prepare_cached("SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let count = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            if count > 0 {
                return Err(FsError::NotEmpty.into());
            }
        }

        // Get parent directory and name
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Delete the specific directory entry (not all entries pointing to this inode)
        let mut stmt = conn
            .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
            .await?;
        stmt.execute((parent_ino, name.as_str())).await?;

        // Invalidate cache for this entry
        self.dentry_cache.remove(parent_ino, name);

        // Decrement link count
        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
            .await?;
        stmt.execute((ino,)).await?;

        // Check if this was the last link to the inode
        let link_count = self.get_link_count(&conn, ino).await?;
        if link_count == 0 {
            // Manually handle cascading deletes since we don't use foreign keys
            // Delete data blocks
            let mut stmt = conn
                .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;

            // Delete symlink if exists
            let mut stmt = conn
                .prepare_cached("DELETE FROM fs_symlink WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;

            // Delete inode
            let mut stmt = conn
                .prepare_cached("DELETE FROM fs_inode WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;
        }

        Ok(())
    }

    /// Change file mode/permissions.
    ///
    /// Only modifies the permission bits (lower 12 bits), preserving the file type.
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let path = self.normalize_path(path);

        let ino = self
            .resolve_path_with_conn(&conn, &path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Get current mode to preserve file type bits
        let mut stmt = conn
            .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        let current_mode = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32
        } else {
            return Err(FsError::NotFound.into());
        };

        // Preserve file type bits (upper bits), replace permission bits (lower 12 bits)
        let new_mode = (current_mode & S_IFMT) | (mode & 0o7777);

        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET mode = ? WHERE ino = ?")
            .await?;
        stmt.execute((new_mode as i64, ino)).await?;

        Ok(())
    }

    /// Rename/move a file or directory.
    ///
    /// This operation is atomic - either all changes succeed or none do.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let from_path = self.normalize_path(from);
        let to_path = self.normalize_path(to);

        // Cannot rename root
        if from_path == "/" {
            return Err(FsError::RootOperation.into());
        }

        // Get source inode
        let src_ino = self
            .resolve_path_with_conn(&conn, &from_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Get source stats to check if it's a directory
        let src_stats = self.stat_with_conn(&conn, &from_path).await?.ok_or(FsError::NotFound)?;

        // Prevent renaming a directory into its own subtree (would create a cycle)
        if src_stats.is_directory() {
            let from_prefix = format!("{}/", from_path);
            if to_path.starts_with(&from_prefix) || to_path == from_path {
                return Err(FsError::InvalidRename.into());
            }
        }

        // Parse source path to get parent and name
        let from_components = self.split_path(&from_path);
        let src_name = from_components.last().ok_or(FsError::InvalidPath)?;
        let src_parent_path = if from_components.len() == 1 {
            "/".to_string()
        } else {
            format!(
                "/{}",
                from_components[..from_components.len() - 1].join("/")
            )
        };
        let src_parent_ino = self
            .resolve_path_with_conn(&conn, &src_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Parse destination path to get parent and name
        let to_components = self.split_path(&to_path);
        if to_components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let dst_name = to_components.last().unwrap();
        let dst_parent_path = if to_components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", to_components[..to_components.len() - 1].join("/"))
        };
        let dst_parent_ino = self
            .resolve_path_with_conn(&conn, &dst_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Clone strings for use inside the transaction closure
        let src_name = src_name.clone();
        let dst_name = dst_name.clone();

        conn.prepare_cached("BEGIN IMMEDIATE")
            .await?
            .execute(())
            .await?;

        let result: Result<()> = async {
            // Check if destination exists (inside transaction for atomicity)
            if let Some(dst_ino) = self.resolve_path_with_conn(&conn, &to_path).await? {
                let dst_stats = self.stat_with_conn(&conn, &to_path).await?.ok_or(FsError::NotFound)?;

                // Can't replace directory with non-directory
                if dst_stats.is_directory() && !src_stats.is_directory() {
                    return Err(FsError::IsADirectory.into());
                }

                // Can't replace non-directory with directory
                if !dst_stats.is_directory() && src_stats.is_directory() {
                    return Err(FsError::NotADirectory.into());
                }

                // If destination is directory, it must be empty
                if dst_stats.is_directory() {
                    let mut stmt = conn
                        .prepare_cached("SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?")
                        .await?;
                    let mut rows = stmt.query((dst_ino,)).await?;

                    if let Some(row) = rows.next().await? {
                        let count = row
                            .get_value(0)
                            .ok()
                            .and_then(|v| v.as_integer().copied())
                            .unwrap_or(0);
                        if count > 0 {
                            return Err(FsError::NotEmpty.into());
                        }
                    }
                }

                // Remove destination entry
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
                    .await?;
                stmt.execute((dst_parent_ino, dst_name.as_str())).await?;

                // Decrement link count
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
                    .await?;
                stmt.execute((dst_ino,)).await?;

                // Clean up destination inode if no more links
                let link_count = self.get_link_count(&conn, dst_ino).await?;
                if link_count == 0 {
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_symlink WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_inode WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
                }
            }

            // Update the dentry: change parent and/or name
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?",
                )
                .await?;
            stmt.execute((
                dst_parent_ino,
                dst_name.as_str(),
                src_parent_ino,
                src_name.as_str(),
            ))
            .await?;

            // Update ctime of the inode
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET ctime = ? WHERE ino = ?")
                .await?;
            stmt.execute((now, src_ino)).await?;

            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                conn.prepare_cached("COMMIT")
                    .await?
                    .execute(())
                    .await?;

                // Invalidate cache for source and destination
                self.dentry_cache.remove(src_parent_ino, &src_name);
                self.dentry_cache.remove(dst_parent_ino, &dst_name);

                // Add new entry to cache (source inode is now at destination)
                self.dentry_cache.insert(dst_parent_ino, &dst_name, src_ino);

                Ok(())
            }
            Err(e) => {
                let _ = conn
                    .prepare_cached("ROLLBACK")
                    .await?
                    .execute(())
                    .await;
                Err(e)
            }
        }
    }

    /// Get filesystem statistics
    ///
    /// Returns the total number of inodes and bytes used by file contents.
    pub async fn statfs(&self) -> Result<FilesystemStats> {
        let conn = self.pool.get_conn().await?;
        // Count total inodes
        let mut stmt = conn
            .prepare_cached("SELECT COUNT(*) FROM fs_inode")
            .await?;
        let mut rows = stmt.query(()).await?;

        let inodes = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        // Sum total bytes used (from file sizes in inodes)
        let mut stmt = conn
            .prepare_cached("SELECT COALESCE(SUM(size), 0) FROM fs_inode")
            .await?;
        let mut rows = stmt.query(()).await?;

        let bytes_used = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        Ok(FilesystemStats { inodes, bytes_used })
    }

    /// Synchronize file data to persistent storage
    ///
    /// Temporarily enables FULL synchronous mode, runs a transaction to force
    /// a checkpoint, then restores OFF mode. This ensures durability while
    /// maintaining high performance for normal operations.
    ///
    /// Note: The path parameter is ignored since all data is in a single database.
    pub async fn fsync(&self, _path: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        conn.prepare_cached("PRAGMA synchronous = FULL")
            .await?
            .execute(())
            .await?;
        conn.prepare_cached("BEGIN").await?.execute(()).await?;
        conn.prepare_cached("COMMIT")
            .await?
            .execute(())
            .await?;
        conn.prepare_cached("PRAGMA synchronous = OFF")
            .await?
            .execute(())
            .await?;
        Ok(())
    }

    /// Open a file and return a file handle.
    ///
    /// The returned handle can be used for efficient read/write/fsync operations
    /// without requiring path lookups on each operation.
    pub async fn open(&self, path: &str) -> Result<BoxedFile> {
        let path = self.normalize_path(path);
        let ino = self.resolve_path(&path).await?.ok_or(FsError::NotFound)?;

        Ok(Arc::new(AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
        }))
    }

    /// Get the number of chunks for a given inode (for testing)
    #[cfg(test)]
    async fn get_chunk_count(&self, ino: i64) -> Result<i64> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0))
        } else {
            Ok(0)
        }
    }
}

#[async_trait]
impl FileSystem for AgentFS {
    async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        AgentFS::stat(self, path).await
    }

    async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        AgentFS::lstat(self, path).await
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        AgentFS::read_file(self, path).await
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        AgentFS::write_file(self, path, data).await
    }

    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        AgentFS::readdir(self, path).await
    }

    async fn readdir_plus(&self, path: &str) -> Result<Option<Vec<DirEntry>>> {
        AgentFS::readdir_plus(self, path).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        AgentFS::mkdir(self, path).await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        AgentFS::remove(self, path).await
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        AgentFS::chmod(self, path, mode).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        AgentFS::rename(self, from, to).await
    }

    async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        AgentFS::symlink(self, target, linkpath).await
    }

    async fn link(&self, oldpath: &str, newpath: &str) -> Result<()> {
        AgentFS::link(self, oldpath, newpath).await
    }

    async fn readlink(&self, path: &str) -> Result<Option<String>> {
        AgentFS::readlink(self, path).await
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        AgentFS::statfs(self).await
    }

    async fn open(&self, path: &str) -> Result<BoxedFile> {
        AgentFS::open(self, path).await
    }

    async fn create_file(&self, path: &str, mode: u32) -> Result<(Stats, BoxedFile)> {
        AgentFS::create_file(self, path, mode).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn create_test_fs() -> Result<(AgentFS, tempfile::TempDir)> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.db");
        let fs = AgentFS::new(db_path.to_str().unwrap()).await?;
        Ok((fs, dir))
    }

    // ==================== Chunk Size Boundary Tests ====================

    #[tokio::test]
    async fn test_file_smaller_than_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file smaller than chunk_size (100 bytes)
        let data = vec![0u8; 100];
        fs.write_file("/small.txt", &data).await?;

        // Read it back
        let read_data = fs.read_file("/small.txt").await?.unwrap();
        assert_eq!(read_data.len(), 100);
        assert_eq!(read_data, data);

        // Verify only 1 chunk was created
        let ino = fs.resolve_path("/small.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_exactly_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write exactly chunk_size bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..chunk_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/exact.txt", &data).await?;

        // Read it back
        let read_data = fs.read_file("/exact.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size);
        assert_eq!(read_data, data);

        // Verify only 1 chunk was created
        let ino = fs.resolve_path("/exact.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_one_byte_over_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write chunk_size + 1 bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..=chunk_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/overflow.txt", &data).await?;

        // Read it back
        let read_data = fs.read_file("/overflow.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size + 1);
        assert_eq!(read_data, data);

        // Verify 2 chunks were created
        let ino = fs.resolve_path("/overflow.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_spanning_multiple_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write ~2.5 chunks worth of data
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 2 + chunk_size / 2;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/multi.txt", &data).await?;

        // Read it back
        let read_data = fs.read_file("/multi.txt").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify 3 chunks were created
        let ino = fs.resolve_path("/multi.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    // ==================== Data Integrity Tests ====================

    #[tokio::test]
    async fn test_roundtrip_byte_for_byte() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create data that spans chunk boundaries with identifiable patterns
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 3 + 123; // Odd size spanning 4 chunks

        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/roundtrip.bin", &data).await?;

        let read_data = fs.read_file("/roundtrip.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data, "Data mismatch after roundtrip");

        Ok(())
    }

    #[tokio::test]
    async fn test_binary_data_with_null_bytes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create data with null bytes at chunk boundaries
        let mut data = vec![0u8; chunk_size * 2 + 100];
        // Put nulls at the chunk boundary
        data[chunk_size - 1] = 0;
        data[chunk_size] = 0;
        data[chunk_size + 1] = 0;
        // Put some non-null bytes around
        data[chunk_size - 2] = 0xFF;
        data[chunk_size + 2] = 0xFF;

        fs.write_file("/nulls.bin", &data).await?;
        let read_data = fs.read_file("/nulls.bin").await?.unwrap();

        assert_eq!(read_data, data, "Null bytes at chunk boundary corrupted");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create sequential bytes spanning multiple chunks
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/sequential.bin", &data).await?;

        let read_data = fs.read_file("/sequential.bin").await?.unwrap();

        // Verify every byte is in the correct position
        for (i, (&expected, &actual)) in data.iter().zip(read_data.iter()).enumerate() {
            assert_eq!(
                expected, actual,
                "Byte mismatch at position {}: expected {}, got {}",
                i, expected, actual
            );
        }

        Ok(())
    }

    // ==================== Edge Case Tests ====================

    #[tokio::test]
    async fn test_empty_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write empty file
        fs.write_file("/empty.txt", &[]).await?;

        // Read it back
        let read_data = fs.read_file("/empty.txt").await?.unwrap();
        assert!(read_data.is_empty());

        // Verify 0 chunks were created
        let ino = fs.resolve_path("/empty.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 0);

        // Verify size is 0
        let stats = fs.stat("/empty.txt").await?.unwrap();
        assert_eq!(stats.size, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial large file (3 chunks)
        let initial_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        fs.write_file("/overwrite.txt", &initial_data).await?;

        let ino = fs.resolve_path("/overwrite.txt").await?.unwrap();
        let initial_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(initial_chunk_count, 3);

        // Overwrite with smaller file (1 chunk)
        let new_data = vec![42u8; 100];
        fs.write_file("/overwrite.txt", &new_data).await?;

        // Verify old chunks are gone and new data is correct
        let read_data = fs.read_file("/overwrite.txt").await?.unwrap();
        assert_eq!(read_data, new_data);

        let new_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(new_chunk_count, 1);

        // Verify size is updated
        let stats = fs.stat("/overwrite.txt").await?.unwrap();
        assert_eq!(stats.size, 100);

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_with_larger_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial small file (1 chunk)
        let initial_data = vec![1u8; 100];
        fs.write_file("/grow.txt", &initial_data).await?;

        let ino = fs.resolve_path("/grow.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 1);

        // Overwrite with larger file (3 chunks)
        let new_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        fs.write_file("/grow.txt", &new_data).await?;

        // Verify data is correct
        let read_data = fs.read_file("/grow.txt").await?.unwrap();
        assert_eq!(read_data, new_data);
        assert_eq!(fs.get_chunk_count(ino).await?, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_very_large_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write 1MB file
        let data_size = 1024 * 1024;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/large.bin", &data).await?;

        let read_data = fs.read_file("/large.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify correct number of chunks
        let chunk_size = fs.chunk_size();
        let expected_chunks = data_size.div_ceil(chunk_size);
        let ino = fs.resolve_path("/large.bin").await?.unwrap();
        let actual_chunks = fs.get_chunk_count(ino).await? as usize;
        assert_eq!(actual_chunks, expected_chunks);

        Ok(())
    }

    // ==================== Configuration Tests ====================

    #[tokio::test]
    async fn test_default_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert_eq!(fs.chunk_size(), DEFAULT_CHUNK_SIZE);
        assert_eq!(fs.chunk_size(), 4096);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_size_accessor() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        assert!(chunk_size > 0);

        // Write data and verify chunks match expected based on chunk_size
        let data = vec![0u8; chunk_size * 2 + 1];
        fs.write_file("/test.bin", &data).await?;

        let ino = fs.resolve_path("/test.bin").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_config_persistence() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Query fs_config table directly
        let conn = fs.pool.get_conn().await?;
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        let row = rows.next().await?.expect("chunk_size config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("chunk_size should be a text value");

        assert_eq!(value, "4096");

        Ok(())
    }

    // ==================== Schema Tests ====================

    #[tokio::test]
    async fn test_chunk_index_uniqueness() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file to create chunks
        let chunk_size = fs.chunk_size();
        let data = vec![0u8; chunk_size * 2];
        fs.write_file("/unique.txt", &data).await?;

        let ino = fs.resolve_path("/unique.txt").await?.unwrap();

        // Try to insert a duplicate chunk - should fail due to PRIMARY KEY constraint
        let conn = fs.pool.get_conn().await?;
        let result = conn
            .execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, 0, ?)",
                (ino, vec![1u8; 10]),
            )
            .await;

        assert!(result.is_err(), "Duplicate chunk_index should be rejected");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering_in_database() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create 5 chunks with identifiable data
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        fs.write_file("/ordered.bin", &data).await?;

        let ino = fs.resolve_path("/ordered.bin").await?.unwrap();

        // Query chunks in order
        let conn = fs.pool.get_conn().await?;
        let mut rows = conn
            .query(
                "SELECT chunk_index FROM fs_data WHERE ino = ? ORDER BY chunk_index",
                (ino,),
            )
            .await?;

        let mut indices = Vec::new();
        while let Some(row) = rows.next().await? {
            let idx = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(-1);
            indices.push(idx);
        }

        assert_eq!(indices, vec![0, 1, 2, 3, 4]);

        Ok(())
    }

    // ==================== Cleanup Tests ====================

    #[tokio::test]
    async fn test_delete_file_removes_all_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create multi-chunk file
        let data = vec![0u8; chunk_size * 4];
        fs.write_file("/deleteme.txt", &data).await?;

        let ino = fs.resolve_path("/deleteme.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 4);

        // Delete the file
        fs.remove("/deleteme.txt").await?;

        // Verify all chunks are gone
        let conn = fs.pool.get_conn().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
            .await?;

        let count = rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1);

        assert_eq!(count, 0, "All chunks should be deleted");

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_files_different_sizes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Create files of various sizes
        let files = vec![
            ("/tiny.txt", 10),
            ("/small.txt", chunk_size / 2),
            ("/exact.txt", chunk_size),
            ("/medium.txt", chunk_size * 2 + 100),
            ("/large.txt", chunk_size * 5),
        ];

        for (path, size) in &files {
            let data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            fs.write_file(path, &data).await?;
        }

        // Verify each file has correct data and chunk count
        for (path, size) in &files {
            let read_data = fs.read_file(path).await?.unwrap();
            assert_eq!(read_data.len(), *size, "Size mismatch for {}", path);

            let expected_data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            assert_eq!(read_data, expected_data, "Data mismatch for {}", path);

            let expected_chunks = size.div_ceil(chunk_size);
            let ino = fs.resolve_path(path).await?.unwrap();
            let actual_chunks = fs.get_chunk_count(ino).await? as usize;
            assert_eq!(
                actual_chunks, expected_chunks,
                "Chunk count mismatch for {}",
                path
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file with known content
        let data: Vec<u8> = (0..100).collect();
        fs.write_file("/test.txt", &data).await?;

        // Read from the beginning
        let result = fs.pread("/test.txt", 0, 10).await?.unwrap();
        assert_eq!(result, &data[0..10]);

        // Read from the middle
        let result = fs.pread("/test.txt", 50, 20).await?.unwrap();
        assert_eq!(result, &data[50..70]);

        // Read from near the end
        let result = fs.pread("/test.txt", 90, 10).await?.unwrap();
        assert_eq!(result, &data[90..100]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_past_eof() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let data: Vec<u8> = (0..50).collect();
        fs.write_file("/test.txt", &data).await?;

        // Read starting past EOF should return empty
        let result = fs.pread("/test.txt", 100, 10).await?.unwrap();
        assert!(result.is_empty());

        // Read that extends past EOF should return only available data
        let result = fs.pread("/test.txt", 40, 20).await?.unwrap();
        assert_eq!(result, &data[40..50]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_nonexistent_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let result = fs.pread("/nonexistent.txt", 0, 10).await?;
        assert!(result.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create data spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        fs.write_file("/test.txt", &data).await?;

        // Read across chunk boundary
        let start = chunk_size - 10;
        let result = fs.pread("/test.txt", start as u64, 20).await?.unwrap();
        assert_eq!(result, &data[start..start + 20]);

        // Read spanning multiple chunks
        let start = chunk_size / 2;
        let size = chunk_size * 2;
        let result = fs
            .pread("/test.txt", start as u64, size as u64)
            .await?
            .unwrap();
        assert_eq!(result, &data[start..start + size]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write initial data
        let data: Vec<u8> = vec![0; 100];
        fs.write_file("/test.txt", &data).await?;

        // Overwrite in the middle
        fs.pwrite("/test.txt", 50, &[1, 2, 3, 4, 5]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[50..55], &[1, 2, 3, 4, 5]);
        assert_eq!(&result[0..50], &vec![0u8; 50][..]);
        assert_eq!(&result[55..100], &vec![0u8; 45][..]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write initial data
        let data: Vec<u8> = vec![1; 50];
        fs.write_file("/test.txt", &data).await?;

        // Write past EOF - should extend with zeros
        fs.pwrite("/test.txt", 100, &[2, 2, 2, 2, 2]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 105);
        assert_eq!(&result[0..50], &vec![1u8; 50][..]);
        assert_eq!(&result[50..100], &vec![0u8; 50][..]);
        assert_eq!(&result[100..105], &[2, 2, 2, 2, 2]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_creates_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // pwrite to a non-existent file should create it
        fs.pwrite("/new.txt", 0, &[1, 2, 3]).await?;

        let result = fs.read_file("/new.txt").await?.unwrap();
        assert_eq!(result, &[1, 2, 3]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create initial data spanning multiple chunks
        let data: Vec<u8> = vec![0; chunk_size * 3];
        fs.write_file("/test.txt", &data).await?;

        // Write across chunk boundary
        let write_data: Vec<u8> = (0..20).collect();
        let start = chunk_size - 10;
        fs.pwrite("/test.txt", start as u64, &write_data).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(&result[start..start + 20], &write_data[..]);

        // Verify surrounding data is unchanged
        assert_eq!(&result[0..start], &vec![0u8; start][..]);
        assert_eq!(
            &result[start + 20..],
            &vec![0u8; chunk_size * 3 - start - 20][..]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_pwrite_roundtrip() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file
        let initial: Vec<u8> = (0..(chunk_size * 2)).map(|i| (i % 256) as u8).collect();
        fs.write_file("/test.txt", &initial).await?;

        // Write some data at various offsets
        let patches = vec![
            (0u64, vec![0xAAu8; 10]),
            (chunk_size as u64 - 5, vec![0xBB; 10]),
            (chunk_size as u64 * 2 - 1, vec![0xCC; 1]),
        ];

        for (offset, data) in &patches {
            fs.pwrite("/test.txt", *offset, data).await?;
        }

        // Verify with pread
        for (offset, expected) in &patches {
            let result = fs
                .pread("/test.txt", *offset, expected.len() as u64)
                .await?
                .unwrap();
            assert_eq!(&result, expected);
        }

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────
    // Truncate Tests
    // ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_truncate_to_zero() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file with some data
        let data: Vec<u8> = (0..100).collect();
        fs.write_file("/test.txt", &data).await?;

        // Truncate to zero
        fs.truncate("/test.txt", 0).await?;

        // Verify file is empty
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert!(result.is_empty());

        // Verify stat shows size 0
        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_smaller_within_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file smaller than chunk size
        let data: Vec<u8> = (0..100).collect();
        fs.write_file("/test.txt", &data).await?;

        // Truncate to 50 bytes
        fs.truncate("/test.txt", 50).await?;

        // Verify data is truncated correctly
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 50);
        assert_eq!(result, &data[..50]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_across_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        fs.write_file("/test.txt", &data).await?;

        // Truncate to middle of second chunk
        let new_size = chunk_size + chunk_size / 2;
        fs.truncate("/test.txt", new_size as u64).await?;

        // Verify data
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), new_size);
        assert_eq!(result, &data[..new_size]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a small file
        let data: Vec<u8> = (0..50).collect();
        fs.write_file("/test.txt", &data).await?;

        // Extend to 100 bytes
        fs.truncate("/test.txt", 100).await?;

        // Verify size increased
        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 100);

        // Original data should be preserved, rest should be zeros (sparse)
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[..50], &data[..]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_nonexistent_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Truncate non-existent file should fail
        let result = fs.truncate("/nonexistent.txt", 100).await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_at_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        fs.write_file("/test.txt", &data).await?;

        // Truncate exactly at chunk boundary
        fs.truncate("/test.txt", chunk_size as u64).await?;

        // Verify
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), chunk_size);
        assert_eq!(result, &data[..chunk_size]);

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────
    // Rename Tests
    // ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_rename_file_same_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file
        let data = b"hello world";
        fs.write_file("/old.txt", data).await?;

        // Rename it
        fs.rename("/old.txt", "/new.txt").await?;

        // Old path should not exist
        assert!(fs.stat("/old.txt").await?.is_none());

        // New path should exist with same data
        let result = fs.read_file("/new.txt").await?.unwrap();
        assert_eq!(result, data);

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_file_to_different_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create directory and file
        fs.mkdir("/subdir").await?;
        let data = b"test data";
        fs.write_file("/file.txt", data).await?;

        // Move file to subdirectory
        fs.rename("/file.txt", "/subdir/file.txt").await?;

        // Old path should not exist
        assert!(fs.stat("/file.txt").await?.is_none());

        // New path should exist
        let result = fs.read_file("/subdir/file.txt").await?.unwrap();
        assert_eq!(result, data);

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create two files
        fs.write_file("/src.txt", b"source").await?;
        fs.write_file("/dst.txt", b"destination").await?;

        // Rename src to dst (overwrites dst)
        fs.rename("/src.txt", "/dst.txt").await?;

        // Only dst should exist with src's content
        assert!(fs.stat("/src.txt").await?.is_none());
        let result = fs.read_file("/dst.txt").await?.unwrap();
        assert_eq!(result, b"source");

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create directory with a file inside
        fs.mkdir("/olddir").await?;
        fs.write_file("/olddir/file.txt", b"content").await?;

        // Rename directory
        fs.rename("/olddir", "/newdir").await?;

        // Old path should not exist
        assert!(fs.stat("/olddir").await?.is_none());

        // New path should exist and contain the file
        assert!(fs.stat("/newdir").await?.is_some());
        let result = fs.read_file("/newdir/file.txt").await?.unwrap();
        assert_eq!(result, b"content");

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory_into_own_subtree_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create nested directories
        fs.mkdir("/parent").await?;
        fs.mkdir("/parent/child").await?;

        // Try to rename parent into its child - should fail
        let result = fs.rename("/parent", "/parent/child/parent").await;
        assert!(result.is_err());

        // Original structure should be intact
        assert!(fs.stat("/parent").await?.is_some());
        assert!(fs.stat("/parent/child").await?.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Try to rename root - should fail
        let result = fs.rename("/", "/newroot").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_to_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        fs.write_file("/file.txt", b"data").await?;

        // Try to rename to root - should fail
        let result = fs.rename("/file.txt", "/").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_nonexistent_source_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Try to rename non-existent file
        let result = fs.rename("/nonexistent.txt", "/new.txt").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_overwrite_nonempty_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create source directory and target directory with content
        fs.mkdir("/src").await?;
        fs.mkdir("/dst").await?;
        fs.write_file("/dst/file.txt", b"content").await?;

        // Try to rename src to dst (dst is not empty) - should fail
        let result = fs.rename("/src", "/dst").await;
        assert!(result.is_err());

        // Both directories should still exist
        assert!(fs.stat("/src").await?.is_some());
        assert!(fs.stat("/dst").await?.is_some());
        assert!(fs.stat("/dst/file.txt").await?.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_file_to_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file and an empty directory
        fs.write_file("/file.txt", b"data").await?;
        fs.mkdir("/dir").await?;

        // Try to rename file over directory - should fail
        let result = fs.rename("/file.txt", "/dir").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory_to_file_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a directory and a file
        fs.mkdir("/dir").await?;
        fs.write_file("/file.txt", b"data").await?;

        // Try to rename directory over file - should fail
        let result = fs.rename("/dir", "/file.txt").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_updates_ctime() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file
        fs.write_file("/old.txt", b"data").await?;
        let stats_before = fs.stat("/old.txt").await?.unwrap();

        // Small delay to ensure time changes
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Rename it
        fs.rename("/old.txt", "/new.txt").await?;

        // ctime should be updated
        let stats_after = fs.stat("/new.txt").await?.unwrap();
        assert!(stats_after.ctime >= stats_before.ctime);

        Ok(())
    }

    #[tokio::test]
    async fn test_write_file_updates_mode_to_regular_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a target file for the symlink
        fs.write_file("/target.txt", b"target content").await?;

        // Create a symlink
        fs.symlink("/target.txt", "/link.txt").await?;

        // Verify it's a symlink
        let stats = fs.lstat("/link.txt").await?.unwrap();
        assert!(stats.is_symlink(), "Should be a symlink before write_file");

        // Overwrite the symlink with write_file
        fs.write_file("/link.txt", b"new content").await?;

        // Verify it's now a regular file with DEFAULT_FILE_MODE
        let stats = fs.lstat("/link.txt").await?.unwrap();
        assert!(stats.is_file(), "Should be a regular file after write_file");
        assert_eq!(
            stats.mode, DEFAULT_FILE_MODE,
            "Mode should be DEFAULT_FILE_MODE after write_file"
        );

        // Verify the content
        let content = fs.read_file("/link.txt").await?.unwrap();
        assert_eq!(content, b"new content");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_regular_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file with default permissions
        fs.write_file("/test.txt", b"content").await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o644,
            "Default file mode should be 0o644"
        );

        // Change to executable
        fs.chmod("/test.txt", 0o755).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o755,
            "Mode should be 0o755 after chmod"
        );
        assert!(stats.is_file(), "Should still be a regular file");

        // Change to read-only
        fs.chmod("/test.txt", 0o444).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o444,
            "Mode should be 0o444 after chmod"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_preserves_file_type() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a regular file
        fs.write_file("/file.txt", b"content").await?;
        fs.chmod("/file.txt", 0o755).await?;
        let stats = fs.stat("/file.txt").await?.unwrap();
        assert!(stats.is_file(), "Should remain a regular file after chmod");

        // Create a directory
        fs.mkdir("/dir").await?;
        fs.chmod("/dir", 0o700).await?;
        let stats = fs.stat("/dir").await?.unwrap();
        assert!(
            stats.is_directory(),
            "Should remain a directory after chmod"
        );
        assert_eq!(stats.mode & 0o7777, 0o700, "Directory mode should be 0o700");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_nonexistent_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let result = fs.chmod("/nonexistent.txt", 0o755).await;
        assert!(result.is_err(), "chmod on nonexistent file should fail");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_symlink() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create target and symlink
        fs.write_file("/target.txt", b"content").await?;
        fs.symlink("/target.txt", "/link.txt").await?;

        // chmod the symlink path (should work on the symlink inode)
        fs.chmod("/link.txt", 0o755).await?;

        let stats = fs.lstat("/link.txt").await?.unwrap();
        assert!(stats.is_symlink(), "Should still be a symlink");

        Ok(())
    }
}
