use crate::net::NetworkMode;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

#[derive(Debug, Clone)]
pub struct Config {
    pub network: NetworkConfig,
    pub dns: DnsConfig,
    pub http: HttpConfig,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub mode: NetworkMode,
}

#[derive(Debug, Clone)]
pub struct DnsConfig {
    pub records: Vec<DnsRecord>,
    pub upstreams: Vec<[u8; 4]>,
    pub blocklist: Vec<String>,
    pub cache_size: usize,
}

#[derive(Debug, Clone)]
pub struct DnsRecord {
    pub name: String,
    pub record_type: RecordType,
    pub value: String,
    pub ttl: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordType {
    A,
    AAAA,
    CNAME,
    PTR,
}

impl RecordType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordType::A => "A",
            RecordType::AAAA => "AAAA",
            RecordType::CNAME => "CNAME",
            RecordType::PTR => "PTR",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "A" | "a" => Some(RecordType::A),
            "AAAA" | "aaaa" => Some(RecordType::AAAA),
            "CNAME" | "cname" => Some(RecordType::CNAME),
            "PTR" | "ptr" => Some(RecordType::PTR),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub port: u16,
    pub username: String,
    pub password: String,
}

static CURRENT: Mutex<Option<Config>> = Mutex::new(None);

pub fn load_config() -> Config {
    let config = match crate::fs::read_config() {
        Ok(data) => match parse_config(&data) {
            Some(config) => {
                log::info!("Loaded config from filesystem");
                config
            }
            None => {
                log::info!("Config file unreadable, using defaults");
                default_config()
            }
        },
        Err(_) => {
            log::info!("Using default config");
            default_config()
        }
    };
    *CURRENT.lock() = Some(config.clone());
    config
}

pub fn records() -> Vec<DnsRecord> {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.records.clone())
        .unwrap_or_default()
}

pub fn upstreams() -> Vec<[u8; 4]> {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.upstreams.clone())
        .unwrap_or_default()
}

pub fn blocklist() -> Vec<String> {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.blocklist.clone())
        .unwrap_or_default()
}

pub fn cache_size() -> usize {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.cache_size)
        .unwrap_or(1000)
}

/// Write `name TYPE value ttl` lines. `None` if `out` is too small.
/// These writers do not allocate: the kernel heap never frees, and the DNS
/// program asks for this text on every query.
pub fn write_records(out: &mut [u8]) -> Option<usize> {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return Some(0);
    };
    let mut used = 0;
    for record in &config.dns.records {
        used = push_slice(out, used, record.name.as_bytes())?;
        used = push_slice(out, used, b" ")?;
        used = push_slice(out, used, record.record_type.as_str().as_bytes())?;
        used = push_slice(out, used, b" ")?;
        used = push_slice(out, used, record.value.as_bytes())?;
        used = push_slice(out, used, b" ")?;
        used = push_u32(out, used, record.ttl)?;
        used = push_slice(out, used, b"\n")?;
    }
    Some(used)
}

pub fn write_upstreams(out: &mut [u8]) -> Option<usize> {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return Some(0);
    };
    let mut used = 0;
    for ip in &config.dns.upstreams {
        used = append_ip(out, used, *ip)?;
        used = push_slice(out, used, b"\n")?;
    }
    Some(used)
}

pub fn write_blocklist(out: &mut [u8]) -> Option<usize> {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return Some(0);
    };
    let mut used = 0;
    for name in &config.dns.blocklist {
        used = push_slice(out, used, name.as_bytes())?;
        used = push_slice(out, used, b"\n")?;
    }
    Some(used)
}

fn push_slice(out: &mut [u8], used: usize, bytes: &[u8]) -> Option<usize> {
    let end = used.checked_add(bytes.len())?;
    if end > out.len() {
        return None;
    }
    out[used..end].copy_from_slice(bytes);
    Some(end)
}

