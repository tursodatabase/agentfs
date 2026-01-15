use crate::fuser::{
    consts::{
        FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_NO_OPENDIR_SUPPORT, FUSE_PARALLEL_DIROPS,
        FUSE_WRITEBACK_CACHE,
    },
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request,
};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::{BoxedFile, FileSystem, Stats};
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
use tokio::runtime::Runtime;
use tracing;

/// Convert an SDK error to an errno code for FUSE replies.
///
/// If the error is a filesystem-specific FsError, returns the appropriate
/// errno code (ENOENT, EEXIST, ENOTDIR, etc.). Database busy errors and
/// connection pool timeouts return EAGAIN to signal the caller should retry.
/// Otherwise falls back to EIO.
fn error_to_errno(e: &SdkError) -> i32 {
    match e {
        SdkError::Fs(fs_err) => fs_err.to_errno(),
        SdkError::Io(io_err) => io_err.raw_os_error().unwrap_or(libc::EIO),
        SdkError::Database(turso::Error::Busy(_)) => libc::EAGAIN,
        SdkError::ConnectionPoolTimeout => libc::EAGAIN,
        _ => libc::EIO,
    }
}

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
    /// Allow other system users to access the mount.
    /// Requires 'user_allow_other' in /etc/fuse.conf for non-root users.
    pub allow_other: bool,
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

