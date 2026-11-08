use core::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering};

use crate::net::NetworkMode;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

use config::{self, Value};

#[derive(Debug, Clone)]
pub struct Config {
    pub network: NetworkConfig,
    pub dns: DnsConfig,
    pub http: HttpConfig,
    pub display: DisplayConfig,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub vnc: bool,
    pub vnc_port: u16,
    pub vnc_password: String,
    pub hdmi: bool,
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
    pub cache_reap_secs: u64,
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
static LOG_MAX: AtomicU32 = AtomicU32::new(config::LOG_MIN_BYTES as u32);
static LOG_ARCHIVES: AtomicU32 = AtomicU32::new(3);
static LOG_CACHED: Mutex<[u8; 512]> = Mutex::new([0; 512]);
static LOG_CACHED_LEN: AtomicUsize = AtomicUsize::new(0);
/// 1 start, 2 stop, 3 stop then start. The poll loop consumes these.
static VNC_CMD: AtomicU8 = AtomicU8::new(0);
static HDMI_CMD: AtomicU8 = AtomicU8::new(0);

pub fn load_config() -> Config {
    let config = load_main();
    *CURRENT.lock() = Some(config.clone());
    if let Err(err) = reload_log() {
        log::info!("Log config unchanged: {err}");
    }
    config
}

pub fn log_limits() -> (u32, u32) {
    (
        LOG_MAX.load(Ordering::Relaxed),
        LOG_ARCHIVES.load(Ordering::Relaxed),
    )
}

/// Re-read `LOG.JSN`. The log task calls this when it starts and while it runs,
/// so a replacement file is picked up without a reboot.
pub fn reload_log() -> Result<(), String> {
    match crate::fs::read_text(crate::fs::LOG_CONFIG_NAME) {
        Ok(text) => {
            if cached_log(&text) {
                return Ok(());
            }
            match log_from_text(&text) {
                Ok((max, archives)) => {
                    remember_log(max, archives, &text);
                    log::info!("Loaded log config from filesystem");
                    Ok(())
                }
                Err(err) => {
                    log::info!("LOG.JSN unreadable ({err}), writing default");
                    write_default_log()
                }
            }
        }
        Err(crate::fs::FsError::NotInitialized) => Ok(()),
        Err(_) => write_default_log(),
    }
}

pub fn copy_config_file(which: u8, out: &mut [u8]) -> Result<usize, String> {
    let name = file_name(which)?;
    let text = match crate::fs::read_text(name) {
        Ok(text) => text,
        Err(crate::fs::FsError::NotInitialized) => {
            if which == 1 {
                default_log_json()
            } else {
                let current = CURRENT.lock();
                match current.as_ref() {
                    Some(config) => serialize_config(config),
                    None => serialize_config(&default_config()),
                }
            }
        }
        Err(_) if which == 1 => {
            write_default_log()?;
            default_log_json()
        }
        Err(err) => return Err(alloc::format!("{err}")),
    };
    if text.len() > out.len() {
        return Err(String::from("config does not fit"));
    }
    out[..text.len()].copy_from_slice(text.as_bytes());
    Ok(text.len())
}

/// Validate the whole document, then replace the file. Nothing is written
/// when validation fails. `Ok(true)` means the NIC or the HTTP port changed
/// and a reboot is required before that part takes effect.
pub fn store_config_file(which: u8, text: &str) -> Result<bool, String> {
    if text.len() > config::UPLOAD_MAX {
        return Err(String::from("upload exceeds 4096 bytes"));
    }
    match which {
        0 => {
            let next = config_from_text(text)?;
            let previous = CURRENT.lock().clone();
            let restart = previous
                .as_ref()
                .map(|old| needs_restart(old, &next))
                .unwrap_or(false);
            crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes())
                .map_err(|err| alloc::format!("save failed: {err}"))?;
            *CURRENT.lock() = Some(next.clone());
            let old = previous.unwrap_or_else(default_config);
            queue_display(&old, &next);
            log::info!("Replaced CONFIG.JSN");
            Ok(restart)
        }
        1 => {
            let (max, archives) = log_from_text(text)?;
            crate::fs::replace_file(crate::fs::LOG_CONFIG_NAME, text.as_bytes())
                .map_err(|err| alloc::format!("save failed: {err}"))?;
            remember_log(max, archives, text);
            log::info!("Replaced LOG.JSN");
            Ok(false)
        }
        _ => Err(String::from("unknown config")),
    }
}