fn push_u32(out: &mut [u8], mut used: usize, mut value: u32) -> Option<usize> {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if value == 0 {
        return push_slice(out, used, b"0");
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    used = push_slice(out, used, &buf[i..])?;
    Some(used)
}

fn append_ip(out: &mut [u8], mut used: usize, ip: [u8; 4]) -> Option<usize> {
    for (index, byte) in ip.iter().enumerate() {
        if index > 0 {
            used = push_slice(out, used, b".")?;
        }
        used = push_u32(out, used, *byte as u32)?;
    }
    Some(used)
}

pub fn add_record(name: &str, record_type: &str, value: &str, ttl: u32) -> Result<(), String> {
    let name = name.trim();
    let value = value.trim();
    if !valid_label(name) || !valid_label(value) {
        return Err(String::from("invalid record"));
    }
    let record_type = RecordType::parse(record_type).ok_or_else(|| String::from("invalid type"))?;
        let ttl = ttl.clamp(1, 86_400);
    {
        let mut config = CURRENT.lock();
        let config = config.as_mut().ok_or_else(|| String::from("no config"))?;
        config.dns.records.retain(|record| !names_eq(&record.name, name));
        config.dns.records.push(DnsRecord {
            name: name.to_string(),
            record_type,
            value: value.to_string(),
            ttl,
        });
    }
    save_current()?;
    crate::fs::append_log(&alloc::format!("record added {name}"));
    Ok(())
}

pub fn remove_record(name: &str) -> Result<bool, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(String::from("invalid record"));
    }
    let removed = {
        let mut config = CURRENT.lock();
        let config = config.as_mut().ok_or_else(|| String::from("no config"))?;
        let before = config.dns.records.len();
        config
            .dns
            .records
            .retain(|record| !names_eq(&record.name, name));
        before != config.dns.records.len()
    };
    if removed {
        save_current()?;
        crate::fs::append_log(&alloc::format!("record removed {name}"));
    }
    Ok(removed)
}

fn save_current() -> Result<(), String> {
    let config = CURRENT.lock().clone().ok_or_else(|| String::from("no config"))?;
    let text = serialize_config(&config);
    crate::fs::write_config(&text)
}

fn default_config() -> Config {
    let mut records = Vec::new();
    records.push(DnsRecord {
        name: String::from("test.local"),
        record_type: RecordType::A,
        value: String::from("192.168.1.100"),
        ttl: 300,
    });
    records.push(DnsRecord {
        name: String::from("pi4.local"),
        record_type: RecordType::A,
        value: String::from("10.0.2.15"),
        ttl: 300,
    });
    records.push(DnsRecord {
        name: String::from("www.local"),
        record_type: RecordType::CNAME,
        value: String::from("pi4.local"),
        ttl: 300,
    });

    Config {
        network: NetworkConfig {
            mode: default_network_mode(),
        },
        dns: DnsConfig {
            records,
            upstreams: alloc::vec![[8, 8, 8, 8], [1, 1, 1, 1], [10, 0, 2, 3]],
            blocklist: Vec::new(),
            cache_size: 1000,
        },
        http: HttpConfig {
            port: 80,
            username: String::from("admin"),
            password: String::from("admin"),
        },
    }
}

fn default_network_mode() -> NetworkMode {
    #[cfg(feature = "dhcp-default")]
    {
        NetworkMode::Dhcp {
            fallback_ip: [10, 0, 2, 15],
            fallback_netmask: [255, 255, 255, 0],
            fallback_gateway: [10, 0, 2, 2],
            timeout_ms: 3_000,
        }
    }
    #[cfg(not(feature = "dhcp-default"))]
    {
        NetworkMode::Static {
            ip: [10, 0, 2, 15],
            netmask: [255, 255, 255, 0],
            gateway: [10, 0, 2, 2],
        }
    }
}

