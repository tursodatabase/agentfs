pub mod agentfs;
#[cfg(unix)]
pub mod hostfs;

use anyhow::Result;
use async_trait::async_trait;
use thiserror::Error;

// Re-export implementations
pub use agentfs::AgentFS;
#[cfg(unix)]
pub use hostfs::HostFS;

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
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()>;

    /// Read from a file at a given offset (like POSIX pread)
    ///
    /// Returns `Ok(None)` if the file does not exist.
    async fn pread(&self, path: &str, offset: u64, size: u64) -> Result<Option<Vec<u8>>>;

    /// Write to a file at a given offset (like POSIX pwrite)
    ///
    /// If the file does not exist, it will be created.
    /// If the offset is beyond the current file size, the file is extended with zeros.
    async fn pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<()>;

    /// Truncate a file to a specific size
    async fn truncate(&self, path: &str, size: u64) -> Result<()>;

    /// List directory contents
    ///
    /// Returns `Ok(None)` if the directory does not exist.
    async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>>;

    /// Create a directory
    async fn mkdir(&self, path: &str) -> Result<()>;

    /// Remove a file or empty directory
    async fn remove(&self, path: &str) -> Result<()>;

    /// Rename/move a file or directory
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Create a symbolic link
    async fn symlink(&self, target: &str, linkpath: &str) -> Result<()>;

    /// Read the target of a symbolic link
    ///
    /// Returns `Ok(None)` if the path does not exist.
    async fn readlink(&self, path: &str) -> Result<Option<String>>;

    /// Get filesystem statistics
    async fn statfs(&self) -> Result<FilesystemStats>;
}
