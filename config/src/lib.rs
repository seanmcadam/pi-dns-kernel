//! Shared JSON parse, serialize, and schema checks.
//!
//! Every config file is a JSON object with a `version` field. A document is
//! accepted only when it is well formed, every required field is present, each
//! value has the right type and lies inside its bounds, and no key is unknown.
//! Callers apply cross-field rules, such as the log-file budget, after this
//! check and before anything is written.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const SCHEMA_VERSION: u64 = 1;
pub const UPLOAD_MAX: usize = 4096;

/// Active log file size, in bytes.
pub const LOG_MIN_BYTES: u64 = 4096;
pub const LOG_MAX_BYTES: u64 = 65_536;
/// Rotated files kept beside the active log. `0` truncates in place.
pub const LOG_MAX_ARCHIVES: u64 = 16;
/// `max_bytes * (archives + 1)` must fit this, which is well under the
/// smallest FAT image we boot (8 MiB on the raspi4b test card).
pub const LOG_MAX_TOTAL: u64 = 1_048_576;

pub const CACHE_MIN: u64 = 1;
pub const CACHE_MAX: u64 = 10_000;
pub const RECORD_MAX: usize = 64;
pub const UPSTREAM_MAX: usize = 8;
pub const BLOCK_MAX: usize = 32;
pub const NAME_MAX: usize = 64;
pub const SECRET_MAX: usize = 32;
pub const TTL_MAX: u64 = 86_400;
pub const PORT_MAX: u64 = 65_535;
pub const TIMEOUT_MIN: u64 = 100;
pub const TIMEOUT_MAX: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub path: String,
    pub message: String,
}

impl Error {
    pub fn new(path: &str, message: &str) -> Self {
        Self {
            path: path.to_string(),
            message: message.to_string(),
        }
    }

    pub fn text(&self) -> String {
        if self.path.is_empty() || self.path == "$" {
            self.message.clone()
        } else {
            alloc::format!("{}: {}", self.path, self.message)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        let Value::Object(items) = self else {
            return None;
        };
        items.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

pub enum FieldKind {
    U64 { min: u64, max: u64 },
    Str { min: usize, max: usize },
    Enum(&'static [&'static str]),
    Object(&'static [Field]),
    Array { min: usize, max: usize, of: ArrayOf },
}

pub enum ArrayOf {
    Str { min: usize, max: usize },
    Object(&'static [Field]),
}

pub struct Field {
    pub name: &'static str,
    pub required: bool,
    pub kind: FieldKind,
}

pub fn parse(text: &str) -> Result<Value, Error> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        index: 0,
    };
    parser.skip();
    let value = parser.value()?;
    parser.skip();
    if parser.index != parser.bytes.len() {
        return Err(Error::new("$", "malformed json"));
    }
    Ok(value)
}

pub fn check(value: &Value, fields: &[Field]) -> Result<(), Error> {
    check_object(value, fields, "$")
}

pub fn log_fields() -> &'static [Field] {
    &LOG_FIELDS
}

pub fn main_fields() -> &'static [Field] {
    &MAIN_FIELDS
}

/// `max_bytes * (archives + 1)` has to stay within [`LOG_MAX_TOTAL`].
pub fn log_budget(max_bytes: u64, archives: u64) -> Result<(), Error> {
    let slots = archives.saturating_add(1);
    let total = max_bytes.saturating_mul(slots);
    if total > LOG_MAX_TOTAL || total / slots != max_bytes {
        return Err(Error::new(
            "max_bytes",
            "out of bounds: max_bytes * (archives + 1) exceeds 1048576",
        ));
    }
    Ok(())
}

const LOG_FIELDS: &[Field] = &[
    Field {
        name: "version",
        required: true,
        kind: FieldKind::U64 {
            min: SCHEMA_VERSION,
            max: SCHEMA_VERSION,
        },
    },
    Field {
        name: "max_bytes",
        required: true,
        kind: FieldKind::U64 {
            min: LOG_MIN_BYTES,
            max: LOG_MAX_BYTES,
        },
    },
    Field {
        name: "archives",
        required: true,
        kind: FieldKind::U64 {
            min: 0,
            max: LOG_MAX_ARCHIVES,
        },
    },
];

const RECORD_FIELDS: &[Field] = &[
    Field {
        name: "name",
        required: true,
        kind: FieldKind::Str {
            min: 1,
            max: NAME_MAX,
        },
    },
    Field {
        name: "type",
        required: true,
        kind: FieldKind::Enum(&["A", "AAAA", "CNAME", "PTR"]),
    },
    Field {
        name: "value",
        required: true,
        kind: FieldKind::Str {
            min: 1,
            max: NAME_MAX,
        },
    },
    Field {
        name: "ttl",
        required: true,
        kind: FieldKind::U64 { min: 1, max: TTL_MAX },
    },
];

