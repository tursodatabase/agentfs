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

use super::{
    agentfs::AgentFS, BoxedFile, DirEntry, FileSystem, FilesystemStats, FsError, Stats, TimeChange,
};

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

    /// Refresh an existing overlay inode mapping to point at a new backing inode/path.
    ///
    /// This is used when we intentionally reuse an existing overlay inode number
    /// (for stability), but the underlying layer/path has changed (for example after
    /// a base file is copied-up and then renamed in delta).
    fn refresh_overlay_mapping(
        &self,
        overlay_ino: i64,
        new_layer: Layer,
        new_underlying_ino: i64,
        new_path: &str,
    ) {
        let old_path = {
            let mut inode_map = self.inode_map.write().unwrap();
            let Some(info) = inode_map.get_mut(&overlay_ino) else {
                return;
            };
            let old_path = info.path.clone();
            info.layer = new_layer;
            info.underlying_ino = new_underlying_ino;
            info.path = new_path.to_string();
            old_path
        };

        {
            let mut reverse = self.reverse_map.write().unwrap();
            reverse.insert((new_layer, new_underlying_ino), overlay_ino);
        }

        {
            let mut path_map = self.path_map.write().unwrap();
            if path_map.get(&old_path).copied() == Some(overlay_ino) {
                path_map.remove(&old_path);
            }
            path_map.insert(new_path.to_string(), overlay_ino);
        }
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

    /// Promote an overlay inode from base layer to delta layer.
    ///
    /// When a directory that was originally looked up from base gets a
    /// corresponding directory created in delta (via ensure_parent_dirs),
    /// we need to update the overlay inode to point to delta. This ensures
    /// that operations like readdir and unlink will check the delta layer.
    fn promote_to_delta(&self, path: &str, delta_ino: i64) {
        let path_map = self.path_map.read().unwrap();
        let overlay_ino = match path_map.get(path) {
            Some(&ino) => ino,
            None => return, // No existing mapping, nothing to promote
        };
        drop(path_map);

        // Update the inode mapping to point to delta
        let mut inode_map = self.inode_map.write().unwrap();
        if let Some(info) = inode_map.get_mut(&overlay_ino) {
            if info.layer == Layer::Base {
                let old_base_ino = info.underlying_ino;
                info.layer = Layer::Delta;
                info.underlying_ino = delta_ino;

                // Update reverse map: add delta mapping (keep base mapping for origin lookups)
                drop(inode_map);
                let mut reverse = self.reverse_map.write().unwrap();
                reverse.remove(&(Layer::Base, old_base_ino));
                reverse.insert((Layer::Delta, delta_ino), overlay_ino);
            }
        }
    }

    /// Resolve the delta-layer inode for a parent directory.
    ///
    /// If the parent's overlay inode already maps to Delta, returns the underlying
    /// inode directly. Otherwise, walks the delta filesystem from root using the
    /// stored path. Returns Ok(None) if any path component is missing in delta.
    async fn resolve_delta_parent(&self, info: &InodeInfo) -> Result<Option<i64>> {
        if info.layer == Layer::Delta {
            return Ok(Some(info.underlying_ino));
        }
        let mut ino: i64 = 1;
        for comp in info.path.split('/').filter(|s| !s.is_empty()) {
            match FileSystem::lookup(&self.delta, ino, comp).await? {
                Some(s) if s.is_directory() => ino = s.ino,
                Some(_) => return Ok(None),
                None => return Ok(None),
            }
        }
        Ok(Some(ino))
    }

    /// Ensure parent directories exist in delta layer
    async fn ensure_parent_dirs(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_path = String::new();
        let mut current_delta_ino: i64 = 1; // Delta root
        let mut current_base_ino: i64 = 1; // Base root

        for component in components.iter().take(components.len().saturating_sub(1)) {
            current_path = format!("{}/{}", current_path, component);

            // Remove any whiteout for this path
            self.remove_whiteout(&current_path).await?;

            // Check if directory exists in delta
            if let Some(stats) =
                FileSystem::lookup(&self.delta, current_delta_ino, component).await?
            {
                if stats.is_directory() {
                    current_delta_ino = stats.ino;
                    // Advance base in parallel so it stays in sync
                    if let Some(bs) = self.base.lookup(current_base_ino, component).await? {
                        current_base_ino = bs.ino;
                    }
                    continue;
                } else {
                    return Err(FsError::NotADirectory.into());
                }
            }

            // Not in delta, check base (using the base inode, not delta inode)
            let base_stats = self.base.lookup(current_base_ino, component).await?;
            let (dir_uid, dir_gid, origin_base_ino) = if let Some(s) = &base_stats {
                let base_ino = s.ino;
                current_base_ino = base_ino;
                (s.uid, s.gid, Some(base_ino))
            } else {
                (uid, gid, None)
            };

            // Create directory in delta
            let new_stats = FileSystem::mkdir(
                &self.delta,
                current_delta_ino,
                component,
                0o755,
                dir_uid,
                dir_gid,
            )
            .await?;
            current_delta_ino = new_stats.ino;

            // Create origin mapping if directory exists in base, so that
            // lookups return consistent overlay inodes
            if let Some(base_ino) = origin_base_ino {
                self.add_origin_mapping(new_stats.ino, base_ino).await?;
                // Promote the overlay inode to delta so readdir/unlink will check delta
                self.promote_to_delta(&current_path, new_stats.ino);
            }
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
                base_stats.mode & 0o7777,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else {
            // Regular file - read content and create
            let base_file = self.base.open(base_ino, libc::O_RDONLY).await?;
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

        // Try delta first
        let delta_parent_ino = self.resolve_delta_parent(&parent_info).await?;

        // Look up in delta (only if we resolved the correct parent)
        if let Some(delta_stats) = match delta_parent_ino {
            Some(ino) => self.delta.lookup(ino, name).await?,
            None => None,
        } {
            let delta_ino = delta_stats.ino;
            let ino = self.get_or_create_overlay_ino(Layer::Delta, delta_ino, &path);
            let mut stats = delta_stats;

            // Origin mapping: reuse an existing Base overlay inode for stable
            // numbering within a session.  After remount the base_ino stored in
            // the mapping may be stale (the new HostFS has a fresh inode cache),
            // so only use it when the reverse_map already contains a live entry.
            // Otherwise keep the Delta overlay inode — the downstream code
            // already walks base from root when the parent is tagged Delta.
            if let Some(base_ino) = self.get_origin_ino(stats.ino) {
                let reverse = self.reverse_map.read().unwrap();
                if let Some(existing_ino) = reverse.get(&(Layer::Base, base_ino)).copied() {
                    drop(reverse);
                    self.refresh_overlay_mapping(existing_ino, Layer::Delta, delta_ino, &path);
                    stats.ino = existing_ino;
                } else {
                    stats.ino = ino;
                }
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
        if self.is_whiteout(&info.path) {
            return Ok(None);
        }

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
        if self.is_whiteout(&info.path) {
            return Ok(None);
        }

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
                for entry in delta_entries {
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
                    if self.is_whiteout(&entry_path) || child_whiteouts.contains(&entry.name) {
                        continue;
                    }

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
        if self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

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
        if self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        self.delta.chown(delta_ino, uid, gid).await
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        trace!("OverlayFS::utimens: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        self.delta.utimens(delta_ino, atime, mtime).await
    }

    async fn open(&self, ino: i64, flags: i32) -> Result<BoxedFile> {
        trace!("OverlayFS::open: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => self.copy_up_and_update_mapping(ino, &info).await?,
        };

        FileSystem::open(&self.delta, delta_ino, flags).await
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
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

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::mkdir(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
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

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

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

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

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

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

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

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            match FileSystem::unlink(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
        }

        // If the file is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
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

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            match FileSystem::rmdir(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
        }

        // If the directory is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
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

        // Resolve delta parent AFTER ensure_parent_dirs so the directories exist.
        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

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

        // If source is in base, copy to delta first
        if src_info.layer == Layer::Base {
            self.copy_up(&old_path, src_info.underlying_ino).await?;
        }

        // Remove whiteout at destination
        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Resolve delta parents AFTER copy_up / ensure_parent_dirs,
        // since those create the parent directories in delta.
        let delta_src_parent_ino = self
            .resolve_delta_parent(&old_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;
        let delta_dst_parent_ino = self
            .resolve_delta_parent(&new_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        // Perform rename in delta
        FileSystem::rename(
            &self.delta,
            delta_src_parent_ino,
            oldname,
            delta_dst_parent_ino,
            newname,
        )
        .await?;

        // If the old file is still visible through the overlay after the rename,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(oldparent_ino, oldname).await?.is_some() {
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
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
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
        let file = overlay.open(stats_before.ino, libc::O_RDWR).await?;
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
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
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
        let file = overlay.open(renamed_stats.ino, libc::O_RDONLY).await?;
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
        let file = overlay.open(nested_stats.ino, libc::O_RDWR).await?;
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
        let existing_file = overlay.open(existing_stats.ino, libc::O_RDONLY).await?;
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
        let new_dir_stats = overlay.mkdir(c_stats.ino, "newdir", 0o755, 0, 0).await?;
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
        let debug_stats = overlay
            .mkdir(target_stats.ino, "debug", 0o755, 0, 0)
            .await?;
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

    /// Test that lookup in a base subdirectory does not return an unrelated
    /// delta entry with the same name from a wrong parent.
    ///
    /// Reproduces the ENOTDIR bug:
    ///   1. Base has /sdk/rust/ (directories)
    ///   2. Delta has a *file* named "rust" under delta root (from some unrelated op)
    ///   3. lookup(sdk_ino, "rust") should return the base *directory*, not the delta file
    ///
    /// The bug: when parent is Base layer, the delta path walk breaks early
    /// (because "sdk" doesn't exist in delta) and uses delta root as parent.
    /// Then delta.lookup(root, "rust") finds the unrelated file and returns it.
    #[tokio::test]
    async fn test_overlay_lookup_base_subdir_not_shadowed_by_wrong_delta_parent() -> Result<()> {
        // Base: /sdk/rust/Cargo.toml (nested directories + file)
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::write(
            base_dir.path().join("sdk/rust/Cargo.toml"),
            b"[package]\nname = \"test\"",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a *file* named "rust" at the overlay root (in delta).
        // This is the entry that could shadow the base directory if the delta
        // path walk uses the wrong parent inode.
        let (_file_stats, file) = overlay
            .create_file(ROOT_INO, "rust", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"this is a file, not a directory").await?;

        // Lookup "sdk" from root — should be a base directory
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        assert!(sdk_stats.is_directory(), "sdk should be a directory");

        // Lookup "rust" under "sdk" — MUST return the base *directory*, not
        // the delta *file* named "rust" that lives under root.
        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_stats.is_directory(),
            "sdk/rust should be a directory from base, not the file from delta root"
        );

        // Verify we can traverse further into sdk/rust/Cargo.toml
        let toml_stats = overlay.lookup(rust_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml should be a file");

        Ok(())
    }

    /// Test that readdir_plus and lookup agree on entry types for base dirs.
    ///
    /// readdir_plus for a Base-layer directory only returns base entries,
    /// while lookup checks delta first. They must agree on types.
    #[tokio::test]
    async fn test_overlay_readdir_plus_consistent_with_lookup_for_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::write(base_dir.path().join("sdk/rust/lib.rs"), b"fn main() {}")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a file named "rust" at the root in delta (wrong-parent scenario)
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "rust", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy").await?;

        // Lookup "sdk" to get its overlay inode
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();

        // readdir_plus on "sdk" should list "rust" as a directory
        let entries = overlay
            .readdir_plus(sdk_stats.ino)
            .await?
            .expect("readdir_plus should succeed on sdk");
        let rust_entry = entries.iter().find(|e| e.name == "rust");
        assert!(rust_entry.is_some(), "readdir_plus should list 'rust'");
        assert!(
            rust_entry.unwrap().stats.is_directory(),
            "readdir_plus should report 'rust' as directory"
        );

        // lookup on "sdk" for "rust" should also return a directory
        let rust_lookup = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_lookup.is_directory(),
            "lookup should also report 'rust' as directory (consistent with readdir_plus)"
        );

        Ok(())
    }

    /// Test lookup through deeply nested base directories when an unrelated
    /// file exists at an intermediate name in the delta root.
    ///
    /// Base: /a/b/c/file.txt
    /// Delta root has file named "b"
    /// lookup(a_ino, "b") must return the base directory, not the delta file.
    #[tokio::test]
    async fn test_overlay_lookup_deep_nesting_with_delta_name_collision() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/file.txt"), b"deep content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create files named "b" and "c" at delta root — potential collisions
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "b", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy b").await?;
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "c", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy c").await?;

        // Walk the base path: root → a → b → c → file.txt
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory(), "a should be a directory");

        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(
            b_stats.is_directory(),
            "a/b should be a directory, not the delta file 'b'"
        );

        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(
            c_stats.is_directory(),
            "a/b/c should be a directory, not the delta file 'c'"
        );

        let file_stats = overlay.lookup(c_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Read the file to verify correct traversal
        let file = overlay.open(file_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"deep content");

        Ok(())
    }

    /// Test that after a copy-up creates directories in delta, lookup still
    /// returns correct types for sibling entries in the base.
    ///
    /// Scenario:
    ///   1. Base has /sdk/rust/ and /sdk/python/ (two sibling dirs)
    ///   2. Modify a file under /sdk/rust/ → triggers copy-up, creates
    ///      "sdk" and "rust" dirs in delta
    ///   3. Lookup /sdk/python/ must still work (base directory)
    #[tokio::test]
    async fn test_overlay_lookup_sibling_base_dirs_after_copy_up() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::create_dir_all(base_dir.path().join("sdk/python"))?;
        std::fs::write(base_dir.path().join("sdk/rust/lib.rs"), b"fn main() {}")?;
        std::fs::write(base_dir.path().join("sdk/python/main.py"), b"print('hi')")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to sdk/rust/lib.rs and modify it (triggers copy-up)
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        let lib_stats = overlay.lookup(rust_stats.ino, "lib.rs").await?.unwrap();
        let lib_file = overlay.open(lib_stats.ino, libc::O_RDWR).await?;
        lib_file
            .pwrite(0, b"fn main() { println!(\"hello\"); }")
            .await?;

        // Now lookup the sibling: sdk/python must still be a directory
        let python_stats = overlay.lookup(sdk_stats.ino, "python").await?.unwrap();
        assert!(
            python_stats.is_directory(),
            "sdk/python should still be a directory after copy-up of sdk/rust/lib.rs"
        );

        // And sdk/python/main.py must be accessible
        let main_py = overlay.lookup(python_stats.ino, "main.py").await?.unwrap();
        assert!(main_py.is_file());
        let file = overlay.open(main_py.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"print('hi')");

        Ok(())
    }

    /// Test the exact cargo scenario: path dependency at ../sdk/rust/Cargo.toml
    /// accessed after some delta writes have occurred.
    #[tokio::test]
    async fn test_overlay_cargo_path_dependency_scenario() -> Result<()> {
        // Simulate the agentfs repo structure:
        // /cli/Cargo.toml
        // /sdk/rust/Cargo.toml
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("cli/src"))?;
        std::fs::write(
            base_dir.path().join("cli/Cargo.toml"),
            b"[package]\nname = \"cli\"",
        )?;
        std::fs::write(base_dir.path().join("cli/src/main.rs"), b"fn main() {}")?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust/src"))?;
        std::fs::write(
            base_dir.path().join("sdk/rust/Cargo.toml"),
            b"[package]\nname = \"sdk\"",
        )?;
        std::fs::write(
            base_dir.path().join("sdk/rust/src/lib.rs"),
            b"pub fn hello() {}",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Simulate some writes in cli/ (like cargo creating target/)
        let cli_stats = overlay.lookup(ROOT_INO, "cli").await?.unwrap();
        let _target_stats = overlay.mkdir(cli_stats.ino, "target", 0o755, 0, 0).await?;

        // Now simulate cargo resolving ../sdk/rust/Cargo.toml
        // This is the path that fails with ENOTDIR in the bug report
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        assert!(sdk_stats.is_directory(), "sdk must be a directory");

        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_stats.is_directory(),
            "sdk/rust must be a directory (ENOTDIR bug)"
        );

        let toml_stats = overlay.lookup(rust_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml must be a file");

        // Also verify reading the file works
        let file = overlay.open(toml_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"[package]\nname = \"sdk\"");

        Ok(())
    }

    /// Test that files created in delta layer under a base directory are visible
    /// in readdir and can be deleted with unlink.
    ///
    /// This test reproduces a bug where:
    /// 1. Base has a directory (e.g., `.git/`)
    /// 2. A file is created in that directory via overlay (e.g., `.git/index.lock`)
    /// 3. `ensure_parent_dirs` creates `.git` in delta with origin mapping
    /// 4. But the overlay inode for `.git` still has `layer: Layer::Base`
    /// 5. readdir only checks delta if layer == Delta, so the new file is invisible
    /// 6. unlink only deletes from delta if parent layer == Delta, so deletion fails
    #[tokio::test]
    async fn test_overlay_readdir_and_unlink_delta_file_in_base_dir() -> Result<()> {
        // Setup: base has a .git directory with some files
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join(".git"))?;
        std::fs::write(base_dir.path().join(".git/config"), b"[core]\n")?;
        std::fs::write(base_dir.path().join(".git/HEAD"), b"ref: refs/heads/main")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup .git directory (creates Base layer mapping)
        let git_stats = overlay.lookup(ROOT_INO, ".git").await?.unwrap();
        assert!(git_stats.is_directory());

        // Step 2: Create a new file in .git (triggers ensure_parent_dirs)
        let (lock_stats, lock_file) = overlay
            .create_file(git_stats.ino, "index.lock", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        lock_file.pwrite(0, b"lock content").await?;
        assert!(lock_stats.is_file());

        // Step 3: Verify readdir shows the new file (BUG: was invisible)
        let entries = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"index.lock".to_string()),
            "readdir should show index.lock, got: {:?}",
            entries
        );
        // Also verify base files are still visible
        assert!(entries.contains(&"config".to_string()));
        assert!(entries.contains(&"HEAD".to_string()));

        // Step 4: Verify lookup also works
        let lookup_stats = overlay.lookup(git_stats.ino, "index.lock").await?.unwrap();
        assert!(lookup_stats.is_file());

        // Step 5: Delete the file
        overlay.unlink(git_stats.ino, "index.lock").await?;

        // Step 6: Verify the file is actually gone (BUG: persisted after unlink)
        let deleted = overlay.lookup(git_stats.ino, "index.lock").await?;
        assert!(
            deleted.is_none(),
            "index.lock should be deleted, but lookup still finds it"
        );

        // Also verify readdir no longer shows it
        let entries_after = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            !entries_after.contains(&"index.lock".to_string()),
            "readdir should not show index.lock after deletion"
        );

        // Base files should still be there
        assert!(entries_after.contains(&"config".to_string()));
        assert!(entries_after.contains(&"HEAD".to_string()));

        Ok(())
    }

    /// Test readdir_plus also shows delta files in base directories.
    #[tokio::test]
    async fn test_overlay_readdir_plus_delta_file_in_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("mydir"))?;
        std::fs::write(base_dir.path().join("mydir/base.txt"), b"base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup the directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "mydir").await?.unwrap();

        // Create a file in the directory
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // readdir_plus should show both base and delta files
        let entries = overlay.readdir_plus(dir_stats.ino).await?.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"base.txt"),
            "readdir_plus should show base.txt"
        );
        assert!(
            names.contains(&"delta.txt"),
            "readdir_plus should show delta.txt"
        );

        Ok(())
    }

    /// After remount, origin mappings can leave overlay inodes tagged as
    /// Layer::Base with stale base inode numbers. Verify that base files
    /// in directories with origin mappings remain accessible.
    #[tokio::test]
    async fn test_overlay_base_file_accessible_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/keep.txt"), b"keep")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and verify base files are still accessible
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let keep = overlay.lookup(dir_stats.ino, "keep.txt").await?;
        assert!(keep.is_some(), "keep.txt should be visible after remount");

        Ok(())
    }

    /// Test unlink of a BASE file after the parent directory has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario:
    /// 1. Base has /dir/base.txt and /dir/other.txt
    /// 2. Lookup /dir/ (creates Base layer mapping)
    /// 3. Create /dir/delta.txt (triggers ensure_parent_dirs, promotes /dir/ to Delta)
    /// 4. Unlink /dir/base.txt (base file in promoted parent)
    /// 5. base.txt should be gone (whiteout must be created)
    ///
    /// Bug: The base-walk loop in unlink() returns Ok(()) when a path component
    /// lookup fails in HostFS, skipping whiteout creation.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/other.txt"), b"other content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup the directory (creates Base layer mapping)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        assert!(dir_stats.is_directory());

        // Step 2: Create a file in the directory (promotes /dir/ from Base to Delta)
        let (_delta_stats, delta_file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        delta_file.pwrite(0, b"delta content").await?;

        // Step 3: Unlink the BASE file
        overlay.unlink(dir_stats.ino, "base.txt").await?;

        // Step 4: Verify the base file is gone via lookup
        let deleted = overlay.lookup(dir_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink, but lookup still finds it"
        );

        // Step 5: Verify readdir no longer shows it
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );

        // Other files should still be visible
        assert!(entries.contains(&"other.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Unlink of a base file must create a whiteout even when the parent
    /// directory has a stale origin mapping from a previous session.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_whiteout_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and unlink the base file
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "base.txt").await?;
        assert!(
            overlay.lookup(dir_stats.ino, "base.txt").await?.is_none(),
            "base.txt should be whiteout-deleted after unlink"
        );

        Ok(())
    }

    /// Test unlink of a BASE file in a deeply nested promoted parent.
    ///
    /// Scenario: base has /a/b/file.txt, promote /a/b/ by creating a delta
    /// file there, then unlink /a/b/file.txt. The base-walk must resolve
    /// both "a" and "b" in the HostFS to find the base parent.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_nested_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b"))?;
        std::fs::write(base_dir.path().join("a/b/base.txt"), b"deep base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /a/b/ (creates Base layer mappings)
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();

        // Create a delta file in /a/b/ (promotes /a/ and /a/b/ to Delta)
        let (_stats, file) = overlay
            .create_file(b_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Unlink the base file
        overlay.unlink(b_stats.ino, "base.txt").await?;

        // Verify it's gone
        let deleted = overlay.lookup(b_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink in nested promoted parent"
        );

        let entries = overlay.readdir(b_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rmdir of a BASE directory after the parent has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario: base has /parent/emptydir/, promote /parent/ by creating a
    /// delta file, then rmdir /parent/emptydir/. The whiteout must be created
    /// so the directory doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rmdir_base_dir_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::create_dir(base_dir.path().join("parent/emptydir"))?;
        std::fs::write(base_dir.path().join("parent/keep.txt"), b"keep")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup parent directory (Base layer)
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Lookup emptydir so overlay knows about it
        let emptydir_stats = overlay.lookup(parent_stats.ino, "emptydir").await?.unwrap();
        assert!(emptydir_stats.is_directory());

        // Create a delta file in /parent/ (promotes /parent/ to Delta)
        let (_stats, file) = overlay
            .create_file(parent_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // rmdir the base directory
        overlay.rmdir(parent_stats.ino, "emptydir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "emptydir").await?;
        assert!(
            deleted.is_none(),
            "emptydir should be deleted after rmdir, but lookup still finds it"
        );

        let entries = overlay.readdir(parent_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"emptydir".to_string()),
            "readdir should not show emptydir after rmdir, got: {:?}",
            entries
        );
        assert!(entries.contains(&"keep.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rename of a BASE file creates a whiteout at the source when the
    /// parent directory has been promoted from Base to Delta layer.
    ///
    /// Scenario: base has /dir/original.txt, promote /dir/ by creating a delta
    /// file, rename /dir/original.txt to /dir/renamed.txt. The source path
    /// must get a whiteout so original.txt doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rename_base_file_whiteout_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/original.txt"), b"original")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        // Lookup original.txt so overlay has the inode
        let orig_stats = overlay
            .lookup(dir_stats.ino, "original.txt")
            .await?
            .unwrap();
        assert!(orig_stats.is_file());

        // Create a delta file to promote /dir/ from Base to Delta
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Rename the base file within the same promoted directory
        overlay
            .rename(dir_stats.ino, "original.txt", dir_stats.ino, "renamed.txt")
            .await?;

        // Verify original.txt is gone (whiteout must exist)
        let deleted = overlay.lookup(dir_stats.ino, "original.txt").await?;
        assert!(
            deleted.is_none(),
            "original.txt should be gone after rename, but lookup still finds it"
        );

        // Verify renamed.txt exists
        let renamed = overlay.lookup(dir_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        // Verify readdir shows the right state
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"original.txt".to_string()),
            "readdir should not show original.txt after rename, got: {:?}",
            entries
        );
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// After remount, unlink must clean up both the delta entry and create
    /// a whiteout for the base entry — even when the parent is tagged Delta
    /// rather than Base.
    #[tokio::test]
    async fn test_overlay_unlink_removes_delta_entry_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/file.txt"), b"original base")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: copy-up file.txt to delta via write
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let file_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let file = overlay.open(file_stats.ino, libc::O_WRONLY).await?;
        file.pwrite(0, b"modified in delta").await?;

        // Session 2: remount, unlink, recreate, verify new content
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "file.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "file.txt").await?.is_none());

        let (_stats, new_file) = overlay
            .create_file(dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        new_file.pwrite(0, b"brand new content").await?;

        let read_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let read_file = overlay.open(read_stats.ino, libc::O_RDONLY).await?;
        let content = read_file.pread(0, 1024).await?;
        assert_eq!(std::str::from_utf8(&content).unwrap(), "brand new content");

        Ok(())
    }

    /// Hard-link copy-up in session 1, then unlink source in session 2.
    /// The link target must survive even though the parent has a stale
    /// origin mapping.
    #[tokio::test]
    async fn test_overlay_link_copy_up_then_unlink_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/src.txt"), b"link source")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: hard-link triggers copy_up of src.txt
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let src_stats = overlay.lookup(dir_stats.ino, "src.txt").await?.unwrap();
        overlay
            .link(src_stats.ino, dir_stats.ino, "dst.txt")
            .await?;

        // Session 2: remount, unlink source, verify link survives
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "src.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "src.txt").await?.is_none());
        assert!(overlay.lookup(dir_stats.ino, "dst.txt").await?.is_some());

        Ok(())
    }

    /// Test rename of base file across directories where both parents have
    /// been promoted. Source directory must get a whiteout for the original
    /// file, even though the base-walk must resolve through promoted parents.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_promoted_parents() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/moveme.txt"), b"moving")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup both directories
        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();

        // Promote /src/ by creating a delta file
        let (_s, f) = overlay
            .create_file(src_stats.ino, "trigger1.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Promote /dst/ by creating a delta file
        let (_s, f) = overlay
            .create_file(dst_stats.ino, "trigger2.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Lookup moveme.txt so overlay knows it
        let moveme = overlay.lookup(src_stats.ino, "moveme.txt").await?.unwrap();
        assert!(moveme.is_file());

        // Rename across promoted parents
        overlay
            .rename(src_stats.ino, "moveme.txt", dst_stats.ino, "moved.txt")
            .await?;

        // Source must be gone (whiteout at /src/moveme.txt)
        let src_lookup = overlay.lookup(src_stats.ino, "moveme.txt").await?;
        assert!(
            src_lookup.is_none(),
            "moveme.txt should be gone from /src/ after rename"
        );

        // Destination must exist
        let dst_lookup = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            dst_lookup.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        // readdir /src/ should not show moveme.txt
        let src_entries = overlay.readdir(src_stats.ino).await?.unwrap();
        assert!(
            !src_entries.contains(&"moveme.txt".to_string()),
            "readdir /src/ should not show moveme.txt, got: {:?}",
            src_entries
        );

        Ok(())
    }

    /// Test rename of a BASE file in a deeply nested directory that has not
    /// been promoted to Delta.
    ///
    /// Scenario: base has /deep/nested/file.txt, lookup the path (Base layer
    /// only, no promotion), then rename file.txt within the same directory.
    /// The delta parent must be resolved after copy_up creates it.
    #[tokio::test]
    async fn test_overlay_rename_base_file_delta_src_parent_before_copyup() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("deep/nested"))?;
        std::fs::write(base_dir.path().join("deep/nested/file.txt"), b"content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /deep/nested/ (creates Base layer mappings)
        let deep_stats = overlay.lookup(ROOT_INO, "deep").await?.unwrap();
        let nested_stats = overlay.lookup(deep_stats.ino, "nested").await?.unwrap();
        let file_stats = overlay.lookup(nested_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Rename within same directory — parents only exist in base
        overlay
            .rename(
                nested_stats.ino,
                "file.txt",
                nested_stats.ino,
                "renamed.txt",
            )
            .await?;

        let renamed = overlay.lookup(nested_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        let original = overlay.lookup(nested_stats.ino, "file.txt").await?;
        assert!(original.is_none(), "file.txt should be gone after rename");

        let entries = overlay.readdir(nested_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt, got: {:?}",
            entries
        );
        assert!(
            !entries.contains(&"file.txt".to_string()),
            "readdir should not show file.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// Test rename of a BASE file across directories when neither parent has
    /// been promoted to Delta.
    ///
    /// Scenario: base has /src/base.txt and /dst/, neither exists in delta.
    /// Rename /src/base.txt to /dst/moved.txt. Both source and destination
    /// delta parents must be correctly resolved after copy_up.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_dirs_no_found_guard() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/base.txt"), b"source content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();
        let file_stats = overlay.lookup(src_stats.ino, "base.txt").await?.unwrap();
        assert!(file_stats.is_file());
        overlay
            .rename(src_stats.ino, "base.txt", dst_stats.ino, "moved.txt")
            .await?;

        let moved = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            moved.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        let original = overlay.lookup(src_stats.ino, "base.txt").await?;
        assert!(
            original.is_none(),
            "base.txt should be gone from /src/ after rename"
        );

        let file = overlay.open(moved.unwrap().ino, libc::O_RDONLY).await?;
        let data = file.pread(0, 1024).await?;
        assert_eq!(data, b"source content");

        Ok(())
    }

    /// Test unlink of a delta-only file does not create a spurious whiteout.
    ///
    /// Scenario: base has /dir/ (empty), create delta_only.txt in delta,
    /// unlink it, then recreate with the same name. The recreated file must
    /// be visible — no whiteout should have been left behind.
    #[tokio::test]
    async fn test_overlay_unlink_delta_only_file_no_spurious_whiteout() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta only").await?;

        overlay.unlink(dir_stats.ino, "delta_only.txt").await?;

        let deleted = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            deleted.is_none(),
            "delta_only.txt should be gone after unlink"
        );

        // Recreate with the same name
        let (_stats2, file2) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file2.pwrite(0, b"recreated").await?;

        let recreated = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            recreated.is_some(),
            "recreated delta_only.txt should be visible (no spurious whiteout)"
        );

        let f = overlay.open(recreated.unwrap().ino, libc::O_RDONLY).await?;
        let data = f.pread(0, 1024).await?;
        assert_eq!(data, b"recreated");

        Ok(())
    }

    /// Test rmdir works for directories created in delta under base parent.
    #[tokio::test]
    async fn test_overlay_rmdir_delta_dir_in_base_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::write(base_dir.path().join("parent/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup base directory
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Create a subdirectory in delta
        let subdir_stats = overlay
            .mkdir(parent_stats.ino, "newsubdir", 0o755, 0, 0)
            .await?;
        assert!(subdir_stats.is_directory());

        // Verify it exists
        let lookup = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(lookup.is_some());

        // Delete it with rmdir
        overlay.rmdir(parent_stats.ino, "newsubdir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(deleted.is_none(), "newsubdir should be deleted after rmdir");

        Ok(())
    }
}
