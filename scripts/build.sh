#!/bin/bash
set -e

echo "=== Building Pi4 DNS Server Kernel ==="

# Build the kernel
echo "Building kernel..."
cargo build --release --target aarch64-unknown-none -p pi4-dns-kernel

# Copy to standard name
echo "Creating kernel8.img..."
cp target/aarch64-unknown-none/release/kernel8 boot/kernel8.img

# Create objdump for debugging
echo "Creating kernel8.asm (disassembly)..."
aarch64-linux-gnu-objdump -d boot/kernel8.img > boot/kernel8.asm 2>/dev/null || \
    rust-objdump -d boot/kernel8.img > boot/kernel8.asm || \
    echo "Warning: Could not create disassembly (objdump not found)"

echo "Kernel size: $(stat -f%z boot/kernel8.img 2>/dev/null || stat -c%s boot/kernel8.img) bytes"
echo "Build complete! kernel8.img is in boot/"
