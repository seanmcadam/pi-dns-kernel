# Raspberry Pi 4 Bare-Metal DNS Server

A purpose-built, bare-metal kernel written in Rust (no Linux, `no_std`) for the Raspberry Pi 4 (BCM2711, AArch64) that implements a complete DNS server appliance.

**Current Status**: QEMU virt image serves local DNS, forwards other names, answers HTTP, can take a DHCP lease, and stores records on a FAT32 virtio-blk disk. A separate Pi 4 image links at `0x80000`. That image has not been tested on hardware.

## Quick Status

| Component | Status | Notes |
|-----------|--------|-------|
| Boot (QEMU virt) | Working | Raw image linked at `0x40080000` |
| VirtIO-net | Working | Legacy virtio-mmio, ARP/ICMP, UDP, TCP |
| DNS | Working | Local records, upstream forward, cache |
| HTTP | Working | Dashboard can add and remove records |
| DHCP | Working | User-net lease, static fallback on timeout |
| FAT32 config | Working | virtio-blk, records survive reboot, rotating logs |
| Pi 4 image | Compiles, untested on hardware | `0x80000`, PL011, EMMC2, GENET |
| EL1 exceptions | Working on QEMU | VBAR_EL1, fault report, then halt |
| MMU | Working on QEMU | Identity map, caches on, W^X. Pi 4 map untested |

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
- The ARM virtual timer (`CNTV`, PPI 11, INTID 27) ticks at 100 Hz. QEMU `virt` with `-cpu cortex-a72` is GICv2 (`0x08000000` distributor, `0x08010000` CPU interface); the run scripts pass `gic-version=2`. The Pi 4 GIC-400 addresses are compiled in and untested. `/status` reports uptime in seconds (`CNTVCT` / frequency) and the IRQ tick count. The poll loop is unchanged and is not preempted. Spurious GIC id 1023 is ignored.
- EL0 runs from a window at `0x1_0000_0000` in TTBR0, outside the identity map. Kernel pages are EL1-only. User code is EL0 read/execute and privileged-execute-never; user stack is EL0 read/write and not executable. User pages are non-global with ASID 1. PAN is enabled when the CPU implements it. The kernel stays identity-mapped: moving it to TTBR1 needs a high link address plus a second identity map for virtio DMA. A boot-time user program logs, yields, waits for a tick, and exits. Bad syscalls and bad pointers return errors. A kernel load, a privileged `mrs`, or a branch to the stack kills that task and the kernel keeps serving DNS. Yield is cooperative. The timer IRQ at EL0 resumes the same task and does not preempt.
- Identity-mapped 4 KiB pages on TTBR0. TTBR1 is an empty table reserved for a later high-half kernel. `.text` is read-only and executable, `.rodata` is read-only and not executable, and data, bss, heap, and stack are read-write and not executable. One guard page sits under the stack.
- Virtio rings and packet buffers are in `.dma` and mapped Normal non-cacheable so DMA stays coherent with the data cache on. Other RAM is write-back cacheable. UART, virtio-mmio, and the GIC are Device-nGnRE.
- `GET /debug/fault/guard` and `GET /debug/fault/text` (HTTP basic auth) deliberately fault.
- `boot.s` drops EL2 to EL1h when `CurrentEL` is EL2 (QEMU `-M virt,virtualization=on`, and a Pi 4 firmware handover). QEMU `-kernel` without that option is already at EL1 and skips the drop. The boot log prints `Entered at EL` and `Running at EL`. The Pi 4 drop is untested on hardware.

### Pi 4 image (untested on hardware)
- Separate link at `0x80000`
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
- the Pi 4 image links at `0x80000` and `boot/pi4/config.txt` names `kernel8.img`

## Deployment

### QEMU (for testing)

```bash
./scripts/run-qemu.sh
```

Note: QEMU's raspi3b/raspi4b models have limited peripheral support. Network testing may require the `virt` machine type with virtio devices.

### Real Raspberry Pi 4

**This image has not been tested on hardware.** `scripts/build-pi4.sh` is a separate target from the QEMU image. Do not copy `boot/kernel8.img` to the card: that file is linked at `0x40080000` for QEMU. The Pi 4 file is `boot/pi4/kernel8.img`, linked at `0x80000`.

1. Build the SD card files:

   ```bash
   ./scripts/build-pi4.sh
   ```

   That writes:

   - `boot/pi4/kernel8.img` — raw kernel, entry at `0x80000`
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
