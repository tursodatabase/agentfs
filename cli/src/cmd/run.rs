use agentfs_sandbox::{
    init_fd_tables, init_mount_table, init_strace, BindVfs, MountConfig, MountTable, Sandbox,
    SqliteVfs,
};
use reverie_process::Command;
use reverie_ptrace::TracerBuilder;
use std::path::PathBuf;
use std::sync::Arc;

pub async fn run_sandbox(
    mut mounts: Vec<MountConfig>,
    strace: bool,
    command: PathBuf,
    args: Vec<String>,
) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();

    let mut mount_table = MountTable::new();

    // If no mounts specified, add default agent.db mount at /agent
    if mounts.is_empty() {
        mounts.push(MountConfig {
            mount_type: agentfs_sandbox::MountType::Sqlite {
                src: PathBuf::from("agent.db"),
            },
            dst: PathBuf::from("/agent"),
        });
    }

    eprintln!("The following mount points are sandboxed:");
    for mount_config in &mounts {
        match &mount_config.mount_type {
            agentfs_sandbox::MountType::Bind { src } => {
                eprintln!(
                    " - {} -> {} (host)",
                    mount_config.dst.display(),
                    src.display()
                );

                // Create a BindVfs for this bind mount
                let vfs = Arc::new(BindVfs::new(src.clone(), mount_config.dst.clone()));
                mount_table.add_mount(mount_config.dst.clone(), vfs);
            }
            agentfs_sandbox::MountType::Sqlite { src } => {
                eprintln!(
                    " - {} -> {} (sqlite)",
                    mount_config.dst.display(),
                    src.display()
                );

                // Create a SqliteVfs for this sqlite mount
                let vfs = SqliteVfs::new(src, mount_config.dst.clone())
                    .await
                    .expect("Failed to create SQLite VFS");
                mount_table.add_mount(mount_config.dst.clone(), Arc::new(vfs));
            }
        }
    }
    eprintln!();

    init_mount_table(mount_table);
    init_fd_tables();
    init_strace(strace);

    let mut cmd = Command::new(command);
    for arg in args {
        cmd.arg(arg);
    }

    let tracer = TracerBuilder::<Sandbox>::new(cmd).spawn().await.unwrap();

    let (status, _) = tracer.wait().await.unwrap();
    status.raise_or_exit()
}
