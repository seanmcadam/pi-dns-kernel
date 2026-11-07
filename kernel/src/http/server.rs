use crate::config_manager::HttpConfig;
use alloc::{format, string::String, vec};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use http_server::{HttpRequest, HttpResponse, parse_basic_auth};
use spin::Mutex;

type TcpSocket = smoltcp::socket::tcp::Socket<'static>;
type TcpSocketBuffer = smoltcp::socket::tcp::SocketBuffer<'static>;

static HTTP_CONFIG: Mutex<Option<HttpConfig>> = Mutex::new(None);
static mut HTTP_HANDLES: [Option<SocketHandle>; 4] = [None; 4];

pub fn start(config: &HttpConfig) {
    log::info!("HTTP server initialized on port {}", config.port);
    *HTTP_CONFIG.lock() = Some(config.clone());
}

pub fn init_sockets(stack: &mut crate::net::NetworkStack) -> Result<(), &'static str> {
    let config = HTTP_CONFIG.lock();
    let port = config.as_ref().map(|c| c.port).unwrap_or(80);
    drop(config);

    // Several listeners so a connection stuck in TIME-WAIT does not block the next request.
    // Only these handles are HTTP. EL0 DNS sockets live in the same set.
    for index in 0..4 {
        let rx_buffer = TcpSocketBuffer::new(vec![0u8; 8192]);
        let tx_buffer = TcpSocketBuffer::new(vec![0u8; 8192]);
        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        let http_handle = stack.sockets_mut().add(socket);
        let socket = stack.sockets_mut().get_mut::<TcpSocket>(http_handle);
        socket
            .listen(port)
            .map_err(|_| "Failed to listen on HTTP port")?;
        unsafe { HTTP_HANDLES[index] = Some(http_handle) };
    }

    log::info!("HTTP socket listening on port {}", port);
    Ok(())
}

pub fn poll(stack: &mut crate::net::NetworkStack, _device: &mut crate::net::EthernetDevice) {
    let port = HTTP_CONFIG
        .lock()
        .as_ref()
        .map(|c| c.port)
        .unwrap_or(80);

    let handles = unsafe { HTTP_HANDLES };
    let sockets = stack.sockets_mut();

    for handle in handles.into_iter().flatten() {
        let tcp_socket = sockets.get_mut::<TcpSocket>(handle);
        // Recycle sockets that have finished (CLOSED or TIME-WAIT).
        if !tcp_socket.is_open() {
            tcp_socket.listen(port).ok();
        } else if tcp_socket.state() == tcp::State::FinWait2 {
            tcp_socket.abort();
            tcp_socket.listen(port).ok();
        } else if tcp_socket.is_active() && tcp_socket.may_recv() {
            process_http_socket(tcp_socket);
        }
    }
}

fn process_http_socket(socket: &mut TcpSocket) {
    let queued = socket.recv_queue();
    if queued == 0 {
        return;
    }
    let peek_len = queued.min(2048);
    let buffered = match socket.peek(peek_len) {
        Ok(data) if !data.is_empty() => data.to_vec(),
        _ => return,
    };

    match HttpRequest::parse(&buffered) {
        Err(http_server::ParseError::Incomplete) => {}
        Ok(request) => {
            if let Some(length) = content_length(&request) {
                if request.body.len() < length && buffered.len() < 2048 {
                    return;
                }
            }
            let mut discard = vec![0u8; buffered.len()];
            socket.recv_slice(&mut discard).ok();
            log::info!(
                "{} {}",
                match request.method {
                    http_server::HttpMethod::GET => "GET",
                    http_server::HttpMethod::POST => "POST",
                    http_server::HttpMethod::PUT => "PUT",
                    http_server::HttpMethod::DELETE => "DELETE",
                },
                request.path
            );
            let response = handle_request(&request);
            if socket.can_send() {
                socket.send_slice(&response.to_bytes()).ok();
            }
            socket.close();
        }
        Err(err) => {
            log::warn!("Failed to parse HTTP request: {:?}", err);
            let mut discard = vec![0u8; buffered.len()];
            socket.recv_slice(&mut discard).ok();
            let response = HttpResponse::new(400, "Bad Request");
            if socket.can_send() {
                socket.send_slice(&response.to_bytes()).ok();
            }
            socket.close();
        }
    }
}

fn content_length(request: &HttpRequest) -> Option<usize> {
    request
        .get_header("Content-Length")
        .and_then(|value| value.parse().ok())
}

