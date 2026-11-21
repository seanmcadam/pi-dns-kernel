//! Firmware-update checks that do not touch a board.
//!
//! The kernel streams bytes to a temp file, then calls [`commit`]. The
//! live kernel and `config.txt` are not destinations this crate will
//! publish. A simulated FAT volume in the tests is the stand-in for the
//! boot partition.

#![cfg_attr(not(feature = "std"), no_std)]

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Running SHA-256. `Copy` so a caller can finish a snapshot while the
/// transfer is still open.
#[derive(Clone, Copy)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    len: u64,
    nbuf: usize,
}

impl Sha256 {
    pub const fn new() -> Self {
        Self {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            len: 0,
            nbuf: 0,
        }
    }

    pub fn hash(data: &[u8]) -> [u8; 32] {
        let mut sha = Self::new();
        sha.update(data);
        sha.finish()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        self.ingest(data);
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.len.wrapping_mul(8);
        self.ingest(&[0x80]);
        while self.nbuf != 56 {
            self.ingest(&[0]);
        }
        self.ingest(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 8 {
            out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_be_bytes());
            i += 1;
        }
        out
    }

    fn ingest(&mut self, mut data: &[u8]) {
        if self.nbuf > 0 {
            let need = 64 - self.nbuf;
            if data.len() < need {
                self.buf[self.nbuf..self.nbuf + data.len()].copy_from_slice(data);
                self.nbuf += data.len();
                return;
            }
            self.buf[self.nbuf..64].copy_from_slice(&data[..need]);
            compress(&mut self.h, &self.buf);
            self.nbuf = 0;
            data = &data[need..];
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            compress(&mut self.h, &block);
            data = &data[64..];
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.nbuf = data.len();
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

fn compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    let mut i = 0;
    while i < 16 {
        let mut chunk = [0u8; 4];
        chunk.copy_from_slice(&block[i * 4..i * 4 + 4]);
        w[i] = u32::from_be_bytes(chunk);
        i += 1;
    }
    while i < 64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
        i += 1;
    }
    let mut a = h[0];
    let mut b = h[1];
    let mut c = h[2];
    let mut d = h[3];
    let mut e = h[4];
    let mut f = h[5];
    let mut g = h[6];
    let mut hh = h[7];
    i = 0;
    while i < 64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
        i += 1;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(src: &[u8], out: &mut [u8]) -> Option<usize> {
    if out.len() < src.len().saturating_mul(2) {
        return None;
    }
    for (i, byte) in src.iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0xf) as usize];
    }
    Some(src.len() * 2)
}

