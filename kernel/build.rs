use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Boot assembly is assembled by rustc (global_asm in src/main.rs).
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
    println!("cargo:rerun-if-changed=src/exception.s");
    println!("cargo:rerun-if-changed=src/task.s");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=link-pi4.ld");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PI4");
    build_user(&out_dir, &manifest_dir);
}

fn build_user(out_dir: &Path, manifest_dir: &Path) {
    let user_dir = manifest_dir.join("../user-test");
    let src = user_dir.join("src/main.rs");
    let link = user_dir.join("link.ld");
    println!("cargo:rerun-if-changed={}", src.display());
    println!(
        "cargo:rerun-if-changed={}",
        user_dir.join("src/user.s").display()
    );
    println!("cargo:rerun-if-changed={}", link.display());

    let obj = out_dir.join("user.o");
    let elf = out_dir.join("user.elf");
    let bin = out_dir.join("user.bin");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let status = Command::new(&rustc)
        .args([
            "--target",
            "aarch64-unknown-none",
            "--edition",
            "2021",
            "-C",
            "panic=abort",
            "-C",
            "opt-level=z",
            "--emit=obj",
        ])
        .arg("-o")
        .arg(&obj)
        .arg(&src)
        .status()
        .unwrap_or_else(|err| panic!("failed to run rustc for user-test: {err}"));
    if !status.success() {
        panic!("user-test rustc failed");
    }
    let status = Command::new("aarch64-linux-gnu-ld")
        .arg("-T")
        .arg(&link)
        .arg("-o")
        .arg(&elf)
        .arg(&obj)
        .status()
        .unwrap_or_else(|err| panic!("failed to link user-test: {err}"));
    if !status.success() {
        panic!("user-test link failed");
    }
    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm user-test: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start") || line.ends_with(" T _start") || line.contains(" _start"))
        .unwrap_or_else(|| panic!("user-test has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("user-test _start is {addr}, expected 0x100000000");
    }
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy user-test: {err}"));
    if !status.success() {
        panic!("user-test objcopy failed");
    }
}
