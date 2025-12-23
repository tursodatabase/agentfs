use agentfs_sdk::{BoxedFile, FileSystem, FsError, Stats};
use fuser::{
    consts::FUSE_WRITEBACK_CACHE, FileAttr, FileType, Filesystem, KernelConfig, MountOption,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request,
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
use tokio::runtime::Runtime;

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
    /// Inode number for this file (needed for buffer key).
    ino: u64,
}

/// Default chunk size (matches AgentFS default).
const CHUNK_SIZE: usize = 4096;

/// Maximum dirty bytes before triggering a flush (64 MB).
const WRITE_BUFFER_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Key for identifying a dirty chunk: (inode, chunk index).
type ChunkKey = (u64, i64);

/// Write buffer for batching writes before committing to SQLite.
///
/// This buffer implements write-back caching at the FUSE layer:
/// - Writes are buffered in memory instead of going directly to SQLite
/// - Reads check the buffer first (read-through)
/// - Data is flushed to SQLite on fsync, flush, release, or when buffer is full
///
/// This dramatically reduces write amplification by:
/// 1. Batching multiple writes into single transactions
/// 2. Allowing checkpoint to run less frequently
/// 3. Coalescing overwrites to the same chunks
struct WriteBuffer {
    /// Dirty chunks keyed by (inode, chunk index).
    dirty_chunks: HashMap<ChunkKey, Vec<u8>>,
    /// Total bytes currently buffered.
    dirty_bytes: usize,
    /// Pending file sizes (tracks size changes from writes past EOF).
    /// Maps inode -> new size.
    pending_sizes: HashMap<u64, u64>,
    /// Original file sizes when buffering started (for correct truncation).
    /// Maps inode -> original size at first write.
    original_sizes: HashMap<u64, u64>,
}

impl WriteBuffer {
    fn new() -> Self {
        Self {
            dirty_chunks: HashMap::new(),
            dirty_bytes: 0,
            pending_sizes: HashMap::new(),
            original_sizes: HashMap::new(),
        }
    }

    /// Check if the buffer needs flushing due to size.
    fn needs_flush(&self) -> bool {
        self.dirty_bytes >= WRITE_BUFFER_MAX_BYTES
    }

    /// Get a dirty chunk if it exists.
    fn get_chunk(&self, ino: u64, chunk_idx: i64) -> Option<&Vec<u8>> {
        self.dirty_chunks.get(&(ino, chunk_idx))
    }

    /// Insert or update a dirty chunk.
    fn put_chunk(&mut self, ino: u64, chunk_idx: i64, data: Vec<u8>) {
        let key = (ino, chunk_idx);
        // Subtract old size if replacing
        if let Some(old) = self.dirty_chunks.get(&key) {
            self.dirty_bytes -= old.len();
        }
        self.dirty_bytes += data.len();
        self.dirty_chunks.insert(key, data);
    }

    /// Track the original file size before buffering starts (for correct truncation).
    fn track_original_size(&mut self, ino: u64, current_size: u64) {
        // Only record the first time we see this inode
        self.original_sizes.entry(ino).or_insert(current_size);
    }

    /// Update pending file size if write extends past original size.
    fn update_pending_size(&mut self, ino: u64, write_end: u64) {
        // Only track pending size if it extends past the original file size
        if let Some(&original) = self.original_sizes.get(&ino) {
            if write_end > original {
                self.pending_sizes
                    .entry(ino)
                    .and_modify(|s| *s = (*s).max(write_end))
                    .or_insert(write_end);
            }
        }
    }

    /// Get pending size for an inode (if any writes extended the file).
    fn get_pending_size(&self, ino: u64) -> Option<u64> {
        self.pending_sizes.get(&ino).copied()
    }

