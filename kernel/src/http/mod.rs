use crate::config_manager::HttpConfig;

pub fn init(config: &HttpConfig) {
    log::info!("HTTP server initialized on port {}", config.port);
}
