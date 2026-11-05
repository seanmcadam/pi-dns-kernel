# Raspberry Pi 4 Bare-Metal DNS Server

A purpose-built, bare-metal kernel written in Rust (no Linux, `no_std`) for the Raspberry Pi 4 (BCM2711, AArch64) that implements a complete DNS server appliance.

**Current Status**: ⚠️ Core software complete (5,247 lines), VirtIO driver packet I/O debugging in progress

## Quick Status

| Component | Status | Notes |
|-----------|--------|-------|
| Build System | ✅ Working | 315 KB kernel, 0 errors, clean compile |
| Boot (QEMU) | ✅ Working | Loads and runs to main event loop |
| PL011 UART | ✅ Working | Serial output functional |
| Memory/Timer | ✅ Working | Bump allocator, ARM generic timer |
| Network Init | ✅ Working | Stack configured, sockets bound |
| VirtIO Driver | ⚠️ 80% | Detection OK, **packet I/O broken** |
| DNS Server | ✅ Complete | 259 lines, logic ready to test |
| HTTP Server | ✅ Complete | 247 lines, logic ready to test |
| Integration Tests | ❌ 0/7 Pass | **Blocked by VirtIO packet reception** |
| DHCP Client | ❌ Not Started | Deferred per plan |
| Storage (FAT32) | ❌ Not Started | Deferred per plan |
| Pi 4 Drivers | ❌ Not Started | Deferred (GENET, EMMC2) |

**Critical Blocker**: VirtIO network driver RX buffer/descriptor management needs debugging. Ports are listening (verified with `netstat`), but packets aren't reaching application layer.

**See**: [TEST_RESULTS.md](TEST_RESULTS.md) for detailed root cause analysis and [ACTUAL_TEST_OUTPUT.md](ACTUAL_TEST_OUTPUT.md) for complete test output transcripts.

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

### In Progress
- 🚧 Network stack integration (smoltcp)
- 🚧 GENET Ethernet driver for Pi 4 hardware
- 🚧 QEMU network device support for testing
- 🚧 DNS server socket handling
- 🚧 HTTP web interface
- 🚧 SD card driver (EMMC2)
- 🚧 FAT32 filesystem support
- 🚧 DHCP client

### Planned
- 📋 Complete DNS forwarding to upstream resolvers
- 📋 Persistent configuration on SD card
- 📋 Log rotation on filesystem
- 📋 Full integration tests with real DNS queries

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

# Run in QEMU with network
./scripts/run-qemu.sh

# In another terminal, test DNS and HTTP
dig @127.0.0.1 -p 5353 test.local
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

Current integration tests verify:
- Kernel boots in QEMU
- Serial output is produced
- Main event loop starts

**Planned integration tests:**
- DNS query/response via dig
- HTTP GET/POST via curl
- Configuration persistence
- DNS cache behavior

## Deployment

### QEMU (for testing)

```bash
./scripts/run-qemu.sh
```

Note: QEMU's raspi3b/raspi4b models have limited peripheral support. Network testing may require the `virt` machine type with virtio devices.

### Real Raspberry Pi 4

1. **Download Pi 4 firmware:**
   ```bash
   wget https://github.com/raspberrypi/firmware/raw/master/boot/start4.elf
   wget https://github.com/raspberrypi/firmware/raw/master/boot/fixup4.dat
   ```

2. **Prepare SD card:**
   - Format as FAT32
   - Copy `start4.elf`, `fixup4.dat`
   - Copy `boot/config.txt`
   - Copy `boot/kernel8.img`

3. **Connect serial console:**
   - USB-to-TTL adapter at 115200 baud
   - GND → Pin 6
   - TX → Pin 10 (RXD)
   - RX → Pin 8 (TXD)

4. **Connect Ethernet** to your network

5. **Power on** and watch serial output

6. **Access:**
   - DNS: Query port 53 on the configured IP
   - Web UI: Browse to `http://<ip>/` (default: admin/admin)

## Configuration

Default configuration (when filesystem is not available):

```
Network:
  IP: 192.168.1.100
  Netmask: 255.255.255.0
  Gateway: 192.168.1.1

DNS:
  Upstream: 8.8.8.8, 1.1.1.1
  Cache: 1000 entries

HTTP:
  Port: 80
  Auth: admin/admin
```

Configuration will be read from `/config.txt` on the SD card once filesystem support is complete.

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
