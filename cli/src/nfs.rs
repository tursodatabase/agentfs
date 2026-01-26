//! NFS server adapter for AgentFS.
//!
//! This module implements nfsserve's NFSFileSystem trait on top of AgentFS's
//! FileSystem trait, enabling systems to mount AgentFS via NFS without requiring
//! FUSE or other system extensions.

use std::collections::HashMap;
use std::sync::Arc;

use crate::nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_gid3, set_mode3,
    set_size3, set_uid3, specdata3,
};
use crate::nfsserve::vfs::{auth_unix, DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::FsError;
use agentfs_sdk::{
    FileSystem, Stats, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK,
};
use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

/// Root directory inode number
const ROOT_INO: fileid3 = 1;
/// Filesystem root inode (for underlying filesystem operations)
const FS_ROOT_INO: i64 = 1;

/// Convert an SDK error to an NFS status code.
///
/// Connection pool timeouts return NFS3ERR_JUKEBOX to signal the client
/// should retry the operation later. Other errors map to NFS3ERR_IO.
fn error_to_nfsstat(e: SdkError) -> nfsstat3 {
    match e {
        SdkError::Fs(ref fs_err) => match fs_err {
            FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
            FsError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            FsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
            FsError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
            FsError::IsADirectory => nfsstat3::NFS3ERR_ISDIR,
            FsError::NameTooLong => nfsstat3::NFS3ERR_NAMETOOLONG,
            FsError::RootOperation => nfsstat3::NFS3ERR_ACCES,
            _ => nfsstat3::NFS3ERR_IO,
        },
        SdkError::ConnectionPoolTimeout => nfsstat3::NFS3ERR_JUKEBOX,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

/// NFS adapter that wraps an AgentFS FileSystem.
pub struct AgentNFS {
    /// The underlying filesystem (wrapped in Mutex to serialize operations)
    fs: Arc<Mutex<dyn FileSystem>>,
    /// Inode-to-path mapping (async RwLock for use in async methods)
    inode_map: RwLock<InodeMap>,
}

/// Bidirectional mapping between inodes and paths.
struct InodeMap {
    /// Path to NFS inode
    path_to_ino: HashMap<String, fileid3>,
    /// NFS inode to path
    ino_to_path: HashMap<fileid3, String>,
    /// NFS inode to underlying filesystem inode
    ino_to_fs_ino: HashMap<fileid3, i64>,
    /// Next available NFS inode number
    next_ino: fileid3,
}

impl InodeMap {
    fn new() -> Self {
        let mut map = InodeMap {
            path_to_ino: HashMap::new(),
            ino_to_path: HashMap::new(),
            ino_to_fs_ino: HashMap::new(),
            next_ino: ROOT_INO + 1,
        };
        // Root directory is always inode 1
        map.path_to_ino.insert("/".to_string(), ROOT_INO);
        map.ino_to_path.insert(ROOT_INO, "/".to_string());
        map.ino_to_fs_ino.insert(ROOT_INO, FS_ROOT_INO);
        map
    }

    fn get_or_create_ino(&mut self, path: &str, fs_ino: i64) -> fileid3 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            // Update fs_ino mapping in case it changed
            self.ino_to_fs_ino.insert(ino, fs_ino);
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(path.to_string(), ino);
        self.ino_to_path.insert(ino, path.to_string());
        self.ino_to_fs_ino.insert(ino, fs_ino);
        ino
    }

    fn get_path(&self, ino: fileid3) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    fn get_fs_ino(&self, ino: fileid3) -> Option<i64> {
        self.ino_to_fs_ino.get(&ino).copied()
    }

    fn remove_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
            self.ino_to_fs_ino.remove(&ino);
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
    pub fn new(fs: Arc<Mutex<dyn FileSystem>>) -> Self {
        AgentNFS {
            fs,
            inode_map: RwLock::new(InodeMap::new()),
        }
    }

    /// Resolve a path to a filesystem inode by walking from root.
    async fn resolve_path_to_fs_ino(
        &self,
        fs: &tokio::sync::MutexGuard<'_, dyn FileSystem>,
        path: &str,
    ) -> Result<i64, nfsstat3> {
        if path == "/" {
            return Ok(FS_ROOT_INO);
        }

        let mut current_ino = FS_ROOT_INO;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            let stats = fs
                .lookup(current_ino, component)
                .await
                .map_err(error_to_nfsstat)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?;
            current_ino = stats.ino;
        }

        Ok(current_ino)
    }

    /// Convert AgentFS Stats to NFS fattr3.
    fn stats_to_fattr(&self, stats: &Stats, ino: fileid3) -> fattr3 {
        let ftype = match stats.mode & S_IFMT {
            S_IFREG => ftype3::NF3REG,
            S_IFDIR => ftype3::NF3DIR,
            S_IFLNK => ftype3::NF3LNK,
            S_IFIFO => ftype3::NF3FIFO,
            S_IFCHR => ftype3::NF3CHR,
            S_IFBLK => ftype3::NF3BLK,
            S_IFSOCK => ftype3::NF3SOCK,
            _ => ftype3::NF3REG,
        };

        // Extract major/minor from rdev for device files
        let rdev = specdata3 {
            specdata1: libc::major(stats.rdev as libc::dev_t) as u32,
            specdata2: libc::minor(stats.rdev as libc::dev_t) as u32,
        };

        fattr3 {
            ftype,
            mode: stats.mode & 0o7777,
            nlink: stats.nlink,
            uid: stats.uid,
            gid: stats.gid,
            size: stats.size as u64,
            used: stats.size as u64,
            rdev,
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

    /// Get path for an NFS inode, returning NOENT error if not found.
    async fn get_path(&self, ino: fileid3) -> Result<String, nfsstat3> {
        self.inode_map
            .read()
            .await
            .get_path(ino)
            .ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    /// Get filesystem inode for an NFS inode.
    async fn get_fs_ino(&self, ino: fileid3) -> Result<i64, nfsstat3> {
        self.inode_map
            .read()
            .await
            .get_fs_ino(ino)
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

    /// Get parent directory path.
    fn parent_path(path: &str) -> String {
        if path == "/" {
            return "/".to_string();
        }
        std::path::Path::new(path)
            .parent()
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                if s.is_empty() {
                    "/".to_string()
                } else {
                    s
                }
            })
            .unwrap_or_else(|| "/".to_string())
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
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Handle . and ..
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            let parent_path = Self::parent_path(&dir_path);
            let fs = self.fs.lock().await;
            let parent_fs_ino = self.resolve_path_to_fs_ino(&fs, &parent_path).await?;
            drop(fs);
            return Ok(self
                .inode_map
                .write()
                .await
                .get_or_create_ino(&parent_path, parent_fs_ino));
        }

        let full_path = Self::join_path(&dir_path, name);

        // Lock filesystem for the lookup
        let fs = self.fs.lock().await;

        // Verify parent is a directory
        let dir_stats = fs
            .getattr(dir_fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if !dir_stats.is_directory() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        // Lookup the entry
        let stats = fs
            .lookup(dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        drop(fs);

        Ok(self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, stats.ino))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let fs_ino = self.get_fs_ino(id).await?;
        let fs = self.fs.lock().await;
        let stats = fs
            .getattr(fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats, id))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let fs_ino = self.get_fs_ino(id).await?;
        let fs = self.fs.lock().await;

        // Handle chmod (mode change)
        if let set_mode3::mode(mode) = setattr.mode {
            fs.chmod(fs_ino, mode).await.map_err(error_to_nfsstat)?;
        }

        // Handle chown (uid/gid change)
        let new_uid = if let set_uid3::uid(uid) = setattr.uid {
            Some(uid)
        } else {
            None
        };
        let new_gid = if let set_gid3::gid(gid) = setattr.gid {
            Some(gid)
        } else {
            None
        };
        if new_uid.is_some() || new_gid.is_some() {
            fs.chown(fs_ino, new_uid, new_gid)
                .await
                .map_err(error_to_nfsstat)?;
        }

        // Handle size change (truncate)
        if let set_size3::size(size) = setattr.size {
            let file = fs.open(fs_ino).await.map_err(error_to_nfsstat)?;
            file.truncate(size).await.map_err(error_to_nfsstat)?;
        }

        // Get updated stats
        let stats = fs
            .getattr(fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats, id))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let fs_ino = self.get_fs_ino(id).await?;
        let fs = self.fs.lock().await;

        let file = fs.open(fs_ino).await.map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        let data = file
            .pread(offset, count as u64)
            .await
            .map_err(error_to_nfsstat)?;

        // Check if we've reached EOF
        let stats = file.fstat().await.map_err(error_to_nfsstat)?;

        let eof = offset + data.len() as u64 >= stats.size as u64;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let fs_ino = self.get_fs_ino(id).await?;

        {
            let fs = self.fs.lock().await;

            let file = fs.open(fs_ino).await.map_err(error_to_nfsstat)?;
            file.pwrite(offset, data).await.map_err(error_to_nfsstat)?;
        }

        self.getattr(id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        // Use mode from sattr3 if provided, otherwise default to 0o644
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        // Create file with caller's uid/gid and requested mode
        let new_fs_ino = {
            let fs = self.fs.lock().await;
            let (stats, _file) = fs
                .create_file(dir_fs_ino, name, S_IFREG | mode, auth.uid, auth.gid)
                .await
                .map_err(error_to_nfsstat)?;
            stats.ino
        };

        let ino = self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, new_fs_ino);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
        auth: &auth_unix,
    ) -> Result<fileid3, nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        let fs = self.fs.lock().await;

        // Check if file already exists
        if fs
            .lookup(dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?
            .is_some()
        {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }

        // Create file with caller's uid/gid
        let (stats, _file) = fs
            .create_file(dir_fs_ino, name, S_IFREG | 0o644, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        drop(fs);
        Ok(self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, stats.ino))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        // Use mode from sattr3 if provided, otherwise default to 0o755
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o755,
        };

        let new_fs_ino = {
            let fs = self.fs.lock().await;

            let stats = fs
                .mkdir(dir_fs_ino, name, auth.uid, auth.gid)
                .await
                .map_err(error_to_nfsstat)?;

            // Set the mode after creation (SDK mkdir doesn't take mode)
            fs.chmod(stats.ino, S_IFDIR | mode)
                .await
                .map_err(error_to_nfsstat)?;

            stats.ino
        };

        let ino = self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, new_fs_ino);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn mknod(
        &self,
        dirid: fileid3,
        filename: &filename3,
        ftype: ftype3,
        attr: sattr3,
        rdev: specdata3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        // Use mode from sattr3 if provided, otherwise default to 0o644
        let perm_mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        // Convert NFS file type to SDK mode constant
        let type_mode = match ftype {
            ftype3::NF3CHR => S_IFCHR,
            ftype3::NF3BLK => S_IFBLK,
            ftype3::NF3SOCK => S_IFSOCK,
            ftype3::NF3FIFO => S_IFIFO,
            _ => return Err(nfsstat3::NFS3ERR_BADTYPE),
        };

        // Convert rdev from specdata3 (major/minor) to u64
        let rdev_val = libc::makedev(rdev.specdata1 as _, rdev.specdata2 as _) as u64;

        let new_fs_ino = {
            let fs = self.fs.lock().await;

            let stats = fs
                .mknod(
                    dir_fs_ino,
                    name,
                    type_mode | perm_mode,
                    rdev_val,
                    auth.uid,
                    auth.gid,
                )
                .await
                .map_err(error_to_nfsstat)?;

            stats.ino
        };

        let ino = self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, new_fs_ino);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        {
            let fs = self.fs.lock().await;

            // Check if it's a file or directory and use appropriate method
            let stats = fs
                .lookup(dir_fs_ino, name)
                .await
                .map_err(error_to_nfsstat)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?;

            if stats.is_directory() {
                fs.rmdir(dir_fs_ino, name).await.map_err(error_to_nfsstat)?;
            } else {
                fs.unlink(dir_fs_ino, name)
                    .await
                    .map_err(error_to_nfsstat)?;
            }
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
        let from_dir_fs_ino = self.get_fs_ino(from_dirid).await?;
        let to_dir = self.get_path(to_dirid).await?;
        let to_dir_fs_ino = self.get_fs_ino(to_dirid).await?;
        let from_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let from_path = Self::join_path(&from_dir, from_name);
        let to_path = Self::join_path(&to_dir, to_name);

        {
            let fs = self.fs.lock().await;

            fs.rename(from_dir_fs_ino, from_name, to_dir_fs_ino, to_name)
                .await
                .map_err(error_to_nfsstat)?;
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
        let dir_fs_ino = self.get_fs_ino(dirid).await?;

        let entries = {
            let fs = self.fs.lock().await;

            fs.readdir_plus(dir_fs_ino)
                .await
                .map_err(error_to_nfsstat)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?
        };

        let mut result = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };

        // Find start position if start_after is specified
        let mut skip = start_after > 0;
        let mut skipped_count = 0;

        for entry in entries.iter() {
            let entry_path = Self::join_path(&dir_path, &entry.name);
            let ino = self
                .inode_map
                .write()
                .await
                .get_or_create_ino(&entry_path, entry.stats.ino);

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
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.get_path(dirid).await?;
        let dir_fs_ino = self.get_fs_ino(dirid).await?;
        let name = std::str::from_utf8(linkname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let full_path = Self::join_path(&dir_path, name);

        let new_fs_ino = {
            let fs = self.fs.lock().await;

            let stats = fs
                .symlink(dir_fs_ino, name, target, auth.uid, auth.gid)
                .await
                .map_err(error_to_nfsstat)?;
            stats.ino
        };

        let ino = self
            .inode_map
            .write()
            .await
            .get_or_create_ino(&full_path, new_fs_ino);
        let attr = self.getattr(ino).await?;
        Ok((ino, attr))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let fs_ino = self.get_fs_ino(id).await?;

        let fs = self.fs.lock().await;

        let target = fs
            .readlink(fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(target.into_bytes().into())
    }
}
