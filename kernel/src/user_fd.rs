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
//!
//! Each EL0 task has its own fd table. The scheduler selects it before eret.

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

const EFAULT: i64 = -2;
const EBADF: i64 = -3;
const EINVAL: i64 = -4;
const EIO: i64 = -5;
const EMFILE: i64 = -6;
const EMSGSIZE: i64 = -7;
const EAGAIN: i64 = -11;

const MAX_FD: usize = 24;
const MAX_PACKET: usize = 4096;
/// Listeners allocated once. A socket leaves LISTEN on the SYN, and a peer
/// that stops in FIN-WAIT-2 must not be the only socket on the port.
const LISTENERS: usize = service::TCP_LISTENERS;

#[derive(Clone, Copy)]
enum Kind {
    Empty,
    Reserved,
    Udp(SocketHandle),
    TcpListen([Option<SocketHandle>; LISTENERS], u16),
    TcpConn(SocketHandle),
    Closing(SocketHandle),
}

const TASKS: usize = 7;

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
        UDP_RECV => udp_recv(args[0], args[1], args[2], args[3]),
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
        _ => -1,
    };
    value as u64
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

fn udp_recv(fd: u64, buf: u64, cap: u64, addr: u64) -> i64 {
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
    let Kind::TcpListen(_, _) = kind_at(fd) else {
        return EBADF;
    };
    let Some(conn) = find_free() else {
        return EMFILE;
    };
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
        Err(_) => service::Step::Pending,
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
        if socket.may_recv()
            || matches!(
                socket.state(),
                tcp::State::SynSent | tcp::State::SynReceived
            )
        {
            return if nonblock {
                service::Step::Done(EAGAIN)
            } else {
                service::Step::Pending
            };
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
        if matches!(
            socket.state(),
            tcp::State::Established | tcp::State::CloseWait | tcp::State::SynReceived | tcp::State::SynSent
        ) {
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
}

fn recycle_task(task: usize, stack: &mut crate::net::NetworkStack) {
    let table = table_of(task);
    let Some(listen_fd) = (0..MAX_FD).find(|index| matches!(table[*index], Kind::TcpListen(_, _)))
    else {
        return;
    };
    let Kind::TcpListen(mut slots, port) = table[listen_fd] else {
        return;
    };
    for index in 0..MAX_FD {
        let Kind::Closing(handle) = table[index] else {
            continue;
        };
        if slots.iter().all(|slot| slot.is_some()) {
            break;
        }
        let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
        if socket.send_queue() > 0 {
            continue;
        }
        if matches!(
            socket.state(),
            tcp::State::Established
                | tcp::State::CloseWait
                | tcp::State::SynReceived
                | tcp::State::SynSent
        ) {
            continue;
        }
        if socket.is_open() {
            socket.abort();
        }
        if socket.listen(port).is_err() {
            continue;
        }
        if let Some(slot) = slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(handle);
            table[index] = Kind::Empty;
        }
    }
    table[listen_fd] = Kind::TcpListen(slots, port);
}

/// The kernel heap never frees. Socket buffers are parked here on teardown
/// and taken again the next time a port is bound, so a restart does not
/// allocate another set. Touched only from the network owner.
const TCP_POOL: usize = 64;
const UDP_POOL: usize = 16;
static mut TCP_POOL_H: [Option<SocketHandle>; TCP_POOL] = [None; TCP_POOL];
static mut TCP_POOL_N: usize = 0;
static mut UDP_POOL_H: [Option<SocketHandle>; UDP_POOL] = [None; UDP_POOL];
static mut UDP_POOL_N: usize = 0;

fn take_tcp(stack: &mut crate::net::NetworkStack) -> SocketHandle {
    unsafe {
        if TCP_POOL_N > 0 {
            TCP_POOL_N -= 1;
            if let Some(handle) = TCP_POOL_H[TCP_POOL_N].take() {
                let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
                if socket.is_open() {
                    socket.abort();
                }
                return handle;
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
        if TCP_POOL_N < TCP_POOL {
            TCP_POOL_H[TCP_POOL_N] = Some(handle);
            TCP_POOL_N += 1;
            return;
        }
    }
    stack.sockets_mut().remove(handle);
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
    let rx = udp::PacketBuffer::new(meta(16), vec![0u8; 4096]);
    let tx = udp::PacketBuffer::new(meta(16), vec![0u8; 4096]);
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
