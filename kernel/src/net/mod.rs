mod device;
mod stack;

use alloc::boxed::Box;
use smoltcp::time::Instant;
use spin::Mutex;

pub use device::{DeviceType, EthernetDevice};
pub use stack::NetworkStack;

static NETWORK: Mutex<Option<Box<NetworkStack>>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub enum NetworkMode {
    Static {
        ip: [u8; 4],
        netmask: [u8; 4],
        gateway: [u8; 4],
    },
    Dhcp,
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

    let device = device::create_device(device_type).ok_or(NetworkError::DeviceNotFound)?;

    let mut stack = NetworkStack::new(device);

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
        NetworkMode::Dhcp => {
            log::info!("Starting DHCP...");
            stack.configure_dhcp();
        }
    }

    *NETWORK.lock() = Some(Box::new(stack));

    Ok(())
}

pub fn poll() {
    let mut net = NETWORK.lock();
    if let Some(stack) = net.as_mut() {
        let timestamp = Instant::from_millis(crate::timer::get_ticks() as i64 / 1000);
        stack.poll(timestamp);
    }
}

pub fn with_stack<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkStack) -> R,
{
    let mut net = NETWORK.lock();
    net.as_mut().map(|stack| f(stack.as_mut()))
}
