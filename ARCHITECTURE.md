# Architecture

This is a bare-metal DNS appliance. The kernel is `no_std` Rust. It does not run Linux. One Cargo feature selects the board, and the rest of the tree calls `crate::board` instead of naming that feature.

| Image | Feature | Load address | Link address | Output | NIC | Block |
| --- | --- | --- | --- | --- | --- | --- |
| QEMU virt | `board-virt` (default) | `0x40080000` | `0xFFFF000040080000` | `boot/kernel8.img` | virtio-net | virtio-blk |
| Raspberry Pi 4 | `board-pi4` | `0x80000` | `0xFFFF000000080000` | `boot/pi4/kernel8.img` | GENET | EMMC2, then the legacy Arasan |
| Raspberry Pi 5 | `board-pi5` | `0x200000` | `0xFFFF000000200000` | `boot/pi5/kernel_2712.img` | not driven (RP1 is behind PCIe) | `sdio1` |

`./scripts/build.sh` builds virt. `./scripts/build.sh pi4` and `./scripts/build-pi4.sh` pass `--no-default-features --features board-pi4,dhcp-default`. `./scripts/build.sh pi5` and `./scripts/build-pi5.sh` pass `--no-default-features --features board-pi5`. The target is `aarch64-unknown-none`. `kernel/build.rs` copies the selected link script and links the EL0 programs.

The Pi 4 image is the one that runs on the board: PL011, EMMC, GENET, the firmware framebuffer, and the network updater. QEMU `raspi4b` (9.0 or newer; the install script is QEMU 10) boots that image far enough to check the UART, the upper-half map, four cores, FAT, and the log task. That machine has no GENET, so DNS, HTTP, and telnet stay down there. The Pi 5 image links and is not booted. QEMU has no Pi 5 machine. The integration stage then boots the virt image with `-cpu cortex-a76`.

## Boot

`kernel/src/boot.s` runs on core 0. Cores that enter `_start` instead of the secondary entry park. The sequence drops EL2 to EL1 when the CPU entered at EL2, sets the stack at the load address, zeroes BSS, builds a 1 GiB block map, turns the MMU on, jumps to the high link address, and clears TTBR0. Rust then installs the strict TTBR1 map. Kernel text is read-only and executable, rodata is read-only, data is not executable, `.dma` is Normal non-cacheable, devices are Device-nGnRE, and one guard page under the stack is unmapped. Devices are programmed with physical addresses. The CPU uses the high alias. Bits 47:0 of a kernel virtual address are the physical address.

`kernel_main` in `kernel/src/main.rs`:

1. On Pi 4, `fb_con::start_early` reads the firmware framebuffer before the UART and before the strict map. A hang in either of those used to leave the rainbow up. This step does not write the HDMI block. See HDMI below.
2. Board early init, then the strict MMU. On Pi 4 the framebuffer is switched to a non-cacheable window and the firmware message log is copied.
3. Bump allocator (2 MiB, never frees), logger, ARM virtual timer, GIC.
4. Secondary cores. Virt and Pi 5 use PSCI `CPU_ON` (`0xC4000003`). Virt's conduit is HVC. With `-M virt,virtualization=on` that HVC stops at EL2 and the extra cores stay offline. The default virt machine has no EL2, so `-smp 4` brings all four up. Pi 5 uses SMC and affinity `cpu << 8`. Pi 4 does not use PSCI. It writes the spin-table release words at `0xE0`, `0xE8`, and `0xF0` (core n at `0xD8 + 8*n`), cleans the line, and sends an event. If the firmware stub is not parked on those words, the stores hit RAM and the extra cores stay offline.
5. A short EL0 boot test, then the FAT volume, Pi 4 `HDMISHUT.TXT`, firmware scrub and boot report, and `CONFIG.JSN`.
6. The NIC. A missing device skips DNS, HTTP, and telnet. The log task still starts. That is the raspi4b case and the Pi 5 image.
7. Supervisor slots: log, then DNS, HTTP, and telnet when the NIC is up, VNC when `display.vnc` is 1, and the EL0 HDMI process when `display.hdmi` is 1 and the board has a controller.
8. The poll loop drives the NIC, the user tasks, the power command, and the firmware pump. On Pi 4 a stall checkpoint halts, and does not reboot, if the network owner stays inside one poll.

Each core has its own stack, its own virtual timer, and its own GIC CPU interface. `TPIDR_EL1` holds that core's current thread. The published tick count is core 0's 100 Hz counter. The other cores preempt and do not add to it. Uptime is `CNTVCT` since boot, because QEMU does not clear the counter on reset.

