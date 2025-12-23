use agentfs_sdk::{BoxedFile, FileSystem, FsError, Stats};
use fuser::{
    consts::{
        FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_NO_OPENDIR_SUPPORT, FUSE_PARALLEL_DIROPS,
        FUSE_WRITEBACK_CACHE,
    },
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::{Handle, Runtime};

/// Cache entries never expire - we explicitly invalidate on mutations.
/// This is safe because we are the only writer to the filesystem.
const TTL: Duration = Duration::MAX;

/// Options for mounting an agent filesystem via FUSE.
#[derive(Debug, Clone)]
pub struct FuseMountOptions {
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Automatically unmount when the process exits.
    pub auto_unmount: bool,
    /// Allow root to access the mount.
    pub allow_root: bool,
    /// Filesystem name shown in mount output.
    pub fsname: String,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
}

/// Tracks an open file handle
struct OpenFile {
    /// The file handle from the filesystem layer.
    file: BoxedFile,
}

/// FUSE filesystem adapter that dispatches operations to async tasks.
///
/// Each FUSE callback spawns a tokio task to handle the operation concurrently,
/// allowing multiple filesystem operations to run in parallel instead of
/// serializing them through block_on.
struct AgentFSFuse {
    fs: Arc<dyn FileSystem>,
    /// Tokio runtime handle for spawning async tasks
    handle: Handle,
    path_cache: Arc<Mutex<HashMap<u64, String>>>,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: Arc<AtomicU64>,
    /// User ID to report for all files (set at mount time)
    uid: u32,
    /// Group ID to report for all files (set at mount time)
    gid: u32,
}

impl Filesystem for AgentFSFuse {
    /// Initialize the filesystem and enable performance optimizations.
    ///
    /// - Async read: allows the kernel to issue multiple read requests in parallel,
    ///   improving throughput for concurrent file access.
    /// - Writeback caching: allows the kernel to buffer writes and flush them
    ///   later, significantly improving write performance for small writes.
    /// - Parallel dirops: allows concurrent lookup() and readdir() on the same
    ///   directory, improving performance for parallel file access patterns.
    /// - Cache symlinks: caches readlink responses, avoiding repeated round-trips
    ///   for symlink resolution.
    /// - No opendir support: skips opendir/releasedir calls since we don't track
    ///   directory handles, reducing round-trips for directory operations.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.add_capabilities(
            FUSE_ASYNC_READ
                | FUSE_WRITEBACK_CACHE
                | FUSE_PARALLEL_DIROPS
                | FUSE_CACHE_SYMLINKS
                | FUSE_NO_OPENDIR_SUPPORT,
        );
        Ok(())
    }

    // ─────────────────────────────────────────────────────────────
    // Name Resolution & Attributes
    // ─────────────────────────────────────────────────────────────

    /// Looks up a directory entry by name within a parent directory.
    ///
    /// Resolves `name` under the directory identified by `parent` inode, stats the
    /// resulting path, and caches the inode-to-path mapping on success.
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            match fs.lstat(&path).await {
                Ok(Some(stats)) => {
                    let attr = fillattr(&stats, uid, gid);
                    path_cache.lock().insert(attr.ino, path);
                    reply.entry(&TTL, &attr, 0);
                }
                Ok(None) => reply.error(libc::ENOENT),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Retrieves file attributes for a given inode.
    ///
    /// Returns metadata (size, permissions, timestamps, etc.) for the file or
    /// directory identified by `ino`. Root inode (1) is handled specially.
    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            match fs.lstat(&path).await {
                Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats, uid, gid)),
                Ok(None) => reply.error(libc::ENOENT),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Reads the target of a symbolic link.
    ///
    /// Returns the path that the symlink points to. This is called by operations
    /// like `ls -l` to display symlink targets.
    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        self.handle.spawn(async move {
            match fs.readlink(&path).await {
                Ok(Some(target)) => reply.data(target.as_bytes()),
                Ok(None) => reply.error(libc::ENOENT),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Sets file attributes, primarily handling truncate operations.
    ///
    /// Currently only `size` changes (truncate) are supported. Other attribute
    /// changes (mode, uid, gid, timestamps) are accepted but ignored.
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Get file handle or path for truncate, and path for final stat
        let file_for_truncate =
            fh.and_then(|fh| self.open_files.lock().get(&fh).map(|f| f.file.clone()));
        let path = self.get_path(ino);

        // Early return if we need a file handle but don't have one
        if size.is_some() && fh.is_some() && file_for_truncate.is_none() {
            reply.error(libc::EBADF);
            return;
        }

        let Some(path) = path else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            // Handle truncate if requested
            if let Some(new_size) = size {
                let truncate_result = if let Some(file) = file_for_truncate {
                    file.truncate(new_size).await
                } else {
                    match fs.open(&path).await {
                        Ok(file) => file.truncate(new_size).await,
                        Err(_) => {
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                };

                if truncate_result.is_err() {
                    reply.error(libc::EIO);
                    return;
                }
            }

            // Return updated attributes
            match fs.stat(&path).await {
                Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats, uid, gid)),
                Ok(None) => reply.error(libc::ENOENT),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    // ─────────────────────────────────────────────────────────────
    // Directory Operations
    // ─────────────────────────────────────────────────────────────

    /// Reads directory entries for the given inode.
    ///
    /// Returns "." and ".." entries followed by the directory contents.
    /// Each entry's inode is cached for subsequent lookups.
    ///
    /// Uses readdir_plus to fetch entries with stats in a single query,
    /// avoiding N+1 database queries.
    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        self.handle.spawn(async move {
            let entries = match fs.readdir_plus(&path).await {
                Ok(Some(entries)) => entries,
                Ok(None) => {
                    reply.error(libc::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            // Determine parent inode for ".." entry
            let parent_ino = if ino == 1 {
                1 // Root's parent is itself
            } else {
                let parent_path = Path::new(&path)
                    .parent()
                    .map(|p| {
                        let s = p.to_string_lossy().to_string();
                        if s.is_empty() {
                            "/".to_string()
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| "/".to_string());

                if parent_path == "/" {
                    1
                } else {
                    match fs.stat(&parent_path).await {
                        Ok(Some(stats)) => stats.ino as u64,
                        _ => 1, // Fallback to root if parent lookup fails
                    }
                }
            };

            // Build entries list: ".", "..", then directory contents
            let mut all_entries: Vec<(u64, FileType, String)> = vec![
                (ino, FileType::Directory, ".".to_string()),
                (parent_ino, FileType::Directory, "..".to_string()),
            ];

            // Process entries with stats already available (no N+1 queries!)
            for entry in &entries {
                let entry_path = if path == "/" {
                    format!("/{}", entry.name)
                } else {
                    format!("{}/{}", path, entry.name)
                };

                let kind = if entry.stats.is_directory() {
                    FileType::Directory
                } else if entry.stats.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };

                path_cache.lock().insert(entry.stats.ino as u64, entry_path);
                all_entries.push((entry.stats.ino as u64, kind, entry.name.clone()));
            }

            for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
                if reply.add(entry.0, (i + 1) as i64, entry.1, &entry.2) {
                    break;
                }
            }
            reply.ok();
        });
    }

    /// Reads directory entries with full attributes for the given inode.
    ///
    /// This is an optimized version that returns both directory entries and
    /// their attributes in a single call, reducing kernel/userspace round trips.
    /// Uses readdir_plus to fetch entries with stats in a single database query.
    fn readdirplus(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            let entries = match fs.readdir_plus(&path).await {
                Ok(Some(entries)) => entries,
                Ok(None) => {
                    reply.error(libc::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            // Get current directory stats for "."
            let dir_stats = fs.stat(&path).await.ok().flatten();

            // Determine parent inode and stats for ".." entry
            let (parent_ino, parent_stats) = if ino == 1 {
                (1u64, dir_stats.clone()) // Root's parent is itself
            } else {
                let parent_path = Path::new(&path)
                    .parent()
                    .map(|p| {
                        let s = p.to_string_lossy().to_string();
                        if s.is_empty() {
                            "/".to_string()
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| "/".to_string());

                let parent_stats = fs.stat(&parent_path).await.ok().flatten();
                let parent_ino = if parent_path == "/" {
                    1u64
                } else {
                    parent_stats.as_ref().map(|s| s.ino as u64).unwrap_or(1)
                };
                (parent_ino, parent_stats)
            };

            let mut offset_counter = 0i64;

            // Add "." entry
            if offset <= offset_counter {
                if let Some(ref stats) = dir_stats {
                    let attr = fillattr(stats, uid, gid);
                    if reply.add(ino, offset_counter + 1, ".", &TTL, &attr, 0) {
                        reply.ok();
                        return;
                    }
                }
            }
            offset_counter += 1;

            // Add ".." entry
            if offset <= offset_counter {
                if let Some(ref stats) = parent_stats {
                    let attr = fillattr(stats, uid, gid);
                    if reply.add(parent_ino, offset_counter + 1, "..", &TTL, &attr, 0) {
                        reply.ok();
                        return;
                    }
                }
            }
            offset_counter += 1;

            // Add directory entries with their attributes
            for entry in &entries {
                if offset <= offset_counter {
                    let entry_path = if path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", path, entry.name)
                    };

                    let attr = fillattr(&entry.stats, uid, gid);
                    path_cache.lock().insert(entry.stats.ino as u64, entry_path);

                    if reply.add(
                        entry.stats.ino as u64,
                        offset_counter + 1,
                        &entry.name,
                        &TTL,
                        &attr,
                        0,
                    ) {
                        reply.ok();
                        return;
                    }
                }
                offset_counter += 1;
            }

            reply.ok();
        });
    }

    /// Creates a new directory.
    ///
    /// Creates a directory at `name` under `parent`, then stats it to return
    /// proper attributes and cache the inode mapping.
    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            if fs.mkdir(&path).await.is_err() {
                reply.error(libc::EIO);
                return;
            }

            // Get the new directory's stats
            match fs.stat(&path).await {
                Ok(Some(stats)) => {
                    let attr = fillattr(&stats, uid, gid);
                    path_cache.lock().insert(attr.ino, path);
                    reply.entry(&TTL, &attr, 0);
                }
                _ => reply.error(libc::EIO),
            }
        });
    }

    /// Removes an empty directory.
    ///
    /// Verifies the target is a directory and is empty before removal.
    /// Returns `ENOTDIR` if not a directory, `ENOTEMPTY` if not empty.
    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        self.handle.spawn(async move {
            // Verify target is a directory
            let stats = match fs.lstat(&path).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    reply.error(libc::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            if !stats.is_directory() {
                reply.error(libc::ENOTDIR);
                return;
            }

            // Verify directory is empty
            match fs.readdir(&path).await {
                Ok(Some(entries)) if !entries.is_empty() => {
                    reply.error(libc::ENOTEMPTY);
                    return;
                }
                Ok(None) => {
                    reply.error(libc::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
                Ok(Some(_)) => {} // Empty directory, proceed
            }

            // Remove the directory
            let ino = stats.ino as u64;
            match fs.remove(&path).await {
                Ok(()) => {
                    path_cache.lock().remove(&ino);
                    reply.ok();
                }
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    // ─────────────────────────────────────────────────────────────
    // File Creation & Removal
    // ─────────────────────────────────────────────────────────────

    /// Creates and opens a new file.
    ///
    /// Creates an empty file at `name` under `parent`, allocates a file handle,
    /// and returns both the file attributes and handle for immediate use.
    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        let open_files = self.open_files.clone();
        let next_fh = self.next_fh.clone();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            // Create empty file
            if fs.write_file(&path, &[]).await.is_err() {
                reply.error(libc::EIO);
                return;
            }

            // Get the new file's stats
            let attr = match fs.stat(&path).await {
                Ok(Some(stats)) => {
                    let attr = fillattr(&stats, uid, gid);
                    path_cache.lock().insert(attr.ino, path.clone());
                    attr
                }
                _ => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            // Open the file to get a file handle
            let file = match fs.open(&path).await {
                Ok(file) => file,
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            let fh = next_fh.fetch_add(1, Ordering::SeqCst);
            open_files.lock().insert(fh, OpenFile { file });

            reply.created(&TTL, &attr, 0, fh, 0);
        });
    }

    /// Creates a symbolic link.
    ///
    /// Creates a symlink at `name` under `parent` pointing to `link`.
    fn symlink(
        &mut self,
        _req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(path) = self.lookup_path(parent, link_name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let Some(target_str) = target.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        let target_owned = target_str.to_string();
        let uid = self.uid;
        let gid = self.gid;
        self.handle.spawn(async move {
            if fs.symlink(&target_owned, &path).await.is_err() {
                reply.error(libc::EIO);
                return;
            }

            // Get the new symlink's stats
            match fs.lstat(&path).await {
                Ok(Some(stats)) => {
                    let attr = fillattr(&stats, uid, gid);
                    path_cache.lock().insert(attr.ino, path);
                    reply.entry(&TTL, &attr, 0);
                }
                _ => reply.error(libc::EIO),
            }
        });
    }

    /// Removes a file (unlinks it from the directory).
    ///
    /// Gets the file's inode before removal to clean up the path cache.
    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        self.handle.spawn(async move {
            // Get inode before removing so we can uncache
            let stats = match fs.lstat(&path).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    reply.error(libc::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            if stats.is_directory() {
                reply.error(libc::EISDIR);
                return;
            }

            let ino = stats.ino as u64;
            match fs.remove(&path).await {
                Ok(()) => {
                    path_cache.lock().remove(&ino);
                    reply.ok();
                }
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Renames a file or directory.
    ///
    /// Moves `name` from `parent` to `newname` under `newparent`. Updates the
    /// path cache accordingly, removing any replaced destination entry.
    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(from_path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let Some(to_path) = self.lookup_path(newparent, newname) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_cache = self.path_cache.clone();
        self.handle.spawn(async move {
            // Get source inode before rename so we can update cache
            let src_ino = fs
                .stat(&from_path)
                .await
                .ok()
                .flatten()
                .map(|s| s.ino as u64);

            // Check if destination exists and get its inode for cache cleanup
            let dst_ino = fs.stat(&to_path).await.ok().flatten().map(|s| s.ino as u64);

            // Perform the rename
            match fs.rename(&from_path, &to_path).await {
                Ok(()) => {
                    let mut cache = path_cache.lock();
                    // Update path cache: remove old path, add new path
                    if let Some(ino) = src_ino {
                        cache.remove(&ino);
                        cache.insert(ino, to_path);
                    }
                    // Remove destination from cache if it was replaced
                    if let Some(ino) = dst_ino {
                        cache.remove(&ino);
                    }
                    reply.ok();
                }
                Err(e) => {
                    let errno = e
                        .downcast_ref::<FsError>()
                        .map(|fs_err| fs_err.to_errno())
                        .unwrap_or(libc::EIO);
                    reply.error(errno);
                }
            }
        });
    }

    // ─────────────────────────────────────────────────────────────
    // File I/O Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Opens a file for reading or writing.
    ///
    /// Allocates a file handle and opens the file in the filesystem layer.
    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let open_files = self.open_files.clone();
        let next_fh = self.next_fh.clone();
        self.handle.spawn(async move {
            match fs.open(&path).await {
                Ok(file) => {
                    let fh = next_fh.fetch_add(1, Ordering::SeqCst);
                    open_files.lock().insert(fh, OpenFile { file });
                    reply.opened(fh, 0);
                }
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Reads data using the file handle.
    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let file = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.file.clone()
        };

        self.handle.spawn(async move {
            match file.pread(offset as u64, size as u64).await {
                Ok(data) => reply.data(&data),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Writes data using the file handle.
    fn write(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let file = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.file.clone()
        };

        let data_len = data.len();
        let data_vec = data.to_vec();
        self.handle.spawn(async move {
            match file.pwrite(offset as u64, &data_vec).await {
                Ok(()) => reply.written(data_len as u32),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Flushes data to the backend storage.
    ///
    /// Since writes go directly to the database, this is a no-op.
    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let open_files = self.open_files.lock();
        if open_files.contains_key(&fh) {
            reply.ok();
        } else {
            reply.error(libc::EBADF);
        }
    }

    /// Synchronizes file data to persistent storage using the file handle.
    ///
    /// This now uses the file handle's fsync which knows which layer(s) the
    /// file exists in, avoiding errors when a file only exists in one layer.
    fn fsync(&mut self, _req: &Request, _ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let file = {
            let open_files = self.open_files.lock();
            match open_files.get(&fh) {
                Some(open_file) => open_file.file.clone(),
                None => {
                    reply.error(libc::EBADF);
                    return;
                }
            }
        };

        self.handle.spawn(async move {
            match file.fsync().await {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(libc::EIO),
            }
        });
    }

    /// Releases (closes) an open file handle.
    ///
    /// Removes the file handle from the open files table.
    /// Since writes go directly to the database, no flushing is needed.
    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.open_files.lock().remove(&fh);
        reply.ok();
    }

    /// Returns filesystem statistics.
    ///
    /// Queries actual usage from the SDK and reports it to tools like `df`.
    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        let fs = self.fs.clone();
        self.handle.spawn(async move {
            const BLOCK_SIZE: u64 = 4096;
            const TOTAL_INODES: u64 = 1_000_000; // Virtual limit
            const MAX_NAMELEN: u32 = 255;

            let (used_blocks, used_inodes) = match fs.statfs().await {
                Ok(stats) => {
                    let used_blocks = stats.bytes_used.div_ceil(BLOCK_SIZE);
                    (used_blocks, stats.inodes)
                }
                Err(_) => (0, 1), // Fallback: just root inode
            };

            // Report a large virtual capacity so tools don't think we're out of space
            const TOTAL_BLOCKS: u64 = 1024 * 1024 * 1024; // ~4TB virtual size
            let free_blocks = TOTAL_BLOCKS.saturating_sub(used_blocks);
            let free_inodes = TOTAL_INODES.saturating_sub(used_inodes);

            reply.statfs(
                TOTAL_BLOCKS,
                free_blocks,
                free_blocks,
                TOTAL_INODES,
                free_inodes,
                BLOCK_SIZE as u32,
                MAX_NAMELEN,       // namelen: maximum filename length
                BLOCK_SIZE as u32, // frsize: fragment size
            );
        });
    }
}

impl AgentFSFuse {
    /// Create a new FUSE filesystem adapter wrapping a FileSystem instance.
    ///
    /// The provided Tokio runtime handle is used to spawn async FileSystem operations
    /// from within synchronous FUSE callbacks, allowing concurrent execution.
    ///
    /// The uid and gid are used for all file ownership to avoid "dubious ownership"
    /// errors from tools like git that check file ownership.
    fn new(fs: Arc<dyn FileSystem>, handle: Handle, uid: u32, gid: u32) -> Self {
        Self {
            fs,
            handle,
            path_cache: Arc::new(Mutex::new(HashMap::new())),
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(AtomicU64::new(1)),
            uid,
            gid,
        }
    }

    /// Resolve a full path from a parent inode and child name.
    ///
    /// Similar to the Linux kernel's dentry lookup (`d_lookup`), this method
    /// reconstructs the full pathname by looking up the parent's path in our
    /// inode-to-path cache and appending the child name.
    ///
    /// Returns `None` if the parent inode is not in the cache or the name
    /// contains invalid UTF-8.
    fn lookup_path(&self, parent_ino: u64, name: &OsStr) -> Option<String> {
        let path_cache = self.path_cache.lock();
        let parent_path = path_cache.get(&parent_ino)?;
        let name_str = name.to_str()?;

        if parent_path == "/" {
            Some(format!("/{}", name_str))
        } else {
            Some(format!("{}/{}", parent_path, name_str))
        }
    }

    /// Retrieve a path from an inode number.
    ///
    /// Similar to the Linux kernel's `d_path()`, this performs the reverse
    /// lookup from inode to pathname.
    ///
    /// Returns `None` if the inode is not in the cache.
    fn get_path(&self, ino: u64) -> Option<String> {
        self.path_cache.lock().get(&ino).cloned()
    }

    /// Add an inode → path mapping to the path cache.
    ///
    /// Similar to the Linux kernel's `d_add()`, this associates an inode
    /// with its full pathname for later lookup.
    fn add_path(&self, ino: u64, path: String) {
        let mut path_cache = self.path_cache.lock();
        path_cache.insert(ino, path);
    }
}

// ─────────────────────────────────────────────────────────────
// Attribute Conversion
// ─────────────────────────────────────────────────────────────

/// Fill a `FileAttr` from AgentFS stats.
///
/// Similar to the Linux kernel's `generic_fillattr()`, this converts
/// filesystem-specific stat information into the VFS attribute structure.
///
/// The uid and gid parameters override the stored values to ensure proper
/// file ownership reporting (avoids "dubious ownership" errors from git).
fn fillattr(stats: &Stats, uid: u32, gid: u32) -> FileAttr {
    let kind = if stats.is_directory() {
        FileType::Directory
    } else if stats.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    FileAttr {
        ino: stats.ino as u64,
        size: stats.size as u64,
        blocks: ((stats.size + 511) / 512) as u64,
        atime: UNIX_EPOCH + Duration::from_secs(stats.atime as u64),
        mtime: UNIX_EPOCH + Duration::from_secs(stats.mtime as u64),
        ctime: UNIX_EPOCH + Duration::from_secs(stats.ctime as u64),
        crtime: UNIX_EPOCH,
        kind,
        perm: (stats.mode & 0o777) as u16,
        nlink: stats.nlink,
        uid,
        gid,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

pub fn mount(
    fs: Arc<dyn FileSystem>,
    opts: FuseMountOptions,
    runtime: Runtime,
) -> anyhow::Result<()> {
    // Use provided uid/gid or default to current user
    // This avoids "dubious ownership" errors from git and similar tools
    let uid = opts.uid.unwrap_or_else(|| unsafe { libc::getuid() });
    let gid = opts.gid.unwrap_or_else(|| unsafe { libc::getgid() });

    // Get handle from runtime and enter the runtime context so spawned tasks can run
    let handle = runtime.handle().clone();
    let _guard = runtime.enter();

    let fs = AgentFSFuse::new(fs, handle, uid, gid);

    fs.add_path(1, "/".to_string());

    let mut mount_opts = vec![MountOption::FSName(opts.fsname)];
    if opts.auto_unmount {
        mount_opts.push(MountOption::AutoUnmount);
    }
    if opts.allow_root {
        mount_opts.push(MountOption::AllowRoot);
    }

    fuser::mount2(fs, &opts.mountpoint, &mount_opts)?;

    Ok(())
}