struct AgentFSFuse {
    fs: Arc<dyn FileSystem>,
    runtime: Runtime,
    path_cache: Arc<Mutex<HashMap<u64, String>>>,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
    /// Lossy string representation of the absolute mountpoint path.
    /// This is used to avoid looking up ourselves inside ourselves,
    /// e.g., when we mount an under filesystem `/` at /mntpnt,
    /// we do not want to look up `/mntpnt/mntpnt`, because the handler will then try
    /// to lookup `/mntpnt` from he under filesystem, which will hit our mountpoint again,
    /// causing a deadlock.
    mountpoint_path: String,
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
        tracing::debug!("FUSE::init");
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
        tracing::debug!("FUSE::lookup: parent={}, name={:?}", parent, name);
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let fs = self.fs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.lstat(&path).await;
            (result, path)
        });
        match result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Retrieves file attributes for a given inode.
    ///
    /// Returns metadata (size, permissions, timestamps, etc.) for the file or
    /// directory identified by `ino`. Root inode (1) is handled specially.
    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        tracing::debug!("FUSE::getattr: ino={}", ino);
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.lstat(&path).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Reads the target of a symbolic link.
    ///
    /// Returns the path that the symlink points to. This is called by operations
    /// like `ls -l` to display symlink targets.
    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        tracing::debug!("FUSE::readlink: ino={}", ino);
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.readlink(&path).await });

        match result {
            Ok(Some(target)) => reply.data(target.as_bytes()),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Sets file attributes, handling truncate and chmod operations.
    ///
    /// Currently `size` changes (truncate) and `mode` changes (chmod) are supported.
    /// Other attribute changes (uid, gid, timestamps) are accepted but ignored.
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<crate::fuser::TimeOrNow>,
        _mtime: Option<crate::fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        tracing::debug!(
            "FUSE::setattr: ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}",
            ino,
            mode,
            uid,
            gid,
            size
        );
        // Handle chmod
        if let Some(new_mode) = mode {
            let Some(path) = self.path_cache.lock().get(&ino).cloned() else {
                reply.error(libc::ENOENT);
                return;
            };

            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chmod(&path, new_mode).await });

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Handle chown
        if uid.is_some() || gid.is_some() {
            let Some(path) = self.path_cache.lock().get(&ino).cloned() else {
                reply.error(libc::ENOENT);
                return;
            };

            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chown(&path, uid, gid).await });

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Handle truncate
        if let Some(new_size) = size {
            let result = if let Some(fh) = fh {
                // Use file handle if available (ftruncate)
                let file = {
                    let open_files = self.open_files.lock();
                    open_files.get(&fh).map(|f| f.file.clone())
                };

                if let Some(file) = file {
                    self.runtime
                        .block_on(async move { file.truncate(new_size).await })
                } else {
                    reply.error(libc::EBADF);
                    return;
                }
            } else {
                // Open file and truncate via file handle
                let Some(path) = self.path_cache.lock().get(&ino).cloned() else {
                    reply.error(libc::ENOENT);
                    return;
                };

                let fs = self.fs.clone();
                self.runtime.block_on(async move {
                    let file = fs.open(&path).await?;
                    file.truncate(new_size).await
                })
            };

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Return updated attributes
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.stat(&path).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
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
        tracing::debug!("FUSE::readdir: ino={}, offset={}", ino, offset);
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let (entries_result, path) = self.runtime.block_on(async move {
            let result = fs.readdir_plus(&path).await;
            (result, path)
        });

        let entries = match entries_result {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
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
                let fs = self.fs.clone();
                match self
                    .runtime
                    .block_on(async move { fs.stat(&parent_path).await })
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

            self.add_path(entry.stats.ino as u64, entry_path);
            all_entries.push((entry.stats.ino as u64, kind, entry.name.as_str()));
        }

        for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
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
        tracing::debug!("FUSE::readdirplus: ino={}, offset={}", ino, offset);
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let (entries_result, path) = self.runtime.block_on(async move {
            let result = fs.readdir_plus(&path).await;
            (result, path)
        });

        let entries = match entries_result {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        // Get current directory stats for "."
        let fs = self.fs.clone();
        let path_for_stat = path.clone();
        let dir_stats = self
            .runtime
            .block_on(async move { fs.stat(&path_for_stat).await })
            .ok()
            .flatten();

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

            if parent_path == "/" {
                let fs = self.fs.clone();
                let parent_stats = self
                    .runtime
                    .block_on(async move { fs.stat(&parent_path).await })
                    .ok()
                    .flatten();
                (1u64, parent_stats)
            } else {
                let fs = self.fs.clone();
                let parent_stats = self
                    .runtime
                    .block_on(async move { fs.stat(&parent_path).await })
                    .ok()
                    .flatten();
                let parent_ino = parent_stats.as_ref().map(|s| s.ino as u64).unwrap_or(1);
                (parent_ino, parent_stats)
            }
        };

        // Build the entries list with full attributes
        let mut offset_counter = 0i64;

        // Add "." entry
        if offset <= offset_counter {
            if let Some(ref stats) = dir_stats {
                let attr = fillattr(stats);
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
                let attr = fillattr(stats);
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

                let attr = fillattr(&entry.stats);
                self.add_path(entry.stats.ino as u64, entry_path);

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
    }

    /// Creates a new directory.
    ///
    /// Creates a directory at `name` under `parent`, then stats it to return
    /// proper attributes and cache the inode mapping.
    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!("FUSE::mkdir: parent={}, name={:?}", parent, name);
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.mkdir(&path, uid, gid).await;
            (result, path)
        });

        if let Err(e) = result {
            reply.error(error_to_errno(&e));
            return;
        }

        // Get the new directory's stats
        let fs = self.fs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = fs.stat(&path).await;
            (result, path)
        });

        match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Removes an empty directory.
    ///
    /// Verifies the target is a directory and is empty before removal.
    /// Returns `ENOTDIR` if not a directory, `ENOTEMPTY` if not empty.
    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::rmdir: parent={}, name={:?}", parent, name);
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Verify target is a directory
        let fs = self.fs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = fs.lstat(&path).await;
            (result, path)
        });

        let stats = match stat_result {
            Ok(Some(s)) => s,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        if !stats.is_directory() {
            reply.error(libc::ENOTDIR);
            return;
        }

        // Verify directory is empty
        let fs = self.fs.clone();
        let (readdir_result, path) = self.runtime.block_on(async move {
            let result = fs.readdir(&path).await;
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
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
            Ok(Some(_)) => {} // Empty directory, proceed
        }

        // Remove the directory
        let ino = stats.ino as u64;
        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.remove(&path).await });

        match result {
            Ok(()) => {
                self.drop_path(ino);
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
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
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        tracing::debug!(
            "FUSE::create: parent={}, name={:?}, mode={:o}",
            parent,
            name,
            mode
        );
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Create file with mode, get stats and file handle in one operation
        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let path_for_create = path.clone();
        let result = self
            .runtime
            .block_on(async move { fs.create_file(&path_for_create, mode, uid, gid).await });

        match result {
            Ok((stats, file)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);

                let fh = self.alloc_fh();
                self.open_files.lock().insert(fh, OpenFile { file });

                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Creates a symbolic link.
    ///
    /// Creates a symlink at `name` under `parent` pointing to `link`.
    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::symlink: parent={}, link_name={:?}, target={:?}",
            parent,
            link_name,
            target
        );
        let Some(path) = self.lookup_path(parent, link_name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let Some(target_str) = target.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let target_owned = target_str.to_string();
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.symlink(&target_owned, &path, uid, gid).await;
            (result, path)
        });

        if let Err(e) = result {
            reply.error(error_to_errno(&e));
            return;
        }

        // Get the new symlink's stats
        let fs = self.fs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = fs.lstat(&path).await;
            (result, path)
        });

        match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Creates a hard link.
    ///
    /// Creates a new directory entry `newname` under `newparent` that refers to the
    /// same inode as `ino`. The link count of the inode is incremented.
    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::link: ino={}, newparent={}, newname={:?}",
            ino,
            newparent,
            newname
        );
        // Get the path for the source inode
        let Some(oldpath) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Get the path for the new link
        let Some(newpath) = self.lookup_path(newparent, newname) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let (result, newpath) = self.runtime.block_on(async move {
            let result = fs.link(&oldpath, &newpath).await;
            (result, newpath)
        });

        if let Err(e) = result {
            reply.error(error_to_errno(&e));
            return;
        }

        // Get the new link's stats
        let fs = self.fs.clone();
        let (stat_result, newpath) = self.runtime.block_on(async move {
            let result = fs.lstat(&newpath).await;
            (result, newpath)
        });

        match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                self.add_path(attr.ino, newpath);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Removes a file (unlinks it from the directory).
    ///
    /// Gets the file's inode before removal to clean up the path cache.
    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::unlink: parent={}, name={:?}", parent, name);
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Get inode before removing so we can uncache
        let fs = self.fs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = fs.lstat(&path).await;
            (result, path)
        });

        let stats = match &stat_result {
            Ok(Some(s)) => s,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(e));
                return;
            }
        };

        if stats.is_directory() {
            reply.error(libc::EISDIR);
            return;
        }

        let ino = stats.ino as u64;
        let nlink = stats.nlink;

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.remove(&path).await });

        match result {
            Ok(()) => {
                // Only drop from path_cache if this was the last link.
                // If nlink > 1, there are other hard links that still reference
                // this inode, and the path_cache entry points to one of them.
                if nlink <= 1 {
                    self.drop_path(ino);
                }
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
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
        tracing::debug!(
            "FUSE::rename: parent={}, name={:?}, newparent={}, newname={:?}",
            parent,
            name,
            newparent,
            newname
        );
        let Some(from_path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };

        let Some(to_path) = self.lookup_path(newparent, newname) else {
            reply.error(libc::ENOENT);
            return;
        };

        // Get source inode before rename so we can update cache
        let fs = self.fs.clone();
        let (src_stat, from_path) = self.runtime.block_on(async move {
            let result = fs.stat(&from_path).await;
            (result, from_path)
        });

        let src_ino = src_stat.ok().flatten().map(|s| s.ino as u64);

        // Check if destination exists and get its inode for cache cleanup
        let fs = self.fs.clone();
        let (dst_stat, to_path) = self.runtime.block_on(async move {
            let result = fs.stat(&to_path).await;
            (result, to_path)
        });

        let dst_ino = dst_stat.ok().flatten().map(|s| s.ino as u64);

        // Perform the rename
        let fs = self.fs.clone();
        let (result, to_path) = self.runtime.block_on(async move {
            let result = fs.rename(&from_path, &to_path).await;
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
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // File I/O Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Opens a file for reading or writing.
    ///
    /// Allocates a file handle and opens the file in the filesystem layer.
    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        tracing::debug!("FUSE::open: ino={}", ino);
        let Some(path) = self.get_path(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let fs = self.fs.clone();
        let path_clone = path.clone();
        let result = self
            .runtime
            .block_on(async move { fs.open(&path_clone).await });

        match result {
            Ok(file) => {
                let fh = self.alloc_fh();
                self.open_files.lock().insert(fh, OpenFile { file });
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
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
        tracing::debug!("FUSE::read: fh={}, offset={}, size={}", fh, offset, size);
        let file = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.file.clone()
        };

        let result = self
            .runtime
            .block_on(async move { file.pread(offset as u64, size as u64).await });

        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(error_to_errno(&e)),
        }
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
        tracing::debug!(
            "FUSE::write: fh={}, offset={}, data_len={}",
            fh,
            offset,
            data.len()
        );
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
        let result = self
            .runtime
            .block_on(async move { file.pwrite(offset as u64, &data_vec).await });

        match result {
            Ok(()) => reply.written(data_len as u32),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Flushes data to the backend storage.
    ///
    /// Since writes go directly to the database, this is a no-op.
    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        tracing::debug!("FUSE::flush: fh={}", fh);
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
        tracing::debug!("FUSE::fsync: fh={}", fh);
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

        let result = self.runtime.block_on(async move { file.fsync().await });

        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error_to_errno(&e)),
        }
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
        tracing::debug!("FUSE::release: fh={}", fh);
        self.open_files.lock().remove(&fh);
        reply.ok();
    }

    /// Returns filesystem statistics.
    ///
    /// Queries actual usage from the SDK and reports it to tools like `df`.
    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        tracing::debug!("FUSE::statfs");
        const BLOCK_SIZE: u64 = 4096;
        const TOTAL_INODES: u64 = 1_000_000; // Virtual limit
        const MAX_NAMELEN: u32 = 255;

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.statfs().await });

        let (used_blocks, used_inodes) = match result {
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
    }
}