### Pi 5 map

`boot.s` widens `TCR_EL1.IPS` to 40-bit physical addresses so the early map can cover the SoC window at `0x10_0000_0000`. That window is two 1 GiB device blocks in the early tables and 2 MiB blocks in the strict map. RAM above 4 GiB is not mapped. The debug UART is `uart10` at `0x107d001000`. The GIC-400 distributor is `0x107fff9000` and the CPU interface is `0x107fffa000`. The SD host is `sdio1` at `0x1000fff000`. The first register read is fault-catching, so a missing window fails the probe instead of halting. Card power and card detect are not programmed. RP1 Ethernet is a Cadence GEM behind `pcie2`. Link training, ECAM, the RP1 BAR, and a MAC driver are not in this image.

## EL0

TTBR0 holds only the user window, in its own page tables:

- code `0x1_0000_0000`, 512 KiB, EL0 read/execute
- a gap, then data `0x1_0009_0000`, 64 KiB
- heap `0x1_000A_0000`, 1536 KiB
- stack `0x1_0022_0000`, 64 KiB

User pages are non-global. Threads of one process share that process's TTBR0 root and ASID. A switch between those threads does not flush the TLB. A map change uses `tlbi vmalle1is` and `dsb ish`. EL0 cannot walk TTBR1, so a user access to kernel memory faults and kills that task. PAN is enabled when the CPU implements it.

Supervised slots:

| Slot | Process | Preferred ASID | Started |
| --- | --- | --- | --- |
| 0 | DNS | 1 | with a NIC |
| 1 | HTTP | 2 | with a NIC |
| 2 | spinner | 3 | from the dashboard |
| 3 | log | 4 | always |
| 4 | telnet | 5 | with a NIC |
| 5 | VNC | 6 | `display.vnc` is 1 |
| 6 | HDMI | 7 | `display.hdmi` is 1 and the board has HDMI |

The boot self-test runs on slot 0 with ASID 1, then DNS reuses that id. The kernel keeps the NIC, the smoltcp sockets, storage, and the config. A task yields when a socket would block. A timer IRQ at EL0 returns to the poll loop after two 10 ms ticks. The supervisor restarts dns, http, log, telnet, and an armed spinner. The delay starts at 100 ms and doubles up to 10 seconds, and it resets after 60 healthy seconds. Five deaths in 60 seconds leave that service down. Config is not touched. `GET /debug` shows each service, free user pages, held ASIDs, and open fds.

Syscall numbers are in `x8`, arguments in `x0`–`x5`, and the result in `x0`. A pointer outside the user window returns `-2` and is not touched. A store into user code is the same. `-1` is an unknown number, `-3` a bad fd, `-4` a bad argument, `-5` I/O, `-6` too many fds or threads, `-7` text that does not fit, and `-11` means try again. Packet and config buffers are at most 4096 bytes. Each task has 24 fds.

| x8 | Call |
| --- | --- |
| 0 | `log(ptr, len)` |
| 1 | `yield` |
| 2 | `uptime_ticks` |
| 3 | `exit(code)` |
| 4 | `time_ms` |
| 5 | `udp_bind(port)` |
| 6 | `udp_recv(fd, buf, cap, addr)` |
| 7 | `udp_send(fd, buf, len, ip, port)` |
| 8 | `tcp_listen(port)` |
| 9 | `tcp_accept(fd)` |
| 10 | `tcp_recv(fd, buf, cap, flags)`. `flags` 1 returns `-11` instead of sleeping |
| 11 | `tcp_send(fd, buf, len)` |
| 12 | `tcp_close(fd)` |
| 13–16 | config records, upstreams, blocklist, cache size |
| 17 | `set_cache_entries(n)` |
| 18 | `auth` |
| 19–20 | `config_add`, `config_remove` |
| 21–22 | `uptime_secs`, `cache_entries` |
| 23–24 | `fault_guard`, `fault_text`. Do not return |
| 25–27 | `http_info`, `tick_hz`, `debug_spin` |
| 28–33 | log post, drain, emit, recent, file read, file write |
| 34–38 | `thread_create`, `thread_exit`, `thread_join`, `futex_wait`, `futex_wake` |
| 39–44 | log publish and follow, supervisor status, crash, `thread_try_join` |
| 45–50 | log reload, config read and write, log fill, cache set, cache reap |
| 51–54 | timer ticks and frequency, cache stats, cache plant |
| 55 | `display_io`. HDMI mailbox and framebuffer, plus VNC settings |
| 56 | `display_set`. Writes `display` in `CONFIG.JSN` and queues the supervisor |
| 57 | `set_dns_top` |
| 58 | `power`. 0 queries, 1 shutdown, 2 reboot, 3 tryboot |
| 59 | `vnc_ctrl` |
| 60–63 | upstream health publish and read, probe interval, `net_ready` |
| 64 | `net_counters`. Five `u64`s: IRQs, RX, TX, busy ms, idle ms |
| 65 | `set_dns_load` |
| 66 | `net_extra`. ARP, DNS load, RX faults, poll time, IRQ rearms, TX stalls |
| 67 | `firmware`. Staged SD updates. Op 10 promotes, op 11 rolls back |
| 68 | `sd_status` |
| 69 | `hdmi_console`. 836 bytes: state, size, recoveries, diag, last shut, regs, port, vclog line, `vclog_bytes`, `vclog_status` |
| 70 | `vclog_read`. A window of the rendered firmware log |

