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
        
        let mut config = Config::new(ethernet_addr.into());
        // A zero seed makes every board pick the same DHCP xid and TCP ISN.
        let mut seed = 0xA5A5_u64;
        for (shift, byte) in mac.iter().enumerate() {
            seed |= (*byte as u64) << (shift * 8);
        }
        config.random_seed = seed;
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

    /// `Interface::poll` answers ARP and ICMPv4 echo for the configured
    /// address. No extra socket is required.
    pub fn poll(&mut self, timestamp: Instant, device: &mut EthernetDevice) {
        self.iface.poll(timestamp, device, &mut self.sockets);
    }

    pub fn poll_delay(&mut self, timestamp: Instant) -> Option<smoltcp::time::Duration> {
        self.iface.poll_delay(timestamp, &self.sockets)
    }

    pub fn sockets_mut(&mut self) -> &mut SocketSet<'static> {
        &mut self.sockets
    }

    pub fn connect_tcp(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
        ip: [u8; 4],
        port: u16,
        local: u16,
    ) -> Result<(), ()> {
        use smoltcp::socket::tcp;
        use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};
        let remote = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(ip)), port);
        let local_ep = IpListenEndpoint {
            addr: None,
            port: local,
        };
        let cx = self.iface.context();
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        socket.connect(cx, remote, local_ep).map_err(|_| ())
    }
}

fn netmask_to_prefix_len(netmask: [u8; 4]) -> u8 {
    let mask = u32::from_be_bytes(netmask);
    mask.leading_ones() as u8
}
