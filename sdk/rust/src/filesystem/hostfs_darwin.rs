//! macOS/Darwin HostFS implementation using path-based operations.
//!
//! This is a simpler implementation compared to the Linux version which uses
//! O_PATH file descriptors. macOS doesn't support O_PATH or AT_EMPTY_PATH,
//! so we use a path-based approach similar to libfuse's passthrough.c example.

use super::{BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats, TimeChange};
use crate::error::{Error, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
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

/// An inode entry caching the path to the file
struct Inode {
    /// Full path to the file
    path: PathBuf,
    /// Source inode number from the host filesystem
    src_ino: u64,
    /// Source device id from the host filesystem
    #[allow(dead_code)]
    src_dev: u64,
    /// Reference count (number of kernel lookups)
    nlookup: AtomicU64,
}

/// A filesystem backed by a host directory (passthrough) using path-based operations
///
/// This implementation follows the simple passthrough pattern from libfuse:
/// - Inodes are cached as paths
/// - All operations use the cached path for file access
/// - Simpler but has TOCTOU considerations compared to fd-based approaches
pub struct HostFS {
    root: PathBuf,
    /// Map from our inode numbers to Inode structs
    inodes: RwLock<HashMap<i64, Inode>>,
    /// Map from source identity (ino, dev) to our inode number (for hardlink detection)
    src_to_ino: RwLock<HashMap<SrcId, i64>>,
    /// Next inode number to allocate
    next_ino: AtomicU64,
    /// FUSE mountpoint inode to avoid deadlock when overlaying
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
        mode: stat.st_mode as u32,
        nlink: stat.st_nlink as u32,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: stat.st_size,
        atime: stat.st_atime,
        mtime: stat.st_mtime,
        ctime: stat.st_ctime,
        atime_nsec: stat.st_atime_nsec as u32,
        mtime_nsec: stat.st_mtime_nsec as u32,
        ctime_nsec: stat.st_ctime_nsec as u32,
        rdev: stat.st_rdev as u64,
    }
}

/// Clear errno (platform-specific)
#[inline]
fn clear_errno() {
    unsafe { *libc::__error() = 0 };
}