`log_publish` is the log task only. It copies one line into a kernel ring and returns. `thread_create` checks that `entry` is a 4-byte-aligned address in the user code window and that the top 256 bytes of `stack` are writable and 16-byte aligned.

Lock order, outermost first. A holder may take a later lock and must not take an earlier one.

1. `SCHED`. Run queue, thread state, futex waiters, join waiters, and socket sleepers. IRQ-safe. The timer IRQ does not take it.
2. `FD`. One lock per EL0 task. A socket syscall drops it before sleeping.
3. `CONFIG`.
4. `FS`. One FAT lock. The library is not reentrant and the block device has one command slot.
5. `LOGQ`.

The telnet fan-out is not a lock. The supervisor lock is taken alone and dropped before any lock in this list. smoltcp and the NIC are not behind a lock. One core owns them. No IRQ handler takes `SCHED`, `FS`, or the disk mutex.

## Pi 4 firmware handoff

The card needs the March 2025 matched set, the same bytes as the Raspberry Pi OS card that boots this board. `scripts/fetch-pi4-boot.sh` downloads them from `raspberrypi/firmware` commit `80f23d4be5` (19 Mar 2025):

- `start4.elf`, 2263840 bytes, `VC_BUILD_ID` `ca6e8171`
- `fixup4.dat`, 5459 bytes
- `bcm2711-rpi-4-b.dtb`, 56211 bytes

A newer firmware master is a different matched pair and is not what this board uses. Copy those three files from a working card when the EEPROM already matches that card. Do not copy that card's `config.txt`, `cmdline.txt`, `kernel8.img`, or `overlays/`.

`boot/pi4/config.txt` is the whole handoff:

```
arm_64bit=1
kernel=kernel8.img
kernel_address=0x80000
enable_uart=1
```

`hdmi_force_hotplug` is not in that file, the blink-test handoff, or the tryboot preset. On 0.1.67 that line reported 640×480 and left the rainbow up. `os_check`, `uart_2ndstage`, `core_freq`, `init_uart_clock`, `arm_boost`, and `dtoverlay` stay out. The network updater does not rewrite the live `config.txt`. A mode is not hard-coded.

The firmware probes HDMI and paints the rainbow before this kernel runs. The kernel keeps that scanout.

## HDMI and display

Two different paths exist. The boot console is the one that must not disturb a picture the firmware already brought up. The optional EL0 HDMI process is off unless `display.hdmi` is 1.

### Boot console

`kernel/src/fb_con.rs` and `kernel/src/hdmi_hw.rs` are Pi 4 only. Virt and Pi 5 return zeros and the text `none`.

A cold boot only reads. `bring_up` records registers, then asks the mailbox for the size the firmware already published. A size in range is kept and painted. A miss retries the query twice, 50 ms apart, and then stops. The boot path does not blank, program a mode, release the framebuffer, or write a PHY, a DVP line, or a power domain. Recoveries stay at 0 when the firmware picture is kept.

The sample reads both hot-plug registers, both PHYs, both pixel valves, the HVS, `CM_HSMCTL` (`0xFE101088`), `PM_HDMI` (`0xFE100058`), and DVP `+4` and `+8`. It does not skip those reads when `CM_HSMCTL` bit 4 is clear. A live picture was measured with that bit clear (`hsm=0000636d`) and HVS still on; skipping the reads left the port and valve words at zero. A set bit in `DVP_HT_RPI_MISC_CONFIG` (`0xFEF00008`) means that port's 108 MHz clock is stopped. `dvp8=0` means both gates are open, not that the clock is off. A faulting read returns `ffffffff` and is not followed by a write.

