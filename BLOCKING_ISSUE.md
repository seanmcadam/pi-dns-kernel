# Blocking Issue - VirtIO-Drivers Integration

**Date**: November 5-6, 2024  
**Time Spent**: 3+ hours  
**Status**: BLOCKED

## Summary

Attempted to integrate virtio-drivers 0.7.5 crate to fix VirtIO packet I/O. Integration compiles successfully but kernel crashes immediately during early initialization, before any meaningful output.

## What Works

✅ **Compilation**: Clean build with virtio-drivers  
✅ **Boot assembly**: Kernel loads, cores halt, BSS cleared  
✅ **GDB verification**: `kernel_main()` is reached at 0x84420  
✅ **FP/SIMD**: Enabled in boot.s (CPACR_EL1 configured)  
✅ **Raw UART**: Direct writes to 0x09000000 produce output ("ABC")  
✅ **HAL implementation**: Page-aligned DMA allocation implemented  

## What Fails

❌ **Early crash**: Kernel crashes after first UART writes  
❌ **No boot messages**: Never reaches logger or network init  
❌ **Silent failure**: No panic message, just terminates  

## Investigation Steps Taken

1. **GDB Debugging**:
   ```bash
   qemu-system-aarch64 -s -S ...
   gdb-multiarch -ex "target remote :1234" -ex "break kernel_main"
   ```
   Result: kernel_main reached, registers look normal

2. **FP/SIMD Enable**:
   Added to boot.s:
   ```asm
   mrs     x1, cpacr_el1
   orr     x1, x1, #(3 << 20)
   msr     cpacr_el1, x1
   isb
   ```

3. **Raw UART Test**:
   ```rust
   let uart_dr = 0x0900_0000 as *mut u32;
   write_volatile(uart_dr, b'A' as u32);  // Works!
   ```

4. **Incremental Testing**:
   - Bypassed network init: Still crashes
   - Removed UART driver init: Still crashes  
   - Added markers after each init step: Crashes before marker "2"

## Crash Location

Somewhere between:
- Raw UART write ("START" appears)
- Memory allocator init
- Logger init

Hypothesis: virtio-drivers uses something (heap allocation, FP ops, atomics?) during static initialization that fails.

## Files Modified

- `Cargo.toml`: Added virtio-drivers dependency
- `kernel/Cargo.toml`: Added virtio-drivers
- `kernel/src/virtio_hal.rs`: HAL implementation (NEW)
- `kernel/src/net/virtio_new.rs`: VirtIONet wrapper (NEW)
- `kernel/src/boot.s`: FP/SIMD enable
- `kernel/src/main.rs`: Debug output, network bypass
- `kernel/src/net/device.rs`: Switch to virtio_new
- `kernel/src/net/mod.rs`: VIRTIO_MMIO_BASE constant

## Possible Causes

1. **Heap exhaustion**: virtio-drivers allocates during init?
2. **Stack overflow**: Nested calls too deep?
3. **Unaligned access**: Despite page alignment in HAL?
4. **Missing CPU feature**: Beyond FP/SIMD?
5. **MMIO address wrong**: But 0x0a000000 is standard for virt
6. **Initialization order**: Static constructors run before allocator ready?

## What Would Help

1. **QEMU tracing**: `-d int,cpu_reset` to see exceptions
2. **Simpler test**: Minimal virtio-drivers usage (just transport creation)
3. **Different approach**: Use hand-written driver with proper buffer management
4. **More time**: Systematic elimination of init steps

## Recommendation

**Abandon virtio-drivers path** for now. Instead:

1. Fix hand-written VirtIO driver buffer address issue:
   - Use `Box::into_raw()` for stable addresses
   - Or use static mut arrays instead of Vec
   
2. Verify against VirtIO 1.0 spec section 2.4

3. Test with simple ping/ARP before full DNS

**Estimated**: 2-4 hours to fix hand-written driver vs unknown time to debug virtio-drivers crash.

## Code State

Current code DOES NOT BOOT. To get back to working state:
```bash
git checkout cbf5596  # Last known working commit
```

Then apply ONLY the buffer stability fix to hand-written driver.
