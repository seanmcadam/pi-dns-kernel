//! HTTPS and HTTP downloads for block lists.
//!
//! Certificate time checks run only when the caller passes a synced Unix time.
//! Plain HTTP is accepted so a lab server and public hosts-format lists that
//! are still published over HTTP can be used. HTTPS checks the chain against
//! the baked-in roots, plus TESTCA.DER from the card when that file is present.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use embedded_io::{ErrorType, Read, Write};
use embedded_tls::blocking::{
    Aes128GcmSha256, Certificate, CertificateEntryRef, CertificateRef, CertificateVerifyRef,
    CryptoProvider, TlsCipherSuite, TlsClock, TlsConfig, TlsConnection, TlsContext, TlsError,
    TlsVerifier,
};
use embedded_tls::pki::CertVerifier;
use embedded_tls::SignatureScheme;
use p256::ecdsa::{signature::SignerMut, DerSignature, SigningKey};
use p256::SecretKey;

const YIELD: u64 = 1;
const TIME_MS: u64 = 4;
const UDP_RECV: u64 = 6;
const UDP_SEND: u64 = 7;
const TCP_RECV: u64 = 10;
const TCP_SEND: u64 = 11;
const TCP_CLOSE: u64 = 12;
const TCP_CONNECT: u64 = 79;
const BLOCK: u64 = 80;

const ISRG1: &[u8] = include_bytes!("../roots/isrgx1.der");
const ISRG2: &[u8] = include_bytes!("../roots/isrgx2.der");
const DIGI: &[u8] = include_bytes!("../roots/digicertg2.der");

static UNIX: AtomicI64 = AtomicI64::new(0);
static DNS_FD: AtomicI64 = AtomicI64::new(-1);

pub fn set_dns(fd: i64) {
    DNS_FD.store(fd, Ordering::Relaxed);
}
static mut EXTRA: [u8; 2048] = [0; 2048];
static EXTRA_LEN: AtomicUsize = AtomicUsize::new(0);

pub struct SysClock;

impl TlsClock for SysClock {
    fn now() -> Option<u64> {
        let now = UNIX.load(Ordering::Relaxed);
        if now <= 0 {
            None
        } else {
            Some(now as u64)
        }
    }
}

struct Rng(u64);

impl rand_core::RngCore for Rng {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if x == 0 {
            x = 0x9e37_79b9_7f4a_7c15;
        }
        self.0 = x;
        x
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let next = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&next[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for Rng {}

type Verifier<'a> = CertVerifier<'a, Aes128GcmSha256, SysClock, 6144>;

struct Roots<'a> {
    a: Box<Verifier<'a>>,
    b: Box<Verifier<'a>>,
    c: Box<Verifier<'a>>,
    extra: Option<Box<Verifier<'a>>>,
    winner: u8,
}

impl TlsVerifier<Aes128GcmSha256> for Roots<'_> {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.a.set_hostname_verification(hostname)?;
        self.b.set_hostname_verification(hostname)?;
        self.c.set_hostname_verification(hostname)?;
        if let Some(extra) = self.extra.as_mut() {
            extra.set_hostname_verification(hostname)?;
        }
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &<Aes128GcmSha256 as TlsCipherSuite>::Hash,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        let Some(der) = last_der(&cert) else {
            return Err(TlsError::InvalidCertificate);
        };
        let issuer = blocklist::cert_name(der, false);
        let pick = if issuer == blocklist::cert_name(ISRG1, true) {
            0
        } else if issuer == blocklist::cert_name(ISRG2, true) {
            1
        } else if issuer == blocklist::cert_name(DIGI, true) {
            2
        } else if self.extra.is_some()
            && issuer == blocklist::cert_name(extra_bytes(), true)
        {
            3
        } else {
            return Err(TlsError::InvalidCertificate);
        };
        self.winner = pick;
        match pick {
            0 => self.a.verify_certificate(transcript, cert),
            1 => self.b.verify_certificate(transcript, cert),
            2 => self.c.verify_certificate(transcript, cert),
            _ => self
                .extra
                .as_mut()
                .ok_or(TlsError::InvalidCertificate)?
                .verify_certificate(transcript, cert),
        }
    }

    fn verify_signature(
        &mut self,
        verify: CertificateVerifyRef,
    ) -> Result<(), TlsError> {
        match self.winner {
            0 => self.a.verify_signature(verify),
            1 => self.b.verify_signature(verify),
            2 => self.c.verify_signature(verify),
            _ => self
                .extra
                .as_mut()
                .ok_or(TlsError::InvalidCertificate)?
                .verify_signature(verify),
        }
    }
}

