//! SNTP client and a small LAN server.
//!
//! One step runs on the network owner after the stack has polled. It sends
//! or receives a single datagram and returns. DNS keeps its own sockets.
//! A hostname is one A query to the configured resolvers, the same list
//! the DNS server forwards to, on this socket rather than through that
//! task. A failed round logs once and retries later. DHCP option 42, when
//! a lease offered it, is tried before the configured names.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp::{self, PacketMetadata};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use crate::net::NetworkStack;

const CLIENT_PORT: u16 = 12300;
const SERVER_PORT: u16 = 123;
const STEP_MS: u64 = 2_000;
const RETRY_MS: u64 = 30_000;
const SERVERS: usize = 8;

static FLUSH: AtomicBool = AtomicBool::new(false);
static DHCP_N: AtomicU8 = AtomicU8::new(0);
static DHCP_IP: [AtomicU32; 4] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

#[derive(Clone, Copy)]
struct Target {
    host: [u8; 64],
    host_len: u8,
    port: u16,
    ip: [u8; 4],
    have_ip: bool,
}

impl Target {
    const fn empty() -> Self {
        Self {
            host: [0; 64],
            host_len: 0,
            port: 123,
            ip: [0; 4],
            have_ip: false,
        }
    }
}

enum Step {
    Idle,
    Dns {
        index: u8,
        resolver: u8,
        id: u16,
        sent: u64,
    },
    Query {
        index: u8,
        sent: u64,
        t1: i64,
        stamp: [u8; 8],
    },
}

struct Client {
    step: Step,
    targets: [Target; SERVERS],
    count: u8,
    index: u8,
    token: u64,
    next_ms: u64,
    interval_ms: u64,
    client: Option<SocketHandle>,
    server: Option<SocketHandle>,
    /// A failed bind is not retried. Each try would leak another buffer.
    client_stop: bool,
    server_stop: bool,
    failed: bool,
    resolvers: [[u8; 4]; 4],
    resolver_n: u8,
    packet: [u8; 512],
    rates: [ntp::RateSlot; ntp::RATE_SLOTS],
}

static CLIENT: spin::Mutex<Client> = spin::Mutex::new(Client {
    step: Step::Idle,
    targets: [Target::empty(); SERVERS],
    count: 0,
    index: 0,
    token: 0,
    next_ms: 0,
    interval_ms: 3_600_000,
    client: None,
    server: None,
    client_stop: false,
    server_stop: false,
    failed: false,
    resolvers: [[0; 4]; 4],
    resolver_n: 0,
    packet: [0; 512],
    rates: [ntp::RateSlot::empty(); ntp::RATE_SLOTS],
});

pub fn note_bootreply(frame: &[u8]) {
    let mut found = [[0u8; 4]; 4];
    let n = ntp::ntp_from_bootreply(frame, &mut found);
    let mut i = 0;
    while i < found.len() {
        let ip = if i < n { found[i] } else { [0, 0, 0, 0] };
        DHCP_IP[i].store(u32::from_be_bytes(ip), Ordering::Relaxed);
        i += 1;
    }
    DHCP_N.store(n as u8, Ordering::Release);
}

pub fn clear_dhcp() {
    DHCP_N.store(0, Ordering::Release);
}

/// When the owner should wake to send the next query or give up on one.
pub fn next_wake_ms() -> Option<u64> {
    let client = CLIENT.lock();
    match client.step {
        Step::Dns { sent, .. } | Step::Query { sent, .. } => Some(sent.saturating_add(STEP_MS)),
        Step::Idle if client.next_ms != 0 && client.count > 0 => Some(client.next_ms),
        Step::Idle => None,
    }
}

