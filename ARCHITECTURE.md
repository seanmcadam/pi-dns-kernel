# Architecture Documentation

## Current address space

The kernel is linked in the upper half and runs from TTBR1. QEMU loads the raw image at physical `0x40080000` and the link address is `0xFFFF000040080000`. The Pi 4 firmware loads the raw image at physical `0x80000` and the link address is `0xFFFF000000080000`. That Pi 4 image has not been tested on hardware. Bits 47:0 of a kernel virtual address equal the physical address.

`boot.s` drops EL2 to EL1 when it entered at EL2, builds a 1 GiB block map, turns the MMU on, jumps to the high PC, and clears TTBR0. Rust then installs a strict TTBR1 map: kernel text is read-only and executable, rodata is read-only, data is not executable, `.dma` is Normal non-cacheable, devices are Device-nGnRE, and one guard page under the stack is unmapped. Devices are given physical addresses. The CPU uses the high alias.

TTBR0 holds only the EL0 window, in its own page tables:

- code `0x1_0000_0000`, 512 KiB, EL0 read/execute
- a gap, then data `0x1_0009_0000`, 64 KiB
- heap `0x1_000A_0000`, 1536 KiB
- stack `0x1_0022_0000`, 64 KiB

User pages are non-global. The boot test and the DNS task use ASID 1. The HTTP task uses ASID 2. The spinner uses ASID 3. The log task uses ASID 4. Threads of one process share that process's TTBR0 root and ASID. A switch between those threads does not flush the TLB. A map change uses `tlbi vmalle1is` and `dsb ish`, so the maintenance is inner-shareable and reaches every core. EL0 cannot walk TTBR1, so a user access to kernel memory faults and kills that task. The DNS resolver, the HTTP dashboard, and the log task each run in this window, in separate page tables. The kernel keeps the NIC, the smoltcp sockets, storage, and the config file. A task yields when a socket would block. A timer IRQ at EL0 also returns to the poll loop after two 10 ms ticks, and that loop polls the NIC before running the next task. The published tick count is core 0's 100 Hz counter. The other cores arm their own virtual timers and preempt, and they do not add to that counter.

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
| 34 | `thread_create(entry, stack, arg)` |
| 35 | `thread_exit(code)` |
| 36 | `thread_join(tid)` |
| 37 | `futex_wait(addr, expected)` |
| 38 | `futex_wake(addr, n)` |
| 39 | `log_publish(ptr, len)` |
| 40 | `log_follow(cursor, buf, cap, drops)` |
| 41 | `log_wake()` |
| 42 | `debug_status(buf, cap)` |
| 43 | `debug_crash(slot, worker)` |
| 44 | `thread_try_join(tid)` |
| 45 | `log_reload` |
| 46 | `config_read(which, buf, cap)` |
| 47 | `config_write(which, buf, len, err, err_cap)` |
| 48 | `log_fill(n)` |
| 49 | `config_set_cache(slots, secs, err, err_cap)` |
| 50 | `config_cache_reap` |
| 51 | `timer_ticks` |
| 52 | `timer_freq` |
| 53 | `set_cache_stats(ptr)` |
| 54 | `cache_plant(op, ttl)` |

`log_publish` is the log task only. It copies one line into a kernel ring and returns. It does not wait. `log_follow` copies the next line for that cursor and writes the new cursor and how many lines were skipped. It sleeps until a line is published, unless its fifth argument is `1`, in which case it returns `0`. `tcp_recv` takes a fourth argument: `1` returns `-11` instead of sleeping when the peer has not sent data. Anything else returns `-1`. `-2` is a bad user pointer. `-4` is a bad argument. `-6` means the thread table is full. `-11` means try again. `thread_create` checks that `entry` is a 4-byte-aligned address in the user code window and that the top 256 bytes of `stack` are writable and 16-byte aligned. `futex_wait` and `futex_wake` require a 4-byte-aligned user address. The kernel re-reads the word under the scheduler lock, so a wake that lands before the wait returns a mismatch and the caller retries. The older sections below describe earlier revisions of this tree.

QEMU is started with `-smp 4`. Core 0 brings the other three up with PSCI `CPU_ON` (`0xC4000003`) through HVC. This QEMU virt machine's PSCI conduit is HVC; SMC from EL1 is undefined because EL3 is not implemented. When the CPU implements EL2 (`-M virt,virtualization=on`), that HVC is delivered to EL2 instead of the firmware, and the extra cores stay offline. The default virt machine does not implement EL2, so all four cores come up. Each core has its own stack, its own virtual timer, and its own GICv2 CPU interface. `TPIDR_EL1` points at that core's current thread. The Pi 4 build does not use PSCI. It writes the physical entry to the spin-table release addresses `0xE0`, `0xE8`, and `0xF0` (core n at `0xD8 + 8*n`), cleans the line, and sends an event. That path is untested on hardware. If the firmware stub is not parked on those words, the stores hit RAM and the extra cores stay offline.

