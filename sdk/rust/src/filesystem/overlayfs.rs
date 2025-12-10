use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use turso::Value;

use super::agentfs::AgentFS;
use super::{FileSystem, FilesystemStats, FsError, Stats};

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
}

impl OverlayFS {
    /// Create a new overlay filesystem
    pub fn new(base: Arc<dyn FileSystem>, delta: AgentFS) -> Self {
        Self { base, delta }
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
    fn normalize_path(&self, path: &str) -> String {
        let normalized = path.trim_end_matches('/');
        if normalized.is_empty() {
            "/".to_string()
        } else if !normalized.starts_with('/') {
            format!("/{}", normalized)
        } else {
            normalized.to_string()
        }
    }

    /// Check if a path has a whiteout (is deleted from base)
    ///
    /// This also checks parent directories - if /foo is whiteout,
    /// then /foo/bar is also considered deleted.
    async fn is_whiteout(&self, path: &str) -> Result<bool> {
        let normalized = self.normalize_path(path);
        let conn = self.delta.get_connection();

        // Check the path itself and all parent paths
        let mut check_path = normalized.clone();
        loop {
            let result = conn
                .query(
                    "SELECT 1 FROM fs_whiteout WHERE path = ?",
                    (check_path.as_str(),),
                )
                .await;

            // Handle case where fs_whiteout table doesn't exist
            let mut rows = match result {
                Ok(rows) => rows,
                Err(_) => return Ok(false), // Table doesn't exist, no whiteouts
            };

            if rows.next().await?.is_some() {
                return Ok(true);
            }

            // Check parent directory
            if let Some(parent_end) = check_path.rfind('/') {
                if parent_end == 0 {
                    // We've reached root
                    break;
                }
                check_path = check_path[..parent_end].to_string();
            } else {
                break;
            }
        }
        Ok(false)
    }

    /// Create a whiteout for a path (marks it as deleted from base)
    async fn create_whiteout(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);
        let conn = self.delta.get_connection();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        conn.execute(
            "INSERT INTO fs_whiteout (path, created_at) VALUES (?, ?)
             ON CONFLICT(path) DO UPDATE SET created_at = excluded.created_at",
            (normalized.as_str(), now),
        )
        .await?;
        Ok(())
    }

    /// Remove a whiteout (un-delete a path)
    async fn remove_whiteout(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);
        let conn = self.delta.get_connection();

        conn.execute(
            "DELETE FROM fs_whiteout WHERE path = ?",
            (normalized.as_str(),),
        )
        .await?;
        Ok(())
    }

    /// Get all whiteouts that are direct children of a directory
    async fn get_child_whiteouts(&self, dir_path: &str) -> Result<HashSet<String>> {
        let normalized = self.normalize_path(dir_path);
        let prefix = if normalized == "/" {
            "/".to_string()
        } else {
            format!("{}/", normalized)
        };

        let conn = self.delta.get_connection();
        let mut whiteouts = HashSet::new();

        let mut rows = conn
            .query(
                "SELECT path FROM fs_whiteout WHERE path LIKE ?",
                (format!("{}%", prefix),),
            )
            .await?;

        while let Some(row) = rows.next().await? {
            if let Ok(Value::Text(p)) = row.get_value(0) {
                // Extract immediate child name
                let suffix = p.strip_prefix(&prefix).unwrap_or(&p);
                if let Some(name) = suffix.split('/').next() {
                    if !name.is_empty() {
                        whiteouts.insert(name.to_string());
                    }
                }
            }
        }
        Ok(whiteouts)
    }

    /// Ensure parent directories exist in delta layer
    async fn ensure_parent_dirs(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);
        let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

        let mut current = String::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current = format!("{}/{}", current, component);

            // Check if directory exists in delta
            if self.delta.stat(&current).await?.is_none() {
                // Remove any whiteout for this path
                self.remove_whiteout(&current).await?;

                // Create directory in delta (ignore if already exists)
                let _ = self.delta.mkdir(&current).await;
            }
        }
        Ok(())
    }

    /// Check if a path exists in delta layer
    async fn exists_in_delta(&self, path: &str) -> Result<bool> {
        Ok(self.delta.stat(path).await?.is_some())
    }
}

