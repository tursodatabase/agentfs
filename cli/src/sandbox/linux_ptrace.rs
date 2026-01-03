//! Ptrace-based sandbox implementation using reverie.
//!
//! This module provides syscall interception via ptrace for filesystem
//! virtualization. This is experimental and requires root or CAP_SYS_PTRACE.

use agentfs_sandbox::{
    init_fd_tables, init_mount_table, init_strace, MountTable, Sandbox, SqliteVfs,
};
use reverie_process::Command;
use reverie_ptrace::TracerBuilder;
use std::{path::PathBuf, sync::Arc};

/// Run a command using the experimental ptrace-based syscall interception sandbox.
pub async fn run_cmd(strace: bool, command: PathBuf, args: Vec<String>) {
    eprintln!("Welcome to AgentFS!");
    eprintln!();

    let mut mount_table = MountTable::new();

    // Default mount: agent.db at /agent
    let db_path = PathBuf::from("agent.db");
    let mount_point = PathBuf::from("/agent");

    eprintln!("The following mount points are sandboxed:");
    eprintln!(
        " - {} -> {} (agentfs)",
        mount_point.display(),
        db_path.display()
    );
    eprintln!();

    let vfs = SqliteVfs::new(&db_path, mount_point.clone())
        .await
        .expect("Failed to create AgentFS VFS");
    mount_table.add_mount(mount_point, Arc::new(vfs));

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
