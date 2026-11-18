//! DHCP offer classification shared by the NICs and the host tests.
//!
//! A reply whose client hardware address is not ours is dropped. The bytes
//! are never rewritten: an offer for another client is not a lease we can
//! claim, and patching the live DMA buffer is what the old path did before
//! handing the frame to the stack.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Ethernet source on the frozen Pi 4 frame. A VRRP gateway.
pub const VRRP_MAC: [u8; 6] = [0x00, 0x00, 0x5e, 0x00, 0x01, 0x01];
/// Board MAC in that photo. The host test builds the same header.
pub const PHOTO_MAC: [u8; 6] = [0x98, 0xfe, 0x54, 0x82, 0x0b, 0x6e];

const FRAME_MAX: usize = 384;

#[allow(dead_code)]
static ARMED: AtomicBool = AtomicBool::new(false);
#[allow(dead_code)]
static LEN: AtomicUsize = AtomicUsize::new(0);
#[allow(dead_code)]
static mut FRAME: [u8; FRAME_MAX] = [0; FRAME_MAX];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Not a BOOTREPLY we are willing to judge. The stack may still see it.
    Ignore,
    /// UDP 67→68 BOOTREPLY whose chaddr is the board MAC.
    Pass,
    /// BOOTREPLY for a different client. Do not hand it to the stack.
    DropChaddr,
}

/// Stage one offer for the next `Device::receive`. The destination and the
/// chaddr are `mac`. The transaction id is fixed so it does not collide with
/// the interface PRNG; the stack parses the options and returns.
#[allow(dead_code)]
pub fn arm(mac: &[u8; 6]) {
    let mut buf = [0u8; FRAME_MAX];
    let n = write_offer(&mut buf, mac, mac, 0xffff_ffff);
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), core::ptr::addr_of_mut!(FRAME).cast(), n);
    }
    LEN.store(n, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
}

/// Take the staged offer, once. The bytes stay readable via [`copy_out`].
#[allow(dead_code)]
pub fn take_len() -> Option<u16> {
    if !ARMED.swap(false, Ordering::AcqRel) {
        return None;
    }
    Some(LEN.load(Ordering::Relaxed) as u16)
}

#[allow(dead_code)]
pub fn copy_out(dst: &mut [u8]) -> usize {
    let n = LEN.load(Ordering::Relaxed).min(dst.len());
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!(FRAME).cast(), dst.as_mut_ptr(), n);
    }
    n
}

/// Boot-time check used on virt, raspi4b, and the Pi boards. The line is
/// the serial marker the integration test greps for.
pub fn self_check_line() -> &'static str {
    let mut frame = [0u8; FRAME_MAX];
    let n = write_offer(&mut frame, &PHOTO_MAC, &PHOTO_MAC, 0xa5a5_a5a5);
    let header = n >= 16
        && frame[..16]
            == [
                0x98, 0xfe, 0x54, 0x82, 0x0b, 0x6e, 0x00, 0x00, 0x5e, 0x00, 0x01, 0x01, 0x08, 0x00,
                0x45, 0x00,
            ];
    let kept = verdict(&frame[..n], &PHOTO_MAC) == Verdict::Pass && home_router_options(&frame[..n]);
    let before = frame;
    let _ = verdict(&frame[..n], &PHOTO_MAC);
    let unchanged = frame == before;
    frame[70] ^= 0xff;
    let dropped = verdict(&frame[..n], &PHOTO_MAC) == Verdict::DropChaddr;
    if header && kept && unchanged && dropped {
        "[OK] DHCP offer probe kept src=00:00:5e:00:01:01"
    } else {
        "[FAIL] DHCP offer probe"
    }
}

pub fn verdict(frame: &[u8], mac: &[u8; 6]) -> Verdict {
    if !boot_reply(frame) {
        return Verdict::Ignore;
    }
    if frame[70..76] == *mac {
        Verdict::Pass
    } else {
        Verdict::DropChaddr
    }
}

