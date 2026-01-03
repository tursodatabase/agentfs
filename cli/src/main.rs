use agentfs::{
    cmd::{self, completions::handle_completions},
    get_runtime,
    parser::{Args, Command, FsCommand},
};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use tracing_subscriber::prelude::*;

fn main() {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentfs=info".into()),
        )
        .try_init();

    reset_sigpipe();

    CompleteEnv::with_factory(Args::command).complete();
    let args = Args::parse();

    match args.command {
        Command::Init { id, force, base } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::init::init_database(id, force, base)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Run {
            allow,
            no_default_allows,
            experimental_sandbox,
            strace,
            session,
            command,
            args,
        } => {
            let command = command.unwrap_or_else(default_shell);
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::handle_run_command(
                allow,
                no_default_allows,
                experimental_sandbox,
                strace,
                session,
                command,
                args,
            )) {
                eprintln!("Error: {e:?}");
                std::process::exit(1);
            }
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
            match (id_or_path, mountpoint) {
                (Some(id_or_path), Some(mountpoint)) => {
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
                (None, None) => {
                    cmd::mount::list_mounts(&mut std::io::stdout());
                }
                _ => {
                    eprintln!("Error: both ID_OR_PATH and MOUNTPOINT are required to mount");
                    std::process::exit(1);
                }
            }
        }
        Command::Diff { id_or_path } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::fs::diff_filesystem(id_or_path)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Fs { command } => {
            let rt = get_runtime();
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
        Command::Completions { command } => handle_completions(command),
        Command::Ps { all } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::ps::ps(&mut std::io::stdout(), all)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        #[cfg(unix)]
        Command::Nfs {
            id_or_path,
            bind,
            port,
        } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::nfs::handle_nfs_command(id_or_path, bind, port)) {
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

/// Returns the default shell for the current platform.
/// Linux uses bash, macOS uses zsh.
fn default_shell() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::path::PathBuf::from("zsh")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::path::PathBuf::from("bash")
    }
}
