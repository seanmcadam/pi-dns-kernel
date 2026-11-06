# Testing Guide

This document describes the testing infrastructure and procedures for the Pi4 DNS Server kernel.

## Test Hierarchy

### 1. Unit Tests (Host-side, `cargo test --lib`)

Protocol and logic layers with full `std` support:

#### DNS Protocol (`dns-protocol`)
- **Packet parsing**: Standard queries, responses, multiple questions
- **Packet building**: Response construction, header flags
- **Name compression**: Pointer encoding/decoding, suffix sharing
- **Record types**: A, AAAA, CNAME, PTR, NS, MX, TXT

Run: `cargo test -p dns-protocol`

#### HTTP Server (`http-server`)
- **Request parsing**: GET, POST, PUT, DELETE methods
- **Response building**: Status codes, headers, body
- **Basic auth**: Base64 decoding, credential extraction

Run: `cargo test -p http-server`

#### DNS Cache (`dns-cache`)
- **Insert and retrieval**: Key-value lookups
- **TTL expiration**: Time-based invalidation
- **Eviction**: LRU-style oldest-first removal

Run: `cargo test -p dns-cache`

#### Config (`config`)
- **Parsing**: JSON/key-value configuration (minimal implementation)

Run: `cargo test -p config`

All together: `cargo test --lib`

### 2. Kernel Build (Cross-compiled, `no_std`)

Bare-metal AArch64 kernel for Pi 4:

```bash
./scripts/build.sh
```

Creates `boot/kernel8.img` (~175 KB)

**What's tested**:
- Compilation of all no_std code
- Linking with custom linker script
- Boot assembly integration
- Memory layout correctness

**What's NOT tested** (requires emulation or hardware):
- Runtime behavior
- Hardware driver interactions
- Network stack operation

### 3. QEMU Integration Tests

#### Current Status: Boot Verification

The integration test script verifies basic boot functionality:

```bash
./scripts/test-integration.sh
```

**What it checks**:
- Kernel loads in QEMU raspi3b model
- Serial output is produced
- Boot messages appear
- No immediate crashes

**Limitations**:
- QEMU raspi3b/raspi4b models have incomplete peripheral support
- Network devices (GENET) not emulated
- SD card support limited
- Cannot test DNS or HTTP functionality yet

#### Planned Integration Tests

**Phase 1: Network Stack** (requires virtio-net setup)
```bash
# Start kernel with network
qemu-system-aarch64 -M virt -kernel kernel8.img -device virtio-net-device,netdev=net0 -netdev user,id=net0,hostfwd=udp::15353-:53

# Test DNS queries (host port 15353; guest DNS stays on 53)
dig @127.0.0.1 -p 15353 example.com

# Test HTTP interface
curl http://127.0.0.1:8080/status -u admin:admin
```

**Phase 2: DNS Functionality**
- Query authoritative records
- Forwarding to upstream resolvers
- Cache hits and misses
- PTR lookups

**Phase 3: HTTP Management**
- View status page
- Edit DNS records via POST
- Configuration updates
- Authentication

**Phase 4: Filesystem** (when SD card driver is ready)
- Load config from virtual SD
- Save config changes
- Read/write logs

### 4. Hardware Testing (Raspberry Pi 4)

**Required**:
- Raspberry Pi 4 with SD card
- USB-to-TTL serial adapter (115200 baud)
- Ethernet connection
- Firmware files (start4.elf, fixup4.dat)

**Procedure**:

1. **Prepare SD card**:
   ```bash
   # Format as FAT32
   # Copy firmware + config.txt + kernel8.img
   ```

2. **Connect serial console**:
   - Pin 6 (GND)
   - Pin 8 (TXD) → Adapter RX
   - Pin 10 (RXD) → Adapter TX
   - `screen /dev/ttyUSB0 115200`

3. **Power on** and observe boot messages

4. **Test network**:
   ```bash
   # From another machine on the same network
   dig @<pi-ip> example.com
   curl http://<pi-ip>/ -u admin:admin
   ```

