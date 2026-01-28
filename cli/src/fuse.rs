use crate::fuser::{
    consts::{
        FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_NO_OPENDIR_SUPPORT, FUSE_PARALLEL_DIROPS,
        FUSE_WRITEBACK_CACHE,
    },
    fuse_forget_one, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request,
};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFSOCK};
use agentfs_sdk::{BoxedFile, FileSystem, Stats, TimeChange};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::PathBuf,
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

/// Maximize the file descriptor limit by raising the soft limit to the hard limit.
///
/// This helps avoid "too many open files" errors when passthrough filesystems
/// cache O_PATH file descriptors for inode handles. Unlike raising the hard limit,
/// this does not require root privileges.
fn maximize_fd_limit() {
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    if result == 0 {
        let old_soft = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        let result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
        if result == 0 {
            tracing::debug!("Raised fd limit from {} to {}", old_soft, lim.rlim_max);
        } else {
            tracing::warn!(
                "Failed to raise fd limit: {}",
                std::io::Error::last_os_error()
            );
        }
    } else {
        tracing::warn!(
            "Failed to get fd limit: {}",
            std::io::Error::last_os_error()
        );
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
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
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
    /// Resolves `name` under the directory identified by `parent` inode.
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        tracing::debug!("FUSE::lookup: parent={}, name={:?}", parent, name);

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.lookup(parent as i64, &name_owned).await });

        match result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
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

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await });

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

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.readlink(ino as i64).await });

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
        atime: Option<crate::fuser::TimeOrNow>,
        mtime: Option<crate::fuser::TimeOrNow>,
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
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chmod(ino as i64, new_mode).await });

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Handle chown
        if uid.is_some() || gid.is_some() {
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chown(ino as i64, uid, gid).await });

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
                let fs = self.fs.clone();
                self.runtime.block_on(async move {
                    let file = fs.open(ino as i64).await?;
                    file.truncate(new_size).await
                })
            };

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Handle atime/mtime changes (utimensat)
        if atime.is_some() || mtime.is_some() {
            let new_atime = match atime {
                Some(crate::fuser::TimeOrNow::SpecificTime(t)) => {
                    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                    TimeChange::Set(dur.as_secs() as i64, dur.subsec_nanos())
                }
                Some(crate::fuser::TimeOrNow::Now) => TimeChange::Now,
                None => TimeChange::Omit,
            };
            let new_mtime = match mtime {
                Some(crate::fuser::TimeOrNow::SpecificTime(t)) => {
                    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                    TimeChange::Set(dur.as_secs() as i64, dur.subsec_nanos())
                }
                Some(crate::fuser::TimeOrNow::Now) => TimeChange::Now,
                None => TimeChange::Omit,
            };
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.utimens(ino as i64, new_atime, new_mtime).await });
            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
        }

        // Return updated attributes
        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await });

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

        let fs = self.fs.clone();
        let entries_result = self
            .runtime
            .block_on(async move { fs.readdir_plus(ino as i64).await });

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
        // In the inode-based API we don't track parent relationships directly.
        // The kernel tracks this information and will resolve ".." correctly.
        // We use 1 (root) as a fallback which is safe since the kernel
        // won't actually use this value for path resolution.
        let parent_ino = 1u64;

        let mut all_entries = vec![
            (ino, FileType::Directory, "."),
            (parent_ino, FileType::Directory, ".."),
        ];

        // Process entries with stats already available (no N+1 queries!)
        for entry in &entries {
            let kind = if entry.stats.is_directory() {
                FileType::Directory
            } else if entry.stats.is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };

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

        let fs = self.fs.clone();
        let entries_result = self
            .runtime
            .block_on(async move { fs.readdir_plus(ino as i64).await });

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
        let dir_stats = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await })
            .ok()
            .flatten();

        // Determine parent inode and stats for ".." entry
        // In the inode-based API we don't track parent relationships directly.
        // Use root's stats for ".." as a fallback - the kernel handles proper ".." resolution.
        let (parent_ino, parent_stats) = if ino == 1 {
            (1u64, dir_stats.clone()) // Root's parent is itself
        } else {
            // Use root inode as fallback for parent
            let fs = self.fs.clone();
            let parent_stats = self
                .runtime
                .block_on(async move { fs.getattr(1).await })
                .ok()
                .flatten();
            (1u64, parent_stats)
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
                let attr = fillattr(&entry.stats);

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

    /// Creates a special file node (FIFO, device, socket, or regular file).
    ///
    /// Creates a file node at `name` under `parent` with the specified mode
    /// and device number, then stats it to return proper attributes.
    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::mknod: parent={}, name={:?}, mode={:o}, rdev={}",
            parent,
            name,
            mode,
            rdev
        );

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.mknod(parent as i64, &name_owned, mode, rdev as u64, uid, gid)
                .await
        });

        match result {
            Ok(stats) => {
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
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
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::mkdir: parent={}, name={:?}, mode={:o}",
            parent,
            name,
            mode
        );

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.mkdir(parent as i64, &name_owned, mode, uid, gid).await });

        match result {
            Ok(stats) => {
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.rmdir(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
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

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        // Create file with mode, get stats and file handle in one operation
        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.create_file(parent as i64, &name_owned, mode, uid, gid)
                .await
        });

        match result {
            Ok((stats, file)) => {
                let attr = fillattr(&stats);

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
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::symlink: parent={}, link_name={:?}, target={:?}",
            parent,
            link_name,
            target
        );

        let Some(name_str) = link_name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(target_str) = target.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let target_owned = target_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.symlink(parent as i64, &name_owned, &target_owned, uid, gid)
                .await
        });

        match result {
            Ok(stats) => {
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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

        let Some(name_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.link(ino as i64, newparent as i64, &name_owned).await });

        match result {
            Ok(stats) => {
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.unlink(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Renames a file or directory.
    ///
    /// Moves `name` from `parent` to `newname` under `newparent`.
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

        let Some(old_name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(new_name_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let old_name_owned = old_name_str.to_string();
        let new_name_owned = new_name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.rename(
                parent as i64,
                &old_name_owned,
                newparent as i64,
                &new_name_owned,
            )
            .await
        });

        match result {
            Ok(()) => {
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

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.open(ino as i64).await });

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

    // ─────────────────────────────────────────────────────────────
    // Inode Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Forget about an inode.
    ///
    /// Called when the kernel removes an inode from its cache. For passthrough
    /// filesystems (like HostFS), this allows releasing O_PATH file descriptors
    /// that were cached for the inode, preventing file descriptor exhaustion.
    fn forget(&mut self, _req: &Request, ino: u64, nlookup: u64) {
        tracing::debug!("FUSE::forget: ino={}, nlookup={}", ino, nlookup);
        let fs = self.fs.clone();
        self.runtime.block_on(async move {
            fs.forget(ino as i64, nlookup).await;
        });
    }

    /// Batch forget multiple inodes at once.
    ///
    /// This is an optimization over calling forget() individually for each inode.
    fn batch_forget(&mut self, _req: &Request, nodes: &[fuse_forget_one]) {
        tracing::debug!("FUSE::batch_forget: {} nodes", nodes.len());
        let fs = self.fs.clone();
        let nodes_vec: Vec<(i64, u64)> =
            nodes.iter().map(|n| (n.nodeid as i64, n.nlookup)).collect();
        self.runtime.block_on(async move {
            for (ino, nlookup) in nodes_vec {
                fs.forget(ino, nlookup).await;
            }
        });
    }
}

impl AgentFSFuse {
    /// Create a new FUSE filesystem adapter wrapping a FileSystem instance.
    ///
    /// The provided Tokio runtime is used to execute async FileSystem operations
    /// from within synchronous FUSE callbacks via `block_on`.
    fn new(fs: Arc<dyn FileSystem>, runtime: Runtime) -> Self {
        Self {
            fs,
            runtime,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
        }
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
    let file_type = stats.mode & S_IFMT;
    let kind = match file_type {
        S_IFDIR => FileType::Directory,
        S_IFLNK => FileType::Symlink,
        S_IFIFO => FileType::NamedPipe,
        S_IFCHR => FileType::CharDevice,
        S_IFBLK => FileType::BlockDevice,
        S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    };

    let size = if file_type == S_IFDIR {
        4096_u64 // Standard directory size
    } else {
        stats.size as u64
    };

    FileAttr {
        ino: stats.ino as u64,
        size,
        blocks: size.div_ceil(512),
        atime: UNIX_EPOCH + Duration::new(stats.atime as u64, stats.atime_nsec),
        mtime: UNIX_EPOCH + Duration::new(stats.mtime as u64, stats.mtime_nsec),
        ctime: UNIX_EPOCH + Duration::new(stats.ctime as u64, stats.ctime_nsec),
        crtime: UNIX_EPOCH,
        kind,
        perm: (stats.mode & 0o7777) as u16,
        nlink: stats.nlink,
        uid: stats.uid,
        gid: stats.gid,
        rdev: stats.rdev as u32,
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
    // Raise fd limit to hard limit to prevent "too many open files" errors
    // when passthrough filesystems cache O_PATH file descriptors
    maximize_fd_limit();

    let fs = AgentFSFuse::new(fs, runtime);

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
