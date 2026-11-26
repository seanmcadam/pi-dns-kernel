//! DHCP offer classification shared by the NICs and the host tests.
//!
//! A reply whose client hardware address is not ours is dropped. The bytes
//! are never rewritten: an offer for another client is not a lease we can
//! claim, and patching the live DMA buffer is what the old path did before
//! handing the frame to the stack.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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

/// Ethernet source of the last BOOTREPLY whose chaddr was ours. Bit 63
/// means a MAC has been stored. The low 48 bits are that MAC.
#[allow(dead_code)]
static REPLY_MAC: AtomicU64 = AtomicU64::new(0);
#[allow(dead_code)]
static ARP_ARMED: AtomicBool = AtomicBool::new(false);
#[allow(dead_code)]
static ARP_LEN: AtomicUsize = AtomicUsize::new(0);
#[allow(dead_code)]
static mut ARP_FRAME: [u8; 64] = [0; 64];

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

/// Remember the Ethernet source of a DHCP reply that belongs to this board.
/// The on-link relay (the gateway) is that source. A later neighbor install
/// uses it when an ARP reply for the gateway does not arrive.
#[allow(dead_code)]
pub fn note_reply_mac(mac: [u8; 6]) {
    if mac == [0; 6] || mac == [0xff; 6] || mac[0] & 1 != 0 {
        return;
    }
    let mut packed = 1u64 << 63;
    for (index, byte) in mac.iter().enumerate() {
        packed |= (*byte as u64) << (index * 8);
    }
    REPLY_MAC.store(packed, Ordering::Release);
}

#[allow(dead_code)]
pub fn reply_mac() -> Option<[u8; 6]> {
    let packed = REPLY_MAC.load(Ordering::Acquire);
    if packed & (1 << 63) == 0 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        *byte = (packed >> (index * 8)) as u8;
    }
    Some(mac)
}

/// One unicast ARP reply, `gw_ip` is at `gw_mac`, aimed at `our_mac`.
/// smoltcp learns neighbors only from ARP, so this is how the gateway
/// MAC on the DHCP frame becomes a route next hop.
pub fn write_gateway_arp(
    dst: &mut [u8],
    our_mac: &[u8; 6],
    our_ip: [u8; 4],
    gw_mac: &[u8; 6],
    gw_ip: [u8; 4],
) -> usize {
    const LEN: usize = 60;
    if dst.len() < LEN {
        return 0;
    }
    dst[..LEN].fill(0);
    dst[..6].copy_from_slice(our_mac);
    dst[6..12].copy_from_slice(gw_mac);
    dst[12] = 0x08;
    dst[13] = 0x06;
    dst[14] = 0x00;
    dst[15] = 0x01;
    dst[16] = 0x08;
    dst[17] = 0x00;
    dst[18] = 6;
    dst[19] = 4;
    dst[20] = 0x00;
    dst[21] = 0x02;
    dst[22..28].copy_from_slice(gw_mac);
    dst[28..32].copy_from_slice(&gw_ip);
    dst[32..38].copy_from_slice(our_mac);
    dst[38..42].copy_from_slice(&our_ip);
    LEN
}

#[allow(dead_code)]
pub fn stage_gateway_arp(
    our_mac: &[u8; 6],
    our_ip: [u8; 4],
    mask: [u8; 4],
    gw_mac: &[u8; 6],
    gw_ip: [u8; 4],
) -> bool {
    if gw_ip == our_ip || !on_link(our_ip, mask, gw_ip) {
        return false;
    }
    if *gw_mac == [0; 6] || gw_mac[0] & 1 != 0 {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = write_gateway_arp(&mut buf, our_mac, our_ip, gw_mac, gw_ip);
    if n == 0 {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), core::ptr::addr_of_mut!(ARP_FRAME).cast(), n);
    }
    ARP_LEN.store(n, Ordering::Relaxed);
    ARP_ARMED.store(true, Ordering::Release);
    true
}

#[allow(dead_code)]
pub fn take_arp_len() -> Option<u16> {
    if !ARP_ARMED.swap(false, Ordering::AcqRel) {
        return None;
    }
    Some(ARP_LEN.load(Ordering::Relaxed) as u16)
}

#[allow(dead_code)]
pub fn copy_arp(dst: &mut [u8]) -> usize {
    let n = ARP_LEN.load(Ordering::Relaxed).min(dst.len());
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!(ARP_FRAME).cast(), dst.as_mut_ptr(), n);
    }
    n
}

/// The staged reply is still waiting for `Device::receive`.
#[allow(dead_code)]
pub fn arp_pending() -> bool {
    ARP_ARMED.load(Ordering::Acquire)
}

/// The boot injection is logged once. Later refreshes of the same reply
/// would otherwise scroll the telnet log every refresh period.
#[allow(dead_code)]
static INJECT_LOGGED: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn log_inject() -> bool {
    !INJECT_LOGGED.swap(true, Ordering::Relaxed)
}