Port detection is hot-plug bit 0, else pixel-valve enable (PV2 is HDMI0, PV4 is HDMI1). The status names are `none`, `hdmi0`, `hdmi1`, and `hdmi0+hdmi1`. A mask of zero is not treated as both ports.

### Reboot

The default soft reboot matches the Linux Pi watchdog. `prepare_reboot` stores a read-only register line, sends mailbox `NOTIFY_REBOOT` (`0x00030048`, empty payload), and then `watchdog_power` writes:

- `PM_RSTS = (PM_RSTS & 0xfffffaaa) | 0x5a000000`
- `PM_WDOG = 0x5a000000 | 10`
- `PM_RSTC = 0x5a000000 | (PM_RSTC & 0xffffffcf) | 0x20`

There is no HDMI, PHY, DVP, blank, release, or power-domain step on that path. Tryboot sets `SET_REBOOT_FLAGS` (`0x00038064`, bit 0) first. A refused flag does not reset. The next boot samples that bit and `PM_RSTS` partition bits `0x555`, then clears both, before it reads the firmware log. Pi 4 rev 1.0 and 1.1 store the flag in EEPROM, so a crash that never clears it keeps loading `tryboot.txt` after a power cycle. `fw_tryboot` is 1 only for a boot that was a tryboot. `fw_tryboot_hw` is the sampled mailbox bit, so a normal boot is 0 while `TRYBOOT.TXT` remains. If the clear is refused, `TRYBOOT.TXT` is rewritten in place so it boots `kernel8.img`. A stuck flag with no `tryboot.txt` leaves the firmware on the rainbow. A FAT32 directory walk stops at the end marker and after 64 clusters, so a cyclic chain is an error instead of a hang. Halt ORs `0x555` into the masked `PM_RSTS` and does not call `prepare_reboot`, so a halt leaves the panel text up. Panic and exception halt do not reboot and do not release HDMI. The watchdog reason bits in `PM_RSTS` are set by the reset itself, so the next boot does not look like power-on to `start4.elf`.

`display.hdmi_reset` set to 1 is the only way to get the extra shutdown: per-port PHY and DVP reset, blank, release, then `SET_DOMAIN_STATE` on for firmware domain 6 (HDMI) and domain 4 (HVS). It does not run when no port was detected, so a dark hot-plug sample cannot pulse both ports. The key is optional. The saved `CONFIG.JSN` omits it when it is 0. Domain 6 is shared by both HDMI ports. The old `SET_POWER_STATE` id 5 is I2C1 and is not used. `PM_HDMI` is a diagnostic, not a power switch.

### Diagnostics

Syscall 69 and `GET /status` publish:

- `hdmi_state`: `none`, `up`, `fallback`, or `released`
- `hdmi_mode`: `WxH` or `0x0`
- `hdmi_recoveries`
- `hdmi_diag`, `hdmi_port`
- `hdmi_regs` and `hdmi_last_shut`: `h0`/`h1`, `phy0`/`pwr0`, `phy1`/`pwr1`, `pv0`/`pv1`, `hvs`, `hsm`, `pm`, `dvp4`, `dvp8`

`hdmi_last_shut` is the line stored in `HDMISHUT.TXT` before any optional HDMI write. `GET /vclog` is the whole firmware message log as text, one `vclog -m` line per entry, then the assert log (`vclog -a`) when that ring is present. The rings are copied into a 256 KiB buffer as soon as the MMU is on. `/status` `vclog` is the first line. `vclog_bytes` is the rendered length. `vclog_status` is `ok`, `wrapped`, `truncated`, or `miss`, plus the `/chosen/log` start and size, the bytes copied, and the used ring size. `VCLOG.TXT` is the boot copy. `VCLOGPRE.TXT` is a second read just before the watchdog. A miss names the reason. `vclog` (raspberrypi/utils) takes the table from `/chosen/log` and, through `/dev/vcio`, the property tag `GET_VC_MEMORY` (`0x00010006`). Linux `rpi-6.12.y` has no VC_LOG tag. `RPI_FIRMWARE_VCHIQ_INIT` (`0x00048010`) only enables VCHIQ. The kernel asks for that memory tag, then reads the device-tree pair, and loads the `VLOG` / `MESS` table with volatile reads. An address already in RAM is used as-is. The legacy alias is `bus & 0x3fffffff`. With no device-tree pair it searches the start and the top of the VC window for `VLOG`. Virt and Pi 5 report `none`. Boot and the reboot path also log the raw replies for `GET_DISPLAY_SETTINGS` (`0x00040014`), the framebuffer GET tags, and EDID block 0 (`0x00030020` and `0x00030023`). Those tags do not set a mode.

