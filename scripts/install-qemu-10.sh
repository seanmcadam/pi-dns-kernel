#!/usr/bin/env bash
# Install QEMU 10.2.4 for aarch64 without removing the apt package.
# The binary is placed in /opt and linked from /usr/local/bin, which is
# ahead of /usr/bin on the default PATH. A second run does nothing when
# that version is already the one on /usr/local/bin.

set -euo pipefail

VERSION=10.2.4
PREFIX="/opt/qemu-${VERSION}"
SHA256=821b545b92f165e57dddccac5077d76d4d436a226595b8813ad59306bbfd0746
FINGERPRINT=CEACC9E15534EBABB82D3FA03353C9CEF108B584
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY="${ROOT}/scripts/qemu-10.2.4-release.asc"
LINK="/usr/local/bin/qemu-system-aarch64"

version_of() {
    "$1" --version 2>/dev/null | head -n 1
}

if [ -x "$LINK" ] && version_of "$LINK" | grep -F -q "QEMU emulator version ${VERSION}"; then
    echo "QEMU ${VERSION} already installed at ${LINK}"
    version_of "$LINK"
    exit 0
fi

if [ -x "${PREFIX}/bin/qemu-system-aarch64" ] \
    && version_of "${PREFIX}/bin/qemu-system-aarch64" | grep -F -q "QEMU emulator version ${VERSION}"; then
    sudo ln -sfn "${PREFIX}/bin/qemu-system-aarch64" "$LINK"
    echo "Linked existing ${PREFIX} build"
    version_of "$LINK"
    exit 0
fi

sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    build-essential \
    ca-certificates \
    curl \
    flex \
    bison \
    gnupg \
    ninja-build \
    pkg-config \
    python3 \
    python3-venv \
    python3-pip \
    xz-utils \
    zlib1g-dev \
    libglib2.0-dev \
    libpixman-1-dev \
    libfdt-dev \
    libslirp-dev

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
curl -fsSL --retry 3 -o "${WORK}/qemu.tar.xz" "https://download.qemu.org/qemu-${VERSION}.tar.xz"
curl -fsSL --retry 3 -o "${WORK}/qemu.tar.xz.sig" "https://download.qemu.org/qemu-${VERSION}.tar.xz.sig"
echo "${SHA256}  ${WORK}/qemu.tar.xz" | sha256sum -c -

GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
gpg --batch --import "$KEY" >/dev/null
STATUS="$(gpg --batch --status-fd 1 --verify "${WORK}/qemu.tar.xz.sig" "${WORK}/qemu.tar.xz" 2>/dev/null || true)"
printf '%s\n' "$STATUS" | grep -F -q "GOODSIG 3353C9CEF108B584 "
printf '%s\n' "$STATUS" | grep -F -q "VALIDSIG ${FINGERPRINT} "
rm -rf "$GNUPGHOME"
unset GNUPGHOME

tar -C "$WORK" -xf "${WORK}/qemu.tar.xz"
SRC="${WORK}/qemu-${VERSION}"
python3 -m venv "${WORK}/meson-venv"
"${WORK}/meson-venv/bin/pip" install -q 'meson>=1.5,<1.10'
export PATH="${WORK}/meson-venv/bin:${PATH}"

mkdir -p "${WORK}/build"
(
    cd "${WORK}/build"
    "${SRC}/configure" \
        --prefix="$PREFIX" \
        --target-list=aarch64-softmmu \
        --enable-slirp \
        --enable-fdt=system \
        --disable-docs \
        --disable-gtk \
        --disable-sdl \
        --disable-vnc \
        --disable-opengl \
        --disable-werror
    ninja -j"$(nproc)"
    sudo ninja install
)

sudo ln -sfn "${PREFIX}/bin/qemu-system-aarch64" "$LINK"
version_of "$LINK"
"$LINK" -machine help | grep -F -q 'raspi4b '
echo "QEMU ${VERSION} installed"