pub fn drive(stack: &mut crate::net::NetworkStack, device: &mut crate::net::EthernetDevice) {
    {
        let mut client = CLIENT.lock();
        ensure_sockets(&mut client, stack);
        serve(&mut client, stack);
        if crate::net::has_address() {
            drop(client);
            let view = crate::config_manager::ntp_view();
            let mut client = CLIENT.lock();
            if view.token != client.token {
                load_targets(&mut client, &view);
                client.token = view.token;
                client.interval_ms = view.interval_secs.saturating_mul(1000);
                client.step = Step::Idle;
                client.index = 0;
                client.next_ms = crate::timer::millis();
                client.failed = false;
            }
            step(&mut client, stack);
        }
    }
    if FLUSH.swap(false, Ordering::Relaxed) {
        let timestamp = smoltcp::time::Instant::from_millis(crate::timer::millis() as i64);
        stack.poll(timestamp, device);
    }
}

fn ensure_sockets(client: &mut Client, stack: &mut NetworkStack) {
    if client.client.is_none() && !client.client_stop {
        client.client = bind(stack, CLIENT_PORT);
        if client.client.is_some() {
            log::info!("ntp client socket {CLIENT_PORT}");
        } else {
            client.client_stop = true;
        }
    }
    if client.server.is_none() && !client.server_stop {
        client.server = bind(stack, SERVER_PORT);
        if client.server.is_some() {
            log::info!("ntp server socket {SERVER_PORT}");
        } else {
            client.server_stop = true;
        }
    }
}

fn bind(stack: &mut NetworkStack, port: u16) -> Option<SocketHandle> {
    let rx = udp::PacketBuffer::new(meta(4), alloc::vec![0u8; 2048]);
    let tx = udp::PacketBuffer::new(meta(4), alloc::vec![0u8; 2048]);
    let handle = stack.sockets_mut().add(udp::Socket::new(rx, tx));
    let ok = stack
        .sockets_mut()
        .get_mut::<udp::Socket>(handle)
        .bind(port)
        .is_ok();
    if ok {
        Some(handle)
    } else {
        stack.sockets_mut().remove(handle);
        log::info!("ntp bind {port} failed");
        None
    }
}

fn meta(n: usize) -> alloc::vec::Vec<PacketMetadata> {
    let mut out = alloc::vec::Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        out.push(PacketMetadata::EMPTY);
        i += 1;
    }
    out
}

fn load_targets(client: &mut Client, view: &crate::config_manager::NtpView) {
    client.count = 0;
    client.targets = [Target::empty(); SERVERS];
    let dhcp_n = DHCP_N.load(Ordering::Acquire) as usize;
    let mut i = 0;
    while i < dhcp_n && i < 4 && (client.count as usize) < SERVERS {
        let ip = DHCP_IP[i].load(Ordering::Relaxed).to_be_bytes();
        if ip != [0, 0, 0, 0] {
            push_ip(client, ip, 123);
        }
        i += 1;
    }
    let mut n = 0;
    while n < view.count as usize && (client.count as usize) < SERVERS {
        let len = view.lens[n] as usize;
        let mut target = Target::empty();
        target.host[..len].copy_from_slice(&view.servers[n][..len]);
        target.host_len = len as u8;
        target.port = view.ports[n];
        target.ip = view.ips[n];
        target.have_ip = view.have_ip[n];
        client.targets[client.count as usize] = target;
        client.count += 1;
        n += 1;
    }
}

fn push_ip(client: &mut Client, ip: [u8; 4], port: u16) {
    let mut target = Target::empty();
    target.host_len = write_ip(&mut target.host, ip) as u8;
    target.port = port;
    target.ip = ip;
    target.have_ip = true;
    client.targets[client.count as usize] = target;
    client.count += 1;
}

fn step(client: &mut Client, stack: &mut NetworkStack) {
    let now = crate::timer::millis();
    match client.step {
        Step::Idle => {
            if client.count == 0 || now < client.next_ms {
                return;
            }
            client.index = 0;
            begin(client, stack, now);
        }
        Step::Dns {
            index,
            resolver,
            id,
            sent,
        } => {
            if take_dns(client, stack, index, id) {
                let ip = client.targets[index as usize].ip;
                client.targets[index as usize].have_ip = true;
                send_query(client, stack, index, ip, now);
                return;
            }
            if now.saturating_sub(sent) >= STEP_MS {
                let next = resolver.saturating_add(1);
                if next < client.resolver_n {
                    send_dns(client, stack, index, next, now);
                } else {
                    advance(client, stack, now);
                }
            }
        }
        Step::Query {
            index,
            sent,
            t1,
            stamp,
        } => {
            if take_ntp(client, stack, index, t1, stamp, now) {
                return;
            }
            if now.saturating_sub(sent) >= STEP_MS {
                advance(client, stack, now);
            }
        }
    }
}

