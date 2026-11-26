//! Sockets and config for the EL0 DNS program.
//!
//! The kernel owns the smoltcp sockets, the virtio device, and the config
//! loaded from storage. EL0 passes buffers in its own window. A pointer
//! outside that window, or a store into EL0 code, returns an error and does
//! not touch the address.
//!
//! x8 is the number. x0-x5 are the arguments. The result is x0.
//!   4  time_ms
//!   5  udp_bind(port) -> fd
//!   6  udp_recv(fd, buf, cap, addr) -> len. addr is 8 bytes: ipv4, port be, pad
//!   7  udp_send(fd, buf, len, ip, port). ip is the address as a big-endian u32
//!   8  tcp_listen(port) -> fd
//!   9  tcp_accept(fd) -> fd
//!  10  tcp_recv(fd, buf, cap, flags) -> len, or 0 when the peer has closed.
//!      flags 1 returns -11 instead of sleeping when the buffer is empty.
//!  11  tcp_send(fd, buf, len) -> len
//!  12  tcp_close(fd)
//!  13  config_records(buf, cap) -> len
//!  14  config_upstreams(buf, cap) -> len
//!  15  config_blocklist(buf, cap) -> len
//!  16  config_cache_size
//!  17  set_cache_entries(n)
//!  18  auth(user, user_len, pass, pass_len) -> 0 if the pair matches
//!  19  config_add(buf, len). buf is "name TYPE value ttl"
//!  20  config_remove(buf, len) -> 0 removed, 1 not found
//!  21  uptime_secs
//!  22  cache_entries
//!  23  fault_guard. Does not return.
//!  24  fault_text. Does not return.
//!  25  http_info(buf, cap) -> "port username"
//!  26  tick_hz
//!  27  debug_spin. Arms the EL0 task that never yields. Returns 0.
//!  28  log_post(ptr, len) -> 0 queued, 1 dropped. Never waits.
//!  29  log_drain(buf, cap) -> len, or 0. Bytes are level, flags, text.
//!  30  log_emit(ptr, len). UART plus the recent-log buffer.
//!  31  log_recent(buf, cap) -> len
//!  32  file_read(name, name_len, buf, cap) -> len
//!  33  file_write(name, name_len, buf, len, flags). flags 1 appends.
//!  55  display_io(op, a, b, c). HDMI mailbox and framebuffer, plus VNC settings.
//!  56  display_set(vnc, port, pass, pass_len, hdmi, err). Queues start or stop.
//!  57  set_dns_top(ptr, len). DNS task publishes the fixed top-N blob.
//!  58  power(action). HTTP task. 0 queries, 1 queues shutdown, 2 queues reboot.
//!  59  vnc_ctrl(op). HTTP task. 0 queries the live process, 1 queues start, 2 queues stop.
//!  60  set_upstream_health(ptr, len). DNS task. count, then count*(ipv4, up).
//!  61  upstream_health(buf, cap) -> len of that blob. Empty until the first publish.
//!  62  config_probe_secs. Upstream health-probe interval, 1..=3600.
//!  63  net_ready. 1 once the interface has an address, else 0.
//!  64  net_counters(buf, cap) -> 40. Five little-endian u64s:
//!      nic_irqs, rx_packets, tx_packets, net_busy_ms, net_idle_ms.
//!  65  set_dns_load(inflight, timeouts, forward_full, reply_drops). DNS task.
//!      Pending count, upstream timeouts, forward-table full events, and
//!      client replies that the UDP queue would not take.
//!  66  net_extra(buf, cap) -> 88. Eleven little-endian u64s:
//!      arp_requests_tx, arp_replies_rx, arp_cache_hits, arp_cache_misses,
//!      dns_inflight, upstream_timeouts, rx_drops, rx_overruns, rx_queued,
//!      forward_full, reply_drops, poll_count, poll_us, poll_max_us,
//!      irq_rearms, tx_full, tx_slow. `dns_inflight` is the forward queue depth.
//!  67  firmware(op, ...). Staged SD updates. Op 10 promotes, op 11 rolls back. See `fw::syscall`.
//!  68  sd_status(buf, cap) -> 64. state[16], error[32], blocks u32, mounted u32.
//!  69  hdmi_console(buf, cap) -> 836. Four little-endian u32s
//!      (state, width, height, recoveries), then 80 bytes of `hdmi_diag`,
//!      then 192 bytes of `hdmi_last_shut`, then 192 bytes of `hdmi_regs`,
//!      then 16 bytes of `hdmi_port`, then 240 bytes of `vclog` (the first
//!      line), then a little-endian u32 `vclog_bytes`, then 96 bytes of
//!      `vclog_status`. The texts are NUL padded. State 0 none, 1 up,
//!      2 fallback, 3 released. Virt and Pi 5 return zeros and `none`.
//!  73  board(ptr, cap) -> 40. revision u32, model[16], source[16].
//!  72  boot(op, kind, ptr, cap). Op 0 arms a test: kind 1 hangs without
//!      resetting, kind 2 marks this boot failed and reboots. Op 1 copies
//!      80 status bytes. HTTP task only for op 0.
//!  70  vclog_read(buf, cap, offset) -> n. The next bytes of the rendered
//!      firmware log (`vclog -m`, then `vclog -a`). `cap` is at most 4096.
//!      0 is the end. Virt and Pi 5 return 0.
//!
//! Each EL0 task has its own fd table. The scheduler selects it before eret.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use alloc::vec;
use alloc::vec::Vec;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use crate::net::service;

const TIME_MS: u64 = 4;
const UDP_BIND: u64 = 5;
const UDP_RECV: u64 = 6;
const UDP_SEND: u64 = 7;
const TCP_LISTEN: u64 = 8;
const TCP_ACCEPT: u64 = 9;
const TCP_RECV: u64 = 10;
const TCP_SEND: u64 = 11;
const TCP_CLOSE: u64 = 12;
const CONFIG_RECORDS: u64 = 13;
const CONFIG_UPSTREAMS: u64 = 14;
const CONFIG_BLOCKLIST: u64 = 15;
const CONFIG_CACHE_SIZE: u64 = 16;
const SET_CACHE_ENTRIES: u64 = 17;
const AUTH: u64 = 18;
const CONFIG_ADD: u64 = 19;
const CONFIG_REMOVE: u64 = 20;
const UPTIME_SECS: u64 = 21;
const CACHE_ENTRIES: u64 = 22;
const FAULT_GUARD: u64 = 23;
const FAULT_TEXT: u64 = 24;
const HTTP_INFO: u64 = 25;
const TICK_HZ: u64 = 26;
const DEBUG_SPIN: u64 = 27;
const LOG_POST: u64 = 28;
const LOG_DRAIN: u64 = 29;
const LOG_EMIT: u64 = 30;
const LOG_RECENT: u64 = 31;
const FILE_READ: u64 = 32;
const FILE_WRITE: u64 = 33;
const LOG_RELOAD: u64 = 45;
const CONFIG_READ: u64 = 46;
const CONFIG_WRITE: u64 = 47;
const LOG_FILL: u64 = 48;
const CONFIG_SET_CACHE: u64 = 49;
const CONFIG_CACHE_REAP: u64 = 50;
const TIMER_TICKS: u64 = 51;
const TIMER_FREQ: u64 = 52;
const SET_CACHE_STATS: u64 = 53;
const CACHE_PLANT: u64 = 54;
const DISPLAY_IO: u64 = 55;
const DISPLAY_SET: u64 = 56;
const SET_DNS_TOP: u64 = 57;
const SET_UPSTREAM_HEALTH: u64 = 60;
const UPSTREAM_HEALTH: u64 = 61;
const CONFIG_PROBE_SECS: u64 = 62;
const NET_READY: u64 = 63;
const NET_COUNTERS: u64 = 64;
const SET_DNS_LOAD: u64 = 65;
const NET_EXTRA: u64 = 66;
const FW: u64 = 67;
const SD_STATUS: u64 = 68;
const HDMI_CONSOLE: u64 = 69;
const VCLOG_READ: u64 = 70;
const CRASH: u64 = 71;
const BOOT: u64 = 72;
const BOARD: u64 = 73;
const CSRF: u64 = 74;
const NET_ARM: u64 = 75;
/// 76 clock(buf, cap) -> 100. synced, stratum, offset_ms, utc, last sync, server.
/// 77 clock_prefix(buf, cap) -> n. Empty until time is synced.
/// 78 heap(buf, cap) -> 24. used, peak, size, each a little-endian u64.
const CLOCK: u64 = 76;
const CLOCK_PREFIX: u64 = 77;
const HEAP: u64 = 78;
/// 79 tcp_connect(ip_be, port) -> fd. The handshake waits in the socket job.
const TCP_CONNECT: u64 = 79;
/// 80 block(op, ...). Lookups fail open and do not wait on the blocker.
const BLOCK: u64 = 80;
/// 81 log_file(op, ...). Read-only `LOG.TXT` and `LOG1.TXT`..`LOG16.TXT`.
/// Op 0 lists `name len` lines. Op 1 reads at an offset. Op 2 returns the length.
const LOG_FILE: u64 = 81;
/// 82 http_hold(op). HTTP task only. Op 1 holds a finished close until the
/// idle budget, op 0 releases that hold, op 2 reads it. A test uses this to
/// pin `Kind::Closing` descriptors the way a peer that never ACKs would.
const HTTP_HOLD: u64 = 82;

const EFAULT: i64 = -2;
const EBADF: i64 = -3;
const EINVAL: i64 = -4;
const EIO: i64 = -5;
const EMFILE: i64 = -6;
const EMSGSIZE: i64 = -7;
const EAGAIN: i64 = -11;

const MAX_FD: usize = 24;
const MAX_PACKET: usize = 4096;
/// Listeners allocated once. A socket leaves LISTEN on the SYN. A peer that
/// closes without sending stays out of `may_recv`, so the slot is put back
/// into LISTEN on the next poll instead of waiting for the application.
const LISTENERS: usize = service::TCP_LISTENERS;
/// Hardware-timer budget for an idle HTTP recv or send, a listen slot that
/// has left LISTEN, and a `Kind::Closing` socket whose peer never ACKs.
const TCP_IDLE_MS: u64 = 5_000;

