# Architecture Documentation

## Current address space

The kernel is linked in the upper half and runs from TTBR1. QEMU loads the raw image at physical `0x40080000` and the link address is `0xFFFF000040080000`. The Pi 4 firmware loads the raw image at physical `0x80000` and the link address is `0xFFFF000000080000`. That Pi 4 image has not been tested on hardware. Bits 47:0 of a kernel virtual address equal the physical address.

`boot.s` drops EL2 to EL1 when it entered at EL2, builds a 1 GiB block map, turns the MMU on, jumps to the high PC, and clears TTBR0. Rust then installs a strict TTBR1 map: kernel text is read-only and executable, rodata is read-only, data is not executable, `.dma` is Normal non-cacheable, devices are Device-nGnRE, and one guard page under the stack is unmapped. Devices are given physical addresses. The CPU uses the high alias.

TTBR0 holds only the EL0 window, in its own page tables:

- code `0x1_0000_0000`, 512 KiB, EL0 read/execute
- a gap, then data `0x1_0009_0000`, 64 KiB
- heap `0x1_000A_0000`, 1536 KiB
- stack `0x1_0022_0000`, 64 KiB

User pages are non-global. The boot test and the DNS task use ASID 1. The HTTP task uses ASID 2. The spinner uses ASID 3. The log task uses ASID 4. EL0 cannot walk TTBR1, so a user access to kernel memory faults and kills that task. The DNS resolver, the HTTP dashboard, and the log task each run in this window, in separate page tables. The kernel keeps the NIC, the smoltcp sockets, storage, and the config file. A task yields when a socket would block. A timer IRQ at EL0 also returns to the poll loop after two 10 ms ticks, and that loop polls the NIC before running the next task.

Syscall numbers are in `x8`, arguments in `x0`–`x5`, and the result in `x0`:

| x8 | Call |
|----|------|
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
| 10 | `tcp_recv(fd, buf, cap)` |
| 11 | `tcp_send(fd, buf, len)` |
| 12 | `tcp_close(fd)` |
| 13 | `config_records(buf, cap)` |
| 14 | `config_upstreams(buf, cap)` |
| 15 | `config_blocklist(buf, cap)` |
| 16 | `config_cache_size` |
| 17 | `set_cache_entries(n)` |
| 18 | `auth(user, user_len, pass, pass_len)` |
| 19 | `config_add(buf, len)` |
| 20 | `config_remove(buf, len)` |
| 21 | `uptime_secs` |
| 22 | `cache_entries` |
| 23 | `fault_guard` |
| 24 | `fault_text` |
| 25 | `http_info(buf, cap)` |
| 26 | `tick_hz` |
| 27 | `debug_spin` |
| 28 | `log_post(ptr, len)` |
| 29 | `log_drain(buf, cap)` |
| 30 | `log_emit(ptr, len)` |
| 31 | `log_recent(buf, cap)` |
| 32 | `file_read(name, name_len, buf, cap)` |
| 33 | `file_write(name, name_len, buf, len, flags)` |

Anything else returns `-1`. `-2` is a bad user pointer. `-11` means try again. The older sections below describe earlier revisions of this tree.

## Overview

This is a bare-metal operating system kernel written in Rust for the Raspberry Pi 4, implementing a DNS server appliance. The design prioritizes simplicity, testability, and clear separation of concerns.

## Design Principles

1. **Bare Metal**: No Linux, no standard library in kernel code
2. **Layered Architecture**: Clear separation between hardware, network, and application
3. **Testability**: Protocol layers testable on host with `std`
4. **Type Safety**: Leverage Rust's type system for memory and concurrency safety
5. **Minimal Complexity**: Simple allocator, polling-based I/O

## Component Architecture

### Boot Layer

**Location**: `kernel/src/boot.s`, `kernel/src/main.rs`

The boot process:
1. The loader copies the raw image to a physical address: `0x40080000` on QEMU virt, `0x80000` on the Pi 4
2. Assembly code (`_start`) runs on core 0 and parks the other cores
3. Sets the stack at that physical address, drops EL2 to EL1 when needed, and zeroes BSS
4. Turns the MMU on and jumps to the upper-half link address
5. Calls Rust `kernel_main()`

The stack grows down from the load address. The QEMU link script is `kernel/link.ld`. The Pi 4 link script is `kernel/link-pi4.ld`.

### System Layer

#### Memory Allocator (`kernel/src/memory.rs`)

- **Type**: Bump allocator (monotonic)
- **Size**: 1 MB heap
- **Characteristics**:
  - Never deallocates (acceptable for long-lived server)
  - Simple and fast
  - Suitable for steady-state allocation patterns
