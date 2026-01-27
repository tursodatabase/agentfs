use super::{BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats};
use crate::error::{Error, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

/// Root inode number (matches FUSE convention)
pub const ROOT_INO: i64 = 1;

/// Source file identity (inode + device), used to detect hardlinks
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SrcId {
    ino: u64,
    dev: u64,
}

/// An inode entry caching an O_PATH file descriptor
struct Inode {
    /// O_PATH file descriptor - a handle to the file for metadata operations
    fd: OwnedFd,
    /// Source inode number from the host filesystem
    src_ino: u64,
    /// Source device id from the host filesystem
    #[allow(dead_code)]
    src_dev: u64,
    /// Reference count (number of kernel lookups)
    nlookup: AtomicU64,
}

/// A filesystem backed by a host directory (passthrough) using O_PATH file descriptors
///
/// This implementation follows the architecture of libfuse's passthrough_hp.cc:
/// - Inodes are cached as O_PATH file descriptors (handles to paths)
/// - Metadata operations use fstatat/openat with AT_EMPTY_PATH
/// - File I/O uses real fds obtained via /proc/self/fd/
pub struct HostFS {
    root: PathBuf,
    /// The root O_PATH file descriptor
    root_fd: OwnedFd,
    /// Map from our inode numbers to Inode structs
    inodes: RwLock<HashMap<i64, Inode>>,
    /// Map from source identity (ino, dev) to our inode number (for hardlink detection)
    src_to_ino: RwLock<HashMap<SrcId, i64>>,
    /// Next inode number to allocate
    next_ino: AtomicU64,
    /// FUSE mountpoint inode to avoid deadlock when overlaying
    #[cfg(target_family = "unix")]
    fuse_mountpoint_inode: Option<u64>,
}

/// An open file handle for HostFS (real fd for I/O)
pub struct HostFSFile {
    /// Real file descriptor for read/write operations
    fd: OwnedFd,
}

#[async_trait]
impl File for HostFSFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; size as usize];
            let n = unsafe {
                libc::pread(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as usize,
                    offset as libc::off_t,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            buf.truncate(n as usize);
            Ok(buf)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let n = unsafe {
                libc::pwrite(
                    fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    offset as libc::off_t,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let result = unsafe { libc::ftruncate(fd, size as libc::off_t) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn fsync(&self) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let result = unsafe { libc::fsync(fd) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn fstat(&self) -> Result<Stats> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::fstat(fd, &mut stat) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(stat_to_stats(&stat))
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

/// Convert libc::stat to our Stats struct
fn stat_to_stats(stat: &libc::stat) -> Stats {
    Stats {
        ino: stat.st_ino as i64,
        mode: stat.st_mode,
        nlink: stat.st_nlink as u32,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: stat.st_size,
        atime: stat.st_atime,
        mtime: stat.st_mtime,
        ctime: stat.st_ctime,
        rdev: stat.st_rdev,
    }
}

impl HostFS {
    /// Create a new HostFS rooted at the given directory
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.exists() {
            return Err(Error::BaseDirectoryNotFound(root.display().to_string()));
        }
        if !root.is_dir() {
            return Err(Error::NotADirectory(root.display().to_string()));
        }

        // Open root with O_PATH
        let c_path = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root_fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // Get root stats
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                root_fd.as_raw_fd(),
                c"".as_ptr(),
                &mut stat,
                libc::AT_EMPTY_PATH,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        // Create root inode entry
        let root_inode = Inode {
            fd: root_fd
                .try_clone()
                .map_err(|e| Error::Internal(e.to_string()))?,
            src_ino: stat.st_ino,
            src_dev: stat.st_dev,
            nlookup: AtomicU64::new(1),
        };

        let mut inodes = HashMap::new();
        inodes.insert(ROOT_INO, root_inode);

        let mut src_to_ino = HashMap::new();
        src_to_ino.insert(
            SrcId {
                ino: stat.st_ino,
                dev: stat.st_dev,
            },
            ROOT_INO,
        );

        Ok(Self {
            root,
            root_fd,
            inodes: RwLock::new(inodes),
            src_to_ino: RwLock::new(src_to_ino),
            next_ino: AtomicU64::new(2), // 1 is root
            fuse_mountpoint_inode: None,
        })
    }

    /// Set the FUSE mountpoint inode to avoid deadlock when overlaying
    #[cfg(target_family = "unix")]
    pub fn with_fuse_mountpoint(mut self, inode: u64) -> Self {
        self.fuse_mountpoint_inode = Some(inode);
        self
    }

    /// Get the root directory
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Get the O_PATH fd for an inode
    fn get_inode_fd(&self, ino: i64) -> Result<RawFd> {
        let inodes = self.inodes.read().unwrap();
        let inode = inodes.get(&ino).ok_or(FsError::NotFound)?;
        Ok(inode.fd.as_raw_fd())
    }

    /// Allocate a new inode number
    fn alloc_ino(&self) -> i64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed) as i64
    }

    /// Perform fstatat on an O_PATH fd with AT_EMPTY_PATH
    fn fstatat_empty_path(fd: RawFd) -> Result<libc::stat> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                fd,
                c"".as_ptr(),
                &mut stat,
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(stat)
    }

    /// Open a real fd from an O_PATH fd via /proc/self/fd/
    fn open_real_fd(o_path_fd: RawFd, flags: libc::c_int) -> Result<OwnedFd> {
        let proc_path = format!("/proc/self/fd/{}\0", o_path_fd);
        let fd = unsafe { libc::open(proc_path.as_ptr() as *const libc::c_char, flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Create or reuse an inode for the given source identity
    fn get_or_create_inode(&self, fd: OwnedFd, stat: &libc::stat) -> (i64, bool) {
        let src_id = SrcId {
            ino: stat.st_ino,
            dev: stat.st_dev,
        };

        // Check if we already have this source file
        {
            let src_map = self.src_to_ino.read().unwrap();
            if let Some(&ino) = src_map.get(&src_id) {
                // Increment nlookup on existing inode
                let inodes = self.inodes.read().unwrap();
                if let Some(inode) = inodes.get(&ino) {
                    inode.nlookup.fetch_add(1, Ordering::Relaxed);
                    return (ino, false);
                }
            }
        }

        // Create new inode
        let ino = self.alloc_ino();
        let inode = Inode {
            fd,
            src_ino: stat.st_ino,
            src_dev: stat.st_dev,
            nlookup: AtomicU64::new(1),
        };

        {
            let mut inodes = self.inodes.write().unwrap();
            inodes.insert(ino, inode);
        }
        {
            let mut src_map = self.src_to_ino.write().unwrap();
            src_map.insert(src_id, ino);
        }

        (ino, true)
    }

    /// Remove an inode from the cache
    #[allow(dead_code)]
    fn remove_inode(&self, ino: i64) {
        let mut inodes = self.inodes.write().unwrap();
        if let Some(inode) = inodes.remove(&ino) {
            let mut src_map = self.src_to_ino.write().unwrap();
            src_map.remove(&SrcId {
                ino: inode.src_ino,
                dev: inode.src_dev,
            });
        }
    }
}

#[async_trait]
impl FileSystem for HostFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        let parent_fd = self.get_inode_fd(parent_ino)?;

        // Check for FUSE mountpoint to avoid deadlock
        #[cfg(target_family = "unix")]
        if let Some(fuse_ino) = self.fuse_mountpoint_inode {
            // Get parent's source inode to check
            let inodes = self.inodes.read().unwrap();
            if let Some(parent_inode) = inodes.get(&parent_ino) {
                if parent_inode.src_ino == fuse_ino {
                    return Ok(None);
                }
            }
        }

        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        // Open child with O_PATH | O_NOFOLLOW
        let child_fd =
            unsafe { libc::openat(parent_fd, c_name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };

        if child_fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err.into());
        }

        let child_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };

        // Get stats
        let stat = Self::fstatat_empty_path(child_fd.as_raw_fd())?;

        // Skip FUSE mountpoint
        #[cfg(target_family = "unix")]
        if let Some(fuse_ino) = self.fuse_mountpoint_inode {
            if stat.st_ino == fuse_ino {
                return Ok(None);
            }
        }

        // Get or create inode
        let (ino, _is_new) = self.get_or_create_inode(child_fd, &stat);

        // Return stats with our inode number
        let mut stats = stat_to_stats(&stat);
        stats.ino = ino;

        Ok(Some(stats))
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        let fd = match self.get_inode_fd(ino) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };

        let stat = Self::fstatat_empty_path(fd)?;
        let mut stats = stat_to_stats(&stat);
        stats.ino = ino;

        Ok(Some(stats))
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        let fd = match self.get_inode_fd(ino) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };

        // Use readlinkat with AT_EMPTY_PATH to read the symlink target
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let c_empty = CString::new("").unwrap();
        let len = unsafe {
            libc::readlinkat(
                fd,
                c_empty.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };

        if len < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            if err.raw_os_error() == Some(libc::EINVAL) {
                // Not a symlink
                return Err(FsError::NotASymlink.into());
            }
            return Err(err.into());
        }

        buf.truncate(len as usize);
        Ok(Some(String::from_utf8_lossy(&buf).to_string()))
    }

    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        let fd = match self.get_inode_fd(ino) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };

        // Open a real fd for reading directory
        let dir_fd = Self::open_real_fd(fd, libc::O_RDONLY | libc::O_DIRECTORY)?;

        tokio::task::spawn_blocking(move || {
            let dir = unsafe { libc::fdopendir(dir_fd.as_raw_fd()) };
            if dir.is_null() {
                return Err::<_, Error>(std::io::Error::last_os_error().into());
            }

            // Prevent the DIR* from closing our fd when dropped
            std::mem::forget(dir_fd);

            let mut entries = Vec::new();

            loop {
                // Clear errno before readdir
                unsafe { *libc::__errno_location() = 0 };
                let entry = unsafe { libc::readdir(dir) };

                if entry.is_null() {
                    let errno = unsafe { *libc::__errno_location() };
                    if errno != 0 {
                        unsafe { libc::closedir(dir) };
                        return Err(std::io::Error::from_raw_os_error(errno).into());
                    }
                    break;
                }

                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name_str = name.to_string_lossy();

                // Skip . and ..
                if name_str == "." || name_str == ".." {
                    continue;
                }

                entries.push(name_str.to_string());
            }

            unsafe { libc::closedir(dir) };
            entries.sort();
            Ok(Some(entries))
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        let fd = match self.get_inode_fd(ino) {
            Ok(fd) => fd,
            Err(_) => return Ok(None),
        };

        // Open a real fd for reading directory
        let dir_fd = Self::open_real_fd(fd, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let dir_fd_raw = dir_fd.as_raw_fd();

        #[cfg(target_family = "unix")]
        let fuse_mountpoint_inode = self.fuse_mountpoint_inode;

        let entries_raw: Vec<(String, libc::stat)> = tokio::task::spawn_blocking(move || {
            let dir = unsafe { libc::fdopendir(dir_fd.as_raw_fd()) };
            if dir.is_null() {
                return Err::<_, Error>(std::io::Error::last_os_error().into());
            }
            std::mem::forget(dir_fd);

            let mut entries = Vec::new();

            loop {
                unsafe { *libc::__errno_location() = 0 };
                let entry = unsafe { libc::readdir(dir) };

                if entry.is_null() {
                    let errno = unsafe { *libc::__errno_location() };
                    if errno != 0 {
                        unsafe { libc::closedir(dir) };
                        return Err(std::io::Error::from_raw_os_error(errno).into());
                    }
                    break;
                }

                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name_str = name.to_string_lossy();

                if name_str == "." || name_str == ".." {
                    continue;
                }

                // Get stats for this entry
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                let result = unsafe {
                    libc::fstatat(
                        dir_fd_raw,
                        (*entry).d_name.as_ptr(),
                        &mut stat,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };

                if result == 0 {
                    #[cfg(target_family = "unix")]
                    if let Some(fuse_ino) = fuse_mountpoint_inode {
                        if stat.st_ino == fuse_ino {
                            continue;
                        }
                    }

                    entries.push((name_str.to_string(), stat));
                }
            }

            unsafe { libc::closedir(dir) };
            Ok(entries)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;

        // Now create/lookup inodes for each entry
        let mut result = Vec::new();
        for (name, stat) in entries_raw {
            // Open O_PATH fd for this entry
            let c_name = CString::new(name.as_str()).map_err(|_| FsError::InvalidPath)?;
            let child_fd =
                unsafe { libc::openat(fd, c_name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };

            if child_fd < 0 {
                continue; // Skip entries we can't open
            }

            let child_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };
            let (child_ino, _) = self.get_or_create_inode(child_fd, &stat);

            let mut stats = stat_to_stats(&stat);
            stats.ino = child_ino;

            result.push(DirEntry { name, stats });
        }

        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        let fd = self.get_inode_fd(ino)?;

        // fchmod doesn't work on O_PATH fds, use fchmodat via /proc/self/fd
        let proc_path = CString::new(format!("/proc/self/fd/{}", fd))
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::chmod(proc_path.as_ptr(), mode as libc::mode_t) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        let fd = self.get_inode_fd(ino)?;

        // Get current ownership if needed
        let stat = Self::fstatat_empty_path(fd)?;
        let uid = uid.unwrap_or(stat.st_uid);
        let gid = gid.unwrap_or(stat.st_gid);

        // Use fchownat with AT_EMPTY_PATH
        let result = unsafe {
            libc::fchownat(
                fd,
                c"".as_ptr(),
                uid,
                gid,
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) -> Result<()> {
        let fd = self.get_inode_fd(ino)?;

        // Get current stat for unchanged fields
        let stat = Self::fstatat_empty_path(fd)?;

        let atime_ts = libc::timespec {
            tv_sec: atime.unwrap_or(stat.st_atime) as _,
            tv_nsec: if atime.is_some() {
                0
            } else {
                libc::UTIME_OMIT as _
            },
        };
        let mtime_ts = libc::timespec {
            tv_sec: mtime.unwrap_or(stat.st_mtime) as _,
            tv_nsec: if mtime.is_some() {
                0
            } else {
                libc::UTIME_OMIT as _
            },
        };

        let times = [atime_ts, mtime_ts];
        let result = unsafe {
            libc::utimensat(
                fd,
                c"".as_ptr(),
                times.as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn open(&self, ino: i64) -> Result<BoxedFile> {
        let fd = self.get_inode_fd(ino)?;

        // Open real fd via /proc/self/fd for read/write
        let real_fd = Self::open_real_fd(fd, libc::O_RDWR)?;

        Ok(Arc::new(HostFSFile { fd: real_fd }))
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        _uid: u32,
        _gid: u32,
    ) -> Result<Stats> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe { libc::mkdirat(parent_fd, c_name.as_ptr(), mode as libc::mode_t) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(FsError::AlreadyExists.into());
            }
            return Err(err.into());
        }

        // Lookup the newly created directory
        self.lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound.into())
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        _uid: u32,
        _gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        // Create and open the file
        let file_fd = unsafe {
            libc::openat(
                parent_fd,
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                mode as libc::mode_t,
            )
        };
        if file_fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(FsError::AlreadyExists.into());
            }
            return Err(err.into());
        }
        let real_fd = unsafe { OwnedFd::from_raw_fd(file_fd) };

        // Also open O_PATH fd for the inode cache
        let o_path_fd =
            unsafe { libc::openat(parent_fd, c_name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };
        if o_path_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let o_path_fd = unsafe { OwnedFd::from_raw_fd(o_path_fd) };

        // Get stats
        let stat = Self::fstatat_empty_path(o_path_fd.as_raw_fd())?;
        let (ino, _) = self.get_or_create_inode(o_path_fd, &stat);

        let mut stats = stat_to_stats(&stat);
        stats.ino = ino;

        let file: BoxedFile = Arc::new(HostFSFile { fd: real_fd });
        Ok((stats, file))
    }

    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        _uid: u32,
        _gid: u32,
    ) -> Result<Stats> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe {
            libc::mknodat(
                parent_fd,
                c_name.as_ptr(),
                mode as libc::mode_t,
                rdev as libc::dev_t,
            )
        };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(FsError::AlreadyExists.into());
            }
            return Err(err.into());
        }

        self.lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound.into())
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        _uid: u32,
        _gid: u32,
    ) -> Result<Stats> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;
        let c_target = CString::new(target).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe { libc::symlinkat(c_target.as_ptr(), parent_fd, c_name.as_ptr()) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(FsError::AlreadyExists.into());
            }
            return Err(err.into());
        }

        self.lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound.into())
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), 0) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Err(FsError::NotFound.into());
            }
            return Err(err.into());
        }

        // Note: We don't remove the inode from cache here because
        // other hard links might still reference it. The cache will
        // be cleaned up on forget() or when the FS is dropped.

        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        let parent_fd = self.get_inode_fd(parent_ino)?;
        let c_name = CString::new(name).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), libc::AT_REMOVEDIR) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Err(FsError::NotFound.into());
            }
            if err.raw_os_error() == Some(libc::ENOTEMPTY) {
                return Err(FsError::NotEmpty.into());
            }
            if err.raw_os_error() == Some(libc::ENOTDIR) {
                return Err(FsError::NotADirectory.into());
            }
            return Err(err.into());
        }

        Ok(())
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        let fd = self.get_inode_fd(ino)?;
        let newparent_fd = self.get_inode_fd(newparent_ino)?;
        let c_newname = CString::new(newname).map_err(|_| FsError::InvalidPath)?;

        // linkat with AT_EMPTY_PATH to link from an O_PATH fd
        let result = unsafe {
            libc::linkat(
                fd,
                c"".as_ptr(),
                newparent_fd,
                c_newname.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(FsError::AlreadyExists.into());
            }
            return Err(err.into());
        }

        // Return updated stats (nlink should be incremented)
        self.getattr(ino).await?.ok_or(FsError::NotFound.into())
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<()> {
        let oldparent_fd = self.get_inode_fd(oldparent_ino)?;
        let newparent_fd = self.get_inode_fd(newparent_ino)?;
        let c_oldname = CString::new(oldname).map_err(|_| FsError::InvalidPath)?;
        let c_newname = CString::new(newname).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe {
            libc::renameat(
                oldparent_fd,
                c_oldname.as_ptr(),
                newparent_fd,
                c_newname.as_ptr(),
            )
        };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Err(FsError::NotFound.into());
            }
            return Err(err.into());
        }

        Ok(())
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        let fd = self.root_fd.as_raw_fd();

        tokio::task::spawn_blocking(move || {
            let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::fstatfs(fd, &mut statfs) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }

            Ok(FilesystemStats {
                inodes: statfs.f_files,
                bytes_used: (statfs.f_blocks - statfs.f_bfree) * statfs.f_bsize as u64,
            })
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn forget(&self, ino: i64, nlookup: u64) {
        // Never forget root inode
        if ino == ROOT_INO {
            return;
        }

        // Decrement nlookup and check if we should remove the inode
        let should_remove = {
            let inodes = self.inodes.read().unwrap();
            if let Some(inode) = inodes.get(&ino) {
                // Subtract nlookup from current count
                let old = inode.nlookup.fetch_sub(nlookup, Ordering::Relaxed);
                old <= nlookup // Will be zero or underflow
            } else {
                false
            }
        };

        if should_remove {
            // Remove the inode from cache (this closes the O_PATH fd)
            let mut inodes = self.inodes.write().unwrap();
            if let Some(inode) = inodes.remove(&ino) {
                let mut src_map = self.src_to_ino.write().unwrap();
                src_map.remove(&SrcId {
                    ino: inode.src_ino,
                    dev: inode.src_dev,
                });
            }
            // Also remove from path_map if present
            // Note: We'd need to track path->ino mapping to do this properly,
            // but for now the inode cache cleanup is the critical part for fd management
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_FILE_MODE;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_hostfs_basic() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create and write a file
        let (_, file) = fs
            .create_file(ROOT_INO, "test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello world").await?;

        // Lookup and read it back
        let stats = fs.lookup(ROOT_INO, "test.txt").await?.unwrap();
        assert!(stats.is_file());

        let file = fs.open(stats.ino).await?;
        let data = file.pread(0, 100).await?;
        assert_eq!(data, b"hello world");

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_mkdir_readdir() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create directory
        let subdir_stats = fs.mkdir(ROOT_INO, "subdir", 0o755, 0, 0).await?;
        assert!(subdir_stats.is_directory());

        // Create files in subdirectory
        let (_, file_a) = fs
            .create_file(subdir_stats.ino, "a.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file_a.pwrite(0, b"a").await?;
        let (_, file_b) = fs
            .create_file(subdir_stats.ino, "b.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file_b.pwrite(0, b"b").await?;

        // List directory
        let entries = fs.readdir(subdir_stats.ino).await?.unwrap();
        assert_eq!(entries, vec!["a.txt", "b.txt"]);

        Ok(())
    }

    #[tokio::test]
    async fn test_hostfs_symlink() -> Result<()> {
        let dir = tempdir()?;
        let fs = HostFS::new(dir.path())?;

        // Create a file
        let (_file_stats, file) = fs
            .create_file(ROOT_INO, "target.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;

        // Create a symlink
        let link_stats = fs.symlink(ROOT_INO, "link.txt", "target.txt", 0, 0).await?;
        assert!(link_stats.is_symlink());

        // Read the symlink
        let target = fs.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(target, "target.txt");

        Ok(())
    }
}
