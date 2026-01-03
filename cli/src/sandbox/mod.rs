//! Sandbox implementations for running commands in isolated environments.
//!
//! This module provides platform-specific sandbox approaches:
//! - `linux`: FUSE + namespace-based sandbox with copy-on-write filesystem
//! - `linux_ptrace`: ptrace-based syscall interception sandbox (experimental)
//! - `darwin`: Kernel-enforced sandbox using sandbox-exec

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub mod linux_ptrace;

#[cfg(target_os = "macos")]
pub mod darwin;