pub fn fill_log(count: u32) {
    let count = count.min(200);
    let mut index = 0;
    while index < count {
        crate::fs::append_log_direct(&alloc::format!("mark {index:04}"));
        index += 1;
    }
}

pub fn cache_size() -> usize {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.cache_size)
        .unwrap_or(config::CACHE_DEFAULT as usize)
}

pub fn cache_reap_secs() -> u64 {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.cache_reap_secs)
        .unwrap_or(config::REAP_DEFAULT)
}

/// Update the two DNS cache fields and rewrite `CONFIG.JSN`. `Ok(true)` means
/// `cache_size` changed, so the DNS task must restart before the new pool is
/// reserved. The reap interval is read on each pass and does not need a restart.
pub fn set_dns_cache(slots: u64, reap_secs: u64) -> Result<bool, String> {
    if slots < config::CACHE_MIN || slots > config::CACHE_MAX {
        return Err(alloc::format!(
            "dns.cache_size: out of bounds ({}..={})",
            config::CACHE_MIN,
            config::CACHE_MAX
        ));
    }
    if reap_secs < config::REAP_MIN || reap_secs > config::REAP_MAX {
        return Err(alloc::format!(
            "dns.cache_reap_interval_secs: out of bounds ({}..={})",
            config::REAP_MIN,
            config::REAP_MAX
        ));
    }
    let mut next = CURRENT.lock().clone().unwrap_or_else(default_config);
    let restart = next.dns.cache_size != slots as usize;
    next.dns.cache_size = slots as usize;
    next.dns.cache_reap_secs = reap_secs;
    let text = serialize_config(&next);
    crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes())
        .map_err(|err| alloc::format!("save failed: {err}"))?;
    *CURRENT.lock() = Some(next);
    log::info!("Updated DNS cache settings");
    Ok(restart)
}

pub fn vnc_port() -> u16 {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.display.vnc_port)
        .unwrap_or(config::VNC_PORT_DEFAULT as u16)
}

pub fn vnc_password() -> String {
    CURRENT
        .lock()
        .as_ref()
        .map(|config| config.display.vnc_password.clone())
        .unwrap_or_else(String::new)
}

/// Persist the display object and queue a supervisor start or stop. The
/// caller may hold an FD lock. This does not take the supervisor lock.
pub fn set_display(vnc: u64, port: u64, password: &str, hdmi: u64) -> Result<(), String> {
    if vnc > 1 {
        return Err(String::from("display.vnc: out of bounds (0..=1)"));
    }
    if hdmi > 1 {
        return Err(String::from("display.hdmi: out of bounds (0..=1)"));
    }
    if port < 1 || port > config::PORT_MAX {
        return Err(String::from("display.vnc_port: out of bounds (1..=65535)"));
    }
    if !vnc_password_ok(password) {
        return Err(alloc::format!(
            "display.vnc_password: out of bounds (0..={})",
            config::VNC_PASS_MAX
        ));
    }
    let mut next = CURRENT.lock().clone().unwrap_or_else(default_config);
    let old = next.clone();
    next.display = DisplayConfig {
        vnc: vnc != 0,
        vnc_port: port as u16,
        vnc_password: String::from(password),
        hdmi: hdmi != 0,
    };
    let text = serialize_config(&next);
    match crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes()) {
        Ok(()) => {}
        Err(crate::fs::FsError::NotInitialized) => {}
        Err(err) => return Err(alloc::format!("save failed: {err}")),
    }
    *CURRENT.lock() = Some(next.clone());
    queue_display(&old, &next);
    log::info!("Updated display settings");
    Ok(())
}

