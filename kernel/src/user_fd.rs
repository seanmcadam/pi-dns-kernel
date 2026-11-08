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
//!  10  tcp_recv(fd, buf, cap) -> len, or 0 when the peer has closed
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
//!
//! Each EL0 task has its own fd table. The scheduler selects it before eret.

use alloc::vec;
use alloc::vec::Vec;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use crate::net::with_stack;

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

const EFAULT: i64 = -2;
const EBADF: i64 = -3;
const EINVAL: i64 = -4;
const EIO: i64 = -5;
const EMFILE: i64 = -6;
const EMSGSIZE: i64 = -7;
const EAGAIN: i64 = -11;

const MAX_FD: usize = 8;
const MAX_PACKET: usize = 4096;
/// Listeners allocated once. A socket leaves LISTEN on the SYN, and a peer
/// that stops in FIN-WAIT-2 must not be the only socket on the port.
const LISTENERS: usize = 4;

#[derive(Clone, Copy)]
enum Kind {
    Empty,
    Udp(SocketHandle),
    TcpListen([Option<SocketHandle>; LISTENERS], u16),
    TcpConn(SocketHandle),
    Closing(SocketHandle),
}

const TASKS: usize = 4;

static mut FDS: [[Kind; MAX_FD]; TASKS] = [
    [Kind::Empty; MAX_FD],
    [Kind::Empty; MAX_FD],
    [Kind::Empty; MAX_FD],
    [Kind::Empty; MAX_FD],
];
static mut TASK_SLOT: [usize; 4] = [0; 4];
static FD_LOCK: [crate::spin::Spin; TASKS] = [
    crate::spin::Spin::new(),
    crate::spin::Spin::new(),
    crate::spin::Spin::new(),
    crate::spin::Spin::new(),
];

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

static mut PACKET: [[u8; MAX_PACKET]; 4] = [[0; MAX_PACKET]; 4];

fn packet() -> &'static mut [u8] {
    unsafe { &mut PACKET[crate::percpu::id()] }
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
        TCP_RECV => tcp_recv(args[0], args[1], args[2]),
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
    let result = with_stack(|stack, _| {
        reap(stack);
        if find_free().is_none() {
            return EMFILE;
        }
        let rx = udp::PacketBuffer::new(meta(16), vec![0u8; 4096]);
        let tx = udp::PacketBuffer::new(meta(16), vec![0u8; 4096]);
        let handle = stack.sockets_mut().add(udp::Socket::new(rx, tx));
        let bound = stack
            .sockets_mut()
            .get_mut::<udp::Socket>(handle)
            .bind(port)
            .is_ok();
        if !bound {
            stack.sockets_mut().remove(handle);
            return EIO;
        }
        let fd = find_free().unwrap();
        set_kind(fd, Kind::Udp(handle));
        fd as i64
    });
    let fd = result.unwrap_or(EIO);
    if fd >= 0 {
        if port == 53 {
            log::info!("DNS socket bound to port 53");
        } else if port == 53535 {
            log::info!("DNS upstream socket bound to port 53535");
        }
    }
    fd
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
    let outcome = with_stack(|stack, _| {
        reap(stack);
        let Kind::Udp(handle) = kind_at(fd) else {
            return Err(EBADF);
        };
        let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
        if !socket.can_recv() {
            return Err(EAGAIN);
        }
        match socket.recv() {
            Ok((data, meta)) => {
                let n = data.len().min(cap as usize);
                unsafe {
                    packet()[..n].copy_from_slice(&data[..n]);
                }
                let ip = match meta.endpoint.addr {
                    IpAddress::Ipv4(ip) => {
                        let bytes = ip.as_bytes();
                        [bytes[0], bytes[1], bytes[2], bytes[3]]
                    }
                    _ => [0, 0, 0, 0],
                };
                let port = meta.endpoint.port.to_be_bytes();
                Ok((n, [ip[0], ip[1], ip[2], ip[3], port[0], port[1], 0, 0]))
            }
            Err(_) => Err(EAGAIN),
        }
    });
    match outcome {
        Some(Ok((n, addr_bytes))) => {
            let copied = unsafe { write_user(buf, &packet()[..n]) };
            if copied && write_user(addr, &addr_bytes) {
                n as i64
            } else {
                EFAULT
            }
        }
        Some(Err(err)) => err,
        None => EIO,
    }
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
    if !read_user(buf, len as usize) {
        return EFAULT;
    }
    let ip_bytes = (ip as u32).to_be_bytes();
    let sent = with_stack(|stack, _| {
        reap(stack);
        let Kind::Udp(handle) = kind_at(fd) else {
            return Err(EBADF);
        };
        let socket = stack.sockets_mut().get_mut::<udp::Socket>(handle);
        let endpoint = IpEndpoint::new(
            Ipv4Address::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]).into(),
            port,
        );
        let packet = unsafe { &packet()[..len as usize] };
        socket
            .send_slice(packet, endpoint)
            .map(|_| 0)
            .map_err(|_| EIO)
    });
    sent.unwrap_or(Err(EIO)).unwrap_or_else(|err| err)
}

fn tcp_listen(port: u64) -> i64 {
    let Some(port) = port_ok(port) else {
        return EINVAL;
    };
    let result = with_stack(|stack, _| {
        recycle(stack);
        let Some(fd) = find_free() else {
            return EMFILE;
        };
        let mut slots = [None; LISTENERS];
        for slot in slots.iter_mut() {
            *slot = add_listener(stack, port);
        }
        if slots.iter().all(|slot| slot.is_none()) {
            return EIO;
        }
        set_kind(fd, Kind::TcpListen(slots, port));
        fd as i64
    });
    let fd = result.unwrap_or(EIO);
    if fd >= 0 && port == 80 {
        log::info!("HTTP socket listening on port 80");
    }
    fd
}