#[derive(Clone, Copy)]
enum Kind {
    Empty,
    Reserved,
    Udp(SocketHandle),
    TcpListen([Option<SocketHandle>; LISTENERS], u16),
    TcpConn(SocketHandle),
    Closing(SocketHandle),
}

const TASKS: usize = 8;

static mut FDS: [[Kind; MAX_FD]; TASKS] = [[Kind::Empty; MAX_FD]; TASKS];
static mut TASK_SLOT: [usize; 4] = [0; 4];
static mut GEN: [[u32; MAX_FD]; TASKS] = [[0; MAX_FD]; TASKS];
static FD_LOCK: [crate::spin::Spin; TASKS] = [const { crate::spin::Spin::new() }; TASKS];

pub fn set_task(slot: usize) {
    if slot < TASKS {
        unsafe { TASK_SLOT[crate::percpu::id()] = slot };
    }
}

fn slot() -> usize {
    unsafe { TASK_SLOT[crate::percpu::id()] }
}

fn table() -> &'static mut [Kind; MAX_FD] {
    unsafe { &mut FDS[slot()] }
}

pub fn call(num: u64, args: [u64; 6]) -> u64 {
    let _fd = FD_LOCK[slot()].lock();
    let value = match num {
        TIME_MS => crate::timer::millis() as i64,
        UDP_BIND => udp_bind(args[0]),
        UDP_RECV => udp_recv(args[0], args[1], args[2], args[3], args[4]),
        UDP_SEND => udp_send(args[0], args[1], args[2], args[3], args[4]),
        TCP_LISTEN => tcp_listen(args[0]),
        TCP_ACCEPT => tcp_accept(args[0]),
        TCP_RECV => tcp_recv(args[0], args[1], args[2], args[3]),
        TCP_SEND => tcp_send(args[0], args[1], args[2]),
        TCP_CLOSE => tcp_close(args[0]),
        CONFIG_RECORDS => config_out(args[0], args[1], crate::config_manager::write_records),
        CONFIG_UPSTREAMS => config_out(args[0], args[1], crate::config_manager::write_upstreams),
        CONFIG_BLOCKLIST => config_out(args[0], args[1], crate::config_manager::write_blocklist),
        CONFIG_CACHE_SIZE => crate::config_manager::cache_size() as i64,
        SET_CACHE_ENTRIES => {
            crate::dns::set_cache_entries(args[0] as usize);
            0
        }
        AUTH => auth(args[0], args[1], args[2], args[3]),
        CONFIG_ADD => config_add(args[0], args[1]),
        CONFIG_REMOVE => config_remove(args[0], args[1]),
        UPTIME_SECS => crate::timer::uptime_secs() as i64,
        CACHE_ENTRIES => crate::dns::cache_entries() as i64,
        FAULT_GUARD => crate::exception::fault_guard(),
        FAULT_TEXT => crate::exception::fault_text(),
        HTTP_INFO => config_out(args[0], args[1], crate::config_manager::write_http_info),
        TICK_HZ => crate::timer::TICK_HZ as i64,
        DEBUG_SPIN => crate::task::arm_spinner(),
        LOG_POST => log_post(args[0], args[1]),
        LOG_DRAIN => log_drain(args[0], args[1]),
        LOG_EMIT => log_emit(args[0], args[1]),
        LOG_RECENT => log_recent(args[0], args[1]),
        FILE_READ => file_read(args[0], args[1], args[2], args[3]),
        FILE_WRITE => file_write(args[0], args[1], args[2], args[3], args[4]),
        LOG_RELOAD => match crate::config_manager::reload_log() {
            Ok(()) => 0,
            Err(_) => EIO,
        },
        CONFIG_READ => config_read(args[0], args[1], args[2]),
        CONFIG_WRITE => config_write(args[0], args[1], args[2], args[3], args[4]),
        LOG_FILL => {
            crate::config_manager::fill_log(args[0] as u32);
            0
        }
        CONFIG_SET_CACHE => config_set_cache(args[0], args[1], args[2], args[3]),
        CONFIG_CACHE_REAP => crate::config_manager::cache_reap_secs() as i64,
        TIMER_TICKS => crate::timer::get_ticks() as i64,
        TIMER_FREQ => crate::timer::frequency() as i64,
        SET_CACHE_STATS => set_cache_stats(args[0]),
        CACHE_PLANT => cache_plant(args[0], args[1]),
        DISPLAY_IO => display_io(args[0], args[1], args[2], args[3]),
        DISPLAY_SET => display_set(args[0], args[1], args[2], args[3], args[4], args[5]),
        SET_DNS_TOP => set_dns_top(args[0], args[1]),
        SET_UPSTREAM_HEALTH => set_upstream_health(args[0], args[1]),
        UPSTREAM_HEALTH => upstream_health(args[0], args[1]),
        CONFIG_PROBE_SECS => crate::config_manager::probe_interval_secs() as i64,
        NET_READY => i64::from(crate::net::has_address()),
        NET_COUNTERS => net_counters(args[0], args[1]),
        SET_DNS_LOAD => set_dns_load(args[0], args[1], args[2], args[3]),
        NET_EXTRA => net_extra(args[0], args[1]),
        FW => crate::fw::syscall(args),
        SD_STATUS => sd_status(args[0], args[1]),
        HDMI_CONSOLE => hdmi_console(args[0], args[1]),
        VCLOG_READ => vclog_read(args[0], args[1], args[2]),
        CRASH => crash_call(args),
        BOOT => boot_call(args),
        BOARD => crash_copy(args[0], args[1], crate::board_id::status),
        CSRF => csrf_token(args[0], args[1]),
        NET_ARM => i64::from(crate::config_manager::arm_net_pending()),
        CLOCK => clock_status(args[0], args[1]),
        CLOCK_PREFIX => clock_prefix(args[0], args[1]),
        HEAP => heap_stats(args[0], args[1]),
        TCP_CONNECT => tcp_connect(args[0], args[1]),
        BLOCK => crate::block_tab::call(args[0], [args[1], args[2], args[3], args[4], args[5]]),
        LOG_FILE => log_file(args),
        HTTP_HOLD => http_hold(args[0]),
        _ => -1,
    };
    value as u64
}

fn clock_status(ptr: u64, cap: u64) -> i64 {
    if cap < crate::clock::STATUS_LEN as u64 || !crate::mmu::user_write_ok(ptr, crate::clock::STATUS_LEN as u64)
    {
        return EFAULT;
    }
    let mut raw = [0u8; crate::clock::STATUS_LEN];
    let n = crate::clock::status(&mut raw);
    if n == raw.len() && write_user(ptr, &raw) {
        n as i64
    } else {
        EFAULT
    }
}

fn heap_stats(ptr: u64, cap: u64) -> i64 {
    const N: u64 = 24;
    if cap < N || !crate::mmu::user_write_ok(ptr, N) {
        return EFAULT;
    }
    let (used, peak, size) = crate::memory::ALLOCATOR.stats();
    let mut raw = [0u8; 24];
    raw[..8].copy_from_slice(&(used as u64).to_le_bytes());
    raw[8..16].copy_from_slice(&(peak as u64).to_le_bytes());
    raw[16..24].copy_from_slice(&(size as u64).to_le_bytes());
    if write_user(ptr, &raw) {
        N as i64
    } else {
        EFAULT
    }
}

fn clock_prefix(ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 64 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut raw = [0u8; 64];
    let n = crate::clock::prefix(&mut raw[..cap as usize]);
    if write_user(ptr, &raw[..n]) {
        n as i64
    } else {
        EFAULT
    }
}

fn config_out(ptr: u64, cap: u64, write: fn(&mut [u8]) -> Option<usize>) -> i64 {
    if cap == 0 || cap > 4096 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut buf = [0u8; 4096];
    let cap = cap as usize;
    match write(&mut buf[..cap]) {
        Some(n) => {
            if write_user(ptr, &buf[..n]) {
                n as i64
            } else {
                EFAULT
            }
        }
        None => EMSGSIZE,
    }
}

fn udp_bind(port: u64) -> i64 {
    let Some(port) = port_ok(port) else {
        return EINVAL;
    };
    let Some(fd) = find_free() else {
        return EMFILE;
    };
    let task = slot();
    set_kind(fd, Kind::Reserved);
    bump_gen(task, fd);
    if service::submit(service::OP_UDP_BIND, |body| {
        body.fd = fd as u8;
        body.port = port;
        body.gen = gen_at(task, fd);
    })
    .is_none()
    {
        set_kind(fd, Kind::Empty);
        return EAGAIN;
    }
    0
}

fn udp_recv(fd: u64, buf: u64, cap: u64, addr: u64, try_once: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    if cap == 0 || cap > MAX_PACKET as u64 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    if !crate::mmu::user_write_ok(addr, 8) {
        return EFAULT;
    }
    let Kind::Udp(handle) = kind_at(fd) else {
        return EBADF;
    };
    let task = slot();
    if service::submit(service::OP_UDP_RECV, |body| {
        body.fd = fd as u8;
        body.gen = gen_at(task, fd);
        body.handle_raw = service::handle_raw(handle);
        body.cap = cap as u32;
        body.user_ptr = buf;
        body.user_aux = addr;
        // port is unused by recv. 1 means return EAGAIN instead of sleeping
        // when the socket is empty, so a worker can drain a burst.
        body.port = u16::from(try_once != 0);
    })
    .is_none()
    {
        return EAGAIN;
    }
    0
}

fn udp_send(fd: u64, buf: u64, len: u64, ip: u64, port: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let Some(port) = port_ok(port) else {
        return EINVAL;
    };
    if len == 0 || len > MAX_PACKET as u64 || !crate::mmu::user_slice_ok(buf, len) {
        return EFAULT;
    }
    let Kind::Udp(handle) = kind_at(fd) else {
        return EBADF;
    };
    let task = slot();
    let posted = service::submit(service::OP_UDP_SEND, |body| {
        body.fd = fd as u8;
        body.gen = gen_at(task, fd);
        body.handle_raw = service::handle_raw(handle);
        body.ip = ip as u32;
        body.port = port;
        body.len = len as u32;
        let n = len as usize;
        crate::mmu::set_pan(false);
        unsafe {
            core::ptr::copy_nonoverlapping(buf as *const u8, body.buf.as_mut_ptr(), n);
        }
        crate::mmu::set_pan(true);
    });
    if posted.is_none() {
        return EAGAIN;
    }
    0
}

