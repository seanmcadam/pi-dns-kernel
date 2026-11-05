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
    Config {
        network: NetworkConfig {
            mode: NetworkMode::Static {
                ip: [192, 168, 1, 100],
                netmask: [255, 255, 255, 0],
                gateway: [192, 168, 1, 1],
            },
        },
        dns: DnsConfig {
            records: Vec::new(),
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
