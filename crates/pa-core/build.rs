//! Points the linker at the frida-core devkit when the `frida` feature is on.
//!
//! `frida-sys` asks for `-lfrida-core` but only looks in its own source
//! directory. The devkit (a prebuilt static library and its header, one per
//! target) is unpacked by `scripts/frida-devkit.sh` (`make frida-devkit`) and
//! named by `FRIDA_DEVKIT_DIR`; `frida-sys` finds the header through
//! `BINDGEN_EXTRA_CLANG_ARGS=-I$FRIDA_DEVKIT_DIR`. Without the feature this
//! script does nothing.

fn main() {
    println!("cargo:rerun-if-env-changed=FRIDA_DEVKIT_DIR");
    if std::env::var_os("CARGO_FEATURE_FRIDA").is_none() {
        return;
    }
    let devkit = std::env::var_os("FRIDA_DEVKIT_DIR")
        .map(std::path::PathBuf::from)
        .filter(|devkit| devkit.join("frida-core.h").is_file())
        .unwrap_or_else(|| {
            panic!(
                "the `frida` feature needs the frida-core devkit: run `make frida-devkit` and \
                 build with FRIDA_DEVKIT_DIR set to the directory it prints (it must hold \
                 frida-core.h and the frida-core static library)"
            )
        });
    println!("cargo:rustc-link-search=native={}", devkit.display());
}