const NET_FIELDS: &[Field] = &[
    Field {
        name: "mode",
        required: true,
        kind: FieldKind::Enum(&["static", "dhcp"]),
    },
    Field {
        name: "ip",
        required: true,
        kind: FieldKind::Str { min: 7, max: 15 },
    },
    Field {
        name: "netmask",
        required: true,
        kind: FieldKind::Str { min: 7, max: 15 },
    },
    Field {
        name: "gateway",
        required: true,
        kind: FieldKind::Str { min: 7, max: 15 },
    },
    Field {
        name: "timeout_ms",
        required: true,
        kind: FieldKind::U64 {
            min: TIMEOUT_MIN,
            max: TIMEOUT_MAX,
        },
    },
];

const DNS_FIELDS: &[Field] = &[
    Field {
        name: "cache_size",
        required: true,
        kind: FieldKind::U64 {
            min: CACHE_MIN,
            max: CACHE_MAX,
        },
    },
    Field {
        name: "upstreams",
        required: true,
        kind: FieldKind::Array {
            min: 1,
            max: UPSTREAM_MAX,
            of: ArrayOf::Str { min: 7, max: 15 },
        },
    },
    Field {
        name: "blocklist",
        required: true,
        kind: FieldKind::Array {
            min: 0,
            max: BLOCK_MAX,
            of: ArrayOf::Str {
                min: 1,
                max: NAME_MAX,
            },
        },
    },
    Field {
        name: "records",
        required: true,
        kind: FieldKind::Array {
            min: 0,
            max: RECORD_MAX,
            of: ArrayOf::Object(RECORD_FIELDS),
        },
    },
];

const HTTP_FIELDS: &[Field] = &[
    Field {
        name: "port",
        required: true,
        kind: FieldKind::U64 { min: 1, max: PORT_MAX },
    },
    Field {
        name: "username",
        required: true,
        kind: FieldKind::Str {
            min: 1,
            max: SECRET_MAX,
        },
    },
    Field {
        name: "password",
        required: true,
        kind: FieldKind::Str {
            min: 1,
            max: SECRET_MAX,
        },
    },
];

const MAIN_FIELDS: &[Field] = &[
    Field {
        name: "version",
        required: true,
        kind: FieldKind::U64 {
            min: SCHEMA_VERSION,
            max: SCHEMA_VERSION,
        },
    },
    Field {
        name: "network",
        required: true,
        kind: FieldKind::Object(NET_FIELDS),
    },
    Field {
        name: "dns",
        required: true,
        kind: FieldKind::Object(DNS_FIELDS),
    },
    Field {
        name: "http",
        required: true,
        kind: FieldKind::Object(HTTP_FIELDS),
    },
];

fn check_object(value: &Value, fields: &[Field], path: &str) -> Result<(), Error> {
    let Value::Object(items) = value else {
        return Err(Error::new(path, "expected object"));
    };
    for (key, _) in items {
        if !fields.iter().any(|field| field.name == key) {
            let child = child_path(path, key);
            return Err(Error::new(&child, "unknown key"));
        }
    }
    for field in fields {
        let found = items.iter().find(|(key, _)| key == field.name);
        let child = child_path(path, field.name);
        match found {
            None if field.required => {
                return Err(Error::new(&child, "missing field"));
            }
            None => {}
            Some((_, item)) => check_kind(item, &field.kind, &child)?,
        }
    }
    Ok(())
}

fn check_kind(value: &Value, kind: &FieldKind, path: &str) -> Result<(), Error> {
    match kind {
        FieldKind::U64 { min, max } => {
            let Some(number) = value.as_u64() else {
                return Err(Error::new(path, "expected integer"));
            };
            if number < *min || number > *max {
                return Err(Error::new(
                    path,
                    &alloc::format!("out of bounds ({min}..={max})"),
                ));
            }
        }
        FieldKind::Str { min, max } => check_str(value, *min, *max, path)?,
        FieldKind::Enum(options) => {
            let Some(text) = value.as_str() else {
                return Err(Error::new(path, "expected string"));
            };
            if !options.contains(&text) {
                return Err(Error::new(path, "out of bounds"));
            }
        }
        FieldKind::Object(fields) => check_object(value, fields, path)?,
        FieldKind::Array { min, max, of } => {
            let Some(items) = value.as_array() else {
                return Err(Error::new(path, "expected array"));
            };
            if items.len() < *min || items.len() > *max {
                return Err(Error::new(path, "out of bounds"));
            }
            for (index, item) in items.iter().enumerate() {
                let child = alloc::format!("{path}[{index}]");
                match of {
                    ArrayOf::Str { min, max } => check_str(item, *min, *max, &child)?,
                    ArrayOf::Object(fields) => check_object(item, fields, &child)?,
                }
            }
        }
    }
    Ok(())
}

