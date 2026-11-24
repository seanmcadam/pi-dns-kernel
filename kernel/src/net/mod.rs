pub(crate) mod rearm;
pub mod service;
mod stack;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use smoltcp::iface::SocketHandle;
use smoltcp::phy::{Device, TxToken};
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};

pub use crate::board::Nic as EthernetDevice;
pub use stack::NetworkStack;

pub(crate) struct NetworkState {
    stack: NetworkStack,
    device: EthernetDevice,
    dhcp: Option<DhcpClient>,
}

struct DhcpClient {
    handle: SocketHandle,
    started_ms: u64,
    timeout_ms: u64,
    fallback_ip: [u8; 4],
    fallback_netmask: [u8; 4],
    fallback_gateway: [u8; 4],
    finished: bool,
    /// First millisecond the link was down. Zero once the link is up.
    link_wait_ms: u64,
    link_noted: bool,
}

/// How long to wait for a PHY link before the configured DHCP timeout runs.
const LINK_HOLD_MS: u64 = 15_000;
/// Subnet, router, DNS, and NTP. The NTP option is 42.
const DHCP_PARAMS: &[u8] = &[1, 3, 6, 42];

/// 0 absent, 1 waiting, 2 leased, 3 fallback, 4 static.
static DHCP_PHASE: AtomicU8 = AtomicU8::new(0);
static DHCP_IP: AtomicU32 = AtomicU32::new(0);

/// True once a static address, a DHCP lease, or the DHCP fallback is
/// installed. Health probes stay quiet until then: there is no route,
/// and the pre-lease resolver list is not the one the lease will keep.
pub fn has_address() -> bool {
    DHCP_IP.load(Ordering::Relaxed) != 0
}
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
static ARP_PERIOD_MS: AtomicU64 = AtomicU64::new(0);
static ARP_NEXT_MS: AtomicU64 = AtomicU64::new(0);

/// Wire ARP requests that are not gratuitous announcements, replies we
/// actually received, IPv4 unicasts sent with a resolved neighbor, and
/// requests sent because the neighbor was missing. The telnet line is
/// limited to one per two seconds, so these are the real rates.
static ARP_REQUESTS_TX: AtomicU64 = AtomicU64::new(0);
static ARP_REPLIES_RX: AtomicU64 = AtomicU64::new(0);
static ARP_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static ARP_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Gateway we already learned from the DHCP reply. Refreshed locally so
/// smoltcp's 60 s neighbor lifetime does not put a who-has on the wire.
static GW_OUR: AtomicU32 = AtomicU32::new(0);
static GW_MASK: AtomicU32 = AtomicU32::new(0);
static GW_IP: AtomicU32 = AtomicU32::new(0);
static GW_MAC: AtomicU64 = AtomicU64::new(0);
static GW_REFRESH_MS: AtomicU64 = AtomicU64::new(0);
/// Shorter than smoltcp's 60 s `ENTRY_LIFETIME`. A lookup does not move
/// that stamp, so a quiet link would solicit the gateway every minute.
const GATEWAY_HOLD_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy)]
pub enum NetworkMode {
    Static {
        ip: [u8; 4],
        netmask: [u8; 4],
        gateway: [u8; 4],
    },
    /// Ask for an address, then use the fallback if no lease arrives in time.
    Dhcp {
        fallback_ip: [u8; 4],
        fallback_netmask: [u8; 4],
        fallback_gateway: [u8; 4],
        timeout_ms: u64,
    },
}

#[derive(Debug)]
pub enum NetworkError {
    DeviceNotFound,
}

