mod cmd;

#[cfg(target_os = "linux")]
mod daemon;

#[cfg(target_os = "linux")]
mod fuse;

use clap::{Parser, Subcommand};
use cmd::MountConfig;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "agentfs")]
#[command(about = "The filesystem for agents", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
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
        command: FsCommand,
    },
    /// Run a command in the sandboxed environment (experimental).
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
enum FsCommand {
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

fn main() {
    reset_sigpipe();

    let args = Args::parse();

    match args.command {
        Command::Init { id, force } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            if let Err(e) = rt.block_on(cmd::init::init_database(id, force)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Fs { command } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            match command {
                FsCommand::Ls {
                    id_or_path,
                    fs_path,
                } => {
                    if let Err(e) = rt.block_on(cmd::fs::ls_filesystem(id_or_path, &fs_path)) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
                FsCommand::Cat {
                    id_or_path,
                    file_path,
                } => {
                    if let Err(e) = rt.block_on(cmd::fs::cat_filesystem(id_or_path, &file_path)) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Run {
            mounts,
            strace,
            command,
            args,
        } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(cmd::handle_run_command(mounts, strace, command, args));
        }
        Command::Mount {
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

/// Reset SIGPIPE to the default behavior (terminate the process) so that
/// piping output to tools like `head` doesn't cause a panic.
#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}
