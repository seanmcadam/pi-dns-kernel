use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::net::NetworkMode;
use alloc::string::{String, ToString};
use alloc::vec;
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
    /// `board.detect`. Missing means on. Off reports the build-time board.
    pub board_detect: bool,
    pub ntp: NtpConfig,
    pub block: BlockConfig,
}

#[derive(Debug, Clone)]
pub struct BlockConfig {
    pub enabled: bool,
    pub refresh_secs: u64,
    pub urls: Vec<String>,
    pub allow: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NtpConfig {
    pub servers: Vec<String>,
    pub interval_secs: u64,
    pub serve: bool,
}

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub vnc: bool,
    pub vnc_port: u16,
    pub vnc_password: String,
    pub hdmi: bool,
    /// Off unless `display.hdmi_reset` is 1. Arms the extra HDMI
    /// shutdown before a reboot. The default reboot path does not use it.
    pub hdmi_reset: bool,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub mode: NetworkMode,
    /// 0 autoneg, 100 advertise 100full only, 1000 advertise gigabit.
    pub speed_cap: u32,
    /// 0 sends one gratuitous ARP. Any other value repeats it, in seconds.
    pub arp_secs: u64,
}

#[derive(Debug, Clone)]
pub struct DnsConfig {
    pub records: Vec<DnsRecord>,
    pub upstreams: Vec<[u8; 4]>,
    pub blocklist: Vec<String>,
    pub cache_size: usize,
    pub cache_reap_secs: u64,
    /// Seconds between health probes of each upstream. 1..=3600, default 5.
    pub probe_secs: u64,
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
static BOARD_DETECT: AtomicBool = AtomicBool::new(true);
static LOG_MAX: AtomicU32 = AtomicU32::new(config::LOG_MIN_BYTES as u32);
static LOG_ARCHIVES: AtomicU32 = AtomicU32::new(3);
static LOG_CACHED: Mutex<[u8; 512]> = Mutex::new([0; 512]);
static LOG_CACHED_LEN: AtomicUsize = AtomicUsize::new(0);
/// 1 start, 2 stop, 3 stop then start. The poll loop consumes these.
static VNC_CMD: AtomicU8 = AtomicU8::new(0);
static HDMI_CMD: AtomicU8 = AtomicU8::new(0);
#[cfg(feature = "board-pi4")]
static HDMI_RESET: AtomicBool = AtomicBool::new(false);

fn publish(config: &Config) {
    let previous = CURRENT.lock().clone();
    crate::block_tab::set_allow(&config.block.allow);
    if let Some(old) = previous.as_ref() {
        let urls_changed = old.block.urls != config.block.urls;
        let enabled_changed = old.block.enabled != config.block.enabled;
        if urls_changed || enabled_changed {
            BLOCK_EPOCH.fetch_add(1, Ordering::Release);
            // `CURRENT` is not held. Deleting the cache takes `FS`.
            retire_block_caches(&old.block.urls, config.block.enabled, &config.block.urls);
            if blocklist::must_unload(config.block.enabled, config.block.urls.len())
                && old.block.enabled
                && !old.block.urls.is_empty()
            {
                crate::block_tab::unload();
            }
        }
    }
    BOARD_DETECT.store(config.board_detect, Ordering::Relaxed);
    NTP_SERVE.store(config.ntp.serve, Ordering::Relaxed);
    NTP_TOKEN.store(ntp_hash(&config.ntp), Ordering::Relaxed);
    #[cfg(feature = "board-pi4")]
    HDMI_RESET.store(config.display.hdmi_reset, Ordering::Relaxed);
}

static NTP_SERVE: AtomicBool = AtomicBool::new(true);
static NTP_TOKEN: AtomicU64 = AtomicU64::new(0);
/// Bumped when the block URL list or enabled flag changes. The blocker
/// publishes until it has applied this value. An allow-list edit does not
/// bump it.
static BLOCK_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn ntp_serve() -> bool {
    NTP_SERVE.load(Ordering::Relaxed)
}

pub struct NtpView {
    pub servers: [[u8; 64]; 4],
    pub lens: [u8; 4],
    pub ports: [u16; 4],
    pub ips: [[u8; 4]; 4],
    pub have_ip: [bool; 4],
    pub count: u8,
    pub interval_secs: u64,
    pub serve: bool,
    pub token: u64,
}

pub fn ntp_view() -> NtpView {
    let current = CURRENT.lock();
    let ntp = current.as_ref().map(|config| &config.ntp);
    let mut view = NtpView {
        servers: [[0; 64]; 4],
        lens: [0; 4],
        ports: [123; 4],
        ips: [[0; 4]; 4],
        have_ip: [false; 4],
        count: 0,
        interval_secs: config::NTP_INTERVAL_DEFAULT,
        serve: true,
        token: NTP_TOKEN.load(Ordering::Relaxed),
    };
    let Some(ntp) = ntp else {
        return view;
    };
    view.interval_secs = ntp.interval_secs;
    view.serve = ntp.serve;
    view.token = ntp_hash(ntp);
    for server in &ntp.servers {
        if view.count as usize >= view.servers.len() {
            break;
        }
        let Some((host, port, ip)) = config::ntp_server_parts(server) else {
            continue;
        };
        let index = view.count as usize;
        let bytes = host.as_bytes();
        let n = bytes.len().min(view.servers[index].len());
        view.servers[index][..n].copy_from_slice(&bytes[..n]);
        view.lens[index] = n as u8;
        view.ports[index] = port;
        if let Some(ip) = ip {
            view.ips[index] = ip;
            view.have_ip[index] = true;
        }
        view.count += 1;
    }
    view
}

pub fn copy_resolvers(out: &mut [[u8; 4]]) -> usize {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return 0;
    };
    let mut nout = 0;
    let mut seen = [[0u8; 4]; 8];
    let mut nseen = 0usize;
    let lease = LEASE_IP.load(Ordering::Relaxed).to_be_bytes();
    let mut index = 0;
    while index < DHCP_DNS.len() && nout < out.len() {
        let raw = DHCP_DNS[index].load(Ordering::Relaxed);
        index += 1;
        if raw == 0 {
            continue;
        }
        let ip = raw.to_be_bytes();
        if crate::dhcp_rx::omit_slirp_dns(ip, lease) || seen[..nseen].contains(&ip) {
            continue;
        }
        out[nout] = ip;
        nout += 1;
        if nseen < seen.len() {
            seen[nseen] = ip;
            nseen += 1;
        }
    }
    for ip in &config.dns.upstreams {
        if nout >= out.len() {
            break;
        }
        if crate::dhcp_rx::omit_slirp_dns(*ip, lease) || seen[..nseen].contains(ip) {
            continue;
        }
        out[nout] = *ip;
        nout += 1;
        if nseen < seen.len() {
            seen[nseen] = *ip;
            nseen += 1;
        }
    }
    nout
}

