use agentfs::{
    cmd::{self, completions::handle_completions},
    get_runtime,
    opts::{Args, Command, FsCommand, PruneCommand, ServeCommand, SyncCommand},
};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use tracing_subscriber::prelude::*;

/// Parse and validate encryption key and cipher options.
/// Both must be provided together or neither.
fn parse_encryption(key: Option<String>, cipher: Option<String>) -> Option<(String, String)> {
    match (key, cipher) {
        (Some(key), Some(cipher)) => Some((key, cipher)),
        (Some(_), None) => {
            eprintln!("Error: --cipher is required when using --key");
            std::process::exit(1);
        }
        (None, Some(_)) => {
            eprintln!("Error: --key is required when using --cipher");
            std::process::exit(1);
        }
        (None, None) => None,
    }
}

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
        Command::Init {
            id,
            force,
            base,
            key,
            cipher,
            command,
            backend,
            sync,
        } => {
            let rt = get_runtime();
            let encryption_opts = parse_encryption(key, cipher)
                .map(|(key, cipher)| cmd::init::EncryptionOptions { key, cipher });
            if let Err(e) = rt.block_on(cmd::init::init_database(
                id,
                sync,
                force,
                base,
                encryption_opts,
                command,
                backend,
            )) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Sync {
            id_or_path,
            command,
        } => match command {
            SyncCommand::Pull => {
                let rt = get_runtime();
                if let Err(e) = rt.block_on(cmd::sync::handle_pull_command(id_or_path)) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            SyncCommand::Push => {
                let rt = get_runtime();
                if let Err(e) = rt.block_on(cmd::sync::handle_push_command(id_or_path)) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            SyncCommand::Checkpoint => {
                let rt = get_runtime();
                if let Err(e) = rt.block_on(cmd::sync::handle_checkpoint_command(id_or_path)) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            SyncCommand::Stats => {
                let rt = get_runtime();
                if let Err(e) = rt.block_on(cmd::sync::handle_stats_command(
                    &mut std::io::stdout(),
                    id_or_path,
                )) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Command::Run {
            allow,
            no_default_allows,
            experimental_sandbox,
            strace,
            session,
            system,
            key,
            cipher,
            command,
            args,
        } => {
            let encryption = parse_encryption(key, cipher);
            let command = command.unwrap_or_else(default_shell);
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::handle_run_command(
                allow,
                no_default_allows,
                experimental_sandbox,
                strace,
                session,
                system,
                encryption,
                command,
                args,
            )) {
                eprintln!("Error: {e:?}");
                std::process::exit(1);
            }
        }
        #[cfg(unix)]
        Command::Exec {
            id_or_path,
            command,
            args,
            backend,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher);
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::exec::handle_exec_command(
                id_or_path, command, args, backend, encryption,
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
            system,
            foreground,
            uid,
            gid,
            backend,
        } => match (id_or_path, mountpoint) {
            (Some(id_or_path), Some(mountpoint)) => {
                if let Err(e) = cmd::mount(cmd::MountArgs {
                    id_or_path,
                    mountpoint,
                    auto_unmount,
                    allow_root,
                    allow_other: system,
                    foreground,
                    uid,
                    gid,
                    backend,
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
        },
        Command::Diff { id_or_path } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::fs::diff_filesystem(id_or_path)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Timeline {
            id_or_path,
            limit,
            filter,
            status,
            format,
        } => {
            let rt = get_runtime();
            let options = cmd::timeline::TimelineOptions {
                limit,
                filter,
                status,
                format,
            };
            if let Err(e) = rt.block_on(cmd::timeline::show_timeline(
                &mut std::io::stdout(),
                &id_or_path,
                &options,
            )) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Fs {
            command,
            id_or_path,
            key,
            cipher,
        } => {
            let encryption = parse_encryption(key, cipher);
            let rt = get_runtime();
            match command {
                FsCommand::Ls { fs_path } => {
                    if let Err(e) = rt.block_on(cmd::fs::ls_filesystem(
                        &mut std::io::stdout(),
                        id_or_path,
                        &fs_path,
                        encryption.as_ref(),
                    )) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
                FsCommand::Cat { file_path } => {
                    if let Err(e) = rt.block_on(cmd::fs::cat_filesystem(
                        &mut std::io::stdout(),
                        id_or_path,
                        &file_path,
                        encryption.as_ref(),
                    )) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
                FsCommand::Write { file_path, content } => {
                    if let Err(e) = rt.block_on(cmd::fs::write_filesystem(
                        id_or_path,
                        &file_path,
                        &content,
                        encryption.as_ref(),
                    )) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Completions { command } => handle_completions(command),
        #[cfg(unix)]
        Command::Nfs {
            id_or_path,
            bind,
            port,
        } => {
            eprintln!("Warning: `agentfs nfs` is deprecated, use `agentfs serve nfs` instead");
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::nfs::handle_nfs_command(id_or_path, bind, port)) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::McpServer { id_or_path, tools } => {
            eprintln!(
                "Warning: `agentfs mcp-server` is deprecated, use `agentfs serve mcp` instead"
            );
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::mcp_server::handle_mcp_server_command(
                id_or_path, tools,
            )) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Serve { command } => match command {
            #[cfg(unix)]
            ServeCommand::Nfs {
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
            ServeCommand::Mcp { id_or_path, tools } => {
                let rt = get_runtime();
                if let Err(e) = rt.block_on(cmd::mcp_server::handle_mcp_server_command(
                    id_or_path, tools,
                )) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Command::Ps => {
            if let Err(e) = cmd::ps::list_ps(&mut std::io::stdout()) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::Prune { command } => match command {
            PruneCommand::Mounts { force } => {
                if let Err(e) = cmd::mount::prune_mounts(force) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Command::Migrate {
            id_or_path,
            dry_run,
        } => {
            let rt = get_runtime();
            if let Err(e) = rt.block_on(cmd::migrate::handle_migrate_command(
                &mut std::io::stdout(),
                id_or_path,
                dry_run,
            )) {
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
