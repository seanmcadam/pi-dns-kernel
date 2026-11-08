//! Web UI. Same routes as the old kernel HTTP loop. Records, upstreams,
//! the blocklist, and the login check go through syscalls, so a change is
//! visible to the DNS task on its next lookup and is saved by the kernel.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use http_server::{parse_basic_auth, HttpMethod, HttpRequest, HttpResponse, ParseError};

const HTTP_WORKERS: usize = 4;
static JOB: [AtomicU32; HTTP_WORKERS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static WAITING: [AtomicU32; HTTP_WORKERS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

const YIELD: u64 = 1;
const EXIT: u64 = 3;
const TCP_LISTEN: u64 = 8;
const TCP_ACCEPT: u64 = 9;
const TCP_RECV: u64 = 10;
const TCP_SEND: u64 = 11;
const TCP_CLOSE: u64 = 12;
const CONFIG_RECORDS: u64 = 13;
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
const LOG_RECENT: u64 = 31;
const DEBUG_STATUS: u64 = 42;
const DEBUG_CRASH: u64 = 43;

const EAGAIN: i64 = -11;

pub fn run() -> ! {
    let listen = sys(TCP_LISTEN, 80, 0, 0, 0, 0, 0);
    if listen < 0 {
        exit(1);
    }
    let mut worker = 0;
    while worker < HTTP_WORKERS {
        let top = 0x1_000A_0000u64 + (1536 * 1024) - (worker as u64 + 1) * (16 * 1024);
        let _ = crate::thread::spawn(conn_worker, top, worker as u64);
        worker += 1;
    }
    loop {
        let conn = sys(TCP_ACCEPT, listen as u64, 0, 0, 0, 0, 0);
        if conn < 0 {
            yield_now();
            continue;
        }
        if !handoff(conn as u32) {
            serve(conn);
            let _ = sys(TCP_CLOSE, conn as u64, 0, 0, 0, 0, 0);
        }
        yield_now();
    }
}

extern "C" fn conn_worker(index: u64) {
    let slot = index as usize;
    let job = &JOB[slot];
    let waiting = &WAITING[slot];
    loop {
        waiting.store(1, Ordering::Release);
        let value = job.load(Ordering::Acquire);
        if value == 0 {
            let _ = crate::thread::futex_wait(job, 0);
            continue;
        }
        waiting.store(0, Ordering::Release);
        if job
            .compare_exchange(value, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        serve((value - 1) as i64);
        let _ = sys(TCP_CLOSE, (value - 1) as u64, 0, 0, 0, 0, 0);
    }
}

fn handoff(conn: u32) -> bool {
    let mut index = 0;
    while index < HTTP_WORKERS {
        if WAITING[index]
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            JOB[index].store(conn + 1, Ordering::Release);
            let _ = crate::thread::futex_wake(&JOB[index], 1);
            return true;
        }
        index += 1;
    }
    false
}

fn serve(conn: i64) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if buf.len() >= 2048 {
            send_response(conn, &HttpResponse::new(400, "Bad Request"));
            return;
        }
        let mut tmp = [0u8; 512];
        let n = sys(
            TCP_RECV,
            conn as u64,
            tmp.as_mut_ptr() as u64,
            tmp.len() as u64,
            0,
            0,
            0,
        );
        if n == EAGAIN {
            yield_now();
            continue;
        }
        if n <= 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        match ready(&buf) {
            Ready::Wait => continue,
            Ready::Bad => {
                send_response(conn, &HttpResponse::new(400, "Bad Request"));
                return;
            }
            Ready::Request(request) => {
                let response = handle(&request);
                send_response(conn, &response);
                return;
            }
        }
    }
}

enum Ready {
    Wait,
    Bad,
    Request(HttpRequest),
}

fn ready(buf: &[u8]) -> Ready {
    match HttpRequest::parse(buf) {
        Err(ParseError::Incomplete) => Ready::Wait,
        Err(_) => Ready::Bad,
        Ok(request) => {
            if let Some(length) = content_length(&request) {
                if request.body.len() < length && buf.len() < 2048 {
                    return Ready::Wait;
                }
            }
            Ready::Request(request)
        }
    }
}

fn content_length(request: &HttpRequest) -> Option<usize> {
    request
        .get_header("Content-Length")
        .and_then(|value| value.parse().ok())
}