pub fn take_display_cmd() -> (u8, u8) {
    (
        VNC_CMD.swap(0, Ordering::AcqRel),
        HDMI_CMD.swap(0, Ordering::AcqRel),
    )
}

pub fn restore_display_cmd(vnc: u8, hdmi: u8) {
    if vnc != 0 {
        let _ = VNC_CMD.compare_exchange(0, vnc, Ordering::AcqRel, Ordering::Acquire);
    }
    if hdmi != 0 {
        let _ = HDMI_CMD.compare_exchange(0, hdmi, Ordering::AcqRel, Ordering::Acquire);
    }
}

fn queue_display(old: &Config, new: &Config) {
    let vnc_changed = old.display.vnc_port != new.display.vnc_port
        || old.display.vnc_password != new.display.vnc_password;
    let vnc = display_cmd(old.display.vnc, new.display.vnc, vnc_changed);
    let hdmi = display_cmd(old.display.hdmi, new.display.hdmi, false);
    if vnc != 0 {
        VNC_CMD.store(vnc, Ordering::Release);
    }
    if hdmi != 0 {
        HDMI_CMD.store(hdmi, Ordering::Release);
    }
}

fn display_cmd(was_on: bool, now_on: bool, changed: bool) -> u8 {
    if was_on == now_on && !(was_on && changed) {
        return 0;
    }
    if !now_on {
        2
    } else if was_on {
        3
    } else {
        1
    }
}

fn vnc_password_ok(text: &str) -> bool {
    text.len() <= config::VNC_PASS_MAX && text.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
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

pub fn credentials_match(user: &str, pass: &str) -> bool {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return false;
    };
    config.http.username == user && config.http.password == pass
}

/// `port username` without the password. The HTTP task builds `/config` from this.
pub fn write_http_info(out: &mut [u8]) -> Option<usize> {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return Some(0);
    };
    let mut used = push_u32(out, 0, config.http.port as u32)?;
    used = push_slice(out, used, b" ")?;
    used = push_slice(out, used, config.http.username.as_bytes())?;
    push_slice(out, used, b"\n")
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
            cache_size: config::CACHE_DEFAULT as usize,
            cache_reap_secs: config::REAP_DEFAULT,
        },
        http: HttpConfig {
            port: 80,
            username: String::from("admin"),
            password: String::from("admin"),
        },
        display: DisplayConfig {
            vnc: false,
            vnc_port: config::VNC_PORT_DEFAULT as u16,
            vnc_password: String::new(),
            hdmi: false,
        },
        timeout_ms: 3_000,
    }
}

fn default_network_mode() -> NetworkMode {
    crate::board::default_network()
}

fn load_main() -> Config {
    if let Ok(text) = crate::fs::read_text(crate::fs::CONFIG_NAME) {
        match config_from_text(&text) {
            Ok(config) => {
                log::info!("Loaded config from filesystem");
                return config;
            }
            Err(err) => log::info!("CONFIG.JSN unreadable ({err})"),
        }
    } else if let Ok(text) = crate::fs::read_text(crate::fs::LEGACY_CONFIG) {
        if let Some(config) = parse_legacy(&text) {
            let json = serialize_config(&config);
            if crate::fs::write_config(&json).is_ok() {
                log::info!("Migrated CONFIG.TXT to CONFIG.JSN");
            }
            log::info!("Loaded config from filesystem");
            return config;
        }
    } else if let Ok(text) = crate::fs::read_text("CFG.OLD") {
        if let Ok(config) = config_from_text(&text) {
            let _ = crate::fs::rename_file("CFG.OLD", crate::fs::CONFIG_NAME);
            log::info!("Loaded config from filesystem");
            return config;
        }
    }
    let config = default_config();
    match crate::fs::write_config(&serialize_config(&config)) {
        Ok(()) => log::info!("Wrote default CONFIG.JSN"),
        Err(_) => log::info!("Using default config"),
    }
    config
}

