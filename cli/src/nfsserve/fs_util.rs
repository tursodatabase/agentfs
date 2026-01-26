use super::nfs::*;
use std::fs::Metadata;
use std::fs::Permissions;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tokio::fs::OpenOptions;
use tracing::debug;

/// Compares if file metadata has changed in a significant way
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn metadata_differ(lhs: &Metadata, rhs: &Metadata) -> bool {
    lhs.ino() != rhs.ino()
        || lhs.mtime() != rhs.mtime()
        || lhs.len() != rhs.len()
        || lhs.file_type() != rhs.file_type()
}
pub fn fattr3_differ(lhs: &fattr3, rhs: &fattr3) -> bool {
    lhs.fileid != rhs.fileid
        || lhs.mtime.seconds != rhs.mtime.seconds
        || lhs.mtime.nseconds != rhs.mtime.nseconds
        || lhs.size != rhs.size
        || lhs.ftype as u32 != rhs.ftype as u32
}

/// path.exists() is terrifyingly unsafe as that
/// traverses symlinks. This can cause deadlocks if we have a
/// recursive symlink.
pub fn exists_no_traverse(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn mode_unmask(mode: u32) -> u32 {
    // it is possible to create a file we cannot write to.
    // we force writable always.
    let mode = mode | 0x80;
    let mode = Permissions::from_mode(mode);
    mode.mode() & 0x1FF
}

/// Converts fs Metadata to NFS fattr3
pub fn metadata_to_fattr3(fid: fileid3, meta: &Metadata) -> fattr3 {
    let size = meta.size();
    let file_mode = mode_unmask(meta.mode());
    if meta.is_file() {
        fattr3 {
            ftype: ftype3::NF3REG,
            mode: file_mode,
            nlink: 1,
            uid: meta.uid(),
            gid: meta.gid(),
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: fid,
            atime: nfstime3 {
                seconds: meta.atime() as u32,
                nseconds: meta.atime_nsec() as u32,
            },
            mtime: nfstime3 {
                seconds: meta.mtime() as u32,
                nseconds: meta.mtime_nsec() as u32,
            },
            ctime: nfstime3 {
                seconds: meta.ctime() as u32,
                nseconds: meta.ctime_nsec() as u32,
            },
        }
    } else if meta.is_symlink() {
        fattr3 {
            ftype: ftype3::NF3LNK,
            mode: file_mode,
            nlink: 1,
            uid: meta.uid(),
            gid: meta.gid(),
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: fid,
            atime: nfstime3 {
                seconds: meta.atime() as u32,
                nseconds: meta.atime_nsec() as u32,
            },
            mtime: nfstime3 {
                seconds: meta.mtime() as u32,
                nseconds: meta.mtime_nsec() as u32,
            },
            ctime: nfstime3 {
                seconds: meta.ctime() as u32,
                nseconds: meta.ctime_nsec() as u32,
            },
        }
    } else {
        fattr3 {
            ftype: ftype3::NF3DIR,
            mode: file_mode,
            nlink: 2,
            uid: meta.uid(),
            gid: meta.gid(),
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: fid,
            atime: nfstime3 {
                seconds: meta.atime() as u32,
                nseconds: meta.atime_nsec() as u32,
            },
            mtime: nfstime3 {
                seconds: meta.mtime() as u32,
                nseconds: meta.mtime_nsec() as u32,
            },
            ctime: nfstime3 {
                seconds: meta.ctime() as u32,
                nseconds: meta.ctime_nsec() as u32,
            },
        }
    }
}

/// Set attributes of a path
pub async fn path_setattr(path: &Path, setattr: &sattr3) -> Result<(), nfsstat3> {
    match setattr.atime {
        set_atime::SET_TO_SERVER_TIME => {
            let _ = filetime::set_file_atime(path, filetime::FileTime::now());
        }
        set_atime::SET_TO_CLIENT_TIME(time) => {
            let _ = filetime::set_file_atime(path, time.into());
        }
        _ => {}
    };
    match setattr.mtime {
        set_mtime::SET_TO_SERVER_TIME => {
            let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
        }
        set_mtime::SET_TO_CLIENT_TIME(time) => {
            let _ = filetime::set_file_mtime(path, time.into());
        }
        _ => {}
    };
    if let set_mode3::mode(mode) = setattr.mode {
        debug!(" -- set permissions {:?} {:?}", path, mode);
        let mode = mode_unmask(mode);
        let _ = std::fs::set_permissions(path, Permissions::from_mode(mode));
    };
    if let set_uid3::uid(_) = setattr.uid {
        debug!("Set uid not implemented");
    }
    if let set_gid3::gid(_) = setattr.gid {
        debug!("Set gid not implemented");
    }
    if let set_size3::size(size3) = setattr.size {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .await
            .or(Err(nfsstat3::NFS3ERR_IO))?;
        debug!(" -- set size {:?} {:?}", path, size3);
        file.set_len(size3).await.or(Err(nfsstat3::NFS3ERR_IO))?;
    }
    Ok(())
}

/// Set attributes of a file
pub async fn file_setattr(file: &std::fs::File, setattr: &sattr3) -> Result<(), nfsstat3> {
    if let set_mode3::mode(mode) = setattr.mode {
        debug!(" -- set permissions {:?}", mode);
        let mode = mode_unmask(mode);
        let _ = file.set_permissions(Permissions::from_mode(mode));
    }
    if let set_size3::size(size3) = setattr.size {
        debug!(" -- set size {:?}", size3);
        file.set_len(size3).or(Err(nfsstat3::NFS3ERR_IO))?;
    }
    Ok(())
}