Lock order, outermost first. A holder may take a later lock and must not take an earlier one.

1. `SCHED` — run queue, thread state, futex waiters, join waiters, and socket sleepers. IRQ-safe. The timer IRQ does not take it.
2. `FD` — one lock per EL0 task, for that task's file descriptors. Not IRQ-safe. A socket syscall drops it before sleeping.
3. `CONFIG` — config records. Not IRQ-safe.
4. `FS` — the FAT volume. One lock, any core. Not IRQ-safe.
5. `LOGQ` — the log queue and the recent-log buffer. Not IRQ-safe.

The telnet fan-out is not a lock. The log task publishes each line once into a fixed ring. A reader that falls behind skips ahead. Waking a reader takes `SCHED` and no other lock. The supervisor lock is taken alone and dropped before any lock in this list. When the log task is down, a crash line is written to the UART and the fan directly so a telnet reader is not left waiting on that task.

smoltcp and the NIC are not behind a lock. Core 1 owns them once the stack is up. It sleeps in `wfi` until the virtio used-ring IRQ, an SGI from a core that queued a socket job, or the timer, which is also how smoltcp `poll_delay` is serviced. If none of those are pending it runs one user thread, so network work stays ahead of user work and the core is not busy-polling. Core 0 owns the NIC until that handoff, and on a one-core boot, and drives on the same conditions from its loop. Other cores push a socket request onto a lock-free ring, send SGI 1 to the owner, and sleep. The owner copies the result into the caller's user buffer and wakes the thread. It may take `FD` to publish a new socket, then drop it, then take `SCHED`. `FS` stays one lock because the FAT library is not reentrant and the block device has one command slot. The virtio-blk waiter holds the disk mutex and sleeps on `wfe`; the IRQ handler does not take that mutex or any lock above. It acknowledges the device and the GIC and stores an atomic. A kick that arrives after the idle check is not lost: the owner rechecks with IRQs masked, and `wfi` returns while the interrupt is still pending. The timer IRQ takes none of these locks. The UART and the panic path take no locks.

QEMU virt (GICv2) maps virtio-mmio slot `n` to SPI `16 + n`, INTID `48 + n`. QEMU 8.2 plugs `-device virtio-*-device` into the high slots, so with a NIC and a block device the measured lines are INTID 79 (SPI 47) and INTID 78 (SPI 46). Both are level-triggered; the handler writes the virtio-mmio interrupt-ack register before EOI. The NIC line is routed at the owner and moved to core 1 when that core claims the stack. The block line targets every online CPU, and the handler sends an event so the waiter wakes on whichever core holds the disk mutex. The `cntpct` timeout remains the fallback.

On the Pi 4 the GIC-400 lines are GENET SPI 157 and 158 (INTID 189 and 190) and EMMC2 SPI 126 (INTID 158). EMMC waits sleep on `wfe` with the same half-second `cntpct` cap, and the handler masks the line so a stuck level cannot spin. GENET's lines are recognized and, if they fire, masked after one count. They are left disabled because the device INTRL2 mask is not programmed and has not been tested. GENET packet completion is observed on the timer or socket-queue wake. None of the Pi 4 IRQ path has been run on hardware.

The DNS task keeps two threads blocked in the upstream receive, one blocked on TCP port 53, and one that sleeps until a query is forwarded. The HTTP task keeps four connection workers, one per listener. Extra threads share the process page tables and sit on one global run queue.

## Overview

This is a bare-metal operating system kernel written in Rust for the Raspberry Pi 4, implementing a DNS server appliance. The design prioritizes simplicity, testability, and clear separation of concerns.

## Design Principles

1. **Bare Metal**: No Linux, no standard library in kernel code
2. **Layered Architecture**: Clear separation between hardware, network, and application
3. **Testability**: Protocol layers testable on host with `std`
4. **Type Safety**: Leverage Rust's type system for memory and concurrency safety
5. **Minimal Complexity**: Simple allocator, interrupt-driven device I/O

## Component Architecture

### Boot Layer

**Location**: `kernel/src/boot.s`, `kernel/src/main.rs`

