#[cfg(not(feature = "std"))]
use alloc::{
    str,
    string::{String, ToString},
    vec::Vec,
};

#[derive(Debug, PartialEq)]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    DELETE,
}

#[derive(Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum ParseError {
    Invalid,
    Incomplete,
}

impl HttpRequest {
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        // Headers are text. The body is raw bytes: a firmware chunk is not
        // UTF-8, and requiring the whole request to be text rejected it.
        let pos = data
            .windows(4)
            .position(|mark| mark == b"\r\n\r\n")
            .ok_or(ParseError::Incomplete)?;
        let text = core::str::from_utf8(&data[..pos]).map_err(|_| ParseError::Invalid)?;
        let header_part = text;
        let body = data[pos + 4..].to_vec();

        let mut lines = header_part.lines();
        let request_line = lines.next().ok_or(ParseError::Invalid)?;

        // Parse request line
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(ParseError::Invalid);
        }

        let method = match parts[0] {
            "GET" => HttpMethod::GET,
            "POST" => HttpMethod::POST,
            "PUT" => HttpMethod::PUT,
            "DELETE" => HttpMethod::DELETE,
            _ => return Err(ParseError::Invalid),
        };

        let path = parts[1].to_string();
        let version = parts[2].to_string();

        // Parse headers
        let mut headers = Vec::new();
        for line in lines {
            if let Some(pos) = line.find(':') {
                let key = line[..pos].trim().to_string();
                let value = line[pos + 1..].trim().to_string();
                headers.push((key, value));
            }
        }

        Ok(HttpRequest {
            method,
            path,
            version,
            headers,
            body,
        })
    }

    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