fn tcp_connect(ip_be: u64, port: u64) -> i64 {
    let Some(port) = port_ok(port) else {
        return EINVAL;
    };
    let ip = (ip_be as u32).to_be_bytes();
    if ip == [0, 0, 0, 0] {
        return EINVAL;
    }
    let Some(fd) = find_free() else {
        return EMFILE;
    };
    let task = slot();
    set_kind(fd, Kind::Reserved);
    bump_gen(task, fd);
    if service::submit(service::OP_TCP_CONNECT, |body| {
        body.fd = fd as u8;
        body.port = port;
        body.ip = u32::from_be_bytes(ip);
        body.gen = gen_at(task, fd);
        body.handle_raw = 0;
        body.conn_fd = 0;
        body.user_aux = crate::timer::millis();
    })
    .is_none()
    {
        set_kind(fd, Kind::Empty);
        return EAGAIN;
    }
    0
}

fn tcp_connect_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let gen = body.gen;
    let ip = body.ip.to_be_bytes();
    let port = body.port;
    let started = body.user_aux;
    if !live(task, fd, gen) {
        return service::Step::Done(EBADF);
    }
    if crate::timer::millis().saturating_sub(started) > 10_000 && body.conn_fd != 0 {
        let handle = service::handle_of(body.handle_raw);
        give_tcp(stack, handle);
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::Empty;
        return service::Step::Done(EIO);
    }
    if body.conn_fd == 0 {
        let handle = take_tcp(stack);
        let local = next_local_port();
        if stack.connect_tcp(handle, ip, port, local).is_err() {
            give_tcp(stack, handle);
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
            return service::Step::Done(EIO);
        }
        service::body(id).handle_raw = service::handle_raw(handle);
        service::body(id).conn_fd = 1;
        return service::Step::Pending;
    }
    let handle = service::handle_of(service::body(id).handle_raw);
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if socket.can_send() || socket.state() == tcp::State::Established {
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::TcpConn(handle);
        return service::Step::Done(fd as i64);
    }
    if matches!(
        socket.state(),
        tcp::State::SynSent | tcp::State::SynReceived
    ) {
        return service::Step::Pending;
    }
    give_tcp(stack, handle);
    let _lock = FD_LOCK[task].lock();
    table_of(task)[fd] = Kind::Empty;
    service::Step::Done(EIO)
}

fn next_local_port() -> u16 {
    use core::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(40000);
    let port = PORT.fetch_add(1, Ordering::Relaxed);
    if port < 40000 {
        40000
    } else {
        port
    }
}

fn tcp_listen(port: u64) -> i64 {
    let Some(port) = port_ok(port) else {
        return EINVAL;
    };
    let Some(fd) = find_free() else {
        return EMFILE;
    };
    let task = slot();
    set_kind(fd, Kind::Reserved);
    bump_gen(task, fd);
    if service::submit(service::OP_TCP_LISTEN, |body| {
        body.fd = fd as u8;
        body.port = port;
        body.gen = gen_at(task, fd);
    })
    .is_none()
    {
        set_kind(fd, Kind::Empty);
        return EAGAIN;
    }
    0
}

fn tcp_accept(fd: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let port = match kind_at(fd) {
        Kind::TcpListen(_, port) => port,
        _ => return EBADF,
    };
    let Some(conn) = find_free() else {
        if port == 80 {
            note_http_emfile();
        }
        return EMFILE;
    };
    if port == 80 {
        HTTP_EMFILE_SINCE.store(0, Ordering::Release);
    }
    let task = slot();
    set_kind(conn, Kind::Reserved);
    bump_gen(task, conn);
    if service::submit(service::OP_TCP_ACCEPT, |body| {
        body.fd = fd as u8;
        body.conn_fd = conn as u8;
        body.gen = gen_at(task, fd);
    })
    .is_none()
    {
        set_kind(conn, Kind::Empty);
        if port == 80 {
            warn_http(&HTTP_ERR_LOG, "HTTP accept failed: busy");
        }
        return EAGAIN;
    }
    0
}

fn tcp_recv(fd: u64, buf: u64, cap: u64, flags: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    if cap == 0 || cap > MAX_PACKET as u64 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    let Kind::TcpConn(handle) = kind_at(fd) else {
        return EBADF;
    };
    let task = slot();
    if service::submit(service::OP_TCP_RECV, |body| {
        body.fd = fd as u8;
        body.gen = gen_at(task, fd);
        body.handle_raw = service::handle_raw(handle);
        body.cap = cap as u32;
        body.user_ptr = buf;
        body.port = if flags == 1 { 1 } else { 0 };
    })
    .is_none()
    {
        return EAGAIN;
    }
    0
}

fn tcp_send(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    if len == 0 || len > MAX_PACKET as u64 || !crate::mmu::user_slice_ok(buf, len) {
        return EFAULT;
    }
    let Kind::TcpConn(handle) = kind_at(fd) else {
        return EBADF;
    };
    let task = slot();
    let posted = service::submit(service::OP_TCP_SEND, |body| {
        body.fd = fd as u8;
        body.gen = gen_at(task, fd);
        body.handle_raw = service::handle_raw(handle);
        body.len = len as u32;
        let n = len as usize;
        crate::mmu::set_pan(false);
        unsafe {
            core::ptr::copy_nonoverlapping(buf as *const u8, body.buf.as_mut_ptr(), n);
        }
        crate::mmu::set_pan(true);
    });
    if posted.is_none() {
        return EAGAIN;
    }
    0
}

fn tcp_close(fd: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let task = slot();
    let kind = kind_at(fd);
    set_kind(fd, Kind::Reserved);
    bump_gen(task, fd);
    let posted = match kind {
        Kind::TcpConn(handle) => service::submit(service::OP_TCP_CLOSE, |body| {
            body.fd = fd as u8;
            body.cap = 1;
            body.handle_raw = service::handle_raw(handle);
        }),
        Kind::TcpListen(slots, _) => service::submit(service::OP_TCP_CLOSE, |body| {
            body.fd = fd as u8;
            body.cap = 2;
            body.listen = slots;
        }),
        Kind::Udp(handle) => service::submit(service::OP_TCP_CLOSE, |body| {
            body.fd = fd as u8;
            body.cap = 3;
            body.handle_raw = service::handle_raw(handle);
        }),
        Kind::Closing(handle) => service::submit(service::OP_TCP_CLOSE, |body| {
            body.fd = fd as u8;
            body.cap = 4;
            body.handle_raw = service::handle_raw(handle);
        }),
        _ => None,
    };
    if posted.is_none() {
        set_kind(fd, kind);
        return if matches!(kind, Kind::Empty | Kind::Reserved) {
            EBADF
        } else {
            EAGAIN
        };
    }
    0
}

fn gen_at(task: usize, fd: usize) -> u32 {
    unsafe { GEN[task][fd] }
}

fn bump_gen(task: usize, fd: usize) {
    unsafe {
        GEN[task][fd] = GEN[task][fd].wrapping_add(1);
    }
}

fn table_of(task: usize) -> &'static mut [Kind; MAX_FD] {
    unsafe { &mut FDS[task] }
}

fn live(task: usize, fd: usize, gen: u32) -> bool {
    let _lock = FD_LOCK[task].lock();
    unsafe { GEN[task][fd] == gen }
}

pub fn perform(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let op = service::body(id).op;
    if op == service::OP_CANCEL {
        release_conn(id);
        return service::Step::Done(EBADF);
    }
    match op {
        service::OP_UDP_BIND => udp_bind_job(id, stack),
        service::OP_UDP_RECV => udp_recv_job(id, stack),
        service::OP_UDP_SEND => udp_send_job(id, stack),
        service::OP_TCP_LISTEN => tcp_listen_job(id, stack),
        service::OP_TCP_ACCEPT => tcp_accept_job(id, stack),
        service::OP_TCP_RECV => tcp_recv_job(id, stack),
        service::OP_TCP_SEND => tcp_send_job(id, stack),
        service::OP_TCP_CLOSE => tcp_close_job(id, stack),
        service::OP_TCP_CONNECT => tcp_connect_job(id, stack),
        _ => service::Step::Done(EBADF),
    }
}

fn release_conn(id: usize) {
    let body = service::body(id);
    if body.conn_fd == 0xff {
        return;
    }
    let task = body.task as usize;
    let conn = body.conn_fd as usize;
    let _lock = FD_LOCK[task].lock();
    if matches!(table_of(task)[conn], Kind::Reserved) {
        table_of(task)[conn] = Kind::Empty;
    }
}

fn udp_bind_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let port = body.port;
    let handle = take_udp(stack);
    let bound = stack
        .sockets_mut()
        .get_mut::<udp::Socket>(handle)
        .bind(port)
        .is_ok();
    if !bound {
        give_udp(stack, handle);
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::Empty;
        return service::Step::Done(EIO);
    }
    {
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::Udp(handle);
    }
    if port == 53 {
        DNS_FD.store(fd as u8, Ordering::Release);
        DNS_DOWN.store(0, Ordering::Release);
        log::info!("DNS socket bound to port 53");
    } else if port == 53535 {
        log::info!("DNS upstream socket bound to port 53535");
    }
    service::Step::Done(fd as i64)
}

fn udp_recv_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let gen = body.gen;
    let handle = service::handle_of(body.handle_raw);
    if !live(task, fd, gen) {
        return service::Step::Done(EBADF);
    }
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    if !socket.can_recv() {
        if service::body(id).port != 0 {
            return service::Step::Done(EAGAIN);
        }
        return service::Step::Pending;
    }
    match socket.recv() {
        Ok((data, meta)) => {
            let cap = service::body(id).cap as usize;
            let n = data.len().min(cap);
            service::body(id).buf[..n].copy_from_slice(&data[..n]);
            service::body(id).copy_len = n as u32;
            let ip = match meta.endpoint.addr {
                IpAddress::Ipv4(ip) => {
                    let bytes = ip.as_bytes();
                    [bytes[0], bytes[1], bytes[2], bytes[3]]
                }
                _ => [0, 0, 0, 0],
            };
            let port = meta.endpoint.port.to_be_bytes();
            service::body(id).addr = [ip[0], ip[1], ip[2], ip[3], port[0], port[1], 0, 0];
            service::body(id).aux_len = 8;
            service::Step::Done(n as i64)
        }
        Err(_) => {
            if service::body(id).port != 0 {
                service::Step::Done(EAGAIN)
            } else {
                service::Step::Pending
            }
        }
    }
}

