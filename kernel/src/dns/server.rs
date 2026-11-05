use crate::config_manager::DnsConfig;

pub fn start(config: &DnsConfig) {
    log::info!("DNS server initialized");
    log::info!("Local records: {}", config.records.len());
    log::info!("Upstream resolvers: {:?}", config.upstreams);

    // TODO: Create UDP socket on port 53
    // TODO: Start DNS request handler
}
