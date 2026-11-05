use crate::config_manager::HttpConfig;
use alloc::{vec, vec::Vec, string::{String, ToString}, format};
use smoltcp::socket::{tcp, Socket};
use http_server::{HttpRequest, HttpResponse, parse_basic_auth};
use spin::Mutex;

type TcpSocket = smoltcp::socket::tcp::Socket<'static>;
type TcpSocketBuffer = smoltcp::socket::tcp::SocketBuffer<'static>;

static HTTP_CONFIG: Mutex<Option<HttpConfig>> = Mutex::new(None);

pub fn start(config: &HttpConfig) {
    log::info!("HTTP server initialized on port {}", config.port);
    *HTTP_CONFIG.lock() = Some(config.clone());
}

pub fn init_sockets(stack: &mut crate::net::NetworkStack) -> Result<(), &'static str> {
    let config = HTTP_CONFIG.lock();
    let port = config.as_ref().map(|c| c.port).unwrap_or(80);
    drop(config);

    // Several listeners so a connection stuck in TIME-WAIT does not block the next request.
    for _ in 0..4 {
        let rx_buffer = TcpSocketBuffer::new(vec![0u8; 8192]);
        let tx_buffer = TcpSocketBuffer::new(vec![0u8; 8192]);
        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        let http_handle = stack.sockets_mut().add(socket);
        let socket = stack.sockets_mut().get_mut::<TcpSocket>(http_handle);
        socket
            .listen(port)
            .map_err(|_| "Failed to listen on HTTP port")?;
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

    let sockets = stack.sockets_mut();

    for item in sockets.iter_mut() {
        let socket = item.1;
        if let Socket::Tcp(tcp_socket) = socket {
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
}

fn process_http_socket(socket: &mut TcpSocket) {
    if !socket.can_recv() {
        return;
    }
    
    // Read request
    let mut buffer = vec![0u8; 2048];
    match socket.recv_slice(&mut buffer) {
        Ok(size) if size > 0 => {
            log::debug!("Received HTTP request: {} bytes", size);
            
            // Parse HTTP request
            match HttpRequest::parse(&buffer[..size]) {
                Err(http_server::ParseError::Incomplete) => return,
                Ok(request) => {
                    log::info!("{} {}", 
                        match request.method {
                            http_server::HttpMethod::GET => "GET",
                            http_server::HttpMethod::POST => "POST",
                            http_server::HttpMethod::PUT => "PUT",
                            http_server::HttpMethod::DELETE => "DELETE",
                        },
                        request.path
                    );
                    
                    // Handle request
                    let response = handle_request(&request);
                    
                    // Send response
                    if socket.can_send() {
                        let response_bytes = response.to_bytes();
                        if socket.send_slice(&response_bytes).is_ok() {
                            log::debug!("Sent HTTP response: {} bytes", response_bytes.len());
                        }
                    }
                    socket.close();
                }
                Err(e) => {
                    log::warn!("Failed to parse HTTP request: {:?}", e);
                    
                    // Send bad request response
                    let response = HttpResponse::new(400, "Bad Request");
                    if socket.can_send() {
                        socket.send_slice(&response.to_bytes()).ok();
                    }
                    socket.close();
                }
            }
        }
        _ => {}
    }
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
    let html = r#"<!DOCTYPE html>
<html>
<head>
    <title>Pi4 DNS Server</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 40px; }
        h1 { color: #333; }
        .status { background: #e8f5e9; padding: 10px; border-radius: 5px; }
        a { color: #1976d2; text-decoration: none; }
    </style>
</head>
<body>
    <h1>Raspberry Pi 4 DNS Server</h1>
    <div class="status">
        <p><strong>Status:</strong> Running</p>
        <p><strong>Kernel:</strong> Bare-metal Rust (no_std)</p>
    </div>
    <h2>Management</h2>
    <ul>
        <li><a href="/status">System Status (JSON)</a></li>
        <li><a href="/config">Configuration</a></li>
        <li><a href="/records">DNS Records</a></li>
    </ul>
</body>
</html>"#;
    
    let mut response = HttpResponse::ok(html);
    response.add_header("Content-Type", "text/html; charset=utf-8");
    response
}

fn handle_status() -> HttpResponse {
    use alloc::format;
    
    let uptime = crate::timer::get_ticks() / 1_000_000; // seconds
    
    let json = format!(
        r#"{{"status":"running","uptime":{},"dns_port":53,"http_port":80}}"#,
        uptime
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

fn handle_records(_request: &HttpRequest) -> HttpResponse {
    // TODO: Return actual DNS records
    let json = r#"{"records":[],"message":"Record management not yet implemented"}"#;
    let mut response = HttpResponse::ok(json);
    response.add_header("Content-Type", "application/json");
    response
}
