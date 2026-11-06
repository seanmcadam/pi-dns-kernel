mod device;
#[cfg(feature = "pi4")]
mod genet;
mod stack;
#[cfg(not(feature = "pi4"))]
mod virtio;

use alloc::boxed::Box;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use spin::Mutex;

pub use device::{DeviceType, EthernetDevice};
pub use stack::NetworkStack;

struct NetworkState {
    stack: NetworkStack,
    device: EthernetDevice,
    dhcp: Option<DhcpClient>,
}

struct DhcpClient {
    handle: SocketHandle,
    started_ms: u64,
    timeout_ms: u64,
    fallback_ip: [u8; 4],
    fallback_netmask: [u8; 4],
    fallback_gateway: [u8; 4],
    finished: bool,
}

static NETWORK: Mutex<Option<Box<NetworkState>>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
pub enum NetworkMode {
    Static {
        ip: [u8; 4],
        netmask: [u8; 4],
        gateway: [u8; 4],
    },
    /// Ask for an address, then use the fallback if no lease arrives in time.
    Dhcp {
        fallback_ip: [u8; 4],
        fallback_netmask: [u8; 4],
        fallback_gateway: [u8; 4],
        timeout_ms: u64,
    },
}

#[derive(Debug)]
pub enum NetworkError {
    InitFailed,
    DeviceNotFound,
    ConfigError,
}

pub fn init(config: &crate::config_manager::NetworkConfig) -> Result<(), NetworkError> {
    log::info!("Detecting network device...");

    let device_type = device::detect_device_type();
    log::info!("Network device: {:?}", device_type);

    let mut device = match device_type {
        #[cfg(not(feature = "pi4"))]
        DeviceType::Virtio => {
            EthernetDevice::new_virtio().ok_or(NetworkError::DeviceNotFound)?
        }
        #[cfg(feature = "pi4")]
        DeviceType::Virtio => return Err(NetworkError::DeviceNotFound),
        #[cfg(feature = "pi4")]
        DeviceType::Genet => {
            EthernetDevice::new_genet().ok_or(NetworkError::DeviceNotFound)?
        }
        #[cfg(not(feature = "pi4"))]
        DeviceType::Genet => return Err(NetworkError::DeviceNotFound),
    };
    
    let mut stack = NetworkStack::new(&mut device);
    
    match config.mode {
        NetworkMode::Static {
            ip,
            netmask,
            gateway,
        } => {
            stack.configure_static(ip, netmask, gateway);
            log::info!(
                "Network configured with static IP: {}.{}.{}.{}",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
        }
        NetworkMode::Dhcp {
            fallback_ip,
            fallback_netmask,
            fallback_gateway,
            timeout_ms,
        } => {
            log::info!("Starting DHCP");
            let handle = stack.sockets_mut().add(dhcpv4::Socket::new());
            *NETWORK.lock() = Some(Box::new(NetworkState {
                stack,
                device,
                dhcp: Some(DhcpClient {
                    handle,
                    started_ms: crate::timer::millis(),
                    timeout_ms,
                    fallback_ip,
                    fallback_netmask,
                    fallback_gateway,
                    finished: false,
                }),
            }));
            return Ok(());
        }
    }

    *NETWORK.lock() = Some(Box::new(NetworkState {
        stack,
        device,
        dhcp: None,
    }));

    Ok(())
}

pub fn poll() {
    let mut net = NETWORK.lock();
    let Some(state) = net.as_mut() else {
        return;
    };
    let timestamp = Instant::from_millis(crate::timer::millis() as i64);
    state.stack.poll(timestamp, &mut state.device);

    let Some(dhcp) = state.dhcp.as_ref() else {
        return;
    };
    if dhcp.finished {
        return;
    }
    let handle = dhcp.handle;
    let started_ms = dhcp.started_ms;
    let timeout_ms = dhcp.timeout_ms;
    let fallback_ip = dhcp.fallback_ip;
    let fallback_netmask = dhcp.fallback_netmask;
    let fallback_gateway = dhcp.fallback_gateway;

    let action = {
        let socket = state
            .stack
            .sockets_mut()
            .get_mut::<dhcpv4::Socket>(handle);
        match socket.poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                let ip = config.address.address().0;
                let mask = prefix_to_mask(config.address.prefix_len());
                let gateway = config.router.map(|addr| addr.0).unwrap_or(fallback_gateway);
                Some(DhcpAction::Configured { ip, mask, gateway })
            }
            _ if crate::timer::millis().saturating_sub(started_ms) >= timeout_ms => {
                Some(DhcpAction::Timeout)
            }
            _ => None,
        }
    };

    let Some(action) = action else {
        return;
    };
    match action {
        DhcpAction::Configured { ip, mask, gateway } => {
            state.stack.configure_static(ip, mask, gateway);
            log::info!(
                "DHCP configured {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3]
            );
        }
        DhcpAction::Timeout => {
            state.stack.sockets_mut().remove(handle);
            state
                .stack
                .configure_static(fallback_ip, fallback_netmask, fallback_gateway);
            log::info!(
                "DHCP timeout, using static {}.{}.{}.{}",
                fallback_ip[0], fallback_ip[1], fallback_ip[2], fallback_ip[3]
            );
        }
    }
    if let Some(dhcp) = state.dhcp.as_mut() {
        dhcp.finished = true;
    }
}

enum DhcpAction {
    Configured {
        ip: [u8; 4],
        mask: [u8; 4],
        gateway: [u8; 4],
    },
    Timeout,
}

fn prefix_to_mask(prefix: u8) -> [u8; 4] {
    let prefix = prefix.min(32);
    if prefix == 0 {
        return [0; 4];
    }
    let mask = u32::MAX << (32 - prefix);
    mask.to_be_bytes()
}

pub fn packet_counts() -> (u32, u32) {
    #[cfg(not(feature = "pi4"))]
    {
        virtio::packet_counts()
    }
    #[cfg(feature = "pi4")]
    {
        genet::packet_counts()
    }
}

pub fn with_stack<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkStack, &mut EthernetDevice) -> R,
{
    let mut net = NETWORK.lock();
    net.as_mut().map(|state| f(&mut state.stack, &mut state.device))
}
