#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

#[derive(Debug, Clone, PartialEq)]
pub struct DnsPacket {
    pub header: Header,
    pub questions: Vec<Question>,
    pub answers: Vec<ResourceRecord>,
    pub authority: Vec<ResourceRecord>,
    pub additional: Vec<ResourceRecord>,
}

impl DnsPacket {
    pub fn new(id: u16) -> Self {
        Self {
            header: Header {
                id,
                qr: false,
                opcode: 0,
                aa: false,
                tc: false,
                rd: true,
                ra: false,
                z: 0,
                rcode: 0,
                qdcount: 0,
                ancount: 0,
                nscount: 0,
                arcount: 0,
            },
            questions: Vec::new(),
            answers: Vec::new(),
            authority: Vec::new(),
            additional: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub id: u16,
    pub qr: bool,
    pub opcode: u8,
    pub aa: bool,
    pub tc: bool,
    pub rd: bool,
    pub ra: bool,
    pub z: u8,
    pub rcode: u8,
    pub qdcount: u16,
    pub ancount: u16,
    pub nscount: u16,
    pub arcount: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub name: String,
    pub qtype: QueryType,
    pub qclass: QueryClass,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRecord {
    pub name: String,
    pub rtype: RecordType,
    pub rclass: RecordClass,
    pub ttl: u32,
    pub rdata: RData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    A = 1,
    NS = 2,
    CNAME = 5,
    SOA = 6,
    PTR = 12,
    MX = 15,
    TXT = 16,
    AAAA = 28,
    ANY = 255,
}

impl QueryType {
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::A),
            2 => Some(Self::NS),
            5 => Some(Self::CNAME),
            6 => Some(Self::SOA),
            12 => Some(Self::PTR),
            15 => Some(Self::MX),
            16 => Some(Self::TXT),
            28 => Some(Self::AAAA),
            255 => Some(Self::ANY),
            _ => None,
        }
    }
}

pub type RecordType = QueryType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    IN = 1,
}

impl QueryClass {
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::IN),
            _ => None,
        }
    }
}

pub type RecordClass = QueryClass;

#[derive(Debug, Clone, PartialEq)]
pub enum RData {
    A([u8; 4]),
    AAAA([u8; 16]),
    CNAME(String),
    PTR(String),
    NS(String),
    MX { priority: u16, exchange: String },
    TXT(String),
    Unknown(Vec<u8>),
}
