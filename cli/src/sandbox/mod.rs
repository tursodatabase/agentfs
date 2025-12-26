//! Sandbox implementations for running commands in isolated environments.
//!
//! This module provides two sandbox approaches:
//! - `overlay`: FUSE + namespace-based sandbox with copy-on-write filesystem
//! - `ptrace`: ptrace-based syscall interception sandbox (experimental)

#[cfg(target_os = "linux")]
pub mod overlay;

#[cfg(target_os = "linux")]
pub mod ptrace;
