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

const EFAULT: i64 = -2;
const EBADF: i64 = -3;
const EINVAL: i64 = -4;
const EIO: i64 = -5;
const EMFILE: i64 = -6;
const EMSGSIZE: i64 = -7;
const EAGAIN: i64 = -11;

const MAX_FD: usize = 8;
const MAX_PACKET: usize = 4096;

#[derive(Clone, Copy)]
enum Kind {
    Empty,
    Udp(SocketHandle),
    TcpListen(SocketHandle, u16),
    TcpConn(SocketHandle),
    Closing(SocketHandle),
}

static mut FDS: [Kind; MAX_FD] = [
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
    Kind::Empty,
];

static mut PACKET: [u8; MAX_PACKET] = [0; MAX_PACKET];

pub fn call(num: u64, args: [u64; 6]) -> u64 {
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
        unsafe { FDS[fd] = Kind::Udp(handle) };
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
                    PACKET[..n].copy_from_slice(&data[..n]);
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
            let copied = unsafe { write_user(buf, &PACKET[..n]) };
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
        let packet = unsafe { &PACKET[..len as usize] };
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
        reap(stack);
        let Some(fd) = find_free() else {
            return EMFILE;
        };
        match add_listener(stack, port) {
            Some(handle) => {
                unsafe { FDS[fd] = Kind::TcpListen(handle, port) };
                fd as i64
            }
            None => EIO,
        }
    });
    result.unwrap_or(EIO)
}

fn tcp_accept(fd: u64) -> i64 {
    let Some(fd) = fd_index(fd) else {
        return EBADF;
    };
    let result = with_stack(|stack, _| {
        reap(stack);
        let (handle, port) = match kind_at(fd) {
            Kind::TcpListen(handle, port) => (handle, port),
            _ => return EBADF,
        };
        let ready = {
            let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
            socket.is_active()
        };
        if !ready {
            return EAGAIN;
        }
        let Some(conn) = find_free() else {
            return EMFILE;
        };
        let Some(fresh) = add_listener(stack, port) else {
            return EIO;
        };
        unsafe {
            FDS[fd] = Kind::TcpListen(fresh, port);
            FDS[conn] = Kind::TcpConn(handle);
        }
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
            if socket.may_recv() {
                return Err(EAGAIN);
            }
            return Ok(0);
        }
        let n = cap as usize;
        match socket.recv_slice(unsafe { &mut PACKET[..n] }) {
            Ok(copied) => Ok(copied),
            Err(tcp::RecvError::Finished) => Ok(0),
            Err(_) => Err(EAGAIN),
        }
    });
    match outcome {
        Some(Ok(0)) => 0,
        Some(Ok(n)) => {
            if unsafe { write_user(buf, &PACKET[..n]) } {
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
        reap(stack);
        let Kind::TcpConn(handle) = kind_at(fd) else {
            return Err(EBADF);
        };
        let socket = stack.sockets_mut().get_mut::<tcp::Socket>(handle);
        if !socket.can_send() {
            return Err(EAGAIN);
        }
        let packet = unsafe { &PACKET[..len as usize] };
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
            Kind::TcpConn(handle) | Kind::TcpListen(handle, _) => {
                stack.sockets_mut().get_mut::<tcp::Socket>(handle).close();
                unsafe { FDS[fd] = Kind::Closing(handle) };
            }
            Kind::Udp(handle) => {
                stack.sockets_mut().remove(handle);
                unsafe { FDS[fd] = Kind::Empty };
            }
            _ => return EBADF,
        }
        0
    });
    result.unwrap_or(EIO)
}

fn add_listener(stack: &mut crate::net::NetworkStack, port: u16) -> Option<SocketHandle> {
    let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
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

fn reap(stack: &mut crate::net::NetworkStack) {
    for index in 0..MAX_FD {
        let Kind::Closing(handle) = kind_at(index) else {
            continue;
        };
        let finished = !stack.sockets_mut().get::<tcp::Socket>(handle).is_open();
        if finished {
            stack.sockets_mut().remove(handle);
            unsafe { FDS[index] = Kind::Empty };
        }
    }
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
    unsafe { FDS[index] }
}

fn read_user(ptr: u64, len: usize) -> bool {
    if !crate::mmu::user_slice_ok(ptr, len as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, core::ptr::addr_of_mut!(PACKET).cast(), len);
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
