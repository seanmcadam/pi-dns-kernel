use crate::packet::*;

#[cfg(not(feature = "std"))]
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
#[cfg(feature = "std")]
use std::collections::HashMap as BTreeMap;

#[derive(Debug)]
pub enum BuildError {
    TooLarge,
    InvalidData,
}

impl DnsPacket {
    pub fn to_bytes(&self) -> Result<Vec<u8>, BuildError> {
        let mut buffer = Vec::new();
        let mut name_map = BTreeMap::new();

        // Write header
        write_header(&mut buffer, self)?;

        // Write questions
        for question in &self.questions {
            write_question(&mut buffer, question, &mut name_map)?;
        }

        // Write answers
        for answer in &self.answers {
            write_resource_record(&mut buffer, answer, &mut name_map)?;
        }

        // Write authority
        for auth in &self.authority {
            write_resource_record(&mut buffer, auth, &mut name_map)?;
        }

        // Write additional
        for add in &self.additional {
            write_resource_record(&mut buffer, add, &mut name_map)?;
        }

        Ok(buffer)
    }
}

fn write_header(buffer: &mut Vec<u8>, packet: &DnsPacket) -> Result<(), BuildError> {
    let header = &packet.header;

    buffer.extend_from_slice(&header.id.to_be_bytes());

    let mut flags: u16 = 0;
    if header.qr {
        flags |= 0x8000;
    }
    flags |= ((header.opcode as u16) & 0x0F) << 11;
    if header.aa {
        flags |= 0x0400;
    }
    if header.tc {
        flags |= 0x0200;
    }
    if header.rd {
        flags |= 0x0100;
    }
    if header.ra {
        flags |= 0x0080;
    }
    flags |= ((header.z as u16) & 0x07) << 4;
    flags |= (header.rcode as u16) & 0x0F;

    buffer.extend_from_slice(&flags.to_be_bytes());
    buffer.extend_from_slice(&(packet.questions.len() as u16).to_be_bytes());
    buffer.extend_from_slice(&(packet.answers.len() as u16).to_be_bytes());
    buffer.extend_from_slice(&(packet.authority.len() as u16).to_be_bytes());
    buffer.extend_from_slice(&(packet.additional.len() as u16).to_be_bytes());

    Ok(())
}

fn write_name(
    buffer: &mut Vec<u8>,
    name: &str,
    name_map: &mut BTreeMap<String, u16>,
) -> Result<(), BuildError> {
    // Check for compression opportunity
    if let Some(&offset) = name_map.get(name) {
        let pointer = 0xC000 | offset;
        buffer.extend_from_slice(&pointer.to_be_bytes());
        return Ok(());
    }

    // Try to compress suffix
    let parts: Vec<&str> = name.split('.').collect();
    for i in 1..parts.len() {
        let suffix = parts[i..].join(".");
        if let Some(&offset) = name_map.get(&suffix) {
            // Write uncompressed prefix
            for part in parts.iter().take(i) {
                let label = *part;
                if label.len() > 63 {
                    return Err(BuildError::InvalidData);
                }
                buffer.push(label.len() as u8);
                buffer.extend_from_slice(label.as_bytes());
            }
            // Write compression pointer
            let pointer = 0xC000 | offset;
            buffer.extend_from_slice(&pointer.to_be_bytes());
            return Ok(());
        }
    }

    // No compression, write full name
    let offset = buffer.len() as u16;
    name_map.insert(name.to_string(), offset);

    for label in parts {
        if label.len() > 63 {
            return Err(BuildError::InvalidData);
        }
        buffer.push(label.len() as u8);
        buffer.extend_from_slice(label.as_bytes());
    }
    buffer.push(0);

    Ok(())
}

fn write_question(
    buffer: &mut Vec<u8>,
    question: &Question,
    name_map: &mut BTreeMap<String, u16>,
) -> Result<(), BuildError> {
    write_name(buffer, &question.name, name_map)?;
    buffer.extend_from_slice(&(question.qtype as u16).to_be_bytes());
    buffer.extend_from_slice(&(question.qclass as u16).to_be_bytes());
    Ok(())
}

fn write_resource_record(
    buffer: &mut Vec<u8>,
    rr: &ResourceRecord,
    name_map: &mut BTreeMap<String, u16>,
) -> Result<(), BuildError> {
    write_name(buffer, &rr.name, name_map)?;
    buffer.extend_from_slice(&(rr.rtype as u16).to_be_bytes());
    buffer.extend_from_slice(&(rr.rclass as u16).to_be_bytes());
    buffer.extend_from_slice(&rr.ttl.to_be_bytes());

    // Write rdata
    let rdata_start = buffer.len();
    buffer.extend_from_slice(&[0, 0]); // Placeholder for length

    match &rr.rdata {
        RData::A(addr) => {
            buffer.extend_from_slice(addr);
        }
        RData::AAAA(addr) => {
            buffer.extend_from_slice(addr);
        }
        RData::CNAME(target) | RData::PTR(target) | RData::NS(target) => {
            write_name(buffer, target, name_map)?;
        }
        RData::MX { priority, exchange } => {
            buffer.extend_from_slice(&priority.to_be_bytes());
            write_name(buffer, exchange, name_map)?;
        }
        RData::TXT(text) => {
            buffer.push(text.len() as u8);
            buffer.extend_from_slice(text.as_bytes());
        }
        RData::Unknown(data) => {
            buffer.extend_from_slice(data);
        }
    }

    let rdata_len = buffer.len() - rdata_start - 2;
    buffer[rdata_start] = (rdata_len >> 8) as u8;
    buffer[rdata_start + 1] = rdata_len as u8;

    Ok(())
}
