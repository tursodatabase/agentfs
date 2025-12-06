fn main() {
    // Only link libunwind dependencies when sandbox feature is enabled.
    // libunwind-ptrace.so depends on liblzma and gcc_s.
    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    {
        println!("cargo:rustc-link-lib=lzma");
        // libgcc_s provides _Unwind_RaiseException and other exception handling symbols
        println!("cargo:rustc-link-lib=dylib=gcc_s");
    }
}