pub fn hex_decode(text: &[u8], out: &mut [u8]) -> Option<usize> {
    if !text.len().is_multiple_of(2) || text.len() / 2 > out.len() {
        return None;
    }
    let n = text.len() / 2;
    let mut i = 0;
    while i < n {
        let hi = hex_val(text[i * 2])?;
        let lo = hex_val(text[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(n)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Bytes of a file, plus the first 64, which is enough for the image check.
#[derive(Clone, Copy)]
pub struct Digest {
    pub len: u32,
    pub sha: [u8; 32],
    pub head: [u8; 64],
}

/// What [`commit`] is allowed to rename onto. The live kernel, the Pi
/// boot `config.txt`, the DNS JSON files, and the firmware blobs are
/// absent on purpose.
pub fn dest_allowed(name: &str) -> bool {
    matches!(
        name,
        "FWTEST.BIN" | "KERNEL8.NEW" | "K2712.NEW" | "TRYBOOT.TXT"
    )
}

pub trait Card {
    type Error: core::fmt::Debug;
    fn remove(&mut self, name: &str) -> Result<(), Self::Error>;
    fn rename(&mut self, from: &str, to: &str) -> Result<(), Self::Error>;
    fn exists(&mut self, name: &str) -> Result<bool, Self::Error>;
    fn digest(&mut self, name: &str) -> Result<Digest, Self::Error>;
}

#[derive(Debug)]
pub enum CommitError<E> {
    Refused,
    Checksum,
    Image,
    Io(E),
}

/// Read `temp` back, check it, then swing the directory entry onto `dest`.
///
/// `dest` is moved to `side` first so a failed rename of `temp` can put
/// the previous file back. On any check failure `temp` is removed and
/// `dest` is not opened for writing. A power loss can leave `side` or
/// `temp`; those names are not boot files.
pub fn commit<C: Card>(
    card: &mut C,
    dest: &str,
    temp: &str,
    side: &str,
    expect_len: u32,
    expect_sha: &[u8; 32],
    image: bool,
) -> Result<(), CommitError<C::Error>> {
    if !dest_allowed(dest)
        || dest.eq_ignore_ascii_case(temp)
        || dest.eq_ignore_ascii_case(side)
        || temp.eq_ignore_ascii_case(side)
    {
        return Err(CommitError::Refused);
    }
    let dig = match card.digest(temp) {
        Ok(dig) => dig,
        Err(err) => {
            let _ = card.remove(temp);
            return Err(CommitError::Io(err));
        }
    };
    commit_digest(card, dest, temp, side, expect_len, expect_sha, image, dig)
}

/// Same rename as [`commit`], using a digest the caller already computed
/// by reading the temp file back. Does not read the file again.
#[allow(clippy::too_many_arguments)]
pub fn commit_digest<C: Card>(
    card: &mut C,
    dest: &str,
    temp: &str,
    side: &str,
    expect_len: u32,
    expect_sha: &[u8; 32],
    image: bool,
    dig: Digest,
) -> Result<(), CommitError<C::Error>> {
    if !dest_allowed(dest)
        || dest.eq_ignore_ascii_case(temp)
        || dest.eq_ignore_ascii_case(side)
        || temp.eq_ignore_ascii_case(side)
    {
        return Err(CommitError::Refused);
    }
    if dig.len != expect_len || dig.sha != *expect_sha {
        let _ = card.remove(temp);
        return Err(CommitError::Checksum);
    }
    if image && !image_ok(dig.len, &dig.head) {
        let _ = card.remove(temp);
        return Err(CommitError::Image);
    }
    let had = card.exists(dest).map_err(CommitError::Io)?;
    if had {
        let _ = card.remove(side);
        if let Err(err) = card.rename(dest, side) {
            let _ = card.remove(temp);
            return Err(CommitError::Io(err));
        }
    }
    if let Err(err) = card.rename(temp, dest) {
        if had {
            let _ = card.rename(side, dest);
        }
        let _ = card.remove(temp);
        return Err(CommitError::Io(err));
    }
    if had {
        let _ = card.remove(side);
    }
    Ok(())
}

/// A raw kernel image: size bounds, not an ELF, and not a blank or erased
/// header. This project's image has no `ARM\x64` magic, so that magic is
/// not required.
pub fn image_ok(len: u32, head: &[u8]) -> bool {
    const MIN: u32 = 4096;
    const MAX: u32 = 8 * 1024 * 1024;
    if !(MIN..=MAX).contains(&len) || head.len() < 64 {
        return false;
    }
    if head.starts_with(b"\x7fELF") {
        return false;
    }
    let blank = head[..64].iter().all(|byte| *byte == 0);
    let erased = head[..64].iter().all(|byte| *byte == 0xff);
    !blank && !erased
}

/// Pi `config.txt` / `tryboot.txt` lines. This is not the DNS JSON schema
/// and it does not accept the legacy `CONFIG.TXT` record format.
pub fn validate_boot(text: &str) -> Result<(), &'static str> {
    if text.is_empty() || text.len() > 4096 {
        return Err("size");
    }
    if text.as_bytes().contains(&0) {
        return Err("nul");
    }
    let mut lines = 0u32;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let Some(inner) = inner.strip_suffix(']') else {
                return Err("filter");
            };
            if inner.is_empty()
                || !inner
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err("filter");
            }
            lines += 1;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("line");
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || key.len() > 32
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err("key");
        }
        if value.len() > 80 || !value.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
            return Err("value");
        }
        if key.eq_ignore_ascii_case("kernel") && !kernel_value_ok(value) {
            return Err("kernel");
        }
        if key.eq_ignore_ascii_case("include")
            || key.eq_ignore_ascii_case("initramfs")
            || key.eq_ignore_ascii_case("cmdline")
        {
            return Err("include");
        }
        lines += 1;
    }
    if lines == 0 {
        return Err("empty");
    }
    Ok(())
}

