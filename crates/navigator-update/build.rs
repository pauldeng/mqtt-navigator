//! Gives the library the exact Rust target triple of this build (`env!("TARGET")`). The update
//! manifest has one entry for each triple, so a table of `cfg` values would not tell gnu from
//! musl or x86_64 from arm64 on macOS cross builds.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=TARGET={target}");
}
