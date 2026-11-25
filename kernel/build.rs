use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Boot assembly is assembled by rustc (global_asm in src/main.rs).
    // The Pi 4 image is linked at 0xFFFF000000080000. The firmware still
    // loads that raw image at physical 0x80000. The Pi 5 image is linked at
    // 0xFFFF000000200000 and the firmware loads kernel_2712.img at physical
    // 0x200000. The QEMU virt image is linked at 0xFFFF000040080000 and
    // loaded at physical 0x40080000.
    let script_rel = if env::var("CARGO_FEATURE_BOARD_PI5").is_ok() {
        "src/board/pi5/link.ld"
    } else if env::var("CARGO_FEATURE_BOARD_PI4").is_ok() {
        "src/board/pi4/link.ld"
    } else if env::var("CARGO_FEATURE_BOARD_VIRT").is_ok() {
        "src/board/virt/link.ld"
    } else {
        panic!("select board-virt, board-pi4, or board-pi5 so a linker script is copied");
    };
    let linker_script_src = manifest_dir.join(script_rel);
    let linker_script_dst = out_dir.join("link.ld");
    fs::copy(&linker_script_src, &linker_script_dst)
        .unwrap_or_else(|err| panic!("Failed to copy {script_rel}: {err}"));

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rerun-if-changed=src/boot.s");
    println!("cargo:rerun-if-changed=src/exception.s");
    println!("cargo:rerun-if-changed=src/task.s");
    println!("cargo:rerun-if-changed=src/board/virt/link.ld");
    println!("cargo:rerun-if-changed=src/board/pi4/link.ld");
    println!("cargo:rerun-if-changed=src/board/pi5/link.ld");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_VIRT");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_PI4");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_PI5");
    build_user(&out_dir, &manifest_dir);
    build_dns(&out_dir, &manifest_dir);
    build_http(&out_dir, &manifest_dir);
    build_block(&out_dir, &manifest_dir);
    build_log(&out_dir, &manifest_dir);
    build_telnet(&out_dir, &manifest_dir);
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rfb = out_dir.join("librfb.rlib");
    let edid = out_dir.join("libedid.rlib");
    let console = out_dir.join("libdisplay_console.rlib");
    compile_rlib(&rustc, &manifest_dir.join("../rfb/src/lib.rs"), "rfb", &rfb);
    compile_rlib(&rustc, &manifest_dir.join("../edid/src/lib.rs"), "edid", &edid);
    compile_rlib(
        &rustc,
        &manifest_dir.join("../display-console/src/lib.rs"),
        "display_console",
        &console,
    );
    build_vnc(&out_dir, &manifest_dir, &rustc, &rfb, &console);
    if env::var("CARGO_FEATURE_BOARD_PI4").is_ok() || env::var("CARGO_FEATURE_BOARD_PI5").is_ok() {
        build_hdmi(&out_dir, &manifest_dir, &rustc, &edid, &console);
    }
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
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../user-common/thread.rs").display()
    );

    let elf = out_dir.join("user.elf");
    let bin = out_dir.join("user.bin");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, &rustc);
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
        .arg(format!("panic_rt={}", panic_rt.display()))
        .arg("-o")
        .arg(&elf)
        .arg(&src)
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
    for rel in [
        "src/main.rs",
        "src/server.rs",
        "src/failover.rs",
        "src/health.rs",
        "src/start.s",
        "link.ld",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../user-common/thread.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../dns-protocol/src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../dns-cache/src").display()
    );

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, &rustc);
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
        .arg("--extern")
        .arg(format!("panic_rt={}", panic_rt.display()))
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
    for rel in ["src/main.rs", "src/server.rs", "src/firmware.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../user-common/thread.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../http-server/src").display()
    );

    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../version/src/lib.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../config/src/lib.rs").display()
    );
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, &rustc);
    let version = out_dir.join("libversion.rlib");
    compile_rlib(
        &rustc,
        &manifest_dir.join("../version/src/lib.rs"),
        "version",
        &version,
    );
    let http = out_dir.join("libhttp_server.rlib");
    compile_rlib(
        &rustc,
        &manifest_dir.join("../http-server/src/lib.rs"),
        "http_server",
        &http,
    );
    let config_rlib = out_dir.join("libconfig_http.rlib");
    compile_rlib(
        &rustc,
        &manifest_dir.join("../config/src/lib.rs"),
        "config",
        &config_rlib,
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
        .arg("--extern")
        .arg(format!("panic_rt={}", panic_rt.display()))
        .arg("--extern")
        .arg(format!("version={}", version.display()))
        .arg("--extern")
        .arg(format!("config={}", config_rlib.display()))
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

fn build_log(out_dir: &Path, manifest_dir: &Path) {
    let dir = manifest_dir.join("../log-el0");
    for rel in ["src/main.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, &rustc);
    let elf = out_dir.join("log.elf");
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
        .arg(format!("panic_rt={}", panic_rt.display()))
        .arg("-o")
        .arg(&elf)
        .arg(dir.join("src/main.rs"))
        .status()
        .unwrap_or_else(|err| panic!("failed to link log-el0: {err}"));
    if !status.success() {
        panic!("log-el0 link failed");
    }
    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm log-el0: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("log-el0 has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("log-el0 _start is {addr}, expected 0x100000000");
    }
    let bin = out_dir.join("log.bin");
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy log-el0: {err}"));
    if !status.success() {
        panic!("log-el0 objcopy failed");
    }
    let sections = elf_sections(&elf);
    let code_limit = 0x1_0002_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_0009_1000u64;
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
            panic!("log-el0 section {name} at {addr:#x} size {size:#x} is outside the user window");
        }
        if name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    fs::write(
        out_dir.join("log_meta.rs"),
        format!(
            "const LOG_DATA_OFF: usize = {data_off};\nconst LOG_DATA_LEN: usize = {data_len};\nconst LOG_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
}

fn build_telnet(out_dir: &Path, manifest_dir: &Path) {
    let dir = manifest_dir.join("../telnet-el0");
    for rel in ["src/main.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../user-common/thread.rs").display()
    );
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, &rustc);
    let elf = out_dir.join("telnet.elf");
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
        .arg(format!("panic_rt={}", panic_rt.display()))
        .arg("-o")
        .arg(&elf)
        .arg(dir.join("src/main.rs"))
        .status()
        .unwrap_or_else(|err| panic!("failed to link telnet-el0: {err}"));
    if !status.success() {
        panic!("telnet-el0 link failed");
    }
    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm telnet-el0: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("telnet-el0 has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("telnet-el0 _start is {addr}, expected 0x100000000");
    }
    let bin = out_dir.join("telnet.bin");
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy telnet-el0: {err}"));
    if !status.success() {
        panic!("telnet-el0 objcopy failed");
    }
    let sections = elf_sections(&elf);
    let code_limit = 0x1_0002_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_0009_1000u64;
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
            panic!("telnet-el0 section {name} at {addr:#x} size {size:#x} is outside the user window");
        }
        if name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    fs::write(
        out_dir.join("telnet_meta.rs"),
        format!(
            "const TELNET_DATA_OFF: usize = {data_off};\nconst TELNET_DATA_LEN: usize = {data_len};\nconst TELNET_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
}

fn build_vnc(out_dir: &Path, manifest_dir: &Path, rustc: &str, rfb: &Path, console: &Path) {
    let dir = manifest_dir.join("../vnc-el0");
    for rel in ["src/main.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!("cargo:rerun-if-changed={}", manifest_dir.join("../rfb/src/lib.rs").display());
    println!("cargo:rerun-if-changed={}", manifest_dir.join("../rfb/src/des.rs").display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../display-console/src/lib.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../display-console/src/font.rs").display()
    );
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, rustc);
    link_el0(
        out_dir,
        rustc,
        &dir.join("src/main.rs"),
        &dir.join("link.ld"),
        "vnc",
        &[
            format!("rfb={}", rfb.display()),
            format!("display_console={}", console.display()),
            format!("panic_rt={}", panic_rt.display()),
        ],
    );
}

fn build_hdmi(out_dir: &Path, manifest_dir: &Path, rustc: &str, edid: &Path, console: &Path) {
    let dir = if env::var("CARGO_FEATURE_BOARD_PI5").is_ok() {
        manifest_dir.join("../hdmi-el0/pi5")
    } else {
        manifest_dir.join("../hdmi-el0/pi4")
    };
    for rel in ["src/main.rs", "src/start.s", "link.ld"] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../hdmi-el0/runner.rs").display()
    );
    println!("cargo:rerun-if-changed={}", manifest_dir.join("../edid/src/lib.rs").display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../display-console/src/lib.rs").display()
    );
    let panic_rt = compile_panic_rt(out_dir, manifest_dir, rustc);
    link_el0(
        out_dir,
        rustc,
        &dir.join("src/main.rs"),
        &dir.join("link.ld"),
        "hdmi",
        &[
            format!("edid={}", edid.display()),
            format!("display_console={}", console.display()),
            format!("panic_rt={}", panic_rt.display()),
        ],
    );
}

fn link_el0(out_dir: &Path, rustc: &str, src: &Path, link: &Path, name: &str, externs: &[String]) {
    let elf = out_dir.join(format!("{name}.elf"));
    let link_arg = format!("link-arg=-T{}", link.display());
    let mut cmd = Command::new(rustc);
    cmd.args([
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
    ]);
    for item in externs {
        cmd.arg("--extern").arg(item);
    }
    let status = cmd
        .arg("-o")
        .arg(&elf)
        .arg(src)
        .status()
        .unwrap_or_else(|err| panic!("failed to link {name}: {err}"));
    if !status.success() {
        panic!("{name} link failed");
    }
    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm {name}: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("{name} has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("{name} _start is {addr}, expected 0x100000000");
    }
    let bin = out_dir.join(format!("{name}.bin"));
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy {name}: {err}"));
    if !status.success() {
        panic!("{name} objcopy failed");
    }
    let sections = elf_sections(&elf);
    let code_limit = 0x1_0002_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_0009_1000u64;
    let mut data_off = 0usize;
    let mut data_len = 0usize;
    let mut data_place = 0usize;
    for (sec_name, addr, size) in &sections {
        if *size == 0
            || *addr == 0
            || sec_name.starts_with(".comment")
            || sec_name.starts_with(".ARM")
            || sec_name.starts_with(".debug")
        {
            continue;
        }
        let end = addr + size;
        let in_code = *addr >= 0x1_0000_0000 && end <= code_limit;
        let in_data = *addr >= data_base && end <= data_limit;
        if !in_code && !in_data {
            panic!("{name} section {sec_name} at {addr:#x} size {size:#x} is outside the user window");
        }
        if sec_name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    let prefix = name.to_uppercase();
    fs::write(
        out_dir.join(format!("{name}_meta.rs")),
        format!(
            "const {prefix}_DATA_OFF: usize = {data_off};\nconst {prefix}_DATA_LEN: usize = {data_len};\nconst {prefix}_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
}

fn compile_minfmt(out_dir: &Path, manifest_dir: &Path, rustc: &str) -> PathBuf {
    let src = manifest_dir.join("../minfmt/src/lib.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    let out = out_dir.join("libminfmt.rlib");
    compile_rlib(rustc, &src, "minfmt", &out);
    out
}

fn compile_panic_rt(out_dir: &Path, manifest_dir: &Path, rustc: &str) -> PathBuf {
    let src = manifest_dir.join("../user-common/panic_rt.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../user-common/panic_line.rs").display()
    );
    let minfmt = compile_minfmt(out_dir, manifest_dir, rustc);
    let out = out_dir.join("libpanic_rt.rlib");
    let status = Command::new(rustc)
        .args([
            "--target",
            "aarch64-unknown-none",
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            "panic_rt",
            "-C",
            "panic=abort",
            "-C",
            "opt-level=z",
            "--extern",
        ])
        .arg(format!("minfmt={}", minfmt.display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|err| panic!("rustc panic_rt: {err}"));
    if !status.success() {
        panic!("panic_rt rlib failed");
    }
    out
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

fn build_block(out_dir: &Path, manifest_dir: &Path) {
    let dir = manifest_dir.join("../block-el0");
    for rel in [
        "src/main.rs",
        "src/fetch.rs",
        "src/start.s",
        "link.ld",
        "Cargo.toml",
        "roots/isrgx1.der",
        "roots/isrgx2.der",
        "roots/digicertg2.der",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(rel).display());
    }
    let link = dir.join("link.ld");
    let target_dir = out_dir.join("block-target");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("aarch64-unknown-none")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            format!(
                "-D\u{1f}warnings\u{1f}-C\u{1f}panic=abort\u{1f}-C\u{1f}opt-level=z\u{1f}-C\u{1f}relocation-model=static\u{1f}-C\u{1f}linker=aarch64-linux-gnu-ld\u{1f}-C\u{1f}link-arg=-T{}",
                link.display()
            ),
        )
        .current_dir(&dir)
        .status()
        .unwrap_or_else(|err| panic!("block-el0 cargo: {err}"));
    if !status.success() {
        panic!("block-el0 build failed");
    }
    let elf = target_dir.join("aarch64-unknown-none/release/block-el0");
    let nm = Command::new("aarch64-linux-gnu-nm")
        .arg(&elf)
        .output()
        .unwrap_or_else(|err| panic!("nm block-el0: {err}"));
    let listing = String::from_utf8_lossy(&nm.stdout);
    let start = listing
        .lines()
        .find(|line| line.ends_with(" _start"))
        .unwrap_or_else(|| panic!("block-el0 has no _start:\n{listing}"));
    let addr = start.split_whitespace().next().unwrap_or("");
    if addr.trim_start_matches('0') != "100000000" && addr != "0000000100000000" {
        panic!("block-el0 _start is {addr}, expected 0x100000000");
    }
    let bin = out_dir.join("block.bin");
    let status = Command::new("aarch64-linux-gnu-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|err| panic!("objcopy block-el0: {err}"));
    if !status.success() {
        panic!("block-el0 objcopy failed");
    }
    let sections = elf_sections(&elf);
    let code_limit = 0x1_0008_0000u64;
    let data_base = 0x1_0009_0000u64;
    let data_limit = 0x1_000A_0000u64;
    let mut data_off = 0usize;
    let mut data_len = 0usize;
    let mut data_place = 0usize;
    for (name, addr, size) in &sections {
        if *size == 0
            || *addr == 0
            || name.starts_with(".comment")
            || name.starts_with(".ARM")
            || name.starts_with(".debug")
        {
            continue;
        }
        let end = addr + size;
        let in_code = *addr >= 0x1_0000_0000 && end <= code_limit;
        let in_data = *addr >= data_base && end <= data_limit;
        if !in_code && !in_data {
            panic!(
                "block-el0 section {name} at {addr:#x} size {size:#x} is outside the user window"
            );
        }
        if name == ".data" {
            data_off = (*addr - 0x1_0000_0000) as usize;
            data_len = *size as usize;
            data_place = (*addr - data_base) as usize;
        }
    }
    fs::write(
        out_dir.join("block_meta.rs"),
        format!(
            "const BLOCK_DATA_OFF: usize = {data_off};\nconst BLOCK_DATA_LEN: usize = {data_len};\nconst BLOCK_DATA_PLACE: usize = {data_place};\n"
        ),
    )
    .unwrap();
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
        let first = parts.next().unwrap_or("");
        if first == "[" {
            let _nr = parts.next();
        }
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