fn udp_send_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let gen = body.gen;
    let handle = service::handle_of(body.handle_raw);
    let port = body.port;
    let ip = body.ip;
    let len = body.len as usize;
    if !live(task, fd, gen) {
        return service::Step::Done(EBADF);
    }
    let bytes = (ip as u32).to_be_bytes();
    let endpoint = IpEndpoint::new(
        Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3]).into(),
        port,
    );
    let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
    match socket.send_slice(&service::body(id).buf[..len], endpoint) {
        Ok(_) => service::Step::Done(0),
        // A full queue used to sleep the caller forever. The DNS task holds
        // its state lock across this syscall, so a neighbor that never
        // resolves also stopped failover and hid the timeout. The caller
        // retries on its own clock.
        Err(udp::SendError::BufferFull) => service::Step::Done(EAGAIN),
        Err(_) => service::Step::Done(EIO),
    }
}

fn tcp_listen_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let port = body.port;
    let mut slots = [None; LISTENERS];
    for slot in slots.iter_mut() {
        *slot = add_listener(stack, port);
    }
    if slots.iter().all(|slot| slot.is_none()) {
        if port == 80 {
            log::warn!("HTTP listen failed on port 80");
            crate::logger::note_fault("http listen failed");
        }
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::Empty;
        return service::Step::Done(EIO);
    }
    {
        let _lock = FD_LOCK[task].lock();
        table_of(task)[fd] = Kind::TcpListen(slots, port);
    }
    if port == 80 {
        log::info!("HTTP socket listening on port 80");
    }
    service::Step::Done(fd as i64)
}

fn tcp_accept_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let conn = body.conn_fd as usize;
    let gen = body.gen;
    if !live(task, fd, gen) {
        release_conn(id);
        return service::Step::Done(EBADF);
    }
    let slots = {
        let _lock = FD_LOCK[task].lock();
        match table_of(task)[fd] {
            Kind::TcpListen(slots, _) => slots,
            _ => {
                drop(_lock);
                release_conn(id);
                return service::Step::Done(EBADF);
            }
        }
    };
    let ready = slots.iter().position(|slot| {
        let Some(handle) = slot else {
            return false;
        };
        stack.sockets_mut().get::<tcp::Socket>(*handle).may_recv()
    });
    let Some(index) = ready else {
        return service::Step::Pending;
    };
    let Some(handle) = slots[index] else {
        return service::Step::Pending;
    };
    let _lock = FD_LOCK[task].lock();
    let Kind::TcpListen(mut live_slots, port) = table_of(task)[fd] else {
        drop(_lock);
        release_conn(id);
        return service::Step::Done(EBADF);
    };
    if live_slots[index] != Some(handle) {
        return service::Step::Pending;
    }
    // A heal between the two locks may have put this socket back into
    // LISTEN. Taking it then would hand the application a listener.
    if !stack
        .sockets_mut()
        .get::<tcp::Socket>(handle)
        .may_recv()
    {
        return service::Step::Pending;
    }
    live_slots[index] = None;
    table_of(task)[fd] = Kind::TcpListen(live_slots, port);
    table_of(task)[conn] = Kind::TcpConn(handle);
    service::Step::Done(conn as i64)
}

fn tcp_recv_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let gen = body.gen;
    let handle = service::handle_of(body.handle_raw);
    let cap = body.cap as usize;
    let nonblock = body.port != 0;
    if !live(task, fd, gen) {
        return service::Step::Done(EBADF);
    }
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if !socket.can_recv() {
        let open = socket.may_recv()
            || matches!(
                socket.state(),
                tcp::State::SynSent | tcp::State::SynReceived
            );
        let http = socket.local_endpoint().is_some_and(|ep| ep.port == 80);
        if open {
            if nonblock {
                return service::Step::Done(EAGAIN);
            }
            if http {
                let now = crate::timer::millis();
                let started = service::body(id).user_aux;
                if started == 0 {
                    service::body(id).user_aux = now.max(1);
                    return service::Step::Pending;
                }
                if now.saturating_sub(started) >= TCP_IDLE_MS {
                    warn_http(&HTTP_ERR_LOG, "HTTP connection idle; closed");
                    return service::Step::Done(0);
                }
            }
            return service::Step::Pending;
        }
        return service::Step::Done(0);
    }
    match socket.recv_slice(&mut service::body(id).buf[..cap]) {
        Ok(copied) => {
            service::body(id).copy_len = copied as u32;
            service::Step::Done(copied as i64)
        }
        Err(tcp::RecvError::Finished) => service::Step::Done(0),
        Err(_) => {
            if nonblock {
                service::Step::Done(EAGAIN)
            } else {
                service::Step::Pending
            }
        }
    }
}

fn tcp_send_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let gen = body.gen;
    let handle = service::handle_of(body.handle_raw);
    let len = body.len as usize;
    if !live(task, fd, gen) {
        return service::Step::Done(EBADF);
    }
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if !socket.can_send() {
        let open = matches!(
            socket.state(),
            tcp::State::Established | tcp::State::CloseWait | tcp::State::SynReceived | tcp::State::SynSent
        );
        let http = socket.local_endpoint().is_some_and(|ep| ep.port == 80);
        if open {
            if http {
                let now = crate::timer::millis();
                let started = service::body(id).user_aux;
                if started == 0 {
                    service::body(id).user_aux = now.max(1);
                    return service::Step::Pending;
                }
                if now.saturating_sub(started) >= TCP_IDLE_MS {
                    warn_http(&HTTP_ERR_LOG, "HTTP connection send timed out");
                    return service::Step::Done(EIO);
                }
            }
            return service::Step::Pending;
        }
        return service::Step::Done(EIO);
    }
    match socket.send_slice(&service::body(id).buf[..len]) {
        Ok(n) => service::Step::Done(n as i64),
        Err(_) => service::Step::Pending,
    }
}