fn check_str(value: &Value, min: usize, max: usize, path: &str) -> Result<(), Error> {
    let Some(text) = value.as_str() else {
        return Err(Error::new(path, "expected string"));
    };
    if text.len() < min || text.len() > max {
        return Err(Error::new(path, "out of bounds"));
    }
    Ok(())
}

fn child_path(parent: &str, key: &str) -> String {
    if parent == "$" {
        key.to_string()
    } else {
        alloc::format!("{parent}.{key}")
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> Parser<'a> {
    fn value(&mut self) -> Result<Value, Error> {
        self.skip();
        let Some(byte) = self.peek() else {
            return Err(Error::new("$", "malformed json"));
        };
        match byte {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            b'0'..=b'9' => self.number(),
            _ => Err(Error::new("$", "malformed json")),
        }
    }

    fn object(&mut self) -> Result<Value, Error> {
        self.bump(b'{')?;
        let mut items = Vec::new();
        self.skip();
        if self.eat(b'}') {
            return Ok(Value::Object(items));
        }
        loop {
            self.skip();
            if self.peek() != Some(b'"') {
                return Err(Error::new("$", "malformed json"));
            }
            let key = self.string()?;
            if items.iter().any(|(name, _)| name == &key) {
                return Err(Error::new(&key, "malformed json"));
            }
            self.skip();
            self.bump(b':')?;
            let value = self.value()?;
            items.push((key, value));
            self.skip();
            if self.eat(b',') {
                continue;
            }
            self.bump(b'}')?;
            break;
        }
        Ok(Value::Object(items))
    }

    fn array(&mut self) -> Result<Value, Error> {
        self.bump(b'[')?;
        let mut items = Vec::new();
        self.skip();
        if self.eat(b']') {
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip();
            if self.eat(b',') {
                continue;
            }
            self.bump(b']')?;
            break;
        }
        Ok(Value::Array(items))
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.index;
        if self.peek() == Some(b'0') {
            self.index += 1;
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(Error::new("$", "malformed json"));
            }
        } else {
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E' | b'-' | b'+')) {
            return Err(Error::new("$", "malformed json"));
        }
        let text = core::str::from_utf8(&self.bytes[start..self.index]).unwrap_or("");
        let number = text
            .parse::<u64>()
            .map_err(|_| Error::new("$", "malformed json"))?;
        Ok(Value::Number(number))
    }

    fn string(&mut self) -> Result<String, Error> {
        self.bump(b'"')?;
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            self.index += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(Error::new("$", "malformed json"));
                    };
                    self.index += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode()?),
                        _ => return Err(Error::new("$", "malformed json")),
                    }
                }
                b if b < 0x20 => return Err(Error::new("$", "malformed json")),
                b => out.push(b as char),
            }
        }
        Err(Error::new("$", "malformed json"))
    }

    fn unicode(&mut self) -> Result<char, Error> {
        if self.index + 4 > self.bytes.len() {
            return Err(Error::new("$", "malformed json"));
        }
        let hex = &self.bytes[self.index..self.index + 4];
        self.index += 4;
        let mut value = 0u32;
        for byte in hex {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(Error::new("$", "malformed json")),
            };
            value = (value << 4) | u32::from(digit);
        }
        char::from_u32(value).ok_or_else(|| Error::new("$", "malformed json"))
    }

    fn literal(&mut self, word: &[u8], value: Value) -> Result<Value, Error> {
        if self.bytes[self.index..].starts_with(word) {
            self.index += word.len();
            Ok(value)
        } else {
            Err(Error::new("$", "malformed json"))
        }
    }

    fn skip(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn bump(&mut self, byte: u8) -> Result<(), Error> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(Error::new("$", "malformed json"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_schema_accepts_the_default() {
        let value = parse(r#"{"version":1,"max_bytes":4096,"archives":3}"#).unwrap();
        check(&value, log_fields()).unwrap();
        log_budget(4096, 3).unwrap();
    }

    #[test]
    fn log_schema_rejects_bad_documents() {
        assert!(parse("{").unwrap_err().message.contains("malformed"));
        let missing = parse(r#"{"version":1,"max_bytes":4096}"#).unwrap();
        assert!(check(&missing, log_fields()).unwrap_err().message.contains("missing"));
        let wrong = parse(r#"{"version":1,"max_bytes":"4096","archives":1}"#).unwrap();
        assert!(check(&wrong, log_fields())
            .unwrap_err()
            .message
            .contains("expected integer"));
        let bounds = parse(r#"{"version":1,"max_bytes":100,"archives":1}"#).unwrap();
        assert!(check(&bounds, log_fields())
            .unwrap_err()
            .message
            .contains("out of bounds"));
        let unknown = parse(r#"{"version":1,"max_bytes":4096,"archives":1,"extra":1}"#).unwrap();
        assert!(check(&unknown, log_fields())
            .unwrap_err()
            .message
            .contains("unknown"));
        assert!(log_budget(65_536, 16).is_err());
    }
}
