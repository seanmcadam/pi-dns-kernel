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
/// absent on purpose. Promote and rollback use [`commit_replacing`].
pub fn dest_allowed(name: &str) -> bool {
    matches!(
        name,
        "FWTEST.BIN" | "KERNEL8.NEW" | "K2712.NEW" | "TRYBOOT.TXT"
    )
}

fn live_dest(name: &str) -> bool {
    matches!(
        name,
        "KERNEL8.IMG" | "KERNEL8.BAK" | "K2712.IMG" | "K2712.BAK"
    )
}

fn dest_ok(name: &str, allow_live: bool) -> bool {
    dest_allowed(name) || (allow_live && live_dest(name))
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
    if !dest_ok(dest, false)
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
    replace_checked(
        card, dest, temp, side, expect_len, expect_sha, image, dig, false,
    )
}

/// Install `temp` as the live kernel or its backup.
///
/// Upload [`commit`] still refuses those names. The previous `dest` is
/// moved to `side` first and put back if the rename of `temp` fails, so a
/// failed install does not leave the card without `dest`.
#[allow(clippy::too_many_arguments)]
pub fn commit_replacing<C: Card>(
    card: &mut C,
    dest: &str,
    temp: &str,
    side: &str,
    expect_len: u32,
    expect_sha: &[u8; 32],
    image: bool,
    dig: Digest,
) -> Result<(), CommitError<C::Error>> {
    replace_checked(
        card, dest, temp, side, expect_len, expect_sha, image, dig, true,
    )
}

#[allow(clippy::too_many_arguments)]
fn replace_checked<C: Card>(
    card: &mut C,
    dest: &str,
    temp: &str,
    side: &str,
    expect_len: u32,
    expect_sha: &[u8; 32],
    image: bool,
    dig: Digest,
    allow_live: bool,
) -> Result<(), CommitError<C::Error>> {
    if !dest_ok(dest, allow_live)
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

/// Which on-disk kernel matches the running image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootPick {
    Img,
    New,
    Unknown,
}

/// `img` and `new_file` are `Some(true)` when that file's tail contains this
/// build's id, `Some(false)` when it was read and does not, and `None` when
/// the file is missing. A conclusive file match wins. `dt` is used when the
/// files are identical or neither carries the id. `partition` and `boot-mode`
/// are not tryboot. Mailbox flag 0 is not a normal boot: the loader clears it.
pub fn choose_booted(img: Option<bool>, new_file: Option<bool>, dt: Option<bool>) -> BootPick {
    let from_files = match (img, new_file) {
        (Some(false), Some(true)) | (None, Some(true)) => Some(BootPick::New),
        (Some(true), Some(false) | None) => Some(BootPick::Img),
        _ => None,
    };
    if let Some(pick) = from_files {
        return pick;
    }
    match dt {
        Some(true) => BootPick::New,
        Some(false) => BootPick::Img,
        None => BootPick::Unknown,
    }
}

pub fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|window| window == needle)
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

/// `/chosen/bootloader/tryboot` from a flattened device tree.
///
/// `Some(true)` is a tryboot boot. `Some(false)` is a bootloader node with
/// the property absent or zero, which is a normal boot. `None` means the
/// blob is not a device tree or it has no bootloader node, so the caller
/// cannot tell. An empty property and a big-endian `1` both count as tryboot.
pub fn bootloader_tryboot(blob: &[u8]) -> Option<bool> {
    if blob.len() < 40 || u32be(blob, 0)? != 0xd00d_feed {
        return None;
    }
    let total = u32be(blob, 4)? as usize;
    if total < 40 || total > blob.len() {
        return None;
    }
    let off_struct = u32be(blob, 8)? as usize;
    let off_strings = u32be(blob, 12)? as usize;
    if off_struct > total || off_strings > total {
        return None;
    }
    let strings = &blob[off_strings..total];
    let mut p = off_struct;
    // A real blob starts with an empty root, so chosen/bootloader is not
    // at a fixed depth. Match the node names wherever they sit.
    let mut path: [&str; 8] = ["", "", "", "", "", "", "", ""];
    let mut depth = 0usize;
    let mut saw_loader = false;
    let mut tryboot = None;
    while p + 4 <= total {
        let token = u32be(blob, p)?;
        p += 4;
        match token {
            1 => {
                let start = p;
                while p < total && blob[p] != 0 {
                    p += 1;
                }
                if p >= total {
                    return None;
                }
                let name = core::str::from_utf8(&blob[start..p]).ok()?;
                p = (p + 4) & !3;
                if depth < path.len() {
                    path[depth] = node_base(name);
                }
                depth += 1;
            }
            2 => {
                if depth == 0 {
                    return None;
                }
                if under_bootloader(&path, depth) {
                    saw_loader = true;
                }
                depth -= 1;
                if depth < path.len() {
                    path[depth] = "";
                }
            }
            3 => {
                if p + 8 > total {
                    return None;
                }
                let len = u32be(blob, p)? as usize;
                let nameoff = u32be(blob, p + 4)? as usize;
                p += 8;
                if p + len > total {
                    return None;
                }
                let value = &blob[p..p + len];
                p = (p + len + 3) & !3;
                let prop = cstr_at(strings, nameoff);
                if under_bootloader(&path, depth) && prop == Some("tryboot") {
                    saw_loader = true;
                    tryboot = Some(tryboot_value(value));
                } else if under_chosen(&path, depth) && prop == Some("tryboot") {
                    tryboot = Some(tryboot_value(value));
                }
            }
            4 => {}
            9 => break,
            _ => return None,
        }
    }
    if let Some(flag) = tryboot {
        return Some(flag);
    }
    if saw_loader {
        Some(false)
    } else {
        None
    }
}

fn node_base(name: &str) -> &str {
    name.split_once('@').map(|(base, _)| base).unwrap_or(name)
}

fn under_chosen(path: &[&str], depth: usize) -> bool {
    depth >= 1 && depth <= path.len() && path[depth - 1] == "chosen"
}

fn under_bootloader(path: &[&str], depth: usize) -> bool {
    depth >= 2 && depth <= path.len() && path[depth - 2] == "chosen" && path[depth - 1] == "bootloader"
}

