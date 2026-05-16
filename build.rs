fn main() {
    // Increase the Windows debug-build stack reserve. Large clap/config
    // construction frames fit within Linux's larger default stack, but can
    // exceed Windows' 1 MiB default when the real binary is exercised by
    // assert_cmd in CI.
    //
    // Keep this in build.rs instead of .cargo/config.toml so CI's global
    // RUSTFLAGS="-Dwarnings" cannot replace the linker setting.
    #[cfg(windows)]
    {
        println!("cargo:rustc-link-arg=/STACK:4194304");
    }
}