fn last_der<'a>(cert: &CertificateRef<'a>) -> Option<&'a [u8]> {
    match cert.entries.last()? {
        CertificateEntryRef::X509(der) => Some(*der),
        CertificateEntryRef::RawPublicKey(_) => None,
    }
}

fn extra_bytes() -> &'static [u8] {
    let len = EXTRA_LEN.load(Ordering::Relaxed);
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(EXTRA).cast(), len) }
}

struct Provider<'a> {
    roots: Roots<'a>,
    rng: Rng,
}

impl CryptoProvider for Provider<'_> {
    type CipherSuite = Aes128GcmSha256;
    type Signature = DerSignature;

    fn rng(&mut self) -> impl rand_core::CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Aes128GcmSha256>, TlsError> {
        Ok(&mut self.roots)
    }

    fn signer(
        &mut self,
    ) -> Result<(impl SignerMut<Self::Signature>, SignatureScheme), TlsError> {
        let secret = SecretKey::from_sec1_der(&[0u8; 0]).map_err(|_| TlsError::InvalidPrivateKey)?;
        Ok((
            SigningKey::from(&secret),
            SignatureScheme::EcdsaSecp256r1Sha256,
        ))
    }
}

#[derive(Debug)]
struct IoErr;

impl core::fmt::Display for IoErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("io")
    }
}

impl core::error::Error for IoErr {}

impl embedded_io::Error for IoErr {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

struct Sock {
    fd: i64,
    deadline: u64,
}

impl ErrorType for Sock {
    type Error = IoErr;
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoErr> {
        let cap = buf.len().min(1024);
        loop {
            if time_ms() > self.deadline {
                return Err(IoErr);
            }
            let n = sys(
                TCP_RECV,
                self.fd as u64,
                buf.as_mut_ptr() as u64,
                cap as u64,
                1,
                0,
                0,
            );
            if n > 0 {
                return Ok(n as usize);
            }
            if n == 0 {
                return Ok(0);
            }
            if n != -11 {
                return Err(IoErr);
            }
            let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
        }
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoErr> {
        let mut sent = 0;
        while sent < buf.len() {
            if time_ms() > self.deadline {
                return Err(IoErr);
            }
            let end = (sent + 1024).min(buf.len());
            let n = sys(
                TCP_SEND,
                self.fd as u64,
                buf[sent..end].as_ptr() as u64,
                (end - sent) as u64,
                0,
                0,
                0,
            );
            if n < 0 {
                return Err(IoErr);
            }
            if n == 0 {
                let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
                continue;
            }
            sent += n as usize;
        }
        Ok(sent)
    }

    fn flush(&mut self) -> Result<(), IoErr> {
        Ok(())
    }
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let rc: i64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => rc,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
            options(nostack),
        );
    }
    rc
}

fn time_ms() -> u64 {
    sys(TIME_MS, 0, 0, 0, 0, 0, 0) as u64
}

struct Url<'a> {
    https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Result<Url<'_>, &'static str> {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("url");
    };
    let (authority, path) = match rest.split_once('/') {
        Some((host, path)) => (host, path),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.len() > 63 {
        return Err("host");
    }
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port: u16 = port.parse().map_err(|_| "port")?;
        if port == 0 {
            return Err("port");
        }
        (host, port)
    } else {
        (authority, if https { 443 } else { 80 })
    };
    if host.is_empty() {
        return Err("host");
    }
    let path = if path.is_empty() { "/" } else { path };
    Ok(Url {
        https,
        host,
        port,
        path,
    })
}

fn ipv4(text: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut n = 0;
    for part in text.split('.') {
        if n == 4 {
            return None;
        }
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
            return None;
        }
        ip[n] = part.parse().ok()?;
        n += 1;
    }
    if n == 4 { Some(ip) } else { None }
}

fn resolve(host: &str, dns: Option<[u8; 4]>) -> Result<[u8; 4], &'static str> {
    if let Some(ip) = ipv4(host) {
        return Ok(ip);
    }
    let Some(dns) = dns else {
        return Err("dns");
    };
    let mut query = [0u8; 512];
    let id = (time_ms() as u16) ^ 0x4b31;
    let len = ntp::write_a_query(&mut query, id, host.as_bytes()).ok_or("name")?;
    let fd = DNS_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return Err("dns");
    }
    let packed = u32::from_be_bytes(dns);
    if sys(UDP_SEND, fd as u64, query.as_ptr() as u64, len as u64, packed as u64, 53, 0) != 0 {
        return Err("dns");
    }
    let deadline = time_ms().saturating_add(2000);
    let mut buf = [0u8; 512];
    let mut addr = [0u8; 8];
    while time_ms() < deadline {
        let n = sys(
            UDP_RECV,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            addr.as_mut_ptr() as u64,
            1,
            0,
        );
        if n > 0 {
            if let Some(ip) = ntp::read_a(&buf[..n as usize], id) {
                return Ok(ip);
            }
        }
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
    Err("dns")
}