pub fn init(config: &crate::config_manager::NetworkConfig) -> Result<(), NetworkError> {
    crate::board::set_phy_speed(config.speed_cap);
    ARP_PERIOD_MS.store(config.arp_secs.saturating_mul(1000), Ordering::Relaxed);
    log::info!("Detecting network device...");
    log::info!("Network device: {}", crate::board::nic_name());

    let mut device = match crate::board::probe_nic() {
        Some(device) => device,
        None => {
            DHCP_PHASE.store(0, Ordering::Relaxed);
            refresh_banner();
            return Err(NetworkError::DeviceNotFound);
        }
    };
    
    let mut stack = NetworkStack::new(&mut device);
    
    match config.mode {
        NetworkMode::Static {
            ip,
            netmask,
            gateway,
        } => {
            stack.configure_static(ip, netmask, gateway);
            crate::config_manager::note_lease(ip);
            DHCP_PHASE.store(4, Ordering::Relaxed);
            DHCP_IP.store(u32::from_be_bytes(ip), Ordering::Relaxed);
            log::info!(
                "Network configured with static IP: {}.{}.{}.{}",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
            console_line_ip("[OK] static IP", ip);
        }
        NetworkMode::Dhcp {
            fallback_ip,
            fallback_netmask,
            fallback_gateway,
            timeout_ms,
        } => {
            log::info!("Starting DHCP");
            console_line("[OK] DHCP waiting for a lease");
            DHCP_PHASE.store(1, Ordering::Relaxed);
            DHCP_IP.store(0, Ordering::Relaxed);
            // smoltcp waits 10 s between DISCOVER retries. The kernel used
            // to give up at 3 s, so the wire saw one frame and then the
            // static address. One second here, four seconds below.
            let mut dhcp = dhcpv4::Socket::new();
            dhcp.set_parameter_request_list(DHCP_PARAMS);
            let mut retry = dhcpv4::RetryConfig::default();
            retry.discover_timeout = Duration::from_secs(1);
            retry.initial_request_timeout = Duration::from_secs(1);
            retry.request_retries = 4;
            dhcp.set_retry_config(retry);
            let handle = stack.sockets_mut().add(dhcp);
            service::install(NetworkState {
                stack,
                device,
                dhcp: Some(DhcpClient {
                    handle,
                    started_ms: crate::timer::millis(),
                    timeout_ms,
                    fallback_ip,
                    fallback_netmask,
                    fallback_gateway,
                    finished: false,
                    link_wait_ms: 0,
                    link_noted: false,
                }),
            });
            refresh_banner();
            return Ok(());
        }
    }

    service::install(NetworkState {
        stack,
        device,
        dhcp: None,
    });

    refresh_banner();
    Ok(())
}

/// Drive the NIC. The network owner is the only caller that reaches the device.
///
/// A timer IRQ never calls this. Socket syscalls do not take a device lock;
/// they enqueue a request and sleep.
pub fn poll() {
    service::pump();
}

pub fn polling_here() -> bool {
    service::polling_here()
}

pub(crate) fn poll_device(state: &mut NetworkState) {
    poll_device_inner(state);
    // After the stack has taken frames. One SNTP step, then the line is armed.
    crate::ntp_client::drive(&mut state.stack, &mut state.device);
    crate::board::arm_nic();
}

fn poll_device_inner(state: &mut NetworkState) {
    crate::board::service_nic();
    refresh_gateway(&mut state.device);
    maybe_announce(&mut state.device);
    let timestamp = Instant::from_millis(crate::timer::millis() as i64);
    state.stack.poll(timestamp, &mut state.device);
    hold_for_link(state);

    let Some(dhcp) = state.dhcp.as_ref() else {
        refresh_banner();
        return;
    };
    if dhcp.finished {
        refresh_banner();
        return;
    }
    let handle = dhcp.handle;
    let started_ms = dhcp.started_ms;
    let timeout_ms = dhcp.timeout_ms;
    let fallback_ip = dhcp.fallback_ip;
    let fallback_netmask = dhcp.fallback_netmask;
    let fallback_gateway = dhcp.fallback_gateway;

    let action = {
        let socket = state
            .stack
            .sockets_mut()
            .get_mut::<dhcpv4::Socket>(handle);
        match socket.poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                let ip = config.address.address().0;
                let mask = prefix_to_mask(config.address.prefix_len());
                let mut dns = [[0u8; 4]; 3];
                let mut count = 0;
                for server in config.dns_servers.iter() {
                    if count >= dns.len() {
                        break;
                    }
                    dns[count] = server.0;
                    count += 1;
                }
                crate::config_manager::note_dhcp_dns_from_stack(&dns[..count]);
                let gateway = crate::dhcp_rx::lease_gateway(
                    ip,
                    mask,
                    config.router.map(|addr| addr.0),
                    config.server.address.0,
                    fallback_gateway,
                );
                Some(DhcpAction::Configured { ip, mask, gateway })
            }
            _ if crate::timer::millis().saturating_sub(started_ms) >= timeout_ms => {
                Some(DhcpAction::Timeout)
            }
            _ => None,
        }
    };

    let Some(action) = action else {
        refresh_banner();
        return;
    };
    match action {
        DhcpAction::Configured { ip, mask, gateway } => {
            state.stack.configure_static(ip, mask, gateway);
            crate::config_manager::note_lease(ip);
            DHCP_PHASE.store(2, Ordering::Relaxed);
            DHCP_IP.store(u32::from_be_bytes(ip), Ordering::Relaxed);
            let prefix: u32 = mask.iter().map(|byte| byte.count_ones()).sum();
            log::info!(
                "DHCP configured {}.{}.{}.{}/{} gateway {}.{}.{}.{}",
                ip[0],
                ip[1],
                ip[2],
                ip[3],
                prefix,
                gateway[0],
                gateway[1],
                gateway[2],
                gateway[3]
            );
            if let Some(mac) = crate::dhcp_rx::reply_mac() {
                if crate::dhcp_rx::stage_gateway_arp(
                    &state.device.mac_address(),
                    ip,
                    mask,
                    &mac,
                    gateway,
                ) {
                    log::info!(
                        "gateway neighbor {}.{}.{}.{} via {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        gateway[0],
                        gateway[1],
                        gateway[2],
                        gateway[3],
                        mac[0],
                        mac[1],
                        mac[2],
                        mac[3],
                        mac[4],
                        mac[5]
                    );
                    // The seeded reply fills our cache, so the stack never
                    // transmits a request. This gateway did not forward
                    // until it had answered a real ARP. Send that request
                    // before any DNS query, then let the seed in.
                    if request_gateway(&mut state.device, ip, gateway) {
                        log::info!(
                            "gateway arp request {}.{}.{}.{}",
                            gateway[0], gateway[1], gateway[2], gateway[3]
                        );
                    }
                    remember_gateway(ip, mask, gateway, mac);
                    let timestamp = Instant::from_millis(crate::timer::millis() as i64);
                    state.stack.poll(timestamp, &mut state.device);
                    if crate::dhcp_rx::arp_pending() {
                        log::info!("gateway arp not consumed");
                    }
                }
            }
            // The lease is applied above. The DHCP socket would keep
            // unicasting renewals at the server (often the gateway). A
            // renew whose timer is already due and whose emit fails stays
            // at PollAt in the past, which is a zero poll delay, and it
            // solicits the gateway again every silent interval. The kernel
            // never reads the socket again once `finished` is set.
            state.stack.sockets_mut().remove(handle);
            console_line_ip("[OK] DHCP leased", ip);
            console_line_ip("[OK] DHCP gateway", gateway);
        }
        DhcpAction::Timeout => {
            crate::ntp_client::clear_dhcp();
            crate::config_manager::note_dhcp_dns(&[]);
            state.stack.sockets_mut().remove(handle);
            state
                .stack
                .configure_static(fallback_ip, fallback_netmask, fallback_gateway);
            crate::config_manager::note_lease(fallback_ip);
            DHCP_PHASE.store(3, Ordering::Relaxed);
            DHCP_IP.store(u32::from_be_bytes(fallback_ip), Ordering::Relaxed);
            log::info!(
                "DHCP timeout, using static {}.{}.{}.{}",
                fallback_ip[0], fallback_ip[1], fallback_ip[2], fallback_ip[3]
            );
            console_line_ip("[WARN] DHCP timeout, static", fallback_ip);
        }
    }
    if let Some(dhcp) = state.dhcp.as_mut() {
        dhcp.finished = true;
    }
    maybe_announce(&mut state.device);
    refresh_banner();
}

