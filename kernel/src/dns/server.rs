use crate::config_manager::DnsConfig;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};
use dns_cache::{CacheEntry, DnsCache};
use dns_protocol::{DnsPacket, QueryType, RData, RecordClass, RecordType, ResourceRecord};
use smoltcp::socket::{udp, Socket};
use smoltcp::wire::{IpEndpoint, Ipv4Address};
use spin::Mutex;

type UdpSocket = smoltcp::socket::udp::Socket<'static>;
type UdpSocketBuffer = smoltcp::socket::udp::PacketBuffer<'static>;

const UPSTREAM_PORT: u16 = 53535;
const UPSTREAM_WAIT_MS: u64 = 2_000;
const MAX_PENDING: usize = 8;

static DNS_CACHE: Mutex<Option<DnsCache>> = Mutex::new(None);
static PENDING: Mutex<Vec<PendingQuery>> = Mutex::new(Vec::new());
static NEXT_UPSTREAM_ID: AtomicU16 = AtomicU16::new(1);

struct PendingQuery {
    client_id: u16,
    upstream_id: u16,
    client: IpEndpoint,
    name: String,
    qtype: u16,
    sent_ms: u64,
    upstream_idx: usize,
    original: Vec<u8>,
}

pub fn start(config: &DnsConfig) {
    log::info!("DNS server initialized");
    log::info!("Local records: {}", config.records.len());
    log::info!("Upstream resolvers: {:?}", config.upstreams);

    *DNS_CACHE.lock() = Some(DnsCache::new(config.cache_size));
    PENDING.lock().clear();
}

pub fn cache_entries() -> usize {
    DNS_CACHE.lock().as_ref().map(|cache| cache.len()).unwrap_or(0)
}

pub fn init_sockets(stack: &mut crate::net::NetworkStack) -> Result<(), &'static str> {
    bind_udp(stack, 53)?;
    bind_udp(stack, UPSTREAM_PORT)?;
    log::info!("DNS socket bound to port 53");
    log::info!("DNS upstream socket bound to port {}", UPSTREAM_PORT);
    Ok(())
}

fn bind_udp(stack: &mut crate::net::NetworkStack, port: u16) -> Result<(), &'static str> {
    let rx_buffer = UdpSocketBuffer::new(vec_meta(16), alloc::vec![0u8; 4096]);
    let tx_buffer = UdpSocketBuffer::new(vec_meta(16), alloc::vec![0u8; 4096]);
    let socket = UdpSocket::new(rx_buffer, tx_buffer);
    let handle = stack.sockets_mut().add(socket);
    let socket = stack.sockets_mut().get_mut::<UdpSocket>(handle);
    socket
        .bind(port)
        .map_err(|_| "Failed to bind DNS socket")?;
    Ok(())
}

fn vec_meta(n: usize) -> Vec<udp::PacketMetadata> {
    let mut meta = Vec::with_capacity(n);
    for _ in 0..n {
        meta.push(udp::PacketMetadata::EMPTY);
    }
    meta
}

