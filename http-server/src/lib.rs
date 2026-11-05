#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

mod auth;
mod request;
mod response;

pub use auth::*;
pub use request::*;
pub use response::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_get_request() {
        let data = b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = HttpRequest::parse(data).expect("Failed to parse request");

        assert_eq!(req.method, HttpMethod::GET);
        assert_eq!(req.path, "/status");
        assert_eq!(req.version, "HTTP/1.1");
    }

    #[test]
    fn test_parse_post_request() {
        let data = b"POST /config HTTP/1.1\r\nHost: localhost\r\nContent-Length: 13\r\n\r\n{\"key\":\"val\"}";
        let req = HttpRequest::parse(data).expect("Failed to parse request");

        assert_eq!(req.method, HttpMethod::POST);
        assert_eq!(req.path, "/config");
        assert_eq!(req.body, b"{\"key\":\"val\"}");
    }

    #[test]
    fn test_build_response() {
        let response = HttpResponse::ok("Hello, world!");
        let bytes = response.to_bytes();
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("Hello, world!"));
    }

    #[test]
    fn test_basic_auth() {
        // Base64 of "admin:password" is "YWRtaW46cGFzc3dvcmQ="
        let auth_header = "Basic YWRtaW46cGFzc3dvcmQ=";
        let (user, pass) = parse_basic_auth(auth_header).expect("Failed to parse auth");

        assert_eq!(user, "admin");
        assert_eq!(pass, "password");
    }
}