fn handle(request: &HttpRequest) -> HttpResponse {
    if !check_auth(request) {
        return HttpResponse::unauthorized();
    }
    match request.path.as_str() {
        "/" => handle_index(),
        "/status" => handle_status(),
        "/config" => handle_config(request),
        "/records" => handle_records(request),
        "/records/delete" => handle_record_delete(request),
        "/logs" => handle_logs(),
        "/debug" => handle_debug(),
        "/debug/spin" => handle_spin(),
        "/debug/crash/dns" => handle_crash(0, 0),
        "/debug/crash/http" => handle_crash(1, 0),
        "/debug/crash/spin" => handle_crash(2, 0),
        "/debug/crash/log" => handle_crash(3, 0),
        "/debug/crash/telnet" => handle_crash(4, 0),
        "/debug/crash/telnet-worker" => handle_crash(4, 1),
        "/debug/fault/guard" => {
            let _ = sys(FAULT_GUARD, 0, 0, 0, 0, 0, 0);
            loop {}
        }
        "/debug/fault/text" => {
            let _ = sys(FAULT_TEXT, 0, 0, 0, 0, 0, 0);
            loop {}
        }
        _ => HttpResponse::not_found(),
    }
}

fn handle_logs() -> HttpResponse {
    let mut buf = [0u8; 1024];
    let n = sys(
        LOG_RECENT,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    let text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).unwrap_or("")
    } else {
        ""
    };
    let mut response = HttpResponse::ok(text);
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn handle_spin() -> HttpResponse {
    let _ = sys(DEBUG_SPIN, 0, 0, 0, 0, 0, 0);
    HttpResponse::ok("spinning\n")
}

