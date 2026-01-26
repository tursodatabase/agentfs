use crate::error::Result;
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, RwLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::trace;
use turso::{Connection, Value};

use super::{agentfs::AgentFS, BoxedFile, DirEntry, FileSystem, FilesystemStats, FsError, Stats};

/// Root inode number (matches FUSE convention)
const ROOT_INO: i64 = 1;

/// Which layer an inode belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Layer {
    Delta,
    Base,
}

/// Information about an inode in the overlay filesystem
#[derive(Debug, Clone)]
struct InodeInfo {
    /// Which layer this inode lives in
    layer: Layer,
    /// The inode number in the underlying layer
    underlying_ino: i64,
    /// Virtual path (for whiteout and copy-up operations)
    path: String,
}

/// A copy-on-write overlay filesystem using inode-based operations.
///
/// Combines a read-only base layer with a writable delta layer (AgentFS).
/// All modifications are written to the delta layer, while reads fall back
/// to the base layer if not found in delta.
pub struct OverlayFS {
    /// The underlying read-only base filesystem
    base: Arc<dyn FileSystem>,
    /// The delta layer where modifications go
    delta: AgentFS,
    /// Map from overlay inode to underlying layer info
    inode_map: RwLock<HashMap<i64, InodeInfo>>,
    /// Reverse map: (layer, underlying_ino) -> overlay_ino
    reverse_map: RwLock<HashMap<(Layer, i64), i64>>,
    /// Map from path to overlay inode (for path-based operations)
    path_map: RwLock<HashMap<String, i64>>,
    /// Next inode number to allocate
    next_ino: AtomicI64,
    /// Set of whiteout paths (deleted from base)
    whiteouts: RwLock<HashSet<String>>,
    /// Origin mapping: delta_ino -> base_ino (for copy-up consistency)
    origin_map: RwLock<HashMap<i64, i64>>,
}

impl OverlayFS {
    /// Create a new overlay filesystem
    pub fn new(base: Arc<dyn FileSystem>, delta: AgentFS) -> Self {
        let mut inode_map = HashMap::new();
        let mut reverse_map = HashMap::new();
        let mut path_map = HashMap::new();

        // Root inode maps to delta's root (inode 1)
        inode_map.insert(
            ROOT_INO,
            InodeInfo {
                layer: Layer::Delta,
                underlying_ino: 1,
                path: "/".to_string(),
            },
        );
        reverse_map.insert((Layer::Delta, 1), ROOT_INO);
        path_map.insert("/".to_string(), ROOT_INO);

        Self {
            base,
            delta,
            inode_map: RwLock::new(inode_map),
            reverse_map: RwLock::new(reverse_map),
            path_map: RwLock::new(path_map),
            next_ino: AtomicI64::new(2),
            whiteouts: RwLock::new(HashSet::new()),
            origin_map: RwLock::new(HashMap::new()),
        }
    }

    /// Initialize the overlay filesystem schema
    pub async fn init_schema(conn: &Connection, base_path: &str) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_whiteout (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
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

    /// Initialize the overlay filesystem
    pub async fn init(&self, base_path: &str) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        Self::init_schema(&conn, base_path).await?;
        self.load_whiteouts(&conn).await?;
        self.load_origins(&conn).await?;
        Ok(())
    }