### VNC and the optional HDMI process

VNC and the EL0 HDMI task are supervised processes. They draw one 640×480 console from `display-console`: counters, DNS top-N, and a log window. The surface is 32-bit little-endian B, G, R, X. Neither process takes keyboard or pointer input.

VNC is an RFB 3.8 server, raw encoding, optional DES authentication. It is not started unless `display.vnc` is 1. The HTTP form only queues the command. The poll loop, which is not holding the FD lock, starts or stops the process. Stopping sets a hold flag so the exit is not a crash.

The EL0 HDMI process (`hdmi-el0`) is linked only for `board-pi4` and `board-pi5`. Virt does not include the binary. It polls hot-plug through syscall 55 and, when that reports a connection, allocates a framebuffer through the mailbox. That is a different path from the boot console, and the default `display.hdmi` is 0 so it does not run. Pi 5 hot-plug stays disconnected, so that process does not program RP1. The Pi 5 mailbox bases are candidates and are not written while hot-plug stays down.

The home page has Shutdown, Reboot, and one VNC button. Shutdown and reboot wait 300 ms, counted on the hardware timer, so the response can leave, then ask the board to stop or reset this guest. They do not reset the machine that is hosting QEMU. On virt the call is PSCI `SYSTEM_OFF` or `SYSTEM_RESET` through HVC. If EL2 is present the call is skipped. On Pi 5 the call is PSCI over SMC.

## Networking

smoltcp is the IPv4 stack: Ethernet, UDP, TCP, and a DHCPv4 client. Core 1 owns the device once the stack is up. It sleeps in `wfi` until a NIC IRQ, an SGI from a core that queued a socket job, or the timer. When none of those are pending it runs one user thread. Other cores push a request onto a lock-free ring, send SGI 1, and sleep. The owner copies the result into the caller's user buffer and wakes the thread. Core 0 owns the NIC until that handoff, and on a one-core boot.

### virt

virtio-mmio is legacy. QEMU virt (GICv2) maps slot `n` to SPI `16 + n`, INTID `48 + n`. With a NIC and a block device the measured lines are INTID 79 (SPI 47) and INTID 78 (SPI 46). Both are level-triggered. The handler writes the virtio-mmio interrupt-ack register before EOI. The NIC line moves to core 1 when that core claims the stack. The block line targets every online CPU. The virtio-blk waiter holds the disk mutex and sleeps on `wfe`. The handler does not take that mutex. Virtio rings and packet buffers live in `.dma`, mapped Normal non-cacheable.

### GENET

`kernel/src/board/pi4/genet.rs` is the BCM2711 GENET v5 block at `0xFD580000` and the BCM54213PE PHY, address 1, RGMII with the receive delay in the PHY. The March 2025 firmware leaves the GENET clocks gated when a device tree is loaded. The probe enables those clocks and retries the revision register before it decides the block is absent. QEMU raspi4b has no GENET, and that probe failure is non-fatal.

RX completion is GIC SPI 157 and 158 (INTID 189 and 190). The handler masks the GENET summary so the level line drops, then the poll unmasks it and counts a rearm if the summary was masked. TX done is cleared in the handler and does not wake the owner. A posted transmit is not waited out for the whole ring. The driver waits about 50 µs for the consumer so a descriptor is not reused while the MAC still owns it, and `tx_slow` counts a wait that did not finish. The TX ring holds 256 frames. RX offers are refilled as they are taken. After the link has been idle for 100 ms the owner sleeps up to 200 ms (`QUIET_POLL_MS`). That backup is not a substitute for the RX line: a masked summary waits out the whole interval. Gateway ARP is refreshed locally from the DHCP reply's Ethernet source so a quiet link does not solicit the gateway on smoltcp's 60 s neighbor timer.

`/status` publishes `nic_irqs`, packet counts, `net_busy_ms`, `net_idle_ms`, ARP counters, RX drops, overruns, and queued frames, `poll_count`, `poll_us`, `poll_max_us`, `irq_rearms`, `tx_full`, and `tx_slow`.

