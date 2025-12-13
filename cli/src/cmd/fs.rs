use std::collections::VecDeque;

use agentfs_sdk::AgentFSOptions;
use anyhow::{Context, Result as AnyhowResult};
use turso::{Builder, Value};

const ROOT_INO: i64 = 1;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

pub async fn ls_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
) -> AnyhowResult<()> {
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

            stdout
                .write_fmt(format_args!("{} {}\n", type_char, full_path))
                .context("Failed to write to stdout")?;

            if is_dir {
                queue.push_back((ino, full_path));
            }
        }
    }

    Ok(())
}

pub async fn cat_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
) -> AnyhowResult<()> {
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
        "SELECT data FROM fs_data WHERE ino = {} ORDER BY chunk_index",
        current_ino
    );

    let mut rows = conn
        .query(&query, ())
        .await
        .context("Failed to query file data")?;

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

        stdout
            .write_all(&data)
            .context("Failed to write to stdout")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use agentfs_sdk::{AgentFS, AgentFSOptions};
    use tempfile::NamedTempFile;

    use crate::cmd::fs::cat_filesystem;
    use crate::cmd::fs::ls_filesystem;

    async fn agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()))
            .await
            .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    #[tokio::test]
    pub async fn cat_file_not_found() {
        let (_agentfs, path, _file) = agentfs().await;
        let mut buf = Vec::new();
        let err = cat_filesystem(&mut buf, path, "test.md").await.unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[tokio::test]
    pub async fn cat_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = b"hello, agentfs";
        agentfs.fs.write_file("test.md", content).await.unwrap();
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md").await.unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    pub async fn cat_big_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = vec![100u8; 4 * 1024 * 1024];
        agentfs.fs.write_file("test.md", &content).await.unwrap();
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md").await.unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    pub async fn ls_empty() {
        let (_agentfs, path, _file) = agentfs().await;
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/").await.unwrap();
        assert_eq!(buf, b"");
    }

    #[tokio::test]
    pub async fn ls_files_only() {
        let (agentfs, path, _file) = agentfs().await;
        agentfs.fs.write_file("1.md", b"1").await.unwrap();
        agentfs.fs.write_file("2.md", b"11").await.unwrap();
        let big = vec![100u8; 1024 * 1024];
        agentfs.fs.write_file("3.md", &big).await.unwrap();
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/").await.unwrap();
        assert_eq!(
            buf,
            b"f 1.md
f 2.md
f 3.md
"
        );
    }

    #[tokio::test]
    pub async fn ls_dirs() {
        let (agentfs, path, _file) = agentfs().await;
        agentfs.fs.mkdir("a").await.unwrap();
        agentfs.fs.mkdir("a/b").await.unwrap();
        agentfs.fs.mkdir("a/c").await.unwrap();
        agentfs.fs.mkdir("d").await.unwrap();
        agentfs.fs.mkdir("d/e").await.unwrap();
        agentfs.fs.write_file("a/b/1.md", b"1").await.unwrap();
        agentfs.fs.write_file("a/c/2.md", b"11").await.unwrap();
        let big = vec![100u8; 1024 * 1024];
        agentfs.fs.write_file("d/e/3.md", &big).await.unwrap();
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/").await.unwrap();
        assert_eq!(
            buf,
            b"d a
d d
d a/b
d a/c
d d/e
f a/b/1.md
f a/c/2.md
f d/e/3.md
"
        );
    }
}
