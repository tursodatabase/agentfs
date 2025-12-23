//! NFS server adapter for AgentFS.
//!
//! This module implements nfsserve's NFSFileSystem trait on top of AgentFS's
//! FileSystem trait, enabling systems to mount AgentFS via NFS without requiring
//! FUSE or other system extensions.

use std::collections::HashMap;
use std::sync::Arc;

use agentfs_sdk::{FileSystem, Stats, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tokio::sync::{Mutex, RwLock};

/// Root directory inode number
const ROOT_INO: fileid3 = 1;

/// NFS adapter that wraps an AgentFS FileSystem.
pub struct AgentNFS {
    /// The underlying filesystem (wrapped in Mutex to serialize operations)
    fs: Arc<Mutex<dyn FileSystem>>,
    /// Inode-to-path mapping (async RwLock for use in async methods)
    inode_map: RwLock<InodeMap>,
    /// User ID for all files
    uid: u32,
    /// Group ID for all files
    gid: u32,
}

/// Bidirectional mapping between inodes and paths.
struct InodeMap {
    /// Path to inode
    path_to_ino: HashMap<String, fileid3>,
    /// Inode to path
    ino_to_path: HashMap<fileid3, String>,
    /// Next available inode number
    next_ino: fileid3,
}

impl InodeMap {
    fn new() -> Self {
        let mut map = InodeMap {
            path_to_ino: HashMap::new(),
            ino_to_path: HashMap::new(),
            next_ino: ROOT_INO + 1,
        };
        // Root directory is always inode 1
        map.path_to_ino.insert("/".to_string(), ROOT_INO);
        map.ino_to_path.insert(ROOT_INO, "/".to_string());
        map
    }

    fn get_or_create_ino(&mut self, path: &str) -> fileid3 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(path.to_string(), ino);
        self.ino_to_path.insert(ino, path.to_string());
        ino
    }

    fn get_path(&self, ino: fileid3) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    fn remove_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }

    fn rename_path(&mut self, from: &str, to: &str) {
        if let Some(ino) = self.path_to_ino.remove(from) {
            self.ino_to_path.insert(ino, to.to_string());
            self.path_to_ino.insert(to.to_string(), ino);
        }
    }
}

impl AgentNFS {
    /// Create a new NFS adapter wrapping the given filesystem.
    pub fn new(fs: Arc<Mutex<dyn FileSystem>>, uid: u32, gid: u32) -> Self {
        AgentNFS {
            fs,
            inode_map: RwLock::new(InodeMap::new()),
            uid,
            gid,
        }
    }

    /// Convert AgentFS Stats to NFS fattr3.
    fn stats_to_fattr(&self, stats: &Stats, ino: fileid3) -> fattr3 {
        let ftype = match stats.mode & S_IFMT {
            S_IFREG => ftype3::NF3REG,
            S_IFDIR => ftype3::NF3DIR,
            S_IFLNK => ftype3::NF3LNK,
            _ => ftype3::NF3REG,
        };

        fattr3 {
            ftype,
            mode: stats.mode & 0o7777,
            nlink: stats.nlink,
            uid: self.uid,
            gid: self.gid,
            size: stats.size as u64,
            used: stats.size as u64,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: ino,
            atime: nfstime3 {
                seconds: stats.atime as u32,
                nseconds: 0,
            },
            mtime: nfstime3 {
                seconds: stats.mtime as u32,
                nseconds: 0,
            },
            ctime: nfstime3 {
                seconds: stats.ctime as u32,
                nseconds: 0,
            },
        }
    }

    /// Get path for an inode, returning NOENT error if not found.
    async fn get_path(&self, ino: fileid3) -> Result<String, nfsstat3> {
        self.inode_map
            .read()
            .await
            .get_path(ino)
            .ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    /// Join parent path and filename into a full path.
    fn join_path(parent: &str, name: &str) -> String {
        if parent == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent, name)
        }
    }
}

