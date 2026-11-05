mod server;

use crate::config_manager::HttpConfig;

pub fn init(config: &HttpConfig) {
    server::start(config);
}
