use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use turso::{Builder, Connection, Value};

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
const S_IFMT: u32 = 0o170000; // File type mask
const S_IFREG: u32 = 0o100000; // Regular file
const S_IFDIR: u32 = 0o040000; // Directory
const S_IFLNK: u32 = 0o120000; // Symbolic link

// Default permissions
const DEFAULT_FILE_MODE: u32 = S_IFREG | 0o644; // Regular file, rw-r--r--
const DEFAULT_DIR_MODE: u32 = S_IFDIR | 0o755; // Directory, rwxr-xr-x

const ROOT_INO: i64 = 1;

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

/// A filesystem backed by SQLite
#[derive(Clone)]
pub struct Filesystem {
    conn: Arc<Connection>,
}

impl Filesystem {
    /// Create a new filesystem
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;
        let fs = Self {
            conn: Arc::new(conn),
        };
        fs.initialize().await?;
        Ok(fs)
    }

    /// Create a filesystem from an existing connection
    pub async fn from_connection(conn: Arc<Connection>) -> Result<Self> {
        let fs = Self { conn };
        fs.initialize().await?;
        Ok(fs)
    }

    /// Initialize the database schema
    async fn initialize(&self) -> Result<()> {
        // Create inode table
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS fs_inode (
                    ino INTEGER PRIMARY KEY AUTOINCREMENT,
                    mode INTEGER NOT NULL,
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL
                )",
                (),
            )
            .await?;

        // Create directory entry table
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS fs_dentry (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    parent_ino INTEGER NOT NULL,
                    ino INTEGER NOT NULL,
                    UNIQUE(parent_ino, name)
                )",
                (),
            )
            .await?;

        // Create index for efficient path lookups
        self.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
                ON fs_dentry(parent_ino, name)",
                (),
            )
            .await?;

        // Create data blocks table
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS fs_data (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ino INTEGER NOT NULL,
                    offset INTEGER NOT NULL,
                    size INTEGER NOT NULL,
                    data BLOB NOT NULL
                )",
                (),
            )
            .await?;

        // Create index for efficient data block lookups
        self.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_fs_data_ino_offset
                ON fs_data(ino, offset)",
                (),
            )
            .await?;

        // Create symlink table
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS fs_symlink (
                    ino INTEGER PRIMARY KEY,
                    target TEXT NOT NULL
                )",
                (),
            )
            .await?;

        // Ensure root directory exists
        self.ensure_root().await?;

        Ok(())
    }

    /// Ensure root directory exists
    async fn ensure_root(&self) -> Result<()> {
        let mut rows = self
            .conn
            .query("SELECT ino FROM fs_inode WHERE ino = ?", (ROOT_INO,))
            .await?;

        if rows.next().await?.is_none() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            self.conn
                .execute(
                    "INSERT INTO fs_inode (ino, mode, uid, gid, size, atime, mtime, ctime)
                    VALUES (?, ?, 0, 0, 0, ?, ?, ?)",
                    (ROOT_INO, DEFAULT_DIR_MODE as i64, now, now, now),
                )
                .await?;
        }

        Ok(())
    }

    /// Normalize a path
    fn normalize_path(&self, path: &str) -> String {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() {
            "/"
        } else if normalized.starts_with('/') {
            normalized
        } else {
            return format!("/{}", normalized);
        };

        // Handle . and .. components
        let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let mut result = Vec::new();

        for component in components {
            match component {
                "." => {
                    // Current directory - skip it
                    continue;
                }
                ".." => {
                    // Parent directory - only pop if there is a component to pop (don't traverse above root)
                    if !result.is_empty() {
                        result.pop();
                    }
                }
                _ => {
                    result.push(component);
                }
            }
        }

        if result.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", result.join("/"))
        }
    }

    /// Split path into components
    fn split_path(&self, path: &str) -> Vec<String> {
        let normalized = self.normalize_path(path);
        if normalized == "/" {
            return vec![];
        }
        normalized
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// Get link count for an inode
    async fn get_link_count(&self, ino: i64) -> Result<u32> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) as count FROM fs_dentry WHERE ino = ?",
                (ino,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let count = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            Ok(count as u32)
        } else {
            Ok(0)
        }
    }

    /// Build a Stats object from a database row
    ///
    /// The row should contain columns in this order:
    /// ino, mode, uid, gid, size, atime, mtime, ctime
    async fn build_stats_from_row(&self, row: &turso::Row, ino: i64) -> Result<Stats> {
        let nlink = self.get_link_count(ino).await?;
        Ok(Stats {
            ino,
            mode: row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            nlink,
            uid: row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            gid: row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            size: row
                .get_value(4)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            atime: row
                .get_value(5)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            mtime: row
                .get_value(6)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            ctime: row
                .get_value(7)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
        })
    }

    /// Resolve a path to an inode number
    async fn resolve_path(&self, path: &str) -> Result<Option<i64>> {
        let components = self.split_path(path);
        if components.is_empty() {
            return Ok(Some(ROOT_INO));
        }

        let mut current_ino = ROOT_INO;
        for component in components {
            let mut rows = self
                .conn
                .query(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?",
                    (current_ino, component.as_str()),
                )
                .await?;

            if let Some(row) = rows.next().await? {
                current_ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);
            } else {
                return Ok(None);
            }
        }

        Ok(Some(current_ino))
    }

    /// Get file statistics without following symlinks
    pub async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let path = self.normalize_path(path);
        let ino = match self.resolve_path(&path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = self
            .conn
            .query(
                "SELECT ino, mode, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?",
                (ino,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let ino_val = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let stats = self.build_stats_from_row(&row, ino_val).await?;
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    /// Get file statistics, following symlinks
    pub async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let path = self.normalize_path(path);

        // Follow symlinks with a maximum depth to prevent infinite loops
        let mut current_path = path;
        let max_symlink_depth = 40; // Standard limit for symlink following

        for _ in 0..max_symlink_depth {
            let ino = match self.resolve_path(&current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };

            let mut rows = self
                .conn
                .query(
                    "SELECT ino, mode, uid, gid, size, atime, mtime, ctime FROM fs_inode WHERE ino = ?",
                    (ino,),
                )
                .await?;

            if let Some(row) = rows.next().await? {
                let ino_val = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);

                let mode = row
                    .get_value(1)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                // Check if this is a symlink
                if (mode & S_IFMT) == S_IFLNK {
                    // Read the symlink target
                    let target = self
                        .readlink(&current_path)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Symlink has no target"))?;

                    // Resolve target path (handle both absolute and relative paths)
                    current_path = if target.starts_with('/') {
                        target
                    } else {
                        // Relative path - resolve relative to the symlink's directory
                        let base_path = Path::new(&current_path);
                        let parent = base_path.parent().unwrap_or(Path::new("/"));
                        let joined = parent.join(&target);
                        joined.to_string_lossy().into_owned()
                    };
                    current_path = self.normalize_path(&current_path);
                    continue; // Follow the symlink
                }

                // Not a symlink, return the stats
                let stats = self.build_stats_from_row(&row, ino_val).await?;
                return Ok(Some(stats));
            } else {
                return Ok(None);
            }
        }

        // Too many symlinks
        anyhow::bail!("Too many levels of symbolic links")
    }

    /// Create a directory
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            anyhow::bail!("Cannot create root directory");
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path(&parent_path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Parent directory does not exist"))?;

        let name = components.last().unwrap();

        // Check if already exists
        if (self.resolve_path(&path).await?).is_some() {
            anyhow::bail!("Directory already exists");
        }

        // Create inode
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.conn
            .execute(
                "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                VALUES (?, 0, 0, 0, ?, ?, ?)",
                (DEFAULT_DIR_MODE as i64, now, now, now),
            )
            .await?;

        let mut rows = self.conn.query("SELECT last_insert_rowid()", ()).await?;
        let ino = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?
        } else {
            anyhow::bail!("Failed to get inode");
        };

        // Create directory entry
        self.conn
            .execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                (name.as_str(), parent_ino, ino),
            )
            .await?;

        Ok(())
    }

    /// Write data to a file
    pub async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            anyhow::bail!("Cannot write to root directory");
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path(&parent_path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Parent directory does not exist"))?;

        let name = components.last().unwrap();

        // Check if file exists
        let ino = if let Some(ino) = self.resolve_path(&path).await? {
            // Delete existing data
            self.conn
                .execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
                .await?;
            ino
        } else {
            // Create new inode
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            self.conn
                .execute(
                    "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                    VALUES (?, 0, 0, ?, ?, ?, ?)",
                    (DEFAULT_FILE_MODE as i64, data.len() as i64, now, now, now),
                )
                .await?;

            let mut rows = self.conn.query("SELECT last_insert_rowid()", ()).await?;
            let ino = if let Some(row) = rows.next().await? {
                row.get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .ok_or_else(|| anyhow::anyhow!("Failed to get inode"))?
            } else {
                anyhow::bail!("Failed to get inode");
            };

            // Create directory entry
            self.conn
                .execute(
                    "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                    (name.as_str(), parent_ino, ino),
                )
                .await?;

            ino
        };

        // Write data
        if !data.is_empty() {
            self.conn
                .execute(
                    "INSERT INTO fs_data (ino, offset, size, data) VALUES (?, 0, ?, ?)",
                    (ino, data.len() as i64, data),
                )
                .await?;
        }

        // Update size and mtime
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.conn
            .execute(
                "UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?",
                (data.len() as i64, now, ino),
            )
            .await?;

        Ok(())
    }

    /// Read data from a file
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let ino = match self.resolve_path(path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = self
            .conn
            .query(
                "SELECT data FROM fs_data WHERE ino = ? ORDER BY offset",
                (ino,),
            )
            .await?;

        let mut data = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Ok(Value::Blob(chunk)) = row.get_value(0) {
                data.extend_from_slice(&chunk);
            }
        }

        Ok(Some(data))
    }

    /// List directory contents
    pub async fn readdir(&self, path: &str) -> Result<Option<Vec<String>>> {
        let ino = match self.resolve_path(path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        let mut rows = self
            .conn
            .query(
                "SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name",
                (ino,),
            )
            .await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
                .get_value(0)
                .ok()
                .and_then(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            if !name.is_empty() {
                entries.push(name);
            }
        }

        Ok(Some(entries))
    }

    /// Create a symbolic link
    pub async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        let linkpath = self.normalize_path(linkpath);
        let components = self.split_path(&linkpath);

        if components.is_empty() {
            anyhow::bail!("Cannot create symlink at root");
        }

        // Get parent directory
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path(&parent_path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Parent directory does not exist"))?;

        let name = components.last().unwrap();

        // Check if entry already exists
        if (self.resolve_path(&linkpath).await?).is_some() {
            anyhow::bail!("Path already exists");
        }

        // Create inode for symlink
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mode = S_IFLNK | 0o777; // Symlinks typically have 777 permissions
        let size = target.len() as i64;

        self.conn
            .execute(
                "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                 VALUES (?, 0, 0, ?, ?, ?, ?)",
                (mode, size, now, now, now),
            )
            .await?;

        // Get the newly created inode
        let mut rows = self.conn.query("SELECT last_insert_rowid()", ()).await?;

        let ino = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0)
        } else {
            anyhow::bail!("Failed to get new inode");
        };

        // Store symlink target
        self.conn
            .execute(
                "INSERT INTO fs_symlink (ino, target) VALUES (?, ?)",
                (ino, target),
            )
            .await?;

        // Create directory entry
        self.conn
            .execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                (name.as_str(), parent_ino, ino),
            )
            .await?;

        Ok(())
    }

    /// Read the target of a symbolic link
    pub async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let path = self.normalize_path(path);

        let ino = match self.resolve_path(&path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        // Check if it's a symlink by querying the inode
        let mut rows = self
            .conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            // Check if it's a symlink
            if (mode & S_IFMT) != S_IFLNK {
                anyhow::bail!("Not a symbolic link");
            }
        } else {
            return Ok(None);
        }

        // Read target from fs_symlink table
        let mut rows = self
            .conn
            .query("SELECT target FROM fs_symlink WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let target = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => Some(s.to_string()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("Invalid symlink target"))?;
            Ok(Some(target))
        } else {
            Ok(None)
        }
    }

    /// Remove a file or empty directory
    pub async fn remove(&self, path: &str) -> Result<()> {
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            anyhow::bail!("Cannot remove root directory");
        }

        let ino = self
            .resolve_path(&path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Path does not exist"))?;

        if ino == ROOT_INO {
            anyhow::bail!("Cannot remove root directory");
        }

        // Check if directory is empty
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?",
                (ino,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let count = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            if count > 0 {
                anyhow::bail!("Directory not empty");
            }
        }

        // Get parent directory and name
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path(&parent_path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Parent directory does not exist"))?;

        let name = components.last().unwrap();

        // Delete the specific directory entry (not all entries pointing to this inode)
        self.conn
            .execute(
                "DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?",
                (parent_ino, name.as_str()),
            )
            .await?;

        // Check if this was the last link to the inode
        let link_count = self.get_link_count(ino).await?;
        if link_count == 0 {
            // Manually handle cascading deletes since we don't use foreign keys
            // Delete data blocks
            self.conn
                .execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
                .await?;

            // Delete symlink if exists
            self.conn
                .execute("DELETE FROM fs_symlink WHERE ino = ?", (ino,))
                .await?;

            // Delete inode
            self.conn
                .execute("DELETE FROM fs_inode WHERE ino = ?", (ino,))
                .await?;
        }

        Ok(())
    }

    /// Rename/move a file or directory
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from_path = self.normalize_path(from);
        let to_path = self.normalize_path(to);

        // Cannot rename root
        if from_path == "/" {
            return Err(FsError::RootOperation.into());
        }

        // Get source inode
        let src_ino = self
            .resolve_path(&from_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Get source stats to check if it's a directory
        let src_stats = self.stat(&from_path).await?.ok_or(FsError::NotFound)?;

        // Prevent renaming a directory into its own subtree (would create a cycle)
        if src_stats.is_directory() {
            let from_prefix = format!("{}/", from_path);
            if to_path.starts_with(&from_prefix) || to_path == from_path {
                return Err(FsError::InvalidRename.into());
            }
        }

        // Parse source path to get parent and name
        let from_components = self.split_path(&from_path);
        let src_name = from_components.last().ok_or(FsError::InvalidPath)?;
        let src_parent_path = if from_components.len() == 1 {
            "/".to_string()
        } else {
            format!(
                "/{}",
                from_components[..from_components.len() - 1].join("/")
            )
        };
        let src_parent_ino = self
            .resolve_path(&src_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Parse destination path to get parent and name
        let to_components = self.split_path(&to_path);
        if to_components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let dst_name = to_components.last().unwrap();
        let dst_parent_path = if to_components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", to_components[..to_components.len() - 1].join("/"))
        };
        let dst_parent_ino = self
            .resolve_path(&dst_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Check if destination exists
        if let Some(dst_ino) = self.resolve_path(&to_path).await? {
            let dst_stats = self.stat(&to_path).await?.ok_or(FsError::NotFound)?;

            // Can't replace directory with non-directory
            if dst_stats.is_directory() && !src_stats.is_directory() {
                return Err(FsError::IsADirectory.into());
            }

            // Can't replace non-directory with directory
            if !dst_stats.is_directory() && src_stats.is_directory() {
                return Err(FsError::NotADirectory.into());
            }

            // If destination is directory, it must be empty
            if dst_stats.is_directory() {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?",
                        (dst_ino,),
                    )
                    .await?;

                if let Some(row) = rows.next().await? {
                    let count = row
                        .get_value(0)
                        .ok()
                        .and_then(|v| v.as_integer().copied())
                        .unwrap_or(0);
                    if count > 0 {
                        return Err(FsError::NotEmpty.into());
                    }
                }
            }

            // Remove destination entry
            self.conn
                .execute(
                    "DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?",
                    (dst_parent_ino, dst_name.as_str()),
                )
                .await?;

            // Clean up destination inode if no more links
            let link_count = self.get_link_count(dst_ino).await?;
            if link_count == 0 {
                self.conn
                    .execute("DELETE FROM fs_data WHERE ino = ?", (dst_ino,))
                    .await?;
                self.conn
                    .execute("DELETE FROM fs_symlink WHERE ino = ?", (dst_ino,))
                    .await?;
                self.conn
                    .execute("DELETE FROM fs_inode WHERE ino = ?", (dst_ino,))
                    .await?;
            }
        }

        // Update the dentry: change parent and/or name
        self.conn
            .execute(
                "UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?",
                (
                    dst_parent_ino,
                    dst_name.as_str(),
                    src_parent_ino,
                    src_name.as_str(),
                ),
            )
            .await?;

        // Update ctime of the inode
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn
            .execute(
                "UPDATE fs_inode SET ctime = ? WHERE ino = ?",
                (now, src_ino),
            )
            .await?;

        Ok(())
    }
}