pub fn kernel_value_ok(value: &str) -> bool {
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        return false;
    }
    let (stem, ext) = match value.split_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => (value, ""),
    };
    if stem.is_empty()
        || stem.len() > 8
        || ext.len() > 3
        || ext.contains('.')
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !ext.is_empty() && !ext.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return false;
    }
    const BAD: &[&str] = &[
        "config.txt",
        "config.jsn",
        "start.elf",
        "start4.elf",
        "fixup.dat",
        "fixup4.dat",
        "bootcode.bin",
    ];
    !BAD.iter().any(|item| value.eq_ignore_ascii_case(item))
}

/// The one-shot boot file. `kernel` is the staged 8.3 name for this board,
/// so a second platform image can pass its own name later.
pub fn fill_tryboot(
    out: &mut [u8],
    kernel: &str,
    address: u32,
    uart: bool,
) -> Result<usize, &'static str> {
    if !kernel_value_ok(kernel) {
        return Err("kernel");
    }
    let mut n = 0;
    push(out, &mut n, b"arm_64bit=1\nkernel=")?;
    push(out, &mut n, kernel.as_bytes())?;
    push(out, &mut n, b"\nkernel_address=0x")?;
    push_hex(out, &mut n, address)?;
    if uart {
        push(out, &mut n, b"\nenable_uart=1\n")?;
    } else {
        push(out, &mut n, b"\n")?;
    }
    let text = core::str::from_utf8(&out[..n]).map_err(|_| "utf8")?;
    validate_boot(text)?;
    Ok(n)
}

fn push(out: &mut [u8], n: &mut usize, bytes: &[u8]) -> Result<(), &'static str> {
    if n.saturating_add(bytes.len()) > out.len() {
        return Err("size");
    }
    out[*n..*n + bytes.len()].copy_from_slice(bytes);
    *n += bytes.len();
    Ok(())
}

fn push_hex(out: &mut [u8], n: &mut usize, value: u32) -> Result<(), &'static str> {
    let digits = if value == 0 {
        1
    } else {
        (32 - value.leading_zeros()).div_ceil(4) as usize
    };
    let mut i = digits;
    while i > 0 {
        i -= 1;
        let shift = (i as u32) * 4;
        let nybble = ((value >> shift) & 0xf) as usize;
        push(out, n, &HEX[nybble..nybble + 1])?;
    }
    Ok(())
}

/// Chunk of an upload. The caller writes the bytes; this only checks the
/// offset and keeps the running hash.
#[derive(Clone, Copy)]
pub struct Transfer {
    total: u32,
    got: u32,
    sha: Sha256,
}

