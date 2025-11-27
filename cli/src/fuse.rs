use agentfs_sdk::{AgentFS, FsError, Stats};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;

const TTL: Duration = Duration::from_secs(1);

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
}

/// Tracks an open file's write buffer
struct OpenFile {
    path: String,
    data: Vec<u8>,
    dirty: bool,
}

struct AgentFSFuse {
    agentfs: Arc<AgentFS>,
    runtime: Runtime,
    path_cache: Arc<Mutex<HashMap<u64, String>>>,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
}

impl Filesystem for AgentFSFuse {
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
        let agentfs = self.agentfs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&path).await;
            (result, path)
        });
        match result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
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

        let agentfs = self.agentfs.clone();
        let result = self
            .runtime
            .block_on(async move { agentfs.fs.stat(&path).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
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
        // Handle truncate
        if let Some(new_size) = size {
            if let Some(fh) = fh {
                let mut open_files = self.open_files.lock();
                if let Some(file) = open_files.get_mut(&fh) {
                    file.data.resize(new_size as usize, 0);
                    file.dirty = true;
                }
            } else {
                // Truncate without open file handle - need to read, truncate, write
                let Some(path) = self.path_cache.lock().get(&ino).cloned() else {
                    reply.error(libc::ENOENT);
                    return;
                };

                let agentfs = self.agentfs.clone();
                let (read_result, path) = self.runtime.block_on(async move {
                    let result = agentfs.fs.read_file(&path).await;
                    (result, path)
                });

                let mut data = match read_result {
                    Ok(Some(d)) => d,
                    Ok(None) => Vec::new(),
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };

                data.resize(new_size as usize, 0);

                let agentfs = self.agentfs.clone();
                let write_result = self
                    .runtime
                    .block_on(async move { agentfs.fs.write_file(&path, &data).await });

                if write_result.is_err() {
                    reply.error(libc::EIO);
                    return;
                }
            }
        }

        // Return updated attributes
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let agentfs = self.agentfs.clone();
        let result = self
            .runtime
            .block_on(async move { agentfs.fs.stat(&path).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Directory Operations
    // ─────────────────────────────────────────────────────────────

    /// Reads directory entries for the given inode.
    ///
    /// Returns "." and ".." entries followed by the directory contents.
    /// Each entry's inode is cached for subsequent lookups.
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

        let agentfs = self.agentfs.clone();
        let (entries_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.readdir(&path).await;
            (result, path)
        });

        let entries = match entries_result {
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
                let agentfs = self.agentfs.clone();
                match self
                    .runtime
                    .block_on(async move { agentfs.fs.stat(&parent_path).await })
                {
                    Ok(Some(stats)) => stats.ino as u64,
                    _ => 1, // Fallback to root if parent lookup fails
                }
            }
        };

        let mut all_entries = vec![
            (ino, FileType::Directory, "."),
            (parent_ino, FileType::Directory, ".."),
        ];

        for entry_name in &entries {
            let entry_path = if path == "/" {
                format!("/{}", entry_name)
            } else {
                format!("{}/{}", path, entry_name)
            };

            let agentfs = self.agentfs.clone();
            let (stats_result, entry_path) = self.runtime.block_on(async move {
                let result = agentfs.fs.stat(&entry_path).await;
                (result, entry_path)
            });

            if let Ok(Some(stats)) = stats_result {
                let kind = if stats.is_directory() {
                    FileType::Directory
                } else if stats.is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };

                self.add_path(stats.ino as u64, entry_path);
                all_entries.push((stats.ino as u64, kind, entry_name.as_str()));
            }
        }

        for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
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

        let agentfs = self.agentfs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.mkdir(&path).await;
            (result, path)
        });

        if result.is_err() {
            reply.error(libc::EIO);
            return;
        }

        // Get the new directory's stats
        let agentfs = self.agentfs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&path).await;
            (result, path)
        });

        match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            _ => {
                // Fail the operation if we cannot stat the new directory
                reply.error(libc::EIO);
            }
        }
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

        // Verify target is a directory
        let agentfs = self.agentfs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&path).await;
            (result, path)
        });

        let stats = match stat_result {
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
        let agentfs = self.agentfs.clone();
        let (readdir_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.readdir(&path).await;
            (result, path)
        });

        match readdir_result {
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
        let agentfs = self.agentfs.clone();
        let result = self
            .runtime
            .block_on(async move { agentfs.fs.remove(&path).await });

        match result {
            Ok(()) => {
                self.drop_path(ino);
                reply.ok();
            }
            Err(_) => reply.error(libc::EIO),
        }
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

        // Create empty file
        let agentfs = self.agentfs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.write_file(&path, &[]).await;
            (result, path)
        });

        if result.is_err() {
            reply.error(libc::EIO);
            return;
        }

        // Get the new file's stats
        let agentfs = self.agentfs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&path).await;
            (result, path)
        });

        let attr = match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path.clone());
                attr
            }
            _ => {
                // Fail the operation if we cannot stat the new file
                reply.error(libc::EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        let mut open_files = self.open_files.lock();
        open_files.insert(
            fh,
            OpenFile {
                path,
                data: Vec::new(),
                dirty: false,
            },
        );

        reply.created(&TTL, &attr, 0, fh, 0);
    }

    /// Removes a file (unlinks it from the directory).
    ///
    /// Gets the file's inode before removal to clean up the path cache.
    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Get inode before removing so we can uncache
        let agentfs = self.agentfs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&path).await;
            (result, path)
        });

        let ino = stat_result.ok().flatten().map(|s| s.ino as u64);

        let agentfs = self.agentfs.clone();
        let result = self
            .runtime
            .block_on(async move { agentfs.fs.remove(&path).await });

        match result {
            Ok(()) => {
                if let Some(ino) = ino {
                    self.drop_path(ino);
                }
                reply.ok();
            }
            Err(_) => reply.error(libc::EIO),
        }
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

        // Get source inode before rename so we can update cache
        let agentfs = self.agentfs.clone();
        let (src_stat, from_path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&from_path).await;
            (result, from_path)
        });

        let src_ino = src_stat.ok().flatten().map(|s| s.ino as u64);

        // Check if destination exists and get its inode for cache cleanup
        let agentfs = self.agentfs.clone();
        let (dst_stat, to_path) = self.runtime.block_on(async move {
            let result = agentfs.fs.stat(&to_path).await;
            (result, to_path)
        });

        let dst_ino = dst_stat.ok().flatten().map(|s| s.ino as u64);

        // Perform the rename
        let agentfs = self.agentfs.clone();
        let (result, to_path) = self.runtime.block_on(async move {
            let result = agentfs.fs.rename(&from_path, &to_path).await;
            (result, to_path)
        });

        match result {
            Ok(()) => {
                // Update path cache: remove old path, add new path
                if let Some(ino) = src_ino {
                    self.drop_path(ino);
                    self.add_path(ino, to_path);
                }
                // Remove destination from cache if it was replaced
                if let Some(ino) = dst_ino {
                    self.drop_path(ino);
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
    }

    // ─────────────────────────────────────────────────────────────
    // File I/O Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Opens a file and loads its contents into memory.
    ///
    /// Allocates a file handle and reads the file data into an in-memory buffer.
    /// Subsequent reads/writes operate on this buffer until flush or release.
    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Read current file contents into buffer
        let agentfs = self.agentfs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = agentfs.fs.read_file(&path).await;
            (result, path)
        });

        let data = match result {
            Ok(Some(data)) => data,
            Ok(None) => Vec::new(),
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        self.open_files.lock().insert(
            fh,
            OpenFile {
                path,
                data,
                dirty: false,
            },
        );

        reply.opened(fh, 0);
    }

    /// Reads data from an open file's in-memory buffer.
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
        let open_files = self.open_files.lock();
        let Some(file) = open_files.get(&fh) else {
            reply.error(libc::EBADF);
            return;
        };

        let offset = offset as usize;
        let size = size as usize;
        if offset < file.data.len() {
            let end = std::cmp::min(offset + size, file.data.len());
            reply.data(&file.data[offset..end]);
        } else {
            reply.data(&[]);
        }
    }

    /// Writes data to an open file's in-memory buffer.
    ///
    /// Marks the file as dirty for later flushing to the backend.
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
        let mut open_files = self.open_files.lock();
        let Some(file) = open_files.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };

        let offset = offset as usize;
        let end = offset + data.len();

        // Extend buffer if needed
        if end > file.data.len() {
            file.data.resize(end, 0);
        }

        file.data[offset..end].copy_from_slice(data);
        file.dirty = true;

        reply.written(data.len() as u32);
    }

    /// Flushes dirty data to the backend storage.
    ///
    /// Called when a file descriptor is closed (but file may still be open
    /// via other descriptors). Writes the in-memory buffer if modified.
    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let mut open_files = self.open_files.lock();
        let Some(file) = open_files.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };

        if file.dirty {
            let agentfs = self.agentfs.clone();
            let path = file.path.clone();
            let data = file.data.clone();
            drop(open_files);

            let result = self
                .runtime
                .block_on(async move { agentfs.fs.write_file(&path, &data).await });

            match result {
                Ok(()) => {
                    if let Some(f) = self.open_files.lock().get_mut(&fh) {
                        f.dirty = false;
                    }
                    reply.ok();
                }
                Err(_) => reply.error(libc::EIO),
            }
        } else {
            reply.ok();
        }
    }

    /// Releases (closes) an open file handle.
    ///
    /// Flushes any remaining dirty data and removes the file handle from
    /// the open files table.
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
        let Some(file) = self.open_files.lock().remove(&fh) else {
            reply.ok();
            return;
        };

        if file.dirty {
            let agentfs = self.agentfs.clone();
            let result = self
                .runtime
                .block_on(async move { agentfs.fs.write_file(&file.path, &file.data).await });

            match result {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(libc::EIO),
            }
        } else {
            reply.ok();
        }
    }

    /// Returns filesystem statistics.
    ///
    /// Reports virtual capacity limits to satisfy tools like `df`.
    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        // FIXME: We need a proper implementation here!
        const BLOCK_SIZE: u64 = 4096;
        const TOTAL_BLOCKS: u64 = 1024 * 1024; // ~4GB virtual size
        const FREE_BLOCKS: u64 = 1024 * 1024 - 1024;
        const TOTAL_INODES: u64 = 1_000_000;
        const FREE_INODES: u64 = 999_000;
        const MAX_NAMELEN: u32 = 255;

        reply.statfs(
            TOTAL_BLOCKS,
            FREE_BLOCKS,
            FREE_BLOCKS,
            TOTAL_INODES,
            FREE_INODES,
            BLOCK_SIZE as u32,
            MAX_NAMELEN,       // namelen: maximum filename length
            BLOCK_SIZE as u32, // frsize: fragment size
        );
    }
}

