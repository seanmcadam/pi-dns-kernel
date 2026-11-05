mod server;

use crate::config_manager::DnsConfig;

pub fn init(config: &DnsConfig) {
    server::start(config);
}