fn tcp_close_job(id: usize, stack: &mut crate::net::NetworkStack) -> service::Step {
    let body = service::body(id);
    let task = body.task as usize;
    let fd = body.fd as usize;
    let mode = body.cap;
    let handle = service::handle_of(body.handle_raw);
    let slots = body.listen;
    service::cancel_fd(task, fd, id);
    match mode {
        1 => {
            stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Closing(handle);
            unsafe { CLOSE_SINCE[task][fd] = crate::timer::millis().max(1) };
        }
        2 => {
            for handle in slots.into_iter().flatten() {
                stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
            }
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
        3 => {
            give_udp(stack, handle);
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
        4 => {
            // A second close of a client socket that never had a listener.
            give_tcp(stack, handle);
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
        5 => {
            give_tcp(stack, handle);
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
        6 => {
            for handle in slots.into_iter().flatten() {
                give_tcp(stack, handle);
            }
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
        _ => {
            stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
            let _lock = FD_LOCK[task].lock();
            table_of(task)[fd] = Kind::Empty;
        }
    }
    service::Step::Done(0)
}

/// Close every fd of a dead process. Returns when the table is empty and no
/// job for it is still queued. A full job ring leaves the fd in place so the
/// caller can try again. Does not drive the stack and does not take `SCHED`.
pub fn release_fds(task: usize) -> bool {
    if task >= TASKS {
        return true;
    }
    service::cancel_task(task);
    close_all(task);
    idle(task)
}

pub fn idle(task: usize) -> bool {
    if task >= TASKS {
        return true;
    }
    if service::task_queued(task) {
        return false;
    }
    let _lock = FD_LOCK[task].lock();
    table_of(task).iter().all(|kind| matches!(kind, Kind::Empty))
}

pub fn open_fds() -> u32 {
    let mut n = 0u32;
    let mut task = 0;
    while task < TASKS {
        let _lock = FD_LOCK[task].lock();
        for kind in table_of(task).iter() {
            if !matches!(kind, Kind::Empty) {
                n += 1;
            }
        }
        task += 1;
    }
    n
}

fn close_all(task: usize) {
    let _lock = FD_LOCK[task].lock();
    let mut fd = 0;
    while fd < MAX_FD {
        let kind = table_of(task)[fd];
        let posted = match kind {
            Kind::Empty | Kind::Reserved => None,
            Kind::TcpConn(handle) | Kind::Closing(handle) => {
                table_of(task)[fd] = Kind::Reserved;
                bump_gen(task, fd);
                service::submit(service::OP_TCP_CLOSE, |body| {
                    body.task = task as u8;
                    body.fd = fd as u8;
                    body.cap = 5;
                    body.handle_raw = service::handle_raw(handle);
                })
            }
            Kind::TcpListen(slots, _) => {
                table_of(task)[fd] = Kind::Reserved;
                bump_gen(task, fd);
                service::submit(service::OP_TCP_CLOSE, |body| {
                    body.task = task as u8;
                    body.fd = fd as u8;
                    body.cap = 6;
                    body.listen = slots;
                })
            }
            Kind::Udp(handle) => {
                table_of(task)[fd] = Kind::Reserved;
                bump_gen(task, fd);
                service::submit(service::OP_TCP_CLOSE, |body| {
                    body.task = task as u8;
                    body.fd = fd as u8;
                    body.cap = 3;
                    body.handle_raw = service::handle_raw(handle);
                })
            }
        };
        if posted.is_none() && !matches!(kind, Kind::Empty | Kind::Reserved) {
            table_of(task)[fd] = kind;
        }
        fd += 1;
    }
}

pub fn recycle_all(stack: &mut crate::net::NetworkStack) {
    for task in 0..TASKS {
        let _lock = FD_LOCK[task].lock();
        recycle_task(task, stack);
    }
    trim_tcp_pool(stack);
}

fn recycle_task(task: usize, stack: &mut crate::net::NetworkStack) {
    let table = table_of(task);
    watch_dns_socket(task, table);
    let Some(listen_fd) = (0..MAX_FD).find(|index| matches!(table[*index], Kind::TcpListen(_, _)))
    else {
        // A client task (the blocker) has no listener to absorb a closed
        // connection. Park the finished socket or its buffers stay allocated.
        reap_clients(task, table, stack);
        return;
    };
    let Kind::TcpListen(mut slots, port) = table[listen_fd] else {
        return;
    };
    for index in 0..MAX_FD {
        let Kind::Closing(handle) = table[index] else {
            continue;
        };
        let now = crate::timer::millis();
        let (queued, connected) = {
            let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
            let queued = socket.send_queue() > 0;
            let connected = matches!(
                socket.state(),
                tcp::State::Established
                    | tcp::State::CloseWait
                    | tcp::State::SynReceived
                    | tcp::State::SynSent
            );
            (queued, connected)
        };
        let since = unsafe { CLOSE_SINCE[task][index] };
        let age = if since == 0 {
            unsafe { CLOSE_SINCE[task][index] = now.max(1) };
            0
        } else {
            now.saturating_sub(since)
        };
        let hold = port == 80 && HTTP_HOLD_CLOSE.load(Ordering::Relaxed);
        if !close_reap(queued, connected, age, hold) {
            continue;
        }
        let forced = age >= TCP_IDLE_MS;
        if slots.iter().all(|slot| slot.is_some()) {
            // The listen set is already full. Park the socket so the fd
            // and its buffers do not stay reserved.
            give_tcp(stack, handle);
            table[index] = Kind::Empty;
            note_close_reaped(task, index, port, forced);
            continue;
        }
        let listened = {
            let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
            if socket.is_open() {
                socket.abort();
            }
            socket.listen(port).is_ok()
        };
        if !listened {
            give_tcp(stack, handle);
            table[index] = Kind::Empty;
            note_close_reaped(task, index, port, forced);
            continue;
        }
        if let Some(slot) = slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(handle);
            table[index] = Kind::Empty;
            note_close_reaped(task, index, port, forced);
        }
    }
    heal_listen(task, &mut slots, port, stack);
    table[listen_fd] = Kind::TcpListen(slots, port);
}

enum SlotFate {
    Listening,
    Hold(u64),
    Relisten,
}

/// First hardware-timer ms at which port 80 had no socket in LISTEN.
/// Zero means the listener is up. The supervisor reads this without `FD`.
static HTTP_LISTEN_DOWN: AtomicU64 = AtomicU64::new(0);
/// First hardware-timer ms at which port 80's accept returned EMFILE.
/// Cleared when a descriptor is freed or an accept is queued. Zero means
/// the table is not stuck. The supervisor reads this without `FD`.
static HTTP_EMFILE_SINCE: AtomicU64 = AtomicU64::new(0);
static HTTP_ERR_LOG: AtomicU64 = AtomicU64::new(0);
static HTTP_CLOSE_LOG: AtomicU64 = AtomicU64::new(0);
static HTTP_HEAL_LOG: AtomicU64 = AtomicU64::new(0);
static HTTP_HOLD_CLOSE: AtomicBool = AtomicBool::new(false);
static mut LISTEN_BORN: [[u64; LISTENERS]; TASKS] = [[0; LISTENERS]; TASKS];
static mut CLOSE_SINCE: [[u64; MAX_FD]; TASKS] = [[0; MAX_FD]; TASKS];

/// Port-53 descriptor, or `0xFF` until the first successful bind.
/// `DNS_DOWN` is the first ms that descriptor was no longer a UDP socket.
/// Zero means it is up, or it has never been bound. The supervisor reads
/// both without `FD`.
pub const DNS_GONE_MS: u64 = 5_000;
static DNS_FD: AtomicU8 = AtomicU8::new(0xFF);
static DNS_DOWN: AtomicU64 = AtomicU64::new(0);

pub fn dns_down_ms() -> u64 {
    DNS_DOWN.load(Ordering::Acquire)
}

pub fn clear_dns_down() {
    DNS_DOWN.store(0, Ordering::Release);
}

fn watch_dns_socket(task: usize, table: &[Kind; MAX_FD]) {
    if task != 0 {
        return;
    }
    let fd = DNS_FD.load(Ordering::Acquire);
    let present = fd != 0xFF && (fd as usize) < MAX_FD && matches!(table[fd as usize], Kind::Udp(_));
    if present || fd == 0xFF {
        if present {
            DNS_DOWN.store(0, Ordering::Release);
        }
        return;
    }
    let now = crate::timer::millis().max(1);
    let _ = DNS_DOWN.compare_exchange(0, now, Ordering::AcqRel, Ordering::Relaxed);
}

/// Bound once, then missing, for the whole budget. A task that has not
/// bound yet does not start the clock.
#[cfg(test)]
fn dns_gone(bound: bool, present: bool, age_ms: u64) -> bool {
    bound && !present && age_ms >= DNS_GONE_MS
}

pub fn http_listen_down_ms() -> u64 {
    HTTP_LISTEN_DOWN.load(Ordering::Acquire)
}

pub fn clear_http_listen_down() {
    HTTP_LISTEN_DOWN.store(0, Ordering::Release);
}

pub fn http_emfile_ms() -> u64 {
    HTTP_EMFILE_SINCE.load(Ordering::Acquire)
}

pub fn clear_http_emfile() {
    HTTP_EMFILE_SINCE.store(0, Ordering::Release);
}

fn warn_http(last: &AtomicU64, text: &str) {
    let now = crate::timer::millis();
    let prev = last.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 1000 {
        return;
    }
    last.store(now, Ordering::Relaxed);
    log::warn!("{text}");
}

fn slot_fate(stack: &mut crate::net::NetworkStack, handle: SocketHandle, now: u64, born: u64) -> SlotFate {
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if socket.state() == tcp::State::Listen {
        return SlotFate::Listening;
    }
    let handshake = matches!(
        socket.state(),
        tcp::State::SynReceived | tcp::State::SynSent
    );
    // CloseWait with an empty buffer is the connect/close storm: `may_recv`
    // is false, so accept never takes the socket and it never returns to LISTEN.
    if !socket.may_recv() && !handshake {
        return SlotFate::Relisten;
    }
    let since = if born == 0 { now.max(1) } else { born };
    if now.saturating_sub(since) >= TCP_IDLE_MS {
        SlotFate::Relisten
    } else {
        SlotFate::Hold(since)
    }
}

fn relisten(stack: &mut crate::net::NetworkStack, handle: SocketHandle, port: u16) -> bool {
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if socket.is_open() {
        socket.abort();
    }
    socket.listen(port).is_ok()
}

fn heal_listen(
    task: usize,
    slots: &mut [Option<SocketHandle>; LISTENERS],
    port: u16,
    stack: &mut crate::net::NetworkStack,
) {
    let now = crate::timer::millis();
    let mut recovered = 0u32;
    let mut listening = 0u32;
    let mut index = 0;
    while index < LISTENERS {
        match slots[index] {
            None => {
                slots[index] = add_listener(stack, port);
                unsafe { LISTEN_BORN[task][index] = 0 };
                if slots[index].is_some() {
                    listening += 1;
                }
            }
            Some(handle) => {
                let born = unsafe { LISTEN_BORN[task][index] };
                match slot_fate(stack, handle, now, born) {
                    SlotFate::Listening => {
                        unsafe { LISTEN_BORN[task][index] = 0 };
                        listening += 1;
                    }
                    SlotFate::Hold(since) => unsafe { LISTEN_BORN[task][index] = since },
                    SlotFate::Relisten => {
                        let back = if relisten(stack, handle, port) {
                            true
                        } else {
                            give_tcp(stack, handle);
                            slots[index] = add_listener(stack, port);
                            slots[index].is_some()
                        };
                        unsafe { LISTEN_BORN[task][index] = 0 };
                        if back {
                            listening += 1;
                            recovered += 1;
                        }
                    }
                }
            }
        }
        index += 1;
    }
    if port != 80 {
        return;
    }
    if recovered > 0 {
        warn_http(&HTTP_HEAL_LOG, "HTTP listen recovered a closed socket");
    }
    if listening == 0 {
        if HTTP_LISTEN_DOWN
            .compare_exchange(0, now.max(1), Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            log::warn!("HTTP listen failed: no socket is listening");
            crate::logger::note_fault("http listen stalled");
        }
    } else {
        HTTP_LISTEN_DOWN.store(0, Ordering::Release);
    }
}

fn reap_clients(task: usize, table: &mut [Kind], stack: &mut crate::net::NetworkStack) {
    let now = crate::timer::millis();
    for index in 0..MAX_FD {
        let Kind::Closing(handle) = table[index] else {
            continue;
        };
        let (queued, connected) = {
            let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
            let queued = socket.send_queue() > 0;
            let connected = matches!(
                socket.state(),
                tcp::State::Established
                    | tcp::State::CloseWait
                    | tcp::State::SynReceived
                    | tcp::State::SynSent
            );
            (queued, connected)
        };
        let since = unsafe { CLOSE_SINCE[task][index] };
        let age = if since == 0 {
            unsafe { CLOSE_SINCE[task][index] = now.max(1) };
            0
        } else {
            now.saturating_sub(since)
        };
        if !close_reap(queued, connected, age, false) {
            continue;
        }
        give_tcp(stack, handle);
        table[index] = Kind::Empty;
        unsafe { CLOSE_SINCE[task][index] = 0 };
    }
}

/// A closing socket is reaped once its send buffer has drained and the
/// handshake is over. A peer that never ACKs used to pin the descriptor.
/// `age_ms` is the hardware-timer age of the close. `hold` keeps a finished
/// close until that budget so a test can fill the fd table.
fn close_reap(send_queued: bool, connected: bool, age_ms: u64, hold: bool) -> bool {
    if age_ms >= TCP_IDLE_MS {
        return true;
    }
    if hold {
        return false;
    }
    !send_queued && !connected
}

fn note_close_reaped(task: usize, index: usize, port: u16, forced: bool) {
    unsafe { CLOSE_SINCE[task][index] = 0 };
    if port != 80 {
        return;
    }
    HTTP_EMFILE_SINCE.store(0, Ordering::Release);
    if forced {
        warn_http(&HTTP_CLOSE_LOG, "HTTP connection close timed out");
    }
}

fn note_http_emfile() {
    let now = crate::timer::millis().max(1);
    let _ = HTTP_EMFILE_SINCE.compare_exchange(0, now, Ordering::AcqRel, Ordering::Relaxed);
    warn_http(&HTTP_ERR_LOG, "HTTP accept failed: no free descriptor");
}

fn http_hold(op: u64) -> i64 {
    if crate::percpu::slot() != crate::task::HTTP_SLOT {
        return EINVAL;
    }
    match op {
        0 => {
            HTTP_HOLD_CLOSE.store(false, Ordering::Release);
            0
        }
        1 => {
            HTTP_HOLD_CLOSE.store(true, Ordering::Release);
            0
        }
        2 => i64::from(HTTP_HOLD_CLOSE.load(Ordering::Acquire)),
        _ => EINVAL,
    }
}

/// Idle TCP buffers. A burst used to park 64 sockets (16 KiB each) and
/// never return them. Eight covers a short storm. After five quiet seconds
/// only two stay, so the next accept is still a reuse and the rest of the
/// heap comes back. Touched only from the network owner.
const TCP_POOL_CAP: usize = 8;
const TCP_POOL_FLOOR: usize = 2;
const TCP_POOL_IDLE_MS: u64 = 5_000;
const UDP_POOL: usize = 16;

#[derive(Clone, Copy)]
struct ParkedTcp {
    handle: SocketHandle,
    since_ms: u64,
}

static mut TCP_POOL_H: [Option<ParkedTcp>; TCP_POOL_CAP] = [None; TCP_POOL_CAP];
static mut TCP_POOL_N: usize = 0;
static mut UDP_POOL_H: [Option<SocketHandle>; UDP_POOL] = [None; UDP_POOL];
static mut UDP_POOL_N: usize = 0;

fn take_tcp(stack: &mut crate::net::NetworkStack) -> SocketHandle {
    unsafe {
        if TCP_POOL_N > 0 {
            TCP_POOL_N -= 1;
            if let Some(parked) = TCP_POOL_H[TCP_POOL_N].take() {
                let socket = stack.sockets_mut().get_mut::<tcp::Socket>(parked.handle);
                if socket.is_open() {
                    socket.abort();
                }
                return parked.handle;
            }
        }
    }
    let rx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    stack.sockets_mut().add(tcp::Socket::new(rx, tx))
}

fn give_tcp(stack: &mut crate::net::NetworkStack, handle: SocketHandle) {
    let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
    if socket.is_open() {
        socket.abort();
    }
    unsafe {
        if TCP_POOL_N < TCP_POOL_CAP {
            TCP_POOL_H[TCP_POOL_N] = Some(ParkedTcp {
                handle,
                since_ms: crate::timer::millis(),
            });
            TCP_POOL_N += 1;
            return;
        }
    }
    stack.sockets_mut().remove(handle);
}

/// Drop parked sockets that have sat past the idle budget, down to the floor.
fn trim_tcp_pool(stack: &mut crate::net::NetworkStack) {
    let now = crate::timer::millis();
    unsafe {
        while TCP_POOL_N > TCP_POOL_FLOOR {
            let Some(oldest) = TCP_POOL_H[0] else {
                break;
            };
            if !pool_reclaim(TCP_POOL_N, TCP_POOL_FLOOR, now.saturating_sub(oldest.since_ms), TCP_POOL_IDLE_MS)
            {
                break;
            }
            let mut index = 0;
            while index + 1 < TCP_POOL_N {
                TCP_POOL_H[index] = TCP_POOL_H[index + 1].take();
                index += 1;
            }
            TCP_POOL_H[TCP_POOL_N - 1] = None;
            TCP_POOL_N -= 1;
            stack.sockets_mut().remove(oldest.handle);
        }
    }
}

/// True when the oldest parked socket should go back to the heap.
fn pool_reclaim(len: usize, floor: usize, oldest_age_ms: u64, idle_ms: u64) -> bool {
    len > floor && oldest_age_ms >= idle_ms
}

fn take_udp(stack: &mut crate::net::NetworkStack) -> SocketHandle {
    unsafe {
        if UDP_POOL_N > 0 {
            UDP_POOL_N -= 1;
            if let Some(handle) = UDP_POOL_H[UDP_POOL_N].take() {
                stack.sockets_mut().get_mut::<udp::Socket>(handle).close();
                return handle;
            }
        }
    }
    // 16 slots of 4096 bytes dropped DNS replies once probes and client
    // forwards shared the socket. 64 slots and 32 KiB keep a 512-byte
    // answer contiguous while a burst is waiting to be drained.
    let rx = udp::PacketBuffer::new(meta(64), vec![0u8; 32 * 1024]);
    let tx = udp::PacketBuffer::new(meta(64), vec![0u8; 32 * 1024]);
    stack.sockets_mut().add(udp::Socket::new(rx, tx))
}

fn give_udp(stack: &mut crate::net::NetworkStack, handle: SocketHandle) {
    stack.sockets_mut().get_mut::<udp::Socket>(handle).close();
    unsafe {
        if UDP_POOL_N < UDP_POOL {
            UDP_POOL_H[UDP_POOL_N] = Some(handle);
            UDP_POOL_N += 1;
            return;
        }
    }
    stack.sockets_mut().remove(handle);
}

fn add_listener(stack: &mut crate::net::NetworkStack, port: u16) -> Option<SocketHandle> {
    let handle = take_tcp(stack);
    let listening = stack
        .sockets_mut()
        .get_mut::<tcp::Socket>(handle)
        .listen(port)
        .is_ok();
    if !listening {
        give_tcp(stack, handle);
        return None;
    }
    Some(handle)
}

fn meta(n: usize) -> Vec<udp::PacketMetadata> {
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        items.push(udp::PacketMetadata::EMPTY);
    }
    items
}

fn port_ok(port: u64) -> Option<u16> {
    if port == 0 || port > 65535 {
        None
    } else {
        Some(port as u16)
    }
}

fn fd_index(fd: u64) -> Option<usize> {
    if fd >= MAX_FD as u64 {
        None
    } else {
        Some(fd as usize)
    }
}

fn find_free() -> Option<usize> {
    (0..MAX_FD).find(|index| matches!(kind_at(*index), Kind::Empty))
}

fn kind_at(index: usize) -> Kind {
    table()[index]
}

fn set_kind(index: usize, kind: Kind) {
    table()[index] = kind;
}

fn copy_user(ptr: u64, dst: &mut [u8]) -> bool {
    if !crate::mmu::user_slice_ok(ptr, dst.len() as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, dst.as_mut_ptr(), dst.len());
    }
    crate::mmu::set_pan(true);
    true
}

fn auth(user: u64, user_len: u64, pass: u64, pass_len: u64) -> i64 {
    if user_len == 0 || user_len > 64 || pass_len > 64 {
        return EINVAL;
    }
    let mut user_buf = [0u8; 64];
    let mut pass_buf = [0u8; 64];
    if !copy_user(user, &mut user_buf[..user_len as usize]) {
        return EFAULT;
    }
    if pass_len > 0 && !copy_user(pass, &mut pass_buf[..pass_len as usize]) {
        return EFAULT;
    }
    let user_text = core::str::from_utf8(&user_buf[..user_len as usize]).unwrap_or("");
    let pass_text = core::str::from_utf8(&pass_buf[..pass_len as usize]).unwrap_or("");
    if crate::config_manager::credentials_match(user_text, pass_text) {
        0
    } else {
        -1
    }
}

fn config_add(ptr: u64, len: u64) -> i64 {
    if len == 0 || len > 256 {
        return EINVAL;
    }
    let mut raw = [0u8; 256];
    if !copy_user(ptr, &mut raw[..len as usize]) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..len as usize]) else {
        return EINVAL;
    };
    let mut parts = text.split_whitespace();
    let Some(name) = parts.next() else {
        return EINVAL;
    };
    let Some(kind) = parts.next() else {
        return EINVAL;
    };
    let Some(value) = parts.next() else {
        return EINVAL;
    };
    let ttl = parts.next().and_then(|text| text.parse().ok()).unwrap_or(300);
    match crate::config_manager::add_record(name, kind, value, ttl) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}

