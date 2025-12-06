fn main() {
    // Sandbox uses libunwind-ptrace which depends on liblzma and gcc_s.
    // Only available on Linux x86_64.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        println!("cargo:rustc-link-lib=lzma");
        println!("cargo:rustc-link-lib=dylib=gcc_s");
    }
}