fn write_default_log() -> Result<(), String> {
    let text = default_log_json();
    let (max, archives) = log_from_text(&text)?;
    match crate::fs::replace_file(crate::fs::LOG_CONFIG_NAME, text.as_bytes()) {
        Ok(()) => {
            remember_log(max, archives, &text);
            log::info!("Wrote default LOG.JSN");
            Ok(())
        }
        Err(crate::fs::FsError::NotInitialized) => {
            remember_log(max, archives, &text);
            Ok(())
        }
        Err(err) => Err(alloc::format!("{err}")),
    }
}

fn default_log_json() -> String {
    String::from("{\"version\":1,\"max_bytes\":4096,\"archives\":3}")
}

fn file_name(which: u8) -> Result<&'static str, String> {
    match which {
        0 => Ok(crate::fs::CONFIG_NAME),
        1 => Ok(crate::fs::LOG_CONFIG_NAME),
        _ => Err(String::from("unknown config")),
    }
}

fn cached_log(text: &str) -> bool {
    let len = LOG_CACHED_LEN.load(Ordering::Relaxed);
    if len == 0 || len != text.len() || len > 512 {
        return false;
    }
    let cached = LOG_CACHED.lock();
    cached[..len] == *text.as_bytes()
}

fn remember_log(max: u32, archives: u32, text: &str) {
    LOG_MAX.store(max, Ordering::Relaxed);
    LOG_ARCHIVES.store(archives, Ordering::Relaxed);
    let mut cached = LOG_CACHED.lock();
    if text.len() <= cached.len() {
        cached[..text.len()].copy_from_slice(text.as_bytes());
        LOG_CACHED_LEN.store(text.len(), Ordering::Relaxed);
    } else {
        LOG_CACHED_LEN.store(0, Ordering::Relaxed);
    }
}