    /// Remove all dirty chunks for an inode, returning them.
    fn take_chunks_for_ino(&mut self, ino: u64) -> Vec<(i64, Vec<u8>)> {
        let keys: Vec<_> = self
            .dirty_chunks
            .keys()
            .filter(|(i, _)| *i == ino)
            .copied()
            .collect();

        let mut chunks = Vec::new();
        for key in keys {
            if let Some(data) = self.dirty_chunks.remove(&key) {
                self.dirty_bytes -= data.len();
                chunks.push((key.1, data));
            }
        }
        chunks
    }

    /// Clear pending size and original size for an inode (after flush).
    fn clear_inode_state(&mut self, ino: u64) {
        self.pending_sizes.remove(&ino);
        self.original_sizes.remove(&ino);
    }

    /// Take all dirty data, clearing the buffer.
    fn take_all(&mut self) -> (HashMap<ChunkKey, Vec<u8>>, HashMap<u64, u64>) {
        self.dirty_bytes = 0;
        self.original_sizes.clear();
        (
            std::mem::take(&mut self.dirty_chunks),
            std::mem::take(&mut self.pending_sizes),
        )
    }
}

struct AgentFSFuse {
    fs: Arc<dyn FileSystem>,
    runtime: Runtime,
    path_cache: Arc<Mutex<HashMap<u64, String>>>,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
    /// User ID to report for all files (set at mount time)
    uid: u32,
    /// Group ID to report for all files (set at mount time)
    gid: u32,
    /// Write buffer for batching writes before SQLite
    write_buffer: Arc<Mutex<WriteBuffer>>,
}