fn config_remove(ptr: u64, len: u64) -> i64 {
    if len == 0 || len > 256 {
        return EINVAL;
    }
    let mut raw = [0u8; 256];
    if !copy_user(ptr, &mut raw[..len as usize]) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..len as usize]) else {
        return EINVAL;
    };
    match crate::config_manager::remove_record(text.trim()) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(_) => EINVAL,
    }
}

/// Four little-endian u32s, 80 bytes of `hdmi_diag`, 192 bytes of
/// `hdmi_last_shut`, 192 bytes of `hdmi_regs`, 16 bytes of `hdmi_port`,
/// 240 bytes of `vclog`, a u32 byte count, and 96 bytes of status.
/// State is 0 none, 1 up, 2 fallback, 3 released. Virt and Pi 5 write
/// zeros and `none`.
fn hdmi_console(ptr: u64, cap: u64) -> i64 {
    const N: usize = 836;
    if cap < N as u64 || !crate::mmu::user_write_ok(ptr, N as u64) {
        return EFAULT;
    }
    let (state, width, height, recoveries) = crate::board::hdmi_console();
    let words = [state, width, height, recoveries];
    let mut raw = [0u8; N];
    for (index, word) in words.iter().enumerate() {
        let off = index * 4;
        raw[off..off + 4].copy_from_slice(&word.to_le_bytes());
    }
    let _ = crate::board::hdmi_diag(&mut raw[16..96]);
    let _ = crate::board::hdmi_last_shut(&mut raw[96..288]);
    let _ = crate::board::hdmi_regs(&mut raw[288..480]);
    let _ = crate::board::hdmi_port(&mut raw[480..496]);
    let _ = crate::board::hdmi_vclog(&mut raw[496..736]);
    let bytes = crate::board::hdmi_vclog_bytes();
    raw[736..740].copy_from_slice(&bytes.to_le_bytes());
    let _ = crate::board::hdmi_vclog_status(&mut raw[740..836]);
    if write_user(ptr, &raw) { N as i64 } else { EFAULT }
}