pub fn poll(stack: &mut crate::net::NetworkStack, _device: &mut crate::net::EthernetDevice) {
    let now = crate::timer::millis();
    let seconds = now / 1000;

    let mut from_clients: Vec<(Vec<u8>, IpEndpoint)> = Vec::new();
    let mut from_upstream: Vec<Vec<u8>> = Vec::new();
    {
        let sockets = stack.sockets_mut();
        for (_, socket) in sockets.iter_mut() {
            if let Socket::Udp(udp_socket) = socket {
                let port = udp_socket.endpoint().port;
                if port != 53 && port != UPSTREAM_PORT {
                    continue;
                }
                while udp_socket.can_recv() {
                    match udp_socket.recv() {
                        Ok((data, meta)) => {
                            let packet = data.to_vec();
                            if port == 53 {
                                from_clients.push((packet, meta.endpoint));
                            } else {
                                from_upstream.push(packet);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    let mut replies: Vec<(Vec<u8>, IpEndpoint)> = Vec::new();
    let mut forwards: Vec<(Vec<u8>, [u8; 4])> = Vec::new();

    retry_expired(now, &mut replies, &mut forwards);
    take_upstream_replies(from_upstream, seconds, &mut replies);

    for (packet, endpoint) in from_clients {
        handle_client_packet(&packet, endpoint, now, seconds, &mut replies, &mut forwards);
    }

    send_all(stack, &replies, &forwards);
}

fn retry_expired(
    now: u64,
    replies: &mut Vec<(Vec<u8>, IpEndpoint)>,
    forwards: &mut Vec<(Vec<u8>, [u8; 4])>,
) {
    let mut pending = PENDING.lock();
    if pending.is_empty() {
        return;
    }
    let mut expired = false;
    for item in pending.iter() {
        if now.saturating_sub(item.sent_ms) >= UPSTREAM_WAIT_MS {
            expired = true;
            break;
        }
    }
    if !expired {
        return;
    }
    let upstreams = crate::config_manager::upstreams();
    let mut index = 0;
    while index < pending.len() {
        if now.saturating_sub(pending[index].sent_ms) < UPSTREAM_WAIT_MS {
            index += 1;
            continue;
        }
        let next = pending[index].upstream_idx + 1;
        if next < upstreams.len() {
            let id = next_upstream_id();
            let mut forwarded = pending[index].original.clone();
            set_id(&mut forwarded, id);
            let dest = upstreams[next];
            log::info!(
                "DNS forward retry {} via {}.{}.{}.{}",
                pending[index].name,
                dest[0],
                dest[1],
                dest[2],
                dest[3]
            );
            pending[index].upstream_id = id;
            pending[index].upstream_idx = next;
            pending[index].sent_ms = now;
            forwards.push((forwarded, dest));
            index += 1;
        } else {
            let item = pending.remove(index);
            log::info!("DNS upstream timeout for {}", item.name);
            if let Some(bytes) = servfail_from_query(&item.original) {
                replies.push((bytes, item.client));
            }
        }
    }
}

fn take_upstream_replies(
    packets: Vec<Vec<u8>>,
    seconds: u64,
    replies: &mut Vec<(Vec<u8>, IpEndpoint)>,
) {
    for mut packet in packets {
        if packet.len() < 12 {
            continue;
        }
        let id = packet_id(&packet);
        let mut pending = PENDING.lock();
        let Some(pos) = pending.iter().position(|item| item.upstream_id == id) else {
            continue;
        };
        let item = pending.remove(pos);
        drop(pending);
        set_id(&mut packet, item.client_id);
        remember(&item.name, item.qtype, &packet, seconds);
        log::info!("DNS upstream answer for {}", item.name);
        replies.push((packet, item.client));
    }
}

fn handle_client_packet(
    packet: &[u8],
    endpoint: IpEndpoint,
    now: u64,
    seconds: u64,
    replies: &mut Vec<(Vec<u8>, IpEndpoint)>,
    forwards: &mut Vec<(Vec<u8>, [u8; 4])>,
) {
    let query = match DnsPacket::parse(packet) {
        Ok(query) => query,
        Err(err) => {
            log::warn!("Failed to parse DNS query: {:?}", err);
            return;
        }
    };
    if query.header.qr || query.questions.is_empty() {
        return;
    }

    let question = &query.questions[0];
    let name = normalize_name(&question.name);
    let qtype = question.qtype as u16;
    log::info!("DNS query: {} {:?}", question.name, question.qtype);

    if let Some(answer) = lookup_local_record(&question.name, question.qtype) {
        let mut response = DnsPacket::new(query.header.id);
        response.header.qr = true;
        response.header.aa = true;
        response.header.rd = query.header.rd;
        response.header.ra = true;
        response.questions.push(question.clone());
        response.answers.push(answer);
        if let Ok(bytes) = response.to_bytes() {
            replies.push((bytes, endpoint));
        }
        return;
    }

    if let Some(mut cached) = cached_response(&name, qtype, seconds) {
        set_id(&mut cached, query.header.id);
        log::info!("DNS cache hit {}", name);
        replies.push((cached, endpoint));
        return;
    }

    let upstreams = crate::config_manager::upstreams();
    if upstreams.is_empty() || PENDING.lock().len() >= MAX_PENDING {
        if let Some(bytes) = error_response(&query, 2) {
            replies.push((bytes, endpoint));
        }
        return;
    }

    let upstream_id = next_upstream_id();
    let mut forwarded = packet.to_vec();
    set_id(&mut forwarded, upstream_id);
    let dest = upstreams[0];
    log::info!(
        "DNS forward {} via {}.{}.{}.{}",
        name,
        dest[0],
        dest[1],
        dest[2],
        dest[3]
    );
    PENDING.lock().push(PendingQuery {
        client_id: query.header.id,
        upstream_id,
        client: endpoint,
        name,
        qtype,
        sent_ms: now,
        upstream_idx: 0,
        original: packet.to_vec(),
    });
    forwards.push((forwarded, dest));
}

fn send_all(
    stack: &mut crate::net::NetworkStack,
    replies: &[(Vec<u8>, IpEndpoint)],
    forwards: &[(Vec<u8>, [u8; 4])],
) {
    let sockets = stack.sockets_mut();
    for (_, socket) in sockets.iter_mut() {
        if let Socket::Udp(udp_socket) = socket {
            match udp_socket.endpoint().port {
                53 => {
                    for (bytes, endpoint) in replies {
                        if udp_socket.send_slice(bytes, *endpoint).is_err() {
                            log::warn!("Failed to send DNS response");
                        }
                    }
                }
                UPSTREAM_PORT => {
                    for (bytes, ip) in forwards {
                        let endpoint = IpEndpoint::new(
                            Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]).into(),
                            53,
                        );
                        if udp_socket.send_slice(bytes, endpoint).is_err() {
                            log::warn!("Failed to send DNS query upstream");
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn lookup_local_record(name: &str, qtype: QueryType) -> Option<ResourceRecord> {
    let wanted = normalize_name(name);
    let records = crate::config_manager::records();
    for record in &records {
        if normalize_name(&record.name) != wanted {
            continue;
        }
        let type_matches = match (&record.record_type, qtype) {
            (crate::config_manager::RecordType::A, QueryType::A) => true,
            (crate::config_manager::RecordType::AAAA, QueryType::AAAA) => true,
            (crate::config_manager::RecordType::CNAME, QueryType::CNAME) => true,
            (crate::config_manager::RecordType::PTR, QueryType::PTR) => true,
            (_, QueryType::ANY) => true,
            _ => false,
        };
        if !type_matches {
            continue;
        }
        return Some(ResourceRecord {
            name: name.to_string(),
            rtype: match record.record_type {
                crate::config_manager::RecordType::A => RecordType::A,
                crate::config_manager::RecordType::AAAA => RecordType::AAAA,
                crate::config_manager::RecordType::CNAME => RecordType::CNAME,
                crate::config_manager::RecordType::PTR => RecordType::PTR,
            },
            rclass: RecordClass::IN,
            ttl: record.ttl,
            rdata: parse_rdata(&record.record_type, &record.value),
        });
    }
    None
}

fn parse_rdata(record_type: &crate::config_manager::RecordType, value: &str) -> RData {
    match record_type {
        crate::config_manager::RecordType::A => {
            let parts: Vec<&str> = value.split('.').collect();
            if parts.len() == 4 {
                if let (Ok(a), Ok(b), Ok(c), Ok(d)) = (
                    parts[0].parse::<u8>(),
                    parts[1].parse::<u8>(),
                    parts[2].parse::<u8>(),
                    parts[3].parse::<u8>(),
                ) {
                    return RData::A([a, b, c, d]);
                }
            }
            RData::A([0, 0, 0, 0])
        }
        crate::config_manager::RecordType::AAAA => RData::AAAA([0; 16]),
        crate::config_manager::RecordType::CNAME => RData::CNAME(value.to_string()),
        crate::config_manager::RecordType::PTR => RData::PTR(value.to_string()),
    }
}

fn cached_response(name: &str, qtype: u16, seconds: u64) -> Option<Vec<u8>> {
    let cache = DNS_CACHE.lock();
    let cache = cache.as_ref()?;
    cache.get(name, qtype, seconds).map(|entry| entry.data.clone())
}

fn remember(name: &str, qtype: u16, packet: &[u8], seconds: u64) {
    if packet.len() < 12 || packet[2] & 0x02 != 0 {
        return;
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 && rcode != 3 {
        return;
    }
    let ttl = match DnsPacket::parse(packet) {
        Ok(parsed) if !parsed.answers.is_empty() => parsed
            .answers
            .iter()
            .map(|answer| answer.ttl)
            .min()
            .unwrap_or(30)
            .max(1),
        Ok(_) => 30,
        Err(_) => 60,
    };
    if let Some(cache) = DNS_CACHE.lock().as_mut() {
        cache.insert(CacheEntry {
            name: name.to_string(),
            record_type: qtype,
            data: packet.to_vec(),
            ttl,
            timestamp: seconds,
        });
    }
}

fn servfail_from_query(original: &[u8]) -> Option<Vec<u8>> {
    let query = DnsPacket::parse(original).ok()?;
    error_response(&query, 2)
}

fn error_response(query: &DnsPacket, rcode: u8) -> Option<Vec<u8>> {
    let mut response = DnsPacket::new(query.header.id);
    response.header.qr = true;
    response.header.rd = query.header.rd;
    response.header.ra = true;
    response.header.rcode = rcode;
    if let Some(question) = query.questions.first() {
        response.questions.push(question.clone());
    }
    response.to_bytes().ok()
}

fn next_upstream_id() -> u16 {
    let id = NEXT_UPSTREAM_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        1
    } else {
        id
    }
}

fn packet_id(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[0], packet[1]])
}

fn set_id(packet: &mut [u8], id: u16) {
    if packet.len() >= 2 {
        let bytes = id.to_be_bytes();
        packet[0] = bytes[0];
        packet[1] = bytes[1];
    }
}

fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        let lower = if byte.is_ascii_uppercase() {
            byte + 32
        } else {
            byte
        };
        out.push(lower as char);
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}
