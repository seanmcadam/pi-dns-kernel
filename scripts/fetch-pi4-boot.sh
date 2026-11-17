#!/bin/bash
# Stage a matched Pi 4 boot set into boot/pi4/card/.
#
# Downloads start4.elf, fixup4.dat, and bcm2711-rpi-4-b.dtb from
# raspberrypi/firmware commit 80f23d4be5 (19 Mar 2025). That commit's
# bytes match the Raspberry Pi OS card that boots this board:
# start4.elf 2263840 bytes, VC_BUILD_ID ca6e8171 (built Mar 19 2025),
# fixup4.dat 5459 bytes, bcm2711-rpi-4-b.dtb 56211 bytes.
# Current master (d131b57900, VC_BUILD_ID fed420a1) is a different
# matched pair and is not what this board's working card uses.
#
# The better set for a board that already boots Raspberry Pi OS is the
# same three files from that card. They match the EEPROM. The script
# prints the copy commands. Do not copy the Pi OS config.txt, cmdline.txt,
# kernel8.img, or overlays/.
set -euo pipefail
cd "$(dirname "$0")/.."

# Public commit whose boot files match the working card, not VC_BUILD_ID.
REV=80f23d4be5
BASE="https://raw.githubusercontent.com/raspberrypi/firmware/${REV}/boot"
DEST=boot/pi4/card
KERNEL=""

if [ $# -gt 1 ]; then
    echo "usage: $0 [blinktest|kernel]" >&2
    exit 1
fi
if [ $# -eq 1 ]; then
    case "$1" in
        blinktest|kernel) KERNEL="$1" ;;
        *)
            echo "usage: $0 [blinktest|kernel]" >&2
            exit 1
            ;;
    esac
fi

mkdir -p "$DEST"

fetch() {
    local name="$1"
    echo "fetch $REV $name"
    curl -fL --retry 3 --retry-delay 2 -o "$DEST/$name.partial" "$BASE/$name"
    mv "$DEST/$name.partial" "$DEST/$name"
}

fetch start4.elf
fetch fixup4.dat
fetch bcm2711-rpi-4-b.dtb

python3 - "$DEST" <<'PY'
import pathlib, sys
dest = pathlib.Path(sys.argv[1])
elf = (dest / "start4.elf").read_bytes()
fix = (dest / "fixup4.dat").read_bytes()
dtb = (dest / "bcm2711-rpi-4-b.dtb").read_bytes()
if len(elf) != 2_263_840:
    raise SystemExit(f"start4.elf is {len(elf)} bytes, want 2263840")
if len(fix) != 5_459:
    raise SystemExit(f"fixup4.dat is {len(fix)} bytes, want 5459")
if len(dtb) != 56_211 or dtb[:4] != b"\xd0\x0d\xfe\xed":
    raise SystemExit(f"bcm2711-rpi-4-b.dtb is {len(dtb)} bytes, want 56211 and FDT magic")
if b"ca6e8171a80ea46924ffaa629250bfb482f3a02c" not in elf:
    raise SystemExit("start4.elf is not VC_BUILD_ID ca6e8171")
print("start4.elf 2263840 bytes, fixup4.dat 5459 bytes, dtb 56211 bytes")
PY

cp boot/pi4/config.txt "$DEST/config.txt"

if [ "$KERNEL" = "blinktest" ]; then
    ./scripts/build-pi4-blinktest.sh
    cp boot/pi4-blinktest/kernel8.img "$DEST/kernel8.img"
    cp boot/pi4-blinktest/config.txt "$DEST/config.txt"
elif [ "$KERNEL" = "kernel" ]; then
    if [ ! -s boot/pi4/kernel8.img ]; then
        echo "boot/pi4/kernel8.img is missing. Run ./scripts/build-pi4.sh first." >&2
        exit 1
    fi
    cp boot/pi4/kernel8.img "$DEST/kernel8.img"
    cp boot/pi4/config.txt "$DEST/config.txt"
fi

echo
echo "Staged in $DEST from firmware commit $REV (VC_BUILD_ID ca6e8171):"
ls -l "$DEST"
echo
echo "To match the EEPROM that already boots Raspberry Pi OS, overwrite the"
echo "three binaries from that card's boot FAT (Bookworm label bootfs):"
echo "  cp /path/to/pi-os-boot/start4.elf $DEST/"
echo "  cp /path/to/pi-os-boot/fixup4.dat $DEST/"
echo "  cp /path/to/pi-os-boot/bcm2711-rpi-4-b.dtb $DEST/"
echo
echo "Copy the staged directory onto the bare-metal boot FAT, then sync:"
echo "  cp $DEST/* /path/to/bare-boot/"
echo "  sync"