fn vclog_read(ptr: u64, cap: u64, offset: u64) -> i64 {
    let cap = cap.min(4096) as usize;
    if cap == 0 || !crate::mmu::user_write_ok(ptr, cap as u64) {
        return EFAULT;
    }
    let mut wrote = 0usize;
    let mut off = offset;
    while wrote < cap {
        let mut chunk = [0u8; 512];
        let want = (cap - wrote).min(chunk.len());
        let n = crate::board::hdmi_vclog_at(off, &mut chunk[..want]);
        if n == 0 {
            break;
        }
        if !write_user(ptr + wrote as u64, &chunk[..n]) {
            return EFAULT;
        }
        wrote += n;
        off += n as u64;
    }
    wrote as i64
}

fn boot_call(args: [u64; 6]) -> i64 {
    match args[0] {
        0 => crate::boot::arm(args[1] as u8),
        1 => crash_copy(args[1], args[2], crate::boot::status),
        _ => EINVAL,
    }
}

fn crash_call(args: [u64; 6]) -> i64 {
    match args[0] {
        0 => crate::crash::arm(args[1] as u8),
        1 => crash_copy(args[1], args[2], crate::crash::status),
        2 => crash_copy(args[1], args[2], crate::crash::last_text),
        3 => crash_copy(args[1], args[2], crate::crash::list_text),
        // Deletes the crash files. `FD` is already held. `clear` takes
        // `FS` only. A supervisor action would have to run after this
        // syscall returns, once `FD` is dropped.
        4 => crate::crash::clear(),
        _ => EINVAL,
    }
}

fn crash_copy(ptr: u64, cap: u64, fill: fn(&mut [u8]) -> usize) -> i64 {
    if cap == 0 || cap > 4096 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut buf = [0u8; 4096];
    let n = fill(&mut buf[..cap as usize]);
    if crate::mmu::user_write_ok(ptr, n as u64) && write_user(ptr, &buf[..n]) {
        n as i64
    } else if n == 0 {
        0
    } else {
        EFAULT
    }
}

fn sd_status(ptr: u64, cap: u64) -> i64 {
    if cap < crate::disklog::EXPORT_LEN as u64 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut buf = [0u8; crate::disklog::EXPORT_LEN];
    crate::disklog::export(&mut buf);
    if write_user(ptr, &buf) {
        crate::disklog::EXPORT_LEN as i64
    } else {
        EFAULT
    }
}

fn write_user(ptr: u64, src: &[u8]) -> bool {
    if !crate::mmu::user_write_ok(ptr, src.len() as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), ptr as *mut u8, src.len());
    }
    crate::mmu::set_pan(true);
    true
}

static mut IOBUF: [[u8; 1024]; 4] = [[0; 1024]; 4];

fn iobuf() -> &'static mut [u8] {
    unsafe { &mut IOBUF[crate::percpu::id()] }
}

fn log_post(ptr: u64, len: u64) -> i64 {
    if len == 0 || len > 80 || !crate::mmu::user_slice_ok(ptr, len) {
        return EFAULT;
    }
    let mut raw = [0u8; 80];
    if !copy_user(ptr, &mut raw[..len as usize]) {
        return EFAULT;
    }
    crate::logger::post(1, 0, &raw[..len as usize])
}

fn log_drain(ptr: u64, cap: u64) -> i64 {
    if cap < 2 || cap > 128 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut raw = [0u8; 128];
    let n = crate::logger::drain(&mut raw[..cap as usize]);
    if n == 0 {
        return 0;
    }
    if write_user(ptr, &raw[..n]) {
        n as i64
    } else {
        EFAULT
    }
}

fn log_emit(ptr: u64, len: u64) -> i64 {
    if len == 0 || len > 96 || !crate::mmu::user_slice_ok(ptr, len) {
        return EFAULT;
    }
    let mut raw = [0u8; 96];
    if !copy_user(ptr, &mut raw[..len as usize]) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..len as usize]) else {
        return EINVAL;
    };
    crate::logger::emit_line(text);
    0
}

fn log_recent(ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 1024 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut raw = [0u8; 1024];
    let n = crate::logger::recent(&mut raw[..cap as usize]);
    if n == 0 {
        return 0;
    }
    if write_user(ptr, &raw[..n]) {
        n as i64
    } else {
        EFAULT
    }
}

fn config_set_cache(slots: u64, reap: u64, err: u64, err_cap: u64) -> i64 {
    match crate::config_manager::set_dns_cache(slots, reap) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(message) => {
            let _ = write_err(err, err_cap, &message);
            EINVAL
        }
    }
}

fn set_upstream_health(ptr: u64, len: u64) -> i64 {
    if slot() != crate::task::DNS_SLOT || len == 0 || len > 41 {
        return EINVAL;
    }
    let mut raw = [0u8; 41];
    let n = len as usize;
    if !copy_user(ptr, &mut raw[..n]) {
        return EFAULT;
    }
    crate::dns::set_upstream_health(&raw[..n]);
    0
}

fn net_counters(ptr: u64, cap: u64) -> i64 {
    const N: usize = 40;
    if cap < N as u64 || !crate::mmu::user_write_ok(ptr, N as u64) {
        return EFAULT;
    }
    let (rx, tx) = crate::board::packet_counts();
    let (nic, _) = crate::interrupts::irq_counts();
    let (busy, idle) = service::owner_time();
    let words = [nic, u64::from(rx), u64::from(tx), busy, idle];
    let mut raw = [0u8; N];
    for (index, word) in words.iter().enumerate() {
        let off = index * 8;
        raw[off..off + 8].copy_from_slice(&word.to_le_bytes());
    }
    if write_user(ptr, &raw) { N as i64 } else { EFAULT }
}

fn set_dns_load(inflight: u64, timeouts: u64, forward_full: u64, reply_drops: u64) -> i64 {
    if slot() != crate::task::DNS_SLOT {
        return EINVAL;
    }
    crate::dns::set_load(inflight, timeouts, forward_full, reply_drops);
    0
}

fn net_extra(ptr: u64, cap: u64) -> i64 {
    const N: usize = 136;
    if cap < N as u64 || !crate::mmu::user_write_ok(ptr, N as u64) {
        return EFAULT;
    }
    let (requests, replies, hits, misses) = crate::net::arp_counters();
    let (inflight, timeouts, forward_full, reply_drops) = crate::dns::load();
    let (rx_drops, rx_overruns, rx_queued) = crate::board::rx_load();
    let (poll_count, poll_us, poll_max_us) = crate::net::service::poll_stats();
    let (irq_rearms, tx_full, tx_slow) = crate::board::nic_faults();
    let words = [
        requests,
        replies,
        hits,
        misses,
        inflight,
        timeouts,
        rx_drops,
        rx_overruns,
        rx_queued,
        forward_full,
        reply_drops,
        poll_count,
        poll_us,
        poll_max_us,
        irq_rearms,
        tx_full,
        tx_slow,
    ];
    let mut raw = [0u8; N];
    for (index, word) in words.iter().enumerate() {
        let off = index * 8;
        raw[off..off + 8].copy_from_slice(&word.to_le_bytes());
    }
    if write_user(ptr, &raw) { N as i64 } else { EFAULT }
}

fn upstream_health(ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 41 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let mut raw = [0u8; 41];
    let n = crate::dns::copy_upstream_health(&mut raw[..cap as usize]);
    if n == 0 {
        return 0;
    }
    if write_user(ptr, &raw[..n]) {
        n as i64
    } else {
        EFAULT
    }
}

fn set_dns_top(ptr: u64, len: u64) -> i64 {
    if slot() != crate::task::DNS_SLOT || len < 21 || len > 576 {
        return EINVAL;
    }
    let mut raw = [0u8; 576];
    let n = len as usize;
    if !copy_user(ptr, &mut raw[..n]) {
        return EFAULT;
    }
    crate::dns::set_top(&raw[..n]);
    0
}

fn set_cache_stats(ptr: u64) -> i64 {
    let mut raw = [0u8; 64];
    if !copy_user(ptr, &mut raw) {
        return EFAULT;
    }
    let mut words = [0u64; 8];
    let mut index = 0;
    while index < 8 {
        let start = index * 8;
        let mut chunk = [0u8; 8];
        chunk.copy_from_slice(&raw[start..start + 8]);
        words[index] = u64::from_le_bytes(chunk);
        index += 1;
    }
    crate::dns::set_cache_stats(&words);
    0
}

fn cache_plant(op: u64, ttl: u64) -> i64 {
    if op == 0 {
        if ttl == 0 || ttl > 5 {
            return EINVAL;
        }
        crate::dns::plant_request(ttl as u32);
        0
    } else if op == 1 {
        crate::dns::plant_take() as i64
    } else {
        EINVAL
    }
}

fn config_read(which: u64, buf: u64, cap: u64) -> i64 {
    if which > 1 || cap == 0 || cap > 8192 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    let mut raw = [0u8; 8192];
    match crate::config_manager::copy_config_file(which as u8, &mut raw[..cap as usize]) {
        Ok(n) => {
            if write_user(buf, &raw[..n]) {
                n as i64
            } else {
                EFAULT
            }
        }
        Err(_) => EIO,
    }
}

fn config_write(which: u64, ptr: u64, len: u64, err: u64, err_cap: u64) -> i64 {
    if which > 1 {
        return EINVAL;
    }
    if len > config::UPLOAD_MAX as u64 {
        let _ = write_err(err, err_cap, "upload exceeds 4096 bytes");
        return EINVAL;
    }
    if len > 0 && !crate::mmu::user_slice_ok(ptr, len) {
        return EFAULT;
    }
    let mut raw = [0u8; 4096];
    if len > 0 && !copy_user(ptr, &mut raw[..len as usize]) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..len as usize]) else {
        let _ = write_err(err, err_cap, "malformed json");
        return EINVAL;
    };
    match crate::config_manager::store_config_file(which as u8, text) {
        Ok(restart) => {
            if restart {
                1
            } else {
                0
            }
        }
        Err(message) => {
            let _ = write_err(err, err_cap, &message);
            EINVAL
        }
    }
}

fn csrf_token(ptr: u64, cap: u64) -> i64 {
    if cap < 16 || !crate::mmu::user_write_ok(ptr, 16) {
        return EFAULT;
    }
    let token = crate::config_manager::csrf_bytes();
    if write_user(ptr, &token) {
        16
    } else {
        EFAULT
    }
}

fn write_err(ptr: u64, cap: u64, message: &str) -> bool {
    if cap == 0 || ptr == 0 {
        return false;
    }
    let n = message.len().min(cap as usize);
    write_user(ptr, &message.as_bytes()[..n])
}

