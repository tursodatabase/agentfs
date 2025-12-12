use std::collections::{HashSet, VecDeque};

use agentfs_sdk::{AgentFS, AgentFSOptions};
use anyhow::{Context, Result as AnyhowResult};
use turso::{Builder, Connection, Value};

const ROOT_INO: i64 = 1;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

pub async fn ls_filesystem(id_or_path: String, path: &str) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let db_path = options.path.context("No database path resolved")?;
    eprintln!("Using agent: {}", id_or_path);

    let db = Builder::new_local(&db_path)
        .build()
        .await
        .context("Failed to open filesystem")?;

    let conn = db.connect().context("Failed to connect to filesystem")?;

    if path != "/" {
        anyhow::bail!("Only root directory (/) is currently supported");
    }

    let mut queue: VecDeque<(i64, String)> = VecDeque::new();
    queue.push_back((ROOT_INO, String::new()));

    while let Some((parent_ino, prefix)) = queue.pop_front() {
        let query = format!(
            "SELECT d.name, d.ino, i.mode FROM fs_dentry d
             JOIN fs_inode i ON d.ino = i.ino
             WHERE d.parent_ino = {}
             ORDER BY d.name",
            parent_ino
        );

        let mut rows = conn
            .query(&query, ())
            .await
            .context("Failed to query directory entries")?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await.context("Failed to fetch row")? {
            let name: String = row
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

            let ino: i64 = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let mode: u32 = row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            entries.push((name, ino, mode));
        }

        for (name, ino, mode) in entries {
            let is_dir = mode & S_IFMT == S_IFDIR;
            let type_char = if is_dir { 'd' } else { 'f' };
            let full_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            println!("{} {}", type_char, full_path);

            if is_dir {
                queue.push_back((ino, full_path));
            }
        }
    }

    Ok(())
}

pub async fn cat_filesystem(id_or_path: String, path: &str) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let db_path = options.path.context("No database path resolved")?;

    let db = Builder::new_local(&db_path)
        .build()
        .await
        .context("Failed to open filesystem")?;

    let conn = db.connect().context("Failed to connect to filesystem")?;

    let path_components: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut current_ino = ROOT_INO;

    for component in path_components {
        let query = format!(
            "SELECT ino FROM fs_dentry WHERE parent_ino = {} AND name = '{}'",
            current_ino, component
        );

        let mut rows = conn
            .query(&query, ())
            .await
            .context("Failed to query directory entries")?;

        if let Some(row) = rows.next().await.context("Failed to fetch row")? {
            current_ino = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| anyhow::anyhow!("Invalid inode"))?;
        } else {
            anyhow::bail!("File not found: {}", path);
        }
    }

    let query = format!("SELECT mode FROM fs_inode WHERE ino = {}", current_ino);
    let mut rows = conn
        .query(&query, ())
        .await
        .context("Failed to query inode")?;

    if let Some(row) = rows.next().await.context("Failed to fetch row")? {
        let mode: u32 = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0) as u32;

        if mode & S_IFMT == S_IFDIR {
            anyhow::bail!("'{}' is a directory", path);
        } else if mode & S_IFMT != S_IFREG {
            anyhow::bail!("'{}' is not a regular file", path);
        }
    } else {
        anyhow::bail!("File not found: {}", path);
    }

    let query = format!(
        "SELECT data FROM fs_data WHERE ino = {} ORDER BY offset",
        current_ino
    );

    let mut rows = conn
        .query(&query, ())
        .await
        .context("Failed to query file data")?;

    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    while let Some(row) = rows.next().await.context("Failed to fetch row")? {
        let data: Vec<u8> = row
            .get_value(0)
            .ok()
            .and_then(|v| {
                if let Value::Blob(b) = v {
                    Some(b.clone())
                } else if let Value::Text(t) = v {
                    Some(t.as_bytes().to_vec())
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow::anyhow!("Invalid file data"))?;

        handle
            .write_all(&data)
            .context("Failed to write to stdout")?;
    }

    Ok(())
}

/// Represents a change type in the overlay filesystem
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChangeType {
    Added,
    Modified,
    Deleted,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeType::Added => write!(f, "A"),
            ChangeType::Modified => write!(f, "M"),
            ChangeType::Deleted => write!(f, "D"),
        }
    }
}

