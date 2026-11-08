# Raspberry Pi 4 Bare-Metal DNS Server

A purpose-built, bare-metal kernel written in Rust (no Linux, `no_std`) for the Raspberry Pi 4 (BCM2711, AArch64) that implements a complete DNS server appliance.

**Current Status**: QEMU virt image serves local DNS, forwards other names, answers HTTP, can take a DHCP lease, and stores records on a FAT32 virtio-blk disk. The DNS resolver runs at EL0. The kernel is linked in the upper half. A separate Pi 4 image is linked at `0xFFFF000000080000` and the firmware still loads that raw file at physical `0x80000`. That image has not been tested on hardware.

## Quick Status

| Component | Status | Notes |
|-----------|--------|-------|
| Boot (QEMU virt) | Working | Linked at `0xFFFF000040080000`, loaded at `0x40080000` |
| VirtIO-net | Working | Legacy virtio-mmio, ARP/ICMP, UDP, TCP |
| DNS | Working | Resolver at EL0. Local records, upstream forward, cache |
| HTTP | Working | Dashboard can add and remove records |
| DHCP | Working | User-net lease, static fallback on timeout |
| FAT32 config | Working | virtio-blk, records survive reboot, rotating logs |
| Pi 4 image | Compiles, untested on hardware | High link `0xFFFF000000080000`, load `0x80000`, PL011, EMMC2, GENET |
| EL1 exceptions | Working on QEMU | VBAR_EL1, fault report, then halt |
| MMU | Working on QEMU | Kernel in TTBR1, user window in TTBR0, W^X. Pi 4 map untested |

---

## Features

### Implemented
- ✅ Bare-metal boot on Raspberry Pi 4 (AArch64)
- ✅ Serial UART output for debugging (115200 baud)
- ✅ Memory allocation (bump allocator)
- ✅ Logging infrastructure
- ✅ Timer support
- ✅ DNS protocol implementation (parsing and building packets)
  - A, AAAA, CNAME, PTR, NS, MX, TXT records
  - Name compression
  - Full unit test coverage
- ✅ HTTP server implementation
  - GET/POST/PUT/DELETE methods
  - Basic authentication
  - Unit tested
- ✅ DNS cache with TTL and eviction
- ✅ Configuration management
- ✅ Unit tests for all protocol layers

### QEMU image
- VirtIO-net packet I/O on the `virt` machine
- Local DNS records and forwarding of other names to the configured upstreams, with TTL caching
- DHCP client with a configurable static fallback
- virtio-blk FAT32 volume for `CONFIG.TXT`, plus `LOG.TXT` rotated to `LOG.OLD`
- Web dashboard to add and remove records