impl Transfer {
    pub fn start(total: u32) -> Result<Self, &'static str> {
        if total == 0 {
            return Err("empty");
        }
        Ok(Self {
            total,
            got: 0,
            sha: Sha256::new(),
        })
    }

    pub fn got(&self) -> u32 {
        self.got
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    pub fn accept(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        if offset != self.got {
            return Err("offset");
        }
        let next = self.got.checked_add(data.len() as u32).ok_or("offset")?;
        if next > self.total {
            return Err("offset");
        }
        self.sha.update(data);
        self.got = next;
        Ok(())
    }

    pub fn running_sha(&self) -> [u8; 32] {
        self.sha.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(hexed: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        assert_eq!(hex_decode(hexed.as_bytes(), &mut out), Some(32));
        out
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            Sha256::hash(b""),
            sha("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            Sha256::hash(b"abc"),
            sha("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            Sha256::hash(&[b'a'; 64]),
            sha("ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb")
        );
        assert_eq!(
            Sha256::hash(&[b'a'; 100]),
            sha("2816597888e4a0d3a36b82b83316ab32680eb8f00f8cd3b904d681246d285a0e")
        );
        let mut bytes = [0u8; 256];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = i as u8;
        }
        assert_eq!(
            Sha256::hash(&bytes),
            sha("40aff2e9d2d8922e47afd4648e6967497158785fbd1da870e7110266bf944880")
        );
    }

    #[test]
    fn chunked_hash_matches_one_shot() {
        let data: Vec<u8> = (0..5000).map(|i| (i * 17) as u8).collect();
        let mut xfer = Transfer::start(data.len() as u32).unwrap();
        let mut off = 0;
        while off < data.len() {
            let end = (off + 1000).min(data.len());
            xfer.accept(off as u32, &data[off..end]).unwrap();
            off = end;
        }
        assert_eq!(xfer.running_sha(), Sha256::hash(&data));
        assert!(xfer.accept(1, &[0]).is_err());
    }

    #[test]
    fn image_bounds() {
        let mut head = [0u8; 64];
        head[0] = 1;
        assert!(!image_ok(4095, &head));
        assert!(!image_ok(8 * 1024 * 1024 + 1, &head));
        assert!(!image_ok(4096, &[0; 64]));
        assert!(!image_ok(4096, &[0xff; 64]));
        let mut elf = head;
        elf[..4].copy_from_slice(b"\x7fELF");
        assert!(!image_ok(4096, &elf));
        assert!(image_ok(4096, &head));
    }

    #[test]
    fn boot_config_rules() {
        let mut buf = [0u8; 128];
        let n = fill_tryboot(&mut buf, "kernel8.new", 0x80000, true).unwrap();
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            text,
            "arm_64bit=1\nkernel=kernel8.new\nkernel_address=0x80000\nenable_uart=1\n"
        );
        let n = fill_tryboot(&mut buf, "k2712.new", 0x200000, false).unwrap();
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            text,
            "arm_64bit=1\nkernel=k2712.new\nkernel_address=0x200000\n"
        );
        assert!(validate_boot("kernel=../x\n").is_err());
        assert!(validate_boot("kernel=/etc/passwd\n").is_err());
        assert!(validate_boot("kernel=start4.elf\n").is_err());
        assert!(validate_boot("kernel=kernel_2712.img\n").is_err());
        assert!(validate_boot("include=foo.txt\n").is_err());
        assert!(validate_boot("# only a comment\n").is_err());
        assert!(validate_boot("[pi4]\narm_64bit=1\n").is_ok());
        assert!(!dest_allowed("KERNEL8.IMG"));
        assert!(!dest_allowed("CONFIG.TXT"));
        assert!(dest_allowed("KERNEL8.NEW"));
        assert!(dest_allowed("K2712.NEW"));
    }
}