/// Get all paths in the delta layer (files in fs_dentry)
async fn get_delta_paths(conn: &Connection) -> AnyhowResult<HashSet<String>> {
    let mut paths = HashSet::new();
    let mut queue: VecDeque<(i64, String)> = VecDeque::new();
    queue.push_back((ROOT_INO, String::new()));

    while let Some((parent_ino, prefix)) = queue.pop_front() {
        let query = format!(
            "SELECT d.name, d.ino, i.mode FROM fs_dentry d
             JOIN fs_inode i ON d.ino = i.ino
             WHERE d.parent_ino = {}
             ORDER BY d.name",
            parent_ino
        );

        let mut rows = conn
            .query(&query, ())
            .await
            .context("Failed to query directory entries")?;

        while let Some(row) = rows.next().await.context("Failed to fetch row")? {
            let name: String = row
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

            let ino: i64 = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let mode: u32 = row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            let full_path = if prefix.is_empty() {
                format!("/{}", name)
            } else {
                format!("{}/{}", prefix, name)
            };

            paths.insert(full_path.clone());

            let is_dir = mode & S_IFMT == S_IFDIR;
            if is_dir {
                queue.push_back((ino, full_path));
            }
        }
    }

    Ok(paths)
}

/// Get all whiteouts (deleted paths from base)
async fn get_whiteouts(conn: &Connection) -> AnyhowResult<HashSet<String>> {
    let mut whiteouts = HashSet::new();

    let result = conn.query("SELECT path FROM fs_whiteout", ()).await;

    if let Ok(mut rows) = result {
        while let Some(row) = rows.next().await? {
            if let Ok(Value::Text(path)) = row.get_value(0) {
                whiteouts.insert(path.clone());
            }
        }
    } // Err case: Table doesn't exist, return empty set

    Ok(whiteouts)
}

/// Get file type character
fn file_type_char(mode: u32) -> char {
    match mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        S_IFREG => 'f',
        _ => '?',
    }
}

/// Get file mode from delta layer
async fn get_file_mode(conn: &Connection, path: &str) -> AnyhowResult<Option<u32>> {
    // Resolve path to inode
    let components: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    if components.is_empty() {
        // Root directory
        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ROOT_INO,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            return Ok(Some(mode));
        }
        return Ok(None);
    }

    let mut current_ino = ROOT_INO;
    for component in &components {
        let query = format!(
            "SELECT ino FROM fs_dentry WHERE parent_ino = {} AND name = '{}'",
            current_ino, component
        );

        let mut rows = conn.query(&query, ()).await?;

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

    let mut rows = conn
        .query("SELECT mode FROM fs_inode WHERE ino = ?", (current_ino,))
        .await?;

    if let Some(row) = rows.next().await? {
        let mode = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0) as u32;
        return Ok(Some(mode));
    }

    Ok(None)
}

/// Check if a path exists in the host filesystem (base layer)
fn path_exists_in_base(base_path: &str, rel_path: &str) -> bool {
    let full_path = format!("{}{}", base_path, rel_path);
    std::path::Path::new(&full_path).exists()
}

pub async fn diff_filesystem(id_or_path: String) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = AgentFS::open(options).await.context("Failed to open agent")?;
    let conn = agent.get_connection();

    // Check if overlay is enabled
    let base_path = match agent.is_overlay_enabled().await? {
        Some(path) => path,
        None => {
            println!("No diff (non-overlay filesystem)");
            return Ok(());
        }
    };

    eprintln!("Base: {}", base_path);

    // Collect all changes
    let mut changes: Vec<(ChangeType, char, String)> = Vec::new();

    // Get all paths in delta layer
    let delta_paths = get_delta_paths(&conn).await?;

    // Get all whiteouts (deleted paths)
    let whiteouts = get_whiteouts(&conn).await?;

    // Process delta paths - determine if added or modified
    for path in &delta_paths {
        let mode = get_file_mode(&conn, path).await?.unwrap_or(0);
        let type_char = file_type_char(mode);

        if path_exists_in_base(&base_path, path) {
            // File exists in both - it was modified (copy-on-write)
            changes.push((ChangeType::Modified, type_char, path.clone()));
        } else {
            // File only exists in delta - it was added
            changes.push((ChangeType::Added, type_char, path.clone()));
        }
    }

    // Process whiteouts (deleted files)
    for path in &whiteouts {
        // Determine file type from base if possible, otherwise use '?'
        let full_path = format!("{}{}", base_path, path);
        let base_path_obj = std::path::Path::new(&full_path);
        let type_char = if base_path_obj.is_dir() {
            'd'
        } else if base_path_obj.is_symlink() {
            'l'
        } else if base_path_obj.is_file() {
            'f'
        } else {
            '?'
        };

        changes.push((ChangeType::Deleted, type_char, path.clone()));
    }

    // Sort changes by path for consistent output
    changes.sort_by(|a, b| a.2.cmp(&b.2));

    // Print changes
    if changes.is_empty() {
        println!("No changes");
    } else {
        for (change_type, type_char, path) in changes {
            println!("{} {} {}", change_type, type_char, path);
        }
    }

    Ok(())
}
