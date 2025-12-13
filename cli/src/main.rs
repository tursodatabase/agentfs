mod cmd;
mod parser;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod daemon;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod fuse;

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use crate::{
    cmd::completions::handle_completions,
    parser::{Args, Command, FsCommand},
};

fn main() {
    reset_sigpipe();

    CompleteEnv::with_factory(Args::command).complete();
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
                    if let Err(e) = rt.block_on(cmd::fs::ls_filesystem(
                        &mut std::io::stdout(),
                        id_or_path,
                        &fs_path,
                    )) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
                FsCommand::Cat {
                    id_or_path,
                    file_path,
                } => {
                    if let Err(e) = rt.block_on(cmd::fs::cat_filesystem(
                        &mut std::io::stdout(),
                        id_or_path,
                        &file_path,
                    )) {
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
        Command::Completions { command } => handle_completions(command),
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