#[cfg(test)]
mod fat {
    use super::*;
    use embedded_sdmmc::{
        Block, BlockCount, BlockDevice, BlockIdx, Mode, VolumeIdx, VolumeManager,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct RamDisk {
        sectors: Rc<RefCell<Vec<[u8; 512]>>>,
    }

    impl BlockDevice for RamDisk {
        type Error = &'static str;

        fn read(
            &self,
            blocks: &mut [Block],
            start_block_idx: BlockIdx,
            _reason: &str,
        ) -> Result<(), Self::Error> {
            let sectors = self.sectors.borrow();
            for (i, block) in blocks.iter_mut().enumerate() {
                let sector = sectors
                    .get(start_block_idx.0 as usize + i)
                    .ok_or("short read")?;
                block.contents.copy_from_slice(sector);
            }
            Ok(())
        }

        fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
            let mut sectors = self.sectors.borrow_mut();
            for (i, block) in blocks.iter().enumerate() {
                let sector = sectors
                    .get_mut(start_block_idx.0 as usize + i)
                    .ok_or("short write")?;
                *sector = block.contents;
            }
            Ok(())
        }

        fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
            Ok(BlockCount(self.sectors.borrow().len() as u32))
        }
    }

    struct Clock;

    impl embedded_sdmmc::TimeSource for Clock {
        fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
            embedded_sdmmc::Timestamp {
                year_since_1970: 56,
                zero_indexed_month: 10,
                zero_indexed_day: 4,
                hours: 0,
                minutes: 0,
                seconds: 0,
            }
        }
    }

    type Mgr = VolumeManager<RamDisk, Clock, 4, 4, 1>;

    fn format() -> RamDisk {
        // FAT16, one sector per cluster, cluster count just above the FAT12
        // cutoff. The MBR is sector 0 and the partition starts at LBA 1.
        const TOTAL: u32 = 4150;
        const FAT: u16 = 16;
        let mut sectors = vec![[0u8; 512]; (TOTAL as usize) + 8];
        let mbr = &mut sectors[0];
        mbr[446] = 0x00;
        mbr[450] = 0x06;
        mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
        mbr[458..462].copy_from_slice(&TOTAL.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        let bpb = &mut sectors[1];
        bpb[0] = 0xEB;
        bpb[1] = 0x3C;
        bpb[2] = 0x90;
        bpb[3..11].copy_from_slice(b"MSDOS5.0");
        bpb[11..13].copy_from_slice(&512u16.to_le_bytes());
        bpb[13] = 1;
        bpb[14..16].copy_from_slice(&1u16.to_le_bytes());
        bpb[16] = 2;
        bpb[17..19].copy_from_slice(&512u16.to_le_bytes());
        bpb[19..21].copy_from_slice(&(TOTAL as u16).to_le_bytes());
        bpb[21] = 0xF8;
        bpb[22..24].copy_from_slice(&FAT.to_le_bytes());
        bpb[24..26].copy_from_slice(&32u16.to_le_bytes());
        bpb[26..28].copy_from_slice(&2u16.to_le_bytes());
        bpb[28..32].copy_from_slice(&1u32.to_le_bytes());
        bpb[38] = 0x29;
        bpb[43..54].copy_from_slice(b"PI4DNS     ");
        bpb[54..62].copy_from_slice(b"FAT16   ");
        bpb[510] = 0x55;
        bpb[511] = 0xAA;
        for fat_lba in [2usize, 18] {
            sectors[fat_lba][0] = 0xF8;
            sectors[fat_lba][1] = 0xFF;
            sectors[fat_lba][2] = 0xFF;
            sectors[fat_lba][3] = 0xFF;
        }
        RamDisk {
            sectors: Rc::new(RefCell::new(sectors)),
        }
    }

    struct FatCard {
        disk: RamDisk,
    }

    impl FatCard {
        fn with_dir<R>(
            &mut self,
            f: impl FnOnce(&mut Mgr, embedded_sdmmc::RawDirectory) -> Result<R, &'static str>,
        ) -> Result<R, &'static str> {
            let mut mgr = VolumeManager::new(self.disk.clone(), Clock);
            let volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "volume")?;
            let dir = match mgr.open_root_dir(volume) {
                Ok(dir) => dir,
                Err(_) => {
                    let _ = mgr.close_volume(volume);
                    return Err("root");
                }
            };
            let result = f(&mut mgr, dir);
            let _ = mgr.close_dir(dir);
            let _ = mgr.close_volume(volume);
            result
        }
    }

    impl Card for FatCard {
        type Error = &'static str;

        fn remove(&mut self, name: &str) -> Result<(), Self::Error> {
            self.with_dir(|mgr, dir| match mgr.delete_file_in_dir(dir, name) {
                Ok(()) => Ok(()),
                Err(embedded_sdmmc::Error::NotFound) => Ok(()),
                Err(_) => Err("delete"),
            })
        }

        fn rename(&mut self, from: &str, to: &str) -> Result<(), Self::Error> {
            self.with_dir(|mgr, dir| mgr.rename_file_in_dir(dir, from, to).map_err(|_| "rename"))
        }

        fn exists(&mut self, name: &str) -> Result<bool, Self::Error> {
            self.with_dir(|mgr, dir| match mgr.find_directory_entry(dir, name) {
                Ok(_) => Ok(true),
                Err(embedded_sdmmc::Error::NotFound) => Ok(false),
                Err(_) => Err("find"),
            })
        }

        fn digest(&mut self, name: &str) -> Result<Digest, Self::Error> {
            self.with_dir(|mgr, dir| {
                let file = mgr
                    .open_file_in_dir(dir, name, Mode::ReadOnly)
                    .map_err(|_| "open")?;
                let mut sha = Sha256::new();
                let mut head = [0u8; 64];
                let mut len = 0u32;
                let mut buf = [0u8; 512];
                loop {
                    let n = match mgr.read(file, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => {
                            let _ = mgr.close_file(file);
                            return Err("read");
                        }
                    };
                    if len < 64 {
                        let want = (64 - len as usize).min(n);
                        head[len as usize..len as usize + want].copy_from_slice(&buf[..want]);
                    }
                    sha.update(&buf[..n]);
                    len = len.saturating_add(n as u32);
                }
                let _ = mgr.close_file(file);
                Ok(Digest {
                    len,
                    sha: sha.finish(),
                    head,
                })
            })
        }
    }

    fn write_new(card: &mut FatCard, name: &str, data: &[u8]) {
        card.with_dir(|mgr, dir| {
            let file = mgr
                .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate)
                .map_err(|_| "create")?;
            let result = mgr.write(file, data).map_err(|_| "write");
            let _ = mgr.close_file(file);
            result
        })
        .unwrap();
    }

    fn read_all(card: &mut FatCard, name: &str) -> Vec<u8> {
        card.with_dir(|mgr, dir| {
            let file = mgr
                .open_file_in_dir(dir, name, Mode::ReadOnly)
                .map_err(|_| "open")?;
            let mut out = Vec::new();
            let mut buf = [0u8; 512];
            loop {
                match mgr.read(file, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(_) => {
                        let _ = mgr.close_file(file);
                        return Err("read");
                    }
                }
            }
            let _ = mgr.close_file(file);
            Ok(out)
        })
        .unwrap()
    }

    #[test]
    fn publish_keeps_the_live_kernel_and_drops_a_bad_temp() {
        let mut card = FatCard { disk: format() };
        let live = b"SENTINEL-KERNEL8.IMG-DO-NOT-TOUCH!!!!!!!!!!!!";
        write_new(&mut card, "KERNEL8.IMG", live);

        let bad = vec![0xffu8; 4096];
        write_new(&mut card, "KN8.TMP", &bad);
        let err = commit(
            &mut card,
            "KERNEL8.NEW",
            "KN8.TMP",
            "KN8.OLD",
            bad.len() as u32,
            &Sha256::hash(&bad),
            true,
        );
        assert!(matches!(err, Err(CommitError::Image)));
        assert!(!card.exists("KN8.TMP").unwrap());
        assert!(!card.exists("KERNEL8.NEW").unwrap());
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        let mut good = vec![0u8; 5000];
        for (i, byte) in good.iter_mut().enumerate() {
            *byte = (i * 3) as u8;
        }
        good[0] = 0x01;
        write_new(&mut card, "KN8.TMP", &good);
        let wrong = Sha256::hash(b"nope");
        let err = commit(
            &mut card,
            "KERNEL8.NEW",
            "KN8.TMP",
            "KN8.OLD",
            good.len() as u32,
            &wrong,
            true,
        );
        assert!(matches!(err, Err(CommitError::Checksum)));
        assert!(!card.exists("KN8.TMP").unwrap());
        assert!(!card.exists("KERNEL8.NEW").unwrap());
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        let refused = commit(
            &mut card,
            "KERNEL8.IMG",
            "KN8.TMP",
            "KN8.OLD",
            live.len() as u32,
            &Sha256::hash(live),
            false,
        );
        assert!(matches!(refused, Err(CommitError::Refused)));
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        write_new(&mut card, "KN8.TMP", &good);
        commit(
            &mut card,
            "KERNEL8.NEW",
            "KN8.TMP",
            "KN8.OLD",
            good.len() as u32,
            &Sha256::hash(&good),
            true,
        )
        .unwrap();
        assert!(!card.exists("KN8.TMP").unwrap());
        assert!(!card.exists("KN8.OLD").unwrap());
        assert_eq!(read_all(&mut card, "KERNEL8.NEW"), good);
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        let mut next = good.clone();
        next[10] = 0x5a;
        write_new(&mut card, "KN8.TMP", &next);
        commit(
            &mut card,
            "KERNEL8.NEW",
            "KN8.TMP",
            "KN8.OLD",
            next.len() as u32,
            &Sha256::hash(&next),
            true,
        )
        .unwrap();
        assert_eq!(read_all(&mut card, "KERNEL8.NEW"), next);
        assert!(!card.exists("KN8.OLD").unwrap());
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        let scratch = b"fw-test-scratch";
        write_new(&mut card, "FWTEST.TMP", scratch);
        commit(
            &mut card,
            "FWTEST.BIN",
            "FWTEST.TMP",
            "FW.OLD",
            scratch.len() as u32,
            &Sha256::hash(scratch),
            false,
        )
        .unwrap();
        assert_eq!(read_all(&mut card, "FWTEST.BIN"), scratch);
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);
    }
}