- **Future**: Consider linked-list or buddy allocator for deallocation

#### UART Driver (`kernel/src/uart.rs`)

- **Hardware**: Mini UART (UART1) at `0xFE215040`
- **Purpose**: Debug logging and early boot output
- **Configuration**: 115200 baud, 8N1
- **Implementation**: Memory-mapped I/O with tock-registers

#### Timer (`kernel/src/timer.rs`)

- **Hardware**: System Timer at `0xFE003000`
- **Resolution**: 1 microsecond
- **Usage**: Delays and smoltcp timestamp source

#### Interrupts (`kernel/src/interrupts.rs`)

- **Status**: Disabled (polling mode only)
- **Hardware**: GIC-400 (not yet configured)
- **Future**: Enable for efficient network I/O

### Network Layer

#### Device Abstraction (`kernel/src/net/device.rs`)

Two device types:
1. **QEMU**: Placeholder for virtio-net (testing)
2. **GENET**: BCM2711 Ethernet (Pi 4 hardware, not yet implemented)

**Trait**: `smoltcp::phy::Device`
- `receive()`: Get incoming packets
- `transmit()`: Send outgoing packets
- `capabilities()`: MTU and medium type

**Current Status**: Stub implementation, no actual packet I/O

#### Network Stack (`kernel/src/net/stack.rs`)

- **Implementation**: `smoltcp` TCP/IP stack
- **Features**:
  - IPv4 and IPv6
  - TCP, UDP, raw sockets
  - DHCP client (planned)
  - Ethernet medium
- **Configuration**:
  - Static IP: Direct configuration
  - DHCP: Automatic address assignment (fallback to static if unavailable)

**Main Loop** (`net::poll()`):
- Called from `kernel_main()` event loop
- Processes network packets
- Drives smoltcp state machine

### Protocol Layer

These crates are `no_std`-compatible but have `std` feature for testing.

#### DNS Protocol (`dns-protocol`)

**Features**:
- Full packet parser and builder
- Supported record types: A, AAAA, CNAME, PTR, NS, MX, TXT
- Name compression (RFC 1035)
- Query and response handling

**Key Types**:
- `DnsPacket`: Complete DNS message
- `Question`: Query section
- `ResourceRecord`: Answer/authority/additional sections
- `RData`: Record data by type

**Testing**: 12 unit tests covering parsing, building, compression

#### HTTP Server (`http-server`)

**Features**:
- HTTP/1.1 request parsing
- Response builder with status codes
- Basic authentication (RFC 7617)

**Key Types**:
- `HttpRequest`: Method, path, headers, body
- `HttpResponse`: Status, headers, body
- `parse_basic_auth()`: Base64 credential extraction

**Testing**: 5 unit tests covering GET/POST, auth, responses

#### DNS Cache (`dns-cache`)

**Features**:
- In-memory cache with TTL
- LRU-style eviction
- Configurable size limit

**Implementation**:
- `Vec` storage (no hash map to minimize dependencies)
- Linear scan for lookups (acceptable for small caches)
- Time-based expiration

**Testing**: 3 unit tests covering insert, expiry, eviction

### Application Layer

#### DNS Server (`kernel/src/dns/`)

**Status**: Initialization only

**Planned Features**:
- UDP socket on port 53
- Authoritative answers for local records
- Forwarding to upstream resolvers
- Cache integration
- Query logging

**Request Flow**:
```
1. Receive UDP packet on port 53
2. Parse DNS query (dns-protocol)
3. Check local records
4. If not found, check cache
5. If cache miss, forward to upstream
6. Build response (dns-protocol)
7. Send UDP packet
8. Log query
```

#### HTTP Server (`kernel/src/http/`)

**Status**: Initialization only

**Planned Features**:
- TCP socket on port 80 (configurable)
- Management web interface
- Basic authentication
- REST API for configuration

**Endpoints** (planned):
- `GET /`: Dashboard
- `GET /status`: System status JSON
- `GET /logs`: Recent queries
- `GET /config`: Current configuration
- `POST /config`: Update configuration
- `GET /records`: DNS records
- `POST /records`: Add/modify records
- `DELETE /records/:name`: Remove record

#### Configuration Manager (`kernel/src/config_manager.rs`)

**Current**: In-memory default configuration

**Planned**: Load from SD card filesystem

**Configuration Structure**:
```rust
Config {
    network: {
        mode: Static { ip, netmask, gateway } | Dhcp
    },
    dns: {
        records: Vec<DnsRecord>,
        upstreams: Vec<Ipv4Addr>,
        cache_size: usize,
    },
    http: {
        port: u16,
        username: String,
        password: String,
    }
}
```

