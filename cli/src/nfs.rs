//! NFS server adapter for AgentFS.
//!
//! This module implements nfsserve's NFSFileSystem trait on top of AgentFS's
//! FileSystem trait, enabling systems to mount AgentFS via NFS without requiring
//! FUSE or other system extensions.

use std::sync::Arc;

use crate::nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime, set_gid3,
    set_mode3, set_mtime, set_size3, set_uid3, specdata3,
};
use crate::nfsserve::vfs::{auth_unix, DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::FsError;
use agentfs_sdk::{
    FileSystem, Stats, TimeChange, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG,
    S_IFSOCK,
};
use async_trait::async_trait;
use tokio::sync::Mutex;

/// Root directory inode number
const ROOT_INO: fileid3 = 1;

/// Convert a fileid3 to a filesystem inode number.
fn id_to_fs_ino(id: fileid3) -> i64 {
    id as i64
}

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
}

impl AgentNFS {
    /// Create a new NFS adapter wrapping the given filesystem.
    pub fn new(fs: Arc<Mutex<dyn FileSystem>>) -> Self {
        AgentNFS { fs }
    }

    /// Convert AgentFS Stats to NFS fattr3.
    fn stats_to_fattr(&self, stats: &Stats) -> fattr3 {
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
            fileid: stats.ino as fileid3,
            atime: nfstime3 {
                seconds: stats.atime as u32,
                nseconds: stats.atime_nsec,
            },
            mtime: nfstime3 {
                seconds: stats.mtime as u32,
                nseconds: stats.mtime_nsec,
            },
            ctime: nfstime3 {
                seconds: stats.ctime as u32,
                nseconds: stats.ctime_nsec,
            },
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
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Handle .
        if name == "." {
            return Ok(dirid);
        }

        let fs = self.fs.lock().await;

        // Handle .. via filesystem lookup
        if name == ".." {
            let stats = fs
                .lookup(id_to_fs_ino(dirid), "..")
                .await
                .map_err(error_to_nfsstat)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?;
            return Ok(stats.ino as fileid3);
        }

        // Verify parent is a directory
        let dir_stats = fs
            .getattr(id_to_fs_ino(dirid))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if !dir_stats.is_directory() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        // Lookup the entry
        let stats = fs
            .lookup(id_to_fs_ino(dirid), name)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(stats.ino as fileid3)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let fs = self.fs.lock().await;
        let stats = fs
            .getattr(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let fs_ino = id_to_fs_ino(id);
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

        // Handle atime/mtime changes (utimensat)
        let new_atime = match setattr.atime {
            set_atime::SET_TO_CLIENT_TIME(t) => TimeChange::Set(t.seconds as i64, t.nseconds),
            set_atime::SET_TO_SERVER_TIME => TimeChange::Now,
            set_atime::DONT_CHANGE => TimeChange::Omit,
        };
        let new_mtime = match setattr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(t) => TimeChange::Set(t.seconds as i64, t.nseconds),
            set_mtime::SET_TO_SERVER_TIME => TimeChange::Now,
            set_mtime::DONT_CHANGE => TimeChange::Omit,
        };
        if !matches!(new_atime, TimeChange::Omit) || !matches!(new_mtime, TimeChange::Omit) {
            fs.utimens(fs_ino, new_atime, new_mtime)
                .await
                .map_err(error_to_nfsstat)?;
        }

        // Get updated stats
        let stats = fs
            .getattr(fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let fs = self.fs.lock().await;

        let file = fs
            .open(id_to_fs_ino(id))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
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
        let fs = self.fs.lock().await;

        let file = fs.open(id_to_fs_ino(id)).await.map_err(error_to_nfsstat)?;
        file.pwrite(offset, data).await.map_err(error_to_nfsstat)?;

        let stats = fs
            .getattr(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Use mode from sattr3 if provided, otherwise default to 0o644
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        let fs = self.fs.lock().await;
        let (stats, _file) = fs
            .create_file(dir_fs_ino, name, S_IFREG | mode, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
        auth: &auth_unix,
    ) -> Result<fileid3, nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

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

        Ok(stats.ino as fileid3)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Use mode from sattr3 if provided, otherwise default to 0o755
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o755,
        };

        let fs = self.fs.lock().await;

        let stats = fs
            .mkdir(dir_fs_ino, name, mode, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
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
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

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

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

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

        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_dir_fs_ino = id_to_fs_ino(from_dirid);
        let to_dir_fs_ino = id_to_fs_ino(to_dirid);
        let from_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.lock().await;

        fs.rename(from_dir_fs_ino, from_name, to_dir_fs_ino, to_name)
            .await
            .map_err(error_to_nfsstat)?;

        Ok(())
    }

    async fn link(
        &self,
        id: fileid3,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fattr3, nfsstat3> {
        let fs_ino = id_to_fs_ino(id);
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.lock().await;
        let stats = fs
            .link(fs_ino, dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);

        let fs = self.fs.lock().await;

        let entries = fs
            .readdir_plus(dir_fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        drop(fs);

        let mut result = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };

        // Find start position if start_after is specified
        let mut skip = start_after > 0;
        let mut skipped_count = 0;

        for entry in entries.iter() {
            let ino = entry.stats.ino as fileid3;

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
                attr: self.stats_to_fattr(&entry.stats),
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
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(linkname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.lock().await;

        let stats = fs
            .symlink(dir_fs_ino, name, target, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let fs = self.fs.lock().await;

        let target = fs
            .readlink(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(target.into_bytes().into())
    }
}
