use crate::cmd::completions::Shell;
use agentfs_sdk::agentfs_dir;
use clap::{Parser, Subcommand};
use clap_complete::{
    engine::ValueCompleter, ArgValueCompleter, CompletionCandidate, PathCompleter,
};
use std::path::{Path, PathBuf};

/// Mount backend type
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MountBackend {
    /// FUSE filesystem (Linux only)
    Fuse,
    /// NFS over localhost
    Nfs,
}

// Platform-specific default: FUSE on Linux, NFS elsewhere
#[allow(clippy::derivable_impls)]
impl Default for MountBackend {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            MountBackend::Fuse
        }
        #[cfg(not(target_os = "linux"))]
        {
            MountBackend::Nfs
        }
    }
}

impl std::fmt::Display for MountBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountBackend::Fuse => write!(f, "fuse"),
            MountBackend::Nfs => write!(f, "nfs"),
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "agentfs")]
#[command(version = env!("AGENTFS_VERSION"))]
#[command(about = "The filesystem for agents", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Parser)]
pub struct SyncCommandOptions {
    #[arg(long)]
    pub sync_remote_url: Option<String>,
    #[arg(long)]
    pub sync_partial_prefetch: Option<bool>,
    #[arg(long)]
    pub sync_partial_segment_size: Option<usize>,
    #[arg(long)]
    pub sync_partial_bootstrap_query: Option<String>,
    #[arg(long)]
    pub sync_partial_bootstrap_length: Option<usize>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage shell completions
    Completions {
        #[command(subcommand)]
        command: CompletionsCommand,
    },
    /// Initialize a new agent filesystem
    Init {
        /// Agent identifier (if not provided, generates a unique one)
        id: Option<String>,

        /// Overwrite existing file if it exists
        #[arg(long)]
        force: bool,

        /// Base directory for overlay filesystem (copy-on-write)
        #[arg(long)]
        base: Option<PathBuf>,

        /// Hex-encoded encryption key.
        /// Enables local encryption when provided.
        #[arg(long, env = "AGENTFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "AGENTFS_CIPHER")]
        cipher: Option<String>,

        /// Command to execute after initialization (mounts the filesystem, runs command, unmounts)
        #[arg(short = 'c', long = "command")]
        command: Option<String>,

        /// Backend to use for mounting when using -c (default: fuse on Linux, nfs on macOS)
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        #[command(flatten)]
        sync: SyncCommandOptions,
    },
    /// Remote sync operations
    Sync {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        #[command(subcommand)]
        command: SyncCommand,
    },
    /// Filesystem operations
    Fs {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "AGENTFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "AGENTFS_CIPHER")]
        cipher: Option<String>,

        #[command(subcommand)]
        command: FsCommand,
    },
    /// Run a command in the sandboxed environment.
    ///
    /// By default, uses FUSE+overlay with Linux user and mount namespaces for isolation.
    /// The overlay uses the host filesystem as a read-only base and stores
    /// all changes in an AgentFS-backed delta layer.
    Run {
        /// Allow write access to additional directories (can be specified multiple times)
        #[arg(long = "allow", value_name = "PATH")]
        allow: Vec<PathBuf>,

        /// Disable default allowed directories (~/.config, ~/.cache, ~/.local, ~/.claude, etc.)
        #[arg(long = "no-default-allows")]
        no_default_allows: bool,

        /// Use experimental ptrace-based syscall interception sandbox
        #[arg(long = "experimental-sandbox")]
        experimental_sandbox: bool,

        /// Enable strace-like output for system calls
        /// Only used with --experimental-sandbox
        #[arg(long = "strace")]
        strace: bool,

        /// Session identifier for sharing delta layer across multiple runs.
        /// If not provided, a unique session ID is generated for each run.
        /// Use the same session ID to share the delta layer between runs.
        #[arg(long = "session", value_name = "ID")]
        session: Option<String>,

        /// Allow other system users to access this mount (requires /etc/fuse.conf
        /// user_allow_other; use cautiously)
        #[arg(long = "system")]
        system: bool,

        /// Hex-encoded encryption key for the delta layer.
        /// Enables local encryption when provided.
        #[arg(long, env = "AGENTFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "AGENTFS_CIPHER")]
        cipher: Option<String>,

        /// Command to execute (defaults to bash on Linux, zsh on macOS)
        command: Option<PathBuf>,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Execute a command with an AgentFS filesystem mounted.
    ///
    /// Mounts the specified AgentFS to a temporary directory, runs the command
    /// with that directory as the working directory, then automatically unmounts.
    /// This is useful for running tools that need filesystem access without
    /// a persistent mount.
    #[cfg(unix)]
    Exec {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Command to execute
        #[arg(value_name = "COMMAND")]
        command: PathBuf,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Backend to use for mounting (default: fuse on Linux, nfs on macOS)
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "AGENTFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        #[arg(long, env = "AGENTFS_CIPHER")]
        cipher: Option<String>,
    },
    /// Mount an agent filesystem using FUSE (or list mounts if no args)
    Mount {
        /// Agent ID or database path (if omitted, lists current mounts)
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: Option<String>,

        /// Mount point directory
        #[arg(value_name = "MOUNTPOINT", add = ArgValueCompleter::new(PathCompleter::dir()))]
        mountpoint: Option<PathBuf>,

        /// Automatically unmount on exit
        #[arg(short = 'a', long)]
        auto_unmount: bool,

        /// Allow root user to access filesystem
        #[arg(long)]
        allow_root: bool,

        /// Allow other system users to access this mount (requires /etc/fuse.conf
        /// user_allow_other; use cautiously)
        #[arg(long = "system")]
        system: bool,

        /// Run in foreground (don't daemonize)
        #[arg(short = 'f', long)]
        foreground: bool,

        /// User ID to report for all files (defaults to current user)
        #[arg(long)]
        uid: Option<u32>,

        /// Group ID to report for all files (defaults to current group)
        #[arg(long)]
        gid: Option<u32>,

        /// Backend to use for mounting
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,
    },
    /// Show differences between base filesystem and delta (overlay mode only)
    Diff {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,
    },
    /// Display agent action timeline from tool call audit log
    Timeline {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Limit number of entries to display
        #[arg(long, default_value = "100")]
        limit: i64,

        /// Filter by tool name
        #[arg(long)]
        filter: Option<String>,

        /// Filter by status (pending/success/error)
        #[arg(long, value_parser = ["pending", "success", "error"])]
        status: Option<String>,

        /// Output format
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        format: String,
    },
    /// Start an NFS server to export an AgentFS filesystem over the network
    /// (deprecated: use `agentfs serve nfs` instead)
    #[cfg(unix)]
    Nfs {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// IP address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Port to listen on
        #[arg(long, default_value = "11111")]
        port: u32,
    },

    /// Start an MCP server exposing filesystem and KV-store tools
    /// (deprecated: use `agentfs serve mcp` instead)
    McpServer {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Tools to expose (comma-separated). If not provided, all tools are exposed.
        /// Available tools: read_file, write_file, readdir, mkdir, rmdir, rm, unlink,
        /// copy_file, rename, stat, access, kv_get, kv_set, kv_delete, kv_list
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
    },

    /// Serve an AgentFS filesystem via different protocols
    Serve {
        #[command(subcommand)]
        command: ServeCommand,
    },
    /// List active agentfs run sessions
    Ps,
    /// Prune unused resources
    Prune {
        #[command(subcommand)]
        command: PruneCommand,
    },
    /// Migrate database schema to the current version
    Migrate {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Preview migration without applying changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// List files in the filesystem
    Ls {
        /// Path to list (default: /)
        #[arg(default_value = "/")]
        fs_path: String,
    },
    /// Display file contents
    Cat {
        /// Path to the file in the filesystem
        file_path: String,
    },
    /// Write file content
    Write {
        /// Path to the file in the filesystem
        file_path: String,

        /// Content of the file
        content: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SyncCommand {
    /// Pull remote changes (only of agentfs was initialized with remote sync)
    Pull,
    /// Push remote changes (only of agentfs was initialized with remote sync)
    Push,
    /// Print synced database stats
    Stats,
    /// Checkpoint local synced db
    Checkpoint,
}

#[derive(Subcommand, Debug)]
pub enum ServeCommand {
    /// Start an NFS server to export an AgentFS filesystem over the network
    #[cfg(unix)]
    Nfs {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// IP address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Port to listen on
        #[arg(long, default_value = "11111")]
        port: u32,
    },

    /// Start an MCP server exposing filesystem and KV-store tools
    Mcp {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Tools to expose (comma-separated). If not provided, all tools are exposed.
        /// Available tools: read_file, write_file, readdir, mkdir, rmdir, rm, unlink,
        /// copy_file, rename, stat, access, kv_get, kv_set, kv_delete, kv_list
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PruneCommand {
    /// Unmount unused agentfs mount points
    Mounts {
        /// Skip confirmation prompt and unmount immediately
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub enum CompletionsCommand {
    /// Install shell completions to your shell rc file
    Install {
        /// Shell to install completions for (defaults to current shell)
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
    /// Uninstall shell completions from your shell rc file
    Uninstall {
        /// Shell to uninstall completions for (defaults to current shell)
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
    /// Print instructions for manual installation
    Show,
}

fn id_completer(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let mut completions = vec![];
    let Some(current) = current.to_str() else {
        return completions;
    };

    let agentfs_dir = agentfs_dir();
    let Ok(read_dir) = agentfs_dir.read_dir() else {
        return completions;
    };

    let mut ids = read_dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name();
            let path = Path::new(&file_name);
            let name = path.file_prefix()?.to_str()?;
            if name.starts_with(current) {
                Some(CompletionCandidate::new(name).help(Some("Agent ID".into())))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    ids.sort();
    ids.dedup();

    completions.append(&mut ids);
    completions
}

fn id_or_path_completer(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let mut completions = vec![];

    // TODO: maybe filter files by `.db`
    let path_completer = PathCompleter::any();
    let mut path_completions = path_completer.complete(current);

    let mut ids = id_completer(current);

    completions.append(&mut ids);

    completions.append(&mut path_completions);

    completions
}