impl Filesystem for AgentFSFuse {
    /// Initialize the filesystem and enable writeback caching.
    ///
    /// Writeback caching allows the kernel to buffer writes and flush them
    /// later, significantly improving write performance for small writes.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.add_capabilities(FUSE_WRITEBACK_CACHE);
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
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.lstat(&path).await;
            (result, path)
        });
        match result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats, self.uid, self.gid);
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

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.lstat(&path).await });

        match result {
            Ok(Some(stats)) => {
                let mut attr = fillattr(&stats, self.uid, self.gid);

                // Check for pending size from buffered writes
                let write_buffer = self.write_buffer.lock();
                if let Some(pending_size) = write_buffer.get_pending_size(ino) {
                    // Use the larger of db size or pending size
                    attr.size = attr.size.max(pending_size);
                }

                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
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
        let result = self
            .runtime
            .block_on(async move { fs.readlink(&path).await });

        match result {
            Ok(Some(target)) => reply.data(target.as_bytes()),
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

            if result.is_err() {
                reply.error(libc::EIO);
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
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats, self.uid, self.gid)),
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
            Err(_) => {
                reply.error(libc::EIO);
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
        let uid = self.uid;
        let gid = self.gid;

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
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.mkdir(&path).await;
            (result, path)
        });

        if result.is_err() {
            reply.error(libc::EIO);
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
                let attr = fillattr(&stats, self.uid, self.gid);
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
            Err(_) => {
                reply.error(libc::EIO);
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
        let fs = self.fs.clone();
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.write_file(&path, &[]).await;
            (result, path)
        });

        if result.is_err() {
            reply.error(libc::EIO);
            return;
        }

        // Get the new file's stats
        let fs = self.fs.clone();
        let (stat_result, path) = self.runtime.block_on(async move {
            let result = fs.stat(&path).await;
            (result, path)
        });

        let attr = match stat_result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats, self.uid, self.gid);
                self.add_path(attr.ino, path.clone());
                attr
            }
            _ => {
                // Fail the operation if we cannot stat the new file
                reply.error(libc::EIO);
                return;
            }
        };

        // Open the file to get a file handle
        let fs = self.fs.clone();
        let path_clone = path.clone();
        let open_result = self
            .runtime
            .block_on(async move { fs.open(&path_clone).await });

        let file = match open_result {
            Ok(file) => file,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        self.open_files.lock().insert(
            fh,
            OpenFile {
                file,
                ino: attr.ino,
            },
        );

        reply.created(&TTL, &attr, 0, fh, 0);
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
        let target_owned = target_str.to_string();
        let (result, path) = self.runtime.block_on(async move {
            let result = fs.symlink(&target_owned, &path).await;
            (result, path)
        });

        if result.is_err() {
            reply.error(libc::EIO);
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
                let attr = fillattr(&stats, self.uid, self.gid);
                self.add_path(attr.ino, path);
                reply.entry(&TTL, &attr, 0);
            }
            _ => {
                reply.error(libc::EIO);
            }
        }
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

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.remove(&path).await });

        match result {
            Ok(()) => {
                self.drop_path(ino);
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

    /// Opens a file for reading or writing.
    ///
    /// Allocates a file handle and opens the file in the filesystem layer.
    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
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
                self.open_files.lock().insert(fh, OpenFile { file, ino });
                reply.opened(fh, 0);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    /// Reads data using the file handle.
    ///
    /// Checks the write buffer first for dirty chunks (read-through),
    /// then falls back to reading from SQLite for non-buffered data.
    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
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

        let offset = offset as u64;
        let size = size as u64;
        if size == 0 {
            reply.data(&[]);
            return;
        }

        let read_end = offset + size;
        let start_chunk = (offset / CHUNK_SIZE as u64) as i64;
        let end_chunk = ((read_end - 1) / CHUNK_SIZE as u64) as i64;

        // First pass: check which chunks are in buffer (keyed by inode, not fh)
        let mut buffered_chunks: HashMap<i64, Vec<u8>> = HashMap::new();
        let mut missing_chunks: Vec<i64> = Vec::new();
        {
            let write_buffer = self.write_buffer.lock();
            for chunk_idx in start_chunk..=end_chunk {
                if let Some(buffered) = write_buffer.get_chunk(ino, chunk_idx) {
                    buffered_chunks.insert(chunk_idx, buffered.clone());
                } else {
                    missing_chunks.push(chunk_idx);
                }
            }
        }

        // Second pass: read missing chunks from file (without holding lock)
        let mut file_chunks: HashMap<i64, Vec<u8>> = HashMap::new();
        for &chunk_idx in &missing_chunks {
            let chunk_start = chunk_idx as u64 * CHUNK_SIZE as u64;
            let file = file.clone();
            let read_result = self
                .runtime
                .block_on(async move { file.pread(chunk_start, CHUNK_SIZE as u64).await });

            match read_result {
                Ok(data) => {
                    file_chunks.insert(chunk_idx, data);
                }
                Err(_) => {
                    // File might be shorter than requested - use empty for missing
                    file_chunks.insert(chunk_idx, Vec::new());
                }
            }
        }

        // Assemble result from buffered and file chunks
        let mut result = Vec::with_capacity(size as usize);
        for chunk_idx in start_chunk..=end_chunk {
            let chunk_start = chunk_idx as u64 * CHUNK_SIZE as u64;

            // Calculate what part of this chunk we need
            let read_start_in_chunk = if offset > chunk_start {
                (offset - chunk_start) as usize
            } else {
                0
            };
            let read_end_in_chunk = std::cmp::min(CHUNK_SIZE, (read_end - chunk_start) as usize);

            let chunk_data = buffered_chunks
                .get(&chunk_idx)
                .or_else(|| file_chunks.get(&chunk_idx));

            if let Some(data) = chunk_data {
                let end = std::cmp::min(read_end_in_chunk, data.len());
                if read_start_in_chunk < end {
                    result.extend_from_slice(&data[read_start_in_chunk..end]);
                }
            }
        }

        reply.data(&result);
    }

    /// Writes data using the file handle.
    ///
    /// Writes are buffered in memory and batched to SQLite on flush/fsync.
    /// This dramatically reduces write amplification by avoiding per-write
    /// SQLite transactions and WAL checkpoints.
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
        let (file, ino) = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            (open_file.file.clone(), open_file.ino)
        };

        let data_len = data.len();
        if data_len == 0 {
            reply.written(0);
            return;
        }

        let offset = offset as u64;
        let write_end = offset + data_len as u64;

        // Calculate affected chunks
        let start_chunk = (offset / CHUNK_SIZE as u64) as i64;
        let end_chunk = ((write_end - 1) / CHUNK_SIZE as u64) as i64;

        // Get current file size for tracking original size (needed for correct truncation)
        let current_size = {
            let file = file.clone();
            self.runtime
                .block_on(async move { file.fstat().await })
                .map(|s| s.size as u64)
                .unwrap_or(0)
        };

        let mut write_buffer = self.write_buffer.lock();

        // Track original file size before any buffered writes
        write_buffer.track_original_size(ino, current_size);

        // Process each affected chunk
        let mut written = 0usize;
        for chunk_idx in start_chunk..=end_chunk {
            let chunk_start = chunk_idx as u64 * CHUNK_SIZE as u64;

            // Calculate what part of this chunk we're writing
            let write_start_in_chunk = if offset > chunk_start {
                (offset - chunk_start) as usize
            } else {
                0
            };
            let write_end_in_chunk = std::cmp::min(CHUNK_SIZE, (write_end - chunk_start) as usize);

            // Get existing chunk data (from buffer or file)
            let mut chunk_data = if let Some(buffered) = write_buffer.get_chunk(ino, chunk_idx) {
                buffered.clone()
            } else {
                // Load from file if partial write needs existing data
                let needs_existing = write_start_in_chunk > 0 || write_end_in_chunk < CHUNK_SIZE;
                if needs_existing {
                    let file = file.clone();
                    let chunk_start = chunk_start;
                    match self
                        .runtime
                        .block_on(async move { file.pread(chunk_start, CHUNK_SIZE as u64).await })
                    {
                        Ok(existing) => {
                            let mut v = existing;
                            v.resize(CHUNK_SIZE, 0);
                            v
                        }
                        Err(_) => vec![0u8; CHUNK_SIZE],
                    }
                } else {
                    vec![0u8; CHUNK_SIZE]
                }
            };

            // Calculate what part of input data goes into this chunk
            let data_start = if chunk_start > offset {
                (chunk_start - offset) as usize
            } else {
                0
            };
            let data_end = std::cmp::min(
                data_len,
                data_start + (write_end_in_chunk - write_start_in_chunk),
            );

            // Copy new data into chunk
            let src = &data[data_start..data_end];
            chunk_data[write_start_in_chunk..write_start_in_chunk + src.len()].copy_from_slice(src);

            written += src.len();

            // Store in buffer (keyed by inode, not fh)
            write_buffer.put_chunk(ino, chunk_idx, chunk_data);
        }

        // Track pending file size (only if extending past original)
        write_buffer.update_pending_size(ino, write_end);

        // Check if we need to flush due to buffer size
        let needs_flush = write_buffer.needs_flush();
        drop(write_buffer);

        if needs_flush {
            // Flush all dirty data to SQLite
            self.flush_write_buffer();
        }

        reply.written(written as u32);
    }

    /// Flushes data to the backend storage.
    ///
    /// Flushes buffered writes for this inode to SQLite.
    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
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

        // Flush buffered writes for this inode
        self.flush_inode(ino, &file);
        reply.ok();
    }

    /// Synchronizes file data to persistent storage using the file handle.
    ///
    /// Flushes buffered writes to SQLite, then calls fsync on the underlying
    /// file handle to ensure data is persisted to disk.
    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
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

        // First flush buffered writes
        self.flush_inode(ino, &file);

        // Then fsync the file
        let result = self.runtime.block_on(async move { file.fsync().await });

        match result {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(libc::EIO),
        }
    }

    /// Releases (closes) an open file handle.
    ///
    /// Flushes any buffered writes for this inode before removing it.
    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Get file before removing from open_files
        let file = {
            let open_files = self.open_files.lock();
            open_files.get(&fh).map(|f| f.file.clone())
        };

        // Flush buffered writes before closing
        if let Some(file) = file {
            self.flush_inode(ino, &file);
        }
        self.open_files.lock().remove(&fh);
        reply.ok();
    }

    /// Returns filesystem statistics.
    ///
    /// Queries actual usage from the SDK and reports it to tools like `df`.
    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
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
    ///
    /// The uid and gid are used for all file ownership to avoid "dubious ownership"
    /// errors from tools like git that check file ownership.
    fn new(fs: Arc<dyn FileSystem>, runtime: Runtime, uid: u32, gid: u32) -> Self {
        Self {
            fs,
            runtime,
            path_cache: Arc::new(Mutex::new(HashMap::new())),
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
            write_buffer: Arc::new(Mutex::new(WriteBuffer::new())),
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

    /// Flush all dirty data from the write buffer to SQLite.
    ///
    /// This writes all buffered chunks to the underlying filesystem in a
    /// single batch, dramatically reducing write amplification compared to
    /// per-write SQLite transactions.
    fn flush_write_buffer(&self) {
        // Take all dirty data from buffer (releases lock immediately)
        let (dirty_chunks, pending_sizes) = {
            let mut buffer = self.write_buffer.lock();
            buffer.take_all()
        };

        if dirty_chunks.is_empty() {
            return;
        }

        // Group chunks by inode
        let mut chunks_by_ino: HashMap<u64, Vec<(i64, Vec<u8>)>> = HashMap::new();
        for ((ino, chunk_idx), data) in dirty_chunks {
            chunks_by_ino.entry(ino).or_default().push((chunk_idx, data));
        }

        // Collect file handles for each inode (brief lock, then release)
        let files_by_ino: HashMap<u64, BoxedFile> = {
            let open_files = self.open_files.lock();
            chunks_by_ino
                .keys()
                .filter_map(|&ino| {
                    // Find any open file handle for this inode
                    open_files
                        .values()
                        .find(|f| f.ino == ino)
                        .map(|f| (ino, f.file.clone()))
                })
                .collect()
        };

        // Flush each inode's chunks (no locks held during I/O)
        for (ino, chunks) in chunks_by_ino {
            if let Some(file) = files_by_ino.get(&ino) {
                // Write all chunks for this inode
                for (chunk_idx, data) in chunks {
                    let offset = chunk_idx as u64 * CHUNK_SIZE as u64;
                    let file = file.clone();
                    let _ = self
                        .runtime
                        .block_on(async move { file.pwrite(offset, &data).await });
                }

                // Update file size if we have a pending size for this inode
                if let Some(&new_size) = pending_sizes.get(&ino) {
                    let file = file.clone();
                    let _ = self
                        .runtime
                        .block_on(async move { file.truncate(new_size).await });
                }
            }
        }
    }

    /// Flush dirty data for a specific inode to SQLite.
    fn flush_inode(&self, ino: u64, file: &BoxedFile) {
        // Take chunks for this inode and clear its state
        let (chunks, pending_size) = {
            let mut buffer = self.write_buffer.lock();
            let chunks = buffer.take_chunks_for_ino(ino);
            let pending_size = buffer.get_pending_size(ino);
            buffer.clear_inode_state(ino);
            (chunks, pending_size)
        };

        if chunks.is_empty() {
            return;
        }

        // Write all chunks (no locks held during I/O)
        for (chunk_idx, data) in chunks {
            let offset = chunk_idx as u64 * CHUNK_SIZE as u64;
            let file = file.clone();
            let _ = self
                .runtime
                .block_on(async move { file.pwrite(offset, &data).await });
        }

        // Update file size if writes extended past original
        if let Some(new_size) = pending_size {
            let file = file.clone();
            let _ = self
                .runtime
                .block_on(async move { file.truncate(new_size).await });
        }
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

    let fs = AgentFSFuse::new(fs, runtime, uid, gid);

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
