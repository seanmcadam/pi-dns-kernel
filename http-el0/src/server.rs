//! Web UI. Same routes as the old kernel HTTP loop. Records, upstreams,
//! the blocklist, and the login check go through syscalls, so a change is
//! visible to the DNS task on its next lookup and is saved by the kernel.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use http_server::{parse_basic_auth, HttpMethod, HttpRequest, HttpResponse, ParseError};

#[path = "firmware.rs"]
mod firmware;

const HTTP_WORKERS: usize = crate::WORKERS;
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
const LOG_RELOAD: u64 = 45;
const CONFIG_READ: u64 = 46;
const CONFIG_WRITE: u64 = 47;
const LOG_FILL: u64 = 48;
const CONFIG_SET_CACHE: u64 = 49;
const CACHE_PLANT: u64 = 54;
const DISPLAY_IO: u64 = 55;
const DISPLAY_SET: u64 = 56;
const POWER: u64 = 58;
const VNC_CTRL: u64 = 59;
const UPSTREAM_HEALTH: u64 = 61;
const NET_COUNTERS: u64 = 64;
const NET_EXTRA: u64 = 66;
const SD_STATUS: u64 = 68;

const EAGAIN: i64 = -11;

pub fn run() -> ! {
    let listen = sys(TCP_LISTEN, 80, 0, 0, 0, 0, 0);
    if listen < 0 {
        panic_rt::note_text(b"http listen failed");
        exit(1);
    }
    let mut worker = 0;
    while worker < HTTP_WORKERS {
        let top = (crate::HEAP_VA + crate::HEAP_SIZE
            - crate::PAGE
            - worker * (crate::STACK_BYTES + crate::PAGE)) as u64;
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
        // Firmware chunks are raw 16 KiB plus headers. Every other body
        // stays at 4096, which `ready` enforces once the path is known.
        if buf.len() >= 18 * 1024 {
            let mut response = HttpResponse::new(400, "Bad Request");
            response.set_body(b"upload exceeds 4096 bytes");
            send_response(conn, &response);
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
            Ready::Oversize => {
                let mut response = HttpResponse::new(400, "Bad Request");
                response.set_body(b"upload exceeds 4096 bytes");
                send_response(conn, &response);
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
    Oversize,
    Request(HttpRequest),
}

fn ready(buf: &[u8]) -> Ready {
    match HttpRequest::parse(buf) {
        Err(ParseError::Incomplete) => Ready::Wait,
        Err(_) => Ready::Bad,
        Ok(request) => {
            if let Some(length) = content_length(&request) {
                let max = if request.path == "/firmware/chunk" {
                    16 * 1024
                } else {
                    4096
                };
                if length > max || request.body.len() > max {
                    return Ready::Oversize;
                }
                if request.body.len() < length {
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
    if request.path == "/firmware" || request.path.starts_with("/firmware/") {
        return firmware::handle(request);
    }
    match request.path.as_str() {
        "/" => handle_index(),
        "/status" => handle_status(),
        "/config" => handle_config(request),
        "/records" => handle_records(request),
        "/records/delete" => handle_record_delete(request),
        "/logs" => handle_logs(),
        "/log-config" => handle_log_config(request),
        "/dns-config" => handle_dns_config(request),
        "/display" => handle_display(request),
        "/vnc" => handle_vnc(request),
        "/power" => handle_power(request),
        "/config/file/config" => handle_config_file(request, 0),
        "/config/file/log" => handle_config_file(request, 1),
        "/debug/log-fill" => handle_log_fill(request),
        "/debug/cache-plant" => handle_cache_plant(request),
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

fn handle_log_config(request: &HttpRequest) -> HttpResponse {
    match request.method {
        HttpMethod::GET => log_config_page(),
        HttpMethod::POST => {
            let body = core::str::from_utf8(&request.body).unwrap_or("");
            let max = form_value(body, "max_bytes").unwrap_or("");
            let archives = form_value(body, "archives").unwrap_or("");
            let json = format!(
                "{{\"version\":1,\"max_bytes\":{max},\"archives\":{archives}}}"
            );
            store_config(1, &json)
        }
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn log_config_page() -> HttpResponse {
    let current = read_config_file(1).unwrap_or_default();
    let html = format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>Log configuration</h1>
<p>File <code>LOG.JSN</code>. Schema version 1.</p>
<p>max_bytes 4096..=65536. archives 0..=16. max_bytes * (archives + 1) must be &lt;= 1048576.</p>
<p>Current: {current}</p>
<form method="POST" action="/log-config">
<label>max_bytes <input name="max_bytes" required></label>
<label>archives <input name="archives" required></label>
<button type="submit">Save</button>
</form>
<p><a href="/config/file/log">Download LOG.JSN</a></p>
<p><a href="/config/file/config">Download CONFIG.JSN</a></p>
</body></html>"#
    );
    html_response(&html)
}

fn handle_dns_config(request: &HttpRequest) -> HttpResponse {
    match request.method {
        HttpMethod::GET => dns_config_page(),
        HttpMethod::POST => {
            let body = core::str::from_utf8(&request.body).unwrap_or("");
            let slots = form_value(body, "cache_size")
                .and_then(|text| text.parse().ok())
                .unwrap_or(0u64);
            let reap = form_value(body, "cache_reap_interval_secs")
                .and_then(|text| text.parse().ok())
                .unwrap_or(0u64);
            let mut err = [0u8; 240];
            let rc = sys(
                CONFIG_SET_CACHE,
                slots,
                reap,
                err.as_mut_ptr() as u64,
                err.len() as u64,
                0,
                0,
            );
            if rc < 0 {
                let end = err.iter().position(|byte| *byte == 0).unwrap_or(err.len());
                let message = core::str::from_utf8(&err[..end]).unwrap_or("rejected");
                return bad_json(message);
            }
            let body = if rc == 1 {
                "{\"status\":\"applied\",\"restart\":true}\n"
            } else {
                "{\"status\":\"applied\",\"restart\":false}\n"
            };
            let mut response = HttpResponse::ok(body);
            response.add_header("Content-Type", "application/json");
            response
        }
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn dns_config_page() -> HttpResponse {
    let current = read_config_file(0).unwrap_or_default();
    let html = format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>DNS cache</h1>
<p>File <code>CONFIG.JSN</code>, field <code>dns</code>. Schema version 1.</p>
<p>cache_size 1..=1024. Each slot reserves 800 bytes for a 255-byte name and a 512-byte response. 1024 slots reserve 823296 bytes, which fits the DNS task heap.</p>
<p>cache_reap_interval_secs 1..=3600. Default 5. The reaper returns expired slots to the free list. Eviction, when the free list is empty, drops the least recently used live entry.</p>
<p>cache_size is reserved when dns-el0 starts. A change reports restart. The reap interval is read on each pass.</p>
<p>Current: {current}</p>
<form method="POST" action="/dns-config">
<label>cache_size <input name="cache_size" required></label>
<label>cache_reap_interval_secs <input name="cache_reap_interval_secs" required></label>
<button type="submit">Save</button>
</form>
<p><a href="/config/file/config">Download CONFIG.JSN</a></p>
</body></html>"#
    );
    html_response(&html)
}

fn handle_display(request: &HttpRequest) -> HttpResponse {
    match request.method {
        HttpMethod::GET => display_page(),
        HttpMethod::POST => {
            let body = core::str::from_utf8(&request.body).unwrap_or("");
            let vnc = form_value(body, "vnc")
                .and_then(|text| text.parse().ok())
                .unwrap_or(2u64);
            let port = form_value(body, "vnc_port")
                .and_then(|text| text.parse().ok())
                .unwrap_or(0u64);
            let password = form_value(body, "vnc_password").unwrap_or("");
            let hdmi = form_value(body, "hdmi")
                .and_then(|text| text.parse().ok())
                .unwrap_or(2u64);
            let mut raw = [0u8; 8];
            let n = password.len().min(raw.len());
            raw[..n].copy_from_slice(&password.as_bytes()[..n]);
            let pass_len = if password.len() > 8 { 9 } else { password.len() as u64 };
            let mut err = [0u8; 160];
            let rc = sys(
                DISPLAY_SET,
                vnc,
                port,
                raw.as_ptr() as u64,
                pass_len,
                hdmi,
                err.as_mut_ptr() as u64,
            );
            if rc < 0 {
                let end = err.iter().position(|byte| *byte == 0).unwrap_or(err.len());
                let message = core::str::from_utf8(&err[..end]).unwrap_or("rejected");
                return bad_json(message);
            }
            let present = sys(DISPLAY_IO, 7, 0, 0, 0, 0, 0);
            let hdmi_state = if present == 1 { "queued" } else { "unavailable" };
            let body = format!(
                "{{\"status\":\"applied\",\"vnc\":\"queued\",\"hdmi\":\"{hdmi_state}\"}}\n"
            );
            let mut response = HttpResponse::ok(&body);
            response.add_header("Content-Type", "application/json");
            response
        }
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_vnc(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let op = match form_value(body, "action") {
        Some("start") => 1,
        Some("stop") => 2,
        _ => return bad_request("action must be start or stop"),
    };
    if sys(VNC_CTRL, op, 0, 0, 0, 0, 0) < 0 {
        return bad_request("VNC request was rejected");
    }
    handle_index()
}

fn handle_power(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let action = form_value(body, "action").unwrap_or("");
    let confirm = form_value(body, "confirm").unwrap_or("");
    let op = if action == "shutdown" && confirm == "shutdown" {
        1
    } else if action == "reboot" && confirm == "reboot" {
        2
    } else {
        return bad_request("Confirmation required");
    };
    let rc = sys(POWER, op, 0, 0, 0, 0, 0);
    if rc < 0 {
        return bad_request("Power request was rejected");
    }
    if rc == 1 {
        return html_ok("A power change is already requested.");
    }
    let word = if op == 1 { "Shutdown" } else { "Reboot" };
    html_ok(&format!("{word} requested."))
}

pub(super) fn bad_request(message: &str) -> HttpResponse {
    let mut response = HttpResponse::new(400, "Bad Request");
    response.set_body(message.as_bytes());
    response
}

fn vnc_block() -> String {
    let state = sys(VNC_CTRL, 0, 0, 0, 0, 0, 0);
    let (word, button) = match state {
        1 => (
            "running",
            "<form method=\"POST\" action=\"/vnc\"><button type=\"submit\" name=\"action\" value=\"stop\">Stop</button></form>",
        ),
        2 => ("starting", "<button type=\"button\" disabled>Starting</button>"),
        3 => ("stopping", "<button type=\"button\" disabled>Stopping</button>"),
        4 => (
            "failed",
            "<form method=\"POST\" action=\"/vnc\"><p>The VNC process failed.</p><button type=\"submit\" name=\"action\" value=\"start\">Start</button></form>",
        ),
        _ => (
            "off",
            "<form method=\"POST\" action=\"/vnc\"><button type=\"submit\" name=\"action\" value=\"start\">Start</button></form>",
        ),
    };
    format!("<h2>VNC</h2><p>VNC process: {word}</p>{button}")
}

fn power_block() -> String {
    let state = sys(POWER, 0, 0, 0, 0, 0, 0);
    let banner = match state {
        1 => "<p>Shutdown requested.</p>",
        2 => "<p>Reboot requested.</p>",
        3 => "<p>Shutdown failed on this board. The machine is still running.</p>",
        4 => "<p>Reboot failed on this board. The machine is still running.</p>",
        _ => "",
    };
    let forms = if state == 1 || state == 2 {
        ""
    } else {
        "<form method=\"POST\" action=\"/power\" onsubmit=\"return confirm('Shut down this DNS server?');\">\
<label><input type=\"checkbox\" name=\"confirm\" value=\"shutdown\" required> Confirm shutdown</label>\
<button type=\"submit\" name=\"action\" value=\"shutdown\">Shutdown</button></form>\
<form method=\"POST\" action=\"/power\" onsubmit=\"return confirm('Reboot this DNS server?');\">\
<label><input type=\"checkbox\" name=\"confirm\" value=\"reboot\" required> Confirm reboot</label>\
<button type=\"submit\" name=\"action\" value=\"reboot\">Reboot</button></form>"
    };
    format!(
        "<h2>Power</h2><p>Shutdown and reboot act on this machine. They do not affect the computer showing this page.</p>{banner}{forms}"
    )
}

fn display_page() -> HttpResponse {
    let current = read_config_file(0).unwrap_or_default();
    let present = sys(DISPLAY_IO, 7, 0, 0, 0, 0, 0);
    let vnc_block = vnc_block();
    let hdmi = if present == 1 {
        "this board has an HDMI process"
    } else {
        "this board has no HDMI process"
    };
    let html = format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>Display</h1>
<p>VNC is an EL0 RFB 3.8 server. It is off until this form enables it. Default port 5900, bounds 1..=65535. Password 0..=8 printable bytes; empty means no authentication. The console is 640 by 480.</p>
<p>HDMI is a separate process on Pi 4 and Pi 5. {hdmi}. vnc and hdmi are 0 or 1.</p>
{vnc_block}
<p>Current: {current}</p>
<form method="POST" action="/display">
<label>vnc <input name="vnc" required></label>
<label>vnc_port <input name="vnc_port" required></label>
<label>vnc_password <input name="vnc_password"></label>
<label>hdmi <input name="hdmi" required></label>
<button type="submit">Save</button>
</form>
<p><a href="/debug">Supervisor</a></p>
</body></html>"#
    );
    html_response(&html)
}

fn handle_cache_plant(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let ttl = form_value(body, "ttl")
        .and_then(|text| text.parse().ok())
        .unwrap_or(0u64);
    if ttl == 0 || ttl > 5 {
        return bad_json("ttl out of bounds");
    }
    let rc = sys(CACHE_PLANT, 0, ttl, 0, 0, 0, 0);
    if rc < 0 {
        return bad_json("plant rejected");
    }
    let mut response = HttpResponse::ok("planted\n");
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn handle_config_file(request: &HttpRequest, which: u64) -> HttpResponse {
    match request.method {
        HttpMethod::GET => {
            let Some(text) = read_config_file(which) else {
                return HttpResponse::internal_error();
            };
            let mut response = HttpResponse::ok(&text);
            response.add_header("Content-Type", "application/json");
            response
        }
        HttpMethod::POST | HttpMethod::PUT => {
            let Ok(text) = core::str::from_utf8(&request.body) else {
                return bad_json("malformed json");
            };
            store_config(which, text)
        }
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_log_fill(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let count = form_value(body, "n").and_then(|text| text.parse().ok()).unwrap_or(0u64);
    if count == 0 || count > 200 {
        return bad_json("n out of bounds");
    }
    let _ = sys(LOG_FILL, count, 0, 0, 0, 0, 0);
    let mut response = HttpResponse::ok("filled\n");
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn store_config(which: u64, text: &str) -> HttpResponse {
    let mut err = [0u8; 240];
    let rc = sys(
        CONFIG_WRITE,
        which,
        text.as_ptr() as u64,
        text.len() as u64,
        err.as_mut_ptr() as u64,
        err.len() as u64,
        0,
    );
    if rc < 0 {
        let end = err.iter().position(|byte| *byte == 0).unwrap_or(err.len());
        let message = core::str::from_utf8(&err[..end]).unwrap_or("rejected");
        return bad_json(message);
    }
    let body = if rc == 1 {
        "{\"status\":\"applied\",\"restart\":true}\n"
    } else {
        "{\"status\":\"applied\",\"restart\":false}\n"
    };
    let mut response = HttpResponse::ok(body);
    response.add_header("Content-Type", "application/json");
    response
}

fn bad_json(message: &str) -> HttpResponse {
    let body = format!("{{\"field\":\"{message}\"}}");
    let mut response = HttpResponse::new(400, "Bad Request");
    response.set_body(body.as_bytes());
    response.add_header("Content-Type", "application/json");
    response
}

fn read_config_file(which: u64) -> Option<String> {
    let mut buf = [0u8; 8192];
    let n = sys(
        CONFIG_READ,
        which,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
    );
    if n < 0 {
        return None;
    }
    core::str::from_utf8(&buf[..n as usize])
        .ok()
        .map(|text| text.into())
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
    let mut buf = [0u8; 2048];
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
    let vnc_block = vnc_block();
    let power_block = power_block();
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
    {vnc_block}
    {power_block}
    <h2>Management</h2>
    <ul>
        <li><a href="/status">System Status (JSON)</a></li>
        <li><a href="/firmware">Firmware update</a></li>
        <li><a href="/config">Configuration</a></li>
        <li><a href="/records">DNS Records (JSON)</a></li>
        <li><a href="/logs">Log</a></li>
        <li><a href="/log-config">Log configuration</a></li>
        <li><a href="/display">Display</a></li>
        <li><a href="/dns-config">DNS cache</a></li>
        <li><a href="/config/file/config">Download CONFIG.JSN</a></li>
        <li><a href="/config/file/log">Download LOG.JSN</a></li>
    </ul>
</body>
</html>"#
    );
    html_response(&html)
}

fn handle_status() -> HttpResponse {
    let (nic_irqs, rx_packets, tx_packets, net_busy_ms, net_idle_ms) = net_counters();
    let (arp_requests_tx, arp_replies_rx, arp_cache_hits, arp_cache_misses, dns_inflight, upstream_timeouts) =
        net_extra();
    let json = format!(
        r#"{{"status":"running","uptime":{},"ticks":{},"tick_hz":{},"dns_port":53,"http_port":80,"cache_entries":{},"upstreams":{},"nic_irqs":{},"rx_packets":{},"tx_packets":{},"net_busy_ms":{},"net_idle_ms":{},"arp_requests_tx":{},"arp_replies_rx":{},"arp_cache_hits":{},"arp_cache_misses":{},"dns_inflight":{},"upstream_timeouts":{},{},{}}}"#,
        sys(UPTIME_SECS, 0, 0, 0, 0, 0, 0),
        sys(2, 0, 0, 0, 0, 0, 0),
        sys(TICK_HZ, 0, 0, 0, 0, 0, 0),
        sys(CACHE_ENTRIES, 0, 0, 0, 0, 0, 0),
        upstream_status(),
        nic_irqs,
        rx_packets,
        tx_packets,
        net_busy_ms,
        net_idle_ms,
        arp_requests_tx,
        arp_replies_rx,
        arp_cache_hits,
        arp_cache_misses,
        dns_inflight,
        upstream_timeouts,
        firmware::fields(),
        sd_fields()
    );
    let mut response = HttpResponse::ok(&json);
    response.add_header("Content-Type", "application/json");
    response
}

/// nic_irqs, rx_packets, tx_packets, net_busy_ms, net_idle_ms.
/// Zeros if the kernel did not return the 40-byte counter block.
fn net_counters() -> (u64, u64, u64, u64, u64) {
    let mut buf = [0u8; 40];
    let n = sys(
        NET_COUNTERS,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n != 40 {
        return (0, 0, 0, 0, 0);
    }
    let word = |index: usize| {
        let off = index * 8;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[off..off + 8]);
        u64::from_le_bytes(raw)
    };
    (word(0), word(1), word(2), word(3), word(4))
}

/// arp_requests_tx, arp_replies_rx, arp_cache_hits, arp_cache_misses,
/// dns_inflight, upstream_timeouts. Zeros if the kernel did not return
/// the 48-byte block.
fn net_extra() -> (u64, u64, u64, u64, u64, u64) {
    let mut buf = [0u8; 48];
    let n = sys(
        NET_EXTRA,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n != 48 {
        return (0, 0, 0, 0, 0, 0);
    }
    let word = |index: usize| {
        let off = index * 8;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[off..off + 8]);
        u64::from_le_bytes(raw)
    };
    (word(0), word(1), word(2), word(3), word(4), word(5))
}

/// SD probe outcome. The kernel records this before the log task exists,
/// so `/status` is the copy that does not scroll off the HDMI console.
fn sd_fields() -> String {
    let mut buf = [0u8; 64];
    let n = sys(
        SD_STATUS,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n < 56 {
        return String::from(
            "\"sd_state\":\"absent\",\"sd_error\":\"unavailable\",\"sd_blocks\":0,\"fs_mounted\":0",
        );
    }
    let blocks = u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]);
    let mounted = u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]);
    format!(
        "\"sd_state\":\"{}\",\"sd_error\":\"{}\",\"sd_blocks\":{},\"fs_mounted\":{}",
        json_token(&buf[..16]),
        json_token(&buf[16..48]),
        blocks,
        mounted
    )
}

fn json_token(raw: &[u8]) -> String {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    let mut out = String::new();
    for byte in &raw[..end] {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || ch == ' ' || ch == '-' {
            out.push(ch);
        }
    }
    out
}

/// `[{"ip":"a.b.c.d","up":true}, ...]`. Empty until the DNS task publishes.
fn upstream_status() -> String {
    let mut buf = [0u8; 41];
    let n = sys(
        UPSTREAM_HEALTH,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n <= 0 {
        return String::from("[]");
    }
    let n = n as usize;
    let count = buf[0] as usize;
    let mut json = String::from("[");
    let mut i = 0;
    while i < count && 1 + (i + 1) * 5 <= n.min(buf.len()) {
        if i != 0 {
            json.push(',');
        }
        let off = 1 + i * 5;
        let up = if buf[off + 4] != 0 { "true" } else { "false" };
        json.push_str(&format!(
            "{{\"ip\":\"{}.{}.{}.{}\",\"up\":{}}}",
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
            up
        ));
        i += 1;
    }
    json.push(']');
    json
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

pub(super) fn html_ok(text: &str) -> HttpResponse {
    let body = format!(
        "<!DOCTYPE html><html><body><p>{text}</p><p><a href=\"/\">Back</a></p></body></html>"
    );
    html_response(&body)
}

pub(super) fn html_response(body: &str) -> HttpResponse {
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

pub(super) fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
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