/// Ethernet + IPv4 + UDP 67→68 BOOTREPLY with the options a home router
/// sends: message type, server id, mask, router, DNS, lease. Returns the
/// frame length, or 0 if `dst` is too small.
pub fn write_offer(dst: &mut [u8], eth_dst: &[u8; 6], chaddr: &[u8; 6], xid: u32) -> usize {
    // 14 ethernet + 20 IPv4 + 8 UDP + 240 DHCP + 34 options.
    const LEN: usize = 316;
    if dst.len() < LEN {
        return 0;
    }
    dst[..LEN].fill(0);
    dst[..6].copy_from_slice(eth_dst);
    dst[6..12].copy_from_slice(&VRRP_MAC);
    dst[12] = 0x08;
    dst[13] = 0x00;
    dst[14] = 0x45;
    dst[15] = 0x00;
    let ip_total: u16 = 302;
    dst[16] = (ip_total >> 8) as u8;
    dst[17] = ip_total as u8;
    dst[22] = 64;
    dst[23] = 17;
    dst[26] = 192;
    dst[27] = 168;
    dst[28] = 1;
    dst[29] = 1;
    dst[30] = 192;
    dst[31] = 168;
    dst[32] = 1;
    dst[33] = 50;
    let udp = 34;
    dst[udp] = 0;
    dst[udp + 1] = 67;
    dst[udp + 2] = 0;
    dst[udp + 3] = 68;
    let udp_len: u16 = 282;
    dst[udp + 4] = (udp_len >> 8) as u8;
    dst[udp + 5] = udp_len as u8;
    let dhcp = 42;
    dst[dhcp] = 2;
    dst[dhcp + 1] = 1;
    dst[dhcp + 2] = 6;
    dst[dhcp + 4] = (xid >> 24) as u8;
    dst[dhcp + 5] = (xid >> 16) as u8;
    dst[dhcp + 6] = (xid >> 8) as u8;
    dst[dhcp + 7] = xid as u8;
    dst[dhcp + 16] = 192;
    dst[dhcp + 17] = 168;
    dst[dhcp + 18] = 1;
    dst[dhcp + 19] = 50;
    dst[dhcp + 20] = 192;
    dst[dhcp + 21] = 168;
    dst[dhcp + 22] = 1;
    dst[dhcp + 23] = 1;
    dst[dhcp + 28..dhcp + 34].copy_from_slice(chaddr);
    dst[dhcp + 236] = 0x63;
    dst[dhcp + 237] = 0x82;
    dst[dhcp + 238] = 0x53;
    dst[dhcp + 239] = 0x63;
    let mut o = dhcp + 240;
    o = put_opt(dst, o, 53, &[2]);
    o = put_opt(dst, o, 54, &[192, 168, 1, 1]);
    o = put_opt(dst, o, 1, &[255, 255, 255, 0]);
    o = put_opt(dst, o, 3, &[192, 168, 1, 1]);
    o = put_opt(dst, o, 6, &[192, 168, 1, 1]);
    o = put_opt(dst, o, 51, &[0x00, 0x01, 0x51, 0x80]);
    dst[o] = 255;
    let ip_sum = finish(add(0, &dst[14..34]));
    dst[24] = (ip_sum >> 8) as u8;
    dst[25] = ip_sum as u8;
    let mut sum = add(0, &dst[26..34]);
    sum += 17;
    sum += udp_len as u32;
    sum = add(sum, &dst[udp..udp + udp_len as usize]);
    let udp_sum = finish(sum);
    let udp_sum = if udp_sum == 0 { 0xffff } else { udp_sum };
    dst[udp + 6] = (udp_sum >> 8) as u8;
    dst[udp + 7] = udp_sum as u8;
    LEN
}

fn put_opt(dst: &mut [u8], at: usize, kind: u8, data: &[u8]) -> usize {
    dst[at] = kind;
    dst[at + 1] = data.len() as u8;
    dst[at + 2..at + 2 + data.len()].copy_from_slice(data);
    at + 2 + data.len()
}