EMMC2 is SPI 126 (INTID 158). Transfers poll with IRQs unmasked. After the card is in transfer state the driver asks CMD6 for high speed (SDR25, 50 MHz, 3.3 V). UHS and the 1.8 V switch are not used. A CRC, a timeout, or a switch status that did not select function 1 leaves the bus at 25 MHz. `/status` reports `sd_mode` and `sd_clock_hz`.

### DHCP

`network.mode` is `static` or `dhcp`. The Pi 4 build enables `dhcp-default`: ask for a lease, and after `timeout_ms` install the fallback (10.0.2.15/24, gateway 10.0.2.2 in that feature). A static config never sends DHCP. The client waits up to 15 s for PHY link before the DHCP timeout runs. A BOOTREPLY whose `chaddr` is not this board is dropped and is not rewritten. Health probes stay off until an address is installed. `net_ready` is that condition.

## DNS

`dns-el0` is the resolver. It parses with `dns-protocol` (A, AAAA, CNAME, PTR, NS, MX, TXT, name compression). Configured records are A, AAAA, CNAME, and PTR. The path is local records, then the blocklist, then the cache, then a forward.

The cache is `dns-cache`: a fixed pool, default 1024 slots, each 800 bytes, reserved at start. Free list, hash chains, LRU eviction, and a reaper. Expiry is an absolute `CNTPCT` value. Replies that do not fit, and names over 255 bytes, are not stored. `cache_size` is 1..=1024. `cache_reap_interval_secs` is 1..=3600, default 5. Changing the size restarts the DNS task. The reaper returns expired slots to the free list.

Forwards are queued and sent off the client lock, so one dead upstream cannot stall the others. In-flight forwards are capped. A full table counts `forward_full` and refuses a new forward. A client reply the UDP queue will not take counts `reply_drops`. The first attempt uses the upstream that answered last. A timeout walks each other upstream once, then the query fails.

Health is a streak. Every upstream starts up. A probe timeout or a forwarded query timing out counts as one failure. Three failures in a row, with no reply between them, mark that server down and the next lookup skips it. Any reply clears the streak. When every server is down the rotation is the full list. Probes are a short query on the shared upstream socket. Their ids use bit 15 (`0x8000` and up) so a probe reply is not returned to a client. `probe_interval_secs` is optional, 1..=3600, default 5. Probes do not start before the interface has an address. The DNS task publishes the up/down list, and `/status` prints it.

Eight names, eight types, and eight sources are counted in the resolver. A new key replaces the smallest counter. `/debug` prints that blob.

## HTTP

`http-el0` is an HTTP/1.1 server with basic authentication. The dashboard, the JSON endpoints, and the firmware page all require it. `http-server` parses the request and builds the response. There is no TLS.

| Path | Role |
| --- | --- |
| `GET /` | Dashboard: records, VNC, shutdown, reboot |
| `GET /status` | Runtime JSON: uptime, ticks, cache, NIC counters, upstreams, HDMI fields, `vclog`, version |
| `GET /vclog` | Firmware log, `text/plain`, one line per entry |
| `GET /logs` | Recent log |
| `/log-config`, `/dns-config`, `/display` | Forms for `LOG.JSN` and the matching `CONFIG.JSN` objects |
| `/config`, `/records` | Config and record JSON |
| `/config/file/config`, `/config/file/log` | Download `CONFIG.JSN` and `LOG.JSN` |
| `POST /power`, `POST /vnc` | Queue shutdown, reboot, or the VNC process |
| `GET /debug` | Supervisor, cache, top-N |
| `GET /debug/fault/guard`, `/debug/fault/text` | Deliberate faults |
| `GET /debug/crash/...` | Arm a data abort in one service |
| `/firmware` | Upload, tryboot, promote, rollback |

### Firmware update

`kernel/src/fw.rs` and `http-el0/src/firmware.rs` stage files on the boot FAT volume. The live kernel and the live `config.txt` are never upload destinations.

- A kernel upload lands on `KERNEL8.NEW` (Pi 4 and virt) or `K2712.NEW` (Pi 5). The browser posts raw 16 KiB chunks to `POST /firmware/chunk` with `X-Fw-Kind`, `X-Fw-Offset`, `X-Fw-Total`, and `X-Fw-Final`.
- `POST /firmware/reboot` with `tryboot=0` is an ordinary reboot. `tryboot=1` asks the firmware to load `tryboot.txt` once, through `SET_REBOOT_FLAGS`, and only when a staged kernel is present. The next kernel clears that flag before it can crash in the firmware-log read, and retries once after that read if the first clear did not land. If the clear is still refused, `TRYBOOT.TXT` is rewritten in place to boot `kernel8.img`. Promote leaves that same file. The live `config.txt` is not renamed.
- Promote runs in slices from the poll loop so `/status` keeps answering. It copies the live kernel to `KERNEL8.BAK`, then installs the staged bytes. It runs only after a tryboot boot unless force is set. A promote that dies between the renames is repaired from the temp names on the next boot.
- `POST /firmware/rollback` restores `KERNEL8.BAK` over the live file.
- `GET /firmware/status` is the JSON view: staged bytes, backup, tryboot, phase, progress, booted id, and SHA-256.

