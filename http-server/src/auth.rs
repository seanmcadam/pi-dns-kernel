#[cfg(not(feature = "std"))]
use alloc::{
    string::{String, ToString},
    vec::Vec,
};

pub fn parse_basic_auth(auth_header: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = auth_header.split_whitespace().collect();
    if parts.len() != 2 || parts[0] != "Basic" {
        return None;
    }

    let decoded = base64_decode(parts[1])?;
    let credentials = String::from_utf8(decoded).ok()?;

    let mut split = credentials.splitn(2, ':');
    let username = split.next()?.to_string();
    let password = split.next()?.to_string();

    Some((username, password))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const DECODE_TABLE: [u8; 128] = [
        255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 255, 255, 255, 62, 255, 255, 255, 63, 52, 53, 54, 55, 56, 57, 58, 59,
        60, 61, 255, 255, 255, 0, 255, 255, 255, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
        15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 255, 255, 255, 255, 255, 255, 26, 27, 28, 29,
        30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
        255, 255, 255, 255, 255,
    ];

    let input = input.trim_end_matches('=');
    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;

    for &byte in input.as_bytes() {
        if byte >= 128 {
            return None;
        }

        let value = DECODE_TABLE[byte as usize];
        if value == 255 {
            return None;
        }

        buffer = (buffer << 6) | (value as u32);
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_decode() {
        assert_eq!(
            base64_decode("YWRtaW46cGFzc3dvcmQ="),
            Some(b"admin:password".to_vec())
        );
        assert_eq!(base64_decode("SGVsbG8="), Some(b"Hello".to_vec()));
        assert_eq!(base64_decode(""), Some(Vec::new()));
    }
}
