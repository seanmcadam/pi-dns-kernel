use crate::net::NetworkMode;
use alloc::string::String;
use alloc::{vec, vec::Vec};

#[derive(Debug, Clone)]
pub struct Config {
    pub network: NetworkConfig,
    pub dns: DnsConfig,
    pub http: HttpConfig,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub mode: NetworkMode,
}

#[derive(Debug, Clone)]
pub struct DnsConfig {
    pub records: Vec<DnsRecord>,
    pub upstreams: Vec<[u8; 4]>,
    pub cache_size: usize,
}

#[derive(Debug, Clone)]
pub struct DnsRecord {
    pub name: String,
    pub record_type: RecordType,
    pub value: String,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub enum RecordType {
    A,
    AAAA,
    CNAME,
    PTR,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub port: u16,
    pub username: String,
    pub password: String,
}

pub fn load_config() -> Config {
    // Try to load from filesystem
    match crate::fs::read_config() {
        Ok(data) => {
            log::info!("Loaded config from filesystem");
            // TODO: Parse config
            default_config()
        }
        Err(_) => {
            log::info!("Using default config");
            default_config()
        }
    }
}

fn default_config() -> Config {
    let mut records = Vec::new();
    
    // Add some default test records
    records.push(DnsRecord {
        name: String::from("test.local"),
        record_type: RecordType::A,
        value: String::from("192.168.1.100"),
        ttl: 300,
    });
    
    records.push(DnsRecord {
        name: String::from("pi4.local"),
        record_type: RecordType::A,
        value: String::from("10.0.2.15"),  // QEMU user networking default
        ttl: 300,
    });
    
    records.push(DnsRecord {
        name: String::from("www.local"),
        record_type: RecordType::CNAME,
        value: String::from("pi4.local"),
        ttl: 300,
    });
    
    Config {
        network: NetworkConfig {
            mode: NetworkMode::Static {
                ip: [10, 0, 2, 15],        // QEMU user networking default
                netmask: [255, 255, 255, 0],
                gateway: [10, 0, 2, 2],
            },
        },
        dns: DnsConfig {
            records,
            upstreams: vec![
                [8, 8, 8, 8], // Google DNS
                [1, 1, 1, 1], // Cloudflare DNS
            ],
            cache_size: 1000,
        },
        http: HttpConfig {
            port: 80,
            username: String::from("admin"),
            password: String::from("admin"),
        },
    }
}

pub fn save_config(_config: &Config) -> Result<(), crate::fs::FsError> {
    // TODO: Serialize and save to filesystem
    Err(crate::fs::FsError::NotInitialized)
}