fn handle_debug() -> HttpResponse {
    let mut buf = [0u8; 1024];
    let n = sys(
        DEBUG_STATUS,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    let text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).unwrap_or("")
    } else {
        ""
    };
    let mut response = HttpResponse::ok(text);
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn handle_crash(slot: u64, worker: u64) -> HttpResponse {
    let rc = sys(DEBUG_CRASH, slot, worker, 0, 0, 0, 0);
    if rc < 0 {
        return HttpResponse::internal_error();
    }
    let mut response = HttpResponse::ok("armed\n");
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn check_auth(request: &HttpRequest) -> bool {
    let Some(header) = request.get_header("Authorization") else {
        return false;
    };
    let Some((user, pass)) = parse_basic_auth(header) else {
        return false;
    };
    sys(
        AUTH,
        user.as_ptr() as u64,
        user.len() as u64,
        pass.as_ptr() as u64,
        pass.len() as u64,
        0,
        0,
    ) == 0
}

fn handle_index() -> HttpResponse {
    let mut rows = String::new();
    for record in load_records() {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>\
<form method=\"POST\" action=\"/records/delete\" style=\"display:inline\">\
<input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\">Remove</button></form>\
</td></tr>",
            record.name, record.kind, record.value, record.ttl, record.name
        ));
    }
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Pi4 DNS Server</title>
    <style>
        body {{ font-family: Arial, sans-serif; margin: 40px; }}
        h1 {{ color: #333; }}
        .status {{ background: #e8f5e9; padding: 10px; border-radius: 5px; }}
        a {{ color: #1976d2; text-decoration: none; }}
        table {{ border-collapse: collapse; }}
        td, th {{ border: 1px solid #ccc; padding: 6px 10px; }}
    </style>
</head>
<body>
    <h1>Raspberry Pi 4 DNS Server</h1>
    <div class="status">
        <p><strong>Status:</strong> Running</p>
        <p><strong>Kernel:</strong> Bare-metal Rust (no_std)</p>
    </div>
    <h2>DNS records</h2>
    <table>
        <tr><th>Name</th><th>Type</th><th>Value</th><th>TTL</th><th></th></tr>
        {rows}
    </table>
    <h2>Add a record</h2>
    <form method="POST" action="/records">
        <input name="name" placeholder="name" required>
        <select name="type"><option>A</option><option>AAAA</option><option>CNAME</option><option>PTR</option></select>
        <input name="value" placeholder="value" required>
        <input name="ttl" value="300">
        <button type="submit">Add record</button>
    </form>
    <h2>Management</h2>
    <ul>
        <li><a href="/status">System Status (JSON)</a></li>
        <li><a href="/config">Configuration</a></li>
        <li><a href="/records">DNS Records (JSON)</a></li>
        <li><a href="/logs">Log</a></li>
    </ul>
</body>
</html>"#
    );
    html_response(&html)
}

fn handle_status() -> HttpResponse {
    let json = format!(
        r#"{{"status":"running","uptime":{},"ticks":{},"tick_hz":{},"dns_port":53,"http_port":80,"cache_entries":{}}}"#,
        sys(UPTIME_SECS, 0, 0, 0, 0, 0, 0),
        sys(2, 0, 0, 0, 0, 0, 0),
        sys(TICK_HZ, 0, 0, 0, 0, 0, 0),
        sys(CACHE_ENTRIES, 0, 0, 0, 0, 0, 0)
    );
    let mut response = HttpResponse::ok(&json);
    response.add_header("Content-Type", "application/json");
    response
}

fn handle_config(request: &HttpRequest) -> HttpResponse {
    match request.method {
        HttpMethod::GET => {
            let (port, user) = http_info();
            let json = format!(r#"{{"http_port":{port},"username":"{user}"}}"#);
            let mut response = HttpResponse::ok(&json);
            response.add_header("Content-Type", "application/json");
            response
        }
        HttpMethod::POST => HttpResponse::ok(r#"{"status":"Configuration update not yet implemented"}"#),
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_records(request: &HttpRequest) -> HttpResponse {
    match request.method {
        HttpMethod::GET => records_json(),
        HttpMethod::POST => add_record_from_body(request),
        HttpMethod::DELETE => delete_record_from_body(request),
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_record_delete(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    delete_record_from_body(request)
}

fn records_json() -> HttpResponse {
    let mut json = String::from("{\"records\":[");
    for (index, record) in load_records().iter().enumerate() {
        if index != 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"name\":\"{}\",\"type\":\"{}\",\"value\":\"{}\",\"ttl\":{}}}",
            record.name, record.kind, record.value, record.ttl
        ));
    }
    json.push_str("]}");
    let mut response = HttpResponse::ok(&json);
    response.add_header("Content-Type", "application/json");
    response
}

fn add_record_from_body(request: &HttpRequest) -> HttpResponse {
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let name = form_value(body, "name").unwrap_or("");
    let record_type = form_value(body, "type").unwrap_or("A");
    let value = form_value(body, "value").unwrap_or("");
    let ttl = form_value(body, "ttl").unwrap_or("300");
    let line = format!("{name} {record_type} {value} {ttl}");
    match sys(CONFIG_ADD, line.as_ptr() as u64, line.len() as u64, 0, 0, 0, 0) {
        0 => html_ok(&format!("Record added: {name}")),
        _ => {
            let mut response = HttpResponse::new(400, "Bad Request");
            response.set_body(b"invalid record");
            response
        }
    }
}

fn delete_record_from_body(request: &HttpRequest) -> HttpResponse {
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let name = form_value(body, "name").unwrap_or("");
    match sys(CONFIG_REMOVE, name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0) {
        0 => html_ok(&format!("Record removed: {name}")),
        1 => HttpResponse::not_found(),
        _ => {
            let mut response = HttpResponse::new(400, "Bad Request");
            response.set_body(b"invalid record");
            response
        }
    }
}

fn html_ok(text: &str) -> HttpResponse {
    let body = format!(
        "<!DOCTYPE html><html><body><p>{text}</p><p><a href=\"/\">Back</a></p></body></html>"
    );
    html_response(&body)
}

fn html_response(body: &str) -> HttpResponse {
    let mut response = HttpResponse::ok(body);
    response.add_header("Content-Type", "text/html; charset=utf-8");
    response
}

fn form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    for pair in body.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name.trim() == key {
                return Some(value.trim());
            }
        }
    }
    None
}

struct Record {
    name: String,
    kind: String,
    value: String,
    ttl: u32,
}

fn load_records() -> Vec<Record> {
    let Some(text) = read_config(CONFIG_RECORDS) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(kind) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        let ttl = parts.next().and_then(|text| text.parse().ok()).unwrap_or(300);
        records.push(Record {
            name: name.into(),
            kind: kind.into(),
            value: value.into(),
            ttl,
        });
    }
    records
}

fn http_info() -> (u16, String) {
    let Some(text) = read_config(HTTP_INFO) else {
        return (80, String::from("admin"));
    };
    let mut parts = text.split_whitespace();
    let port = parts.next().and_then(|text| text.parse().ok()).unwrap_or(80);
    let user = parts.next().unwrap_or("admin").into();
    (port, user)
}

fn read_config(num: u64) -> Option<String> {
    let mut buf = [0u8; 4096];
    let n = sys(num, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n < 0 {
        return None;
    }
    core::str::from_utf8(&buf[..n as usize])
        .ok()
        .map(|text| text.into())
}

fn send_response(conn: i64, response: &HttpResponse) {
    let bytes = response.to_bytes();
    let mut off = 0;
    while off < bytes.len() {
        let end = (off + 2048).min(bytes.len());
        let n = sys(
            TCP_SEND,
            conn as u64,
            bytes[off..end].as_ptr() as u64,
            (end - off) as u64,
            0,
            0,
            0,
        );
        if n > 0 {
            off += n as usize;
        } else if n == EAGAIN {
            yield_now();
        } else {
            return;
        }
    }
}

fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

fn exit(code: u64) -> ! {
    let _ = sys(EXIT, code, 0, 0, 0, 0, 0);
    loop {}
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
        );
    }
    ret as i64
}