impl AgentFSFuse {
    /// Create a new FUSE filesystem adapter wrapping a FileSystem instance.
    ///
    /// The provided Tokio runtime is used to execute async FileSystem operations
    /// from within synchronous FUSE callbacks via `block_on`.
    fn new(fs: Arc<dyn FileSystem>, runtime: Runtime, mountpoint_path: PathBuf) -> Self {
        Self {
            fs,
            runtime,
            path_cache: Arc::new(Mutex::new(HashMap::new())),
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
            mountpoint_path: mountpoint_path.as_os_str().to_string_lossy().to_string(),
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

        let path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        if path.starts_with(&self.mountpoint_path) {
            // Cut the head off here so we never try to lookup anything that falls within
            // our own mount inside our handlers by immediately returning ENOENT.
            None
        } else {
            Some(path)
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
///
/// The uid and gid parameters override the stored values to ensure proper
/// file ownership reporting (avoids "dubious ownership" errors from git).
fn fillattr(stats: &Stats) -> FileAttr {
    let kind = if stats.is_directory() {
        FileType::Directory
    } else if stats.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    let size = if stats.is_directory() {
        4096_u64 // Standard directory size
    } else {
        stats.size as u64
    };

    FileAttr {
        ino: stats.ino as u64,
        size,
        blocks: size.div_ceil(512),
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

/// Check if allow_other is supported for FUSE mounts.
///
/// Returns true if the current user is root or if user_allow_other is enabled
/// in /etc/fuse.conf.
fn allow_other_supported() -> bool {
    // Root can always use allow_other
    if unsafe { libc::getuid() } == 0 {
        return true;
    }

    // Check if user_allow_other is enabled in /etc/fuse.conf
    if let Ok(contents) = std::fs::read_to_string("/etc/fuse.conf") {
        for line in contents.lines() {
            let line = line.trim();
            // Skip comments and empty lines
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line == "user_allow_other" {
                return true;
            }
        }
    }

    false
}

pub fn mount(
    fs: Arc<dyn FileSystem>,
    opts: FuseMountOptions,
    runtime: Runtime,
) -> anyhow::Result<()> {
    let fs = AgentFSFuse::new(fs, runtime, opts.mountpoint.clone());

    fs.add_path(1, "/".to_string());

    let mut mount_opts = vec![
        MountOption::FSName(opts.fsname),
        // Enable kernel-level permission checking based on file mode/uid/gid
        MountOption::DefaultPermissions,
    ];

    // Allow users other than the one who mounted the filesystem to access it.
    // This requires either running as root or having user_allow_other enabled
    // in /etc/fuse.conf.
    if opts.allow_other {
        if allow_other_supported() {
            mount_opts.push(MountOption::AllowOther);
        } else {
            anyhow::bail!(
                "FUSE allow_other not supported. Add 'user_allow_other' to /etc/fuse.conf or run as root."
            );
        }
    }

    if opts.auto_unmount {
        mount_opts.push(MountOption::AutoUnmount);
    }
    if opts.allow_root {
        mount_opts.push(MountOption::AllowRoot);
    }

    crate::fuser::mount2(fs, &opts.mountpoint, &mount_opts)?;

    Ok(())
}