fn parse_config(text: &str) -> Option<Config> {
    let mut config = default_config();
    config.dns.records.clear();
    config.dns.upstreams.clear();
    config.dns.blocklist.clear();
    let mut saw_record = false;
    let mut saw_upstream = false;
    let mut mode_dhcp = false;
    let mut ip = [10, 0, 2, 15];
    let mut netmask = [255, 255, 255, 0];
    let mut gateway = [10, 0, 2, 2];
    let mut timeout_ms = 3_000u64;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "mode" => mode_dhcp = value.eq_ignore_ascii_case("dhcp"),
            "ip" => ip = parse_ipv4(value)?,
            "netmask" => netmask = parse_ipv4(value)?,
            "gateway" => gateway = parse_ipv4(value)?,
            "timeout_ms" => timeout_ms = value.parse().unwrap_or(3_000),
            "upstream" => {
                config.dns.upstreams.push(parse_ipv4(value)?);
                saw_upstream = true;
            }
            "cache" => config.dns.cache_size = value.parse().unwrap_or(1000),
            "block" => {
                if valid_label(value) {
                    config.dns.blocklist.push(value.to_string());
                }
            }
            "user" => config.http.username = value.to_string(),
            "pass" => config.http.password = value.to_string(),
            "port" => config.http.port = value.parse().unwrap_or(80),
            "record" => {
                let mut parts = value.split_whitespace();
                let name = parts.next()?;
                let record_type = RecordType::parse(parts.next()?)?;
                let data = parts.next()?;
                let ttl = parts.next().and_then(|text| text.parse().ok()).unwrap_or(300);
                if !valid_label(name) || !valid_label(data) {
                    continue;
                }
                config.dns.records.push(DnsRecord {
                    name: name.to_string(),
                    record_type,
                    value: data.to_string(),
                    ttl,
                });
                saw_record = true;
            }
            _ => {}
        }
    }

    if !saw_record {
        config.dns.records = default_config().dns.records;
    }
    if !saw_upstream {
        config.dns.upstreams = default_config().dns.upstreams;
    }
    config.network.mode = if mode_dhcp {
        NetworkMode::Dhcp {
            fallback_ip: ip,
            fallback_netmask: netmask,
            fallback_gateway: gateway,
            timeout_ms,
        }
    } else {
        NetworkMode::Static { ip, netmask, gateway }
    };
    Some(config)
}

fn serialize_config(config: &Config) -> String {
    let mut out = String::new();
    match config.network.mode {
        NetworkMode::Static { ip, netmask, gateway } => {
            out.push_str("mode=static\n");
            push_ip(&mut out, "ip", ip);
            push_ip(&mut out, "netmask", netmask);
            push_ip(&mut out, "gateway", gateway);
        }
        NetworkMode::Dhcp {
            fallback_ip,
            fallback_netmask,
            fallback_gateway,
            timeout_ms,
        } => {
            out.push_str("mode=dhcp\n");
            push_ip(&mut out, "ip", fallback_ip);
            push_ip(&mut out, "netmask", fallback_netmask);
            push_ip(&mut out, "gateway", fallback_gateway);
            out.push_str(&alloc::format!("timeout_ms={timeout_ms}\n"));
        }
    }
    for upstream in &config.dns.upstreams {
        push_ip(&mut out, "upstream", *upstream);
    }
    out.push_str(&alloc::format!("cache={}\n", config.dns.cache_size));
    for name in &config.dns.blocklist {
        out.push_str(&alloc::format!("block={name}\n"));
    }
    out.push_str(&alloc::format!("user={}\n", config.http.username));
    out.push_str(&alloc::format!("pass={}\n", config.http.password));
    out.push_str(&alloc::format!("port={}\n", config.http.port));
    for record in &config.dns.records {
        out.push_str(&alloc::format!(
            "record={} {} {} {}\n",
            record.name,
            record.record_type.as_str(),
            record.value,
            record.ttl
        ));
    }
    out
}

fn push_ip(out: &mut String, key: &str, ip: [u8; 4]) {
    out.push_str(&alloc::format!(
        "{key}={}.{}.{}.{}\n",
        ip[0], ip[1], ip[2], ip[3]
    ));
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([a, b, c, d])
}

fn valid_label(text: &str) -> bool {
    let len = text.len();
    (1..=64).contains(&len)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b':')
}

fn names_eq(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('.');
    let right = right.trim_end_matches('.');
    left.eq_ignore_ascii_case(right)
}
