pub mod agentfs;
#[cfg(unix)]
pub mod hostfs;
pub mod overlayfs;

use crate::error::Result;
use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

// Re-export implementations
pub use agentfs::AgentFS;
#[cfg(unix)]
pub use hostfs::HostFS;
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
        }
    }
}

// File types for mode field
pub const S_IFMT: u32 = 0o170000; // File type mask
pub const S_IFREG: u32 = 0o100000; // Regular file
pub const S_IFDIR: u32 = 0o040000; // Directory
pub const S_IFLNK: u32 = 0o120000; // Symbolic link

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

/// A trait defining filesystem operations.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Get file statistics, following symlinks
    async fn stat(&self, path: &str) -> Result<Option<Stats>>;

    /// Get file statistics without following symlinks
    async fn lstat(&self, path: &str) -> Result<Option<Stats>>;

    /// Read entire file contents
    ///
    /// Returns `Ok(None)` if the file does not exist.
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>>;

    /// Write data to a file (creates or overwrites)
    ///
    /// If the file doesn't exist, it will be created with the specified uid/gid.
    /// If the file exists, uid/gid are ignored (existing ownership preserved).
    async fn write_file(&self, path: &str, data: &[u8], uid: u32, gid: u32) -> Result<()>;

    /// List directory contents
    ///
    /// Returns `Ok(None)` if the directory does not exist.
    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>>;

    /// List directory contents with full statistics for each entry
    ///
    /// This is an optimized version of readdir that returns both entry names
    /// and their statistics in a single call, avoiding N+1 queries.
    ///
    /// Returns `Ok(None)` if the directory does not exist.
    async fn readdir_plus(&self, path: &str) -> Result<Option<Vec<DirEntry>>>;

    /// Create a directory with the specified ownership
    async fn mkdir(&self, path: &str, uid: u32, gid: u32) -> Result<()>;

    /// Remove a file or empty directory
    async fn remove(&self, path: &str) -> Result<()>;

    /// Change file mode/permissions
    ///
    /// The mode parameter contains the full mode including file type bits,
    /// but only the permission bits (lower 12 bits) will be modified.
    async fn chmod(&self, path: &str, mode: u32) -> Result<()>;

    /// Change file ownership
    ///
    /// Changes the user and/or group ownership of a file.
    /// Pass None for uid or gid to leave that value unchanged.
    async fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<()>;

    /// Rename/move a file or directory
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Create a symbolic link with the specified ownership
    async fn symlink(&self, target: &str, linkpath: &str, uid: u32, gid: u32) -> Result<()>;

    /// Create a hard link
    ///
    /// Creates a new directory entry `newpath` that refers to the same inode as `oldpath`.
    /// Both paths will share the same file data and metadata (except for the name).
    /// The link count (nlink) of the inode is incremented.
    async fn link(&self, oldpath: &str, newpath: &str) -> Result<()>;

    /// Read the target of a symbolic link
    ///
    /// Returns `Ok(None)` if the path does not exist.
    async fn readlink(&self, path: &str) -> Result<Option<String>>;

    /// Get filesystem statistics
    async fn statfs(&self) -> Result<FilesystemStats>;

    /// Open a file and return a file handle for I/O operations.
    ///
    /// The returned file handle can be used for efficient read/write/fsync
    /// operations without requiring path lookups on each operation.
    async fn open(&self, path: &str) -> Result<BoxedFile>;

    /// Create a new empty file with the specified mode and ownership.
    ///
    /// Returns both the file stats and an open file handle in a single operation.
    /// This is optimized for FUSE create() which needs both atomically.
    /// Fails with AlreadyExists if the file exists.
    async fn create_file(
        &self,
        path: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)>;
}
