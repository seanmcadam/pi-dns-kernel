#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

mod builder;
mod packet;
mod parser;

pub use builder::*;
pub use packet::*;
pub use parser::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_query() {
        // Example DNS query for "example.com" A record
        let query = vec![
            0x12, 0x34, // Transaction ID
            0x01, 0x00, // Flags: standard query
            0x00, 0x01, // Questions: 1
            0x00, 0x00, // Answer RRs: 0
            0x00, 0x00, // Authority RRs: 0
            0x00, 0x00, // Additional RRs: 0
            // Query: example.com
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm',
            0x00, // End of name
            0x00, 0x01, // Type: A
            0x00, 0x01, // Class: IN
        ];

        let packet = DnsPacket::parse(&query).expect("Failed to parse query");
        assert_eq!(packet.header.id, 0x1234);
        assert_eq!(packet.header.qr, false);
        assert_eq!(packet.questions.len(), 1);
        assert_eq!(packet.questions[0].name, "example.com");
        assert_eq!(packet.questions[0].qtype, QueryType::A);
    }

    #[test]
    fn test_build_response() {
        let mut response = DnsPacket::new(0x1234);
        response.header.qr = true;
        response.header.aa = true;

        response.questions.push(Question {
            name: "example.com".into(),
            qtype: QueryType::A,
            qclass: QueryClass::IN,
        });

        response.answers.push(ResourceRecord {
            name: "example.com".into(),
            rtype: RecordType::A,
            rclass: RecordClass::IN,
            ttl: 300,
            rdata: RData::A([93, 184, 216, 34]),
        });

        let bytes = response.to_bytes().expect("Failed to build response");
        assert!(bytes.len() > 12); // At least header size

        // Parse it back
        let parsed = DnsPacket::parse(&bytes).expect("Failed to parse built packet");
        assert_eq!(parsed.header.id, 0x1234);
        assert_eq!(parsed.answers.len(), 1);
    }

    #[test]
    fn test_name_compression() {
        // Test that name compression works correctly
        let mut packet = DnsPacket::new(0x5678);
        packet.header.qr = true;

        packet.questions.push(Question {
            name: "www.example.com".into(),
            qtype: QueryType::A,
            qclass: QueryClass::IN,
        });

        packet.answers.push(ResourceRecord {
            name: "www.example.com".into(),
            rtype: RecordType::CNAME,
            rclass: RecordClass::IN,
            ttl: 300,
            rdata: RData::CNAME("example.com".into()),
        });

        packet.answers.push(ResourceRecord {
            name: "example.com".into(),
            rtype: RecordType::A,
            rclass: RecordClass::IN,
            ttl: 300,
            rdata: RData::A([93, 184, 216, 34]),
        });

        let bytes = packet.to_bytes().expect("Failed to build packet");
        let parsed = DnsPacket::parse(&bytes).expect("Failed to parse");

        assert_eq!(parsed.answers.len(), 2);
        assert_eq!(parsed.answers[0].name, "www.example.com");
        assert_eq!(parsed.answers[1].name, "example.com");
    }
}