Virt stages the Pi 4 names. QEMU boots with `-kernel`, so a virt reboot does not execute the staged file. The test still reads the FAT names back. Pi 5 can stage `K2712.NEW`. Its tryboot mailbox is not wired, and `kernel_2712.img` does not fit an 8.3 name, so promote and rollback are refused there. The running image is recognized by `pi4dns-boot-id:` plus the version, kept in `.bootid` at the end of the kernel.

## Storage and JSON config

The board supplies the block device: virtio-blk, Pi 4 EMMC2 at `0xFE340000` (legacy Arasan at `0xFE300000` only after EMMC2 fails, which is where raspi4b attaches the card), or Pi 5 `sdio1`. Pi 4 EMMC2 and Pi 5 `sdio1` try SDR25 and fall back to 25 MHz. The filesystem is FAT32 through `embedded-sdmmc`. One lock covers the volume.

`CONFIG.JSN` is the appliance config. A leftover `CONFIG.TXT` on that volume is migrated once into `CONFIG.JSN`. That file is not the Pi firmware `config.txt` in the boot partition root. `LOG.JSN` is the log service. `LOG.TXT` rotates to `LOG1.TXT` through `LOG16.TXT`.

Every JSON document has `version` 1. It is accepted only when it is well formed, every required field is present, each value is in range, and no key is unknown. Optional keys may be absent. A saved file is pretty-printed when that text still fits in 4096 bytes. A larger pretty form is stored compact. Callers apply cross-field rules, such as the log budget `max_bytes * (archives + 1) <= 1048576`, before anything is written.

`CONFIG.JSN` objects:

- `network`: `mode` (`static` or `dhcp`), `ip`, `netmask`, `gateway`, `timeout_ms`. Optional `speed` (0..=1000) and `arp_secs` (0..=60).
- `dns`: `cache_size`, `cache_reap_interval_secs`, `upstreams` (1..=8), `blocklist` (0..=32), `records` (0..=64). Optional `probe_interval_secs`.
- `http`: `port`, `username`, `password`.
- `display`: `vnc`, `vnc_port`, `vnc_password` (at most 8 bytes), `hdmi`. Optional `hdmi_reset`, omitted from the file when 0.

`LOG.JSN` is `version`, `max_bytes` (4096..=65536), and `archives` (0..=16). The default is 4096 bytes and 3 archives.

Unknown keys and out-of-range values are rejected and the previous file is left in place. A NIC address or HTTP port change is stored and reported as needing a reboot. The cache size is applied by restarting the DNS task.

## Logging, telnet, exceptions

Before the log task is up, and after it dies, lines go straight to the UART. Once it is up, records go into a fixed queue of 128 slots, 80 bytes each. A full queue drops the line and counts the drop. It never waits. The task prints the line, appends the ones marked for `LOG.TXT`, and publishes each line once into a fan-out ring.

Telnet is TCP port 23, up to 16 connections, each on its own thread. A 17th is told the server is busy and closed. A client that falls behind skips lines and is told how many. That does not stall the log task, the UART, or any other connection. `run-local.sh` forwards host port 2323.

`exception_handler` prints ESR, ELR, FAR, SPSR, and the general registers on the UART. It does not allocate. One short line is copied into the log queue only when that lock is free and the log task is up. A nested fault prints one fixed line and halts. A kernel fault, panic, watchdog stall, or a user fault that is about to stop the kernel writes a checksummed record into an 8 KiB RAM slot 32 MiB into memory. That slot is not part of the image and boot does not zero it. The handler then cleans the cache and resets (Pi 4 watchdog, virt and Pi 5 PSCI). It does not write the card from the fault path. The next boot checks the magic and checksum and writes `CRASH/CRASH000.TXT` through `CRASH007.TXT`, keeping eight. The boot line is `crash recovered <file>`. `/status` adds `crash_count` and `last_crash`. `GET /crash/last` and `GET /crash/list` are authenticated. Three crashes inside three minutes arm a safe flag and reboot once more; a further crash on that boot does not reset, so the machine can stay up with the network. `POST /debug/crash` with `confirm=crash` and `kind` `abort`, `panic`, or `hang` is the hardware trigger. `GET /debug/fault/guard` and `GET /debug/fault/text` are the deliberate cases: a translation fault on the guard page, and a permission fault on kernel text.