/// requests, replies, hits, misses.
pub fn arp_counters() -> (u64, u64, u64, u64) {
    (
        ARP_REQUESTS_TX.load(Ordering::Relaxed),
        ARP_REPLIES_RX.load(Ordering::Relaxed),
        ARP_CACHE_HITS.load(Ordering::Relaxed),
        ARP_CACHE_MISSES.load(Ordering::Relaxed),
    )
}

/// Count one transmitted Ethernet frame. Gratuitous ARP (target equals
/// source) is the `network.arp_secs` announce and is not a cache miss.
pub fn note_tx_frame(frame: &[u8]) {
    match classify_frame(frame) {
        FrameKind::ArpRequest => {
            ARP_REQUESTS_TX.fetch_add(1, Ordering::Relaxed);
            ARP_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        }
        FrameKind::Ipv4Unicast => {
            ARP_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        }
        FrameKind::ArpReply | FrameKind::Other => {}
    }
}

/// Count one received Ethernet frame. The synthetic gateway reply is not
/// passed here; it did not arrive on the wire.
pub fn note_rx_frame(frame: &[u8]) {
    if classify_frame(frame) == FrameKind::ArpReply {
        ARP_REPLIES_RX.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    ArpRequest,
    ArpReply,
    Ipv4Unicast,
    Other,
}

fn classify_frame(frame: &[u8]) -> FrameKind {
    if frame.len() >= 42 && frame[12] == 0x08 && frame[13] == 0x06 {
        let spa = [frame[28], frame[29], frame[30], frame[31]];
        let tpa = [frame[38], frame[39], frame[40], frame[41]];
        if frame[20] == 0 && frame[21] == 1 && tpa != spa {
            return FrameKind::ArpRequest;
        }
        if frame[20] == 0 && frame[21] == 2 {
            return FrameKind::ArpReply;
        }
        return FrameKind::Other;
    }
    if frame.len() >= 14 && frame[12] == 0x08 && frame[13] == 0x00 && unicast_mac(&frame[..6]) {
        return FrameKind::Ipv4Unicast;
    }
    FrameKind::Other
}

fn unicast_mac(mac: &[u8]) -> bool {
    mac.len() == 6 && mac[0] & 1 == 0 && mac != [0xff; 6]
}

fn remember_gateway(our: [u8; 4], mask: [u8; 4], gw: [u8; 4], mac: [u8; 6]) {
    GW_OUR.store(u32::from_be_bytes(our), Ordering::Relaxed);
    GW_MASK.store(u32::from_be_bytes(mask), Ordering::Relaxed);
    GW_IP.store(u32::from_be_bytes(gw), Ordering::Relaxed);
    let mut packed = 0u64;
    for (shift, byte) in mac.iter().enumerate() {
        packed |= (*byte as u64) << (shift * 8);
    }
    GW_MAC.store(packed, Ordering::Relaxed);
    GW_REFRESH_MS.store(crate::timer::millis(), Ordering::Relaxed);
}

/// Re-seed the neighbor we already trust. smoltcp drops a reply whose
/// target is not our address (a gratuitous announcement from the router
/// is logged and then ignored), and it does not extend an entry just
/// because we used it. The logged `arp who-has` is that lookup missing,
/// not the 2 s gratuitous announce (`network.arp_secs`), which sets the
/// target to our own address and is not logged.
fn refresh_gateway(device: &mut EthernetDevice) {
    let gw = GW_IP.load(Ordering::Relaxed);
    if gw == 0 || crate::dhcp_rx::arp_pending() {
        return;
    }
    let now = crate::timer::millis();
    let prev = GW_REFRESH_MS.load(Ordering::Relaxed);
    if prev != 0 && now.saturating_sub(prev) < GATEWAY_HOLD_MS {
        return;
    }
    let mac_word = GW_MAC.load(Ordering::Relaxed);
    let mac = [
        mac_word as u8,
        (mac_word >> 8) as u8,
        (mac_word >> 16) as u8,
        (mac_word >> 24) as u8,
        (mac_word >> 32) as u8,
        (mac_word >> 40) as u8,
    ];
    if crate::dhcp_rx::stage_gateway_arp(
        &device.mac_address(),
        GW_OUR.load(Ordering::Relaxed).to_be_bytes(),
        GW_MASK.load(Ordering::Relaxed).to_be_bytes(),
        &mac,
        gw.to_be_bytes(),
    ) {
        GW_REFRESH_MS.store(now, Ordering::Relaxed);
    }
}

/// When the next gratuitous ARP is due. The owner sleeps on this so a
/// period does not wait for an unrelated IRQ.
pub(crate) fn next_announce_ms() -> Option<u64> {
    if ARP_PERIOD_MS.load(Ordering::Relaxed) == 0 || DHCP_IP.load(Ordering::Relaxed) == 0 {
        return None;
    }
    Some(ARP_NEXT_MS.load(Ordering::Relaxed))
}

/// One broadcast ARP for the address we are using, so a peer does not
/// have to have queried during the DHCP window. `network.arp_secs` repeats it.
fn maybe_announce(device: &mut EthernetDevice) {
    let ip = DHCP_IP.load(Ordering::Relaxed);
    if ip == 0 {
        return;
    }
    let now = crate::timer::millis();
    let period = ARP_PERIOD_MS.load(Ordering::Relaxed);
    if !crate::board::nic_link() {
        // Leave the next attempt in the future. A deadline of 0 would
        // wake the owner on every return while the cable is out.
        if period > 0 && now >= ARP_NEXT_MS.load(Ordering::Relaxed) {
            ARP_NEXT_MS.store(now.saturating_add(period), Ordering::Relaxed);
        }
        return;
    }
    if period == 0 {
        if ANNOUNCED.load(Ordering::Relaxed) {
            return;
        }
    } else if now < ARP_NEXT_MS.load(Ordering::Relaxed) {
        return;
    }
    if announce(device, ip.to_be_bytes()) {
        let first = !ANNOUNCED.swap(true, Ordering::Relaxed);
        if period == 0 {
            ARP_NEXT_MS.store(u64::MAX, Ordering::Relaxed);
        } else {
            ARP_NEXT_MS.store(now.saturating_add(period), Ordering::Relaxed);
        }
        if first {
            console_line("[OK] gratuitous ARP");
        }
    }
}

fn request_gateway(device: &mut EthernetDevice, our_ip: [u8; 4], gw_ip: [u8; 4]) -> bool {
    let mac = device.mac_address();
    let Some(token) = device.transmit(Instant::from_millis(crate::timer::millis() as i64)) else {
        return false;
    };
    token.consume(60, |buf| {
        crate::dhcp_rx::write_gateway_request(buf, &mac, our_ip, gw_ip);
    });
    true
}

fn announce(device: &mut EthernetDevice, ip: [u8; 4]) -> bool {
    let mac = device.mac_address();
    let Some(token) = device.transmit(Instant::from_millis(crate::timer::millis() as i64)) else {
        return false;
    };
    token.consume(60, |buf| {
        buf.fill(0);
        buf[..6].fill(0xff);
        buf[6..12].copy_from_slice(&mac);
        buf[12] = 0x08;
        buf[13] = 0x06;
        buf[14] = 0x00;
        buf[15] = 0x01;
        buf[16] = 0x08;
        buf[17] = 0x00;
        buf[18] = 6;
        buf[19] = 4;
        buf[20] = 0x00;
        buf[21] = 0x01;
        buf[22..28].copy_from_slice(&mac);
        buf[28..32].copy_from_slice(&ip);
        buf[32..38].copy_from_slice(&mac);
        buf[38..42].copy_from_slice(&ip);
    });
    true
}

fn hold_for_link(state: &mut NetworkState) {
    let Some(dhcp) = state.dhcp.as_mut() else {
        return;
    };
    if dhcp.finished {
        return;
    }
    let now = crate::timer::millis();
    if crate::board::nic_link() {
        if dhcp.link_wait_ms != 0 {
            dhcp.started_ms = now;
            dhcp.link_wait_ms = 0;
            console_line("[OK] link up, DHCP timer started");
        }
        return;
    }
    if dhcp.link_wait_ms == 0 {
        dhcp.link_wait_ms = now;
    }
    if now.saturating_sub(dhcp.link_wait_ms) < LINK_HOLD_MS {
        dhcp.started_ms = now;
        return;
    }
    if !dhcp.link_noted {
        dhcp.link_noted = true;
        console_line("[WARN] link still down, DHCP timer running");
    }
}

enum DhcpAction {
    Configured {
        ip: [u8; 4],
        mask: [u8; 4],
        gateway: [u8; 4],
    },
    Timeout,
}

fn prefix_to_mask(prefix: u8) -> [u8; 4] {
    let prefix = prefix.min(32);
    if prefix == 0 {
        return [0; 4];
    }
    let mask = u32::MAX << (32 - prefix);
    mask.to_be_bytes()
}

pub fn packet_counts() -> (u32, u32) {
    crate::board::packet_counts()
}

/// Pinned HDMI row on the Pi 4. Other boards build the same text and drop it.
pub fn refresh_banner() {
    let mut buf = [0u8; 640];
    let mut n = 0;
    n += crate::board::write_nic_status(&mut buf[n..]);
    push(&mut buf, &mut n, " dhcp:");
    match DHCP_PHASE.load(Ordering::Relaxed) {
        1 => push(&mut buf, &mut n, "wait"),
        2 => push(&mut buf, &mut n, "leased"),
        3 => push(&mut buf, &mut n, "timeout"),
        4 => push(&mut buf, &mut n, "static"),
        _ => push(&mut buf, &mut n, "off"),
    }
    push(&mut buf, &mut n, " ip:");
    let ip = DHCP_IP.load(Ordering::Relaxed);
    if ip == 0 {
        push(&mut buf, &mut n, "none");
    } else {
        push_ip(&mut buf, &mut n, ip.to_be_bytes());
    }
    #[cfg(feature = "board-pi4")]
    {
        push(&mut buf, &mut n, " led:");
        push(&mut buf, &mut n, crate::act::led_label());
        push(&mut buf, &mut n, "\n");
        n += crate::board::write_nic_diag(&mut buf[n..]);
        if n < buf.len() {
            buf[n] = b'\n';
            n += 1;
        }
        n += crate::disklog::write_summary(&mut buf[n..]);
        if let Ok(text) = core::str::from_utf8(&buf[..n]) {
            crate::fb_con::set_status(text);
        }
    }
    #[cfg(not(feature = "board-pi4"))]
    {
        push(&mut buf, &mut n, "\n");
        n += crate::board::write_nic_diag(&mut buf[n..]);
        let _ = n;
    }
}

fn console_line(text: &str) {
    crate::console::write_str(text);
    crate::console::write_str("\r\n");
}

fn console_line_ip(prefix: &str, ip: [u8; 4]) {
    let mut buf = [0u8; 64];
    let mut n = 0;
    push(&mut buf, &mut n, prefix);
    push(&mut buf, &mut n, " ");
    push_ip(&mut buf, &mut n, ip);
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        console_line(text);
    }
}

fn push(buf: &mut [u8], n: &mut usize, text: &str) {
    for byte in text.bytes() {
        if *n >= buf.len() {
            return;
        }
        buf[*n] = byte;
        *n += 1;
    }
}

fn push_ip(buf: &mut [u8], n: &mut usize, ip: [u8; 4]) {
    for (index, byte) in ip.iter().enumerate() {
        if index > 0 {
            push(buf, n, ".");
        }
        push_dec(buf, n, *byte as u32);
    }
}

fn push_dec(buf: &mut [u8], n: &mut usize, mut value: u32) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if value == 0 {
        push(buf, n, "0");
        return;
    }
    while value > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    if let Ok(text) = core::str::from_utf8(&tmp[i..]) {
        push(buf, n, text);
    }
}