#[async_trait]
impl FileSystem for OverlayFS {
    async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let normalized = self.normalize_path(path);

        // Check for whiteout first
        if self.is_whiteout(&normalized).await? {
            return Ok(None);
        }

        // Check delta first - this is authoritative for files in delta
        if let Some(stats) = self.delta.stat(&normalized).await? {
            return Ok(Some(stats));
        }

        // Fall back to base, but fix up the inode for root
        if let Some(mut stats) = self.base.stat(&normalized).await? {
            // Root directory must have inode 1 for FUSE compatibility
            if normalized == "/" {
                stats.ino = 1;
            }
            return Ok(Some(stats));
        }

        Ok(None)
    }

    async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized).await? {
            return Ok(None);
        }

        // Check delta first
        if let Some(stats) = self.delta.lstat(&normalized).await? {
            return Ok(Some(stats));
        }

        // Fall back to base, but fix up the inode for root
        if let Some(mut stats) = self.base.lstat(&normalized).await? {
            // Root directory must have inode 1 for FUSE compatibility
            if normalized == "/" {
                stats.ino = 1;
            }
            return Ok(Some(stats));
        }

        Ok(None)
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized).await? {
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

        // Ensure parent directories exist in delta
        self.ensure_parent_dirs(&normalized).await?;

        // Write to delta
        self.delta.write_file(&normalized, data).await
    }

    async fn pread(&self, path: &str, offset: u64, size: u64) -> Result<Option<Vec<u8>>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized).await? {
            return Ok(None);
        }

        // Check if file exists in delta
        if self.exists_in_delta(&normalized).await? {
            return self.delta.pread(&normalized, offset, size).await;
        }

        // Read from base
        self.base.pread(&normalized, offset, size).await
    }

    async fn pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Copy-on-write: if file exists in base but not delta, copy it first
        if !self.exists_in_delta(&normalized).await? {
            if let Some(base_data) = self.base.read_file(&normalized).await? {
                self.ensure_parent_dirs(&normalized).await?;
                self.delta.write_file(&normalized, &base_data).await?;
            }
        }

        // Remove any whiteout
        self.remove_whiteout(&normalized).await?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(&normalized).await?;

        // Write to delta
        self.delta.pwrite(&normalized, offset, data).await
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Copy-on-write: if file exists in base but not delta, copy it first
        if !self.exists_in_delta(&normalized).await? {
            if let Some(base_data) = self.base.read_file(&normalized).await? {
                self.ensure_parent_dirs(&normalized).await?;
                self.delta.write_file(&normalized, &base_data).await?;
            } else {
                return Err(FsError::NotFound.into());
            }
        }

        // Truncate in delta
        self.delta.truncate(&normalized, size).await
    }

    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let normalized = self.normalize_path(path);

        // Check for whiteout on directory itself
        if self.is_whiteout(&normalized).await? {
            return Ok(None);
        }

        // Get whiteouts for children
        let child_whiteouts = self.get_child_whiteouts(&normalized).await?;

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

    async fn mkdir(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Check if already exists (in either layer, not whiteout)
        if !self.is_whiteout(&normalized).await?
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
        self.delta.mkdir(&normalized).await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let normalized = self.normalize_path(path);

        // Try to remove from delta
        let removed_from_delta = self.delta.remove(&normalized).await.is_ok();

        // Check if it exists in base (and not already whiteout)
        let exists_in_base = if self.is_whiteout(&normalized).await? {
            false
        } else {
            self.base.stat(&normalized).await?.is_some()
        };

        // If exists in base, create whiteout
        if exists_in_base {
            self.create_whiteout(&normalized).await?;
        } else if !removed_from_delta {
            return Err(FsError::NotFound.into());
        }

        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from_normalized = self.normalize_path(from);
        let to_normalized = self.normalize_path(to);

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

    async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let normalized = self.normalize_path(path);

        if self.is_whiteout(&normalized).await? {
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

        // Initialize whiteout table
        delta
            .get_connection()
            .execute(
                "CREATE TABLE IF NOT EXISTS fs_whiteout (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
                (),
            )
            .await?;

        let overlay = OverlayFS::new(base, delta);
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

        // Modify base file via pwrite (should copy to delta first)
        overlay.pwrite("/base.txt", 0, b"modified").await?;

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
}