/// Get errno (platform-specific)
#[inline]
fn get_errno() -> i32 {
    unsafe { *libc::__error() }
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

        // Canonicalize the root path
        let root = root
            .canonicalize()
            .map_err(|e| Error::Internal(format!("failed to canonicalize root: {}", e)))?;

        // Get root stats using lstat
        let stat = Self::lstat_path(&root)?;

        // Create root inode entry
        let root_inode = Inode {
            path: root.clone(),
            src_ino: stat.st_ino,
            src_dev: stat.st_dev as u64,
            nlookup: AtomicU64::new(1),
        };

        let mut inodes = HashMap::new();
        inodes.insert(ROOT_INO, root_inode);

        let mut src_to_ino = HashMap::new();
        src_to_ino.insert(
            SrcId {
                ino: stat.st_ino,
                dev: stat.st_dev as u64,
            },
            ROOT_INO,
        );

        Ok(Self {
            root,
            inodes: RwLock::new(inodes),
            src_to_ino: RwLock::new(src_to_ino),
            next_ino: AtomicU64::new(2), // 1 is root
            fuse_mountpoint_inode: None,
        })
    }

    /// Set the FUSE mountpoint inode to avoid deadlock when overlaying
    pub fn with_fuse_mountpoint(mut self, inode: u64) -> Self {
        self.fuse_mountpoint_inode = Some(inode);
        self
    }

    /// Get the root directory
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Get the path for an inode
    fn get_inode_path(&self, ino: i64) -> Result<PathBuf> {
        let inodes = self.inodes.read().unwrap();
        let inode = inodes.get(&ino).ok_or(FsError::NotFound)?;
        Ok(inode.path.clone())
    }

    /// Allocate a new inode number
    fn alloc_ino(&self) -> i64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed) as i64
    }

    /// Perform lstat on a path
    fn lstat_path(path: &PathBuf) -> Result<libc::stat> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::lstat(c_path.as_ptr(), &mut stat) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(stat)
    }

    /// Open a file by path
    fn open_path(path: &PathBuf, flags: libc::c_int) -> Result<OwnedFd> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Create or reuse an inode for the given source identity
    fn get_or_create_inode(&self, path: PathBuf, stat: &libc::stat) -> (i64, bool) {
        let src_id = SrcId {
            ino: stat.st_ino,
            dev: stat.st_dev as u64,
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
            path,
            src_ino: stat.st_ino,
            src_dev: stat.st_dev as u64,
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
        let parent_path = self.get_inode_path(parent_ino)?;

        // Check for FUSE mountpoint to avoid deadlock
        if let Some(fuse_ino) = self.fuse_mountpoint_inode {
            let inodes = self.inodes.read().unwrap();
            if let Some(parent_inode) = inodes.get(&parent_ino) {
                if parent_inode.src_ino == fuse_ino {
                    return Ok(None);
                }
            }
        }

        // Build child path
        let child_path = parent_path.join(name);

        // Get stats using lstat (don't follow symlinks)
        let stat = match Self::lstat_path(&child_path) {
            Ok(stat) => stat,
            Err(e) => {
                if let Error::Io(ref io_err) = e {
                    if io_err.kind() == std::io::ErrorKind::NotFound {
                        return Ok(None);
                    }
                }
                return Err(e);
            }
        };

        // Skip FUSE mountpoint
        if let Some(fuse_ino) = self.fuse_mountpoint_inode {
            if stat.st_ino == fuse_ino {
                return Ok(None);
            }
        }

        // Get or create inode
        let (ino, _is_new) = self.get_or_create_inode(child_path, &stat);

        // Return stats with our inode number
        let mut stats = stat_to_stats(&stat);
        stats.ino = ino;

        Ok(Some(stats))
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        let path = match self.get_inode_path(ino) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        let stat = Self::lstat_path(&path)?;
        let mut stats = stat_to_stats(&stat);
        stats.ino = ino;

        Ok(Some(stats))
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        let path = match self.get_inode_path(ino) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let len = unsafe {
            libc::readlink(
                c_path.as_ptr(),
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
        let path = match self.get_inode_path(ino) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        tokio::task::spawn_blocking(move || {
            let dir = unsafe { libc::opendir(c_path.as_ptr()) };
            if dir.is_null() {
                return Err::<_, Error>(std::io::Error::last_os_error().into());
            }

            let mut entries = Vec::new();

            loop {
                clear_errno();
                let entry = unsafe { libc::readdir(dir) };

                if entry.is_null() {
                    let errno = get_errno();
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
        let path = match self.get_inode_path(ino) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };

        let fuse_mountpoint_inode = self.fuse_mountpoint_inode;
        let path_clone = path.clone();

        let entries_raw: Vec<(String, PathBuf, libc::stat)> =
            tokio::task::spawn_blocking(move || {
                let c_path = CString::new(path_clone.as_os_str().as_bytes())
                    .map_err(|_| Error::Internal("invalid path".to_string()))?;

                let dir = unsafe { libc::opendir(c_path.as_ptr()) };
                if dir.is_null() {
                    return Err::<_, Error>(std::io::Error::last_os_error().into());
                }

                let mut entries = Vec::new();

                loop {
                    clear_errno();
                    let entry = unsafe { libc::readdir(dir) };

                    if entry.is_null() {
                        let errno = get_errno();
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

                    let child_path = path_clone.join(name_str.as_ref());
                    let c_child = CString::new(child_path.as_os_str().as_bytes())
                        .map_err(|_| Error::Internal("invalid path".to_string()))?;

                    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                    let result = unsafe { libc::lstat(c_child.as_ptr(), &mut stat) };

                    if result == 0 {
                        if let Some(fuse_ino) = fuse_mountpoint_inode {
                            if stat.st_ino == fuse_ino {
                                continue;
                            }
                        }
                        entries.push((name_str.to_string(), child_path, stat));
                    }
                }

                unsafe { libc::closedir(dir) };
                Ok(entries)
            })
            .await
            .map_err(|e| Error::Internal(e.to_string()))??;

        // Now create/lookup inodes for each entry
        let mut result = Vec::new();
        for (name, child_path, stat) in entries_raw {
            let (child_ino, _) = self.get_or_create_inode(child_path, &stat);

            let mut stats = stat_to_stats(&stat);
            stats.ino = child_ino;

            result.push(DirEntry { name, stats });
        }

        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        let path = self.get_inode_path(ino)?;
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::chmod(c_path.as_ptr(), mode as libc::mode_t) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        let path = self.get_inode_path(ino)?;

        // Get current ownership if needed
        let stat = Self::lstat_path(&path)?;
        let uid = uid.unwrap_or(stat.st_uid);
        let gid = gid.unwrap_or(stat.st_gid);

        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        // Use lchown to not follow symlinks
        let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        let path = self.get_inode_path(ino)?;

        let to_timespec = |tc: TimeChange| -> libc::timespec {
            match tc {
                TimeChange::Set(secs, nsec) => libc::timespec {
                    tv_sec: secs as libc::time_t,
                    tv_nsec: nsec as libc::c_long,
                },
                TimeChange::Now => libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_NOW,
                },
                TimeChange::Omit => libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            }
        };

        let times = [to_timespec(atime), to_timespec(mtime)];

        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    async fn open(&self, ino: i64) -> Result<BoxedFile> {
        let path = self.get_inode_path(ino)?;
        let real_fd = Self::open_path(&path, libc::O_RDWR)?;
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
        let parent_path = self.get_inode_path(parent_ino)?;
        let new_path = parent_path.join(name);
        let c_path = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::mkdir(c_path.as_ptr(), mode as libc::mode_t) };
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
        let parent_path = self.get_inode_path(parent_ino)?;
        let new_path = parent_path.join(name);
        let c_path = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        // Create and open the file
        let file_fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                mode as libc::mode_t as libc::c_uint,
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

        // Get stats
        let stat = Self::lstat_path(&new_path)?;
        let (ino, _) = self.get_or_create_inode(new_path, &stat);

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
        let parent_path = self.get_inode_path(parent_ino)?;
        let new_path = parent_path.join(name);
        let c_path = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result =
            unsafe { libc::mknod(c_path.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) };
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
        let parent_path = self.get_inode_path(parent_ino)?;
        let new_path = parent_path.join(name);
        let c_path = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let c_target = CString::new(target).map_err(|_| FsError::InvalidPath)?;

        let result = unsafe { libc::symlink(c_target.as_ptr(), c_path.as_ptr()) };
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
        let parent_path = self.get_inode_path(parent_ino)?;
        let path = parent_path.join(name);
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::unlink(c_path.as_ptr()) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Err(FsError::NotFound.into());
            }
            return Err(err.into());
        }

        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        let parent_path = self.get_inode_path(parent_ino)?;
        let path = parent_path.join(name);
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::rmdir(c_path.as_ptr()) };
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
        let path = self.get_inode_path(ino)?;
        let newparent_path = self.get_inode_path(newparent_ino)?;
        let new_path = newparent_path.join(newname);

        let c_old = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let c_new = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::link(c_old.as_ptr(), c_new.as_ptr()) };
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
        let oldparent_path = self.get_inode_path(oldparent_ino)?;
        let newparent_path = self.get_inode_path(newparent_ino)?;
        let old_path = oldparent_path.join(oldname);
        let new_path = newparent_path.join(newname);

        let c_old = CString::new(old_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;
        let c_new = CString::new(new_path.as_os_str().as_bytes())
            .map_err(|_| Error::Internal("invalid path".to_string()))?;

        let result = unsafe { libc::rename(c_old.as_ptr(), c_new.as_ptr()) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Err(FsError::NotFound.into());
            }
            return Err(err.into());
        }

        // Update the cached path for the moved inode
        // First, find the inode for this entry
        if let Ok(Some(stats)) = self.lookup(newparent_ino, newname).await {
            let mut inodes = self.inodes.write().unwrap();
            if let Some(inode) = inodes.get_mut(&stats.ino) {
                inode.path = new_path;
            }
        }

        Ok(())
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        let path = self.root.clone();

        tokio::task::spawn_blocking(move || {
            let c_path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| Error::Internal("invalid path".to_string()))?;
            let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::statfs(c_path.as_ptr(), &mut statfs) };
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
                let old = inode.nlookup.fetch_sub(nlookup, Ordering::Relaxed);
                old <= nlookup
            } else {
                false
            }
        };

        if should_remove {
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
