pub mod agentfs;
#[cfg(target_os = "macos")]
pub mod hostfs_darwin;
#[cfg(target_os = "linux")]
pub mod hostfs_linux;
pub mod overlayfs;

use crate::error::Result;
use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

// Re-export implementations
pub use agentfs::AgentFS;
#[cfg(target_os = "macos")]
pub use hostfs_darwin::HostFS;
#[cfg(target_os = "linux")]
pub use hostfs_linux::HostFS;
pub use overlayfs::OverlayFS;

/// Filesystem-specific errors with errno semantics
#[derive(Debug, Error)]
pub enum FsError {
    #[error("Path does not exist")]
    NotFound,

    #[error("Path already exists")]
    AlreadyExists,

    #[error("Directory not empty")]
    NotEmpty,

    #[error("Not a directory")]
    NotADirectory,

    #[error("Is a directory")]
    IsADirectory,

    #[error("Not a symbolic link")]
    NotASymlink,

    #[error("Invalid path")]
    InvalidPath,

    #[error("Cannot modify root directory")]
    RootOperation,

    #[error("Too many levels of symbolic links")]
    SymlinkLoop,

    #[error("Cannot rename directory into its own subdirectory")]
    InvalidRename,

    #[error("Filename too long")]
    NameTooLong,
}

impl FsError {
    /// Convert to libc errno code
    pub fn to_errno(&self) -> i32 {
        match self {
            FsError::NotFound => libc::ENOENT,
            FsError::AlreadyExists => libc::EEXIST,
            FsError::NotEmpty => libc::ENOTEMPTY,
            FsError::NotADirectory => libc::ENOTDIR,
            FsError::IsADirectory => libc::EISDIR,
            FsError::NotASymlink => libc::EINVAL,
            FsError::InvalidPath => libc::EINVAL,
            FsError::RootOperation => libc::EPERM,
            FsError::SymlinkLoop => libc::ELOOP,
            FsError::InvalidRename => libc::EINVAL,
            FsError::NameTooLong => libc::ENAMETOOLONG,
        }
    }
}

/// Maximum filename length in bytes.
pub const MAX_NAME_LEN: usize = 255;

// File types for mode field
pub const S_IFMT: u32 = 0o170000; // File type mask
pub const S_IFREG: u32 = 0o100000; // Regular file
pub const S_IFDIR: u32 = 0o040000; // Directory
pub const S_IFLNK: u32 = 0o120000; // Symbolic link
pub const S_IFIFO: u32 = 0o010000; // FIFO (named pipe)
pub const S_IFCHR: u32 = 0o020000; // Character device
pub const S_IFBLK: u32 = 0o060000; // Block device
pub const S_IFSOCK: u32 = 0o140000; // Socket

// Default permissions
pub const DEFAULT_FILE_MODE: u32 = S_IFREG | 0o644; // Regular file, rw-r--r--
pub const DEFAULT_DIR_MODE: u32 = S_IFDIR | 0o755; // Directory, rwxr-xr-x

/// File statistics
#[derive(Debug, Clone)]
pub struct Stats {
    pub ino: i64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: i64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub rdev: u64, // Device ID for special files (char/block devices)
}

/// Filesystem statistics for statfs
#[derive(Debug, Clone)]
pub struct FilesystemStats {
    /// Total number of inodes (files, directories, symlinks)
    pub inodes: u64,
    /// Total bytes used by file contents
    pub bytes_used: u64,
}

/// Directory entry with full statistics
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Entry name (without path)
    pub name: String,
    /// Full statistics for this entry
    pub stats: Stats,
}

impl Stats {
    pub fn is_file(&self) -> bool {
        (self.mode & S_IFMT) == S_IFREG
    }

    pub fn is_directory(&self) -> bool {
        (self.mode & S_IFMT) == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode & S_IFMT) == S_IFLNK
    }
}

/// An open file handle for performing I/O operations.
///
/// This trait represents an open file, similar to a file descriptor in POSIX.
/// Operations on this handle don't require path lookups since the file was
/// already resolved at open time.
#[async_trait]
pub trait File: Send + Sync {
    /// Read from the file at the given offset (like POSIX pread).
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>>;