The boot process:
1. The loader copies the raw image to a physical address: `0x40080000` on QEMU virt, `0x80000` on the Pi 4, `0x200000` on the Pi 5
2. Assembly code (`_start`) runs on core 0. Cores that enter `_start` instead of the secondary entry park. QEMU starts the other cores later, at `secondary_entry`, through PSCI
3. Sets the stack at that physical address, drops EL2 to EL1 when needed, and zeroes BSS
4. Turns the MMU on and jumps to the upper-half link address
5. Calls Rust `kernel_main()`

The stack grows down from the load address. The QEMU link script is `kernel/src/board/virt/link.ld`. The Pi 4 link script is `kernel/src/board/pi4/link.ld`. The Pi 5 link script is `kernel/src/board/pi5/link.ld` (`0xFFFF000000200000`). `kernel/build.rs` copies the one selected by the Cargo feature into the link search path.

Board-specific code is `kernel/src/board`. `board-virt` is the default feature. `board-pi4` is selected with `--no-default-features --features board-pi4` (`./scripts/build.sh pi4` or `./scripts/build-pi4.sh`). `board-pi5` is selected with `--no-default-features --features board-pi5` (`./scripts/build.sh pi5` or `./scripts/build-pi5.sh`). The scheduler, syscalls, network owner, filesystem, logger, and EL0 services call `crate::board` and do not name a feature. The board supplies the memory map, UART base, GIC bases and the timer INTID, the NIC, the block device, secondary-core startup, the link script, and the default network config.

### Pi 5 (BCM2712)

The Pi 5 board is Cortex-A76, GICv2 (GIC-400), and the generic timer at PPI 11. `boot.s` widens `TCR_EL1.IPS` to 40-bit physical addresses so the early map can cover the SoC window at `0x10_0000_0000`. That window is two 1 GiB device blocks in the early tables and 2 MiB blocks in the strict map. The extra level-2 tables used for addresses above 4 GiB are not used by virt or Pi 4. RAM above 4 GiB is not mapped.

`probe_nic` returns none. RP1 Ethernet is a Cadence GEM behind PCIe controller 2 (`pcie2` at `0x1000120000`, outbound window `0x1f_00000000`). Bringing that up means link training, an ECAM scan, the RP1 BAR, and a MACB driver. None of that is in this image. The missing NIC is non-fatal: DNS, HTTP, and telnet stay down, and the log task still starts.

The SD driver talks to `sdio1` at `0x1000fff000` (cfg block `0x1000fff400`, IRQ 305). The first read is `read_u32_catching`, so an external abort on a missing window fails the probe. Card power and card detect are `gio_aon` GPIOs and are not programmed. Secondary cores use PSCI `CPU_ON` with SMC and affinity `cpu << 8`. `el2_blocks_secondaries` is false because SMC is not delivered to EL2.

QEMU cannot boot this image. The integration stage checks the link address and then boots the virt image with `-cpu cortex-a76`. See the README section "Real Raspberry Pi 5" for the address table, the sources, and the hardware risks.

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
- Fixed slots reserved at startup (default 1024, `cache_size` 1..=1024)
- Free list, hash chains on a fixed bucket array, LRU eviction
- Reaper thread, interval `cache_reap_interval_secs` (1..=3600, default 5)
- Expiry is an absolute `CNTPCT` value

**Implementation**:
- Each slot is 800 bytes: 255-byte name, type, class, 512-byte response, links
- 1024 slots plus 1024 buckets reserve 823296 bytes
- No allocation after `DnsCache::new`
- Replies that do not fit, and names over 255 bytes, are not stored

**Testing**: unit tests for collisions, free-list reuse, expiry, and LRU eviction

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

**Virt machine**: `-M virt,gic-version=2`. This is the integration suite. virtio-net and virtio-blk are the devices.

**raspi4b** (QEMU 9.0 or newer): `-M raspi4b -smp 4 -kernel boot/pi4/kernel8.img` plus `-drive if=sd`. The integration script runs this as an extra stage and skips with a message when the machine is missing. QEMU 8.2 does not have it. The stage checks the UART, the upper-half MMU, four cores, a FAT `CONFIG.TXT`, and the EL0 log task. raspi4b does not emulate GENET, so the kernel continues without DNS, HTTP, or telnet. The SD card is attached to the legacy Arasan at `0xFE300000`, not to EMMC2; the driver tries EMMC2 first (the real Pi 4 slot) and then that legacy controller. None of this has been run on Pi 4 hardware.

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
