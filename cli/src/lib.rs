pub mod cmd;
pub mod opts;
pub mod sandbox;

#[cfg(target_os = "linux")]
pub mod daemon;

#[cfg(target_os = "linux")]
pub mod fuse;

#[cfg(target_os = "linux")]
pub mod fuser;

#[cfg(unix)]
pub mod nfsserve;

#[cfg(unix)]
pub mod nfs;

#[cfg(unix)]
pub mod mount;

pub fn get_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("Internal error: failed to initialize runtime")
}
