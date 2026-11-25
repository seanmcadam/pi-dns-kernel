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
const CSRF: u64 = 74;
const CLOCK: u64 = 76;
const HEAP: u64 = 78;
const NET_ARM: u64 = 75;
const VNC_CTRL: u64 = 59;
const UPSTREAM_HEALTH: u64 = 61;
const NET_COUNTERS: u64 = 64;
const NET_EXTRA: u64 = 66;
const SD_STATUS: u64 = 68;
const HDMI_CONSOLE: u64 = 69;
const VCLOG_READ: u64 = 70;
const CRASH: u64 = 71;
const BOOT: u64 = 72;
const BOARD: u64 = 73;

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
    let (path, query) = http_server::split_target(&request.path);
    if path == "/firmware" || path.starts_with("/firmware/") {
        return firmware::handle(request);
    }
    match path {
        "/" => handle_index(),
        "/status" => {
            if http_server::wants_html(query, request.get_header("Accept")) {
                status_html()
            } else {
                status_json()
            }
        }
        "/json/status" => {
            if http_server::wants_html(query, request.get_header("Accept")) {
                status_json_page()
            } else {
                status_json()
            }
        }
        "/vclog" => handle_vclog(),
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
        "/debug/crash/dns" => handle_crash(request, 0, 0),
        "/debug/crash/http" => handle_crash(request, 1, 0),
        "/debug/crash/spin" => handle_crash(request, 2, 0),
        "/debug/crash/log" => handle_crash(request, 3, 0),
        "/debug/crash/telnet" => handle_crash(request, 4, 0),
        "/debug/crash/block" => handle_crash(request, 7, 0),
        "/blocklist" => blocklist_json(),
        "/debug/crash/telnet-worker" => handle_crash(request, 4, 1),
        "/debug/fault/guard" => {
            let _ = sys(FAULT_GUARD, 0, 0, 0, 0, 0, 0);
            loop {}
        }
        "/debug/fault/text" => {
            let _ = sys(FAULT_TEXT, 0, 0, 0, 0, 0, 0);
            loop {}
        }
        "/debug/crash" => handle_debug_crash(request),
        "/debug/boot" => handle_debug_boot(request),
        "/crash/last" => handle_crash_body(2),
        "/crash/list" => handle_crash_body(3),
        "/crash/clear" => handle_crash_clear(request),
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
    let current = json_block(&read_config_file(1).unwrap_or_default());
    let html = format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>Log configuration</h1>
<p>File <code>LOG.JSN</code>. Schema version 1.</p>
<p>max_bytes 4096..=65536. archives 0..=16. max_bytes * (archives + 1) must be &lt;= 1048576.</p>
<p>Current:</p>
{current}
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
    let current = json_block(&read_config_file(0).unwrap_or_default());
    let html = format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>DNS cache</h1>
<p>File <code>CONFIG.JSN</code>, field <code>dns</code>. Schema version 1.</p>
<p>cache_size 1..=1024. Each slot reserves 800 bytes for a 255-byte name and a 512-byte response. 1024 slots reserve 823296 bytes, which fits the DNS task heap.</p>
<p>cache_reap_interval_secs 1..=3600. Default 5. The reaper returns expired slots to the free list. Eviction, when the free list is empty, drops the least recently used live entry.</p>
<p>cache_size is reserved when dns-el0 starts. A change reports restart. The reap interval is read on each pass.</p>
<p>Current:</p>
{current}
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
    let current = json_block(&read_config_file(0).unwrap_or_default());
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
<p>Current:</p>
{current}
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

fn handle_debug_crash(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("crash") {
        return bad_request("confirm=crash required\n");
    }
    let kind = match form_value(body, "kind").unwrap_or("") {
        "abort" | "data-abort" | "data abort" => 1,
        "panic" => 2,
        "hang" => 3,
        _ => return bad_request("kind must be abort, panic, or hang\n"),
    };
    let rc = sys(CRASH, 0, kind, 0, 0, 0, 0);
    if rc < 0 {
        return HttpResponse::internal_error();
    }
    let mut response = HttpResponse::ok("crashing\n");
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn handle_crash_clear(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("clear") {
        return bad_request("confirm=clear required\n");
    }
    let rc = sys(CRASH, 4, 0, 0, 0, 0, 0);
    if rc < 0 {
        return bad_request("could not clear crash logs\n");
    }
    html_ok("Crash logs cleared.")
}

fn handle_crash_body(op: u64) -> HttpResponse {
    let mut buf = [0u8; 4096];
    let n = sys(CRASH, op, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    let text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).unwrap_or("none\n")
    } else {
        "none\n"
    };
    let mut response = HttpResponse::ok(text);
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn json_text(raw: &[u8]) -> String {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    let mut out = String::new();
    for byte in &raw[..end] {
        match *byte {
            b'\n' | b'\r' => out.push(' '),
            b'\\' | b'"' => {
                out.push('\\');
                out.push(*byte as char);
            }
            0x20..=0x7e => out.push(*byte as char),
            _ => {}
        }
    }
    out
}

fn crash_token(raw: &[u8]) -> String {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    let mut out = String::new();
    for byte in &raw[..end] {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || ch == ' ' || ch == '-' || ch == '.' || ch == '=' || ch == ':' {
            out.push(ch);
        }
    }
    out
}

fn crash_fields() -> String {
    let mut buf = [0u8; 128];
    let n = sys(CRASH, 1, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    if n < 128 {
        return String::from("\"crash_count\":0,\"safe_mode\":0,\"last_crash\":\"\"");
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let safe = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let uptime = u64::from_le_bytes([
        buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
    ]);
    let file = crash_token(&buf[16..32]);
    let summary = json_text(&buf[32..128]);
    let last = if file.is_empty() {
        String::new()
    } else {
        format!("{file} {uptime}ms {summary}")
    };
    format!("\"crash_count\":{count},\"safe_mode\":{safe},\"last_crash\":\"{last}\"")
}

fn handle_debug_boot(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("boot") {
        return bad_request("confirm=boot required\n");
    }
    let kind = match form_value(body, "kind").unwrap_or("") {
        "hang" => 1,
        "fail" => 2,
        _ => return bad_request("kind must be hang or fail\n"),
    };
    let rc = sys(BOOT, 0, kind, 0, 0, 0, 0);
    if rc < 0 {
        return HttpResponse::internal_error();
    }
    let mut response = HttpResponse::ok("armed\n");
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn board_fields() -> String {
    let mut buf = [0u8; 40];
    let n = sys(BOARD, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n < 40 {
        return String::from("\"board\":\"\",\"board_revision\":\"\",\"board_source\":\"build\"");
    }
    let rev = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let model = crash_token(&buf[4..20]);
    let source = crash_token(&buf[20..36]);
    let revision = if rev == 0 {
        String::new()
    } else {
        format!("{rev:08x}")
    };
    format!("\"board\":\"{model}\",\"board_revision\":\"{revision}\",\"board_source\":\"{source}\"")
}

fn boot_fields() -> String {
    let mut buf = [0u8; 80];
    let n = sys(BOOT, 1, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    if n < 80 {
        return String::from(
            "\"boot_attempts\":0,\"boot_good\":0,\"fallback_active\":0,\"booted_kernel\":\"\",\"boot_stopped\":\"unknown\",\"last_reset\":\"unknown\"",
        );
    }
    let attempts = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let good = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let fallback = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let kernel = crash_token(&buf[20..36]);
    let stopped = crash_token(&buf[36..52]);
    let reset = match u32::from_le_bytes([buf[68], buf[69], buf[70], buf[71]]) {
        1 => "watchdog",
        2 => "reboot",
        3 => "power",
        _ => "unknown",
    };
    format!(
        "\"boot_attempts\":{attempts},\"boot_good\":{good},\"fallback_active\":{fallback},\"booted_kernel\":\"{kernel}\",\"boot_stopped\":\"{stopped}\",\"last_reset\":\"{reset}\""
    )
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

fn handle_crash(request: &HttpRequest, slot: u64, worker: u64) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("crash") {
        return bad_request("confirm=crash required\n");
    }
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
    let version = version::VERSION;
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
        <p><strong>Version:</strong> {version}</p>
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
        <li><a href="/status">System status</a></li>
        <li><a href="/json/status">System status (JSON)</a></li>
        <li><a href="/vclog">Firmware log</a></li>
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

fn status_html() -> HttpResponse {
    let version = version::VERSION;
    let script = http_server::STATUS_SCRIPT;
    let html = format!(
        r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="30">
<title>Status {version}</title>
<style>
body{{font:16px/1.45 sans-serif;margin:1rem;max-width:42rem}}
h1{{font-size:1.35rem;margin:0 0 .4rem}}
h2{{font-size:1.05rem;margin:1rem 0 .3rem}}
dl{{margin:0}} dt{{font-weight:600}} dd{{margin:0 0 .35rem}}
table{{border-collapse:collapse}}
td,th{{border:1px solid #ccc;padding:.15rem .45rem;text-align:left}}
pre{{white-space:pre-wrap;font:14px/1.4 ui-monospace,monospace;background:#f6f6f6;padding:.6rem}}
a{{color:#0645ad}}
</style>
</head><body>
<h1>Status <span id="version">{version}</span></h1>
<p><a href="/">Home</a> · <a href="/config">Config</a> · <a href="/json/status">JSON</a></p>
<h2>System</h2>
<dl>
<dt>Uptime</dt><dd id="uptime">…</dd>
<dt>UTC</dt><dd id="utc">…</dd>
<dt>NTP</dt><dd id="ntp">…</dd>
<dt>Heap</dt><dd id="heap">…</dd>
<dt>Blocker</dt><dd id="block">…</dd>
<dt>Board</dt><dd id="board">…</dd>
</dl>
<h2>Network</h2>
<dl>
<dt>NIC IRQs</dt><dd id="nic_irqs">…</dd>
<dt>RX / TX</dt><dd id="rxtx">…</dd>
<dt>Busy / idle ms</dt><dd id="busy">…</dd>
<dt>Upstreams</dt><dd id="upstreams">…</dd>
</dl>
<h2>DNS</h2>
<dl>
<dt>Cache entries</dt><dd id="cache_entries">…</dd>
<dt>In flight</dt><dd id="dns_inflight">…</dd>
<dt>Upstream timeouts</dt><dd id="upstream_timeouts">…</dd>
</dl>
<h2>SD</h2>
<dl>
<dt>Mode</dt><dd id="sd_mode">…</dd>
<dt>Clock</dt><dd id="sd_clock_hz">…</dd>
<dt>Mounted</dt><dd id="fs_mounted">…</dd>
</dl>
<h2>Boot</h2>
<dl>
<dt>boot_good</dt><dd id="boot_good">…</dd>
<dt>fallback</dt><dd id="fallback_active">…</dd>
<dt>boot_stopped</dt><dd id="boot_stopped">…</dd>
<dt>last_reset</dt><dd id="last_reset">…</dd>
<dt>Kernel</dt><dd id="booted_kernel">…</dd>
</dl>
<h2>Crash</h2>
<dl>
<dt>Count</dt><dd id="crash_count">…</dd>
<dt>Last</dt><dd id="last_crash">…</dd>
</dl>
<p><a href="/crash/list">Crash list</a> · <a href="/crash/last">Last crash</a></p>
<form method="POST" action="/crash/clear" onsubmit="return confirm('Clear crash logs on this machine?');">
<label><input type="checkbox" name="confirm" value="clear" required> Confirm clear</label>
<button type="submit">Clear crash logs</button>
</form>
<h2>JSON</h2>
<pre id="status_json"></pre>
<script>
{script}
function text(id, value) {{
  var el = document.getElementById(id);
  if (el) el.textContent = value;
}}
function fill(d) {{
  text("version", d.version);
  text("uptime", d.uptime + " s");
  text("utc", d.time_synced ? d.utc : "not synced");
  text("ntp", (d.ntp_server || "") + " stratum " + d.stratum + " offset " + d.offset_ms + " last " + (d.last_sync || ""));
  text("heap", d.heap_used + " / " + d.heap_peak + " / " + d.heap_size);
  var lines = (d.block_state || "down") + " domains " + d.block_domains + " blocked " + d.block_queries + " mem " + d.block_used + "/" + d.block_size;
  var urls = d.block_urls || [];
  for (var i = 0; i < urls.length; i++) {{
    var u = urls[i];
    lines += "\\n" + u.url + " entries " + u.entries + " mtime " + u.mtime + " next " + u.next;
    if (u.error) lines += " " + u.error;
  }}
  text("block", lines);
  text("board", d.board + " " + d.board_revision + " " + d.board_source);
  text("nic_irqs", d.nic_irqs);
  text("rxtx", d.rx_packets + " / " + d.tx_packets);
  text("busy", d.net_busy_ms + " / " + d.net_idle_ms);
  renderUpstreams(d.upstreams);
  text("cache_entries", d.cache_entries);
  text("dns_inflight", d.dns_inflight);
  text("upstream_timeouts", d.upstream_timeouts);
  text("sd_mode", d.sd_mode);
  text("sd_clock_hz", d.sd_clock_hz);
  text("fs_mounted", d.fs_mounted);
  text("boot_good", d.boot_good);
  text("fallback_active", d.fallback_active);
  text("boot_stopped", d.boot_stopped);
  text("last_reset", d.last_reset);
  text("booted_kernel", d.booted_kernel);
  text("crash_count", d.crash_count);
  text("last_crash", d.last_crash);
  prettyStatus(d);
}}
function load() {{
  fetch("/json/status", {{headers: {{Accept: "application/json"}}}})
    .then(function(res) {{ return res.json(); }})
    .then(fill);
}}
load();
setInterval(load, 5000);
</script>
</body></html>"#
    );
    html_response(&html)
}

fn block_stats() -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let n = sys(80, 4, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    if n < 48 {
        return None;
    }
    Some(buf[..n as usize].to_vec())
}

fn json_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match byte {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(*byte as char);
            }
            0x20..=0x7e => out.push(*byte as char),
            _ => {}
        }
    }
    out
}

fn block_fields() -> String {
    let Some(buf) = block_stats() else {
        return String::from(
            r#""block_state":"down","block_domains":0,"block_queries":0,"block_used":0,"block_size":0,"block_urls":[]"#,
        );
    };
    block_json(&buf, true)
}

fn blocklist_json() -> HttpResponse {
    let body = match block_stats() {
        Some(buf) => block_json(&buf, false),
        None => String::from(
            r#"{"block_state":"down","block_domains":0,"block_queries":0,"block_used":0,"block_size":0,"block_urls":[]}"#,
        ),
    };
    let mut response = HttpResponse::ok(&body);
    response.add_header("Content-Type", "application/json");
    response
}

fn block_json(buf: &[u8], fragment: bool) -> String {
    let state = match buf[0] {
        1 => "loading",
        2 => "up",
        _ => "down",
    };
    let domains = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8]));
    let queries = u64::from_le_bytes(buf[16..24].try_into().unwrap_or([0; 8]));
    let used = u64::from_le_bytes(buf[24..32].try_into().unwrap_or([0; 8]));
    let size = u64::from_le_bytes(buf[32..40].try_into().unwrap_or([0; 8]));
    let count = buf[40] as usize;
    let mut urls = String::from("[");
    let mut n = 48;
    let mut seen = 0;
    while seen < count && n + 36 <= buf.len() {
        let name = json_escape(&buf[n..n + 12]);
        let ul = buf[n + 12] as usize;
        let el = buf[n + 13] as usize;
        let entries = u32::from_le_bytes(buf[n + 16..n + 20].try_into().unwrap_or([0; 4]));
        let mtime = i64::from_le_bytes(buf[n + 20..n + 28].try_into().unwrap_or([0; 8]));
        let next = i64::from_le_bytes(buf[n + 28..n + 36].try_into().unwrap_or([0; 8]));
        n += 36;
        if n + ul + el > buf.len() {
            break;
        }
        let url = json_escape(&buf[n..n + ul]);
        n += ul;
        let err = json_escape(&buf[n..n + el]);
        n += el;
        if seen > 0 {
            urls.push(',');
        }
        urls.push_str(&format!(
            r#"{{"name":"{}","url":"{}","entries":{},"mtime":{},"next":{},"error":"{}"}}"#,
            name.trim_end_matches('\u{0}'),
            url,
            entries,
            mtime,
            next,
            err
        ));
        seen += 1;
    }
    urls.push(']');
    let body = format!(
        r#""block_state":"{}","block_domains":{},"block_queries":{},"block_used":{},"block_size":{},"block_urls":{}"#,
        state, domains, queries, used, size, urls
    );
    if fragment {
        body
    } else {
        format!("{{{body}}}")
    }
}

fn heap_fields() -> String {
    let mut buf = [0u8; 24];
    let n = sys(HEAP, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n != 24 {
        return String::from(r#""heap_used":0,"heap_peak":0,"heap_size":0"#);
    }
    let used = u64::from_le_bytes(buf[..8].try_into().unwrap_or([0; 8]));
    let peak = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8]));
    let size = u64::from_le_bytes(buf[16..24].try_into().unwrap_or([0; 8]));
    format!(
        r#""heap_used":{},"heap_peak":{},"heap_size":{}"#,
        used, peak, size
    )
}

fn time_fields() -> String {
    let mut buf = [0u8; 100];
    let n = sys(CLOCK, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n != 100 {
        return String::from(
            r#""time_synced":0,"utc":"","last_sync":"","ntp_server":"","offset_ms":0,"stratum":0"#,
        );
    }
    let synced = u64::from(buf[0]);
    let stratum = u64::from(buf[1]);
    let offset = i64::from_le_bytes(buf[4..12].try_into().unwrap_or([0; 8]));
    let utc = text_field(&buf[12..32]);
    let last = text_field(&buf[32..52]);
    let server = text_field(&buf[52..100]);
    format!(
        r#""time_synced":{},"utc":"{}","last_sync":"{}","ntp_server":"{}","offset_ms":{},"stratum":{}"#,
        synced, utc, last, server, offset, stratum
    )
}

fn status_json() -> HttpResponse {
    let json = status_document();
    let mut response = HttpResponse::ok(&json);
    response.add_header("Content-Type", "application/json");
    response
}

fn status_document() -> String {
    let (nic_irqs, rx_packets, tx_packets, net_busy_ms, net_idle_ms) = net_counters();
    let (
        arp_requests_tx,
        arp_replies_rx,
        arp_cache_hits,
        arp_cache_misses,
        dns_inflight,
        upstream_timeouts,
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
    ) = net_extra();
    let (
        hdmi_state,
        hdmi_mode,
        hdmi_recoveries,
        hdmi_diag,
        hdmi_last_shut,
        hdmi_regs,
        hdmi_port,
        vclog,
        vclog_bytes,
        vclog_status,
    ) = hdmi_status();
    format!(
        r#"{{"status":"running","uptime":{},"ticks":{},"tick_hz":{},"dns_port":53,"http_port":80,"cache_entries":{},"upstreams":{},"nic_irqs":{},"rx_packets":{},"tx_packets":{},"net_busy_ms":{},"net_idle_ms":{},"arp_requests_tx":{},"arp_replies_rx":{},"arp_cache_hits":{},"arp_cache_misses":{},"dns_inflight":{},"upstream_timeouts":{},"rx_drops":{},"rx_overruns":{},"rx_queued":{},"forward_full":{},"reply_drops":{},"poll_count":{},"poll_us":{},"poll_max_us":{},"irq_rearms":{},"tx_full":{},"tx_slow":{},"hdmi_state":"{}","hdmi_mode":"{}","hdmi_recoveries":{},"hdmi_diag":"{}","hdmi_last_shut":"{}","hdmi_regs":"{}","hdmi_port":"{}","vclog":"{}","vclog_bytes":{},"vclog_status":"{}",{},{},{},{},{},{},{},{}}}"#,
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
        hdmi_state,
        hdmi_mode,
        hdmi_recoveries,
        hdmi_diag,
        hdmi_last_shut,
        hdmi_regs,
        hdmi_port,
        vclog,
        vclog_bytes,
        vclog_status,
        firmware::fields(),
        sd_fields(),
        crash_fields(),
        boot_fields(),
        board_fields(),
        time_fields(),
        heap_fields(),
        block_fields(),
    )
}

/// Browser view of `/json/status`. The JSON API stays one compact line.
fn status_json_page() -> HttpResponse {
    let safe = status_document().replace('<', "\\u003c");
    let html = format!(
        r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Status JSON</title>
<style>body{{font:16px/1.45 sans-serif;margin:1rem}} pre{{white-space:pre-wrap;font:14px/1.4 ui-monospace,monospace}}</style>
</head><body>
<pre id="doc"></pre>
<script type="application/json" id="src">{safe}</script>
<script>
var raw = document.getElementById("src").textContent;
document.getElementById("doc").textContent = JSON.stringify(JSON.parse(raw), null, 2);
</script>
</body></html>"#
    );
    html_response(&html)
}

/// `hdmi_state` is none, up, fallback, or released. `hdmi_mode` is `WxH`.
/// `hdmi_diag` is this boot's mailbox summary. `hdmi_last_shut` is the
/// previous reboot's pre-write register dump. `hdmi_regs` is this boot's
/// read-only dump. `hdmi_port` is `hdmi0`, `hdmi1`, `hdmi0+hdmi1`, or
/// `none`. `vclog` is the first firmware-log line. `vclog_bytes` is the
/// rendered log `GET /vclog` returns. `vclog_status` says whether that
/// copy wrapped or was truncated, and the `/chosen/log` size. Virt and
/// Pi 5 report none, 0x0, zero recoveries, and `none`.
fn hdmi_status() -> (String, String, u64, String, String, String, String, String, u64, String) {
    let mut buf = [0u8; 836];
    let n = sys(
        HDMI_CONSOLE,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n != 836 {
        return (
            String::from("none"),
            String::from("0x0"),
            0,
            String::from("none"),
            String::from("none"),
            String::from("none"),
            String::from("none"),
            String::from("none"),
            0,
            String::from("none"),
        );
    }
    let word = |index: usize| {
        let off = index * 4;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&buf[off..off + 4]);
        u32::from_le_bytes(raw)
    };
    let state = match word(0) {
        1 => "up",
        2 => "fallback",
        3 => "released",
        _ => "none",
    };
    let diag = text_field(&buf[16..96]);
    let last = text_field(&buf[96..288]);
    let regs = text_field(&buf[288..480]);
    let port = text_field(&buf[480..496]);
    let vclog = text_field(&buf[496..736]);
    let vclog_bytes = u32::from_le_bytes([buf[736], buf[737], buf[738], buf[739]]);
    let vclog_status = text_field(&buf[740..836]);
    (
        String::from(state),
        format!("{}x{}", word(1), word(2)),
        u64::from(word(3)),
        diag,
        last,
        regs,
        port,
        vclog,
        u64::from(vclog_bytes),
        vclog_status,
    )
}

fn handle_vclog() -> HttpResponse {
    let mut body = Vec::new();
    let mut off = 0u64;
    loop {
        let mut buf = [0u8; 2048];
        let n = sys(
            VCLOG_READ,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            off,
            0,
            0,
            0,
        );
        if n <= 0 {
            break;
        }
        let n = n as usize;
        if n > buf.len() {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        off += n as u64;
        if body.len() > 256 * 1024 {
            break;
        }
    }
    if body.is_empty() {
        let (_, _, _, _, _, _, _, vclog, _, _) = hdmi_status();
        body.extend_from_slice(vclog.as_bytes());
        if !vclog.ends_with('\n') {
            body.push(b'\n');
        }
    }
    let mut response = HttpResponse::new(200, "OK");
    response.set_body(&body);
    response.add_header("Content-Type", "text/plain; charset=utf-8");
    response
}

fn text_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from(core::str::from_utf8(&bytes[..end]).unwrap_or("none"))
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
/// dns_inflight, upstream_timeouts, rx_drops, rx_overruns, rx_queued,
/// forward_full, reply_drops, poll_count, poll_us, poll_max_us,
/// irq_rearms, tx_full, tx_slow. Zeros if the kernel did not return the
/// 136-byte block. `dns_inflight` is the forward-table depth.
fn net_extra() -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
    let mut buf = [0u8; 136];
    let n = sys(
        NET_EXTRA,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    if n != 136 {
        return (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    }
    let word = |index: usize| {
        let off = index * 8;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[off..off + 8]);
        u64::from_le_bytes(raw)
    };
    (
        word(0), word(1), word(2), word(3), word(4), word(5), word(6), word(7), word(8), word(9),
        word(10), word(11), word(12), word(13), word(14), word(15), word(16),
    )
}

/// SD probe outcome. The kernel records this before the log task exists,
/// so `/status` is the copy that does not scroll off the HDMI console.
fn sd_fields() -> String {
    let mut buf = [0u8; 80];
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
            "\"sd_state\":\"absent\",\"sd_error\":\"unavailable\",\"sd_blocks\":0,\"fs_mounted\":0,\"sd_mode\":\"none\",\"sd_clock_hz\":0",
        );
    }
    let blocks = u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]);
    let mounted = u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]);
    let mode = if n >= 76 {
        let mode = json_token(&buf[56..72]);
        if mode.is_empty() { String::from("none") } else { mode }
    } else {
        String::from("none")
    };
    let clock_hz = if n >= 76 {
        u32::from_le_bytes([buf[72], buf[73], buf[74], buf[75]])
    } else {
        0
    };
    format!(
        "\"sd_state\":\"{}\",\"sd_error\":\"{}\",\"sd_blocks\":{},\"fs_mounted\":{},\"sd_mode\":\"{}\",\"sd_clock_hz\":{}",
        json_token(&buf[..16]),
        json_token(&buf[16..48]),
        blocks,
        mounted,
        mode,
        clock_hz
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
        HttpMethod::GET => config_page(),
        HttpMethod::POST => config_save(request),
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn csrf_token() -> String {
    let mut buf = [0u8; 16];
    let n = sys(CSRF, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n != 16 {
        return String::new();
    }
    String::from(core::str::from_utf8(&buf).unwrap_or(""))
}

fn json_block(text: &str) -> String {
    format!("<pre>{}</pre>", html_escape(&config::indent_json(text)))
}

fn html_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn config_page() -> HttpResponse {
    let current = read_config_file(0).unwrap_or_else(|| String::from("{}"));
    let token = csrf_token();
    let escaped = html_escape(&current);
    let html = format!(
        r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Config</title>
<style>
body{{font:16px/1.4 sans-serif;margin:1rem;max-width:42rem}}
label{{display:block;margin:.4rem 0}} input,select,textarea{{width:100%;box-sizing:border-box}}
textarea{{min-height:8rem;font-family:monospace}}
button{{margin-top:.6rem;padding:.4rem .8rem}}
</style>
</head><body>
<h1>Configuration</h1>
<p><a href="/">Home</a> · <a href="/status">Status</a> · <a href="/config/file/config">CONFIG.JSN</a></p>
<p id="msg"></p>
<form method="POST" action="/config" id="form">
<input type="hidden" name="csrf" value="{token}">
<label>Network mode <select id="mode" name="mode"><option>dhcp</option><option>static</option></select></label>
<label>IP <input id="ip" name="ip" required></label>
<label>Netmask <input id="netmask" name="netmask" required></label>
<label>Gateway <input id="gateway" name="gateway" required></label>
<label>Timeout ms (100..=60000) <input id="timeout_ms" name="timeout_ms" required></label>
<label>Cache size (1..=1024) <input id="cache_size" name="cache_size" required></label>
<label>Reap seconds (1..=3600) <input id="reap" name="reap" required></label>
<label>Probe seconds (1..=3600) <input id="probe" name="probe" required></label>
<label>Upstreams, one address per line <textarea id="upstreams" name="upstreams"></textarea></label>
<label>Local records, one per line: name type value ttl <textarea id="records" name="records"></textarea></label>
<label>Block list, one name per line <textarea id="blocklist" name="blocklist"></textarea></label>
<label>Block lists enabled (0 or 1) <input id="block_enabled" name="block_enabled" value="0" required></label>
<label>Block list refresh seconds (3600..=604800, default 86400) <input id="block_refresh" name="block_refresh" value="86400" required></label>
<label>Block list URLs, one http(s) URL per line (at most 8) <textarea id="block_urls" name="block_urls"></textarea></label>
<label>Allow list, one domain per line <textarea id="block_allow" name="block_allow"></textarea></label>
<label>board.detect (0 or 1, 1 is the default) <input id="detect" name="detect" value="1" required></label>
<label>HTTP port <input id="port" name="port" required></label>
<label>HTTP user <input id="user" name="user" required></label>
<label>HTTP password <input id="password" name="password" required></label>
<label>NTP servers, one host per line (1..=4) <textarea id="ntp_servers" name="ntp_servers"></textarea></label>
<label>NTP interval seconds (30..=86400) <input id="ntp_interval" name="ntp_interval" required></label>
<label>Answer NTP on the LAN (0 or 1) <input id="ntp_serve" name="ntp_serve" value="1" required></label>
<label><input id="reboot" name="reboot" type="checkbox" value="1" style="width:auto"> Reboot now. Required before a new static address, HTTP port, or cache size is used. A static address that does not come up within 20 seconds is restored from CONFIG.BAK.</label>
<textarea id="json" name="json">{escaped}</textarea>
<button type="submit">Save</button>
</form>
<script type="application/json" id="base">{escaped}</script>
<script>
var base = JSON.parse(document.getElementById("base").textContent);
function val(id) {{ return document.getElementById(id).value; }}
function setv(id, value) {{ document.getElementById(id).value = value; }}
function lines(text) {{
  return text.split(/\n/).map(function(s) {{ return s.trim(); }}).filter(Boolean);
}}
function fill() {{
  var net = base.network || {{}};
  var dns = base.dns || {{}};
  var http = base.http || {{}};
  setv("mode", net.mode || "dhcp");
  setv("ip", net.ip || "");
  setv("netmask", net.netmask || "");
  setv("gateway", net.gateway || "");
  setv("timeout_ms", net.timeout_ms || 3000);
  setv("cache_size", dns.cache_size || 1);
  setv("reap", dns.cache_reap_interval_secs || 5);
  setv("probe", dns.probe_interval_secs || 5);
  setv("upstreams", (dns.upstreams || []).join("\n"));
  setv("blocklist", (dns.blocklist || []).join("\n"));
  var block = base.block || {{}};
  setv("block_enabled", block.enabled === 1 ? "1" : "0");
  setv("block_refresh", block.refresh_secs || 86400);
  setv("block_urls", (block.urls || []).join("\n"));
  setv("block_allow", (block.allow || []).join("\n"));
  setv("records", (dns.records || []).map(function(r) {{
    return r.name + " " + r.type + " " + r.value + " " + r.ttl;
  }}).join("\n"));
  setv("detect", base.board && base.board.detect === 0 ? "0" : "1");
  setv("port", http.port || 80);
  setv("user", http.username || "");
  setv("password", http.password || "");
  var ntp = base.ntp || {{}};
  var servers = ntp.servers || ["0.pool.ntp.org", "1.pool.ntp.org", "2.pool.ntp.org"];
  setv("ntp_servers", servers.join("\n"));
  setv("ntp_interval", ntp.interval_secs || 3600);
  setv("ntp_serve", ntp.serve === 0 ? "0" : "1");
}}
function bounds(n, lo, hi, name) {{
  if (!Number.isInteger(n) || n < lo || n > hi) return name + " out of bounds";
  return "";
}}
function build() {{
  var err = bounds(Number(val("timeout_ms")), 100, 60000, "timeout_ms")
    || bounds(Number(val("cache_size")), 1, 1024, "cache_size")
    || bounds(Number(val("reap")), 1, 3600, "reap")
    || bounds(Number(val("probe")), 1, 3600, "probe")
    || bounds(Number(val("port")), 1, 65535, "port")
    || bounds(Number(val("detect")), 0, 1, "detect")
    || bounds(Number(val("ntp_interval")), 30, 86400, "ntp_interval")
    || bounds(Number(val("ntp_serve")), 0, 1, "ntp_serve")
    || bounds(Number(val("block_enabled")), 0, 1, "block_enabled")
    || bounds(Number(val("block_refresh")), 3600, 604800, "block_refresh")
    || ntpServers(lines(val("ntp_servers")))
    || blockUrls(lines(val("block_urls")))
    || blockAllow(lines(val("block_allow")));
  if (val("mode") !== "dhcp" && val("mode") !== "static") err = err || "mode";
  if (err) return err;
  base.network.mode = val("mode");
  base.network.ip = val("ip").trim();
  base.network.netmask = val("netmask").trim();
  base.network.gateway = val("gateway").trim();
  base.network.timeout_ms = Number(val("timeout_ms"));
  base.dns.cache_size = Number(val("cache_size"));
  base.dns.cache_reap_interval_secs = Number(val("reap"));
  base.dns.probe_interval_secs = Number(val("probe"));
  base.dns.upstreams = lines(val("upstreams"));
  base.dns.blocklist = lines(val("blocklist"));
  base.dns.records = lines(val("records")).map(function(line) {{
    var p = line.split(/\s+/);
    return {{name:p[0], type:p[1], value:p[2], ttl:Number(p[3] || 300)}};
  }});
  base.http.port = Number(val("port"));
  base.http.username = val("user");
  base.http.password = val("password");
  if (Number(val("detect")) === 0) base.board = {{detect:0}};
  else delete base.board;
  base.ntp = {{
    servers: lines(val("ntp_servers")),
    interval_secs: Number(val("ntp_interval")),
    serve: Number(val("ntp_serve"))
  }};
  var urls = lines(val("block_urls"));
  var allow = lines(val("block_allow"));
  var enabled = Number(val("block_enabled"));
  var refresh = Number(val("block_refresh"));
  if (enabled === 0 && refresh === 86400 && urls.length === 0 && allow.length === 0) delete base.block;
  else base.block = {{enabled: enabled, refresh_secs: refresh, urls: urls, allow: allow}};
  return "";
}}
function blockUrls(list) {{
  if (list.length > 8) return "block urls";
  for (var i = 0; i < list.length; i++) {{
    var url = list[i];
    if (url.length < 8 || url.length > 160) return "block url";
    if (url.indexOf("@") >= 0 || url.indexOf(" ") >= 0) return "block url";
    if (!/^https?:\/\/[A-Za-z0-9.-]+(:[0-9]{{1,5}})?(\/[A-Za-z0-9._~/?=&%+-]*)?$/.test(url)) return "block url";
    var port = url.replace(/^https?:\/\//, "").split("/")[0].split(":")[1];
    if (port && (Number(port) < 1 || Number(port) > 65535)) return "block url";
  }}
  return "";
}}
function blockAllow(list) {{
  if (list.length > 32) return "block allow";
  for (var i = 0; i < list.length; i++) {{
    if (list[i].length < 1 || list[i].length > 253) return "block allow";
    if (!/^[A-Za-z0-9.-]+$/.test(list[i])) return "block allow";
  }}
  return "";
}}
function ntpServers(list) {{
  if (list.length < 1 || list.length > 4) return "ntp servers";
  for (var i = 0; i < list.length; i++) {{
    if (list[i].length < 1 || list[i].length > 64) return "ntp server";
    if (!/^[A-Za-z0-9.-]+(?::[0-9]{{1,5}})?$/.test(list[i])) return "ntp server";
    var port = list[i].split(":")[1];
    if (port && (Number(port) < 1 || Number(port) > 65535)) return "ntp port";
  }}
  return "";
}}
fill();
document.getElementById("form").addEventListener("submit", function(ev) {{
  var err = build();
  if (err) {{
    ev.preventDefault();
    document.getElementById("msg").textContent = err;
    return;
  }}
  document.getElementById("json").value = JSON.stringify(base);
}});
</script>
</body></html>"#
    );
    html_response(&html)
}

fn config_save(request: &HttpRequest) -> HttpResponse {
    let Ok(body) = core::str::from_utf8(&request.body) else {
        return bad_request("malformed form");
    };
    let token = csrf_token();
    if !config::csrf_matches(&token, body) {
        let mut response = HttpResponse::new(403, "Forbidden");
        response.set_body(b"csrf");
        return response;
    }
    let json = match config::posted_json(body) {
        Ok(json) => json,
        Err(err) => return bad_request(&err.text()),
    };
    let mut err = [0u8; 240];
    let rc = sys(
        CONFIG_WRITE,
        0,
        json.as_ptr() as u64,
        json.len() as u64,
        err.as_mut_ptr() as u64,
        err.len() as u64,
        0,
    );
    if rc < 0 {
        let end = err.iter().position(|byte| *byte == 0).unwrap_or(err.len());
        let message = core::str::from_utf8(&err[..end]).unwrap_or("rejected");
        return bad_request(message);
    }
    if rc == 1 && config::reboot_confirmed(body) {
        let _ = sys(NET_ARM, 0, 0, 0, 0, 0, 0);
        let _ = sys(POWER, 2, 0, 0, 0, 0, 0);
        return html_ok(
            "Saved. Reboot requested. A static address that does not come up in 20 seconds is restored from CONFIG.BAK.",
        );
    }
    if rc == 1 {
        return html_ok(
            "Saved. Reboot required before network, port, or cache size changes apply. Not rebooted.",
        );
    }
    html_ok("Saved and applied.")
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

/// Form value with the name and the value trimmed.
///
/// `config::form_value` matches the raw name and does not trim. The trim
/// stays here so `confirm=crash`, `clear`, `promote`, and `reboot` still
/// match spaces around the token. A pair with no `=` is ignored.
pub(super) fn form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    for pair in body.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let raw = config::form_value(body, name).unwrap_or(value);
        return Some(raw.trim());
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
