# Project Status

**Date**: November 26, 2026
**Version**: `0.1.95` (`version/src/lib.rs`), verified and promoted on Raspberry Pi 4 hardware
**Tree**: `origin/main` at `41f3344`
**Language**: Rust 2021, `no_std` kernel and EL0 tasks, host `std` tests
**Boards**: Pi 4 (BCM2711), Pi 5 (BCM2712, builds only), QEMU `virt`

This is a bare-metal DNS appliance. There is no Linux. The kernel owns the NIC, the SD card, the socket buffers, and the FAT volume. Isolated EL0 tasks do DNS, HTTP, logging, telnet, block-list download, VNC, and HDMI.

---

## Tree

Workspace members in `Cargo.toml` (15): `kernel`, `dns-protocol`, `http-server`, `config`, `dns-cache`, `rfb`, `edid`, `display-console`, `fault-log`, `fwupd`, `version`, `ntp`, `kalloc`, `blocklist`, `minfmt`.

The workspace `exclude` list is `user-test`, `dns-el0`, `http-el0`, `log-el0`, and `block-el0`. `kernel/build.rs` compiles those four plus `telnet-el0`, `vnc-el0`, and `hdmi-el0`. `user-common` is shared EL0 source. `user-test` is the boot-time EL0 probe. `vendor/embedded-sdmmc` is the FAT32 library.

Project Rust, excluding `vendor/` and `target/`, counted on this date: **105 files, 51,222 lines**. `git ls-files` lists **203** tracked files. Tracked `*.rs`, including `vendor/`, is **124 files, 58,458 lines**.

Build and run scripts in `scripts/`: `build.sh` (virt, `pi4`, `pi5`), `build-pi4.sh`, `build-pi5.sh`, `build-pi4-blinktest.sh`, `run-qemu.sh`, `test-integration.sh`, `fetch-pi4-boot.sh`, `install-qemu-10.sh` (QEMU 10.2.4), `dns_stress.py`.

`cargo test --lib` does not compile the kernel binary. Release images are `./scripts/build.sh`, `./scripts/build.sh pi4`, and `./scripts/build.sh pi5`.

| Image | Link (`_start`) | Load | Output |
| --- | --- | --- | --- |
| QEMU virt | `0xffff000040080000` | `0x40080000` | `boot/kernel8.img` |
| Pi 4 | `0xffff000000080000` | `0x80000` | `boot/pi4/kernel8.img` |
| Pi 5 | `0xffff000000200000` | `0x200000` | `boot/pi5/kernel_2712.img` |

Release image sizes from the 26 November 2026 verification build, `opt-level = "z"`: virt `boot/kernel8.img` 4,295,312 bytes, Pi 4 `boot/pi4/kernel8.img` 4,387,640 bytes, Pi 5 `boot/pi5/kernel_2712.img` 4,183,160 bytes.

Every EL0 `_start` is `0x100000000`. The kernel heap is 2 MiB. Pi 4 and Pi 5 reserve a 32 MiB NOLOAD `.block` region; the virt guest reserves 16 MiB.

---

## What is running

### Kernel and boards

Bare-metal AArch64. Four cores, GICv2, virtual timer at 100 Hz, TTBR1 kernel map, TTBR0 user window, PAN when the CPU has it. The supervisor tracks dns, http, the spinner, log, telnet, vnc, hdmi, and the blocker. A dead service is restarted. Five deaths in 60 seconds leave that service down.

| Board | Hardware path | Where it has run |
| --- | --- | --- |
| Pi 4 | PL011, EMMC2 SDHCI, BCM2711 GENET at `0xFD580000`, HDMI | Real Pi 4, and QEMU `raspi4b` for UART, MMU, four cores, and FAT. `raspi4b` has no GENET, so port 80 is not tested there |
| virt | virtio-net, virtio-blk | QEMU `virt` (`gic-version=2`). This is the integration target |
| Pi 5 | PL011, SDHCI | Builds cleanly. QEMU has no Pi 5 machine. RP1 Ethernet is not driven (`kernel/src/board/pi5/nic.rs` returns `None`). RP1 HDMI is not programmed |

### EL0 services

| Task | Slot | Role |
| --- | --- | --- |
| `dns-el0` | 0 | UDP and TCP DNS, local records, cache, upstream forward, block lookup |
| `http-el0` | 1 | Port 80, basic auth, dashboard, config, logs, firmware, debug |
| `log-el0` | 3 | UART, `LOG.TXT`, fan-out ring for telnet |
| `telnet-el0` | 4 | Port 23, up to 16 clients |
| `vnc-el0` | 5 | Optional RFB framebuffer |
| `hdmi-el0` | 6 | Optional HDMI status text |
| `block-el0` | 7 | HTTPS list download and table publish |

### Networking

smoltcp 0.11 (IPv4, TCP, UDP, DHCP). Pi 4 uses GENET. virt uses virtio-net. DHCP is the Pi 4 default image (`board-pi4,dhcp-default`); a timeout falls back to the static address. The `ntp` crate syncs from a configured server and answers a LAN query. The network owner sleeps when the NIC is idle and is woken by the NIC IRQ, a queued socket job, or the timer.

### Storage and firmware

SDHCI (Pi 4 EMMC2, high speed; Pi 5 SDHCI in the image) and virtio-blk. FAT32 via `embedded-sdmmc`.

