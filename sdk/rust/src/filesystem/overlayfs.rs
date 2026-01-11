use anyhow::Result;
use async_trait::async_trait;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use turso::{Connection, Value};

use super::{
    agentfs::AgentFS, BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats,
};

/// A path-component trie for efficient whiteout lookups.
///
/// This replaces N database queries per is_whiteout() call with a single
/// in-memory trie traversal. The trie is keyed by path components (split by '/'),
/// making ancestor lookups O(depth) with early termination.
#[derive(Default, Debug)]
struct WhiteoutNode {
    /// Child nodes keyed by path component
    children: HashMap<String, WhiteoutNode>,
    /// True if this exact path is a whiteout
    is_whiteout: bool,
}

/// Thread-safe whiteout cache wrapping the trie
#[derive(Default, Debug)]
pub struct WhiteoutCache {
    root: RwLock<WhiteoutNode>,
}

/// Cache for directories known to exist in the delta layer.
///
/// This avoids repeated `delta.stat()` calls in `ensure_parent_dirs()` when
/// creating many files in the same directory tree (e.g., npm install).
///
/// Uses `BTreeSet` instead of `HashSet` for efficient prefix-based removal:
/// - `remove()` is O(log n + k) where k is the number of children
/// - With `HashSet::retain()`, it was O(n) scanning all entries
#[derive(Default, Debug)]
struct DeltaDirCache {
    /// Set of normalized paths known to exist as directories in delta.
    /// BTreeSet allows efficient range queries for prefix-based child removal.
    dirs: RwLock<BTreeSet<String>>,
}

impl DeltaDirCache {
    fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert("/".to_string()); // Root always exists
        Self {
            dirs: RwLock::new(dirs),
        }
    }

    /// Check if a directory is known to exist in delta
    fn contains(&self, path: &str) -> bool {
        self.dirs.read().unwrap().contains(path)
    }

    /// Mark a directory as existing in delta
    fn insert(&self, path: &str) {
        self.dirs.write().unwrap().insert(path.to_string());
    }

    /// Remove a directory from the cache (called on remove())
    ///
    /// Uses BTreeSet range queries for O(log n + k) complexity where k is
    /// the number of children, instead of O(n) with HashSet::retain().
    fn remove(&self, path: &str) {
        let mut dirs = self.dirs.write().unwrap();
        dirs.remove(path);

        // Remove children using efficient range iteration.
        // BTreeSet is sorted, so all children with prefix "/path/" are contiguous.
        let prefix = format!("{}/", path);

        // Collect children in the range [prefix, prefix_end) where prefix_end
        // is the first string that doesn't start with prefix.
        // Since '0' > '/' in ASCII, "path/0" is after all "path/..." entries.
        let children: Vec<_> = dirs
            .range(prefix.clone()..)
            .take_while(|p| p.starts_with(&prefix))
            .cloned()
            .collect();

        for child in children {
            dirs.remove(&child);
        }
    }
}

impl WhiteoutCache {
    /// Create an empty whiteout cache
    pub fn new() -> Self {
        Self {
            root: RwLock::new(WhiteoutNode::default()),
        }
    }

    /// Check if path or any ancestor is a whiteout - O(depth) with early exit
    pub fn has_whiteout_ancestor(&self, path: &str) -> bool {
        let root = self.root.read().unwrap();
        let mut node = &*root;

        for component in path.split('/').filter(|s| !s.is_empty()) {
            if node.is_whiteout {
                return true; // Ancestor is whiteout, early exit
            }
            match node.children.get(component) {
                Some(child) => node = child,
                None => return false, // Path not in trie, no whiteout
            }
        }
        node.is_whiteout // Check the final node itself
    }

    /// Check if this exact path is a whiteout (not ancestors) - O(depth)
    ///
    /// Unlike `has_whiteout_ancestor`, this only returns true if the exact
    /// path is marked as a whiteout, not if any ancestor is.
    pub fn has_exact_whiteout(&self, path: &str) -> bool {
        let root = self.root.read().unwrap();
        let mut node = &*root;

        for component in path.split('/').filter(|s| !s.is_empty()) {
            match node.children.get(component) {
                Some(child) => node = child,
                None => return false, // Path not in trie
            }
        }
        node.is_whiteout
    }

    /// Insert a whiteout path into the cache
    pub fn insert(&self, path: &str) {
        let mut root = self.root.write().unwrap();
        let mut node = &mut *root;

        for component in path.split('/').filter(|s| !s.is_empty()) {
            node = node.children.entry(component.to_string()).or_default();
        }
        node.is_whiteout = true;
    }

    /// Remove a whiteout path from the cache
    ///
    /// Note: This only unmarks the exact path, not ancestors or children.
    /// It also doesn't prune empty nodes (minor memory overhead, but safe).
    pub fn remove(&self, path: &str) {
        let mut root = self.root.write().unwrap();
        let mut node = &mut *root;

        for component in path.split('/').filter(|s| !s.is_empty()) {
            match node.children.get_mut(component) {
                Some(child) => node = child,
                None => return, // Path not in trie, nothing to remove
            }
        }
        node.is_whiteout = false;
    }

    /// Get all whiteout names that are direct children of a directory path.
    ///
    /// This is used by readdir to filter out deleted entries.
    pub fn get_child_whiteouts(&self, dir_path: &str) -> HashSet<String> {
        let root = self.root.read().unwrap();
        let mut node = &*root;

        // Navigate to the directory node
        for component in dir_path.split('/').filter(|s| !s.is_empty()) {
            match node.children.get(component) {
                Some(child) => node = child,
                None => return HashSet::new(), // Directory not in trie
            }
        }

        // Collect children that are whiteouts
        node.children
            .iter()
            .filter(|(_, child)| child.is_whiteout)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Clear all entries from the cache
    pub fn clear(&self) {
        let mut root = self.root.write().unwrap();
        root.children.clear();
        root.is_whiteout = false;
    }
}

/// A normalized path that is guaranteed to start with /
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NormalizedPath(String);

impl NormalizedPath {
    fn as_str(&self) -> &str {
        &self.0
    }

