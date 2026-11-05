use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Compile boot assembly
    cc::Build::new().file("src/boot.s").compile("boot");

    // Copy linker script to OUT_DIR so linker can find it
    let linker_script_src = manifest_dir.join("link.ld");
    let linker_script_dst = out_dir.join("link.ld");
    fs::copy(&linker_script_src, &linker_script_dst).expect("Failed to copy link.ld");

    // Tell cargo where to find the linker script
    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rerun-if-changed=src/boot.s");
    println!("cargo:rerun-if-changed=link.ld");
}
