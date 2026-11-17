use crate::packet::*;

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec::Vec};

#[derive(Debug)]
pub enum ParseError {
    TooShort,
    InvalidFormat,
    UnsupportedType,
}

impl DnsPacket {
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        if data.len() < 12 {
            return Err(ParseError::TooShort);
        }

        let header = parse_header(data)?;
        let mut offset = 12;

        let mut questions = Vec::new();
        for _ in 0..header.qdcount {
            let (question, new_offset) = parse_question(data, offset)?;
            questions.push(question);
            offset = new_offset;
        }

        let mut answers = Vec::new();
        for _ in 0..header.ancount {
            let (rr, new_offset) = parse_resource_record(data, offset)?;
            answers.push(rr);
            offset = new_offset;
        }

        let mut authority = Vec::new();
        for _ in 0..header.nscount {
            let (rr, new_offset) = parse_resource_record(data, offset)?;
            authority.push(rr);
            offset = new_offset;
        }

        let mut additional = Vec::new();
        for _ in 0..header.arcount {
            // dig (and most resolvers) append an EDNS OPT pseudo-RR. Its type
            // and class are not a normal record, so skip anything we don't
            // understand instead of failing the whole query.
            let start = offset;
            match parse_resource_record(data, offset) {
                Ok((rr, new_offset)) => {
                    additional.push(rr);
                    offset = new_offset;
                }
                Err(ParseError::UnsupportedType) => {
                    offset = skip_resource_record(data, start)?;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(DnsPacket {
            header,
            questions,
            answers,
            authority,
            additional,
        })
    }
}

fn parse_header(data: &[u8]) -> Result<Header, ParseError> {
    if data.len() < 12 {
        return Err(ParseError::TooShort);
    }

    let id = u16::from_be_bytes([data[0], data[1]]);
    let flags = u16::from_be_bytes([data[2], data[3]]);

    Ok(Header {
        id,
        qr: (flags & 0x8000) != 0,
        opcode: ((flags >> 11) & 0x0F) as u8,
        aa: (flags & 0x0400) != 0,
        tc: (flags & 0x0200) != 0,
        rd: (flags & 0x0100) != 0,
        ra: (flags & 0x0080) != 0,
        z: ((flags >> 4) & 0x07) as u8,
        rcode: (flags & 0x0F) as u8,
        qdcount: u16::from_be_bytes([data[4], data[5]]),
        ancount: u16::from_be_bytes([data[6], data[7]]),
        nscount: u16::from_be_bytes([data[8], data[9]]),
        arcount: u16::from_be_bytes([data[10], data[11]]),
    })
}

fn parse_name(data: &[u8], mut offset: usize) -> Result<(String, usize), ParseError> {
    let mut name = String::new();
    let mut jumped = false;
    let mut jump_offset = 0;
    let mut hops = 0u32;

    loop {
        if offset >= data.len() {
            return Err(ParseError::TooShort);
        }

        let len = data[offset];

        if len == 0 {
            offset += 1;
            break;
        }

        if (len & 0xC0) == 0xC0 {
            // Compression pointer. A pointer that does not move backward,
            // or a chain longer than a name, loops forever and pins the
            // task. Garbage frames on a live LAN do this.
            if offset + 1 >= data.len() {
                return Err(ParseError::TooShort);
            }
            hops += 1;
            if hops > 16 {
                return Err(ParseError::InvalidFormat);
            }

            if !jumped {
                jump_offset = offset + 2;
            }

            let pointer = u16::from_be_bytes([len & 0x3F, data[offset + 1]]) as usize;
            if pointer >= offset {
                return Err(ParseError::InvalidFormat);
            }
            offset = pointer;
            jumped = true;
            continue;
        }

        offset += 1;

        if offset + (len as usize) > data.len() {
            return Err(ParseError::TooShort);
        }

        if !name.is_empty() {
            name.push('.');
        }

        for i in 0..(len as usize) {
            name.push(data[offset + i] as char);
        }

        offset += len as usize;
    }

    let final_offset = if jumped { jump_offset } else { offset };
    Ok((name, final_offset))
}

fn parse_question(data: &[u8], offset: usize) -> Result<(Question, usize), ParseError> {
    let (name, mut offset) = parse_name(data, offset)?;

    if offset + 4 > data.len() {
        return Err(ParseError::TooShort);
    }

    let qtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
    let qclass = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
    offset += 4;

    let qtype = QueryType::from_u16(qtype).ok_or(ParseError::UnsupportedType)?;
    let qclass = QueryClass::from_u16(qclass).ok_or(ParseError::UnsupportedType)?;

    Ok((
        Question {
            name,
            qtype,
            qclass,
        },
        offset,
    ))
}

fn skip_resource_record(data: &[u8], offset: usize) -> Result<usize, ParseError> {
    let (_, mut offset) = parse_name(data, offset)?;
    if offset + 10 > data.len() {
        return Err(ParseError::TooShort);
    }
    let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]) as usize;
    offset += 10;
    if offset + rdlength > data.len() {
        return Err(ParseError::TooShort);
    }
    Ok(offset + rdlength)
}

fn parse_resource_record(
    data: &[u8],
    offset: usize,
) -> Result<(ResourceRecord, usize), ParseError> {
    let (name, mut offset) = parse_name(data, offset)?;

    if offset + 10 > data.len() {
        return Err(ParseError::TooShort);
    }

    let rtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
    let rclass = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
    let ttl = u32::from_be_bytes([
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]);
    let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]) as usize;
    offset += 10;

    if offset + rdlength > data.len() {
        return Err(ParseError::TooShort);
    }

    let rtype = RecordType::from_u16(rtype).ok_or(ParseError::UnsupportedType)?;
    let rclass = RecordClass::from_u16(rclass).ok_or(ParseError::UnsupportedType)?;

    let rdata = match rtype {
        RecordType::A => {
            if rdlength != 4 {
                return Err(ParseError::InvalidFormat);
            }
            let mut addr = [0u8; 4];
            addr.copy_from_slice(&data[offset..offset + 4]);
            RData::A(addr)
        }
        RecordType::AAAA => {
            if rdlength != 16 {
                return Err(ParseError::InvalidFormat);
            }
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&data[offset..offset + 16]);
            RData::AAAA(addr)
        }
        RecordType::CNAME | RecordType::PTR | RecordType::NS => {
            let (target, _) = parse_name(data, offset)?;
            match rtype {
                RecordType::CNAME => RData::CNAME(target),
                RecordType::PTR => RData::PTR(target),
                RecordType::NS => RData::NS(target),
                _ => unreachable!(),
            }
        }
        RecordType::MX => {
            if rdlength < 2 {
                return Err(ParseError::InvalidFormat);
            }
            let priority = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let (exchange, _) = parse_name(data, offset + 2)?;
            RData::MX { priority, exchange }
        }
        _ => {
            let mut rdata_bytes = Vec::new();
            rdata_bytes.extend_from_slice(&data[offset..offset + rdlength]);
            RData::Unknown(rdata_bytes)
        }
    };

    offset += rdlength;

    Ok((
        ResourceRecord {
            name,
            rtype,
            rclass,
            ttl,
            rdata,
        },
        offset,
    ))
}