    /// Create a NormalizedPath from a string that is already known to be normalized.
    /// This skips validation and should only be used for paths derived from
    /// already-normalized paths (e.g., parent directories, path components).
    fn from_normalized(s: String) -> Self {
        NormalizedPath(s)
    }
}

impl AsRef<str> for NormalizedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for NormalizedPath {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for NormalizedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl PartialEq<&str> for NormalizedPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A copy-on-write overlay filesystem.
///
/// Combines a read-only base layer with a writable delta layer (AgentFS).
/// All modifications are written to the delta layer, while reads fall back
/// to the base layer if not found in delta.
///
/// This allows stacking:
/// ```text
/// OverlayFS {
///     base: OverlayFS {
///         base: HostFS { "/project" },
///         delta: AgentFS { "layer1.db" }
///     },
///     delta: AgentFS { "layer2.db" }
/// }
/// ```
pub struct OverlayFS {
    /// Read-only base layer (can be any FileSystem implementation)
    base: Arc<dyn FileSystem>,
    /// Writable delta layer (must be AgentFS for whiteout storage)
    delta: AgentFS,
    /// In-memory cache for whiteout lookups (replaces N db queries with O(depth) trie lookup)
    whiteout_cache: WhiteoutCache,
    /// Cache of directories known to exist in delta (avoids repeated stat() calls)
    delta_dir_cache: DeltaDirCache,
}

/// An open file handle for OverlayFS.
///
/// Tracks which layer(s) the file exists in so that operations like fsync
/// only operate on the relevant layer(s).
pub struct OverlayFile {
    /// File handle for the delta layer (if file exists there).
    delta_file: Option<BoxedFile>,
    /// File handle for the base layer (if file exists there).
    base_file: Option<BoxedFile>,
    /// Reference to delta for copy-on-write operations.
    delta: AgentFS,
    /// The normalized path for copy-on-write operations.
    path: String,
    /// Track if we've done copy-on-write (to avoid re-copying).
    copied_to_delta: std::sync::atomic::AtomicBool,
}

impl OverlayFile {
    /// Ensure parent directories exist in the delta layer.
    ///
    /// This is needed for copy-on-write: when a file exists only in the base layer
    /// and we need to copy it to delta, the parent directories may not exist in delta yet.
    async fn ensure_parent_dirs_in_delta(&self) -> Result<()> {
        let components: Vec<&str> = self.path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current = String::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current = format!("{}/{}", current, component);

            // Check if directory exists in delta
            if self.delta.stat(&current).await?.is_none() {
                // Create it in delta
                self.delta.mkdir(&current).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl File for OverlayFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        // Prefer delta if available
        if let Some(ref file) = self.delta_file {
            return file.pread(offset, size).await;
        }
        // Fall back to base
        if let Some(ref file) = self.base_file {
            return file.pread(offset, size).await;
        }
        // File doesn't exist in either layer
        Ok(Vec::new())
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        // If we already have a delta file handle, use it directly
        if let Some(ref delta_file) = self.delta_file {
            return delta_file.pwrite(offset, data).await;
        }

        // Copy-on-write if needed
        if !self
            .copied_to_delta
            .load(std::sync::atomic::Ordering::Acquire)
        {
            // Ensure parent directories exist in delta before writing
            self.ensure_parent_dirs_in_delta().await?;

            if let Some(ref base_file) = self.base_file {
                let stats = base_file.fstat().await?;
                let base_data = base_file.pread(0, stats.size as u64).await?;
                self.delta.write_file(&self.path, &base_data).await?;
            } else {
                self.delta.write_file(&self.path, &[]).await?;
            }
            self.copied_to_delta
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // Open file in delta and write using the file handle
        let delta_file = self.delta.open(&self.path).await?;
        delta_file.pwrite(offset, data).await
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        // If we already have a delta file handle, use it directly
        if let Some(ref delta_file) = self.delta_file {
            return delta_file.truncate(size).await;
        }

        // Copy-on-write if needed
        if !self
            .copied_to_delta
            .load(std::sync::atomic::Ordering::Acquire)
        {
            // Ensure parent directories exist in delta before writing
            self.ensure_parent_dirs_in_delta().await?;

            if let Some(ref base_file) = self.base_file {
                let stats = base_file.fstat().await?;
                let base_data = base_file.pread(0, stats.size as u64).await?;
                self.delta.write_file(&self.path, &base_data).await?;
            } else {
                self.delta.write_file(&self.path, &[]).await?;
            }
            self.copied_to_delta
                .store(true, std::sync::atomic::Ordering::Release);
        }

        // Open file in delta and truncate using the file handle
        let delta_file = self.delta.open(&self.path).await?;
        delta_file.truncate(size).await
    }

    async fn fsync(&self) -> Result<()> {
        // If we have a delta file handle, use it
        if let Some(ref delta_file) = self.delta_file {
            return delta_file.fsync().await;
        }

        // If we did copy-on-write, open and sync
        if self
            .copied_to_delta
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let delta_file = self.delta.open(&self.path).await?;
            return delta_file.fsync().await;
        }

        // File only exists in base (read-only), nothing to sync
        Ok(())
    }

    async fn fstat(&self) -> Result<Stats> {
        // Prefer delta stats if available
        if let Some(ref file) = self.delta_file {
            return file.fstat().await;
        }
        if let Some(ref file) = self.base_file {
            return file.fstat().await;
        }
        Err(FsError::NotFound.into())
    }
}

impl OverlayFS {
    /// Create a new overlay filesystem
    pub fn new(base: Arc<dyn FileSystem>, delta: AgentFS) -> Self {
        Self {
            base,
            delta,
            whiteout_cache: WhiteoutCache::new(),
            delta_dir_cache: DeltaDirCache::new(),
        }
    }

    /// Load all whiteouts from the database into the in-memory cache.
    ///
    /// This is called during init() to populate the cache. The cache is then
    /// kept in sync via create_whiteout() and remove_whiteout().
    async fn load_whiteouts_into_cache(&self) -> Result<()> {
        let conn = self.delta.get_connection();

        // Query all whiteouts from the database
        let result = conn.prepare_cached("SELECT path FROM fs_whiteout").await;

        // Handle case where table doesn't exist yet (fresh database)
        let mut stmt = match result {
            Ok(stmt) => stmt,
            Err(_) => return Ok(()), // Table doesn't exist, nothing to load
        };

        let mut rows = stmt.query(()).await?;

        while let Some(row) = rows.next().await? {
            if let Ok(Value::Text(path)) = row.get_value(0) {
                self.whiteout_cache.insert(&path);
            }
        }

        Ok(())
    }

    /// Initialize the overlay filesystem schema in a database.
    ///
    /// This is a static method that can be called without creating an OverlayFS
    /// instance, useful for CLI tools that need to initialize an overlay database.
    ///
    /// The `base_path` parameter specifies the actual filesystem path that the
    /// base layer represents. This is stored in the delta database so that
    /// tools like `agentfs diff` can determine what files were modified.
    pub async fn init_schema(conn: &Connection, base_path: &str) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_whiteout (
                path TEXT PRIMARY KEY,
                parent_path TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        // Index on parent_path for efficient child lookups (avoids LIKE regex compilation)
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_fs_whiteout_parent ON fs_whiteout(parent_path)",
            (),
        )
        .await?;
        // Store overlay configuration so tools can identify this as an overlay database
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_overlay_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_overlay_config (key, value) VALUES ('base_path', ?1)",
            [Value::Text(base_path.to_string())],
        )
        .await?;
        // Track origin inodes for copy-up operations (like Linux overlayfs "trusted.overlay.origin")
        // When a file is copied from base to delta, we store the mapping so stat() returns
        // the original base inode, maintaining consistency with kernel inode cache.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_origin (
                delta_ino INTEGER PRIMARY KEY,
                base_ino INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        Ok(())
    }

    /// Initialize the overlay filesystem schema (creates whiteout table)
    ///
    /// This must be called before using the overlay filesystem to ensure
    /// the whiteout tracking table exists in the delta layer.
    ///
    /// The `base_path` parameter specifies the actual filesystem path that the
    /// base layer represents. This is stored in the delta database so that
    /// tools like `agentfs diff` can determine what files were modified.
    pub async fn init(&self, base_path: &str) -> Result<()> {
        Self::init_schema(&self.delta.get_connection(), base_path).await?;
        // Load existing whiteouts into the in-memory cache
        self.load_whiteouts_into_cache().await?;
        Ok(())
    }

    /// Extract the parent path from a normalized path
    fn parent_path(path: &str) -> String {
        if path == "/" {
            return "/".to_string();
        }
        match path.rfind('/') {
            Some(0) => "/".to_string(),
            Some(idx) => path[..idx].to_string(),
            None => "/".to_string(),
        }
    }

    /// Get a reference to the base layer
    pub fn base(&self) -> &Arc<dyn FileSystem> {
        &self.base
    }

    /// Get a reference to the delta layer
    pub fn delta(&self) -> &AgentFS {
        &self.delta
    }

    /// Normalize a path
    fn normalize_path(&self, path: &str) -> NormalizedPath {
        let normalized = path.trim_end_matches('/');
        let s = if normalized.is_empty() {
            "/".to_string()
        } else if !normalized.starts_with('/') {
            format!("/{}", normalized)
        } else {
            normalized.to_string()
        };
        NormalizedPath(s)
    }

    /// Check if a path has a whiteout (is deleted from base)
    ///
    /// This also checks parent directories - if /foo is whiteout,
    /// then /foo/bar is also considered deleted.
    ///
    /// Uses the in-memory cache for O(depth) lookup instead of N database queries.
    fn is_whiteout(&self, path: &NormalizedPath) -> bool {
        self.whiteout_cache.has_whiteout_ancestor(&path.0)
    }

    /// Create a whiteout for a path (marks it as deleted from base)
    async fn create_whiteout(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);
        let parent = Self::parent_path(&normalized);
        let conn = self.delta.get_connection();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO fs_whiteout (path, parent_path, created_at) VALUES (?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET created_at = excluded.created_at",
            )
            .await?;
        stmt.execute((normalized.as_str(), parent.as_str(), now))
            .await?;

        // Update in-memory cache
        self.whiteout_cache.insert(&normalized);
        Ok(())
    }