    /// Write to the file at the given offset (like POSIX pwrite).
    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Truncate the file to the specified size.
    async fn truncate(&self, size: u64) -> Result<()>;

    /// Synchronize file data to persistent storage.
    async fn fsync(&self) -> Result<()>;

    /// Get file statistics.
    async fn fstat(&self) -> Result<Stats>;
}

/// A boxed File trait object for dynamic dispatch.
pub type BoxedFile = Arc<dyn File>;

/// A trait defining filesystem operations using inode semantics.
///
/// This trait uses inode-based operations rather than path-based operations,
/// matching POSIX and FUSE semantics more closely.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Look up a directory entry by name within a parent directory.
    ///
    /// This is the primary method for resolving names to inodes. Given a parent
    /// directory inode and a child name, returns the stats for the child entry
    /// (without following symlinks, like lstat).
    ///
    /// Returns `Ok(None)` if the entry does not exist.
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>>;

    /// Get file attributes for an inode.
    ///
    /// Returns stats for the inode itself (does not follow symlinks).
    /// Returns `Ok(None)` if the inode does not exist.
    async fn getattr(&self, ino: i64) -> Result<Option<Stats>>;

    /// Read the target of a symbolic link inode.
    ///
    /// Returns `Ok(None)` if the inode does not exist or is not a symlink.
    async fn readlink(&self, ino: i64) -> Result<Option<String>>;

    /// List directory contents by inode.
    ///
    /// Returns entry names (not full paths) for the directory.
    /// Returns `Ok(None)` if the directory does not exist.
    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>>;

    /// List directory contents with full statistics for each entry.
    ///
    /// This is an optimized version of readdir that returns both entry names
    /// and their statistics in a single call, avoiding N+1 queries.
    ///
    /// Returns `Ok(None)` if the directory does not exist.
    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>>;

    /// Change file mode/permissions by inode.
    async fn chmod(&self, ino: i64, mode: u32) -> Result<()>;

    /// Change file ownership by inode.
    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()>;

    /// Set file timestamps by inode.
    ///
    /// Updates atime and/or mtime. `None` means "don't change".
    /// ctime is always updated to the current time when any timestamp changes (POSIX requirement).
    async fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) -> Result<()>;

    /// Open a file by inode and return a file handle for I/O operations.
    async fn open(&self, ino: i64) -> Result<BoxedFile>;

    /// Create a directory with the specified ownership.
    ///
    /// Returns the stats of the newly created directory.
    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Stats>;

    /// Create a new empty file with the specified mode and ownership.
    ///
    /// Returns both the file stats and an open file handle in a single operation.
    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)>;

    /// Create a special file node (FIFO, device, socket, or regular file).
    ///
    /// Returns the stats of the newly created node.
    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Stats>;

    /// Create a symbolic link with the specified ownership.
    ///
    /// Returns the stats of the newly created symlink.
    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Stats>;

    /// Remove a file (non-directory) from a directory.
    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()>;

    /// Remove an empty directory.
    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()>;

    /// Create a hard link.
    ///
    /// Creates a new directory entry `newname` under `newparent_ino` that refers
    /// to the same inode as `ino`. Returns the stats of the linked inode.
    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats>;

    /// Rename/move a file or directory.
    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<()>;

    /// Get filesystem statistics.
    async fn statfs(&self) -> Result<FilesystemStats>;

    /// Forget about an inode (called when kernel drops inode from cache).
    ///
    /// The `nlookup` parameter indicates how many lookups the kernel is forgetting.
    /// For passthrough filesystems that cache file descriptors per inode, this
    /// should decrement a reference count and close the fd when it reaches zero.
    ///
    /// The default implementation is a no-op, suitable for filesystems that don't
    /// cache any resources per inode (like database-backed filesystems).
    async fn forget(&self, _ino: i64, _nlookup: u64) {
        // Default: no-op
    }
}