fn begin(client: &mut Client, stack: &mut NetworkStack, now: u64) {
    if client.index >= client.count {
        finish_fail(client, now);
        return;
    }
    let index = client.index;
    if client.targets[index as usize].have_ip {
        let ip = client.targets[index as usize].ip;
        send_query(client, stack, index, ip, now);
        return;
    }
    client.resolver_n = crate::config_manager::copy_resolvers(&mut client.resolvers) as u8;
    if client.resolver_n == 0 {
        advance(client, stack, now);
        return;
    }
    send_dns(client, stack, index, 0, now);
}

fn write_ip(out: &mut [u8], ip: [u8; 4]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < 4 {
        if i > 0 && n < out.len() {
            out[n] = b'.';
            n += 1;
        }
        n = write_dec(out, n, ip[i]);
        i += 1;
    }
    n
}

fn write_dec(out: &mut [u8], mut n: usize, mut value: u8) -> usize {
    let mut tmp = [0u8; 3];
    let mut i = 3;
    if value == 0 {
        i -= 1;
        tmp[i] = b'0';
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10);
        value /= 10;
    }
    while i < 3 && n < out.len() {
        out[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

fn send_dns(client: &mut Client, stack: &mut NetworkStack, index: u8, resolver: u8, now: u64) {
    let Some(handle) = client.client else {
        advance(client, stack, now);
        return;
    };
    let mut host = [0u8; 64];
    let host_n = {
        let target = &client.targets[index as usize];
        let n = (target.host_len as usize).min(host.len());
        host[..n].copy_from_slice(&target.host[..n]);
        n
    };
    let id = (now as u16) ^ 0x51a7 ^ u16::from(index);
    let Some(len) = ntp::write_a_query(&mut client.packet, id, &host[..host_n]) else {
        advance(client, stack, now);
        return;
    };
    let ip = client.resolvers[resolver as usize];
    if !send_to(stack, handle, &client.packet[..len], ip, 53) {
        client.step = Step::Dns {
            index,
            resolver,
            id,
            sent: now,
        };
        return;
    }
    client.step = Step::Dns {
        index,
        resolver,
        id,
        sent: now,
    };
}

fn send_query(client: &mut Client, stack: &mut NetworkStack, index: u8, ip: [u8; 4], now: u64) {
    let Some(handle) = client.client else {
        advance(client, stack, now);
        return;
    };
    let clock = client_clock_ms(now);
    let mut packet = [0u8; ntp::PACKET_LEN];
    let stamp = ntp::write_client(&mut packet, clock);
    let port = client.targets[index as usize].port;
    if !send_to(stack, handle, &packet, ip, port) {
        client.step = Step::Query {
            index,
            sent: now,
            t1: clock,
            stamp,
        };
        return;
    }
    client.step = Step::Query {
        index,
        sent: now,
        t1: clock,
        stamp,
    };
}

fn client_clock_ms(now: u64) -> i64 {
    if let Some(unix_ms) = crate::clock::unix_ms() {
        unix_ms.saturating_add(ntp::NTP_UNIX_DELTA.saturating_mul(1000))
    } else {
        now as i64
    }
}

fn take_dns(client: &mut Client, stack: &mut NetworkStack, index: u8, id: u16) -> bool {
    let Some(handle) = client.client else {
        return false;
    };
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    if !socket.can_recv() {
        return false;
    }
    let mut buf = [0u8; 512];
    let n = match socket.recv_slice(&mut buf) {
        Ok((n, _)) => n,
        Err(_) => return false,
    };
    ntp::read_a(&buf[..n], id).is_some_and(|ip| {
        if (index as usize) < client.count as usize {
            client.targets[index as usize].ip = ip;
            true
        } else {
            false
        }
    })
}

fn take_ntp(
    client: &mut Client,
    stack: &mut NetworkStack,
    index: u8,
    t1: i64,
    stamp: [u8; 8],
    now: u64,
) -> bool {
    let Some(handle) = client.client else {
        return false;
    };
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    if !socket.can_recv() {
        return false;
    }
    let mut buf = [0u8; 512];
    let n = match socket.recv_slice(&mut buf) {
        Ok((n, _)) => n,
        Err(_) => return false,
    };
    let synced = crate::clock::synced();
    let current = crate::clock::unix_ms().unwrap_or(0);
    let t4 = client_clock_ms(now);
    match ntp::evaluate(&buf[..n], &stamp, t1, t4, synced, current) {
        Ok(fix) => {
            let target = &client.targets[index as usize];
            let name = &target.host[..target.host_len as usize];
            // The first sample adopts the server clock. There was no wall
            // time to correct, so the reported offset is zero. Later samples
            // are the SNTP offset against that running clock.
            let reported = if synced { fix.offset_ms } else { 0 };
            crate::clock::sync(fix.unix_ms, reported, fix.stratum, name, now);
            client.next_ms = now.saturating_add(client.interval_ms);
            client.step = Step::Idle;
            client.failed = false;
            client.index = index;
            log::info!(
                "ntp synced {} offset_ms {} delay_ms {} stratum {}",
                core::str::from_utf8(name).unwrap_or("?"),
                reported,
                fix.delay_ms,
                fix.stratum
            );
            true
        }
        Err(reason) => {
            log::info!("ntp reject {reason}");
            false
        }
    }
}

fn advance(client: &mut Client, stack: &mut NetworkStack, now: u64) {
    client.index = client.index.saturating_add(1);
    begin(client, stack, now);
}

fn finish_fail(client: &mut Client, now: u64) {
    client.step = Step::Idle;
    client.next_ms = now.saturating_add(RETRY_MS);
    if !client.failed {
        client.failed = true;
        log::info!("ntp no server");
    }
}

fn serve(client: &mut Client, stack: &mut NetworkStack) {
    let Some(handle) = client.server else {
        return;
    };
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    if !socket.can_recv() {
        return;
    }
    let mut buf = [0u8; 512];
    let (n, meta) = match socket.recv_slice(&mut buf) {
        Ok(got) => got,
        Err(_) => return,
    };
    if n < ntp::PACKET_LEN || !crate::clock::synced() || !crate::config_manager::ntp_serve() {
        return;
    }
    if buf[0] & 0x7 != ntp::MODE_CLIENT {
        return;
    }
    let endpoint = meta.endpoint;
    let IpAddress::Ipv4(peer) = endpoint.addr else {
        return;
    };
    let ip = u32::from_be_bytes(peer.0);
    let now = crate::timer::millis();
    if !ntp::rate_allow(&mut client.rates, ip, now) {
        return;
    }
    let mut client_tx = [0u8; 8];
    client_tx.copy_from_slice(&buf[40..48]);
    let stratum = crate::clock::stratum().saturating_add(1).min(15);
    let now_ntp = client_clock_ms(now);
    let mut reply = [0u8; ntp::PACKET_LEN];
    let ref_ip = client
        .targets
        .get(client.index as usize)
        .map(|target| target.ip)
        .unwrap_or([0, 0, 0, 0]);
    ntp::write_server(&mut reply, &client_tx, stratum, ref_ip, now_ntp, now_ntp);
    if socket.send_slice(&reply, endpoint).is_ok() {
        FLUSH.store(true, Ordering::Relaxed);
    }
}

fn send_to(
    stack: &mut NetworkStack,
    handle: SocketHandle,
    data: &[u8],
    ip: [u8; 4],
    port: u16,
) -> bool {
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    let endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(ip)), port);
    let ok = socket.send_slice(data, endpoint).is_ok();
    if ok {
        FLUSH.store(true, Ordering::Relaxed);
    }
    ok
}
