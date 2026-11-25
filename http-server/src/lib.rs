#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

mod auth;
mod request;
mod response;

pub use auth::*;
pub use request::*;
pub use response::*;

/// Status-page helpers. The page calls these after it parses `/json/status`.
/// Upstream rows are a table. The whole document is indented into a `<pre>`.
pub const STATUS_SCRIPT: &str = r#"
function renderUpstreams(list) {
  var el = document.getElementById("upstreams");
  if (!el) return;
  while (el.firstChild) el.removeChild(el.firstChild);
  if (!list || !list.length) {
    el.textContent = "none";
    return;
  }
  var table = document.createElement("table");
  var head = document.createElement("tr");
  ["Address", "Status"].forEach(function(name) {
    var th = document.createElement("th");
    th.textContent = name;
    head.appendChild(th);
  });
  table.appendChild(head);
  for (var i = 0; i < list.length; i++) {
    var entry = list[i] || {};
    var row = document.createElement("tr");
    var ip = document.createElement("td");
    ip.textContent = entry.ip || "";
    var state = document.createElement("td");
    state.textContent = entry.up ? "up" : "down";
    row.appendChild(ip);
    row.appendChild(state);
    table.appendChild(row);
  }
  el.appendChild(table);
}
function prettyStatus(d) {
  var el = document.getElementById("status_json");
  if (el) el.textContent = JSON.stringify(d, null, 2);
}
"#;

/// Path and query from a request target. The query is empty when there is none.
pub fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    }
}

/// Browsers send `Accept: text/html`. Scripts and `format=json` stay on JSON.
/// `format=html` forces the page. `*/*` alone does not.
pub fn wants_html(query: &str, accept: Option<&str>) -> bool {
    if query_is(query, "format", "json") {
        return false;
    }
    if query_is(query, "format", "html") {
        return true;
    }
    match accept {
        Some(accept) => {
            let lower = accept.to_ascii_lowercase();
            lower.split(',').any(|part| {
                let media = part.split(';').next().unwrap_or("").trim();
                media == "text/html"
            })
        }
        None => false,
    }
}

fn query_is(query: &str, key: &str, value: &str) -> bool {
    query.split('&').any(|part| {
        let (got_key, got_value) = part.split_once('=').unwrap_or((part, ""));
        got_key == key && got_value == value
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_script_renders_each_upstream_and_indents_json() {
        let js = super::STATUS_SCRIPT;
        assert!(js.contains("entry.ip"));
        assert!(js.contains("entry.up ? \"up\" : \"down\""));
        assert!(js.contains("JSON.stringify(d, null, 2)"));
        assert!(!js.contains("[object Object]"));
    }

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
    fn accept_negotiation_keeps_scripts_on_json() {
        assert!(!wants_html("", None));
        assert!(!wants_html("", Some("*/*")));
        assert!(!wants_html("", Some("application/json")));
        assert!(wants_html("", Some("text/html")));
        assert!(wants_html(
            "",
            Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        ));
        assert!(!wants_html("format=json", Some("text/html")));
        assert!(wants_html("format=html", None));
        assert!(wants_html("format=html&x=1", Some("application/json")));
        let (path, query) = split_target("/status?format=json");
        assert_eq!(path, "/status");
        assert_eq!(query, "format=json");
        assert_eq!(split_target("/json/status"), ("/json/status", ""));
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
