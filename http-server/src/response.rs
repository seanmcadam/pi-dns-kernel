#[cfg(not(feature = "std"))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

pub struct HttpResponse {
    pub status_code: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status_code: u16, status_text: &str) -> Self {
        Self {
            status_code,
            status_text: status_text.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn ok(body: &str) -> Self {
        let mut response = Self::new(200, "OK");
        response.set_body(body.as_bytes());
        response
    }

    pub fn not_found() -> Self {
        let mut response = Self::new(404, "Not Found");
        response.set_body(b"404 Not Found");
        response
    }

    pub fn unauthorized() -> Self {
        let mut response = Self::new(401, "Unauthorized");
        response.add_header("WWW-Authenticate", "Basic realm=\"DNS Server\"");
        response.set_body(b"401 Unauthorized");
        response
    }

    pub fn internal_error() -> Self {
        let mut response = Self::new(500, "Internal Server Error");
        response.set_body(b"500 Internal Server Error");
        response
    }

    pub fn add_header(&mut self, key: &str, value: &str) {
        self.headers.push((key.into(), value.into()));
    }

    pub fn set_body(&mut self, body: &[u8]) {
        self.body = body.to_vec();
        self.add_header("Content-Length", &self.body.len().to_string());
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 {} {}\r\n", self.status_code, self.status_text).into_bytes();

        for (key, value) in &self.headers {
            response.extend_from_slice(format!("{}: {}\r\n", key, value).as_bytes());
        }

        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(&self.body);

        response
    }
}
