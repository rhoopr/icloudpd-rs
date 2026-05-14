fn main() {
    // Increase stack reservation and commit on Windows to prevent stack
    // overflow during debug builds. Config::build_inner and related async
    // machinery create stack frames that fit within Linux's 8 MiB default
    // but exceed Windows' 1 MiB default in unoptimized builds.
    //
    // Pre-committing all 4 MiB avoids __chkstk probe failures during
    // function prologues on Windows. The separate /STACK args are needed
    // because comma-separated values don't survive cargo's argument splitting.
    #[cfg(windows)]
    {
        println!("cargo:rustc-link-arg=/STACK:4194304");
    }
}