    /// Load whiteouts from database into memory
    async fn load_whiteouts(&self, conn: &Connection) -> Result<()> {
        let mut rows = conn.query("SELECT path FROM fs_whiteout", ()).await?;
        let mut paths = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Some(path) = row.get_value(0).ok().and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            }) {
                paths.push(path);
            }
        }
        let mut whiteouts = self.whiteouts.write().unwrap();
        for path in paths {
            whiteouts.insert(path);
        }
        Ok(())
    }

    /// Load existing whiteouts (public interface)
    pub async fn load_whiteouts_public(&self) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        self.load_whiteouts(&conn).await
    }

    /// Load persisted state (whiteouts and origin mappings) from database.
    /// Call this after creating an OverlayFS for an existing database.
    pub async fn load(&self) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        self.load_whiteouts(&conn).await?;
        self.load_origins(&conn).await?;
        Ok(())
    }

    /// Load origin mappings from database
    async fn load_origins(&self, conn: &Connection) -> Result<()> {
        let result = conn
            .query("SELECT delta_ino, base_ino FROM fs_origin", ())
            .await;
        if let Ok(mut rows) = result {
            let mut mappings = Vec::new();
            while let Some(row) = rows.next().await? {
                let delta_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
                let base_ino = row.get_value(1).ok().and_then(|v| v.as_integer().copied());
                if let (Some(d), Some(b)) = (delta_ino, base_ino) {
                    mappings.push((d, b));
                }
            }
            let mut origins = self.origin_map.write().unwrap();
            for (d, b) in mappings {
                origins.insert(d, b);
            }
        }
        Ok(())
    }

    /// Check if a path is whiteout (deleted from base)
    fn is_whiteout(&self, path: &str) -> bool {
        let whiteouts = self.whiteouts.read().unwrap();
        // Check path and all ancestors
        let mut current = String::new();
        for component in path.split('/').filter(|s| !s.is_empty()) {
            current = format!("{}/{}", current, component);
            if whiteouts.contains(&current) {
                return true;
            }
        }
        false
    }

    /// Create a whiteout for a path
    async fn create_whiteout(&self, path: &str) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        conn.execute(
            "INSERT OR REPLACE INTO fs_whiteout (path, created_at) VALUES (?, ?)",
            (path, now),
        )
        .await?;
        self.whiteouts.write().unwrap().insert(path.to_string());
        Ok(())
    }

    /// Remove a whiteout
    async fn remove_whiteout(&self, path: &str) -> Result<()> {
        if !self.whiteouts.read().unwrap().contains(path) {
            return Ok(());
        }
        let conn = self.delta.get_connection().await?;
        conn.execute("DELETE FROM fs_whiteout WHERE path = ?", (path,))
            .await?;
        self.whiteouts.write().unwrap().remove(path);
        Ok(())
    }

    /// Get child whiteouts for a directory
    fn get_child_whiteouts(&self, dir_path: &str) -> HashSet<String> {
        let whiteouts = self.whiteouts.read().unwrap();
        let prefix = if dir_path == "/" {
            "/".to_string()
        } else {
            format!("{}/", dir_path)
        };
        whiteouts
            .iter()
            .filter_map(|p| {
                if dir_path == "/" {
                    // Direct children of root
                    let trimmed = p.trim_start_matches('/');
                    if !trimmed.contains('/') {
                        Some(trimmed.to_string())
                    } else {
                        None
                    }
                } else if p.starts_with(&prefix) {
                    let rest = &p[prefix.len()..];
                    if !rest.contains('/') {
                        Some(rest.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Allocate a new overlay inode number
    fn alloc_ino(&self) -> i64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    /// Get or create an overlay inode for a layer inode
    fn get_or_create_overlay_ino(&self, layer: Layer, underlying_ino: i64, path: &str) -> i64 {
        // Check reverse map first
        {
            let reverse = self.reverse_map.read().unwrap();
            if let Some(&ino) = reverse.get(&(layer, underlying_ino)) {
                return ino;
            }
        }

        // Allocate new inode
        let ino = self.alloc_ino();
        {
            let mut inode_map = self.inode_map.write().unwrap();
            inode_map.insert(
                ino,
                InodeInfo {
                    layer,
                    underlying_ino,
                    path: path.to_string(),
                },
            );
        }
        {
            let mut reverse = self.reverse_map.write().unwrap();
            reverse.insert((layer, underlying_ino), ino);
        }
        {
            let mut path_map = self.path_map.write().unwrap();
            path_map.insert(path.to_string(), ino);
        }

        ino
    }

    /// Get inode info for an overlay inode
    fn get_inode_info(&self, ino: i64) -> Option<InodeInfo> {
        self.inode_map.read().unwrap().get(&ino).cloned()
    }

    /// Build path from parent inode and name
    fn build_path(&self, parent_ino: i64, name: &str) -> Result<String> {
        let info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        Ok(if info.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", info.path, name)
        })
    }

    /// Get a reference to the base layer
    pub fn base(&self) -> &Arc<dyn FileSystem> {
        &self.base
    }

    /// Get a reference to the delta layer
    pub fn delta(&self) -> &AgentFS {
        &self.delta
    }

    /// Store origin mapping for copy-up
    async fn add_origin_mapping(&self, delta_ino: i64, base_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)",
            (delta_ino, base_ino),
        )
        .await?;
        self.origin_map.write().unwrap().insert(delta_ino, base_ino);
        Ok(())
    }

    /// Get origin inode for a delta inode
    fn get_origin_ino(&self, delta_ino: i64) -> Option<i64> {
        self.origin_map.read().unwrap().get(&delta_ino).copied()
    }

    /// Ensure parent directories exist in delta layer
    async fn ensure_parent_dirs(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_path = String::new();
        let mut current_parent_ino: i64 = 1; // Delta root

        for component in components.iter().take(components.len().saturating_sub(1)) {
            current_path = format!("{}/{}", current_path, component);

            // Remove any whiteout for this path
            self.remove_whiteout(&current_path).await?;

            // Check if directory exists in delta
            if let Some(stats) =
                FileSystem::lookup(&self.delta, current_parent_ino, component).await?
            {
                if stats.is_directory() {
                    current_parent_ino = stats.ino;
                    continue;
                } else {
                    return Err(FsError::NotADirectory.into());
                }
            }

            // Not in delta, check base and copy metadata
            let base_stats = self.base.lookup(current_parent_ino, component).await?;
            let (dir_uid, dir_gid) = if let Some(s) = &base_stats {
                (s.uid, s.gid)
            } else {
                (uid, gid)
            };

            // Create directory in delta
            let new_stats =
                FileSystem::mkdir(&self.delta, current_parent_ino, component, dir_uid, dir_gid)
                    .await?;
            current_parent_ino = new_stats.ino;
        }

        Ok(())
    }

    /// Copy a file from base to delta for modification
    async fn copy_up(&self, path: &str, base_ino: i64) -> Result<i64> {
        // Parse path to get parent and name
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let name = components.last().unwrap();

        // Check if already copied up - walk delta to find parent and check for file
        let mut parent_ino: i64 = 1;
        let mut found_parent = true;
        for comp in components.iter().take(components.len() - 1) {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, comp).await? {
                parent_ino = stats.ino;
            } else {
                found_parent = false;
                break;
            }
        }

        // If parent exists in delta, check if file already exists there
        if found_parent {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, name).await? {
                // Already copied up, return delta inode
                return Ok(stats.ino);
            }
        }

        // Get base stats
        let base_stats = self
            .base
            .getattr(base_ino)
            .await?
            .ok_or(FsError::NotFound)?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(path, base_stats.uid, base_stats.gid)
            .await?;

        // Look up parent in delta by walking the path
        let mut parent_ino: i64 = 1; // Start at delta root
        for comp in components.iter().take(components.len() - 1) {
            let stats = FileSystem::lookup(&self.delta, parent_ino, comp)
                .await?
                .ok_or(FsError::NotFound)?;
            parent_ino = stats.ino;
        }

        // Copy based on file type
        let delta_ino = if base_stats.is_symlink() {
            let target = self
                .base
                .readlink(base_ino)
                .await?
                .ok_or(FsError::NotFound)?;
            let stats = FileSystem::symlink(
                &self.delta,
                parent_ino,
                name,
                &target,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else if base_stats.is_directory() {
            let stats = FileSystem::mkdir(
                &self.delta,
                parent_ino,
                name,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else {
            // Regular file - read content and create
            let base_file = self.base.open(base_ino).await?;
            let content = base_file.pread(0, base_stats.size as u64).await?;

            let (stats, delta_file) = FileSystem::create_file(
                &self.delta,
                parent_ino,
                name,
                base_stats.mode,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            delta_file.pwrite(0, &content).await?;
            stats.ino
        };

        // Store origin mapping
        self.add_origin_mapping(delta_ino, base_ino).await?;

        Ok(delta_ino)
    }

    /// Copy-up a file and update the inode mapping so subsequent operations
    /// go to the delta layer. Returns the delta inode.
    async fn copy_up_and_update_mapping(&self, overlay_ino: i64, info: &InodeInfo) -> Result<i64> {
        let delta_ino = self.copy_up(&info.path, info.underlying_ino).await?;

        // Update the inode mapping to point to delta
        {
            let mut inode_map = self.inode_map.write().unwrap();
            inode_map.insert(
                overlay_ino,
                InodeInfo {
                    layer: Layer::Delta,
                    underlying_ino: delta_ino,
                    path: info.path.clone(),
                },
            );
        }
        {
            let mut reverse_map = self.reverse_map.write().unwrap();
            // Keep the base mapping so lookups via origin still return the same overlay inode
            // (Layer::Base, base_ino) -> overlay_ino is kept
            // Add the delta mapping as well
            reverse_map.insert((Layer::Delta, delta_ino), overlay_ino);
        }

        Ok(delta_ino)
    }
}

#[async_trait]
impl FileSystem for OverlayFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        trace!(
            "OverlayFS::lookup: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check for whiteout
        if self.is_whiteout(&path) {
            return Ok(None);
        }

        // Try delta first - need to find the corresponding delta parent
        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            // Parent is in base, walk the path in delta to find corresponding directory
            let mut ino: i64 = 1; // Start at delta root
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = self.delta.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    // Path doesn't exist in delta, use current position
                    break;
                }
            }
            ino
        };

        // Look up in delta
        if let Some(delta_stats) = self.delta.lookup(delta_parent_ino, name).await? {
            let ino = self.get_or_create_overlay_ino(Layer::Delta, delta_stats.ino, &path);
            let mut stats = delta_stats;

            // Check for origin mapping to return stable inode
            if let Some(base_ino) = self.get_origin_ino(stats.ino) {
                stats.ino = self.get_or_create_overlay_ino(Layer::Base, base_ino, &path);
            } else {
                stats.ino = ino;
            }

            return Ok(Some(stats));
        }

        // Try base
        let base_parent_ino = if parent_info.layer == Layer::Base {
            parent_info.underlying_ino
        } else {
            // Need to find corresponding base parent by path
            // For root, use base root (1)
            if parent_info.path == "/" {
                1
            } else {
                // Walk the base to find the parent
                let mut base_ino: i64 = 1;
                for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                    if let Some(s) = self.base.lookup(base_ino, comp).await? {
                        base_ino = s.ino;
                    } else {
                        return Ok(None);
                    }
                }
                base_ino
            }
        };

        if let Some(base_stats) = self.base.lookup(base_parent_ino, name).await? {
            let ino = self.get_or_create_overlay_ino(Layer::Base, base_stats.ino, &path);
            let mut stats = base_stats;
            stats.ino = ino;
            return Ok(Some(stats));
        }

        Ok(None)
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        trace!("OverlayFS::getattr: ino={}", ino);

        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return Ok(None),
        };

        let stats = match info.layer {
            Layer::Delta => FileSystem::getattr(&self.delta, info.underlying_ino).await?,
            Layer::Base => self.base.getattr(info.underlying_ino).await?,
        };

        Ok(stats.map(|mut s| {
            s.ino = ino;
            s
        }))
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        trace!("OverlayFS::readlink: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;

        match info.layer {
            Layer::Delta => FileSystem::readlink(&self.delta, info.underlying_ino).await,
            Layer::Base => self.base.readlink(info.underlying_ino).await,
        }
    }

    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        trace!("OverlayFS::readdir: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries = HashSet::new();

        // Get delta entries
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir(info.underlying_ino).await? {
                entries.extend(delta_entries);
            }
        }

        // Get base entries (need to resolve base inode from path)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            // Walk base to find corresponding directory
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir(base_ino).await? {
                for entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", info.path, entry)
                    };
                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry) {
                        entries.insert(entry);
                    }
                }
            }
        }

        let mut result: Vec<_> = entries.into_iter().collect();
        result.sort();
        Ok(Some(result))
    }

    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        trace!("OverlayFS::readdir_plus: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries_map: HashMap<String, DirEntry> = HashMap::new();

        // Get base entries first (so delta can override)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir_plus(base_ino).await? {
                for mut entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };

                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry.name) {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Base,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                        entries_map.insert(entry.name.clone(), entry);
                    }
                }
            }
        }

        // Get delta entries (override base)
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir_plus(info.underlying_ino).await? {
                for mut entry in delta_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };

                    // Check for origin mapping
                    if let Some(base_ino) = self.get_origin_ino(entry.stats.ino) {
                        entry.stats.ino =
                            self.get_or_create_overlay_ino(Layer::Base, base_ino, &entry_path);
                    } else {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Delta,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                    }

                    entries_map.insert(entry.name.clone(), entry);
                }
            }
        }

        let mut result: Vec<_> = entries_map.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        trace!("OverlayFS::chmod: ino={}, mode={:o}", ino, mode);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        self.delta.chmod(delta_ino, mode).await
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        trace!(
            "OverlayFS::chown: ino={}, uid={:?}, gid={:?}",
            ino,
            uid,
            gid
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        self.delta.chown(delta_ino, uid, gid).await
    }

    async fn open(&self, ino: i64) -> Result<BoxedFile> {
        trace!("OverlayFS::open: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        FileSystem::open(&self.delta, delta_ino).await
    }

    async fn mkdir(&self, parent_ino: i64, name: &str, uid: u32, gid: u32) -> Result<Stats> {
        trace!("OverlayFS::mkdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if already exists
        if self.lookup(parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        // Get delta parent inode
        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            // Walk delta to find parent
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        let mut stats = FileSystem::mkdir(&self.delta, delta_parent_ino, name, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        trace!(
            "OverlayFS::create_file: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        // Get delta parent inode
        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        let (mut stats, file) =
            FileSystem::create_file(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok((stats, file))
    }

    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!("OverlayFS::mknod: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        let mut stats =
            FileSystem::mknod(&self.delta, delta_parent_ino, name, mode, rdev, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!(
            "OverlayFS::symlink: parent_ino={}, name={}, target={}",
            parent_ino,
            name,
            target
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        let mut stats =
            FileSystem::symlink(&self.delta, delta_parent_ino, name, target, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!(
            "OverlayFS::unlink: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.is_directory() {
            return Err(FsError::IsADirectory.into());
        }

        // Try to remove from delta
        if parent_info.layer == Layer::Delta {
            let _ = FileSystem::unlink(&self.delta, parent_info.underlying_ino, name).await;
        }

        // Check if exists in base
        let base_parent_ino = if parent_info.layer == Layer::Base {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    return Ok(()); // Parent doesn't exist in base
                }
            }
            ino
        };

        if self.base.lookup(base_parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!("OverlayFS::rmdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists and is a directory
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if !stats.is_directory() {
            return Err(FsError::NotADirectory.into());
        }

        // Check if directory is empty (in overlay view)
        let dir_entries = self.readdir(stats.ino).await?.unwrap_or_default();
        if !dir_entries.is_empty() {
            return Err(FsError::NotEmpty.into());
        }

        // Try to remove from delta
        if parent_info.layer == Layer::Delta {
            let _ = FileSystem::rmdir(&self.delta, parent_info.underlying_ino, name).await;
        }

        // Check if exists in base
        let base_parent_ino = if parent_info.layer == Layer::Base {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    return Ok(());
                }
            }
            ino
        };

        if self.base.lookup(base_parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        trace!(
            "OverlayFS::link: ino={}, newparent_ino={}, newname={}",
            ino,
            newparent_ino,
            newname
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Ensure file is in delta (copy up if needed)
        let delta_ino = if info.layer == Layer::Delta {
            info.underlying_ino
        } else {
            self.copy_up(&info.path, info.underlying_ino).await?
        };

        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Get delta parent
        let delta_parent_ino = if parent_info.layer == Layer::Delta {
            parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        let mut stats = FileSystem::link(&self.delta, delta_ino, delta_parent_ino, newname).await?;
        stats.ino = ino; // Keep original overlay inode

        Ok(stats)
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<()> {
        trace!(
            "OverlayFS::rename: oldparent={}, oldname={}, newparent={}, newname={}",
            oldparent_ino,
            oldname,
            newparent_ino,
            newname
        );

        let old_parent_info = self
            .get_inode_info(oldparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let old_path = self.build_path(oldparent_ino, oldname)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Get source stats
        let src_stats = self
            .lookup(oldparent_ino, oldname)
            .await?
            .ok_or(FsError::NotFound)?;
        let src_info = self
            .get_inode_info(src_stats.ino)
            .ok_or(FsError::NotFound)?;

        // Ensure source is in delta
        let delta_src_parent_ino = if old_parent_info.layer == Layer::Delta {
            old_parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in old_parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        // If source is in base, copy to delta first
        if src_info.layer == Layer::Base {
            self.copy_up(&old_path, src_info.underlying_ino).await?;
        }

        // Remove whiteout at destination
        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Get delta destination parent
        let delta_dst_parent_ino = if new_parent_info.layer == Layer::Delta {
            new_parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in new_parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = FileSystem::lookup(&self.delta, ino, comp).await? {
                    ino = s.ino;
                }
            }
            ino
        };

        // Perform rename in delta
        FileSystem::rename(
            &self.delta,
            delta_src_parent_ino,
            oldname,
            delta_dst_parent_ino,
            newname,
        )
        .await?;

        // Create whiteout at source if it existed in base
        let base_src_parent_ino = if old_parent_info.layer == Layer::Base {
            old_parent_info.underlying_ino
        } else {
            let mut ino: i64 = 1;
            for comp in old_parent_info.path.split('/').filter(|s| !s.is_empty()) {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    return Ok(());
                }
            }
            ino
        };

        if self
            .base
            .lookup(base_src_parent_ino, oldname)
            .await?
            .is_some()
        {
            self.create_whiteout(&old_path).await?;
        }

        Ok(())
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        FileSystem::statfs(&self.delta).await
    }

    async fn forget(&self, ino: i64, nlookup: u64) {
        // Look up the inode info to determine which layer it belongs to
        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return, // Unknown inode, nothing to forget
        };

        // Pass through to the appropriate layer
        match info.layer {
            Layer::Delta => {
                // Delta (AgentFS) doesn't cache fds, but call it anyway for completeness
                FileSystem::forget(&self.delta, info.underlying_ino, nlookup).await;
            }
            Layer::Base => {
                // Base layer (HostFS) caches O_PATH fds and needs forget
                self.base.forget(info.underlying_ino, nlookup).await;
            }
        }

        // Note: We don't remove from inode_map here because the overlay layer's
        // inode mapping is relatively lightweight (no fd). The base layer's
        // forget handles the actual fd cleanup.
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::filesystem::HostFS;
    use crate::DEFAULT_FILE_MODE;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    async fn create_test_overlay() -> Result<(OverlayFS, tempfile::TempDir, tempfile::TempDir)> {
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("base.txt"), b"base content")?;
        std::fs::create_dir(base_dir.path().join("subdir"))?;
        std::fs::write(base_dir.path().join("subdir/nested.txt"), b"nested")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        Ok((overlay, base_dir, delta_dir))
    }

    #[tokio::test]
    async fn test_overlay_lookup_base() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup file from base
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_in_delta() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Create file in delta
        let (stats, file) = overlay
            .create_file(ROOT_INO, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify it exists
        let lookup_stats = overlay.lookup(ROOT_INO, "new.txt").await?.unwrap();
        assert_eq!(lookup_stats.ino, stats.ino);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_whiteout() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // File exists initially
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_some());

        // Delete it
        overlay.unlink(ROOT_INO, "base.txt").await?;

        // File should be gone
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        // Open and write to it (should trigger copy-up)
        let file = overlay.open(stats.ino).await?;
        file.pwrite(0, b"modified content").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify reading through overlay returns modified content
        let read_back = file.pread(0, 100).await?;
        assert_eq!(
            read_back, b"modified content",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_inode_stability() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file and record its inode
        let stats_before = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats_before.ino;

        // Open triggers copy-up
        let file = overlay.open(stats_before.ino).await?;
        file.pwrite(0, b"modified").await?;

        // Lookup again - inode should be the same
        let stats_after = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_chmod() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats.ino;

        // chmod should trigger copy-up
        overlay.chmod(stats.ino, 0o755).await?;

        // Verify base file mode is UNCHANGED
        let base_meta = std::fs::metadata(base_dir.path().join("base.txt"))?;
        assert_ne!(
            base_meta.permissions().mode() & 0o777,
            0o755,
            "base file mode should be unchanged"
        );

        // Verify overlay returns new mode
        let stats_after = overlay.getattr(stats.ino).await?.unwrap();
        assert_eq!(
            stats_after.mode & 0o777,
            0o755,
            "overlay should return new mode"
        );

        // Inode should remain stable
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after chmod copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_truncate() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(stats.size, 12); // "base content"

        // Open and truncate (triggers copy-up via open)
        let file = overlay.open(stats.ino).await?;
        file.truncate(5).await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify overlay returns truncated size
        let stats_after = file.fstat().await?;
        assert_eq!(stats_after.size, 5, "overlay should return truncated size");

        // Verify content is truncated
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"base ", "content should be truncated");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_rename() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file (to populate overlay state)
        let _stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();

        // Rename should trigger copy-up
        overlay
            .rename(ROOT_INO, "base.txt", ROOT_INO, "renamed.txt")
            .await?;

        // Base file should still exist (we don't modify base)
        assert!(
            base_dir.path().join("base.txt").exists(),
            "base file should still exist"
        );

        // Old name should be gone in overlay (whiteout)
        assert!(
            overlay.lookup(ROOT_INO, "base.txt").await?.is_none(),
            "old name should be gone"
        );

        // New name should exist in overlay
        let renamed_stats = overlay.lookup(ROOT_INO, "renamed.txt").await?.unwrap();
        assert!(renamed_stats.is_file());

        // Content should be preserved
        let file = overlay.open(renamed_stats.ino).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"base content",
            "content should be preserved after rename"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_nested_file() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup nested file in subdir
        let subdir_stats = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        let nested_stats = overlay
            .lookup(subdir_stats.ino, "nested.txt")
            .await?
            .unwrap();

        // Open and modify (triggers copy-up, should also create parent dir in delta)
        let file = overlay.open(nested_stats.ino).await?;
        file.pwrite(0, b"modified nested").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("subdir/nested.txt"))?;
        assert_eq!(
            base_content, b"nested",
            "base nested file should be unchanged"
        );

        // Verify overlay returns modified content
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"modified nested",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_symlink() -> Result<()> {
        // Create overlay with a symlink in base
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("target.txt"), b"target content")?;
        std::os::unix::fs::symlink("target.txt", base_dir.path().join("link.txt"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup symlink
        let link_stats = overlay.lookup(ROOT_INO, "link.txt").await?.unwrap();
        assert!(link_stats.is_symlink());

        // Read the symlink target
        let target = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(target, "target.txt");

        // chmod on symlink triggers copy-up
        overlay.chmod(link_stats.ino, 0o755).await?;

        // Verify symlink target is preserved after copy-up
        let target_after = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(
            target_after, "target.txt",
            "symlink target should be preserved after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_file_in_deeply_nested_base_dir() -> Result<()> {
        // This test reproduces a bug where ensure_parent_dirs uses delta inodes
        // to lookup in base layer, which breaks for paths deeper than one level.
        //
        // Setup: base has /a/b/c/ directory structure
        // Test: create a new file at /a/b/c/new.txt
        // Bug: ensure_parent_dirs would use delta inode for "a" to lookup "b" in base
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory());
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(b_stats.is_directory());
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(c_stats.is_directory());

        // Create a new file in the deeply nested directory
        // This should trigger ensure_parent_dirs to create /a/b/c in delta
        let (new_stats, file) = overlay
            .create_file(c_stats.ino, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify the file was created
        assert!(new_stats.is_file());

        // Verify we can read it back
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"new content");

        // Verify the existing file in base is still accessible
        let existing_stats = overlay.lookup(c_stats.ino, "existing.txt").await?.unwrap();
        let existing_file = overlay.open(existing_stats.ino).await?;
        let existing_content = existing_file.pread(0, 100).await?;
        assert_eq!(existing_content, b"existing");

        // Verify base is unchanged
        assert!(base_dir.path().join("a/b/c/existing.txt").exists());
        assert!(!base_dir.path().join("a/b/c/new.txt").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_mkdir_in_deeply_nested_base_dir() -> Result<()> {
        // Similar test but for mkdir instead of create_file
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();

        // Create a new subdirectory in the deeply nested directory
        let new_dir_stats = overlay.mkdir(c_stats.ino, "newdir", 0, 0).await?;
        assert!(new_dir_stats.is_directory());

        // Verify we can create a file inside the new directory
        let (file_stats, file) = overlay
            .create_file(new_dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"nested file").await?;
        assert!(file_stats.is_file());

        // Verify base is unchanged
        assert!(!base_dir.path().join("a/b/c/newdir").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_lookup_after_mkdir_in_base_parent() -> Result<()> {
        // This test reproduces a bug where lookup uses delta root (inode 1)
        // when parent is in Base layer, instead of walking the delta path.
        //
        // Scenario (mimics FUSE behavior):
        // 1. Lookup "target" in root → gets base layer inode
        // 2. mkdir("debug") inside "target" → creates /target/debug in delta
        // 3. Lookup "debug" in "target" → should find it, but bug causes it to
        //    look at delta root instead of delta's "/target"
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("target"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup "target" - this creates a Base layer mapping
        let target_stats = overlay.lookup(ROOT_INO, "target").await?.unwrap();
        assert!(target_stats.is_directory());

        // Step 2: Create "debug" inside "target"
        // This should create /target in delta, then /target/debug in delta
        let debug_stats = overlay.mkdir(target_stats.ino, "debug", 0, 0).await?;
        assert!(debug_stats.is_directory());

        // Step 3: Lookup "debug" inside "target" - this is where the bug manifests!
        // The bug: lookup uses delta root (1) when parent is Base layer,
        // so it looks for "debug" at delta root instead of delta's "/target"
        let debug_lookup = overlay.lookup(target_stats.ino, "debug").await?;
        assert!(
            debug_lookup.is_some(),
            "Should find 'debug' inside 'target' after mkdir"
        );
        assert!(debug_lookup.unwrap().is_directory());

        // Also verify we can create files inside the new directory
        let (file_stats, file) = overlay
            .create_file(debug_stats.ino, "test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"test content").await?;
        assert!(file_stats.is_file());

        // And lookup should find the file too
        let file_lookup = overlay.lookup(debug_stats.ino, "test.txt").await?;
        assert!(
            file_lookup.is_some(),
            "Should find 'test.txt' inside 'debug'"
        );

        Ok(())
    }
}
