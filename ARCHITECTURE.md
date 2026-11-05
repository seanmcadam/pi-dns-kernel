# Architecture Documentation

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
1. Pi 4 firmware loads `kernel8.img` at `0x80000`
2. Assembly code (`_start`) runs on core 0, halts cores 1-3
3. Sets stack pointer at `0x80000`
4. Clears BSS section
5. Jumps to Rust `kernel_main()`

**Memory Layout** (`link.ld`):
```
0x00000 - 0x80000: Reserved (firmware, etc.)
0x80000 - ...:     Kernel code (.text, .rodata, .data)
...:              BSS (uninitialized data)
...:              Heap (1 MB bump allocator)
```

Stack grows downward from `0x80000`.

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