fn boot_reply(frame: &[u8]) -> bool {
    if frame.len() < 42 + 240 {
        return false;
    }
    if frame[12] != 0x08 || frame[13] != 0x00 || frame[14] >> 4 != 4 {
        return false;
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    if ihl < 20 || frame.len() < 14 + ihl + 8 {
        return false;
    }
    if frame[14 + 9] != 17 {
        return false;
    }
    let udp = 14 + ihl;
    if frame[udp] != 0 || frame[udp + 1] != 67 || frame[udp + 2] != 0 || frame[udp + 3] != 68 {
        return false;
    }
    let dhcp = udp + 8;
    if frame.len() < dhcp + 240 {
        return false;
    }
    frame[dhcp] == 2
        && frame[dhcp + 236] == 0x63
        && frame[dhcp + 237] == 0x82
        && frame[dhcp + 238] == 0x53
        && frame[dhcp + 239] == 0x63
}

fn home_router_options(frame: &[u8]) -> bool {
    option_is(frame, 53, &[2])
        && option_is(frame, 54, &[192, 168, 1, 1])
        && option_is(frame, 1, &[255, 255, 255, 0])
        && option_is(frame, 3, &[192, 168, 1, 1])
        && option_is(frame, 6, &[192, 168, 1, 1])
        && option_is(frame, 51, &[0x00, 0x01, 0x51, 0x80])
}

fn option_is(frame: &[u8], kind: u8, data: &[u8]) -> bool {
    let ihl = (frame[14] & 0x0f) as usize * 4;
    let mut o = 14 + ihl + 8 + 240;
    while o < frame.len() {
        let k = frame[o];
        if k == 255 {
            return false;
        }
        if k == 0 {
            o += 1;
            continue;
        }
        if o + 1 >= frame.len() {
            return false;
        }
        let n = frame[o + 1] as usize;
        if o + 2 + n > frame.len() {
            return false;
        }
        if k == kind && &frame[o + 2..o + 2 + n] == data {
            return true;
        }
        o += 2 + n;
    }
    false
}

fn add(mut sum: u32, data: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    sum
}

/// Gateway installed with a DHCP lease.
///
/// The router option wins. The QEMU fallback `10.0.2.2` is only used
/// when it sits on the leased subnet. Otherwise an off-subnet default
/// route blackholes every forwarded query while on-link HTTP and local
/// records still work. An on-link DHCP server is the next choice, then
/// the first host address of the leased subnet.
pub fn lease_gateway(
    ip: [u8; 4],
    mask: [u8; 4],
    router: Option<[u8; 4]>,
    server: [u8; 4],
    fallback: [u8; 4],
) -> [u8; 4] {
    if let Some(router) = router {
        return router;
    }
    if on_link(ip, mask, fallback) {
        return fallback;
    }
    if server != ip && server != [0, 0, 0, 0] && on_link(ip, mask, server) {
        return server;
    }
    let network = u32::from_be_bytes(ip) & u32::from_be_bytes(mask);
    let gateway = network | 1;
    if gateway == u32::from_be_bytes(ip) {
        return server;
    }
    gateway.to_be_bytes()
}

fn on_link(ip: [u8; 4], mask: [u8; 4], other: [u8; 4]) -> bool {
    let mask = u32::from_be_bytes(mask);
    if mask == 0 {
        return false;
    }
    let ip = u32::from_be_bytes(ip);
    let other = u32::from_be_bytes(other);
    ip & mask == other & mask
}

fn finish(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !sum as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_gateway_keeps_router_and_rejects_qemu_fallback() {
        let leased = [172, 18, 3, 137];
        let mask = [255, 255, 255, 0];
        let qemu = [10, 0, 2, 2];
        assert_eq!(
            lease_gateway(leased, mask, Some([172, 18, 3, 1]), [172, 18, 3, 1], qemu),
            [172, 18, 3, 1]
        );
        assert_eq!(
            lease_gateway(leased, mask, None, [172, 18, 3, 1], qemu),
            [172, 18, 3, 1]
        );
        assert_eq!(
            lease_gateway(leased, mask, None, [8, 8, 8, 8], qemu),
            [172, 18, 3, 1]
        );
        assert_eq!(
            lease_gateway([10, 0, 2, 15], mask, None, [10, 0, 2, 3], qemu),
            qemu
        );
    }

    #[test]
    fn photo_header_and_no_rewrite() {
        let line = self_check_line();
        assert!(line.starts_with("[OK]"), "{line}");
        let mut frame = [0u8; FRAME_MAX];
        let n = write_offer(&mut frame, &PHOTO_MAC, &PHOTO_MAC, 0xa5a5_a5a5);
        assert_eq!(
            &frame[..16],
            &[
                0x98, 0xfe, 0x54, 0x82, 0x0b, 0x6e, 0x00, 0x00, 0x5e, 0x00, 0x01, 0x01, 0x08, 0x00,
                0x45, 0x00
            ]
        );
        assert_eq!(verdict(&frame[..n], &PHOTO_MAC), Verdict::Pass);
        frame[70] ^= 1;
        assert_eq!(verdict(&frame[..n], &PHOTO_MAC), Verdict::DropChaddr);
    }

    #[test]
    fn matching_offer_returns_and_requests() {
        let mac = PHOTO_MAC;
        let mut dev = Pipe::default();
        let mut iface = iface(&mac);
        let mut sockets = smoltcp::iface::SocketSet::new(Vec::new());
        sockets.add(smoltcp::socket::dhcpv4::Socket::new());
        let _ = iface.poll(smoltcp::time::Instant::from_millis(0), &mut dev, &mut sockets);
        let discover = dev.tx.last().cloned().expect("discover");
        assert_eq!(discover[14] >> 4, 4, "discover is not ipv4");
        let xid = u32::from_be_bytes([discover[46], discover[47], discover[48], discover[49]]);
        let mut offer = [0u8; FRAME_MAX];
        let n = write_offer(&mut offer, &mac, &mac, xid);
        dev.rx.push(offer[..n].to_vec());
        let before = dev.tx.len();
        let _ = iface.poll(smoltcp::time::Instant::from_millis(1_000), &mut dev, &mut sockets);
        assert!(
            dev.tx[before..].iter().any(|frame| dhcp_type(frame) == Some(3)),
            "stack did not emit a DHCP REQUEST"
        );
    }

    #[test]
    fn foreign_chaddr_does_not_request() {
        let mac = PHOTO_MAC;
        let mut dev = Pipe::default();
        let mut iface = iface(&mac);
        let mut sockets = smoltcp::iface::SocketSet::new(Vec::new());
        sockets.add(smoltcp::socket::dhcpv4::Socket::new());
        let _ = iface.poll(smoltcp::time::Instant::from_millis(0), &mut dev, &mut sockets);
        let discover = dev.tx.last().cloned().expect("discover");
        let xid = u32::from_be_bytes([discover[46], discover[47], discover[48], discover[49]]);
        let mut other = mac;
        other[5] ^= 0xff;
        let mut offer = [0u8; FRAME_MAX];
        let n = write_offer(&mut offer, &mac, &other, xid);
        assert_eq!(verdict(&offer[..n], &mac), Verdict::DropChaddr);
        dev.rx.push(offer[..n].to_vec());
        let before = dev.tx.len();
        let _ = iface.poll(smoltcp::time::Instant::from_millis(1_000), &mut dev, &mut sockets);
        assert!(
            dev.tx[before..].iter().all(|frame| dhcp_type(frame) != Some(3)),
            "a foreign chaddr produced a REQUEST"
        );
    }

    fn dhcp_type(frame: &[u8]) -> Option<u8> {
        if frame.len() < 42 + 240 {
            return None;
        }
        let mut o = 42 + 240;
        while o + 2 < frame.len() {
            let k = frame[o];
            if k == 255 {
                return None;
            }
            if k == 0 {
                o += 1;
                continue;
            }
            let n = frame[o + 1] as usize;
            if k == 53 && n == 1 {
                return Some(frame[o + 2]);
            }
            o += 2 + n;
        }
        None
    }

    fn iface(mac: &[u8; 6]) -> smoltcp::iface::Interface {
        let eth = smoltcp::wire::EthernetAddress::from_bytes(mac);
        let mut config = smoltcp::iface::Config::new(eth.into());
        config.random_seed = 0x1234_5678_9abc_def0;
        let mut dev = Pipe::default();
        smoltcp::iface::Interface::new(config, &mut dev, smoltcp::time::Instant::from_millis(0))
    }

    #[derive(Default)]
    struct Pipe {
        rx: Vec<Vec<u8>>,
        tx: Vec<Vec<u8>>,
    }

    struct RxTok {
        pkt: Vec<u8>,
    }

    struct TxTok<'a> {
        tx: &'a mut Vec<Vec<u8>>,
    }

    impl smoltcp::phy::Device for Pipe {
        type RxToken<'a> = RxTok;
        type TxToken<'a> = TxTok<'a>;

        fn receive(
            &mut self,
            _timestamp: smoltcp::time::Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let pkt = self.rx.pop()?;
            Some((RxTok { pkt }, TxTok { tx: &mut self.tx }))
        }

        fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
            Some(TxTok { tx: &mut self.tx })
        }

        fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
            let mut caps = smoltcp::phy::DeviceCapabilities::default();
            caps.max_transmission_unit = 1514;
            caps.medium = smoltcp::phy::Medium::Ethernet;
            caps
        }
    }

    impl smoltcp::phy::RxToken for RxTok {
        fn consume<R, F>(mut self, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            f(&mut self.pkt)
        }
    }

    impl smoltcp::phy::TxToken for TxTok<'_> {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            let mut buf = vec![0u8; len];
            let result = f(&mut buf);
            self.tx.push(buf);
            result
        }
    }
}