    /// Remove a whiteout (un-delete a path)
    ///
    /// Fast path: if the path is not a whiteout in the cache, skip the DB operation.
    /// The cache is authoritative (loaded at init, kept in sync with all mutations).
    async fn remove_whiteout(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Fast path: if not in cache, nothing to remove from DB
        if !self.whiteout_cache.has_exact_whiteout(&normalized) {
            return Ok(());
        }

        let conn = self.delta.get_connection();

        let mut stmt = conn
            .prepare_cached("DELETE FROM fs_whiteout WHERE path = ?")
            .await?;
        stmt.execute((normalized.as_str(),)).await?;

        // Update in-memory cache
        self.whiteout_cache.remove(&normalized);
        Ok(())
    }

    /// Get all whiteouts that are direct children of a directory
    ///
    /// Uses the in-memory cache for O(1) lookup instead of database query.
    fn get_child_whiteouts(&self, dir_path: &str) -> HashSet<String> {
        let normalized = self.normalize_path(dir_path);
        self.whiteout_cache.get_child_whiteouts(&normalized)
    }

    /// Ensure parent directories exist in delta layer
    async fn ensure_parent_dirs(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);
        let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

        let mut current = String::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current = format!("{}/{}", current, component);

            // Always remove any whiteout for parent directories
            // (even if the directory exists in delta, it may have been logically deleted)
            self.remove_whiteout(&current).await?;

            // Fast path: if directory is cached as existing in delta, skip expensive stat() calls
            if self.delta_dir_cache.contains(&current) {
                continue;
            }

            // Check if it exists in delta
            let delta_stats = self.delta.stat(&current).await?;

            if let Some(s) = &delta_stats {
                if s.is_directory() {
                    // Directory exists in delta, cache it and continue
                    self.delta_dir_cache.insert(&current);
                    continue;
                } else {
                    // Exists in delta but not a directory
                    return Err(FsError::NotADirectory.into());
                }
            }

            // Not in delta, check base
            let base_stats = self.base.stat(&current).await?;

            match base_stats {
                Some(s) if s.is_directory() => {
                    // Directory exists in base but not delta, create in delta
                    self.delta.mkdir(&current).await?;
                    self.delta_dir_cache.insert(&current);
                }
                Some(_) => {
                    // Exists in base but not a directory
                    return Err(FsError::NotADirectory.into());
                }
                None => {
                    // Doesn't exist anywhere, create it
                    self.delta.mkdir(&current).await?;
                    self.delta_dir_cache.insert(&current);
                }
            }
        }
        Ok(())
    }

    /// Check if a path exists in delta layer
    async fn exists_in_delta(&self, path: &str) -> Result<bool> {
        Ok(self.delta.stat(path).await?.is_some())
    }

    /// Check if a path is traversable (all parent components are directories, not whited out)
    async fn is_path_traversable(&self, path: &NormalizedPath) -> Result<bool> {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Check each parent component
        let mut current = String::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current = format!("{}/{}", current, component);
            let current_normalized = NormalizedPath::from_normalized(current.clone());

            // Check for whiteout
            if self.is_whiteout(&current_normalized) {
                return Ok(false);
            }

            // Check if it's a directory (check delta first, then base)
            let stats = if let Some(s) = self.delta.stat(&current).await? {
                Some(s)
            } else {
                self.base.stat(&current).await?
            };

            match stats {
                Some(s) if s.is_directory() => continue,
                _ => return Ok(false), // Not found or not a directory
            }
        }
        Ok(true)
    }

    /// Store origin inode mapping for a copy-up operation.
    ///
    /// This records that a delta inode originated from a base inode,
    /// so stat() can return the original inode number (like Linux overlayfs).
    async fn add_origin_mapping(&self, delta_ino: i64, base_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection();
        let mut stmt = conn
            .prepare_cached("INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)")
            .await?;
        stmt.execute((delta_ino, base_ino)).await?;
        Ok(())
    }

    /// Get the origin (base) inode for a delta inode, if it was copied up.
    async fn get_origin_inode(&self, delta_ino: i64) -> Result<Option<i64>> {
        let conn = self.delta.get_connection();
        let result = conn
            .prepare_cached("SELECT base_ino FROM fs_origin WHERE delta_ino = ?")
            .await;

        // Handle case where fs_origin table doesn't exist yet (for existing databases)
        let mut stmt = match result {
            Ok(stmt) => stmt,
            Err(_) => return Ok(None),
        };

        let mut rows = stmt.query((delta_ino,)).await?;

        if let Some(row) = rows.next().await? {
            let base_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
            Ok(base_ino)
        } else {
            Ok(None)
        }
    }

    /// Remove the origin mapping for a delta inode.
    ///
    /// Called when a file is deleted from the delta layer to clean up stale mappings.
    async fn remove_origin_mapping(&self, delta_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection();
        let result = conn
            .execute("DELETE FROM fs_origin WHERE delta_ino = ?", (delta_ino,))
            .await;

        // Ignore errors for existing databases without fs_origin table
        match result {
            Ok(_) => Ok(()),
            Err(_) => Ok(()),
        }
    }
}

#[async_trait]
impl FileSystem for OverlayFS {
    async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let normalized = self.normalize_path(path);

        // Check for whiteout first
        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Check delta first for the file content/stats
        let delta_stats = self.delta.stat(&normalized).await?;

        // Check base for the stable inode
        // If file exists in base, use base's inode to maintain consistency
        // (the kernel caches inodes, so changing them after copy-up breaks things)
        if let Some(mut base_stats) = self.base.stat(&normalized).await? {
            // Root directory must have inode 1 for FUSE compatibility
            if normalized == "/" {
                base_stats.ino = 1;
            }

            // If file also exists in delta (was copied up), use delta's metadata
            // but keep the base inode for consistency with kernel cache
            if let Some(mut stats) = delta_stats {
                stats.ino = base_stats.ino;
                return Ok(Some(stats));
            }

            return Ok(Some(base_stats));
        }

        // File only exists in delta (created there, or a hard link to a copied-up file)
        // Check if it has an origin mapping from copy-up
        if let Some(mut stats) = delta_stats {
            if let Some(base_ino) = self.get_origin_inode(stats.ino).await? {
                stats.ino = base_ino;
            }
            return Ok(Some(stats));
        }

