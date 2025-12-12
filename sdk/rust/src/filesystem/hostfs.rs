use anyhow::Result;
use async_trait::async_trait;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{FileSystem, FilesystemStats, Stats};

/// A filesystem backed by a host directory (passthrough)
#[derive(Clone)]
pub struct HostFS {
    root: PathBuf,
}

impl HostFS {
    /// Create a new HostFS rooted at the given directory
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.exists() {
            anyhow::bail!("Root directory does not exist: {}", root.display());
        }
        if !root.is_dir() {
            anyhow::bail!("Root path is not a directory: {}", root.display());
        }
        Ok(Self { root })
    }

    /// Get the root directory
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Resolve a virtual path to a host path
    fn resolve_path(&self, path: &str) -> PathBuf {
        let normalized = path.trim_start_matches('/');
        if normalized.is_empty() {
            self.root.clone()
        } else {
            self.root.join(normalized)
        }
    }

    /// Convert std::fs::Metadata to Stats
    fn metadata_to_stats(metadata: &std::fs::Metadata, path: &str) -> Stats {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Generate synthetic inode from path (host inodes may not be meaningful)
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let ino = (hasher.finish() as i64).abs();

        Stats {
            ino,
            mode: metadata.mode(),
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.size() as i64,
            atime: metadata.atime(),
            mtime: metadata.mtime(),
            ctime: metadata.ctime(),
        }
    }
}

#[async_trait]
impl FileSystem for HostFS {
    async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let full_path = self.resolve_path(path);
        match fs::metadata(&full_path).await {
            Ok(metadata) => Ok(Some(Self::metadata_to_stats(&metadata, path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let full_path = self.resolve_path(path);
        match fs::symlink_metadata(&full_path).await {
            Ok(metadata) => Ok(Some(Self::metadata_to_stats(&metadata, path))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let full_path = self.resolve_path(path);
        match fs::read(&full_path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let full_path = self.resolve_path(path);
        fs::write(&full_path, data).await?;
        Ok(())
    }

    async fn pread(&self, path: &str, offset: u64, size: u64) -> Result<Option<Vec<u8>>> {
        let full_path = self.resolve_path(path);
        let mut file = match fs::File::open(&full_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read(&mut buf).await?;
        buf.truncate(n);
        Ok(Some(buf))
    }

    async fn pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let full_path = self.resolve_path(path);

        // Open file for writing, create if doesn't exist
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&full_path)
            .await?;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        Ok(())
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let full_path = self.resolve_path(path);
        let file = fs::File::open(&full_path).await?;
        file.set_len(size).await?;
        Ok(())
    }

    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let full_path = self.resolve_path(path);
        let mut entries = Vec::new();

        let mut dir = match fs::read_dir(&full_path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                entries.push(name.to_string());
            }
        }
        entries.sort();
        Ok(Some(entries))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let full_path = self.resolve_path(path);
        fs::create_dir(&full_path).await?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let full_path = self.resolve_path(path);
        let metadata = fs::symlink_metadata(&full_path).await?;

        if metadata.is_dir() {
            fs::remove_dir(&full_path).await?;
        } else {
            fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from_path = self.resolve_path(from);
        let to_path = self.resolve_path(to);
        fs::rename(&from_path, &to_path).await?;
        Ok(())
    }

    async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        let full_path = self.resolve_path(linkpath);
        tokio::fs::symlink(target, &full_path).await?;
        Ok(())
    }

    async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let full_path = self.resolve_path(path);
        match fs::read_link(&full_path).await {
            Ok(target) => Ok(Some(target.to_string_lossy().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        // Count files and total size by walking the directory
        let mut inodes = 0u64;
        let mut bytes_used = 0u64;

        fn count_recursive(
            path: &std::path::Path,
            inodes: &mut u64,
            bytes: &mut u64,
        ) -> std::io::Result<()> {
            if path.is_dir() {
                *inodes += 1;
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    count_recursive(&entry.path(), inodes, bytes)?;
                }
            } else {
                *inodes += 1;
                if let Ok(meta) = path.metadata() {
                    *bytes += meta.len();
                }
            }
            Ok(())
        }

        let _ = count_recursive(&self.root, &mut inodes, &mut bytes_used);

        Ok(FilesystemStats { inodes, bytes_used })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_hostfs_basic() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Write a file
        fs.write_file("/test.txt", b"hello world").await?;

        // Read it back
        let data = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(data, b"hello world");

        // Stat it
        let stats = fs.stat("/test.txt").await?.unwrap();
        assert!(stats.is_file());
        assert_eq!(stats.size, 11);

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_mkdir_readdir() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create directory
        fs.mkdir("/subdir").await?;

        // Create files
        fs.write_file("/subdir/a.txt", b"a").await?;
        fs.write_file("/subdir/b.txt", b"b").await?;

        // List directory
        let entries = fs.readdir("/subdir").await?.unwrap();
        assert_eq!(entries, vec!["a.txt", "b.txt"]);

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_pread_pwrite() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Write initial data
        fs.write_file("/test.txt", b"hello world").await?;

        // pread
        let data = fs.pread("/test.txt", 6, 5).await?.unwrap();
        assert_eq!(data, b"world");

        // pwrite
        fs.pwrite("/test.txt", 6, b"rust!").await?;
        let data = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(data, b"hello rust!");

        Ok(())
    }
}
