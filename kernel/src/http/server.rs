use crate::config_manager::HttpConfig;

pub fn start(config: &HttpConfig) {
    log::info!("HTTP server initialized on port {}", config.port);

    // TODO: Create TCP socket
    // TODO: Start HTTP request handler
}