fn tryboot_value(value: &[u8]) -> bool {
    if value.is_empty() {
        return true;
    }
    if value.len() >= 4 {
        let word = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
        return word == 1;
    }
    value.first() == Some(&b'1')
}

fn cstr_at(strings: &[u8], off: usize) -> Option<&str> {
    let rest = strings.get(off..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(&rest[..end]).ok()
}

fn u32be(blob: &[u8], off: usize) -> Option<u32> {
    let bytes = blob.get(off..off + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// First word of the firmware log table. `vclog` checks this before it
/// trusts `/chosen/log`. The fourcc is `VLOG`.
pub const VC_LOG_MAGIC: u32 = 0x564c_4f47;
/// Message-log header. The fourcc is `MESS`.
pub const VC_MSG_MAGIC: u32 = 0x5353_454d;
/// Assert-log header. `vclog -a` checks this before the message log's `MESS`.
pub const VC_ASSERT_MAGIC: u32 = 0x5452_5341;

/// `/chosen/log` from a flattened device tree: VideoCore start and size,
/// both big-endian, as `vclog` reads them. `None` if the node or the
/// property is absent, or the size is empty or above 1 MiB.
pub fn chosen_log(blob: &[u8]) -> Option<(u32, u32)> {
    if blob.len() < 40 || u32be(blob, 0)? != 0xd00d_feed {
        return None;
    }
    let total = u32be(blob, 4)? as usize;
    if total < 40 || total > blob.len() {
        return None;
    }
    let off_struct = u32be(blob, 8)? as usize;
    let off_strings = u32be(blob, 12)? as usize;
    if off_struct > total || off_strings > total {
        return None;
    }
    let strings = &blob[off_strings..total];
    let mut p = off_struct;
    let mut path: [&str; 8] = ["", "", "", "", "", "", "", ""];
    let mut depth = 0usize;
    while p + 4 <= total {
        let token = u32be(blob, p)?;
        p += 4;
        match token {
            1 => {
                let start = p;
                while p < total && blob[p] != 0 {
                    p += 1;
                }
                if p >= total {
                    return None;
                }
                let name = core::str::from_utf8(&blob[start..p]).ok()?;
                p = (p + 4) & !3;
                if depth < path.len() {
                    path[depth] = node_base(name);
                }
                depth += 1;
            }
            2 => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth < path.len() {
                    path[depth] = "";
                }
            }
            3 => {
                if p + 8 > total {
                    return None;
                }
                let len = u32be(blob, p)? as usize;
                let nameoff = u32be(blob, p + 4)? as usize;
                p += 8;
                if p + len > total {
                    return None;
                }
                let value = &blob[p..p + len];
                p = (p + len + 3) & !3;
                if under_chosen(&path, depth) && cstr_at(strings, nameoff) == Some("log") && value.len() >= 8
                {
                    let start = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                    let size = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                    if size > 0 && size <= 1024 * 1024 {
                        return Some((start, size));
                    }
                    return None;
                }
            }
            4 => {}
            9 => break,
            _ => return None,
        }
    }
    None
}

fn u32_le(bytes: &[u8], off: usize) -> Option<u32> {
    let word = bytes.get(off..off + 4)?;
    Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

/// VideoCore address of the message log. `toc` is the 56-byte table at
/// the start of the `/chosen/log` region (`vclog` `log_toc_t`).
pub fn vclog_msg_addr(toc: &[u8]) -> Option<u32> {
    if toc.len() < 56 || u32_le(toc, 0)? != VC_LOG_MAGIC {
        return None;
    }
    u32_le(toc, 44)
}

/// VideoCore address of the assert log. Same table, the word before the
/// message-log address (`vclog` `log_toc_t.assert_addr`).
pub fn vclog_assert_addr(toc: &[u8]) -> Option<u32> {
    if toc.len() < 56 || u32_le(toc, 0)? != VC_LOG_MAGIC {
        return None;
    }
    u32_le(toc, 36)
}

/// `(start, end, write, read)` VideoCore addresses from a message-log
/// header (`vclog` `log_hdr_t`).
pub fn vclog_msg_range(hdr: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if hdr.len() < 20 || u32_le(hdr, 0)? != VC_MSG_MAGIC {
        return None;
    }
    Some((
        u32_le(hdr, 4)?,
        u32_le(hdr, 8)?,
        u32_le(hdr, 12)?,
        u32_le(hdr, 16)?,
    ))
}

/// ARM physical range that may be read for the firmware log.
///
/// `start` is inclusive and `end` is exclusive. Both are byte addresses,
/// not page-aligned. A read has to sit entirely inside this range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VclogWindow {
    pub start: u64,
    pub end: u64,
}

/// Why [`vclog_window`] refused a `/chosen/log` or `GET_VC_MEMORY` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VclogDeny {
    /// Neither source was present.
    NoSource,
    /// A size was zero.
    Empty,
    /// A start plus a size overflowed.
    Overflow,
    /// The translated range is outside `0..ram_end`.
    OutsideRam,
    /// `/chosen/log` does not overlap `GET_VC_MEMORY`.
    OutsideChosen,
    /// The translated range is not inside `GET_VC_MEMORY`, or it misses RAM.
    OutsideVcMem,
}

/// Readable firmware-log window.
///
/// `dt` is `/chosen/log` (VideoCore start, size). `mem` is mailbox
/// `GET_VC_MEMORY` `0x00010006` (base, size). A VideoCore address is
/// tried raw and, when that differs, as `addr & 0x3fff_ffff`. When both
/// sources exist the window is their intersection, kept from the start
/// and capped at 1 MiB (the table of contents is at the start). When
/// only the mailbox window exists, the last 1 MiB is kept: the firmware
/// parks the table at the top of that reservation. The result has to
/// sit inside `0..ram_end`.
pub fn vclog_window(
    dt: Option<(u32, u32)>,
    mem: Option<(u32, u32)>,
    ram_end: u64,
) -> Result<VclogWindow, VclogDeny> {
    const CAP: u64 = 1024 * 1024;
    let dt_ranges = match dt {
        Some((start, size)) => Some(vc_ranges(start, size)?),
        None => None,
    };
    let mem_ranges = match mem {
        Some((start, size)) => Some(vc_ranges(start, size)?),
        None => None,
    };
    let picked = match (dt_ranges, mem_ranges) {
        (None, None) => return Err(VclogDeny::NoSource),
        (Some(dt_ranges), None) => {
            let range = first_in_ram(&dt_ranges, ram_end).ok_or(VclogDeny::OutsideRam)?;
            head_capped(range, CAP)
        }
        (None, Some(mem_ranges)) => {
            let range = first_in_ram(&mem_ranges, ram_end).ok_or(VclogDeny::OutsideRam)?;
            tail_capped(range, CAP)
        }
        (Some(dt_ranges), Some(mem_ranges)) => {
            let mut best: Option<(u64, u64)> = None;
            let mut dt_in_ram = false;
            for dt_range in dt_ranges.into_iter().flatten() {
                if clamp_ram(dt_range, ram_end).is_some() {
                    dt_in_ram = true;
                }
                for mem_range in mem_ranges.into_iter().flatten() {
                    let Some(hit) = intersect(dt_range, mem_range) else {
                        continue;
                    };
                    let Some(hit) = clamp_ram(hit, ram_end) else {
                        continue;
                    };
                    let len = hit.1 - hit.0;
                    let replace = best.map(|have| len > have.1 - have.0).unwrap_or(true);
                    if replace {
                        best = Some(hit);
                    }
                }
            }
            let Some(hit) = best else {
                return Err(if dt_in_ram {
                    VclogDeny::OutsideChosen
                } else {
                    VclogDeny::OutsideVcMem
                });
            };
            head_capped(hit, CAP)
        }
    };
    if picked.0 >= picked.1 {
        return Err(VclogDeny::Empty);
    }
    Ok(VclogWindow {
        start: picked.0,
        end: picked.1,
    })
}

/// `pa` for `len` bytes lies entirely inside `window`.
pub fn vclog_covers(pa: u64, len: u64, window: VclogWindow) -> bool {
    len > 0 && pa >= window.start && pa.saturating_add(len) <= window.end
}

fn vc_ranges(start: u32, size: u32) -> Result<[Option<(u64, u64)>; 2], VclogDeny> {
    if size == 0 {
        return Err(VclogDeny::Empty);
    }
    let raw = span(u64::from(start), u64::from(size))?;
    let masked_base = u64::from(start) & 0x3fff_ffff;
    let masked = if masked_base == u64::from(start) {
        None
    } else {
        Some(span(masked_base, u64::from(size))?)
    };
    Ok([Some(raw), masked])
}

fn span(base: u64, size: u64) -> Result<(u64, u64), VclogDeny> {
    let Some(end) = base.checked_add(size) else {
        return Err(VclogDeny::Overflow);
    };
    if end <= base {
        return Err(VclogDeny::Empty);
    }
    Ok((base, end))
}

fn intersect(a: (u64, u64), b: (u64, u64)) -> Option<(u64, u64)> {
    let start = a.0.max(b.0);
    let end = a.1.min(b.1);
    if start < end {
        Some((start, end))
    } else {
        None
    }
}

fn clamp_ram(range: (u64, u64), ram_end: u64) -> Option<(u64, u64)> {
    if range.0 >= ram_end {
        return None;
    }
    let end = range.1.min(ram_end);
    if range.0 < end {
        Some((range.0, end))
    } else {
        None
    }
}

fn first_in_ram(ranges: &[Option<(u64, u64)>; 2], ram_end: u64) -> Option<(u64, u64)> {
    for range in ranges.iter().flatten() {
        if let Some(hit) = clamp_ram(*range, ram_end) {
            return Some(hit);
        }
    }
    None
}

fn head_capped(range: (u64, u64), cap: u64) -> (u64, u64) {
    if range.1 - range.0 <= cap {
        range
    } else {
        (range.0, range.0 + cap)
    }
}

fn tail_capped(range: (u64, u64), cap: u64) -> (u64, u64) {
    if range.1 - range.0 <= cap {
        range
    } else {
        (range.1 - cap, range.1)
    }
}

/// `(start, end, write, read)` from an assert-log header (`vclog -a`).
pub fn vclog_assert_range(hdr: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if hdr.len() < 20 || u32_le(hdr, 0)? != VC_ASSERT_MAGIC {
        return None;
    }
    Some((
        u32_le(hdr, 4)?,
        u32_le(hdr, 8)?,
        u32_le(hdr, 12)?,
        u32_le(hdr, 16)?,
    ))
}

/// One `vclog` render of a linear (already unwrapped) log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VclogRender {
    /// Bytes written. A line that does not fit is left out entirely.
    pub bytes: usize,
    /// Lines written.
    pub entries: u32,
    /// Every byte of `src` was an entry, and every entry became a line.
    pub complete: bool,
}