fn load_extra() {
    if EXTRA_LEN.load(Ordering::Relaxed) != 0 {
        return;
    }
    let n = sys(
        BLOCK,
        12,
        core::ptr::addr_of_mut!(EXTRA) as u64,
        2048,
        0,
        0,
        0,
    );
    if n > 0 {
        EXTRA_LEN.store(n as usize, Ordering::Relaxed);
    }
}

fn connect(ip: [u8; 4], port: u16) -> Result<i64, &'static str> {
    let fd = sys(TCP_CONNECT, u32::from_be_bytes(ip) as u64, port as u64, 0, 0, 0, 0);
    if fd < 0 {
        Err("connect")
    } else {
        Ok(fd)
    }
}

fn http_body(sock: &mut impl Read, cap: usize) -> Result<Vec<u8>, &'static str> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) if !raw.is_empty() => break,
            Err(_) => return Err("io"),
        };
        if raw.len() + n > cap {
            return Err("size");
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let head = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("http")?;
    let status = raw.get(..12).ok_or("http")?;
    if !status.windows(3).any(|w| w == b"200") {
        return Err("http");
    }
    Ok(raw.split_off(head + 4))
}

fn request_bytes(url: &Url<'_>) -> Result<Vec<u8>, &'static str> {
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    if url.path.starts_with('/') {
        req.extend_from_slice(url.path.as_bytes());
    } else {
        req.extend_from_slice(b"/");
        req.extend_from_slice(url.path.as_bytes());
    }
    req.extend_from_slice(b" HTTP/1.0\r\nHost: ");
    req.extend_from_slice(url.host.as_bytes());
    req.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    Ok(req)
}

pub fn download(url: &str, unix: i64, dns: Option<[u8; 4]>) -> Result<Vec<u8>, &'static str> {
    let parsed = parse_url(url)?;
    if parsed.https && unix <= 0 {
        return Err("clock");
    }
    let ip = resolve(parsed.host, dns)?;
    let fd = connect(ip, parsed.port)?;
    let deadline = time_ms().saturating_add(20_000);
    let mut sock = Sock { fd, deadline };
    let req = request_bytes(&parsed)?;
    let result = if parsed.https {
        UNIX.store(unix, Ordering::Relaxed);
        load_extra();
        https_get(&mut sock, parsed.host, &req)
    } else {
        sock.write_all(&req).map_err(|_| "io")?;
        http_body(&mut sock, blocklist::LIST_MAX_BYTES + 1)
    };
    let _ = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
    result
}

fn https_get(sock: &mut Sock, host: &str, req: &[u8]) -> Result<Vec<u8>, &'static str> {
    if host.len() > 63 {
        return Err("host");
    }
    load_extra();
    let extra = if EXTRA_LEN.load(Ordering::Relaxed) > 0 {
        Some(Box::new(CertVerifier::new(Certificate::X509(extra_bytes()))))
    } else {
        None
    };
    let mut provider = Box::new(Provider {
        roots: Roots {
            a: Box::new(CertVerifier::new(Certificate::X509(ISRG1))),
            b: Box::new(CertVerifier::new(Certificate::X509(ISRG2))),
            c: Box::new(CertVerifier::new(Certificate::X509(DIGI))),
            extra,
            winner: 0,
        },
        rng: Rng(time_ms() ^ 0x51ed_2701_d4c1),
    });
    let mut read_buf = vec![0u8; 16640];
    let mut write_buf = vec![0u8; 4096];
    let mut tls = TlsConnection::new(sock, &mut read_buf, &mut write_buf);
    let config = TlsConfig::new().with_server_name(host);
    tls.open(TlsContext::new(&config, provider.as_mut()))
        .map_err(|_| "tls")?;
    tls.write_all(req).map_err(|_| "tls")?;
    tls.flush().map_err(|_| "tls")?;
    http_body(&mut tls, blocklist::LIST_MAX_BYTES + 1)
}

