use crate::cmd::completions::Shell;
use agentfs_sdk::agentfs_dir;
use clap::{Parser, Subcommand};
use clap_complete::{
    engine::ValueCompleter, ArgValueCompleter, CompletionCandidate, PathCompleter,
};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "agentfs")]
#[command(version = env!("AGENTFS_VERSION"))]
#[command(about = "The filesystem for agents", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
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
    },
    /// Filesystem operations
    Fs {
        #[command(subcommand)]
        command: FsCommand,
    },
    /// Run a command in the sandboxed environment.
    ///
    /// By default, uses FUSE+overlay with unshare/pivot_root for isolation.
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

        /// Command to execute
        command: PathBuf,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Mount an agent filesystem using FUSE
    Mount {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Mount point directory
        #[arg(value_name = "MOUNTPOINT", add = ArgValueCompleter::new(PathCompleter::dir()))]
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
    /// Show differences between base filesystem and delta (overlay mode only)
    Diff {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,
    },
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
}

#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// List files in the filesystem
    Ls {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Path to list (default: /)
        #[arg(default_value = "/")]
        fs_path: String,
    },
    /// Display file contents
    Cat {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Path to the file in the filesystem
        file_path: String,
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
