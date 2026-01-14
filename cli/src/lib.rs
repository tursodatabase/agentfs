pub mod cmd;
pub mod parser;
pub mod sandbox;

#[cfg(target_os = "linux")]
pub mod daemon;

#[cfg(target_os = "linux")]
pub mod fuse;

#[cfg(target_os = "linux")]
pub mod fuser;

#[cfg(unix)]
pub mod nfs;

pub fn get_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Internal error: failed to initialize runtime")
}