/// Used bytes of a circular log, oldest first. The bool is set when `out`
/// could not hold the whole range from `read_off` to `write_off`.
pub fn vclog_linearize(
    ring: &[u8],
    read_off: usize,
    write_off: usize,
    out: &mut [u8],
) -> (usize, bool) {
    if ring.is_empty() || read_off > ring.len() || write_off > ring.len() {
        return (0, false);
    }
    let available = if write_off >= read_off {
        write_off - read_off
    } else {
        ring.len() - read_off + write_off
    };
    let n = available.min(out.len());
    let mut i = 0;
    while i < n {
        out[i] = ring[(read_off + i) % ring.len()];
        i += 1;
    }
    (n, n < available)
}

/// `vclog -m` lines. `src` is the used range, oldest byte first, not the
/// spare capacity of the ring. Each entry is one line:
/// `SSSSSS.mmm: text`.
pub fn vclog_render_messages(src: &[u8], out: &mut [u8]) -> VclogRender {
    render_log(src, out, false)
}

/// `vclog -a` lines. Same ring layout as the message log. Each entry is
/// `SSSSSS.mmm: assert( cond ) failed; file line N`.
pub fn vclog_render_asserts(src: &[u8], out: &mut [u8]) -> VclogRender {
    render_log(src, out, true)
}