fn ntp_hash(ntp: &NtpConfig) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for server in &ntp.servers {
        for byte in server.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    for byte in ntp.interval_secs.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash ^= u64::from(ntp.serve);
    hash
}

/// `board.detect` from JSON. Missing means the build still reads the revision.
pub fn board_detect() -> bool {
    BOARD_DETECT.load(Ordering::Relaxed)
}

/// `display.hdmi_reset` from the loaded JSON. Missing means off.
#[cfg(feature = "board-pi4")]
pub fn hdmi_reset() -> bool {
    HDMI_RESET.load(Ordering::Relaxed)
}

pub fn load_config() -> Config {
    let config = load_main();
    publish(&config);
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
                    Some(config) => serialize_config(config)?,
                    None => serialize_config(&default_config())?,
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

/// Same checks as `store_config_file`, without writing.
pub fn check_config_text(which: u8, text: &str) -> Result<(), String> {
    if text.len() > config::UPLOAD_MAX {
        return Err(String::from("upload exceeds 4096 bytes"));
    }
    let value = config::parse(text).map_err(|err| err.text())?;
    match which {
        0 => {
            config::check(&value, config::main_fields()).map_err(|err| err.text())?;
            config_from_value(&value)?;
            Ok(())
        }
        1 => {
            config::check(&value, config::log_fields()).map_err(|err| err.text())?;
            log_numbers(&value)?;
            Ok(())
        }
        _ => Err(String::from("unknown config")),
    }
}

/// Validate the whole document, then replace the file. Nothing is written
/// when validation fails. `Ok(true)` means the NIC or the HTTP port changed
/// and a reboot is required before that part takes effect.
pub fn store_config_file(which: u8, text: &str) -> Result<bool, String> {
    if text.len() > config::UPLOAD_MAX {
        return Err(String::from("upload exceeds 4096 bytes"));
    }
    let value = config::parse(text).map_err(|err| err.text())?;
    match which {
        0 => {
            config::check(&value, config::main_fields()).map_err(|err| err.text())?;
            let next = config_from_value(&value)?;
            let stored = config::format_document(&value).map_err(|err| err.text())?;
            let previous = CURRENT.lock().clone();
            let restart = previous
                .as_ref()
                .map(|old| needs_restart(old, &next))
                .unwrap_or(false);
            keep_config_backup()?;
            crate::fs::replace_file(crate::fs::CONFIG_NAME, stored.as_bytes())
                .map_err(|err| alloc::format!("save failed: {err}"))?;
            match crate::fs::read_text(crate::fs::CONFIG_NAME) {
                Ok(got) if got == stored => {}
                _ => {
                    restore_config_backup();
                    return Err(String::from("save verify failed"));
                }
            }
            publish(&next);
            *CURRENT.lock() = Some(next.clone());
            let old = previous.unwrap_or_else(default_config);
            queue_display(&old, &next);
            log::info!("Replaced CONFIG.JSN");
            Ok(restart)
        }
        1 => {
            config::check(&value, config::log_fields()).map_err(|err| err.text())?;
            let (max, archives) = log_numbers(&value)?;
            let stored = config::format_document(&value).map_err(|err| err.text())?;
            crate::fs::replace_file(crate::fs::LOG_CONFIG_NAME, stored.as_bytes())
                .map_err(|err| alloc::format!("save failed: {err}"))?;
            remember_log(max, archives, &stored);
            log::info!("Replaced LOG.JSN");
            Ok(false)
        }
        _ => Err(String::from("unknown config")),
    }
}

fn keep_config_backup() -> Result<(), String> {
    match crate::fs::read_text(crate::fs::CONFIG_NAME) {
        Ok(prev) => crate::fs::write_named("CONFIG.BAK", prev.as_bytes(), false)
            .map_err(|err| alloc::format!("backup failed: {err}")),
        Err(crate::fs::FsError::Io { detail, .. }) if detail.contains("NotFound") => Ok(()),
        Err(crate::fs::FsError::NotInitialized) => Ok(()),
        Err(err) => Err(alloc::format!("backup failed: {err}")),
    }
}

fn restore_config_backup() {
    let Ok(prev) = crate::fs::read_text("CONFIG.BAK") else {
        return;
    };
    let _ = crate::fs::replace_file(crate::fs::CONFIG_NAME, prev.as_bytes());
}

static NET_LOOK: AtomicU8 = AtomicU8::new(0);
static NET_AT: AtomicU64 = AtomicU64::new(0);
static CSRF_WORD: AtomicU64 = AtomicU64::new(0);

pub fn csrf_bytes() -> [u8; 16] {
    let mut token = CSRF_WORD.load(Ordering::Acquire);
    if token == 0 {
        let mixed = crate::timer::millis() ^ (u64::from(version::BOOT_ID[20]) << 32) ^ 0xC5F5_001D;
        let mixed = if mixed == 0 { 1 } else { mixed };
        let _ = CSRF_WORD.compare_exchange(0, mixed, Ordering::AcqRel, Ordering::Acquire);
        token = CSRF_WORD.load(Ordering::Acquire);
    }
    let mut out = [0u8; 16];
    let hex = b"0123456789abcdef";
    let bytes = token.to_be_bytes();
    let mut i = 0;
    while i < 8 {
        out[i * 2] = hex[(bytes[i] >> 4) as usize];
        out[i * 2 + 1] = hex[(bytes[i] & 0xf) as usize];
        i += 1;
    }
    out
}

/// Arm the static-address watchdog. A later boot that never installs an
/// address copies `CONFIG.BAK` back. DHCP is left to its own fallback.
pub fn arm_net_pending() -> bool {
    let static_mode = CURRENT
        .lock()
        .as_ref()
        .is_some_and(|config| matches!(config.network.mode, NetworkMode::Static { .. }));
    if !static_mode {
        return false;
    }
    if crate::fs::write_named("NET.PND", b"static\n", false).is_err() {
        log::info!("network config pending file failed");
        return false;
    }
    NET_LOOK.store(1, Ordering::Release);
    NET_AT.store(0, Ordering::Release);
    log::info!("network config pending");
    true
}

/// Called from the main loop. One card look until a pending file is seen.
pub fn poll_net_pending() {
    let look = NET_LOOK.load(Ordering::Acquire);
    if look == 2 {
        return;
    }
    let now = crate::timer::millis();
    if look == 0 {
        let pending = crate::fs::read_text("NET.PND").is_ok();
        if !pending {
            NET_LOOK.store(2, Ordering::Release);
            return;
        }
        NET_LOOK.store(1, Ordering::Release);
        NET_AT.store(now, Ordering::Release);
    }
    let started = {
        let at = NET_AT.load(Ordering::Acquire);
        if at == 0 {
            NET_AT.store(now, Ordering::Release);
            now
        } else {
            at
        }
    };
    let static_mode = CURRENT
        .lock()
        .as_ref()
        .is_some_and(|config| matches!(config.network.mode, NetworkMode::Static { .. }));
    let word = config::net_watch(config::NetWatch {
        pending: true,
        static_mode,
        address_up: crate::net::has_address(),
        elapsed_ms: now.saturating_sub(started),
        limit_ms: 20_000,
    });
    match word {
        "hold" => {
            let _ = crate::fs::delete_file("NET.PND");
            NET_LOOK.store(2, Ordering::Release);
            log::info!("network config held");
        }
        "revert" => {
            restore_config_backup();
            if let Ok(prev) = crate::fs::read_text(crate::fs::CONFIG_NAME) {
                if let Ok(value) = config::parse(&prev) {
                    if let Ok(config) = config_from_value(&value) {
                        publish(&config);
                        *CURRENT.lock() = Some(config);
                    }
                }
            }
            let _ = crate::fs::delete_file("NET.PND");
            NET_LOOK.store(2, Ordering::Release);
            log::info!("network config reverted");
            let _ = crate::board::power(true);
        }
        _ => {}
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

/// Probe interval in seconds. Read on each pass. A missing or stored-out-of-range
/// value stays at the default so an older `CONFIG.JSN` keeps forwarding.
pub fn probe_interval_secs() -> u64 {
    let secs = CURRENT
        .lock()
        .as_ref()
        .map(|config| config.dns.probe_secs)
        .unwrap_or(config::PROBE_DEFAULT);
    if (config::PROBE_MIN..=config::PROBE_MAX).contains(&secs) {
        secs
    } else {
        config::PROBE_DEFAULT
    }
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
    let text = serialize_config(&next)?;
    crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes())
        .map_err(|err| alloc::format!("save failed: {err}"))?;
    publish(&next);
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
    let hdmi_reset = next.display.hdmi_reset;
    next.display = DisplayConfig {
        vnc: vnc != 0,
        vnc_port: port as u16,
        vnc_password: String::from(password),
        hdmi: hdmi != 0,
        hdmi_reset,
    };
    let text = serialize_config(&next)?;
    match crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes()) {
        Ok(()) => {}
        Err(crate::fs::FsError::NotInitialized) => {}
        Err(err) => return Err(alloc::format!("save failed: {err}")),
    }
    publish(&next);
    *CURRENT.lock() = Some(next.clone());
    queue_display(&old, &next);
    log::info!("Updated display settings");
    Ok(())
}

/// Persist `display.vnc` and always queue start (`1`) or stop (`2`).
/// A repeated start still queues, so a process that gave up can be asked again.
/// The poll loop applies the command. This does not take the supervisor lock.
pub fn request_vnc(on: bool) -> Result<(), String> {
    let mut next = CURRENT.lock().clone().unwrap_or_else(default_config);
    next.display.vnc = on;
    let text = serialize_config(&next)?;
    match crate::fs::replace_file(crate::fs::CONFIG_NAME, text.as_bytes()) {
        Ok(()) => {}
        Err(crate::fs::FsError::NotInitialized) => {}
        Err(err) => return Err(alloc::format!("save failed: {err}")),
    }
    publish(&next);
    *CURRENT.lock() = Some(next);
    VNC_CMD.store(if on { 1 } else { 2 }, Ordering::Release);
    log::info!("Queued VNC");
    Ok(())
}

pub fn vnc_pending() -> u8 {
    VNC_CMD.load(Ordering::Acquire)
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

/// Nameservers from the DHCP ACK (option 6). The zone file is not
/// rewritten. `write_upstreams` offers these first, then the configured
/// list, so a forwarded query can reach the resolver on the lease subnet
/// before 8.8.8.8. Eight slots: the stack's parser keeps only three.
static DHCP_DNS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
/// Set when a BOOTREPLY's option 6 was stored. The stack's shorter list
/// must not replace it.
static DHCP_DNS_FRAME: AtomicBool = AtomicBool::new(false);
/// Address installed for the lease, or zero before one exists.
static LEASE_IP: AtomicU32 = AtomicU32::new(0);

pub fn note_lease(ip: [u8; 4]) {
    LEASE_IP.store(u32::from_be_bytes(ip), Ordering::Relaxed);
}

/// Option 6 from the raw BOOTREPLY. A fourth server is past the three
/// the stack keeps, which is how 172.18.50.10 disappeared.
#[allow(dead_code)]
pub fn note_dhcp_dns_frame(frame: &[u8]) {
    let mut servers = [[0u8; 4]; 8];
    let n = crate::dhcp_rx::dns_from_bootreply(frame, &mut servers);
    if n == 0 {
        return;
    }
    note_dhcp_dns(&servers[..n]);
    DHCP_DNS_FRAME.store(true, Ordering::Relaxed);
    log_dhcp_dns(&servers[..n]);
}

/// The stack's list. Ignored once the frame parser stored option 6.
pub fn note_dhcp_dns_from_stack(servers: &[[u8; 4]]) {
    if DHCP_DNS_FRAME.load(Ordering::Relaxed) {
        return;
    }
    note_dhcp_dns(servers);
}

pub fn note_dhcp_dns(servers: &[[u8; 4]]) {
    if servers.is_empty() {
        DHCP_DNS_FRAME.store(false, Ordering::Relaxed);
    }
    let mut index = 0;
    while index < DHCP_DNS.len() {
        let ip = servers.get(index).copied().unwrap_or([0, 0, 0, 0]);
        DHCP_DNS[index].store(u32::from_be_bytes(ip), Ordering::Relaxed);
        index += 1;
    }
}

#[allow(dead_code)]
fn log_dhcp_dns(servers: &[[u8; 4]]) {
    let mut buf = [0u8; 80];
    let mut n = 0usize;
    for byte in b"dns dhcp " {
        buf[n] = *byte;
        n += 1;
    }
    let mut index = 0;
    while index < servers.len() {
        let ip = servers[index];
        index += 1;
        let mut tmp = [0u8; 16];
        let mut t = 0usize;
        if n != 9 {
            tmp[t] = b' ';
            t += 1;
        }
        t = push_dec(&mut tmp, t, ip[0]);
        tmp[t] = b'.';
        t += 1;
        t = push_dec(&mut tmp, t, ip[1]);
        tmp[t] = b'.';
        t += 1;
        t = push_dec(&mut tmp, t, ip[2]);
        tmp[t] = b'.';
        t += 1;
        t = push_dec(&mut tmp, t, ip[3]);
        if n + t > buf.len() {
            break;
        }
        buf[n..n + t].copy_from_slice(&tmp[..t]);
        n += t;
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        log::info!("{text}");
    }
}

#[allow(dead_code)]
fn push_dec(buf: &mut [u8], mut n: usize, value: u8) -> usize {
    let mut tmp = [0u8; 3];
    let end = minfmt::push_dec(&mut tmp, 0, value as u64);
    let mut i = 0;
    if value == 0 {
        buf[n] = tmp[0];
        return n + 1;
    }
    while i < end && n < buf.len() {
        buf[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

pub fn write_upstreams(out: &mut [u8]) -> Option<usize> {
    let current = CURRENT.lock();
    let Some(config) = current.as_ref() else {
        return Some(0);
    };
    let mut used = 0;
    let mut seen = [[0u8; 4]; 8];
    let mut nseen = 0usize;
    let lease = LEASE_IP.load(Ordering::Relaxed).to_be_bytes();
    let mut index = 0;
    while index < DHCP_DNS.len() {
        let raw = DHCP_DNS[index].load(Ordering::Relaxed);
        index += 1;
        if raw == 0 {
            continue;
        }
        let ip = raw.to_be_bytes();
        if crate::dhcp_rx::omit_slirp_dns(ip, lease) || seen[..nseen].contains(&ip) {
            continue;
        }
        used = append_ip(out, used, ip)?;
        used = push_slice(out, used, b"\n")?;
        if nseen < seen.len() {
            seen[nseen] = ip;
            nseen += 1;
        }
    }
    for ip in &config.dns.upstreams {
        if crate::dhcp_rx::omit_slirp_dns(*ip, lease) || seen[..nseen].contains(ip) {
            continue;
        }
        used = append_ip(out, used, *ip)?;
        used = push_slice(out, used, b"\n")?;
        if nseen < seen.len() {
            seen[nseen] = *ip;
            nseen += 1;
        }
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

/// Delete list files that are no longer configured. An empty URL list and
/// a disabled block section both drop every file the previous config had,
/// including the index, so the next boot does not reload them.
fn retire_block_caches(
    old_urls: &[alloc::string::String],
    enabled: bool,
    new_urls: &[alloc::string::String],
) {
    let mut kept = [""; blocklist::URL_MAX];
    let mut kept_n = 0;
    if enabled {
        for url in new_urls {
            if kept_n >= kept.len() {
                break;
            }
            kept[kept_n] = url.as_str();
            kept_n += 1;
        }
    }
    let kept = &kept[..kept_n];
    for url in old_urls {
        if !blocklist::retire_url(enabled, kept, url) {
            continue;
        }
        let list = blocklist::list_file(url);
        let meta = blocklist::meta_file(url);
        if let Ok(name) = core::str::from_utf8(&list) {
            let _ = crate::fs::delete_file(name);
        }
        if let Ok(name) = core::str::from_utf8(&meta) {
            let _ = crate::fs::delete_file(name);
        }
    }
    if blocklist::must_unload(enabled, new_urls.len()) {
        let _ = crate::fs::delete_file(blocklist::INDEX_NAME);
    }
}

pub fn block_text() -> Result<alloc::string::String, alloc::string::String> {
    let config = CURRENT.lock().clone().unwrap_or_else(default_config);
    let mut out = alloc::string::String::new();
    out.push_str(if config.block.enabled {
        "enabled 1\n"
    } else {
        "enabled 0\n"
    });
    out.push_str(&alloc::format!(
        "epoch {}\n",
        BLOCK_EPOCH.load(Ordering::Acquire)
    ));
    out.push_str(&alloc::format!("refresh {}\n", config.block.refresh_secs));
    for url in &config.block.urls {
        out.push_str("url ");
        out.push_str(url);
        out.push('\n');
    }
    for name in &config.block.allow {
        out.push_str("allow ");
        out.push_str(name);
        out.push('\n');
    }
    Ok(out)
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

fn push_u32(out: &mut [u8], used: usize, value: u32) -> Option<usize> {
    let mut buf = [0u8; 10];
    let n = minfmt::push_dec(&mut buf, 0, value as u64);
    push_slice(out, used, &buf[..n])
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
        config
            .dns
            .records
            .retain(|record| !names_eq(&record.name, name));
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
    let config = CURRENT
        .lock()
        .clone()
        .ok_or_else(|| String::from("no config"))?;
    let text = serialize_config(&config)?;
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
        value: String::from("10.0.2.25"),
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
            speed_cap: default_speed_cap(),
            arp_secs: default_arp_secs(),
        },
        dns: DnsConfig {
            records,
            upstreams: alloc::vec![[8, 8, 8, 8], [1, 1, 1, 1], [10, 0, 2, 3]],
            blocklist: Vec::new(),
            cache_size: config::CACHE_DEFAULT as usize,
            cache_reap_secs: config::REAP_DEFAULT,
            probe_secs: config::PROBE_DEFAULT,
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
            hdmi_reset: false,
        },
        timeout_ms: 3_000,
        board_detect: crate::board_rev::DETECT_DEFAULT,
        ntp: default_ntp(),
        block: default_block(),
    }
}

fn default_block() -> BlockConfig {
    BlockConfig {
        enabled: false,
        refresh_secs: config::BLOCK_REFRESH_DEFAULT,
        urls: Vec::new(),
        allow: Vec::new(),
    }
}

fn default_ntp() -> NtpConfig {
    NtpConfig {
        servers: config::NTP_DEFAULT_SERVERS
            .iter()
            .map(|server| String::from(*server))
            .collect(),
        interval_secs: config::NTP_INTERVAL_DEFAULT,
        serve: true,
    }
}

fn default_network_mode() -> NetworkMode {
    crate::board::default_network()
}

fn default_speed_cap() -> u32 {
    if cfg!(feature = "phy-100") {
        100
    } else {
        0
    }
}

fn default_arp_secs() -> u64 {
    if cfg!(feature = "board-pi4") {
        2
    } else {
        0
    }
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
            if let Ok(json) = serialize_config(&config) {
                if crate::fs::write_config(&json).is_ok() {
                    log::info!("Migrated CONFIG.TXT to CONFIG.JSN");
                }
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
    match serialize_config(&config).and_then(|json| crate::fs::write_config(&json)) {
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
    String::from(config::DEFAULT_LOG_JSON)
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
    log_numbers(&value)
}

fn log_numbers(value: &Value) -> Result<(u32, u32), String> {
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
    config_from_value(&value)
}

fn config_from_value(value: &Value) -> Result<Config, String> {
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
    let speed_cap = match network.get("speed").and_then(Value::as_u64) {
        None => default_speed_cap(),
        Some(0) => 0,
        Some(100) => 100,
        Some(1000) => 1000,
        Some(_) => return Err(String::from("network.speed: expected 0, 100, or 1000")),
    };
    let arp_secs = match network.get("arp_secs").and_then(Value::as_u64) {
        None => default_arp_secs(),
        Some(secs) if secs <= 60 => secs,
        Some(_) => return Err(String::from("network.arp_secs: expected 0..=60")),
    };
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
    let probe_secs = match dns.get("probe_interval_secs").and_then(Value::as_u64) {
        None => config::PROBE_DEFAULT,
        Some(secs) if (config::PROBE_MIN..=config::PROBE_MAX).contains(&secs) => secs,
        Some(_) => {
            return Err(alloc::format!(
                "dns.probe_interval_secs: out of bounds ({}..={})",
                config::PROBE_MIN,
                config::PROBE_MAX
            ))
        }
    };
    let mut upstreams = Vec::new();
    if let Some(items) = dns.get("upstreams").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            let text = item.as_str().unwrap_or("");
            let ip = parse_ipv4(text)
                .ok_or_else(|| alloc::format!("dns.upstreams[{index}]: expected ipv4 address"))?;
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
            if records
                .iter()
                .any(|record: &DnsRecord| names_eq(&record.name, name))
            {
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
    let board_detect = match value
        .get("board")
        .and_then(|board| board.get("detect"))
        .and_then(Value::as_u64)
    {
        None | Some(1) => crate::board_rev::DETECT_DEFAULT,
        Some(0) => false,
        Some(_) => return Err(String::from("board.detect: out of bounds (0..=1)")),
    };
    let hdmi_reset = match display.get("hdmi_reset").and_then(Value::as_u64) {
        None | Some(0) => false,
        Some(1) => true,
        Some(_) => return Err(String::from("display.hdmi_reset: out of bounds (0..=1)")),
    };
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
    config::check_block(value).map_err(|err| err.text())?;
    let block = match value.get("block") {
        None => default_block(),
        Some(block) => {
            let enabled = block.get("enabled").and_then(Value::as_u64).unwrap_or(0) != 0;
            let refresh_secs = block
                .get("refresh_secs")
                .and_then(Value::as_u64)
                .unwrap_or(config::BLOCK_REFRESH_DEFAULT);
            let mut urls = Vec::new();
            if let Some(items) = block.get("urls").and_then(Value::as_array) {
                for (index, item) in items.iter().enumerate() {
                    let text = item.as_str().unwrap_or("");
                    if !config::check_block_url(text) {
                        return Err(alloc::format!(
                            "block.urls[{index}]: expected http or https url"
                        ));
                    }
                    urls.push(text.to_string());
                }
            }
            if urls.len() > 1 {
                let mut index = 0;
                while index < urls.len() {
                    let name = blocklist::list_file(&urls[index]);
                    let mut other = index + 1;
                    while other < urls.len() {
                        if blocklist::list_file(&urls[other]) == name {
                            return Err(String::from("block.urls: filename collision"));
                        }
                        other += 1;
                    }
                    index += 1;
                }
            }
            let mut allow = Vec::new();
            if let Some(items) = block.get("allow").and_then(Value::as_array) {
                for (index, item) in items.iter().enumerate() {
                    let text = item.as_str().unwrap_or("");
                    if text.is_empty() || text.len() > config::BLOCK_ALLOW_LEN {
                        return Err(alloc::format!("block.allow[{index}]: out of bounds"));
                    }
                    allow.push(text.to_string());
                }
            }
            BlockConfig {
                enabled,
                refresh_secs,
                urls,
                allow,
            }
        }
    };
    let ntp = match value.get("ntp") {
        None => default_ntp(),
        Some(ntp) => {
            let mut servers = Vec::new();
            if let Some(items) = ntp.get("servers").and_then(Value::as_array) {
                for (index, item) in items.iter().enumerate() {
                    let text = item.as_str().unwrap_or("");
                    if config::parse_ntp_server(text).is_none() {
                        return Err(alloc::format!(
                            "ntp.servers[{index}]: expected host or ipv4"
                        ));
                    }
                    servers.push(text.to_string());
                }
            }
            if servers.is_empty() {
                return Err(String::from("ntp.servers: missing field"));
            }
            let interval_secs = ntp
                .get("interval_secs")
                .and_then(Value::as_u64)
                .unwrap_or(config::NTP_INTERVAL_DEFAULT);
            let serve = ntp.get("serve").and_then(Value::as_u64).unwrap_or(1) != 0;
            NtpConfig {
                servers,
                interval_secs,
                serve,
            }
        }
    };
    let network = if mode == "dhcp" {
        NetworkMode::Dhcp {
            fallback_ip: ip,
            fallback_netmask: netmask,
            fallback_gateway: gateway,
            timeout_ms,
        }
    } else {
        NetworkMode::Static {
            ip,
            netmask,
            gateway,
        }
    };
    Ok(Config {
        network: NetworkConfig {
            mode: network,
            speed_cap,
            arp_secs,
        },
        dns: DnsConfig {
            records,
            upstreams,
            blocklist,
            cache_size,
            cache_reap_secs,
            probe_secs,
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
            hdmi_reset,
        },
        timeout_ms,
        board_detect,
        ntp,
        block,
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
        || old.network.speed_cap != new.network.speed_cap
        || old.network.arp_secs != new.network.arp_secs
}

fn same_mode(left: &NetworkMode, right: &NetworkMode) -> bool {
    match (left, right) {
        (
            NetworkMode::Static {
                ip,
                netmask,
                gateway,
            },
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
                let ttl = parts
                    .next()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(300);
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
        NetworkMode::Static {
            ip,
            netmask,
            gateway,
        }
    };
    config.timeout_ms = timeout_ms;
    Some(config)
}

fn serialize_config(config: &Config) -> Result<String, String> {
    config::format_document(&config_value(config)).map_err(|err| err.text())
}

fn config_value(config: &Config) -> Value {
    let (mode, ip, netmask, gateway, timeout_ms) = match config.network.mode {
        NetworkMode::Static {
            ip,
            netmask,
            gateway,
        } => ("static", ip, netmask, gateway, config.timeout_ms),
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
    let mut upstreams = Vec::new();
    for upstream in &config.dns.upstreams {
        upstreams.push(Value::String(ip_text(*upstream)));
    }
    let mut blocklist = Vec::new();
    for name in &config.dns.blocklist {
        blocklist.push(Value::String(name.clone()));
    }
    let mut records = Vec::new();
    for record in &config.dns.records {
        records.push(Value::Object(vec![
            (String::from("name"), Value::String(record.name.clone())),
            (
                String::from("type"),
                Value::String(String::from(record.record_type.as_str())),
            ),
            (String::from("value"), Value::String(record.value.clone())),
            (String::from("ttl"), Value::Number(u64::from(record.ttl))),
        ]));
    }
    let mut fields = vec![
        (
            String::from("version"),
            Value::Number(config::SCHEMA_VERSION),
        ),
        (
            String::from("network"),
            Value::Object(vec![
                (String::from("mode"), Value::String(String::from(mode))),
                (String::from("ip"), Value::String(ip_text(ip))),
                (String::from("netmask"), Value::String(ip_text(netmask))),
                (String::from("gateway"), Value::String(ip_text(gateway))),
                (String::from("timeout_ms"), Value::Number(timeout_ms)),
                (
                    String::from("speed"),
                    Value::Number(u64::from(config.network.speed_cap)),
                ),
                (
                    String::from("arp_secs"),
                    Value::Number(config.network.arp_secs),
                ),
            ]),
        ),
        (
            String::from("dns"),
            Value::Object(vec![
                (
                    String::from("cache_size"),
                    Value::Number(config.dns.cache_size as u64),
                ),
                (
                    String::from("cache_reap_interval_secs"),
                    Value::Number(config.dns.cache_reap_secs),
                ),
                (
                    String::from("probe_interval_secs"),
                    Value::Number(config.dns.probe_secs),
                ),
                (String::from("upstreams"), Value::Array(upstreams)),
                (String::from("blocklist"), Value::Array(blocklist)),
                (String::from("records"), Value::Array(records)),
            ]),
        ),
        (
            String::from("http"),
            Value::Object(vec![
                (
                    String::from("port"),
                    Value::Number(u64::from(config.http.port)),
                ),
                (
                    String::from("username"),
                    Value::String(config.http.username.clone()),
                ),
                (
                    String::from("password"),
                    Value::String(config.http.password.clone()),
                ),
            ]),
        ),
        (
            String::from("display"),
            Value::Object(display_fields(config)),
        ),
    ];
    let mut ntp_servers = Vec::new();
    for server in &config.ntp.servers {
        ntp_servers.push(Value::String(server.clone()));
    }
    fields.push((
        String::from("ntp"),
        Value::Object(vec![
            (String::from("servers"), Value::Array(ntp_servers)),
            (
                String::from("interval_secs"),
                Value::Number(config.ntp.interval_secs),
            ),
            (
                String::from("serve"),
                Value::Number(u64::from(config.ntp.serve)),
            ),
        ]),
    ));
    if config.block.enabled
        || !config.block.urls.is_empty()
        || !config.block.allow.is_empty()
        || config.block.refresh_secs != config::BLOCK_REFRESH_DEFAULT
    {
        let mut urls = Vec::new();
        for url in &config.block.urls {
            urls.push(Value::String(url.clone()));
        }
        let mut allow = Vec::new();
        for name in &config.block.allow {
            allow.push(Value::String(name.clone()));
        }
        fields.push((
            String::from("block"),
            Value::Object(vec![
                (
                    String::from("enabled"),
                    Value::Number(u64::from(config.block.enabled)),
                ),
                (
                    String::from("refresh_secs"),
                    Value::Number(config.block.refresh_secs),
                ),
                (String::from("urls"), Value::Array(urls)),
                (String::from("allow"), Value::Array(allow)),
            ]),
        ));
    }
    if !config.board_detect {
        fields.push((
            String::from("board"),
            Value::Object(vec![(String::from("detect"), Value::Number(0))]),
        ));
    }
    Value::Object(fields)
}

fn display_fields(config: &Config) -> Vec<(String, Value)> {
    let mut fields = vec![
        (
            String::from("vnc"),
            Value::Number(u64::from(config.display.vnc)),
        ),
        (
            String::from("vnc_port"),
            Value::Number(u64::from(config.display.vnc_port)),
        ),
        (
            String::from("vnc_password"),
            Value::String(config.display.vnc_password.clone()),
        ),
        (
            String::from("hdmi"),
            Value::Number(u64::from(config.display.hdmi)),
        ),
    ];
    if config.display.hdmi_reset {
        fields.push((String::from("hdmi_reset"), Value::Number(1)));
    }
    fields
}

fn ip_text(ip: [u8; 4]) -> String {
    alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
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
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b':'
        })
}

fn names_eq(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('.');
    let right = right.trim_end_matches('.');
    left.eq_ignore_ascii_case(right)
}