fn log_from_text(text: &str) -> Result<(u32, u32), String> {
    let value = config::parse(text).map_err(|err| err.text())?;
    config::check(&value, config::log_fields()).map_err(|err| err.text())?;
    let max = value
        .get("max_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| String::from("max_bytes: missing field"))?;
    let archives = value
        .get("archives")
        .and_then(Value::as_u64)
        .ok_or_else(|| String::from("archives: missing field"))?;
    config::log_budget(max, archives).map_err(|err| err.text())?;
    Ok((max as u32, archives as u32))
}

fn config_from_text(text: &str) -> Result<Config, String> {
    let value = config::parse(text).map_err(|err| err.text())?;
    config::check(&value, config::main_fields()).map_err(|err| err.text())?;
    let network = value
        .get("network")
        .ok_or_else(|| String::from("network: missing field"))?;
    let dns = value
        .get("dns")
        .ok_or_else(|| String::from("dns: missing field"))?;
    let http = value
        .get("http")
        .ok_or_else(|| String::from("http: missing field"))?;
    let mode = network
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("static");
    let ip = require_ipv4(network, "ip")?;
    let netmask = require_ipv4(network, "netmask")?;
    let gateway = require_ipv4(network, "gateway")?;
    let timeout_ms = network
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(3_000);
    let username = require_secret(http, "username")?;
    let password = require_secret(http, "password")?;
    let port = http.get("port").and_then(Value::as_u64).unwrap_or(80) as u16;
    let cache_size = dns
        .get("cache_size")
        .and_then(Value::as_u64)
        .unwrap_or(config::CACHE_DEFAULT) as usize;
    let cache_reap_secs = dns
        .get("cache_reap_interval_secs")
        .and_then(Value::as_u64)
        .unwrap_or(config::REAP_DEFAULT);
    let mut upstreams = Vec::new();
    if let Some(items) = dns.get("upstreams").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            let text = item.as_str().unwrap_or("");
            let ip = parse_ipv4(text).ok_or_else(|| {
                alloc::format!("dns.upstreams[{index}]: expected ipv4 address")
            })?;
            upstreams.push(ip);
        }
    }
    let mut blocklist = Vec::new();
    if let Some(items) = dns.get("blocklist").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            let text = item.as_str().unwrap_or("");
            if !valid_label(text) {
                return Err(alloc::format!("dns.blocklist[{index}]: out of bounds"));
            }
            blocklist.push(text.to_string());
        }
    }
    let mut records = Vec::new();
    if let Some(items) = dns.get("records").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            let data = item.get("value").and_then(Value::as_str).unwrap_or("");
            let ttl = item.get("ttl").and_then(Value::as_u64).unwrap_or(300) as u32;
            if !valid_label(name) || !valid_label(data) {
                return Err(alloc::format!("dns.records[{index}]: out of bounds"));
            }
            if records.iter().any(|record: &DnsRecord| names_eq(&record.name, name)) {
                return Err(alloc::format!("dns.records[{index}].name: duplicate"));
            }
            let record_type = RecordType::parse(kind)
                .ok_or_else(|| alloc::format!("dns.records[{index}].type: out of bounds"))?;
            records.push(DnsRecord {
                name: name.to_string(),
                record_type,
                value: data.to_string(),
                ttl,
            });
        }
    }
    let display = value
        .get("display")
        .ok_or_else(|| String::from("display: missing field"))?;
    let vnc = display.get("vnc").and_then(Value::as_u64).unwrap_or(0) != 0;
    let hdmi = display.get("hdmi").and_then(Value::as_u64).unwrap_or(0) != 0;
    let vnc_port = display
        .get("vnc_port")
        .and_then(Value::as_u64)
        .unwrap_or(config::VNC_PORT_DEFAULT) as u16;
    let vnc_password = display
        .get("vnc_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !vnc_password_ok(vnc_password) {
        return Err(alloc::format!(
            "display.vnc_password: out of bounds (0..={})",
            config::VNC_PASS_MAX
        ));
    }
    let network = if mode == "dhcp" {
        NetworkMode::Dhcp {
            fallback_ip: ip,
            fallback_netmask: netmask,
            fallback_gateway: gateway,
            timeout_ms,
        }
    } else {
        NetworkMode::Static { ip, netmask, gateway }
    };
    Ok(Config {
        network: NetworkConfig { mode: network },
        dns: DnsConfig {
            records,
            upstreams,
            blocklist,
            cache_size,
            cache_reap_secs,
        },
        http: HttpConfig {
            port,
            username,
            password,
        },
        display: DisplayConfig {
            vnc,
            vnc_port,
            vnc_password: vnc_password.to_string(),
            hdmi,
        },
        timeout_ms,
    })
}

fn require_ipv4(value: &Value, key: &str) -> Result<[u8; 4], String> {
    let text = value.get(key).and_then(Value::as_str).unwrap_or("");
    parse_ipv4(text).ok_or_else(|| alloc::format!("{key}: expected ipv4 address"))
}

fn require_secret(value: &Value, key: &str) -> Result<String, String> {
    let text = value.get(key).and_then(Value::as_str).unwrap_or("");
    if text.is_empty()
        || text.len() > config::SECRET_MAX
        || !text.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err(alloc::format!("{key}: out of bounds"));
    }
    Ok(text.to_string())
}

fn needs_restart(old: &Config, new: &Config) -> bool {
    old.http.port != new.http.port
        || old.dns.cache_size != new.dns.cache_size
        || !same_mode(&old.network.mode, &new.network.mode)
}

fn same_mode(left: &NetworkMode, right: &NetworkMode) -> bool {
    match (left, right) {
        (
            NetworkMode::Static { ip, netmask, gateway },
            NetworkMode::Static {
                ip: ip2,
                netmask: mask2,
                gateway: gw2,
            },
        ) => ip == ip2 && netmask == mask2 && gateway == gw2,
        (
            NetworkMode::Dhcp {
                fallback_ip,
                fallback_netmask,
                fallback_gateway,
                timeout_ms,
            },
            NetworkMode::Dhcp {
                fallback_ip: ip2,
                fallback_netmask: mask2,
                fallback_gateway: gw2,
                timeout_ms: timeout2,
            },
        ) => {
            fallback_ip == ip2
                && fallback_netmask == mask2
                && fallback_gateway == gw2
                && timeout_ms == timeout2
        }
        _ => false,
    }
}

