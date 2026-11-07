# Boot Files for Raspberry Pi 4

This directory contains the files needed to boot the DNS server kernel on a Raspberry Pi 4.

## Required Files

For a real Pi 4 SD card, you need:

1. **Firmware files** (from https://github.com/raspberrypi/firmware/tree/master/boot):
   - bootcode.bin (not needed on Pi 4, but harmless)
   - start4.elf
   - fixup4.dat

2. **Configuration**:
   - config.txt (provided here)

3. **Kernel**:
   - kernel8.img (built from this project)

## SD Card Preparation

1. Format the SD card as FAT32
2. Copy all firmware files to the root
3. Copy config.txt to the root
4. Copy the built kernel8.img to the root

The Pi 4 firmware loads kernel8.img at physical address 0x80000 and jumps to it. The kernel is linked at 0xFFFF000000080000 and turns the MMU on before using that address. This image has not been tested on hardware.

## Serial Console

Connect a USB-to-TTL serial adapter to pins:
- Pin 6 (GND)
- Pin 8 (TXD / GPIO 14)
- Pin 10 (RXD / GPIO 15)

Use 115200 baud, 8N1.
