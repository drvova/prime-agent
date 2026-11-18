//! The `prime-agent` main thread's stack reserve on Windows.
//!
//! Windows sizes a process's main thread from the executable header: 1 MiB unless the linker
//! records more. Linux and macOS give the main thread 8 MiB. The interactive surface runs its
//! whole async state machine under `block_on` on the main thread, and a debug build's poll frames
//! along `run_interactive_mode` -> `run_interactive_route` -> `run_interactive_surface` need more
//! than 1 MiB before the first frame paints. So the debug binary overflowed at startup on Windows
//! (`thread 'main' has overflowed its stack`) and nowhere else. Recording the same 8 MiB reserve
//! gives every platform the main-thread budget the code is already exercised under. The reserve is
//! address space; pages are committed only as the stack grows.

/// The main-thread stack reserve, matching the Linux and macOS default.
const MAIN_STACK_BYTES: u32 = 8 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let link_arg = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        format!("/STACK:{MAIN_STACK_BYTES}")
    } else {
        // The GNU toolchains (`windows-gnu`, `windows-gnullvm`) drive the linker through the
        // compiler driver.
        format!("-Wl,--stack,{MAIN_STACK_BYTES}")
    };
    println!("cargo:rustc-link-arg-bin=prime-agent={link_arg}");
}