fn tcp_accept(fd: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let result = with_stack(|stack, _| {
        recycle(stack);
        let Kind::TcpListen(mut slots, port) = kind_at(fd) else {
            return EBADF;
        };
        let ready = slots.iter().position(|slot| {
            let Some(handle) = slot else {
                return false;
            };
            let socket = stack.sockets_mut().get::<tcp::Socket>(*handle);
            // SYN-RECEIVED is active but may_recv is false. Handing it out
            // makes the first recv look like EOF and aborts the handshake.
            socket.may_recv()
        });
        let Some(index) = ready else {
            return EAGAIN;
        };
        let Some(handle) = slots[index].take() else {
            return EAGAIN;
        };
        let Some(conn) = find_free() else {
            slots[index] = Some(handle);
            set_kind(fd, Kind::TcpListen(slots, port));
            return EMFILE;
        };
        set_kind(fd, Kind::TcpListen(slots, port));
        set_kind(conn, Kind::TcpConn(handle));
        conn as i64
    });
    result.unwrap_or(EIO)
}

fn tcp_recv(fd: u64, buf: u64, cap: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    if cap == 0 || cap > MAX_PACKET as u64 || !crate::mmu::user_write_ok(buf, cap) {
        return EFAULT;
    }
    let outcome = with_stack(|stack, _| {
        reap(stack);
        let Kind::TcpConn(handle) = kind_at(fd) else {
            return Err(EBADF);
        };
        let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
        if !socket.can_recv() {
            if socket.may_recv()
                || matches!(
                    socket.state(),
                    tcp::State::SynSent | tcp::State::SynReceived
                )
            {
                return Err(EAGAIN);
            }
            return Ok(0);
        }
        let n = cap as usize;
        match socket.recv_slice(unsafe { &mut packet()[..n] }) {
            Ok(copied) => Ok(copied),
            Err(tcp::RecvError::Finished) => Ok(0),
            Err(_) => Err(EAGAIN),
        }
    });
    match outcome {
        Some(Ok(0)) => 0,
        Some(Ok(n)) => {
            if unsafe { write_user(buf, &packet()[..n]) } {
                n as i64
            } else {
                EFAULT
            }
        }
        Some(Err(err)) => err,
        None => EIO,
    }
}

fn tcp_send(fd: u64, buf: u64, len: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    if len == 0 || len > MAX_PACKET as u64 || !crate::mmu::user_slice_ok(buf, len) {
        return EFAULT;
    }
    if !read_user(buf, len as usize) {
        return EFAULT;
    }
    let sent = with_stack(|stack, _| {
        recycle(stack);
        let Kind::TcpConn(handle) = kind_at(fd) else {
            return Err(EBADF);
        };
        let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
        if !socket.can_send() {
            return Err(EAGAIN);
        }
        let packet = unsafe { &packet()[..len as usize] };
        socket
            .send_slice(packet)
            .map(|n| n as i64)
            .map_err(|_| EIO)
    });
    sent.unwrap_or(Err(EIO)).unwrap_or_else(|err| err)
}

fn tcp_close(fd: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let result = with_stack(|stack, _| {
        match kind_at(fd) {
            Kind::TcpConn(handle) => {
                stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
                set_kind(fd, Kind::Closing(handle));
            }
            Kind::TcpListen(slots, _) => {
                for handle in slots.into_iter().flatten() {
                    stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
                }
                set_kind(fd, Kind::Empty);
            }
            Kind::Udp(handle) => {
                stack.sockets_mut().remove(handle);
                set_kind(fd, Kind::Empty);
            }
            _ => return EBADF,
        }
        0
    });
    result.unwrap_or(EIO)
}

fn add_listener(stack: &mut crate::net::NetworkStack, port: u16) -> Option<SocketHandle> {
    let rx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let handle = stack.sockets_mut().add(tcp::Socket::new(rx, tx));
    let listening = stack
        .sockets_mut()
        .get_mut::<tcp::Socket>(handle)
        .listen(port)
        .is_ok();
    if !listening {
        stack.sockets_mut().remove(handle);
        return None;
    }
    Some(handle)
}

fn recycle(stack: &mut crate::net::NetworkStack) {
    let Some(listen_fd) = (0..MAX_FD).find(|index| matches!(kind_at(*index), Kind::TcpListen(_, _)))
    else {
        return;
    };
    let Kind::TcpListen(mut slots, port) = kind_at(listen_fd) else {
        return;
    };
    for index in 0..MAX_FD {
        let Kind::Closing(handle) = kind_at(index) else {
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
            tcp::State::Established | tcp::State::CloseWait | tcp::State::SynReceived | tcp::State::SynSent
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
            set_kind(index, Kind::Empty);
        }
    }
    set_kind(listen_fd, Kind::TcpListen(slots, port));
}

fn reap(stack: &mut crate::net::NetworkStack) {
    recycle(stack);
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

fn read_user(ptr: u64, len: usize) -> bool {
    if !crate::mmu::user_slice_ok(ptr, len as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, packet().as_mut_ptr(), len);
    }
    crate::mmu::set_pan(true);
    true
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
    let n = unsafe { crate::fs::read_named(text, &mut iobuf()[..cap as usize]) };
    match n {
        Ok(0) => 0,
        Ok(copied) => {
            if unsafe { write_user(buf, &iobuf()[..copied]) } {
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
    let data = unsafe { &iobuf()[..len as usize] };
    match crate::fs::write_named(text, data, (flags & 1) == 1) {
        Ok(()) => 0,
        Err(_) => EIO,
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