/// Broadcast ARP request for `gw_ip`. A seeded unicast reply fills our
/// cache and then the stack does not transmit a request of its own. The
/// gateway on this LAN did not forward until it had answered one.
pub fn write_gateway_request(
    dst: &mut [u8],
    our_mac: &[u8; 6],
    our_ip: [u8; 4],
    gw_ip: [u8; 4],
) -> usize {
    const LEN: usize = 60;
    if dst.len() < LEN {
        return 0;
    }
    dst[..LEN].fill(0);
    dst[..6].fill(0xff);
    dst[6..12].copy_from_slice(our_mac);
    dst[12] = 0x08;
    dst[13] = 0x06;
    dst[14] = 0x00;
    dst[15] = 0x01;
    dst[16] = 0x08;
    dst[17] = 0x00;
    dst[18] = 6;
    dst[19] = 4;
    dst[20] = 0x00;
    dst[21] = 0x01;
    dst[22..28].copy_from_slice(our_mac);
    dst[28..32].copy_from_slice(&our_ip);
    dst[38..42].copy_from_slice(&gw_ip);
    LEN
}

/// Option 6 addresses from a BOOTREPLY, in order, up to `out.len()`.
/// The stack's DHCP parser keeps three and drops the rest.
#[allow(dead_code)]
pub fn dns_from_bootreply(frame: &[u8], out: &mut [[u8; 4]]) -> usize {
    if !boot_reply(frame) || out.is_empty() {
        return 0;
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    let mut o = 14 + ihl + 8 + 240;
    let mut nout = 0usize;
    while o < frame.len() && nout < out.len() {
        let kind = frame[o];
        if kind == 255 {
            break;
        }
        if kind == 0 {
            o += 1;
            continue;
        }
        if o + 1 >= frame.len() {
            break;
        }
        let n = frame[o + 1] as usize;
        if o + 2 + n > frame.len() {
            break;
        }
        if kind == 6 {
            let mut p = 0;
            while p + 4 <= n && nout < out.len() {
                let ip = [
                    frame[o + 2 + p],
                    frame[o + 2 + p + 1],
                    frame[o + 2 + p + 2],
                    frame[o + 2 + p + 3],
                ];
                if ip != [0, 0, 0, 0] {
                    out[nout] = ip;
                    nout += 1;
                }
                p += 4;
            }
        }
        o += 2 + n;
    }
    nout
}

/// `10.0.2.3` is the QEMU user-mode resolver. Keep it on a 10.0.2.0/24
/// lease and before any lease is known. Drop it on any other lease.
pub fn omit_slirp_dns(server: [u8; 4], lease: [u8; 4]) -> bool {
    if server != [10, 0, 2, 3] {
        return false;
    }
    if lease == [0, 0, 0, 0] {
        return false;
    }
    !(lease[0] == 10 && lease[1] == 0 && lease[2] == 2)
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
    let kept =
        verdict(&frame[..n], &PHOTO_MAC) == Verdict::Pass && home_router_options(&frame[..n]);
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
        // The host fault-log crate compiles this file without the client.
        #[cfg(target_os = "none")]
        crate::ntp_client::note_bootreply(frame);
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
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(0),
            &mut dev,
            &mut sockets,
        );
        let discover = dev.tx.last().cloned().expect("discover");
        assert_eq!(discover[14] >> 4, 4, "discover is not ipv4");
        let xid = u32::from_be_bytes([discover[46], discover[47], discover[48], discover[49]]);
        let mut offer = [0u8; FRAME_MAX];
        let n = write_offer(&mut offer, &mac, &mac, xid);
        dev.rx.push(offer[..n].to_vec());
        let before = dev.tx.len();
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(1_000),
            &mut dev,
            &mut sockets,
        );
        assert!(
            dev.tx[before..]
                .iter()
                .any(|frame| dhcp_type(frame) == Some(3)),
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
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(0),
            &mut dev,
            &mut sockets,
        );
        let discover = dev.tx.last().cloned().expect("discover");
        let xid = u32::from_be_bytes([discover[46], discover[47], discover[48], discover[49]]);
        let mut other = mac;
        other[5] ^= 0xff;
        let mut offer = [0u8; FRAME_MAX];
        let n = write_offer(&mut offer, &mac, &other, xid);
        assert_eq!(verdict(&offer[..n], &mac), Verdict::DropChaddr);
        dev.rx.push(offer[..n].to_vec());
        let before = dev.tx.len();
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(1_000),
            &mut dev,
            &mut sockets,
        );
        assert!(
            dev.tx[before..]
                .iter()
                .all(|frame| dhcp_type(frame) != Some(3)),
            "a foreign chaddr produced a REQUEST"
        );
    }

    #[test]
    fn gateway_arp_reply_is_unicast_and_fills_the_neighbor() {
        note_reply_mac(VRRP_MAC);
        assert_eq!(reply_mac(), Some(VRRP_MAC));
        note_reply_mac([0xff; 6]);
        assert_eq!(reply_mac(), Some(VRRP_MAC));

        let mac = PHOTO_MAC;
        let our_ip = [172, 18, 3, 137];
        let gw_ip = [172, 18, 3, 1];
        let mask = [255, 255, 255, 0];
        assert!(stage_gateway_arp(&mac, our_ip, mask, &VRRP_MAC, gw_ip));
        let n = take_arp_len().expect("staged") as usize;
        let mut frame = [0u8; 64];
        assert_eq!(copy_arp(&mut frame), n);
        assert_eq!(&frame[..6], &mac);
        assert_eq!(&frame[6..12], &VRRP_MAC);
        assert_eq!(&frame[12..14], &[0x08, 0x06]);
        assert_eq!(&frame[20..22], &[0x00, 0x02]);
        assert_eq!(&frame[28..32], &gw_ip);
        assert!(!stage_gateway_arp(
            &mac,
            our_ip,
            mask,
            &VRRP_MAC,
            [8, 8, 8, 8]
        ));

        let mut request = [0u8; 60];
        assert_eq!(write_gateway_request(&mut request, &mac, our_ip, gw_ip), 60);
        assert_eq!(&request[..6], &[0xff; 6]);
        assert_eq!(&request[6..12], &mac);
        assert_eq!(&request[20..22], &[0x00, 0x01]);
        assert_eq!(&request[28..32], &our_ip);
        assert_eq!(&request[32..38], &[0; 6]);
        assert_eq!(&request[38..42], &gw_ip);

        let mut dev = Pipe::default();
        let eth = smoltcp::wire::EthernetAddress::from_bytes(&mac);
        let mut config = smoltcp::iface::Config::new(eth.into());
        config.random_seed = 1;
        let mut iface = smoltcp::iface::Interface::new(
            config,
            &mut dev,
            smoltcp::time::Instant::from_millis(0),
        );
        let ip = smoltcp::wire::Ipv4Address::from_bytes(&our_ip);
        iface.update_ip_addrs(|addrs| {
            addrs.clear();
            addrs
                .push(smoltcp::wire::IpCidr::new(ip.into(), 24))
                .unwrap();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(smoltcp::wire::Ipv4Address::from_bytes(&gw_ip))
            .unwrap();
        dev.rx.push(frame[..n].to_vec());
        let mut sockets = smoltcp::iface::SocketSet::new(Vec::new());
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(1),
            &mut dev,
            &mut sockets,
        );

        let rx = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::storage::PacketMetadata::EMPTY; 2],
            vec![0u8; 256],
        );
        let tx = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::storage::PacketMetadata::EMPTY; 2],
            vec![0u8; 256],
        );
        let mut sock = smoltcp::socket::udp::Socket::new(rx, tx);
        sock.bind(53535).unwrap();
        let handle = sockets.add(sock);
        sockets
            .get_mut::<smoltcp::socket::udp::Socket>(handle)
            .send_slice(
                b"hi",
                smoltcp::wire::IpEndpoint::new(
                    smoltcp::wire::Ipv4Address::new(8, 8, 8, 8).into(),
                    53,
                ),
            )
            .unwrap();
        let before = dev.tx.len();
        let _ = iface.poll(
            smoltcp::time::Instant::from_millis(2),
            &mut dev,
            &mut sockets,
        );
        let sent = dev.tx[before..]
            .iter()
            .find(|pkt| pkt.len() >= 34 && pkt[12] == 0x08 && pkt[13] == 0x00)
            .expect("off-link udp was not sent");
        assert_eq!(&sent[..6], &VRRP_MAC, "udp did not use the gateway mac");
        assert_eq!(&sent[30..34], &[8, 8, 8, 8]);
    }

    #[test]
    fn option6_keeps_the_fourth_server() {
        let servers = [
            [172, 18, 254, 4],
            [172, 18, 254, 5],
            [8, 8, 8, 8],
            [172, 18, 50, 10],
        ];
        let frame = frame_with_dns(&servers);
        let mut got = [[0u8; 4]; 8];
        assert_eq!(dns_from_bootreply(&frame, &mut got), 4);
        assert_eq!(&got[..4], &servers);
    }

    #[test]
    fn slirp_resolver_is_only_for_the_qemu_subnet() {
        assert!(omit_slirp_dns([10, 0, 2, 3], [172, 18, 3, 137]));
        assert!(!omit_slirp_dns([10, 0, 2, 3], [10, 0, 2, 15]));
        assert!(!omit_slirp_dns([10, 0, 2, 3], [0, 0, 0, 0]));
        assert!(!omit_slirp_dns([8, 8, 8, 8], [172, 18, 3, 137]));
        assert!(!omit_slirp_dns([172, 18, 50, 10], [172, 18, 3, 137]));
    }

    fn frame_with_dns(servers: &[[u8; 4]]) -> [u8; 384] {
        let mut frame = [0u8; 384];
        assert!(write_offer(&mut frame, &PHOTO_MAC, &PHOTO_MAC, 1) > 0);
        let dhcp = 42;
        for byte in &mut frame[dhcp + 240..] {
            *byte = 0;
        }
        let mut flat = [0u8; 32];
        let mut n = 0;
        for ip in servers {
            flat[n..n + 4].copy_from_slice(ip);
            n += 4;
        }
        let mut o = dhcp + 240;
        o = put_opt(&mut frame, o, 53, &[5]);
        o = put_opt(&mut frame, o, 6, &flat[..n]);
        frame[o] = 255;
        frame
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