#### Filesystem (`kernel/src/fs/`)

**Status**: Not implemented

**Planned**:
- EMMC2 SD card driver (SPI or native mode)
- FAT32 filesystem (via `embedded-sdmmc`)
- Configuration file: `/config.txt`
- Log files: `/logs/queries.log`, `/logs/system.log`
- Log rotation: Size-based or daily

## Data Flow Examples

### DNS Query Resolution

```
Network (UDP) → smoltcp → dns::server
                             ↓
                    dns_protocol::parse()
                             ↓
                    Check local records
                             ↓
                    Check cache (dns-cache)
                             ↓
                    Forward to upstream
                             ↓
                    Cache response
                             ↓
                    dns_protocol::build()
                             ↓
                    smoltcp → Network (UDP)
```

### HTTP Configuration Update

```
Network (TCP) → smoltcp → http::server
                             ↓
                    http_server::parse()
                             ↓
                    Verify authentication
                             ↓
                    Update config_manager
                             ↓
                    Save to filesystem
                             ↓
                    http_server::build()
                             ↓
                    smoltcp → Network (TCP)
```

### Boot Sequence

```
Power On
  ↓
Firmware loads kernel8.img
  ↓
boot.s: Initialize stack, clear BSS
  ↓
kernel_main()
  ↓
├─ Init UART
├─ Init memory allocator
├─ Init logger
├─ Init timer
├─ Init interrupts (stub)
├─ Init filesystem (stub)
├─ Load config
├─ Init network stack
├─ Start DNS server
├─ Start HTTP server
  ↓
Main event loop: net::poll()
```

## Concurrency Model

**Single-threaded, cooperative**:
- No preemption
- No interrupts (currently)
- Polling-based I/O
- Event loop in `kernel_main()`

**Future**: Enable interrupts for:
- Network packet arrival
- Timer events
- UART receive

## Error Handling

**Kernel**: Panic on unrecoverable errors
- Memory allocation failure
- Assertion violations
- Unexpected hardware state

**Applications**: Result types for recoverable errors
- Network errors: Log and continue
- Parse errors: Send error response
- Configuration errors: Use defaults

## Security Considerations

**Current**:
- Basic authentication on HTTP interface
- No TLS/HTTPS
- No DNS security (DNSSEC)
- No rate limiting

**Future**:
- HTTPS via embedded-tls
- DNSSEC validation
- Query rate limiting
- DDoS mitigation

## Performance Characteristics

**Expected**:
- Query latency: <10ms (cached), <100ms (forwarded)
- Throughput: ~1000 queries/sec (limited by single-threaded polling)
- Memory: ~2MB runtime (1MB heap + code + stack)

**Optimization Opportunities**:
- Enable interrupts
- Use DMA for network
- Optimize smoltcp buffer sizes
- Cache preloading

## Platform-Specific Details

### Raspberry Pi 4 (BCM2711)

**CPU**: Cortex-A72 (ARMv8-A, 64-bit)
**Memory**: 1GB+ DRAM (depends on model)
**Peripherals**:
- Mini UART: 0xFE215040
- System Timer: 0xFE003000
- GENET Ethernet: 0xFD580000
- EMMC2: 0xFE340000
- GIC-400: 0xFF840000

**Addresses**: Low peripheral mode (0xFE000000 base)

### QEMU Testing

**Machine**: `raspi3b` (better support than `raspi4b`)
**Limitations**:
- No GENET emulation
- Limited EMMC support
- Use `virt` machine with virtio-net for network testing

## Dependencies

**Kernel** (`no_std`):
- `cortex-a`: ARM CPU control
- `aarch64-cpu`: AArch64 instructions
- `tock-registers`: MMIO register access
- `smoltcp`: TCP/IP stack
- `spin`: Spinlock for allocator
- `log`: Logging facade
- `heapless`: Fixed-capacity containers
- `embedded-sdmmc`: FAT32 filesystem

**Protocol Libraries** (testable with `std`):
- `dns-protocol`, `http-server`, `dns-cache`, `config`: No dependencies

**Build**:
- `cc`: Compile boot assembly

## Future Enhancements

### Short Term
1. Complete QEMU network testing (virtio-net)
2. Implement UDP socket handling for DNS
3. Wire up DNS server logic
4. Basic HTTP endpoints

### Medium Term
1. GENET Ethernet driver
2. SD card driver (EMMC2)
3. FAT32 integration
4. Configuration persistence

### Long Term
1. DNSSEC support
2. HTTPS (embedded-tls)
3. DHCPv6
4. IPv6 SLAAC
5. Systemd-resolved compatible API
6. Prometheus metrics endpoint