        Ok(None)
    }

    async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Check delta first for the file content/stats
        let delta_stats = self.delta.lstat(&normalized).await?;

        // Check base for the stable inode
        // If file exists in base, use base's inode to maintain consistency
        // (the kernel caches inodes, so changing them after copy-up breaks things)
        if let Some(mut base_stats) = self.base.lstat(&normalized).await? {
            // Root directory must have inode 1 for FUSE compatibility
            if normalized == "/" {
                base_stats.ino = 1;
            }

            // If file also exists in delta (was copied up), use delta's metadata
            // but keep the base inode for consistency with kernel cache
            if let Some(mut stats) = delta_stats {
                stats.ino = base_stats.ino;
                return Ok(Some(stats));
            }

            return Ok(Some(base_stats));
        }

        // File only exists in delta (created there, or a hard link to a copied-up file)
        // Check if it has an origin mapping from copy-up
        if let Some(mut stats) = delta_stats {
            if let Some(base_ino) = self.get_origin_inode(stats.ino).await? {
                stats.ino = base_ino;
            }
            return Ok(Some(stats));
        }

        Ok(None)
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let normalized = self.normalize_path(path);

        // Check if path is traversable (all parents are directories)
        if !self.is_path_traversable(&normalized).await? {
            return Ok(None);
        }

        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Check delta first
        if let Some(data) = self.delta.read_file(&normalized).await? {
            return Ok(Some(data));
        }

        // Fall back to base
        self.base.read_file(&normalized).await
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Remove any whiteout for this path
        self.remove_whiteout(&normalized).await?;

        // Invalidate directory cache - writing a file may overwrite a directory
        self.delta_dir_cache.remove(&normalized);

        // Ensure parent directories exist in delta
        self.ensure_parent_dirs(&normalized).await?;

        // Write to delta
        self.delta.write_file(&normalized, data).await
    }

    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let normalized = self.normalize_path(path);

        // Check for whiteout on directory itself
        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Get whiteouts for children
        let child_whiteouts = self.get_child_whiteouts(&normalized);

        let mut entries = HashSet::new();

        // Get entries from delta
        if let Some(delta_entries) = self.delta.readdir(&normalized).await? {
            entries.extend(delta_entries);
        }

        // Get entries from base (if not whiteout)
        if let Some(base_entries) = self.base.readdir(&normalized).await? {
            for entry in base_entries {
                if !child_whiteouts.contains(&entry) {
                    entries.insert(entry);
                }
            }
        }

        // Check if directory exists in either layer
        let delta_exists = self.delta.stat(&normalized).await?.is_some();
        let base_exists = self.base.stat(&normalized).await?.is_some();

        if !delta_exists && !base_exists {
            return Ok(None);
        }

        let mut result: Vec<_> = entries.into_iter().collect();
        result.sort();
        Ok(Some(result))
    }

    async fn readdir_plus(&self, path: &str) -> Result<Option<Vec<DirEntry>>> {
        let normalized = self.normalize_path(path);

        // Check for whiteout on directory itself
        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Get whiteouts for children
        let child_whiteouts = self.get_child_whiteouts(&normalized);

        // Use a HashMap to merge entries, with delta taking precedence
        let mut entries_map = std::collections::HashMap::new();

        // Get entries from base first (to have their inodes available)
        let mut base_inodes = std::collections::HashMap::new();
        if let Some(base_entries) = self.base.readdir_plus(&normalized).await? {
            for entry in base_entries {
                if !child_whiteouts.contains(&entry.name) {
                    base_inodes.insert(entry.name.clone(), entry.stats.ino);
                    entries_map.insert(entry.name.clone(), entry);
                }
            }
        }

        // Get entries from delta (these take precedence for content/metadata)
        // But we need to use the base inode if the file exists in base,
        // to maintain consistency with stat() which returns base inodes
        if let Some(delta_entries) = self.delta.readdir_plus(&normalized).await? {
            for mut entry in delta_entries {
                // If file exists in base, use base inode for consistency with stat()
                if let Some(&base_ino) = base_inodes.get(&entry.name) {
                    entry.stats.ino = base_ino;
                } else {
                    // File only in delta - check for origin mapping (from copy-up)
                    if let Some(origin_ino) = self.get_origin_inode(entry.stats.ino).await? {
                        entry.stats.ino = origin_ino;
                    }
                }
                entries_map.insert(entry.name.clone(), entry);
            }
        }

        // Check if directory exists in either layer
        let delta_exists = self.delta.stat(&normalized).await?.is_some();
        let base_exists = self.base.stat(&normalized).await?.is_some();

        if !delta_exists && !base_exists {
            return Ok(None);
        }

        let mut result: Vec<_> = entries_map.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Check if already exists (in either layer, not whiteout)
        if !self.is_whiteout(&normalized)
            && (self.delta.stat(&normalized).await?.is_some()
                || self.base.stat(&normalized).await?.is_some())
        {
            return Err(FsError::AlreadyExists.into());
        }

        // Remove any whiteout
        self.remove_whiteout(&normalized).await?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(&normalized).await?;

        // Create in delta
        self.delta.mkdir(&normalized).await?;

        // Update cache
        self.delta_dir_cache.insert(&normalized);
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Check if path is a symlink - symlinks don't have children, so skip directory checks
        let is_symlink = if let Some(stats) = self.lstat(&normalized).await? {
            stats.is_symlink()
        } else {
            false
        };

        // Only check for children if not a symlink (directories need to be empty)
        if !is_symlink {
            // Check if directory has children in delta - if so, can't remove
            if let Some(children) = self.delta.readdir(&normalized).await? {
                if !children.is_empty() {
                    return Err(FsError::NotEmpty.into());
                }
            }

            // Check for visible children in base (not whiteout-ed)
            if let Some(base_children) = self.base.readdir(&normalized).await? {
                for child in base_children {
                    let child_path =
                        NormalizedPath::from_normalized(format!("{}/{}", normalized, child));
                    if !self.is_whiteout(&child_path) {
                        return Err(FsError::NotEmpty.into());
                    }
                }
            }
        }

        // Get delta inode before removal to clean up origin mapping
        let delta_ino = self.delta.lstat(&normalized).await?.map(|s| s.ino);

        // Try to remove from delta
        let removed_from_delta = self.delta.remove(&normalized).await.is_ok();

        // Clean up origin mapping if file was removed from delta
        if removed_from_delta {
            if let Some(ino) = delta_ino {
                self.remove_origin_mapping(ino).await?;
            }
            // Invalidate delta directory cache (handles both files and directories)
            self.delta_dir_cache.remove(&normalized);
        }

        // Check if it exists in base (and not already whiteout) - use lstat to not follow symlinks
        let exists_in_base = if self.is_whiteout(&normalized) {
            false
        } else {
            self.base.lstat(&normalized).await?.is_some()
        };

        // If exists in base, create whiteout
        if exists_in_base {
            self.create_whiteout(&normalized).await?;
        } else if !removed_from_delta {
            return Err(FsError::NotFound.into());
        }

        Ok(())
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Check if whited-out
        if self.is_whiteout(&normalized) {
            return Err(FsError::NotFound.into());
        }

        // If file exists in delta, chmod there directly
        if self.exists_in_delta(&normalized).await? {
            return self.delta.chmod(&normalized, mode).await;
        }

        // Check if exists in base
        let base_stats = self.base.lstat(&normalized).await?;
        if let Some(stats) = base_stats {
            // Need to copy to delta first, then chmod
            if stats.is_directory() {
                // For directories, just create in delta and chmod
                self.ensure_parent_dirs(&normalized).await?;
                self.delta.mkdir(&normalized).await?;
                self.delta.chmod(&normalized, mode).await?;
            } else if stats.is_symlink() {
                // For symlinks, copy the symlink to delta
                if let Some(target) = self.base.readlink(&normalized).await? {
                    self.ensure_parent_dirs(&normalized).await?;
                    self.delta.symlink(&target, &normalized).await?;
                    self.delta.chmod(&normalized, mode).await?;
                }
            } else {
                // For regular files, copy content to delta
                if let Some(data) = self.base.read_file(&normalized).await? {
                    self.ensure_parent_dirs(&normalized).await?;
                    self.delta.write_file(&normalized, &data).await?;
                    self.delta.chmod(&normalized, mode).await?;
                }
            }
            Ok(())
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from_normalized = self.normalize_path(from);
        let to_normalized = self.normalize_path(to);

        // Renaming to self is a no-op
        if from_normalized == to_normalized {
            return Ok(());
        }

        // Cannot rename a directory into its own subdirectory
        let from_prefix = format!("{}/", from_normalized);
        if to_normalized.starts_with(&from_prefix) {
            return Err(FsError::InvalidRename.into());
        }

        // If source is in base layer but not delta, copy to delta first
        if !self.exists_in_delta(&from_normalized).await? {
            // Check if exists in base
            let base_stats = self.base.stat(&from_normalized).await?;
            if let Some(stats) = base_stats {
                if stats.is_directory() {
                    // Copy directory structure to delta
                    self.copy_dir_to_delta(&from_normalized).await?;
                } else {
                    // Copy file to delta
                    if let Some(data) = self.base.read_file(&from_normalized).await? {
                        self.ensure_parent_dirs(&from_normalized).await?;
                        self.delta.write_file(&from_normalized, &data).await?;
                    }
                }
            } else {
                return Err(FsError::NotFound.into());
            }
        }

        // Remove whiteout at destination
        self.remove_whiteout(&to_normalized).await?;

        // Ensure parent directories exist at destination
        self.ensure_parent_dirs(&to_normalized).await?;

        // Perform rename in delta
        self.delta.rename(&from_normalized, &to_normalized).await?;

        // Invalidate delta directory cache for both source and destination (and their children)
        self.delta_dir_cache.remove(&from_normalized);
        self.delta_dir_cache.remove(&to_normalized);

        // Create whiteout at source if it existed in base
        if self.base.stat(&from_normalized).await?.is_some() {
            self.create_whiteout(&from_normalized).await?;
        }

        Ok(())
    }

    async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        let normalized = self.normalize_path(linkpath);

        // Remove any whiteout
        self.remove_whiteout(&normalized).await?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(&normalized).await?;

        // Create in delta
        self.delta.symlink(target, &normalized).await
    }

    async fn link(&self, oldpath: &str, newpath: &str) -> Result<()> {
        let old_normalized = self.normalize_path(oldpath);
        let new_normalized = self.normalize_path(newpath);

        // Check if source is whited out
        if self.is_whiteout(&old_normalized) {
            return Err(FsError::NotFound.into());
        }

        // Remove any whiteout at destination
        self.remove_whiteout(&new_normalized).await?;

        // Ensure parent directories exist for the new path
        self.ensure_parent_dirs(&new_normalized).await?;

        // If source is only in base, copy it to delta first
        let in_delta = self.exists_in_delta(&old_normalized).await?;

        if !in_delta {
            // Check if exists in base
            let base_stats = self.base.lstat(&old_normalized).await?;
            if let Some(stats) = base_stats {
                // Hard links to directories are not allowed
                if stats.is_directory() {
                    anyhow::bail!("Cannot create hard link to directory");
                }
                // Copy-up: read from base and write to delta
                if let Some(data) = self.base.read_file(&old_normalized).await? {
                    self.ensure_parent_dirs(&old_normalized).await?;
                    self.delta.write_file(&old_normalized, &data).await?;

                    // Store origin mapping: delta_ino -> base_ino
                    // This ensures stat() returns the original base inode after copy-up
                    if let Some(delta_stats) = self.delta.lstat(&old_normalized).await? {
                        self.add_origin_mapping(delta_stats.ino, stats.ino).await?;
                    }
                } else {
                    return Err(FsError::NotFound.into());
                }
            } else {
                return Err(FsError::NotFound.into());
            }
        }

        // Create hard link in delta
        self.delta.link(&old_normalized, &new_normalized).await
    }

    async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized) {
            return Ok(None);
        }

        // Check delta first
        if let Some(target) = self.delta.readlink(&normalized).await? {
            return Ok(Some(target));
        }

        // Fall back to base
        self.base.readlink(&normalized).await
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        // Return delta stats (base stats would be misleading for overlay)
        self.delta.statfs().await
    }

    async fn open(&self, path: &str) -> Result<BoxedFile> {
        let normalized = self.normalize_path(path);

        // Check for whiteout
        if self.is_whiteout(&normalized) {
            return Err(FsError::NotFound.into());
        }

        // Try to open from delta
        let delta_file = if self.exists_in_delta(&normalized).await? {
            Some(self.delta.open(&normalized).await?)
        } else {
            None
        };

        // Try to open from base
        let base_file = self.base.open(&normalized).await.ok();

        // Must exist in at least one layer
        if delta_file.is_none() && base_file.is_none() {
            return Err(FsError::NotFound.into());
        }

        Ok(Arc::new(OverlayFile {
            delta_file,
            base_file,
            delta: self.delta.clone(),
            path: normalized.0,
            copied_to_delta: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    async fn create_file(&self, path: &str, mode: u32) -> Result<(Stats, BoxedFile)> {
        let normalized = self.normalize_path(path);

        // Remove any whiteout for this path
        self.remove_whiteout(&normalized).await?;

        // Invalidate directory cache
        self.delta_dir_cache.remove(&normalized);

        // Ensure parent directories exist in delta
        self.ensure_parent_dirs(&normalized).await?;

        // Create in delta layer
        self.delta.create_file(&normalized, mode).await
    }
}

impl OverlayFS {
    /// Recursively copy a directory from base to delta
    async fn copy_dir_to_delta(&self, path: &str) -> Result<()> {
        self.delta.mkdir(path).await?;

        if let Some(entries) = self.base.readdir(path).await? {
            for entry in entries {
                let entry_path = if path == "/" {
                    format!("/{}", entry)
                } else {
                    format!("{}/{}", path, entry)
                };

                if let Some(stats) = self.base.stat(&entry_path).await? {
                    if stats.is_directory() {
                        Box::pin(self.copy_dir_to_delta(&entry_path)).await?;
                    } else if stats.is_symlink() {
                        if let Some(target) = self.base.readlink(&entry_path).await? {
                            self.delta.symlink(&target, &entry_path).await?;
                        }
                    } else if let Some(data) = self.base.read_file(&entry_path).await? {
                        self.delta.write_file(&entry_path, &data).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::filesystem::HostFS;
    use tempfile::tempdir;

    async fn create_test_overlay() -> Result<(OverlayFS, tempfile::TempDir, tempfile::TempDir)> {
        // Create base directory with some files
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("base.txt"), b"base content")?;
        std::fs::create_dir(base_dir.path().join("subdir"))?;
        std::fs::write(base_dir.path().join("subdir/nested.txt"), b"nested")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        // Create delta database
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        Ok((overlay, base_dir, delta_dir))
    }

    #[tokio::test]
    async fn test_overlay_read_from_base() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Read file from base layer
        let data = overlay.read_file("/base.txt").await?.unwrap();
        assert_eq!(data, b"base content");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_write_to_delta() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Write new file (goes to delta)
        overlay.write_file("/new.txt", b"new content").await?;

        // Read it back
        let data = overlay.read_file("/new.txt").await?.unwrap();
        assert_eq!(data, b"new content");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Open file handle and modify base file via pwrite (should copy to delta first)
        let file = overlay.open("/base.txt").await?;
        file.pwrite(0, b"modified").await?;

        // Read should show modified content
        let data = overlay.read_file("/base.txt").await?.unwrap();
        assert_eq!(&data[..8], b"modified");

        // Original base file should be unchanged
        let base_data = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(base_data, b"base content");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_whiteout() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // File exists initially
        assert!(overlay.stat("/base.txt").await?.is_some());

        // Delete it
        overlay.remove("/base.txt").await?;

        // File should no longer be visible
        assert!(overlay.stat("/base.txt").await?.is_none());
        assert!(overlay.read_file("/base.txt").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_readdir_merge() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Add file to delta
        overlay.write_file("/delta.txt", b"delta").await?;

        // Readdir should show both base and delta files
        let entries = overlay.readdir("/").await?.unwrap();
        assert!(entries.contains(&"base.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));
        assert!(entries.contains(&"subdir".to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_readdir_with_whiteout() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Delete base file
        overlay.remove("/base.txt").await?;

        // Readdir should not show deleted file
        let entries = overlay.readdir("/").await?.unwrap();
        assert!(!entries.contains(&"base.txt".to_string()));
        assert!(entries.contains(&"subdir".to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_recreate_deleted() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Delete base file
        overlay.remove("/base.txt").await?;
        assert!(overlay.stat("/base.txt").await?.is_none());

        // Recreate it
        overlay.write_file("/base.txt", b"recreated").await?;

        // Should be visible again with new content
        let data = overlay.read_file("/base.txt").await?.unwrap();
        assert_eq!(data, b"recreated");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_chmod_delta_file() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Write a new file to delta
        overlay.write_file("/new.txt", b"content").await?;

        // chmod it
        overlay.chmod("/new.txt", 0o755).await?;

        let stats = overlay.stat("/new.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o777,
            0o755,
            "Mode should be 0o755 after chmod"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_chmod_base_file_copies_to_delta() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Get original mode of base file
        let original_stats = overlay.stat("/base.txt").await?.unwrap();

        // chmod the base file - should copy to delta
        overlay.chmod("/base.txt", 0o755).await?;

        let stats = overlay.stat("/base.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o777,
            0o755,
            "Mode should be 0o755 after chmod"
        );

        // Verify base file is unchanged
        use std::os::unix::fs::PermissionsExt;
        let base_path = base_dir.path().join("base.txt");
        let base_meta = std::fs::metadata(&base_path)?;
        assert_eq!(
            base_meta.permissions().mode() & 0o777,
            original_stats.mode & 0o777,
            "Base file should be unchanged"
        );

        // Verify content is preserved after copy-on-write
        let data = overlay.read_file("/base.txt").await?.unwrap();
        assert_eq!(data, b"base content", "Content should be preserved");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_chmod_whiteout_fails() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Delete the base file (creates whiteout)
        overlay.remove("/base.txt").await?;

        // chmod should fail on whited-out file
        let result = overlay.chmod("/base.txt", 0o755).await;
        assert!(result.is_err(), "chmod on whited-out file should fail");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_chmod_nonexistent_fails() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let result = overlay.chmod("/nonexistent.txt", 0o755).await;
        assert!(result.is_err(), "chmod on nonexistent file should fail");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_chmod_directory() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // chmod the base subdir
        overlay.chmod("/subdir", 0o700).await?;

        let stats = overlay.stat("/subdir").await?.unwrap();
        assert_eq!(stats.mode & 0o777, 0o700, "Directory mode should be 0o700");
        assert!(stats.is_directory(), "Should still be a directory");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_link_base_file_preserves_inode() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Get the original base file's inode
        let base_stats = overlay.stat("/base.txt").await?.unwrap();
        let original_ino = base_stats.ino;

        // Create a hard link to the base file (triggers copy-up)
        overlay.link("/base.txt", "/base_link.txt").await?;

        // Both paths should return the same (original base) inode
        let stats_original = overlay.stat("/base.txt").await?.unwrap();
        let stats_link = overlay.stat("/base_link.txt").await?.unwrap();

        assert_eq!(
            stats_original.ino, original_ino,
            "Original file should keep base inode after copy-up"
        );
        assert_eq!(
            stats_link.ino, original_ino,
            "Hard link should have same inode as original"
        );

        // Verify both files have the same content
        let data_original = overlay.read_file("/base.txt").await?.unwrap();
        let data_link = overlay.read_file("/base_link.txt").await?.unwrap();
        assert_eq!(data_original, data_link);
        assert_eq!(data_original, b"base content");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_link_base_file_lstat_preserves_inode() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Get the original base file's inode via lstat
        let base_stats = overlay.lstat("/base.txt").await?.unwrap();
        let original_ino = base_stats.ino;

        // Create a hard link
        overlay.link("/base.txt", "/base_link.txt").await?;

        // lstat should also return consistent inodes
        let stats_original = overlay.lstat("/base.txt").await?.unwrap();
        let stats_link = overlay.lstat("/base_link.txt").await?.unwrap();

        assert_eq!(
            stats_original.ino, original_ino,
            "lstat on original file should keep base inode"
        );
        assert_eq!(
            stats_link.ino, original_ino,
            "lstat on hard link should have same inode"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_origin_mapping_cleanup_on_delete() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Get the original base inode
        let base_stats = overlay.stat("/base.txt").await?.unwrap();
        let original_ino = base_stats.ino;

        // Create a hard link (triggers copy-up and creates origin mapping)
        overlay.link("/base.txt", "/base_link.txt").await?;

        // Verify the link has the base inode
        let link_stats = overlay.stat("/base_link.txt").await?.unwrap();
        assert_eq!(link_stats.ino, original_ino);

        // Delete the link
        overlay.remove("/base_link.txt").await?;

        // Create a new file in delta - it should get its own inode, not the old mapping
        overlay.write_file("/new_file.txt", b"new content").await?;
        let new_stats = overlay.stat("/new_file.txt").await?.unwrap();

        // The new file's inode should not be affected by stale origin mappings
        // (it should be a fresh delta inode, not the old base inode)
        // Note: We can't easily verify the mapping was cleaned up directly,
        // but we verify the system still works correctly after deletion
        assert!(new_stats.ino > 0, "New file should have a valid inode");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_link_delta_file_no_origin_mapping() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Create a new file directly in delta (no base file)
        overlay
            .write_file("/delta_only.txt", b"delta content")
            .await?;
        let delta_stats = overlay.stat("/delta_only.txt").await?.unwrap();
        let delta_ino = delta_stats.ino;

        // Create a hard link to the delta-only file
        overlay.link("/delta_only.txt", "/delta_link.txt").await?;

        // Both should have the same delta inode (no origin mapping needed)
        let stats_original = overlay.stat("/delta_only.txt").await?.unwrap();
        let stats_link = overlay.stat("/delta_link.txt").await?.unwrap();

        assert_eq!(stats_original.ino, delta_ino);
        assert_eq!(stats_link.ino, delta_ino);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_multiple_links_to_base_file() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Get original base inode
        let base_stats = overlay.stat("/base.txt").await?.unwrap();
        let original_ino = base_stats.ino;

        // Create multiple hard links
        overlay.link("/base.txt", "/link1.txt").await?;
        overlay.link("/base.txt", "/link2.txt").await?;

        // All paths should return the same base inode
        let stats0 = overlay.stat("/base.txt").await?.unwrap();
        let stats1 = overlay.stat("/link1.txt").await?.unwrap();
        let stats2 = overlay.stat("/link2.txt").await?.unwrap();

        assert_eq!(stats0.ino, original_ino);
        assert_eq!(stats1.ino, original_ino);
        assert_eq!(stats2.ino, original_ino);

        Ok(())
    }
}

/// Property-based tests using proptest to verify that overlay operations
/// NEVER modify the underlying host filesystem.
#[cfg(all(test, unix))]
mod prop_tests {
    use super::*;
    use crate::filesystem::HostFS;
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::tempdir;

    /// A snapshot of a directory's contents for comparison.
    /// Uses BTreeMap for deterministic ordering.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DirectorySnapshot {
        /// Map of relative path -> (is_dir, file_contents_or_empty, mode)
        entries: BTreeMap<String, (bool, Vec<u8>, u32)>,
    }

    impl DirectorySnapshot {
        /// Take a snapshot of a directory, capturing all files and their contents.
        fn capture(root: &Path) -> std::io::Result<Self> {
            let mut entries = BTreeMap::new();
            Self::capture_recursive(root, root, &mut entries)?;
            Ok(Self { entries })
        }

        fn capture_recursive(
            root: &Path,
            current: &Path,
            entries: &mut BTreeMap<String, (bool, Vec<u8>, u32)>,
        ) -> std::io::Result<()> {
            for entry in std::fs::read_dir(current)? {
                let entry = entry?;
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let metadata = entry.metadata()?;
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode()
                };

                if metadata.is_dir() {
                    entries.insert(relative.clone(), (true, Vec::new(), mode));
                    Self::capture_recursive(root, &path, entries)?;
                } else if metadata.is_file() {
                    let contents = std::fs::read(&path)?;
                    entries.insert(relative, (false, contents, mode));
                } else if metadata.is_symlink() {
                    // For symlinks, store the target as "contents"
                    let target = std::fs::read_link(&path)?;
                    let target_bytes = target.to_string_lossy().as_bytes().to_vec();
                    entries.insert(relative, (false, target_bytes, mode));
                }
            }
            Ok(())
        }

        /// Compare two snapshots and return a description of differences.
        fn diff(&self, other: &Self) -> Option<String> {
            if self == other {
                return None;
            }

            let mut diffs = Vec::new();

            // Check for missing entries
            for (path, (is_dir, contents, mode)) in &self.entries {
                match other.entries.get(path) {
                    None => diffs.push(format!(
                        "DELETED: {} (was {})",
                        path,
                        if *is_dir { "dir" } else { "file" }
                    )),
                    Some((other_is_dir, other_contents, other_mode)) => {
                        if is_dir != other_is_dir {
                            diffs.push(format!(
                                "TYPE CHANGED: {} (dir={} -> dir={})",
                                path, is_dir, other_is_dir
                            ));
                        }
                        if contents != other_contents {
                            diffs.push(format!(
                                "CONTENT CHANGED: {} ({} bytes -> {} bytes)",
                                path,
                                contents.len(),
                                other_contents.len()
                            ));
                        }
                        if mode != other_mode {
                            diffs.push(format!(
                                "MODE CHANGED: {} ({:o} -> {:o})",
                                path, mode, other_mode
                            ));
                        }
                    }
                }
            }

            // Check for new entries
            for path in other.entries.keys() {
                if !self.entries.contains_key(path) {
                    diffs.push(format!("CREATED: {}", path));
                }
            }

            if diffs.is_empty() {
                None
            } else {
                Some(diffs.join("\n"))
            }
        }
    }

    /// Filesystem operations that can be performed through the overlay.
    #[derive(Debug, Clone)]
    enum FsOperation {
        /// Write a new file or overwrite existing
        WriteFile { path: String, contents: Vec<u8> },
        /// Partial write at offset
        PWrite {
            path: String,
            offset: u64,
            data: Vec<u8>,
        },
        /// Truncate a file
        Truncate { path: String, size: u64 },
        /// Create a directory
        Mkdir { path: String },
        /// Remove a file or directory
        Remove { path: String },
        /// Rename a file or directory
        Rename { from: String, to: String },
        /// Create a symlink
        Symlink { target: String, linkpath: String },
    }

    impl FsOperation {
        /// Execute this operation on an overlay filesystem.
        /// Returns Ok(()) on success, or the error (which we ignore for fuzzing).
        async fn execute(&self, overlay: &OverlayFS) -> Result<()> {
            match self {
                FsOperation::WriteFile { path, contents } => {
                    overlay.write_file(path, contents).await
                }
                FsOperation::PWrite { path, offset, data } => {
                    // Create file if it doesn't exist, then use file handle
                    if overlay.stat(path).await?.is_none() {
                        overlay.write_file(path, &[]).await?;
                    }
                    let file = overlay.open(path).await?;
                    file.pwrite(*offset, data).await
                }
                FsOperation::Truncate { path, size } => {
                    let file = overlay.open(path).await?;
                    file.truncate(*size).await
                }
                FsOperation::Mkdir { path } => overlay.mkdir(path).await,
                FsOperation::Remove { path } => overlay.remove(path).await,
                FsOperation::Rename { from, to } => overlay.rename(from, to).await,
                FsOperation::Symlink { target, linkpath } => {
                    overlay.symlink(target, linkpath).await
                }
            }
        }
    }

    /// Fixed set of paths including ones that target base files
    const PATHS: &[&str] = &[
        // Existing base paths (important for testing copy-on-write)
        "/base.txt",
        "/another.txt",
        "/subdir",
        "/subdir/nested.txt",
        "/subdir/deep.bin",
        "/deep/nested/path/file.txt",
        "/deep/nested/path",
        "/deep/nested",
        "/deep",
        // New paths
        "/newfile.txt",
        "/newdir",
        "/foo",
        "/bar",
        "/test.txt",
        "/subdir/new.txt",
        "/deep/new.txt",
        "/a/b/c",
    ];

    /// Strategy for generating filesystem paths using fixed set.
    fn path_strategy() -> impl Strategy<Value = String> {
        prop::sample::select(PATHS).prop_map(|s| s.to_string())
    }

    /// Strategy for generating file contents.
    fn contents_strategy() -> impl Strategy<Value = Vec<u8>> {
        (0u8..6).prop_map(|i| match i {
            0 => Vec::new(),
            1 => b"test content".to_vec(),
            2 => b"modified data".to_vec(),
            3 => b"new content here".to_vec(),
            4 => b"\x00\x01\x02\x03\x04".to_vec(),
            _ => vec![b'a'; 100],
        })
    }

    /// Strategy for generating a single filesystem operation.
    fn operation_strategy() -> impl Strategy<Value = FsOperation> {
        // Use index-based selection to avoid deep prop_oneof nesting
        (
            0u8..7,
            path_strategy(),
            path_strategy(),
            contents_strategy(),
            0u64..1000,
        )
            .prop_map(|(op_type, path1, path2, contents, offset)| match op_type {
                0 => FsOperation::WriteFile {
                    path: path1,
                    contents,
                },
                1 => FsOperation::PWrite {
                    path: path1,
                    offset,
                    data: contents,
                },
                2 => FsOperation::Truncate {
                    path: path1,
                    size: offset,
                },
                3 => FsOperation::Mkdir { path: path1 },
                4 => FsOperation::Remove { path: path1 },
                5 => FsOperation::Rename {
                    from: path1,
                    to: path2,
                },
                _ => FsOperation::Symlink {
                    target: path1,
                    linkpath: path2,
                },
            })
    }

    /// Strategy for generating a sequence of operations.
    fn operations_strategy() -> impl Strategy<Value = Vec<FsOperation>> {
        prop_vec(operation_strategy(), 1..30)
    }

    /// Create a test overlay with a known base directory structure.
    async fn create_fuzz_overlay() -> Result<(OverlayFS, tempfile::TempDir, tempfile::TempDir)> {
        // Create base directory with various files and structures
        let base_dir = tempdir()?;

        // Create some files
        std::fs::write(base_dir.path().join("base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("another.txt"), b"another file")?;

        // Create nested directories
        std::fs::create_dir(base_dir.path().join("subdir"))?;
        std::fs::write(base_dir.path().join("subdir/nested.txt"), b"nested content")?;
        std::fs::write(base_dir.path().join("subdir/deep.bin"), b"\x00\x01\x02\x03")?;

        std::fs::create_dir_all(base_dir.path().join("deep/nested/path"))?;
        std::fs::write(
            base_dir.path().join("deep/nested/path/file.txt"),
            b"deep file",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        // Create delta database
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        Ok((overlay, base_dir, delta_dir))
    }

    /// The core property test: overlay operations must NEVER modify the host filesystem.
    fn test_host_unchanged_property(operations: Vec<FsOperation>) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;

        rt.block_on(async {
            let (overlay, base_dir, _delta_dir) = create_fuzz_overlay().await?;

            // Take snapshot BEFORE any operations
            let snapshot_before = DirectorySnapshot::capture(base_dir.path())
                .expect("Failed to capture initial snapshot");

            // Execute all operations (ignoring errors - some operations may fail
            // due to invalid paths, missing files, etc. That's fine for fuzzing)
            for op in &operations {
                let _ = op.execute(&overlay).await;
            }

            // Take snapshot AFTER all operations
            let snapshot_after = DirectorySnapshot::capture(base_dir.path())
                .expect("Failed to capture final snapshot");

            // THE CRITICAL INVARIANT: snapshots must be identical
            if let Some(diff) = snapshot_before.diff(&snapshot_after) {
                panic!(
                    "HOST FILESYSTEM WAS MODIFIED!\n\
                     Operations performed: {:?}\n\
                     Differences:\n{}",
                    operations, diff
                );
            }

            Ok(())
        })
    }

    /// The property test: written files must be visible, deleted files must not be.
    fn test_modifications_visible_property(operations: Vec<FsOperation>) -> Result<()> {
        use std::collections::{HashMap, HashSet};

        let rt = tokio::runtime::Runtime::new()?;

        rt.block_on(async {
            let (overlay, _base_dir, _delta_dir) = create_fuzz_overlay().await?;

            // Track files we've written and expect to read back
            let mut expected_files: HashMap<String, Vec<u8>> = HashMap::new();
            // Track files we've deleted (that we previously wrote)
            let mut deleted_files: HashSet<String> = HashSet::new();

            for op in &operations {
                match op {
                    FsOperation::WriteFile { path, contents } => {
                        if op.execute(&overlay).await.is_ok() {
                            // Writing a file at this path means anything under it is inaccessible
                            let path_prefix = format!("{}/", path);
                            expected_files.retain(|k, _| !k.starts_with(&path_prefix));

                            expected_files.insert(path.clone(), contents.clone());
                            // Writing removes whiteouts for parent dirs, making their
                            // children visible again. Remove affected paths from deleted.
                            deleted_files.remove(path);
                            deleted_files.retain(|p| !p.starts_with(&path_prefix));
                            let parts: Vec<&str> =
                                path.split('/').filter(|s| !s.is_empty()).collect();
                            for i in 1..parts.len() {
                                let parent = format!("/{}", parts[..i].join("/"));
                                deleted_files.remove(&parent);
                                // Children of this parent become visible
                                let prefix = format!("{}/", parent);
                                deleted_files.retain(|p| !p.starts_with(&prefix));
                            }
                        }
                    }
                    FsOperation::Remove { path } => {
                        if op.execute(&overlay).await.is_ok() {
                            // Remove exact path
                            if expected_files.remove(path).is_some() {
                                deleted_files.insert(path.clone());
                            }
                            // Remove any files under this path (directory removal)
                            let prefix = format!("{}/", path);
                            let children: Vec<_> = expected_files
                                .keys()
                                .filter(|k| k.starts_with(&prefix))
                                .cloned()
                                .collect();
                            for child in children {
                                expected_files.remove(&child);
                                deleted_files.insert(child);
                            }
                        }
                    }
                    FsOperation::Rename { from, to } => {
                        if op.execute(&overlay).await.is_ok() {
                            // Check if source was tracked
                            let source_tracked = expected_files.contains_key(from);

                            // Move tracked file from source to destination
                            if let Some(contents) = expected_files.remove(from) {
                                expected_files.insert(to.clone(), contents);
                            } else {
                                // Source wasn't tracked - destination is overwritten with unknown contents
                                expected_files.remove(to);
                            }

                            // Handle directory renames - move children too
                            let from_prefix = format!("{}/", from);
                            let to_move: Vec<_> = expected_files
                                .keys()
                                .filter(|k| k.starts_with(&from_prefix))
                                .cloned()
                                .collect();
                            if !to_move.is_empty() {
                                for old_path in to_move {
                                    if let Some(contents) = expected_files.remove(&old_path) {
                                        let new_path = old_path.replacen(from, to, 1);
                                        expected_files.insert(new_path, contents);
                                    }
                                }
                            } else if !source_tracked {
                                // Source wasn't tracked at all - remove any tracked children at dest
                                let to_prefix = format!("{}/", to);
                                expected_files.retain(|k, _| !k.starts_with(&to_prefix));
                            }

                            // Source is gone, destination exists - update deleted_files
                            deleted_files.remove(to);
                            let to_prefix = format!("{}/", to);
                            deleted_files.retain(|p| !p.starts_with(&to_prefix));
                        }
                    }
                    _ => {}
                }
            }

            // Verify all expected files are readable with correct contents
            for (path, expected_contents) in &expected_files {
                match overlay.read_file(path).await {
                    Ok(Some(actual_contents)) => {
                        if actual_contents != *expected_contents {
                            panic!(
                                "CONTENT MISMATCH at {}!\n\
                                 Expected: {:?}\n\
                                 Actual: {:?}\n\
                                 Operations: {:?}",
                                path, expected_contents, actual_contents, operations
                            );
                        }
                    }
                    Ok(None) => {
                        panic!(
                            "FILE NOT FOUND at {} but expected contents: {:?}\n\
                             Operations: {:?}",
                            path, expected_contents, operations
                        );
                    }
                    Err(e) => {
                        panic!(
                            "ERROR reading {}: {:?}\n\
                             Operations: {:?}",
                            path, e, operations
                        );
                    }
                }
            }

            // Verify deleted files are NOT readable
            for path in &deleted_files {
                match overlay.read_file(path).await {
                    Ok(Some(contents)) => {
                        panic!(
                            "DELETED FILE STILL EXISTS at {}!\n\
                             Contents: {:?}\n\
                             Operations: {:?}",
                            path, contents, operations
                        );
                    }
                    Ok(None) => {
                        // Good - file is deleted
                    }
                    Err(_) => {
                        // Also acceptable - file doesn't exist
                    }
                }
            }

            Ok(())
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Property test: Any sequence of overlay operations must not modify the host filesystem.
        #[test]
        fn prop_overlay_never_modifies_host(operations in operations_strategy()) {
            test_host_unchanged_property(operations).unwrap();
        }

        /// Property test: Newly written files must be visible when read back.
        #[test]
        fn prop_overlay_modifications_visible(operations in operations_strategy()) {
            test_modifications_visible_property(operations).unwrap();
        }
    }

    // ============================================================================
    // Single-file write operations
    // ============================================================================

    #[test]
    fn test_write_to_existing_base_file() {
        test_host_unchanged_property(vec![FsOperation::WriteFile {
            path: "/base.txt".to_string(),
            contents: b"completely new content".to_vec(),
        }])
        .unwrap();
    }

    #[test]
    fn test_pwrite_to_existing_base_file() {
        test_host_unchanged_property(vec![FsOperation::PWrite {
            path: "/base.txt".to_string(),
            offset: 0,
            data: b"overwrite".to_vec(),
        }])
        .unwrap();
    }

    #[test]
    fn test_truncate_existing_base_file() {
        test_host_unchanged_property(vec![FsOperation::Truncate {
            path: "/base.txt".to_string(),
            size: 0,
        }])
        .unwrap();
    }

    // ============================================================================
    // Remove operations
    // ============================================================================

    #[test]
    fn test_remove_existing_base_file() {
        test_host_unchanged_property(vec![FsOperation::Remove {
            path: "/base.txt".to_string(),
        }])
        .unwrap();
    }

    #[test]
    fn test_remove_existing_base_directory() {
        test_host_unchanged_property(vec![FsOperation::Remove {
            path: "/subdir".to_string(),
        }])
        .unwrap();
    }

    // ============================================================================
    // Rename operations
    // ============================================================================

    #[test]
    fn test_rename_base_file() {
        test_host_unchanged_property(vec![FsOperation::Rename {
            from: "/base.txt".to_string(),
            to: "/renamed.txt".to_string(),
        }])
        .unwrap();
    }

    #[test]
    fn test_rename_base_directory() {
        test_host_unchanged_property(vec![FsOperation::Rename {
            from: "/subdir".to_string(),
            to: "/renamed_dir".to_string(),
        }])
        .unwrap();
    }

    // ============================================================================
    // Symlink operations
    // ============================================================================

    #[test]
    fn test_symlink_operations() {
        test_host_unchanged_property(vec![
            FsOperation::Symlink {
                target: "/base.txt".to_string(),
                linkpath: "/link_to_base".to_string(),
            },
            FsOperation::Symlink {
                target: "../base.txt".to_string(),
                linkpath: "/subdir/relative_link".to_string(),
            },
        ])
        .unwrap();
    }

    // ============================================================================
    // Deep/nested path operations
    // ============================================================================

    #[test]
    fn test_deep_nested_operations() {
        test_host_unchanged_property(vec![
            FsOperation::WriteFile {
                path: "/deep/nested/path/file.txt".to_string(),
                contents: b"modified deep file".to_vec(),
            },
            FsOperation::Mkdir {
                path: "/deep/nested/path/newdir".to_string(),
            },
            FsOperation::Remove {
                path: "/deep/nested/path".to_string(),
            },
        ])
        .unwrap();
    }

    // ============================================================================
    // Complex multi-operation sequences
    // ============================================================================

    #[test]
    fn test_complex_sequence_on_base_files() {
        test_host_unchanged_property(vec![
            // Modify a base file
            FsOperation::WriteFile {
                path: "/base.txt".to_string(),
                contents: b"modified".to_vec(),
            },
            // Delete it
            FsOperation::Remove {
                path: "/base.txt".to_string(),
            },
            // Recreate it
            FsOperation::WriteFile {
                path: "/base.txt".to_string(),
                contents: b"recreated".to_vec(),
            },
            // Rename it
            FsOperation::Rename {
                from: "/base.txt".to_string(),
                to: "/moved.txt".to_string(),
            },
            // Partial write
            FsOperation::PWrite {
                path: "/moved.txt".to_string(),
                offset: 5,
                data: b"XXX".to_vec(),
            },
        ])
        .unwrap();
    }

    // ============================================================================
    // Edge cases
    // ============================================================================

    #[tokio::test]
    async fn test_write_file_over_directory() {
        use std::sync::Arc;
        use tempfile::tempdir;

        let base_dir = tempdir().unwrap();
        let delta_dir = tempdir().unwrap();

        let base = Arc::new(HostFS::new(base_dir.path()).unwrap());
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await.unwrap();
        let overlay = OverlayFS::new(base, delta);
        overlay
            .init(base_dir.path().to_str().unwrap())
            .await
            .unwrap();

        // Create /subdir/nested.txt
        overlay
            .write_file("/subdir/nested.txt", b"content")
            .await
            .unwrap();

        // Verify nested.txt is readable
        let data = overlay.read_file("/subdir/nested.txt").await.unwrap();
        assert_eq!(data, Some(b"content".to_vec()));

        // Write /subdir as a FILE (overwriting the directory)
        overlay.write_file("/subdir", b"file").await.unwrap();

        // Now /subdir is a file, /subdir/nested.txt should NOT be accessible
        let data = overlay.read_file("/subdir/nested.txt").await.unwrap();
        assert_eq!(
            data, None,
            "/subdir/nested.txt should not be accessible when /subdir is a file"
        );
    }
}
