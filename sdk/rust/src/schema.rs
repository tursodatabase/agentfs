//! Schema versioning and detection for AgentFS databases.

use crate::error::{Error, Result};
use turso::Connection;

/// Current schema version.
pub const AGENTFS_SCHEMA_VERSION: &str = "0.4";

/// Detected schema version based on column introspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaVersion {
    /// Base schema: fs_inode, fs_dentry, fs_data, fs_symlink, fs_config, kv_store, tool_calls
    V0_0,
    /// Added nlink column to fs_inode
    V0_2,
    /// Added atime_nsec, mtime_nsec, ctime_nsec, rdev columns to fs_inode
    V0_4,
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaVersion::V0_0 => write!(f, "0.0"),
            SchemaVersion::V0_2 => write!(f, "0.2"),
            SchemaVersion::V0_4 => write!(f, "0.4"),
        }
    }
}

impl SchemaVersion {
    /// Returns the version string.
    pub fn as_str(&self) -> &'static str {
        match self {
            SchemaVersion::V0_0 => "0.0",
            SchemaVersion::V0_2 => "0.2",
            SchemaVersion::V0_4 => "0.4",
        }
    }

    /// Returns true if this version is the current version.
    pub fn is_current(&self) -> bool {
        matches!(self, SchemaVersion::V0_4)
    }
}

/// Column information from PRAGMA table_info.
#[derive(Debug)]
struct ColumnInfo {
    name: String,
}

/// Detect the schema version of an existing database by introspecting fs_inode columns.
/// Returns None if the database has no fs_inode table (new database).
pub async fn detect_schema_version(conn: &Connection) -> Result<Option<SchemaVersion>> {
    // Check if fs_inode table exists
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='fs_inode'",
            (),
        )
        .await?;

    if rows.next().await?.is_none() {
        // New database
        return Ok(None);
    }

    // Get columns from fs_inode table
    let columns = get_table_columns(conn, "fs_inode").await?;

    let has_nlink = columns.iter().any(|c| c.name == "nlink");
    let has_atime_nsec = columns.iter().any(|c| c.name == "atime_nsec");
    let has_mtime_nsec = columns.iter().any(|c| c.name == "mtime_nsec");
    let has_ctime_nsec = columns.iter().any(|c| c.name == "ctime_nsec");
    let has_rdev = columns.iter().any(|c| c.name == "rdev");

    // v0.4: has all nsec columns and rdev
    if has_atime_nsec && has_mtime_nsec && has_ctime_nsec && has_rdev {
        return Ok(Some(SchemaVersion::V0_4));
    }

    // v0.2: has nlink but not the nsec columns
    if has_nlink {
        return Ok(Some(SchemaVersion::V0_2));
    }

    // v0.0: base schema
    Ok(Some(SchemaVersion::V0_0))
}

/// Check that a database has a compatible schema version.
/// Returns Ok(()) for new databases or databases at the current version.
/// Returns Err(SchemaVersionMismatch) for databases with old schemas.
pub async fn check_schema_version(conn: &Connection) -> Result<()> {
    if let Some(version) = detect_schema_version(conn).await? {
        if !version.is_current() {
            return Err(Error::SchemaVersionMismatch {
                found: version.to_string(),
                expected: AGENTFS_SCHEMA_VERSION.to_string(),
            });
        }
    }
    Ok(())
}

/// Get column information for a table using PRAGMA table_info.
async fn get_table_columns(conn: &Connection, table_name: &str) -> Result<Vec<ColumnInfo>> {
    let mut rows = conn
        .query(&format!("PRAGMA table_info({})", table_name), ())
        .await?;

    let mut columns = Vec::new();
    while let Some(row) = rows.next().await? {
        let name: String = row.get(1)?;
        columns.push(ColumnInfo { name });
    }

    Ok(columns)
}