fn render_log(src: &[u8], out: &mut [u8], assert_log: bool) -> VclogRender {
    if src.is_empty() {
        return VclogRender {
            bytes: 0,
            entries: 0,
            complete: true,
        };
    }
    let mut pos = 0usize;
    let mut n = 0usize;
    let mut entries = 0u32;
    while pos + 8 <= src.len() {
        let time = u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]]);
        let size = u16::from_le_bytes([src[pos + 6], src[pos + 7]]) as usize;
        if size < 8 || pos + size > src.len() {
            return VclogRender {
                bytes: n,
                entries,
                complete: false,
            };
        }
        let payload = &src[pos + 8..pos + size];
        let wrote = if assert_log {
            write_assert_line(time, payload, out, &mut n)
        } else {
            write_msg_line(time, payload, out, &mut n)
        };
        if !wrote {
            return VclogRender {
                bytes: n,
                entries,
                complete: false,
            };
        }
        entries += 1;
        pos += size;
    }
    VclogRender {
        bytes: n,
        entries,
        complete: pos == src.len(),
    }
}

fn write_msg_line(time: u32, payload: &[u8], out: &mut [u8], n: &mut usize) -> bool {
    let text = if payload.len() > 4 { &payload[4..] } else { &[] };
    let end = text.iter().position(|byte| *byte == 0).unwrap_or(text.len());
    let text = &text[..end];
    write_stamped(time, out, n, |dst, at| {
        let at = copy_literal(b": ", dst, at);
        finish_line(text, dst, at)
    })
}

fn write_assert_line(time: u32, payload: &[u8], out: &mut [u8], n: &mut usize) -> bool {
    let Some((file, line, cond)) = assert_parts(payload) else {
        return false;
    };
    write_stamped(time, out, n, |dst, at| {
        let mut i = at;
        i = copy_literal(b": assert( ", dst, i);
        i = copy_visible(cond, dst, i);
        i = copy_literal(b" ) failed; ", dst, i);
        i = copy_visible(file, dst, i);
        i = copy_literal(b" line ", dst, i);
        write_u_pad(dst, &mut i, line, 1);
        if i >= dst.len() {
            return i;
        }
        dst[i] = b'\n';
        i + 1
    })
}

fn assert_parts(payload: &[u8]) -> Option<(&[u8], u32, &[u8])> {
    let file_len = payload.iter().position(|byte| *byte == 0)?;
    let cond_at = file_len + 1 + 4;
    if cond_at >= payload.len() {
        return None;
    }
    let line = u32::from_le_bytes([
        payload[file_len + 1],
        payload[file_len + 2],
        payload[file_len + 3],
        payload[file_len + 4],
    ]);
    let cond = &payload[cond_at..];
    let cond_end = cond.iter().position(|byte| *byte == 0).unwrap_or(cond.len());
    Some((&payload[..file_len], line, &cond[..cond_end]))
}

/// `body` writes the text after the timestamp and returns the new length.
/// The timestamp is `vclog`'s `%06i.%03i`. A line that does not fit is not
/// started.
fn write_stamped(time: u32, out: &mut [u8], n: &mut usize, body: impl FnOnce(&mut [u8], usize) -> usize) -> bool {
    let start = *n;
    let sec_len = digit_len(time / 1000).max(6);
    let prefix = sec_len + 1 + 3;
    if start + prefix > out.len() {
        return false;
    }
    write_u_pad(out, n, time / 1000, 6);
    if *n >= out.len() {
        *n = start;
        return false;
    }
    out[*n] = b'.';
    *n += 1;
    write_u_pad(out, n, time % 1000, 3);
    let after = body(out, *n);
    if after < *n || after > out.len() || after == *n || out.get(after - 1) != Some(&b'\n') {
        *n = start;
        return false;
    }
    *n = after;
    true
}

fn copy_literal(literal: &[u8], dst: &mut [u8], at: usize) -> usize {
    let mut i = 0;
    let mut n = at;
    while i < literal.len() {
        if n >= dst.len() {
            return n;
        }
        dst[n] = literal[i];
        n += 1;
        i += 1;
    }
    n
}

fn copy_visible(text: &[u8], dst: &mut [u8], at: usize) -> usize {
    let mut i = 0;
    let mut n = at;
    while i < text.len() {
        if n >= dst.len() {
            return n;
        }
        let byte = text[i];
        dst[n] = if byte.is_ascii_graphic() || byte == b' ' {
            byte
        } else {
            b' '
        };
        n += 1;
        i += 1;
    }
    n
}

fn finish_line(text: &[u8], dst: &mut [u8], at: usize) -> usize {
    let n = copy_visible(text, dst, at);
    if n >= dst.len() {
        return n;
    }
    dst[n] = b'\n';
    n + 1
}

fn digit_len(mut value: u32) -> usize {
    let mut n = 1;
    while value >= 10 {
        value /= 10;
        n += 1;
    }
    n
}

fn write_u_pad(out: &mut [u8], n: &mut usize, mut value: u32, width: usize) {
    let mut digits = [0u8; 10];
    let mut d = 0usize;
    loop {
        digits[d] = b'0' + (value % 10) as u8;
        value /= 10;
        d += 1;
        if value == 0 {
            break;
        }
    }
    let mut pad = width;
    while pad > d {
        if *n >= out.len() {
            return;
        }
        out[*n] = b'0';
        *n += 1;
        pad -= 1;
    }
    while d > 0 {
        d -= 1;
        if *n >= out.len() {
            return;
        }
        out[*n] = digits[d];
        *n += 1;
    }
}

