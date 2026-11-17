use super::EthernetDevice;
use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};

pub struct NetworkStack {
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl NetworkStack {
    pub fn new(device: &mut EthernetDevice) -> Self {
        let mac = device.mac_address();
        let ethernet_addr = EthernetAddress::from_bytes(&mac);
        
        let config = Config::new(ethernet_addr.into());
        let iface = Interface::new(config, device, Instant::ZERO);
        
        let sockets = SocketSet::new(Vec::new());
        
        Self {
            iface,
            sockets,
        }
    }

    pub fn configure_static(&mut self, ip: [u8; 4], netmask: [u8; 4], gateway: [u8; 4]) {
        let ip_addr = Ipv4Address::from_bytes(&ip);
        let prefix_len = netmask_to_prefix_len(netmask);

        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            addrs.push(IpCidr::new(ip_addr.into(), prefix_len)).ok();
        });

        self.iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::from_bytes(&gateway))
            .ok();
    }

    pub fn poll(&mut self, timestamp: Instant, device: &mut EthernetDevice) {
        self.iface.poll(timestamp, device, &mut self.sockets);
    }

    pub fn poll_delay(&mut self, timestamp: Instant) -> Option<smoltcp::time::Duration> {
        self.iface.poll_delay(timestamp, &self.sockets)
    }

    pub fn sockets_mut(&mut self) -> &mut SocketSet<'static> {
        &mut self.sockets
    }
}

fn netmask_to_prefix_len(netmask: [u8; 4]) -> u8 {
    let mask = u32::from_be_bytes(netmask);
    mask.leading_ones() as u8
}
