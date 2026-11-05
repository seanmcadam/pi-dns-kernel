mod server;

use crate::config_manager::DnsConfig;

pub fn init(config: &DnsConfig) {
    server::start(config);
}

pub fn init_sockets(stack: &mut crate::net::NetworkStack) -> Result<(), &'static str> {
    server::init_sockets(stack)
}

pub fn poll(stack: &mut crate::net::NetworkStack, device: &mut crate::net::EthernetDevice) {
    server::poll(stack, device);
}
