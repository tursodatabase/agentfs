use anyhow::Result;
use async_trait::async_trait;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[cfg(unix)]
use libc;

use super::{BoxedFile, DirEntry, File, FileSystem, FilesystemStats, Stats};
use std::sync::Arc;

/// A filesystem backed by a host directory (passthrough)
#[derive(Clone)]
pub struct HostFS {
    root: PathBuf,
    // If the HostFS is mounted as an overlay underfilesystem via FUSE, we need to know
    // the inode of the FUSE mountpoint to avoid deadlock.
    //
    // For example, if we overlay `/` and mount it at /mntpnt, then when we do an `ls /mntpnt`,
    // inside the readdir_plus handler (which grabs a global fuser lock),
    // agentfs reads `/`, and sees the direntry `mntpnt`, and then stats `/mntpnt`, which would enter a new fuser handler
    // trying to grab the fuser lock, causing a deadlock.
    //
    // The linux kernel overlay filesystem does not have this problem because it has access to the under filesystem
    // directly, and it does not cross mountpoints, so it will just return /mntpnt as a raw direntry of the underlying ext4/xfs
    // filesystem and does not call into the overlay layer at all. We do not have this access, so we have to special-case ourselves.
    //
    // We only need to compare the root inode, because for anything under /mntpnt, the access is gated by the fuse layer, and we block
    // looking up ourselves in the fuse layer, but `ls /mntpnt` has to pass the fuse layer and come to us.
    #[cfg(target_family = "unix")]
    fuse_mountpoint_inode: Option<u64>,
}

/// An open file handle for HostFS.
pub struct HostFSFile {
    /// The resolved full path on the host filesystem.
    full_path: PathBuf,
    /// The virtual path (for generating consistent inode numbers in fstat).
    virtual_path: String,
}

#[async_trait]
impl File for HostFSFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let mut file = fs::File::open(&self.full_path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.full_path)
            .await?;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(())
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&self.full_path)
            .await?;
        file.set_len(size).await?;
        Ok(())
    }

    async fn fsync(&self) -> Result<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&self.full_path)
            .await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn fstat(&self) -> Result<Stats> {
        let metadata = fs::metadata(&self.full_path).await?;
        Ok(HostFS::metadata_to_stats(&metadata, &self.virtual_path))
    }
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
        Ok(Self {
            root,
            fuse_mountpoint_inode: None,
        })
    }

    /// Create a new HostFS rooted at the given directory with a FUSE mountpoint inode
    #[cfg(target_family = "unix")]
    pub fn with_fuse_mountpoint(mut self, inode: u64) -> Self {
        self.fuse_mountpoint_inode = Some(inode);
        self
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

    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let full_path = self.resolve_path(path);
        let mut entries = Vec::new();

        let mut dir = match fs::read_dir(&full_path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // Not a directory - return None (path exists but has no children)
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            #[cfg(target_family = "unix")]
            {
                // We won't be able to stat the mountpoint, so better not even expose it
                if let Some(inode) = self.fuse_mountpoint_inode {
                    if entry.ino() == inode {
                        continue;
                    }
                }
            }

            if let Some(name) = entry.file_name().to_str() {
                entries.push(name.to_string());
            }
        }
        entries.sort();
        Ok(Some(entries))
    }

    async fn readdir_plus(&self, path: &str) -> Result<Option<Vec<DirEntry>>> {
        let full_path = self.resolve_path(path);
        let mut entries = Vec::new();

        let mut dir = match fs::read_dir(&full_path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            #[cfg(target_os = "linux")]
            {
                if let Some(inode) = self.fuse_mountpoint_inode {
                    if entry.ino() == inode {
                        continue;
                    }
                }
            }

            if let Some(name) = entry.file_name().to_str() {
                // Build the virtual path for this entry
                let entry_path = if path == "/" {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", path.trim_end_matches('/'), name)
                };

                // Get metadata for the entry
                if let Ok(metadata) = entry.metadata().await {
                    let stats = Self::metadata_to_stats(&metadata, &entry_path);
                    entries.push(DirEntry {
                        name: name.to_string(),
                        stats,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
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

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let full_path = self.resolve_path(path);
        let permissions = std::fs::Permissions::from_mode(mode);
        fs::set_permissions(&full_path, permissions).await?;
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

    async fn open(&self, path: &str) -> Result<BoxedFile> {
        let full_path = self.resolve_path(path);
        // Verify the file exists
        if !full_path.exists() {
            anyhow::bail!("File not found: {}", path);
        }
        Ok(Arc::new(HostFSFile {
            full_path,
            virtual_path: path.to_string(),
        }))
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

        // Open file handle
        let file = fs.open("/test.txt").await?;

        // pread via file handle
        let data = file.pread(6, 5).await?;
        assert_eq!(data, b"world");

        // pwrite via file handle
        file.pwrite(6, b"rust!").await?;
        let data = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(data, b"hello rust!");

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_chmod() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create a file
        fs.write_file("/test.txt", b"content").await?;

        // Change to executable
        fs.chmod("/test.txt", 0o755).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o777,
            0o755,
            "Mode should be 0o755 after chmod"
        );

        // Change to read-only
        fs.chmod("/test.txt", 0o444).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o777,
            0o444,
            "Mode should be 0o444 after chmod"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_chmod_directory() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create a directory
        fs.mkdir("/subdir").await?;

        // Change permissions
        fs.chmod("/subdir", 0o700).await?;

        let stats = fs.stat("/subdir").await?.unwrap();
        assert_eq!(stats.mode & 0o777, 0o700, "Directory mode should be 0o700");
        assert!(stats.is_directory(), "Should still be a directory");

        Ok(())
    }
}