fn file_name(ptr: u64, len: u64) -> Option<[u8; 12]> {
    if len == 0 || len > 12 || !crate::mmu::user_slice_ok(ptr, len) {
        return None;
    }
    let mut raw = [0u8; 12];
    if !copy_user(ptr, &mut raw[..len as usize]) {
        return None;
    }
    let name = &raw[..len as usize];
    if !name
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'.' || *byte == b'_')
    {
        return None;
    }
    Some(raw)
}

fn log_file(args: [u64; 6]) -> i64 {
    match args[0] {
        0 => log_file_list(args[1], args[2]),
        1 => log_file_read(args[1], args[2], args[3], args[4], args[5]),
        2 => log_file_len(args[1], args[2]),
        _ => EINVAL,
    }
}

fn log_disk_name(ptr: u64, len: u64) -> Option<[u8; 12]> {
    let raw = file_name(ptr, len)?;
    let text = core::str::from_utf8(&raw[..len as usize]).ok()?;
    if crate::fs::log_disk_ok(text) {
        Some(raw)
    } else {
        None
    }
}

fn log_file_len(name: u64, name_len: u64) -> i64 {
    let Some(raw) = log_disk_name(name, name_len) else {
        return EINVAL;
    };
    let Ok(text) = core::str::from_utf8(&raw[..name_len as usize]) else {
        return EINVAL;
    };
    match crate::fs::file_len(text) {
        Ok(len) => len as i64,
        Err(crate::fs::FsError::NotInitialized) => -1,
        Err(crate::fs::FsError::Io { detail, .. }) if detail == "NotFound" => -1,
        Err(_) => EIO,
    }
}

fn log_file_read(name: u64, name_len: u64, buf: u64, cap: u64, offset: u64) -> i64 {
    let Some(raw) = log_disk_name(name, name_len) else {
        return EINVAL;
    };
    if cap == 0 || cap > 1024 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    if offset > u32::MAX as u64 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(&raw[..name_len as usize]) else {
        return EINVAL;
    };
    match crate::fs::read_at(text, offset as u32, &mut iobuf()[..cap as usize]) {
        Ok(0) => 0,
        Ok(copied) => {
            if write_user(buf, &iobuf()[..copied]) {
                copied as i64
            } else {
                EFAULT
            }
        }
        Err(crate::fs::FsError::NotInitialized) => -1,
        Err(crate::fs::FsError::Io { detail, .. }) if detail == "NotFound" => -1,
        Err(_) => EIO,
    }
}

fn log_file_list(buf: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 1024 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    let mut out = [0u8; 512];
    let mut n = 0usize;
    let mut index = 0u32;
    while index <= 16 {
        let name = if index == 0 {
            crate::fs::LOG_NAME
        } else {
            crate::fs::log_archive_name(index)
        };
        if let Ok(true) = crate::fs::file_exists(name) {
            if let Ok(len) = crate::fs::file_len(name) {
                if !push_log_line(&mut out, &mut n, name, len) {
                    break;
                }
            }
        }
        index += 1;
    }
    if n == 0 {
        return 0;
    }
    let copy = n.min(cap as usize);
    if write_user(buf, &out[..copy]) {
        copy as i64
    } else {
        EFAULT
    }
}

fn push_log_line(out: &mut [u8], n: &mut usize, name: &str, len: u32) -> bool {
    let mut tmp = [0u8; 32];
    let mut t = 0usize;
    let bytes = name.as_bytes();
    if t + bytes.len() + 12 >= tmp.len() {
        return false;
    }
    tmp[t..t + bytes.len()].copy_from_slice(bytes);
    t += bytes.len();
    tmp[t] = b' ';
    t += 1;
    let mut val = len;
    let mut digits = [0u8; 10];
    let mut d = 10usize;
    if val == 0 {
        d -= 1;
        digits[d] = b'0';
    }
    while val > 0 {
        d -= 1;
        digits[d] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    let shown = &digits[d..];
    tmp[t..t + shown.len()].copy_from_slice(shown);
    t += shown.len();
    tmp[t] = b'\n';
    t += 1;
    if *n + t > out.len() {
        return false;
    }
    out[*n..*n + t].copy_from_slice(&tmp[..t]);
    *n += t;
    true
}

fn file_read(name: u64, name_len: u64, buf: u64, cap: u64) -> i64 {
    let Some(raw) = file_name(name, name_len) else {
        return EINVAL;
    };
    if cap == 0 || cap > 1024 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..name_len as usize]) else {
        return EINVAL;
    };
    let n = crate::fs::read_named(text, &mut iobuf()[..cap as usize]);
    match n {
        Ok(0) => 0,
        Ok(copied) => {
            if write_user(buf, &iobuf()[..copied]) {
                copied as i64
            } else {
                EFAULT
            }
        }
        Err(crate::fs::FsError::NotInitialized) => 0,
        Err(crate::fs::FsError::Io { detail, .. }) if detail == "NotFound" => 0,
        Err(_) => EIO,
    }
}

fn file_write(name: u64, name_len: u64, buf: u64, len: u64, flags: u64) -> i64 {
    let Some(raw) = file_name(name, name_len) else {
        return EINVAL;
    };
    if len > 1024 {
        return EINVAL;
    }
    if len > 0 && !crate::mmu::user_slice_ok(buf, len) {
        return EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..name_len as usize]) else {
        return EINVAL;
    };
    if len > 0 && !read_user_io(buf, len as usize) {
        return EFAULT;
    }
    let data = &iobuf()[..len as usize];
    match crate::fs::write_named(text, data, (flags & 1) == 1) {
        Ok(()) => 0,
        Err(_) => EIO,
    }
}

fn display_io(op: u64, a: u64, b: u64, _c: u64) -> i64 {
    match op {
        0 if hdmi_caller() => crate::board::hdmi_hpd(),
        1 if hdmi_caller() => mailbox_user(a, b),
        2 if hdmi_caller() => map_fb(a, b),
        3 if hdmi_caller() => {
            crate::mmu::unmap_user_fb(crate::task::HDMI_SLOT);
            0
        }
        4 => crate::config_manager::vnc_port() as i64,
        5 => copy_vnc_password(a, b),
        6 => {
            if crate::task::vnc_is_live() {
                1
            } else {
                0
            }
        }
        7 => {
            if crate::board::hdmi_present() {
                1
            } else {
                0
            }
        }
        0 | 1 | 2 | 3 => EBADF,
        _ => EINVAL,
    }
}

fn hdmi_caller() -> bool {
    slot() == crate::task::HDMI_SLOT
}

fn mailbox_user(ptr: u64, len: u64) -> i64 {
    if len < 16 || len > 1024 || len % 4 != 0 || ptr % 4 != 0 || !crate::mmu::user_write_ok(ptr, len)
    {
        return EINVAL;
    }
    let n = (len as usize) / 4;
    let mut words = [0u32; 256];
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u32, words.as_mut_ptr(), n);
    }
    crate::mmu::set_pan(true);
    let rc = crate::board::hdmi_mailbox(&mut words[..n]);
    if rc != 0 {
        return rc;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(words.as_ptr(), ptr as *mut u32, n);
    }
    crate::mmu::set_pan(true);
    0
}

fn map_fb(bus: u64, bytes: u64) -> i64 {
    let Some((phys, bytes)) = crate::board::fb_bus_to_phys(bus, bytes) else {
        return EINVAL;
    };
    crate::mmu::map_user_fb(crate::task::HDMI_SLOT, phys, bytes)
}

fn copy_vnc_password(ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 8 || !crate::mmu::user_write_ok(ptr, cap) {
        return EFAULT;
    }
    let pass = crate::config_manager::vnc_password();
    let n = pass.len().min(cap as usize);
    if n > 0 && !write_user(ptr, &pass.as_bytes()[..n]) {
        return EFAULT;
    }
    n as i64
}

fn display_set(vnc: u64, port: u64, pass_ptr: u64, pass_len: u64, hdmi: u64, err: u64) -> i64 {
    if pass_len > 8 {
        let _ = write_err(err, 160, "display.vnc_password: out of bounds (0..=8)");
        return EINVAL;
    }
    let mut raw = [0u8; 8];
    if pass_len > 0 && !copy_user(pass_ptr, &mut raw[..pass_len as usize]) {
        return EFAULT;
    }
    let Ok(password) = core::str::from_utf8(&raw[..pass_len as usize]) else {
        let _ = write_err(err, 160, "display.vnc_password: out of bounds (0..=8)");
        return EINVAL;
    };
    match crate::config_manager::set_display(vnc, port, password, hdmi) {
        Ok(()) => 0,
        Err(message) => {
            let _ = write_err(err, 160, &message);
            EINVAL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{close_reap, pool_reclaim};

    #[test]
    fn a_drained_close_is_reaped_immediately() {
        assert!(close_reap(false, false, 0, false));
    }

    #[test]
    fn an_unacked_close_is_held_until_the_budget() {
        assert!(!close_reap(true, false, 0, false));
        assert!(!close_reap(true, false, 4_999, false));
        assert!(close_reap(true, true, 5_000, false));
    }

    #[test]
    fn a_held_close_fills_the_table_until_the_budget() {
        assert!(!close_reap(false, false, 0, true));
        assert!(!close_reap(false, false, 4_999, true));
        assert!(close_reap(false, false, 5_000, true));
    }

    #[test]
    fn idle_parked_sockets_drop_back_to_the_floor() {
        assert!(!pool_reclaim(2, 2, 10_000, 5_000));
        assert!(!pool_reclaim(8, 2, 4_999, 5_000));
        assert!(pool_reclaim(8, 2, 5_000, 5_000));
        assert!(pool_reclaim(3, 2, 5_000, 5_000));
    }

    #[test]
    fn dns_socket_is_watched_only_after_it_binds() {
        use super::dns_gone;
        assert!(!dns_gone(false, false, 60_000));
        assert!(!dns_gone(true, true, 60_000));
        assert!(!dns_gone(true, false, 4_999));
        assert!(dns_gone(true, false, 5_000));
    }
}

fn read_user_io(ptr: u64, len: usize) -> bool {
    if !crate::mmu::user_slice_ok(ptr, len as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, iobuf().as_mut_ptr(), len);
    }
    crate::mmu::set_pan(true);
    true
}