**What requires hardware**:
- ✅ Real GENET Ethernet driver testing
- ✅ EMMC2 SD card operations
- ✅ GIC-400 interrupt handling
- ✅ Production performance metrics
- ✅ Power and thermal behavior

**What can be tested in QEMU**:
- ✅ Boot and initialization
- ⚠️  Network stack (with virtio-net)
- ⚠️  DNS protocol handling (with virtio-net)
- ⚠️  HTTP server (with virtio-net)
- ❌ GENET driver
- ❌ SD card driver
- ❌ Real-world load

## Continuous Integration

`.github/workflows/ci.yml` runs on every push:

1. **Unit tests**: All library crates
2. **Kernel build**: Cross-compile to AArch64
3. **Integration tests**: Boot in QEMU

## Test Coverage Summary

| Component | Unit Tests | QEMU Integration | Hardware Required |
|-----------|-----------|------------------|-------------------|
| DNS Protocol | ✅ Comprehensive | N/A | N/A |
| HTTP Server | ✅ Comprehensive | N/A | N/A |
| DNS Cache | ✅ Comprehensive | N/A | N/A |
| Config Parser | ⚠️  Minimal | N/A | N/A |
| Boot Process | N/A | ✅ Verified | ✅ Recommended |
| UART Serial | N/A | ✅ Works | ✅ Works |
| Memory Allocator | N/A | ⚠️  Untested | ✅ Needs Testing |
| Network Stack | N/A | 🚧 In Progress | ✅ Required |
| DNS Server | N/A | 🚧 Planned | ✅ Required |
| HTTP Server | N/A | 🚧 Planned | ✅ Required |
| SD Card/FS | N/A | ❌ Limited | ✅ Required |
| GENET Driver | N/A | ❌ Not Emulated | ✅ Only Option |

## Known Testing Limitations

### QEMU Limitations
1. **No GENET**: Pi 4 Ethernet controller not emulated
   - Workaround: Use virt machine + virtio-net for network testing
2. **Incomplete SD**: EMMC2 not fully functional in raspi models
   - Workaround: Test filesystem logic with mock/in-memory backend
3. **No interrupts**: GIC-400 emulation incomplete
   - Impact: Polling-based I/O only

### Unit Test Limitations
1. **No hardware interaction**: Cannot test MMIO register access
2. **Different allocator**: Host uses system malloc, not bare-metal allocator
3. **Different concurrency**: Host has OS threads, kernel has single-core polling

## Adding New Tests

### Adding a Unit Test

```rust
// In protocol crate (e.g., dns-protocol/src/lib.rs)
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_new_feature() {
        // Test with full std support
        let result = my_function();
        assert_eq!(result, expected);
    }
}
```

### Adding an Integration Test

1. Update `scripts/test-integration.sh`:
   ```bash
   # Add new test case
   echo "Testing feature X..."
   timeout 5 nc 127.0.0.1 4444 > qemu/test-x.txt &
   # ... verify output
   ```

2. Update CI workflow if needed

### Adding a Hardware Test

Document in this file under "Hardware Testing Procedures" with:
- Required setup
- Step-by-step procedure
- Expected output
- Known issues

## Troubleshooting

### Unit tests fail
- Check that all dependencies have `std` feature enabled
- Verify `#[cfg(test)]` guards are correct
- Run `cargo clean` and retry

### Kernel build fails
- Ensure `aarch64-unknown-none` target is installed
- Check that `link.ld` is in `kernel/` directory
- Verify no `std` usage in kernel code

### QEMU doesn't boot
- Check kernel size is reasonable (not 0 bytes)
- Verify boot messages in serial output
- Try different QEMU machine types: `raspi3b`, `raspi4b`, `virt`

### Hardware doesn't boot
- Verify firmware files are correct versions
- Check SD card formatting (FAT32)
- Confirm serial console settings (115200, 8N1)
- Ensure `arm_64bit=1` in config.txt
