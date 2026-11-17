pub mod service;
mod stack;

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;

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

/// 0 absent, 1 waiting, 2 leased, 3 fallback, 4 static.
static DHCP_PHASE: AtomicU8 = AtomicU8::new(0);
static DHCP_IP: AtomicU32 = AtomicU32::new(0);

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
            let handle = stack.sockets_mut().add(dhcpv4::Socket::new());
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
    crate::board::service_nic();
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
                let gateway = config.router.map(|addr| addr.0).unwrap_or(fallback_gateway);
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
            DHCP_PHASE.store(2, Ordering::Relaxed);
            DHCP_IP.store(u32::from_be_bytes(ip), Ordering::Relaxed);
            log::info!(
                "DHCP configured {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3]
            );
            console_line_ip("[OK] DHCP leased", ip);
        }
        DhcpAction::Timeout => {
            state.stack.sockets_mut().remove(handle);
            state
                .stack
                .configure_static(fallback_ip, fallback_netmask, fallback_gateway);
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
    refresh_banner();
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
    let mut buf = [0u8; 96];
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
        if let Ok(text) = core::str::from_utf8(&buf[..n]) {
            crate::fb_con::set_status(text);
        }
    }
    #[cfg(not(feature = "board-pi4"))]
    {
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

