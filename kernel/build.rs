use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Compile boot assembly
    cc::Build::new().file("src/boot.s").compile("boot");

    // The Pi 4 feature is linked at 0x80000. The QEMU image stays at 0x40080000.
    let script_name = if env::var("CARGO_FEATURE_PI4").is_ok() {
        "link-pi4.ld"
    } else {
        "link.ld"
    };
    let linker_script_src = manifest_dir.join(script_name);
    let linker_script_dst = out_dir.join("link.ld");
    fs::copy(&linker_script_src, &linker_script_dst)
        .unwrap_or_else(|err| panic!("Failed to copy {script_name}: {err}"));

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rerun-if-changed=src/boot.s");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=link-pi4.ld");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PI4");
}