fn parse_legacy(text: &str) -> Option<Config> {
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
            "cache" => {
                config.dns.cache_size = clamp_u64(
                    value.parse().unwrap_or(config::CACHE_DEFAULT),
                    config::CACHE_MIN,
                    config::CACHE_MAX,
                ) as usize
            }
            "cache_reap" => {
                config.dns.cache_reap_secs = clamp_u64(
                    value.parse().unwrap_or(config::REAP_DEFAULT),
                    config::REAP_MIN,
                    config::REAP_MAX,
                )
            }
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
    config.timeout_ms = timeout_ms;
    Some(config)
}

fn serialize_config(config: &Config) -> String {
    let (mode, ip, netmask, gateway, timeout_ms) = match config.network.mode {
        NetworkMode::Static { ip, netmask, gateway } => {
            ("static", ip, netmask, gateway, config.timeout_ms)
        }
        NetworkMode::Dhcp {
            fallback_ip,
            fallback_netmask,
            fallback_gateway,
            timeout_ms,
        } => (
            "dhcp",
            fallback_ip,
            fallback_netmask,
            fallback_gateway,
            timeout_ms,
        ),
    };
    let mut out = String::from("{\"version\":1,\"network\":{");
    out.push_str(&alloc::format!("\"mode\":\"{mode}\","));
    push_ip_json(&mut out, "ip", ip);
    out.push(',');
    push_ip_json(&mut out, "netmask", netmask);
    out.push(',');
    push_ip_json(&mut out, "gateway", gateway);
    out.push_str(&alloc::format!(",\"timeout_ms\":{timeout_ms}"));
    out.push_str("},\"dns\":{");
    out.push_str(&alloc::format!(
        "\"cache_size\":{},\"cache_reap_interval_secs\":{},\"upstreams\":[",
        config.dns.cache_size, config.dns.cache_reap_secs
    ));
    for (index, upstream) in config.dns.upstreams.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        push_ip_text(&mut out, *upstream);
        out.push('"');
    }
    out.push_str("],\"blocklist\":[");
    for (index, name) in config.dns.blocklist.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_str(&mut out, name);
    }
    out.push_str("],\"records\":[");
    for (index, record) in config.dns.records.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_json_str(&mut out, &record.name);
        out.push_str(",\"type\":");
        push_json_str(&mut out, record.record_type.as_str());
        out.push_str(",\"value\":");
        push_json_str(&mut out, &record.value);
        out.push_str(&alloc::format!(",\"ttl\":{}}}", record.ttl));
    }
    out.push_str("]},\"http\":{\"port\":");
    out.push_str(&alloc::format!("{},\"username\":", config.http.port));
    push_json_str(&mut out, &config.http.username);
    out.push_str(",\"password\":");
    push_json_str(&mut out, &config.http.password);
    out.push_str("},\"display\":{");
    out.push_str(&alloc::format!(
        "\"vnc\":{},\"vnc_port\":{},\"vnc_password\":",
        u8::from(config.display.vnc),
        config.display.vnc_port
    ));
    push_json_str(&mut out, &config.display.vnc_password);
    // Append the flag and both closing braces as plain text. A trailing
    // escaped brace in format! was dropped from the last piece on this target.
    out.push_str(",\"hdmi\":");
    out.push(char::from(b'0' + u8::from(config.display.hdmi)));
    out.push_str("}}");
    out
}

fn clamp_u64(value: u64, min: u64, max: u64) -> u64 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn push_ip_json(out: &mut String, key: &str, ip: [u8; 4]) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    push_ip_text(out, ip);
    out.push('"');
}

fn push_ip_text(out: &mut String, ip: [u8; 4]) {
    out.push_str(&alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]));
}

fn push_json_str(out: &mut String, text: &str) {
    out.push('"');
    for byte in text.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b if (0x20..=0x7e).contains(&b) => out.push(b as char),
            b => out.push_str(&alloc::format!("\\u{b:04x}")),
        }
    }
    out.push('"');
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