/// Oldest firmware messages that fit in `out`. `buf` is the circular
/// buffer already unwrapped, or a linear slice whose messages do not
/// wrap. `read_off` is the oldest byte and `write_off` is where the
/// firmware will write next. Each message is `vclog`'s `msg_hdr_t` plus
/// a 4-byte payload prefix and a C string. The text is ASCII, with
/// quotes and backslashes turned into spaces so it can sit in JSON.
pub fn vclog_tail(buf: &[u8], read_off: usize, write_off: usize, out: &mut [u8]) -> usize {
    if buf.is_empty() || out.is_empty() || read_off > buf.len() || write_off > buf.len() {
        return 0;
    }
    let mut pos = read_off;
    let mut n = 0usize;
    let mut steps = 0u32;
    while pos != write_off && pos < buf.len() && steps < 400 {
        steps += 1;
        if pos + 8 > buf.len() {
            break;
        }
        let size = u16::from_le_bytes([buf[pos + 6], buf[pos + 7]]) as usize;
        if size < 8 || pos + size > buf.len() {
            break;
        }
        let payload = &buf[pos + 8..pos + size];
        if payload.len() > 4 {
            let text = &payload[4..];
            let end = text.iter().position(|byte| *byte == 0).unwrap_or(text.len());
            if end > 0 {
                if n > 0 {
                    if n + 3 >= out.len() {
                        break;
                    }
                    out[n] = b' ';
                    out[n + 1] = b'|';
                    out[n + 2] = b' ';
                    n += 3;
                }
                let mut i = 0;
                while i < end {
                    if n >= out.len() {
                        return n;
                    }
                    out[n] = json_safe(text[i]);
                    n += 1;
                    i += 1;
                }
            }
        }
        pos += size;
    }
    n
}

