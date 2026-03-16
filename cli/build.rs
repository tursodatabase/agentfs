use std::process::Command;

fn main() {
    compile_appfs_grpc_bridge_proto();

    // Sandbox uses libunwind-ptrace which depends on liblzma and gcc_s.
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=lzma");
        // libgcc_s provides _Unwind_RaiseException and other exception handling symbols
        println!("cargo:rustc-link-lib=dylib=gcc_s");
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

    // Enable delay-load for winfsp on Windows
    // Note: This requires WinFsp to be installed from https://winfsp.dev/rel/
    // The winfsp crate includes pre-built bindings, no LLVM/bindgen needed
    #[cfg(target_os = "windows")]
    {
        winfsp::build::winfsp_link_delayload();
    }
}

fn compile_appfs_grpc_bridge_proto() {
    let proto = "../examples/appfs/grpc-bridge/proto/appfs_adapter_v1.proto";
    let include_dir = "../examples/appfs/grpc-bridge/proto";

    println!("cargo:rerun-if-changed={proto}");
    if !std::path::Path::new(proto).exists() {
        return;
    }

    if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[include_dir])
        .expect("failed to compile AppFS gRPC bridge proto");
}