fn handle_request(request: &HttpRequest) -> HttpResponse {
    // Check authentication
    if !check_auth(request) {
        return HttpResponse::unauthorized();
    }
    
    // Route request
    match request.path.as_str() {
        "/" => handle_index(),
        "/status" => handle_status(),
        "/config" => handle_config(request),
        "/records" => handle_records(request),
        "/records/delete" => handle_record_delete(request),
        "/debug/fault/guard" => crate::exception::fault_guard(),
        "/debug/fault/text" => crate::exception::fault_text(),
        _ => HttpResponse::not_found(),
    }
}

fn check_auth(request: &HttpRequest) -> bool {
    let config = HTTP_CONFIG.lock();
    let config = match config.as_ref() {
        Some(c) => c,
        None => return false,
    };
    
    if let Some(auth_header) = request.get_header("Authorization") {
        if let Some((username, password)) = parse_basic_auth(auth_header) {
            return username == config.username && password == config.password;
        }
    }
    
    false
}

fn handle_index() -> HttpResponse {
    let mut rows = String::new();
    for record in crate::config_manager::records() {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>\
<form method=\"POST\" action=\"/records/delete\" style=\"display:inline\">\
<input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\">Remove</button></form>\
</td></tr>",
            record.name,
            record.record_type.as_str(),
            record.value,
            record.ttl,
            record.name
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
    </ul>
</body>
</html>"#
    );
    let mut response = HttpResponse::ok(&html);
    response.add_header("Content-Type", "text/html; charset=utf-8");
    response
}

fn handle_status() -> HttpResponse {
    use alloc::format;
    
    let json = format!(
        r#"{{"status":"running","uptime":{},"ticks":{},"tick_hz":{},"dns_port":53,"http_port":80,"cache_entries":{}}}"#,
        crate::timer::uptime_secs(),
        crate::timer::irq_ticks(),
        crate::timer::TICK_HZ,
        crate::dns::cache_entries()
    );
    
    let mut response = HttpResponse::ok(&json);
    response.add_header("Content-Type", "application/json");
    response
}

fn handle_config(request: &HttpRequest) -> HttpResponse {
    match request.method {
        http_server::HttpMethod::GET => {
            let config = HTTP_CONFIG.lock();
            if let Some(config) = config.as_ref() {
                let json = format!(
                    r#"{{"http_port":{},"username":"{}"}}"#,
                    config.port, config.username
                );
                let mut response = HttpResponse::ok(&json);
                response.add_header("Content-Type", "application/json");
                response
            } else {
                HttpResponse::internal_error()
            }
        }
        http_server::HttpMethod::POST => {
            // TODO: Update configuration
            HttpResponse::ok(r#"{"status":"Configuration update not yet implemented"}"#)
        }
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_records(request: &HttpRequest) -> HttpResponse {
    match request.method {
        http_server::HttpMethod::GET => records_json(),
        http_server::HttpMethod::POST => add_record_from_body(request),
        http_server::HttpMethod::DELETE => delete_record_from_body(request),
        _ => HttpResponse::new(405, "Method Not Allowed"),
    }
}

fn handle_record_delete(request: &HttpRequest) -> HttpResponse {
    if request.method != http_server::HttpMethod::POST {
        return HttpResponse::new(405, "Method Not Allowed");
    }
    delete_record_from_body(request)
}

fn records_json() -> HttpResponse {
    let mut json = String::from("{\"records\":[");
    for (index, record) in crate::config_manager::records().iter().enumerate() {
        if index != 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"name\":\"{}\",\"type\":\"{}\",\"value\":\"{}\",\"ttl\":{}}}",
            record.name,
            record.record_type.as_str(),
            record.value,
            record.ttl
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
    let ttl = form_value(body, "ttl")
        .and_then(|text| text.parse().ok())
        .unwrap_or(300);
    match crate::config_manager::add_record(name, record_type, value, ttl) {
        Ok(()) => html_ok(&format!("Record added: {name}")),
        Err(err) => {
            let mut response = HttpResponse::new(400, "Bad Request");
            response.set_body(err.as_bytes());
            response
        }
    }
}

fn delete_record_from_body(request: &HttpRequest) -> HttpResponse {
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let name = form_value(body, "name").unwrap_or("");
    match crate::config_manager::remove_record(name) {
        Ok(true) => html_ok(&format!("Record removed: {name}")),
        Ok(false) => HttpResponse::not_found(),
        Err(err) => {
            let mut response = HttpResponse::new(400, "Bad Request");
            response.set_body(err.as_bytes());
            response
        }
    }
}

fn html_ok(text: &str) -> HttpResponse {
    let body = format!(
        "<!DOCTYPE html><html><body><p>{text}</p><p><a href=\"/\">Back</a></p></body></html>"
    );
    let mut response = HttpResponse::ok(&body);
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
