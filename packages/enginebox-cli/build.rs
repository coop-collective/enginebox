fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Clap builds the complete nested command graph before selecting a command.
    // Its debug-build temporaries exceed Windows' default 1 MiB main stack as
    // optional modules grow. Reserve 8 MiB (committed on demand), like the Unix
    // CLI stack, instead of letting unrelated commands abort during parsing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bin=ebox=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bin=ebox=-Wl,--stack,8388608");
        }
    }
}