`CONFIG.JSN` is the config file. A saved document is pretty-printed when the pretty form still fits in 4096 bytes, and compact when it does not. `/json/status` stays one compact JSON line. A leftover `CONFIG.TXT` is migrated once. `LOG.JSN` holds the log settings. `LOG.TXT` rotates to `LOG1.TXT` through `LOG16.TXT`.

Crash text is `CRASHxxx.TXT`. The crash slot is an 8 KiB NOLOAD page that survives a watchdog reset. Firmware update stages `kernel8.new`, reboots through `tryboot.txt`, promotes the staged kernel to the live file, and keeps `KERNEL8.BAK` for rollback. Three failed boots rewrite `tryboot.txt` to the backup.

### DNS and blocking

Local records, UDP and TCP forward, TTL cache. Block lists are Pi-hole style: up to 8 URLs, a 4 MiB cap on one download, and an allow list of 32 names. The live table is a double-buffered open-addressed hash of 64-bit name hashes in the `.block` region. Insert stops at half the slots. On the 32 MiB Pi 4 and Pi 5 region that cap is **1,048,573** names (about 1.05 million). The 16 MiB virt region caps at **524,285**. A lookup fails open while a half is unpublished or a rebuild is in progress.

On the Pi 4 running 0.1.95, measured DNS latency was **0.53–0.62 ms median** and **0.63–0.82 ms p95**.

### Web UI and diagnostics

HTTP basic auth. Compact `/json/status`. `/status` is HTML for a browser and JSON otherwise. The dashboard edits records and config. `/logfiles` lists `LOG.TXT` and the archives, shows the first 24 KiB, and downloads a file. It does not delete. Crash records can be viewed and cleared after a confirm. Debug crash and log-fill endpoints require a confirm. HDMI and VNC are output-only status displays. Telnet port 23 streams the live log. A client that falls behind loses lines and is told how many. That does not stall the log task or the other clients.

### Reliability through 0.1.95

- **0.1.93** shared `minfmt` and the EL0 link, entry, and form helpers.
- **0.1.94** puts a listen socket that is stuck in CLOSE-WAIT back into LISTEN, drops an idle HTTP read after 5 seconds, and restarts HTTP when no socket has been listening for 5 seconds. `/logfiles` landed in this release.
- **0.1.95** force-reaps a `Kind::Closing` descriptor after 5 seconds, fails an HTTP send that cannot progress for 5 seconds, retries `TCP_CLOSE` in `http-el0` when the job ring returns `EAGAIN`, and restarts HTTP when accept has been out of descriptors for 10 seconds. A successful accept, or a port-80 close that is reaped, clears that clock. The 10 second wait is longer than the 5 second close budget so the reap runs first.

---

## Tests

Host unit tests are `RUSTFLAGS="-D warnings" cargo test --lib`. On this tree that is **166 passed, 0 failed**, across the workspace libraries. The kernel binary is not one of them. `kernel/src/user_fd.rs` has a `close_reap` policy test that runs only if the kernel crate itself is tested, which `cargo test --lib` does not do.

Integration is `scripts/test-integration.sh`: **66 checks**, all passed on QEMU virt for 0.1.95, including the connect-storm check and the stalled-close check. The script also builds the Pi 4 image and checks `_start` `0xffff000000080000`, smoke-boots QEMU `raspi4b`, builds the Pi 5 image and checks `_start` `0xffff000000200000`, and boots the virt image on `cortex-a76`. Host forwards are DNS `15353`, HTTP `8080`, telnet `2323`, and VNC `15900`.

---

## Open

These are open and not started.

- **Warm-reboot HDMI (backlog 12).** A cold boot brings HDMI up. A software reboot or a tryboot reboot leaves `hdmi_state=none`.
- **`TCP_CLOSE` `EAGAIN` outside HTTP.** Telnet, VNC, DNS, and the block-list task still ignore that error. A `Kind::Closing` socket on those tasks is still reaped after 5 seconds. A full 64-entry job ring can put the descriptor back as a live connection, and recycle does not reap a live connection.
- **TCP buffer pool.** `TCP_POOL` is 64 sockets. Each socket's buffers are 8 KiB receive and 8 KiB transmit (16 KiB). Teardown parks those buffers instead of freeing them, up to the cap, then `sockets.remove` drops further ones. Used heap steps up across a burst and then flattens.
- **Block-list product work.** A DNS blocking config and status page, Pi-hole URL lists with notes and JSON save/restore, and a table large enough for 17 million already-deduplicated names. That needs a denser slot than 8 bytes and a `.block` region larger than 32 MiB.
- **Consolidation still open.** One shared syscall table, one shared EL0 freelist allocator, an integration-test helper, and one config schema. Each EL0 program still has its own syscall numbers and allocator, and `scripts/test-integration.sh` is a single script.
- **Pi 5.** The image links at `0xffff000000200000` and builds with warnings denied. It has not been run on Pi 5 hardware. RP1 Ethernet and RP1 HDMI are not implemented. Real hardware results so far are Pi 4 and QEMU virt.

---

## How to check this tree

```bash
RUSTFLAGS="-D warnings" cargo test --lib
./scripts/test-integration.sh
./scripts/build.sh
./scripts/build.sh pi4
./scripts/build.sh pi5
```

Release builds need the kernel link flags as well as `-D warnings`, because `RUSTFLAGS` replaces `.cargo/config.toml`:

```bash
RUSTFLAGS="-D warnings -C link-arg=-Tlink.ld -C target-cpu=cortex-a72"
```

`scripts/build.sh` reads that environment. The three board images share one `kernel8` ELF in `target/`, so a Pi 4 or Pi 5 build replaces the virt ELF until the next virt build.
