use std::process::Command;

fn main() {
    // Sandbox uses libunwind-ptrace which depends on liblzma and gcc_s.
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=lzma");
        // libgcc_s provides _Unwind_RaiseException and other exception handling symbols
        println!("cargo:rustc-link-lib=dylib=gcc_s");
    }

    // macOS: Weak-link libfuse so the binary can load without macFUSE installed.
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-arg=-Wl,-weak-lfuse");
        println!("cargo:rustc-link-search=/usr/local/lib");
        println!("cargo:rustc-link-search=/Library/Frameworks/macFUSE.framework/Versions/A");
    }

    // Capture git version from tags for --version flag
    // Rerun if git HEAD changes (new commits or tags)
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/tags");

    let version = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=AGENTFS_VERSION={}", version);
}