impl AgentFSFuse {
    /// Create a new FUSE filesystem adapter wrapping an AgentFS instance.
    ///
    /// The provided Tokio runtime is used to execute async AgentFS operations
    /// from within synchronous FUSE callbacks via `block_on`.
    fn new(agentfs: AgentFS, runtime: Runtime) -> Self {
        Self {
            agentfs: Arc::new(agentfs),
            runtime,
            path_cache: Arc::new(Mutex::new(HashMap::new())),
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
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

    /// Remove an inode from the path cache.
    ///
    /// Similar to the Linux kernel's `d_drop()`, this removes the inode's
    /// pathname mapping when the file or directory is deleted or renamed.
    fn drop_path(&self, ino: u64) {
        let mut path_cache = self.path_cache.lock();
        path_cache.remove(&ino);
    }

    /// Allocate a new file handle for tracking open files.
    ///
    /// Similar to the Linux kernel's `get_unused_fd()`, this returns a unique
    /// handle that identifies an open file throughout its lifetime.
    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }
}

// ─────────────────────────────────────────────────────────────
// Attribute Conversion
// ─────────────────────────────────────────────────────────────

/// Fill a `FileAttr` from AgentFS stats.
///
/// Similar to the Linux kernel's `generic_fillattr()`, this converts
/// filesystem-specific stat information into the VFS attribute structure.
fn fillattr(stats: &Stats) -> FileAttr {
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
        uid: stats.uid,
        gid: stats.gid,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

pub fn mount(agentfs: AgentFS, opts: FuseMountOptions, runtime: Runtime) -> anyhow::Result<()> {
    let fs = AgentFSFuse::new(agentfs, runtime);

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