fn json_safe(byte: u8) -> u8 {
    if byte.is_ascii_graphic() && byte != b'"' && byte != b'\\' {
        byte
    } else if byte == b' ' || byte == b'\t' {
        b' '
    } else {
        b' '
    }
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
/// Pi 4 and Pi 5 tryboot preset. `hdmi_force_hotplug` is not written.
/// On 0.1.67 that line made a Pi 4 power-on report 640x480 and left the
/// rainbow splash up. The live `config.txt` is not a destination here.
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
    push(out, &mut n, b"\n")?;
    if uart {
        push(out, &mut n, b"enable_uart=1\n")?;
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
        assert!(!text.contains("hdmi_force_hotplug"));
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

    #[test]
    fn promote_replaces_the_live_kernel_and_keeps_a_bad_one_in_place() {
        let mut card = FatCard { disk: format() };
        let live = b"SENTINEL-KERNEL8.IMG-DO-NOT-TOUCH!!!!!!!!!!!!";
        write_new(&mut card, "KERNEL8.IMG", live);
        let mut next = vec![0u8; 4096];
        next[0] = 0x11;
        next[1] = 0x22;
        write_new(&mut card, "KN8.TMP", &next);
        let refused = commit(
            &mut card,
            "KERNEL8.IMG",
            "KN8.TMP",
            "KN8.OLD",
            next.len() as u32,
            &Sha256::hash(&next),
            true,
        );
        assert!(matches!(refused, Err(CommitError::Refused)));
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);

        let wrong = dig_of(b"nope");
        let err = commit_replacing(
            &mut card,
            "KERNEL8.IMG",
            "KN8.TMP",
            "KN8.OLD",
            next.len() as u32,
            &wrong.sha,
            true,
            wrong,
        );
        assert!(matches!(err, Err(CommitError::Checksum)));
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), live);
        assert!(!card.exists("KN8.TMP").unwrap());

        write_new(&mut card, "KN8B.TMP", live);
        commit_replacing(
            &mut card,
            "KERNEL8.BAK",
            "KN8B.TMP",
            "KN8B.OLD",
            live.len() as u32,
            &Sha256::hash(live),
            false,
            dig_of(live),
        )
        .unwrap();
        write_new(&mut card, "KN8.TMP", &next);
        commit_replacing(
            &mut card,
            "KERNEL8.IMG",
            "KN8.TMP",
            "KN8.OLD",
            next.len() as u32,
            &Sha256::hash(&next),
            true,
            dig_of(&next),
        )
        .unwrap();
        assert_eq!(read_all(&mut card, "KERNEL8.IMG"), next);
        assert_eq!(read_all(&mut card, "KERNEL8.BAK"), live);
        assert!(!card.exists("KN8.OLD").unwrap());
        assert!(!card.exists("KN8.TMP").unwrap());
    }

    fn dig_of(data: &[u8]) -> Digest {
        let mut head = [0u8; 64];
        let n = data.len().min(64);
        head[..n].copy_from_slice(&data[..n]);
        Digest {
            len: data.len() as u32,
            sha: Sha256::hash(data),
            head,
        }
    }

    fn format_fat32() -> RamDisk {
        const RESERVED: u32 = 8;
        const FAT_SECS: u32 = 4;
        const DATA: u32 = 256;
        const TOTAL: u32 = RESERVED + FAT_SECS * 2 + DATA;
        let mut sectors = vec![[0u8; 512]; 1 + TOTAL as usize];
        let mbr = &mut sectors[0];
        mbr[450] = 0x0C;
        mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
        mbr[458..462].copy_from_slice(&TOTAL.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        let bpb = &mut sectors[1];
        bpb[0] = 0xEB;
        bpb[1] = 0x58;
        bpb[2] = 0x90;
        bpb[3..11].copy_from_slice(b"MSDOS5.0");
        bpb[11..13].copy_from_slice(&512u16.to_le_bytes());
        bpb[13] = 1;
        bpb[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        bpb[16] = 2;
        bpb[21] = 0xF8;
        bpb[24..26].copy_from_slice(&32u16.to_le_bytes());
        bpb[26..28].copy_from_slice(&2u16.to_le_bytes());
        bpb[28..32].copy_from_slice(&1u32.to_le_bytes());
        bpb[32..36].copy_from_slice(&TOTAL.to_le_bytes());
        bpb[36..40].copy_from_slice(&FAT_SECS.to_le_bytes());
        bpb[44..48].copy_from_slice(&2u32.to_le_bytes());
        bpb[48..50].copy_from_slice(&1u16.to_le_bytes());
        bpb[50..52].copy_from_slice(&6u16.to_le_bytes());
        bpb[64] = 0x80;
        bpb[66] = 0x29;
        bpb[71..82].copy_from_slice(b"PI4DNS     ");
        bpb[82..90].copy_from_slice(b"FAT32   ");
        bpb[510] = 0x55;
        bpb[511] = 0xAA;
        for base in [1 + RESERVED as usize, 1 + (RESERVED + FAT_SECS) as usize] {
            let fat = &mut sectors[base];
            fat[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
            fat[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            fat[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        }
        RamDisk {
            sectors: Rc::new(RefCell::new(sectors)),
        }
    }

    fn fat_link(disk: &RamDisk, cluster: u32, next: u32) {
        let mut sectors = disk.sectors.borrow_mut();
        for base in [9usize, 13] {
            let off = (cluster as usize) * 4;
            sectors[base][off..off + 4].copy_from_slice(&next.to_le_bytes());
        }
    }

    fn fill_dir_sector(disk: &RamDisk, sector: usize) {
        let mut sectors = disk.sectors.borrow_mut();
        let block = &mut sectors[sector];
        for entry in 0..16 {
            let start = entry * 32;
            block[start..start + 32].fill(b'B');
            block[start + 11] = 0x20;
        }
    }

    fn stat_line(size_hex: &str, sha_prefix: &str) -> Vec<u8> {
        let mut line = Vec::new();
        line.extend_from_slice(size_hex.as_bytes());
        line.push(b' ');
        line.extend_from_slice(sha_prefix.as_bytes());
        line.resize(73, b'0');
        line.push(b'\n');
        line
    }

    #[test]
    fn boot_files_of_the_stuck_card_do_not_hang_a_cyclic_directory() {
        let disk = format_fat32();
        let mut card = FatCard { disk: disk.clone() };
        let kstat = stat_line("0037c0a8", "c38ea57f");
        let bstat = stat_line("0037abd0", "d7e0252d");
        let fwres = b"reboot requested\n".to_vec();
        let shut = vec![b'h'; 189];
        let tryb = b"kernel=kernel8.new\n".to_vec();
        let vclog = vec![b'v'; 3198];
        let pre = vec![b'p'; 3656];
        let fwtest = vec![b'f'; 65536];
        assert_eq!(kstat.len(), 74);
        assert_eq!(bstat.len(), 74);
        write_new(&mut card, "KSTAT.TXT", &kstat);
        write_new(&mut card, "BSTAT.TXT", &bstat);
        write_new(&mut card, "FWRES.TXT", &fwres);
        write_new(&mut card, "HDMISHUT.TXT", &shut);
        write_new(&mut card, "TRYB.OLD", &tryb);
        write_new(&mut card, "VCLOG.TXT", &vclog);
        write_new(&mut card, "VCLOGPRE.TXT", &pre);
        write_new(&mut card, "FWTEST.BIN", &fwtest);
        assert_eq!(read_all(&mut card, "KSTAT.TXT"), kstat);
        assert_eq!(read_all(&mut card, "BSTAT.TXT"), bstat);
        assert_eq!(read_all(&mut card, "FWRES.TXT"), fwres);
        assert_eq!(read_all(&mut card, "HDMISHUT.TXT").len(), 189);
        assert_eq!(read_all(&mut card, "TRYB.OLD"), tryb);
        assert_eq!(read_all(&mut card, "VCLOG.TXT").len(), 3198);
        assert_eq!(read_all(&mut card, "VCLOGPRE.TXT").len(), 3656);
        assert_eq!(read_all(&mut card, "FWTEST.BIN").len(), 65536);

        fat_link(&disk, 2, 2);
        assert!(card.exists("VCLOG.TXT").unwrap());
        assert!(!card.exists("NOPE.TXT").unwrap());
        assert_eq!(read_all(&mut card, "FWRES.TXT"), fwres);

        fill_dir_sector(&disk, 17);
        let missing = card.with_dir(
            |mgr, dir| match mgr.find_directory_entry(dir, "VCLOG.TXT") {
                Err(embedded_sdmmc::Error::BadCluster) => Ok("bad"),
                Ok(_) => Ok("found"),
                Err(_) => Ok("other"),
            },
        );
        assert_eq!(missing.unwrap(), "bad");

        fat_link(&disk, 2, 3);
        fat_link(&disk, 3, 2);
        fill_dir_sector(&disk, 18);
        let cycled = card.with_dir(|mgr, dir| match mgr.find_directory_entry(dir, "LOG.TXT") {
            Err(embedded_sdmmc::Error::BadCluster) => Ok(()),
            other => Err(match other {
                Ok(_) => "found",
                Err(_) => "other",
            }),
        });
        assert!(cycled.is_ok());
    }
}

#[cfg(test)]
mod fdt_tests {
    use super::{
        bootloader_tryboot, chosen_log, vclog_assert_addr, vclog_covers, vclog_linearize,
        vclog_msg_addr, vclog_msg_range, vclog_render_asserts, vclog_render_messages, vclog_tail,
        vclog_window, VclogDeny,
    };

    fn be(n: u32) -> [u8; 4] {
        n.to_be_bytes()
    }

    fn push_name(body: &mut Vec<u8>, name: &str) {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }

    fn finish(body: Vec<u8>) -> Vec<u8> {
        let strings = b"tryboot\0";
        let mut out = vec![0u8; 40];
        let off_struct = 40u32;
        let off_strings = 40 + body.len() as u32;
        out.extend_from_slice(&body);
        out.extend_from_slice(strings);
        let total = out.len() as u32;
        out[0..4].copy_from_slice(&be(0xd00d_feed));
        out[4..8].copy_from_slice(&be(total));
        out[8..12].copy_from_slice(&be(off_struct));
        out[12..16].copy_from_slice(&be(off_strings));
        out
    }

    fn blob(tryboot: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&be(1));
        push_name(&mut body, "chosen");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "bootloader");
        if let Some(value) = tryboot {
            body.extend_from_slice(&be(3));
            body.extend_from_slice(&be(value.len() as u32));
            body.extend_from_slice(&be(0));
            body.extend_from_slice(value);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(9));
        finish(body)
    }

    #[test]
    fn tryboot_property_and_a_normal_bootloader_node() {
        assert_eq!(bootloader_tryboot(&blob(Some(&be(1)))), Some(true));
        assert_eq!(bootloader_tryboot(&blob(Some(&[]))), Some(true));
        assert_eq!(bootloader_tryboot(&blob(Some(&be(0)))), Some(false));
        assert_eq!(bootloader_tryboot(&blob(None)), Some(false));
        assert_eq!(bootloader_tryboot(&[0; 64]), None);
        let mut body = Vec::new();
        body.extend_from_slice(&be(1));
        push_name(&mut body, "chosen");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "bootargs");
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(9));
        assert_eq!(bootloader_tryboot(&finish(body)), None);
        assert_eq!(bootloader_tryboot(&rooted(Some(&[]))), Some(true));
        assert_eq!(bootloader_tryboot(&rooted(None)), Some(false));
        assert_eq!(bootloader_tryboot(&rooted_mode()), Some(false));
    }

    fn rooted(tryboot: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&be(1));
        push_name(&mut body, "");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "chosen");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "bootloader");
        if let Some(value) = tryboot {
            body.extend_from_slice(&be(3));
            body.extend_from_slice(&be(value.len() as u32));
            body.extend_from_slice(&be(0));
            body.extend_from_slice(value);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(9));
        finish(body)
    }

    fn rooted_mode() -> Vec<u8> {
        let strings = b"boot-mode\0";
        let mut body = Vec::new();
        body.extend_from_slice(&be(1));
        push_name(&mut body, "");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "chosen");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "bootloader");
        body.extend_from_slice(&be(3));
        body.extend_from_slice(&be(4));
        body.extend_from_slice(&be(0));
        body.extend_from_slice(&be(1));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(9));
        let mut out = vec![0u8; 40];
        let off_struct = 40u32;
        let off_strings = 40 + body.len() as u32;
        out.extend_from_slice(&body);
        out.extend_from_slice(strings);
        let total = out.len() as u32;
        out[0..4].copy_from_slice(&be(0xd00d_feed));
        out[4..8].copy_from_slice(&be(total));
        out[8..12].copy_from_slice(&be(off_struct));
        out[12..16].copy_from_slice(&be(off_strings));
        out
    }

    fn log_tree(start: u32, size: u32) -> Vec<u8> {
        let strings = b"log\0";
        let mut body = Vec::new();
        body.extend_from_slice(&be(1));
        push_name(&mut body, "");
        body.extend_from_slice(&be(1));
        push_name(&mut body, "chosen");
        body.extend_from_slice(&be(3));
        body.extend_from_slice(&be(8));
        body.extend_from_slice(&be(0));
        body.extend_from_slice(&be(start));
        body.extend_from_slice(&be(size));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(2));
        body.extend_from_slice(&be(9));
        let mut out = vec![0u8; 40];
        let off_struct = 40u32;
        let off_strings = 40 + body.len() as u32;
        out.extend_from_slice(&body);
        out.extend_from_slice(strings);
        let total = out.len() as u32;
        out[0..4].copy_from_slice(&be(0xd00d_feed));
        out[4..8].copy_from_slice(&be(total));
        out[8..12].copy_from_slice(&be(off_struct));
        out[12..16].copy_from_slice(&be(off_strings));
        out
    }

    #[test]
    fn chosen_log_is_the_vclog_pair() {
        assert_eq!(chosen_log(&log_tree(0x3e00_0000, 4096)), Some((0x3e00_0000, 4096)));
        assert_eq!(chosen_log(&blob(Some(&be(1)))), None);
        assert_eq!(chosen_log(&[0; 64]), None);
    }

    /// Pi 4 RAM stops at the peripheral window. The 0.1.77 fault address
    /// `0x3ffa4164` is inside a `/chosen/log` that starts at `0x3ffa0000`.
    const RAM_END: u64 = 0xFC00_0000;
    const FAR: u64 = 0x3FFA_4164;

    #[test]
    fn vclog_window_accepts_the_fault_address_and_rejects_a_short_log() {
        let dt = Some((0x3FFA_0000, 0x8_0000));
        let mem = Some((0x3C00_0000, 0x0400_0000));
        let win = vclog_window(dt, mem, RAM_END).unwrap();
        assert_eq!(win.start, 0x3FFA_0000);
        assert_eq!(win.end, 0x4000_0000);
        assert!(vclog_covers(FAR, 4, win));
        assert!(vclog_covers(0x3FFA_0000, 0x6_0000, win));
        assert!(!vclog_covers(0x3FFA_0000, 0x8_0000, win));
        assert!(!vclog_covers(FAR, 0x8_0000, win));
        assert!(!vclog_covers(win.end - 1, 2, win));
        assert!(!vclog_covers(0xFFFF_0000_3FFA_4164, 4, win));

        let short = vclog_window(Some((0x3FFA_0000, 0x1000)), mem, RAM_END).unwrap();
        assert!(!vclog_covers(FAR, 4, short));
        assert!(vclog_covers(0x3FFA_0000, 0x1000, short));
    }

    #[test]
    fn vclog_window_uses_the_bus_alias_and_refuses_a_miss() {
        let dt = Some((0xFFFA_0000, 0x8000));
        let mem = Some((0xFFC0_0000, 0x0400_0000));
        let win = vclog_window(dt, mem, RAM_END).unwrap();
        assert_eq!(win.start, 0x3FFA_0000);
        assert!(vclog_covers(FAR, 4, win));
        assert_eq!(
            vclog_window(Some((0x3FFA_0000, 0x1000)), Some((0x1000_0000, 0x1000)), RAM_END),
            Err(VclogDeny::OutsideChosen)
        );
        assert_eq!(vclog_window(None, None, RAM_END), Err(VclogDeny::NoSource));
        assert_eq!(
            vclog_window(Some((0x3FFA_0000, 0)), None, RAM_END),
            Err(VclogDeny::Empty)
        );
        assert_eq!(
            vclog_window(Some((0x3FFA_0000, 0x1000)), None, 0x1000_0000),
            Err(VclogDeny::OutsideRam)
        );
        let tail = vclog_window(None, Some((0x3C00_0000, 0x0400_0000)), RAM_END).unwrap();
        assert_eq!(tail.start, 0x3FF0_0000);
        assert_eq!(tail.end, 0x4000_0000);
        assert!(vclog_covers(FAR, 4, tail));
    }

    #[test]
    fn vclog_keeps_the_oldest_firmware_lines() {
        let mut toc = [0u8; 56];
        toc[0..4].copy_from_slice(&0x564c_4f47u32.to_le_bytes());
        toc[44..48].copy_from_slice(&0x1000u32.to_le_bytes());
        assert_eq!(vclog_msg_addr(&toc), Some(0x1000));
        let mut hdr = [0u8; 20];
        hdr[0..4].copy_from_slice(&0x5353_454du32.to_le_bytes());
        hdr[4..8].copy_from_slice(&0u32.to_le_bytes());
        hdr[8..12].copy_from_slice(&64u32.to_le_bytes());
        hdr[12..16].copy_from_slice(&0u32.to_le_bytes());
        hdr[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(vclog_msg_range(&hdr), Some((0, 64, 0, 0)));
        let mut buf = Vec::new();
        for text in ["HDMI1:EDID", "hdmi: clock off \"x\""] {
            let payload = 4 + text.len() + 1;
            let size = 8 + payload;
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(&(size as u16).to_le_bytes());
            buf.extend_from_slice(&[9, 9, 9, 9]);
            buf.extend_from_slice(text.as_bytes());
            buf.push(0);
        }
        let mut out = [0u8; 64];
        let n = vclog_tail(&buf, 0, buf.len(), &mut out);
        assert_eq!(&out[..n], b"HDMI1:EDID | hdmi: clock off  x ");
    }

    fn push_entry(buf: &mut Vec<u8>, time_ms: u32, text: &str) {
        let payload = 4 + text.len() + 1;
        let size = 8 + payload;
        buf.extend_from_slice(&time_ms.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&(size as u16).to_le_bytes());
        buf.extend_from_slice(&[9, 9, 9, 9]);
        buf.extend_from_slice(text.as_bytes());
        buf.push(0);
    }

    #[test]
    fn vclog_renders_every_message_on_its_own_line() {
        let mut buf = Vec::new();
        push_entry(&mut buf, 1500, "HDMI1:EDID");
        push_entry(&mut buf, 0, "hdmi: clock off \"x\"");
        let mut out = [0u8; 128];
        let got = vclog_render_messages(&buf, &mut out);
        assert!(got.complete);
        assert_eq!(got.entries, 2);
        assert_eq!(
            &out[..got.bytes],
            b"000001.500: HDMI1:EDID\n000000.000: hdmi: clock off \"x\"\n"
        );
        let short = vclog_render_messages(&buf, &mut out[..12]);
        assert!(!short.complete);
        assert_eq!(short.entries, 0);
        assert_eq!(short.bytes, 0);
    }

    #[test]
    fn vclog_unwraps_the_ring_oldest_first() {
        let mut older = Vec::new();
        push_entry(&mut older, 1000, "older");
        let mut newer = Vec::new();
        push_entry(&mut newer, 2000, "newer");
        let mut ring = vec![0u8; newer.len() + 8 + older.len()];
        ring[..newer.len()].copy_from_slice(&newer);
        let read_off = newer.len() + 8;
        ring[read_off..read_off + older.len()].copy_from_slice(&older);
        let write_off = newer.len();
        let mut linear = [0u8; 64];
        let (n, truncated) = vclog_linearize(&ring, read_off, write_off, &mut linear);
        assert!(!truncated);
        let mut text = [0u8; 128];
        let got = vclog_render_messages(&linear[..n], &mut text);
        assert!(got.complete);
        assert_eq!(got.entries, 2);
        assert_eq!(
            &text[..got.bytes],
            b"000001.000: older\n000002.000: newer\n"
        );
    }

    #[test]
    fn vclog_renders_an_assert_line() {
        let mut toc = [0u8; 56];
        toc[0..4].copy_from_slice(&0x564c_4f47u32.to_le_bytes());
        toc[36..40].copy_from_slice(&0x2000u32.to_le_bytes());
        assert_eq!(vclog_assert_addr(&toc), Some(0x2000));
        let mut payload = Vec::new();
        payload.extend_from_slice(b"hdmi.c\0");
        payload.extend_from_slice(&42u32.to_le_bytes());
        payload.extend_from_slice(b"hpd\0");
        let mut buf = Vec::new();
        let size = 8 + payload.len();
        buf.extend_from_slice(&2500u32.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&(size as u16).to_le_bytes());
        buf.extend_from_slice(&payload);
        let mut out = [0u8; 96];
        let got = vclog_render_asserts(&buf, &mut out);
        assert!(got.complete);
        assert_eq!(
            &out[..got.bytes],
            b"000002.500: assert( hpd ) failed; hdmi.c line 42\n"
        );
    }
}

