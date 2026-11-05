use crate::config_manager::DnsConfig;
use alloc::{vec, vec::Vec, string::ToString};
use smoltcp::socket::{udp, Socket};
use smoltcp::wire::IpEndpoint;
use dns_protocol::{DnsPacket, Question, ResourceRecord, QueryType, RecordType, RData, RecordClass};
use dns_cache::{DnsCache, CacheEntry};
use spin::Mutex;

type UdpSocket = smoltcp::socket::udp::Socket<'static>;
type UdpSocketBuffer = smoltcp::socket::udp::PacketBuffer<'static>;

static DNS_CACHE: Mutex<Option<DnsCache>> = Mutex::new(None);
static DNS_CONFIG: Mutex<Option<DnsConfig>> = Mutex::new(None);

pub fn start(config: &DnsConfig) {
    log::info!("DNS server initialized");
    log::info!("Local records: {}", config.records.len());
    log::info!("Upstream resolvers: {:?}", config.upstreams);
    
    // Initialize cache
    *DNS_CACHE.lock() = Some(DnsCache::new(config.cache_size));
    *DNS_CONFIG.lock() = Some(config.clone());
    
    // Socket will be created in init_sockets()
}

pub fn init_sockets(stack: &mut crate::net::NetworkStack) -> Result<(), &'static str> {
    // Create UDP socket for DNS (port 53)
    let rx_buffer = UdpSocketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
    let tx_buffer = UdpSocketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
    let socket = UdpSocket::new(rx_buffer, tx_buffer);
    
    let dns_handle = stack.sockets_mut().add(socket);
    
    // Bind to port 53
    let socket = stack.sockets_mut().get_mut::<UdpSocket>(dns_handle);
    socket.bind(53).map_err(|_| "Failed to bind DNS socket")?;
    
    log::info!("DNS socket bound to port 53");
    Ok(())
}

pub fn poll(stack: &mut crate::net::NetworkStack, _device: &mut crate::net::EthernetDevice) {
    // Process DNS requests
    let sockets = stack.sockets_mut();
    
    // Find the DNS socket (first UDP socket on port 53)
    for item in sockets.iter_mut() {
        let socket = item.1;
        if let Socket::Udp(udp_socket) = socket {
            // Check if this is the DNS socket
            if udp_socket.endpoint().port == 53 {
                process_dns_socket(udp_socket);
            }
        }
    }
}

fn process_dns_socket(socket: &mut UdpSocket) {
    // Receive DNS queries
    while socket.can_recv() {
        match socket.recv() {
            Ok((data, endpoint)) => {
                log::debug!("Received DNS query from {}", endpoint);
                
                // Parse DNS query
                match DnsPacket::parse(data) {
                    Ok(query) => {
                        // Process and send response
                        if let Some(response) = handle_dns_query(&query) {
                            if let Ok(response_bytes) = response.to_bytes() {
                                if socket.can_send() {
                                    match socket.send_slice(&response_bytes, endpoint) {
                                        Ok(()) => {
                                            log::debug!("Sent DNS response to {}", endpoint);
                                        }
                                        Err(e) => {
                                            log::warn!("Failed to send DNS response: {:?}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to parse DNS query: {:?}", e);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn handle_dns_query(query: &DnsPacket) -> Option<DnsPacket> {
    if query.questions.is_empty() {
        return None;
    }
    
    let question = &query.questions[0];
    log::info!("DNS query: {} {:?}", question.name, question.qtype);
    
    // Create response
    let mut response = DnsPacket::new(query.header.id);
    response.header.qr = true; // This is a response
    response.header.aa = false; // Not authoritative by default
    response.header.rd = query.header.rd; // Copy recursion desired
    response.header.ra = true; // Recursion available
    response.header.rcode = 0; // No error
    
    // Copy question
    response.questions.push(question.clone());
    
    // Try to answer from local records
    if let Some(answer) = lookup_local_record(&question.name, question.qtype) {
        response.answers.push(answer);
        response.header.aa = true; // Authoritative for local records
        return Some(response);
    }
    
    // Try cache
    let current_time = crate::timer::get_ticks();
    if let Some(cache) = DNS_CACHE.lock().as_ref() {
        if let Some(entry) = cache.get(&question.name, question.qtype as u16, current_time) {
            log::debug!("Cache hit for {}", question.name);
            
            // Parse cached data back into record
            if let Ok(cached_packet) = DnsPacket::parse(&entry.data) {
                if !cached_packet.answers.is_empty() {
                    response.answers.extend(cached_packet.answers.clone());
                    return Some(response);
                }
            }
        }
    }
    
    // Cache miss - would normally forward to upstream here
    // For now, return NXDOMAIN
    log::debug!("No local record or cache entry for {}", question.name);
    response.header.rcode = 3; // NXDOMAIN
    
    Some(response)
}

fn lookup_local_record(name: &str, qtype: QueryType) -> Option<ResourceRecord> {
    let config = DNS_CONFIG.lock();
    let config = config.as_ref()?;
    
    for record in &config.records {
        if record.name == name {
            // Check if record type matches query or if query is ANY
            let type_matches = match (&record.record_type, qtype) {
                (crate::config_manager::RecordType::A, QueryType::A) => true,
                (crate::config_manager::RecordType::AAAA, QueryType::AAAA) => true,
                (crate::config_manager::RecordType::CNAME, QueryType::CNAME) => true,
                (crate::config_manager::RecordType::PTR, QueryType::PTR) => true,
                (_, QueryType::ANY) => true,
                _ => false,
            };
            
            if type_matches {
                return Some(ResourceRecord {
                    name: record.name.clone(),
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
        }
    }
    
    None
}

fn parse_rdata(record_type: &crate::config_manager::RecordType, value: &str) -> RData {
    match record_type {
        crate::config_manager::RecordType::A => {
            // Parse IPv4 address
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
        crate::config_manager::RecordType::AAAA => {
            // Parse IPv6 address (simplified - just store zeros for now)
            RData::AAAA([0; 16])
        }
        crate::config_manager::RecordType::CNAME => RData::CNAME(value.to_string()),
        crate::config_manager::RecordType::PTR => RData::PTR(value.to_string()),
    }
}
