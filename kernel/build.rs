use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Boot assembly is assembled by rustc (global_asm in src/main.rs).
    // The Pi 4 feature is linked at 0xFFFF000000080000. The firmware still
    // loads that raw image at physical 0x80000. The QEMU image is linked at
    // 0xFFFF000040080000 and loaded at physical 0x40080000.
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
    build_dns(&out_dir, &manifest_dir);
    build_http(&out_dir, &manifest_dir);
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

fn build_dns(out_dir: &Path, manifest_dir: &Path) {
    let dir = manifest_dir.join("../dns-el0");
    for rel in ["src/main.rs", "src/server.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../dns-protocol/src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../dns-cache/src").display()
    );

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let proto = out_dir.join("libdns_protocol.rlib");
    let cache = out_dir.join("libdns_cache.rlib");
    compile_rlib(
        &rustc,
        &manifest_dir.join("../dns-protocol/src/lib.rs"),
        "dns_protocol",
        &proto,
    );
    compile_rlib(
        &rustc,
        &manifest_dir.join("../dns-cache/src/lib.rs"),
        "dns_cache",
        &cache,
    );

    let elf = out_dir.join("dns.elf");
    let link = dir.join("link.ld");
    let link_arg = format!("link-arg=-T{}", link.display());
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
            "-C",
            "relocation-model=static",
            "-C",
            "linker=aarch64-linux-gnu-ld",
            "-C",
            &link_arg,
            "--extern",
        ])
        .arg(format!("dns_protocol={}", proto.display()))
        .arg("--extern")
        .arg(format!("dns_cache={}", cache.display()))
        .arg("-o")
        .arg(&elf)
        .arg(dir.join("src/main.rs"))
        .status()
        .unwrap_or_else(|err| panic!("failed to link dns-el0: {err}"));
    if !status.success() {
        panic!("dns-el0 link failed");
    }

    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm dns-el0: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("dns-el0 has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("dns-el0 _start is {addr}, expected 0x100000000");
    }

    let bin = out_dir.join("dns.bin");
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy dns-el0: {err}"));
    if !status.success() {
        panic!("dns-el0 objcopy failed");
    }

    let sections = elf_sections(&elf);
    let code_limit = 0x1_0008_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_000A_0000u64;
    let mut data_off = 0usize;
    let mut data_len = 0usize;
    let mut data_place = 0usize;
    for (name, addr, size) in &sections {
        if *size == 0 || *addr == 0 || name.starts_with(".comment") || name.starts_with(".ARM") || name.starts_with(".debug") {
            continue;
        }
        let end = addr + size;
        let in_code = *addr >= 0x1_0000_0000 && end <= code_limit;
        let in_data = *addr >= data_base && end <= data_limit;
        if !in_code && !in_data {
            panic!("dns-el0 section {name} at {addr:#x} size {size:#x} is outside the user window");
        }
        if name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    fs::write(
        out_dir.join("dns_meta.rs"),
        format!(
            "const DNS_DATA_OFF: usize = {data_off};\nconst DNS_DATA_LEN: usize = {data_len};\nconst DNS_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
}

fn build_http(out_dir: &Path, manifest_dir: &Path) {
    let dir = manifest_dir.join("../http-el0");
    for rel in ["src/main.rs", "src/server.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../http-server/src").display()
    );

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let http = out_dir.join("libhttp_server.rlib");
    compile_rlib(
        &rustc,
        &manifest_dir.join("../http-server/src/lib.rs"),
        "http_server",
        &http,
    );

    let elf = out_dir.join("http.elf");
    let link = dir.join("link.ld");
    let link_arg = format!("link-arg=-T{}", link.display());
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
            "-C",
            "relocation-model=static",
            "-C",
            "linker=aarch64-linux-gnu-ld",
            "-C",
            &link_arg,
            "--extern",
        ])
        .arg(format!("http_server={}", http.display()))
        .arg("-o")
        .arg(&elf)
        .arg(dir.join("src/main.rs"))
        .status()
        .unwrap_or_else(|err| panic!("failed to link http-el0: {err}"));
    if !status.success() {
        panic!("http-el0 link failed");
    }

    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm http-el0: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("http-el0 has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("http-el0 _start is {addr}, expected 0x100000000");
    }

    let bin = out_dir.join("http.bin");
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy http-el0: {err}"));
    if !status.success() {
        panic!("http-el0 objcopy failed");
    }

    let sections = elf_sections(&elf);
    let code_limit = 0x1_0008_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_000A_0000u64;
    let mut data_off = 0usize;
    let mut data_len = 0usize;
    let mut data_place = 0usize;
    for (name, addr, size) in &sections {
        if *size == 0 || *addr == 0 || name.starts_with(".comment") || name.starts_with(".ARM") || name.starts_with(".debug") {
            continue;
        }
        let end = addr + size;
        let in_code = *addr >= 0x1_0000_0000 && end <= code_limit;
        let in_data = *addr >= data_base && end <= data_limit;
        if !in_code && !in_data {
            panic!("http-el0 section {name} at {addr:#x} size {size:#x} is outside the user window");
        }
        if name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    fs::write(
        out_dir.join("http_meta.rs"),
        format!(
            "const HTTP_DATA_OFF: usize = {data_off};\nconst HTTP_DATA_LEN: usize = {data_len};\nconst HTTP_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
}

fn compile_rlib(rustc: &str, src: &Path, name: &str, out: &Path) {
    let status = Command::new(rustc)
        .args([
            "--target",
            "aarch64-unknown-none",
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            name,
            "-C",
            "panic=abort",
            "-C",
            "opt-level=z",
        ])
        .arg("-o")
        .arg(out)
        .arg(src)
        .status()
        .unwrap_or_else(|err| panic!("rustc {name}: {err}"));
    if !status.success() {
        panic!("{name} rlib failed");
    }
}

fn elf_sections(elf: &Path) -> Vec<(String, u64, u64)> {
    let output = Command::new("aarch64-linux-gnu-readelf")
        .args(["-S", "-W"])
        .arg(elf)
        .output()
        .unwrap_or_else(|err| panic!("readelf dns-el0: {err}"));
    let text = String::from_utf8_lossy(&output.stdout);
    let mut sections = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('[') || line.starts_with("[Nr]") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let _nr = parts.next();
        let name = parts.next().unwrap_or("").to_string();
        let kind = parts.next().unwrap_or("");
        if kind != "PROGBITS" && kind != "NOBITS" {
            continue;
        }
        let addr = u64::from_str_radix(parts.next().unwrap_or("0"), 16).unwrap_or(0);
        let _off = parts.next();
        let size = u64::from_str_radix(parts.next().unwrap_or("0"), 16).unwrap_or(0);
        sections.push((name, addr, size));
    }
    sections
}