#[cfg(test)]
mod boot_pick_tests {
    use super::{choose_booted, contains_bytes, BootPick};

    #[test]
    fn file_id_beats_a_missing_tree_and_boot_mode_is_not_tryboot() {
        assert_eq!(
            choose_booted(Some(false), Some(true), None),
            BootPick::New
        );
        assert_eq!(choose_booted(Some(true), None, Some(true)), BootPick::Img);
        assert_eq!(
            choose_booted(Some(true), Some(true), Some(true)),
            BootPick::New
        );
        assert_eq!(choose_booted(Some(true), Some(true), None), BootPick::Unknown);
        assert_eq!(
            choose_booted(Some(false), Some(false), Some(true)),
            BootPick::New
        );
        assert_eq!(choose_booted(None, None, None), BootPick::Unknown);
        assert!(contains_bytes(b"xxpi4dns-boot-id:0.1.65yy", b"pi4dns-boot-id:0.1.65"));
        assert!(!contains_bytes(b"pi4dns-boot-id:0.1.64", b"pi4dns-boot-id:0.1.65"));
        assert!(contains_bytes(
            b"pi4dns-boot-id:0.1.65\0",
            b"pi4dns-boot-id:0.1.65\0"
        ));
        assert!(!contains_bytes(
            b"pi4dns-boot-id:0.1.650\0",
            b"pi4dns-boot-id:0.1.65\0"
        ));
    }
}