The Pi 4 power path is the watchdog above. A panic and an exception store the crash record and then use that watchdog. They do not release the display first. After three quick crashes the next fault halts instead of resetting, and that halt disarms the watchdog so the freeze is not turned into another reset.

`GET_BOARD_REVISION` is the same mailbox read the tryboot line already makes. When it matches the image (`pi4`, `pi5`, or `virt`), `/status` `board` is the model (`pi4b`, `pi400`, `cm4`, `pi5`) and `board_source` is `revision` or `dtb`. A mismatch keeps that image's drivers and addresses and reports `board_source` `build`. `board.detect` `0` in `CONFIG.JSN` does the same. Missing means detect stays on. Virt without a device tree reports `virt` / `build`.

`POST /debug/boot` `kind=hang` suspends the stall detector so the 15 second PM watchdog is what resets the board. It records a watchdog reset in the boot RAM record before it stops petting. The firmware clears `PM_RSTS` before the kernel can see bit 5, so the next boot reports `last_reset=watchdog` from that record. Bit 9 is still the marker for a reset this kernel asked for, and bit 12 is power-on, when those bits survive. `boot_stopped` is the last stage the previous boot reached. It is `good` when that boot passed the 15 second mark, and `start` only when it did not get past arming the record. `boot_good` is this boot. A tryboot of `kernel8.new`, a promote, or a normal boot of a different image clears the attempt counter. Rollback runs only when this boot is the `kernel8.bak` tryboot. `/crash/list` and `crash_count` are the files on the card.

A 15 second watchdog is armed at the start of `kernel_main`, after one bounded firmware-revision read and before HDMI and the card. The main loop pets it. Pi 4 uses the PM watchdog. QEMU's Pi 4 power manager answers firmware revision 346337 and resets as soon as the reset bit is written, so that machine stays on the timer deadline. Virt and Pi 5 have no PM block, so the timer fires the same reset if the pet stops. A boot that does not reach `boot good` (network, HTTP, and DNS up for 15 seconds) counts as a failed boot. The counter is the second half of the crash RAM page. After three failed boots the next start rewrites `TRYBOOT.TXT` in place so it names `kernel8.bak`, then uses `SET_REBOOT_FLAGS`. The file is not renamed or deleted. If the backup is missing, or the flag is refused, the kernel stays up and does not reset in a loop. A refused flag clear on an ordinary boot still rewrites that file to `kernel8.img`, unless a fallback is in progress. `POST /debug/boot` with `confirm=boot` and `kind=hang` stops petting and does not reset itself. `kind=fail` marks the counter and reboots so the next boot takes the fallback.

## Version, build, and tests

The version is `0.1.<pull request>`. Major stays 0 and minor stays 1. The only literal is `release!()` in `version/src/lib.rs`. The boot banner, the telnet preface, `/status`, the firmware page, and `.bootid` all read it. Bump that literal, and the copies in `scripts/test-integration.sh`, when opening a pull request.

Host tests are `RUSTFLAGS="-D warnings" cargo test --lib`. That does not compile the kernel binary. `cargo test --bin kernel8` is not a supported check. Release checks are `cargo check --release --target aarch64-unknown-none` three ways: virt with default features, Pi 4 with `board-pi4,dhcp-default`, and Pi 5 with `board-pi5`. `RUSTFLAGS="-D warnings"` replaces the flags in `.cargo/config.toml`, so the check has to pass with that variable set.

`scripts/test-integration.sh` builds the virt image, boots QEMU virt (`-M virt,gic-version=2 -cpu cortex-a72 -smp 4`), and checks DNS, HTTP, DHCP, FAT, telnet, VNC, firmware staging, faults, SMP, and load. It then builds the Pi 4 and Pi 5 images, checks the link addresses, and runs the raspi4b smoke test when that machine exists. The first QEMU boot is capped at 240 seconds. Do not edit the script while a run is in progress.

`scripts/run-qemu.sh` is the interactive virt boot. `scripts/fetch-pi4-boot.sh` stages the pinned firmware next to `config.txt`. `scripts/install-qemu-10.sh` installs the QEMU used by the raspi4b stage.