#[async_trait]
impl NFSFileSystem for AgentNFS {
    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Handle . and ..
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            let parent_path = if dir_path == "/" {
                "/".to_string()
            } else {
                std::path::Path::new(&dir_path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "/".to_string())
            };
            let parent_path = if parent_path.is_empty() {
                "/".to_string()
            } else {
                parent_path
            };
            return Ok(self.inode_map.write().await.get_or_create_ino(&parent_path));
        }

        let full_path = Self::join_path(&dir_path, name);

        // Lock filesystem for the duration of these operations
        let fs = self.fs.lock().await;

        // Check if path exists
        let stats = fs
            .lstat(&full_path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        // Verify parent is a directory
        let dir_stats = fs
            .lstat(&dir_path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        drop(fs); // Release lock before acquiring inode_map lock

        if !dir_stats.is_directory() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        let _ = stats; // We just needed to verify it exists
        Ok(self.inode_map.write().await.get_or_create_ino(&full_path))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let path = self.get_path(id).await?;
        let fs = self.fs.lock().await;
        let stats = fs
            .lstat(&path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats, id))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.get_path(id).await?;

        // Handle size change (truncate)
        if let nfsserve::nfs::set_size3::size(size) = setattr.size {
            let fs = self.fs.lock().await;
            let file = fs.open(&path).await.map_err(|_| nfsstat3::NFS3ERR_IO)?;
            file.truncate(size)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        // Return updated attributes
        self.getattr(id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.get_path(id).await?;
        let fs = self.fs.lock().await;

        let file = fs.open(&path).await.map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        let data = file
            .pread(offset, count as u64)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        // Check if we've reached EOF
        let stats = file.fstat().await.map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let eof = offset + data.len() as u64 >= stats.size as u64;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let path = self.get_path(id).await?;

        {
            let fs = self.fs.lock().await;
            let file = fs.open(&path).await.map_err(|_| nfsstat3::NFS3ERR_IO)?;
            file.pwrite(offset, data)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        self.getattr(id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        // Create empty file
        {
            let fs = self.fs.lock().await;
            fs.write_file(&full_path, &[])
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        let ino = self.inode_map.write().await.get_or_create_ino(&full_path);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        let fs = self.fs.lock().await;

        // Check if file already exists
        if fs
            .lstat(&full_path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .is_some()
        {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }

        // Create empty file
        fs.write_file(&full_path, &[])
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        drop(fs);
        Ok(self.inode_map.write().await.get_or_create_ino(&full_path))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        {
            let fs = self.fs.lock().await;
            fs.mkdir(&full_path)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        let ino = self.inode_map.write().await.get_or_create_ino(&full_path);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        {
            let fs = self.fs.lock().await;
            fs.remove(&full_path)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        self.inode_map.write().await.remove_path(&full_path);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_dir = self.get_path(from_dirid).await?;
        let to_dir = self.get_path(to_dirid).await?;
        let from_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let from_path = Self::join_path(&from_dir, from_name);
        let to_path = Self::join_path(&to_dir, to_name);

        {
            let fs = self.fs.lock().await;
            fs.rename(&from_path, &to_path)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        self.inode_map
            .write()
            .await
            .rename_path(&from_path, &to_path);
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir_path = self.get_path(dirid).await?;

        let entries = {
            let fs = self.fs.lock().await;
            fs.readdir_plus(&dir_path)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?
        };

        let mut result = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };

        // Find start position if start_after is specified
        let mut skip = start_after > 0;
        let mut skipped_count = 0;

        for entry in &entries {
            let entry_path = Self::join_path(&dir_path, &entry.name);
            let ino = self.inode_map.write().await.get_or_create_ino(&entry_path);

            if skip {
                if ino == start_after {
                    skip = false;
                }
                skipped_count += 1;
                continue;
            }

            if result.entries.len() >= max_entries {
                break;
            }

            result.entries.push(DirEntry {
                fileid: ino,
                name: entry.name.as_bytes().into(),
                attr: self.stats_to_fattr(&entry.stats, ino),
            });
        }

        // Mark as end if we've returned all remaining entries
        result.end = result.entries.len() + skipped_count >= entries.len();

        Ok(result)
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let name = std::str::from_utf8(linkname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        {
            let fs = self.fs.lock().await;
            fs.symlink(target, &full_path)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }

        let ino = self.inode_map.write().await.get_or_create_ino(&full_path);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let path = self.get_path(id).await?;

        let fs = self.fs.lock().await;
        let target = fs
            .readlink(&path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(target.into_bytes().into())
    }
}
