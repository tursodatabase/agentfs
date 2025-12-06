mod cmd;

#[cfg(target_os = "linux")]
mod daemon;

#[cfg(target_os = "linux")]
mod fuse;

use agentfs_sdk::{AgentFS, AgentFSOptions};
use anyhow::{Context, Result as AnyhowResult};
use clap::{Parser, Subcommand};
use cmd::MountConfig;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use turso::{Builder, Value};

#[derive(Parser, Debug)]
#[command(name = "agentfs")]
#[command(about = "A sandbox for agents that intercepts filesystem operations", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a new agent filesystem
    Init {
        /// Agent identifier (if not provided, generates a unique one)
        id: Option<String>,

        /// Overwrite existing file if it exists
        #[arg(long)]
        force: bool,
    },
    /// Filesystem operations
    Fs {
        #[command(subcommand)]
        command: FsCommands,
    },
    Run {
        /// Mount configuration (format: type=bind,src=<host_path>,dst=<sandbox_path>)
        #[arg(long = "mount", value_name = "MOUNT_SPEC")]
        mounts: Vec<MountConfig>,

        /// Enable strace-like output for system calls
        #[arg(long = "strace")]
        strace: bool,

        /// Command to execute
        command: PathBuf,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Mount an agent filesystem using FUSE
    Mount {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH")]
        id_or_path: String,

        /// Mount point directory
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,

        /// Automatically unmount on exit
        #[arg(short = 'a', long)]
        auto_unmount: bool,

        /// Allow root user to access filesystem
        #[arg(long)]
        allow_root: bool,

        /// Run in foreground (don't daemonize)
        #[arg(short = 'f', long)]
        foreground: bool,

        /// User ID to report for all files (defaults to current user)
        #[arg(long)]
        uid: Option<u32>,

        /// Group ID to report for all files (defaults to current group)
        #[arg(long)]
        gid: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
enum FsCommands {
    /// List files in the filesystem
    Ls {
        /// Agent ID or database path
        id_or_path: String,

        /// Path to list (default: /)
        #[arg(default_value = "/")]
        fs_path: String,
    },
    /// Display file contents
    Cat {
        /// Agent ID or database path
        id_or_path: String,

        /// Path to the file in the filesystem
        file_path: String,
    },
}

async fn ls_filesystem(id_or_path: String, path: &str) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let db_path = options.path.context("No database path resolved")?;
    eprintln!("Using agent: {}", id_or_path);

    let db = Builder::new_local(&db_path)
        .build()
        .await
        .context("Failed to open filesystem")?;

    let conn = db.connect().context("Failed to connect to filesystem")?;

    const ROOT_INO: i64 = 1;
    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;

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

async fn cat_filesystem(id_or_path: String, path: &str) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let db_path = options.path.context("No database path resolved")?;

    let db = Builder::new_local(&db_path)
        .build()
        .await
        .context("Failed to open filesystem")?;

    let conn = db.connect().context("Failed to connect to filesystem")?;

    const ROOT_INO: i64 = 1;

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

        const S_IFMT: u32 = 0o170000;
        const S_IFDIR: u32 = 0o040000;
        const S_IFREG: u32 = 0o100000;

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

async fn init_database(id: Option<String>, force: bool) -> AnyhowResult<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Generate ID if not provided
    let id = id.unwrap_or_else(|| {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("agent-{}", timestamp)
    });

    // Validate agent ID for safety
    if !AgentFS::validate_agent_id(&id) {
        anyhow::bail!(
            "Invalid agent ID '{}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.",
            id
        );
    }

    // Check if agent already exists
    let db_path = Path::new(".agentfs").join(format!("{}.db", id));
    if db_path.exists() && !force {
        anyhow::bail!(
            "Agent '{}' already exists at '{}'. Use --force to overwrite.",
            id,
            db_path.display()
        );
    }

    // Use the SDK to initialize the database - this ensures consistency
    // The SDK will create .agentfs directory and database file
    AgentFS::open(AgentFSOptions::with_id(&id))
        .await
        .context("Failed to initialize database")?;

    eprintln!("Created agent filesystem: {}", db_path.display());
    eprintln!("Agent ID: {}", id);

    Ok(())
}

fn main() {
    let args = Args::parse();

    match args.command {
        Commands::Init { id, force } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            if let Err(e) = rt.block_on(init_database(id, force)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Fs { command } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            match command {
                FsCommands::Ls {
                    id_or_path,
                    fs_path,
                } => {
                    if let Err(e) = rt.block_on(ls_filesystem(id_or_path, &fs_path)) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
                FsCommands::Cat {
                    id_or_path,
                    file_path,
                } => {
                    if let Err(e) = rt.block_on(cat_filesystem(id_or_path, &file_path)) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::Run {
            mounts,
            strace,
            command,
            args,
        } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(cmd::handle_run_command(mounts, strace, command, args));
        }
        Commands::Mount {
            id_or_path,
            mountpoint,
            auto_unmount,
            allow_root,
            foreground,
            uid,
            gid,
        } => {
            if let Err(e) = cmd::mount(cmd::MountArgs {
                id_or_path,
                mountpoint,
                auto_unmount,
                allow_root,
                foreground,
                uid,
                gid,
            }) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }
}
