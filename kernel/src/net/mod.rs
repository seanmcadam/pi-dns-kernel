mod device;
mod stack;
mod virtio;
mod virtio_new;

use alloc::boxed::Box;
use smoltcp::time::Instant;
use spin::Mutex;

pub use device::{DeviceType, EthernetDevice};
pub use stack::NetworkStack;

struct NetworkState {
    stack: NetworkStack,
    device: EthernetDevice,
}

static NETWORK: Mutex<Option<Box<NetworkState>>> = Mutex::new(None);

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
    
    // TEMPORARY: Skip network init to test if kernel outputs anything
    log::warn!("Network init skipped for testing");
    return Ok(());

    let device_type = device::detect_device_type();
    log::info!("Network device: {:?}", device_type);

    const VIRTIO_MMIO_BASE: usize = 0x0a000000;  // QEMU virt machine

    let mut device = match device_type {
        DeviceType::Virtio => {
            EthernetDevice::new_virtio(VIRTIO_MMIO_BASE).map_err(|e| {
                log::error!("VirtIO init failed: {}", e);
                NetworkError::InitFailed
            })?
        }
        DeviceType::Genet => {
            log::error!("GENET driver not yet implemented");
            return Err(NetworkError::DeviceNotFound);
        }
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
        NetworkMode::Dhcp => {
            log::info!("Starting DHCP...");
            stack.configure_dhcp();
        }
    }
    
    *NETWORK.lock() = Some(Box::new(NetworkState { stack, device }));

    Ok(())
}

pub fn poll() {
    let mut net = NETWORK.lock();
    if let Some(state) = net.as_mut() {
        let timestamp = Instant::from_millis(crate::timer::get_ticks() as i64 / 1000);
        state.stack.poll(timestamp, &mut state.device);
    }
}

pub fn with_stack<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkStack, &mut EthernetDevice) -> R,
{
    let mut net = NETWORK.lock();
    net.as_mut().map(|state| f(&mut state.stack, &mut state.device))
}