### Exceptions and MMU
- EL1 vector table installed from `boot.s` before Rust runs. Synchronous exceptions, SError, and unexpected IRQs from EL1 print ESR/ELR/FAR/SPSR plus the general registers, then halt. The current-EL IRQ vector saves and restores general and SIMD registers, acknowledges the GICv2, and returns. Lower-EL AArch64 sync and IRQ save the user task and can return to it or to the kernel.
- The ARM virtual timer (`CNTV`, PPI 11, INTID 27) ticks at 100 Hz on every core. QEMU `virt` with `-cpu cortex-a72 -smp 4` is GICv2 (`0x08000000` distributor, `0x08010000` CPU interface); the run scripts pass `gic-version=2`. Each core initializes its own CPU interface and arms its own compare register. `/status` reports uptime in seconds (`CNTVCT` / frequency) and core 0's IRQ tick count, still 100 Hz. The other cores preempt and do not add to that count. Spurious GIC id 1023 is ignored.
- Four cores. On QEMU, core 0 calls PSCI `CPU_ON` with HVC (this machine's conduit is HVC; SMC is undefined at EL1). If EL2 is implemented, that HVC never reaches the firmware, so the extra cores stay offline; the default virt machine has no EL2. On the Pi 4 image, core 0 writes the spin-table release words at `0xE0`, `0xE8`, and `0xF0`. That Pi 4 path has not been tested on hardware. `TPIDR_EL1` holds the core's current thread. Threads of one process share TTBR0 and ASID. The run queue is global. Core 1 owns smoltcp and the NIC. It sleeps until a virtio used-ring IRQ (QEMU 8.2: INTID 79, SPI 47, the high mmio slot), an SGI from a core that queued a socket operation, or the timer, and it runs user threads when the NIC has nothing to do. The block device (INTID 78, SPI 46, on that same QEMU) wakes its waiter with an IRQ instead of a spin; the half-second counter timeout stays. The Pi 4 GENET (INTID 189 and 190) and EMMC2 (INTID 158) lines are wired in the GIC-400 driver and have not been tested on hardware. The FAT volume stays behind one lock. No IRQ handler takes the scheduler lock, the FAT lock, or the disk mutex.
- The kernel is linked in the upper half and runs from TTBR1. QEMU's link address is `0xFFFF000040080000` (the raw image is still loaded at physical `0x40080000`). The Pi 4 link address is `0xFFFF000000080000` (the firmware still loads the raw image at physical `0x80000`; that image is untested on hardware). Bits 47:0 of a kernel virtual address are the physical address. `boot.s` builds a 1 GiB identity map in TTBR0 and the same blocks in TTBR1, turns the MMU on, jumps to the high PC, then clears TTBR0. Rust then installs the strict TTBR1 map. TTBR0 holds only the user window after that. EL0 cannot walk TTBR1, so a user load of kernel memory faults.
- EL0 runs from a window at `0x1_0000_0000` in its own TTBR0 root. Kernel pages are EL1-only. User code is EL0 read/execute and privileged-execute-never; user data, heap, and stack are EL0 read/write and not executable. User pages are non-global. The boot test and the DNS task use ASID 1. The HTTP task uses ASID 2. A spinner, armed from the dashboard, uses ASID 3. PAN is enabled when the CPU implements it. A boot-time user program logs, yields, spins across a preemption until the tick count moves, and exits. The DNS resolver then runs in that window: it parses queries, checks the blocklist, answers local names, caches, and forwards upstream. The web UI is a second task, with its own pages. The log task is a third. It drains a kernel queue, prints the lines, and appends the ones marked for `LOG.TXT`. A full queue drops the line and counts it. Until that task is running, and if it stops, the kernel writes the UART itself. It also publishes each printed line once into a fixed ring. The telnet task is a fourth process, with its own pages and ASID. It accepts TCP port 23 and gives each connection its own thread, up to 16. A 17th connection is told `busy, try later` and closed. A client that falls behind the ring loses lines and is told how many. That does not stall the log task, the UART, or any other connection. UDP and TCP sockets, the clock, login checks, and config reads and writes are syscalls. Bad pointers return an error. A kernel load, a privileged `mrs`, or a branch to the stack kills that task. The kernel keeps the NIC, the socket buffers, storage, and the config file. A task yields when a socket would block. A timer IRQ at EL0 also returns to the poll loop after two ticks, so a task that never yields cannot starve the others or the NIC. The boot test still reads the tick count after that preemption. Upstream waits use the timer counter.
- Strict TTBR1 map: `.text` is read-only and executable, `.rodata` is read-only and not executable, and data, bss, heap, and stack are read-write and not executable. One guard page sits under the stack. Devices are programmed with physical addresses. The CPU uses the high alias, including for `.dma`.
- Virtio rings and packet buffers are in `.dma` and mapped Normal non-cacheable so DMA stays coherent with the data cache on. Other RAM is write-back cacheable. UART, virtio-mmio, and the GIC are Device-nGnRE.
- `GET /debug/fault/guard` and `GET /debug/fault/text` (HTTP basic auth) deliberately fault.
- `boot.s` drops EL2 to EL1h when `CurrentEL` is EL2 (QEMU `-M virt,virtualization=on`, and a Pi 4 firmware handover). QEMU `-kernel` without that option is already at EL1 and skips the drop. The boot log prints `Entered at EL` and `Running at EL`. The Pi 4 drop is untested on hardware.

### EL0 syscall ABI
The user program puts the number in `x8`, arguments in `x0`–`x5`, and reads the result from `x0`. A pointer must lie in the user code, data, heap, or stack window. A store into user code returns `-2` and is not performed. The kernel does not allocate on these calls.

| x8 | Call | Result |
|----|------|--------|
| 0 | `log(ptr, len)` | `0`, or `-2` if the slice is not in the user window. `len` is 1..=128 |
| 1 | `yield` | `0`. The kernel polls the NIC and runs the other tasks, then this task resumes |
| 2 | `uptime_ticks` | IRQ tick count (100 Hz) |
| 3 | `exit(code)` | Does not return |
| 4 | `time_ms` | `CNTPCT` milliseconds, not the IRQ tick count |
| 5 | `udp_bind(port)` | fd, or an error |
| 6 | `udp_recv(fd, buf, cap, addr)` | length, or `-11` if nothing is waiting. `addr` is 8 writable bytes: IPv4, big-endian port, 2 bytes of pad |
| 7 | `udp_send(fd, buf, len, ip, port)` | `0` on success. `ip` is a `u32` whose big-endian bytes are the address |
| 8 | `tcp_listen(port)` | fd |
| 9 | `tcp_accept(fd)` | new fd, or `-11` |
| 10 | `tcp_recv(fd, buf, cap)` | length, `0` if the peer has closed, or `-11` |
| 11 | `tcp_send(fd, buf, len)` | bytes sent, or an error |
| 12 | `tcp_close(fd)` | `0` |
| 13 | `config_records(buf, cap)` | length of `name TYPE value ttl` lines |
| 14 | `config_upstreams(buf, cap)` | length of `a.b.c.d` lines |
| 15 | `config_blocklist(buf, cap)` | length of `name` lines |
| 16 | `config_cache_size` | configured cache size |
| 17 | `set_cache_entries(n)` | `0`. The DNS task publishes the live cache count |
| 18 | `auth(user, user_len, pass, pass_len)` | `0` if the pair matches the saved login |
| 19 | `config_add(buf, len)` | `0`. `buf` is `name TYPE value ttl`. The kernel saves the file |
| 20 | `config_remove(buf, len)` | `0` if removed, `1` if the name was absent |
| 21 | `uptime_secs` | `CNTVCT` seconds |
| 22 | `cache_entries` | live cache count |
| 23 | `fault_guard` | does not return. EL1 reads the guard page |
| 24 | `fault_text` | does not return. EL1 stores to kernel text |
| 25 | `http_info(buf, cap)` | `port username` |
| 26 | `tick_hz` | timer frequency, 100 |
| 27 | `debug_spin` | `0`. Arms an EL0 task that branches to itself |
| 28 | `log_post(ptr, len)` | `0` queued, `1` if the queue was full and the line was dropped |
| 29 | `log_drain(buf, cap)` | length, or `0`. Bytes are level, flags, text |
| 30 | `log_emit(ptr, len)` | `0`. UART and the recent-log buffer |
| 31 | `log_recent(buf, cap)` | length of the recent log text |
| 32 | `file_read(name, name_len, buf, cap)` | length, or `0` if the file is missing |
| 33 | `file_write(name, name_len, buf, len, flags)` | `0`. `flags` 1 appends |
| 34 | `thread_create(entry, stack, arg)` | thread id, or `-4` / `-2` / `-6` |
| 35 | `thread_exit(code)` | does not return. A pool thread's joiners wake |
| 36 | `thread_join(tid)` | the exit code, or the caller blocks until the thread exits |
| 37 | `futex_wait(addr, expected)` | `0` after a wake, `1` if the word no longer matches, `-2` if `addr` is bad |
| 38 | `futex_wake(addr, n)` | number woken. `n` of 0 wakes one |
| 39 | `log_publish(ptr, len)` | `0`. Log task only. One line into the fan-out. Never waits |
| 40 | `log_follow(cursor, buf, cap, drops, flags)` | length of the next line. The caller sleeps if the ring is idle, unless `flags` is `1`, which returns `0`. `drops` receives how many lines this reader skipped |
| 41 | `log_wake()` | `0`. Wakes every `log_follow` sleeper. Does not publish a line |
| other | | `-1` |

Errors: `-1` unknown number, `-2` bad pointer, `-3` bad fd, `-4` bad argument, `-5` I/O, `-6` too many fds or threads, `-7` the config text does not fit, `-11` try again. Packet and config buffers are at most 4096 bytes. Each task has 24 fds. `tcp_recv`'s fourth argument is `1` to return `-11` instead of sleeping. Telnet listens on guest port 23. `run-local.sh` forwards host port 2323 (`TELNET_HOST_PORT`).

### Pi 4 image (untested on hardware)
- Linked at `0xFFFF000000080000`. The firmware loads the raw image at physical `0x80000`.
- PL011 UART on GPIO 14/15
- EMMC2 SD driver and GENET Ethernet driver, compiled into `boot/pi4/kernel8.img`

## Architecture

```
┌─────────────────────────────────────┐
│         Application Layer            │
│  ┌──────────────┐  ┌──────────────┐ │
│  │  DNS Server  │  │  HTTP Server │ │
│  │   (Port 53)  │  │   (Port 80)  │ │
│  └──────────────┘  └──────────────┘ │
└─────────────────────────────────────┘
┌─────────────────────────────────────┐
│         Protocol Layer               │
│  ┌──────────────┐  ┌──────────────┐ │
│  │ DNS Protocol │  │ HTTP Protocol│ │
│  │    Parser    │  │    Parser    │ │
│  └──────────────┘  └──────────────┘ │
│  ┌──────────────┐  ┌──────────────┐ │
│  │  DNS Cache   │  │    Config    │ │
│  └──────────────┘  └──────────────┘ │
└─────────────────────────────────────┘
┌─────────────────────────────────────┐
│         Network Layer                │
│  ┌──────────────────────────────────┐│
│  │      smoltcp TCP/IP Stack       ││
│  │   (IPv4/IPv6, TCP/UDP, DHCP)    ││
│  └──────────────────────────────────┘│
│  ┌──────────────┐  ┌──────────────┐ │
│  │GENET Driver  │  │QEMU VirtIO   │ │
│  │  (Pi 4 HW)   │  │   (Test)     │ │
│  └──────────────┘  └──────────────┘ │
└─────────────────────────────────────┘
┌─────────────────────────────────────┐
│         System Layer                 │
│  ┌──────────────┐  ┌──────────────┐ │
│  │   SD Card    │  │    Timer     │ │
│  │  (EMMC2)     │  │              │ │
│  └──────────────┘  └──────────────┘ │
│  ┌──────────────┐  ┌──────────────┐ │
│  │     UART     │  │  Interrupts  │ │
│  │  (Serial)    │  │   (GIC-400)  │ │
│  └──────────────┘  └──────────────┘ │
│  ┌──────────────────────────────────┐│
│  │      Memory Allocator            ││
│  └──────────────────────────────────┘│
└─────────────────────────────────────┘
┌─────────────────────────────────────┐
│         Boot Layer                   │
│    Assembly boot → Rust kernel_main  │
└─────────────────────────────────────┘
```

## Building

### Prerequisites

- Rust toolchain (1.83+)
- AArch64 target: `rustup target add aarch64-unknown-none`
- For QEMU testing: `qemu-system-aarch64`
- For physical Pi 4: Raspberry Pi firmware files

Boot assembly is compiled with Rust's built-in assembler (`core::arch::global_asm!`). A C cross compiler such as `aarch64-linux-gnu-gcc` is not required. `scripts/build.sh` still uses `aarch64-linux-gnu-objcopy` to write the raw `kernel8.img`.

### Local Setup (Linux Mint 22.3 / Ubuntu 24.04)

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Add AArch64 bare-metal target
rustup target add aarch64-unknown-none

# Install QEMU (version 8.2+)
sudo apt update
sudo apt install -y qemu-system-arm qemu-system-aarch64

# Install DNS and HTTP test tools
sudo apt install -y dnsutils curl netcat-openbsd

# Clone and build
git clone <repository-url>
cd pi4-dns-server
./scripts/build.sh

# Run in QEMU with network.
# Host DNS defaults to 15353 because 5353 is mDNS (often held by avahi,
# Spotify, or Brave). Override either port with DNS_HOST_PORT / HTTP_HOST_PORT.
./scripts/run-qemu.sh

# In another terminal, test DNS and HTTP
dig @127.0.0.1 -p 15353 test.local
curl -u admin:admin http://127.0.0.1:8080/status

# Or run full integration tests
./scripts/test-integration.sh
```

**Note**: QEMU 8.2 is included in Ubuntu 24.04 and Linux Mint 22.3 repositories. For older versions, you may need to build QEMU from source or use a PPA.

### Build Commands

```bash
# Build the kernel
./scripts/build.sh

# Run in QEMU (raspi3b model for testing)
./scripts/run-qemu.sh

# Run integration tests
./scripts/test-integration.sh

# Run unit tests
cargo test --lib
```

## Testing

### Unit Tests

Each protocol layer has comprehensive unit tests:

```bash
cargo test --lib
```

Test coverage:
- **dns-protocol**: Packet parsing, building, name compression
- **http-server**: Request parsing, response building, basic auth
- **dns-cache**: Insert, retrieve, TTL expiration, eviction
- **config**: Configuration parsing (minimal)

All tests run with `std` feature for host-side validation.

### Integration Tests

```bash
./scripts/test-integration.sh
```

`./scripts/test-integration.sh` checks the original boot, local DNS, and HTTP cases, then:

- `dig` for `example.com` through QEMU user networking, and a second query served from the cache
- a DHCP lease from QEMU user-net, and the static fallback when no DHCP server answers
- adding a record in the web UI, rebooting the same FAT disk, and resolving that name
- the Pi 4 image links at `0xFFFF000000080000` and `boot/pi4/config.txt` names `kernel8.img`

## Deployment

### QEMU (for testing)

```bash
./scripts/run-qemu.sh
```

Note: QEMU's raspi3b/raspi4b models have limited peripheral support. Network testing may require the `virt` machine type with virtio devices.

### Real Raspberry Pi 4

**This image has not been tested on hardware.** `scripts/build-pi4.sh` is a separate target from the QEMU image. Do not copy `boot/kernel8.img` to the card: that file is the QEMU image, linked at `0xFFFF000040080000` and loaded at physical `0x40080000`. The Pi 4 file is `boot/pi4/kernel8.img`, linked at `0xFFFF000000080000`. The firmware loads that raw file at physical `0x80000`.

1. Build the SD card files:

   ```bash
   ./scripts/build-pi4.sh
   ```

   That writes:

   - `boot/pi4/kernel8.img` — raw kernel. The ELF entry is `0xFFFF000000080000`. The firmware loads the file at physical `0x80000`.
   - `boot/pi4/config.txt` — `arm_64bit=1`, `enable_uart=1`, `kernel=kernel8.img`

   The image uses the Pi 4 PL011 UART at `0xFE201000` (GPIO 14/15, 115200 8N1), the EMMC2 SD controller at `0xFE340000`, and the GENET Ethernet controller at `0xFD580000`.

2. Flash a microSD card. The Pi boot ROM reads a FAT32 filesystem. On a card with one FAT32 partition, that partition is the boot partition:

   ```bash
   # Example only. Replace sdX with the card, and unmount it first.
   sudo mkfs.vfat -F 32 /dev/sdX1
   sudo mount /dev/sdX1 /mnt
   sudo cp boot/pi4/kernel8.img boot/pi4/config.txt /mnt/
   # start4.elf and fixup4.dat from the Raspberry Pi firmware are also required.
   sudo umount /mnt
   ```

3. Serial console, 115200 baud: GND to pin 6, adapter TX to pin 10, adapter RX to pin 8.

4. Insert the card, connect Ethernet, and power on. Until this image is run on a board, treat boot, DHCP, and SD writes as unverified. The QEMU image remains the tested one.

## Configuration

The default QEMU image uses a static address so the integration tests can reach it immediately:

```
mode=static
ip=10.0.2.15
netmask=255.255.255.0
gateway=10.0.2.2
upstream=8.8.8.8
upstream=1.1.1.1
upstream=10.0.2.3
```

`10.0.2.3` is QEMU's user-net DNS proxy. Build with `--features dhcp-default` to request a lease first and fall back to `10.0.2.15/24` after three seconds. The same fields are stored in `CONFIG.TXT` on the FAT volume (`record=name TYPE value ttl`). The dashboard at `/` adds and removes records; `DELETE /records` with `name=` removes one. `LOG.TXT` is copied to `LOG.OLD` once it passes 512 bytes.

## Project Structure

```
.
├── kernel/              # Main bare-metal kernel
│   ├── src/
│   │   ├── main.rs      # Entry point
│   │   ├── boot.s       # Assembly boot code
│   │   ├── uart.rs      # Serial driver
│   │   ├── memory.rs    # Allocator
│   │   ├── net/         # Network stack
│   │   ├── dns/         # DNS server
│   │   ├── http/        # HTTP server
│   │   └── fs/          # Filesystem
│   └── link.ld          # Linker script
├── dns-protocol/        # DNS packet parser (with std tests)
├── http-server/         # HTTP handler (with std tests)
├── dns-cache/           # DNS cache (with std tests)
├── config/              # Config parser (with std tests)
├── boot/                # Boot files for Pi 4
├── scripts/             # Build and test scripts
└── tests/               # Integration tests

```

## Limitations and Known Issues

### What Works
- ✅ Boots on QEMU (raspi3b model)
- ✅ Serial console output
- ✅ Memory allocation
- ✅ Unit tests for all protocol layers

### What's Tested in QEMU
- ✅ Boot process
- ✅ Serial output
- ⚠️  Network stack (requires virtio-net setup)
- ❌ DNS queries (network not yet working in QEMU)
- ❌ HTTP requests (network not yet working in QEMU)
- ❌ SD card operations (QEMU SD support incomplete)

### What Requires Real Hardware
- ⚠️  GENET Ethernet driver (not yet implemented)
- ⚠️  EMMC2 SD card driver (not yet implemented)
- ⚠️  Full interrupt handling (GIC-400)
- ❌ Real-world network performance testing
- ❌ Real-world DNS query load testing

### Current Blockers
1. **Network device abstraction**: GENET driver for Pi 4 not implemented
2. **QEMU network testing**: Need to set up virtio-net configuration
3. **SD card driver**: EMMC2 driver not implemented
4. **Filesystem**: FAT32 support not wired up
5. **DHCP**: Client implementation incomplete

## Development Roadmap

### Phase 1: Foundation ✅
- [x] Boot code and serial output
- [x] Memory allocator
- [x] Logger and timer
- [x] Protocol parsers with unit tests

### Phase 2: Network Stack 🚧
- [ ] Complete smoltcp integration
- [ ] QEMU virtio-net driver
- [ ] Basic UDP socket handling
- [ ] DNS query/response in QEMU

### Phase 3: DNS Server 🚧
- [ ] Authoritative record lookup
- [ ] Query forwarding
- [ ] Cache integration
- [ ] DNS integration tests

### Phase 4: Storage 📋
- [ ] EMMC2 SD card driver
- [ ] FAT32 filesystem
- [ ] Configuration loading/saving
- [ ] Log file rotation

### Phase 5: Web UI 📋
- [ ] HTTP TCP socket handling
- [ ] Basic web pages (status, logs)
- [ ] Configuration editing
- [ ] Authentication

### Phase 6: Hardware 📋
- [ ] GENET Ethernet driver
- [ ] DHCP client
- [ ] Pi 4 testing and optimization
- [ ] Performance tuning

## Contributing

This is a learning/demonstration project showing bare-metal Rust on AArch64. Contributions are welcome, especially for:

- GENET Ethernet driver implementation
- EMMC2 SD card driver
- QEMU network test infrastructure
- Additional DNS record types
- Performance optimizations

## License

MIT License - see LICENSE file

## References

- [Raspberry Pi 4 Documentation](https://www.raspberrypi.com/documentation/computers/raspberry-pi.html)
- [BCM2711 ARM Peripherals](https://datasheets.raspberrypi.com/bcm2711/bcm2711-peripherals.pdf)
- [ARM Cortex-A72 Documentation](https://developer.arm.com/documentation/100095/0003/)
- [smoltcp Network Stack](https://github.com/smoltcp-rs/smoltcp)
- [The Embedonomicon](https://docs.rust-embedded.org/embedonomicon/)
- [Writing an OS in Rust](https://os.phil-opp.com/)

## Acknowledgments

Built with:
- Rust `no_std` ecosystem
- `smoltcp` for networking
- `embedded-sdmmc` for FAT32
- `cortex-a`, `aarch64-cpu`, `tock-registers` for AArch64 support
