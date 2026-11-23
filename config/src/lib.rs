//! Shared JSON parse, serialize, and schema checks.
//!
//! Every config file is a JSON object with a `version` field. A document is
//! accepted only when it is well formed, every required field is present, each
//! value has the right type and lies inside its bounds, and no key is unknown.
//! Callers apply cross-field rules, such as the log-file budget, after this
//! check and before anything is written. A saved file is pretty-printed when
//! that text still fits in [`UPLOAD_MAX`]. A larger pretty form is stored
//! compact instead of being truncated, and the input limit stays 4096.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const SCHEMA_VERSION: u64 = 1;
pub const UPLOAD_MAX: usize = 4096;

/// Canonical `LOG.JSN` written when the file is missing. Version 1,
/// `max_bytes` 4096, `archives` 3.
pub const DEFAULT_LOG_JSON: &str = "{\n  \"version\": 1,\n  \"max_bytes\": 4096,\n  \"archives\": 3\n}\n";

/// Active log file size, in bytes.
pub const LOG_MIN_BYTES: u64 = 4096;
pub const LOG_MAX_BYTES: u64 = 65_536;
/// Rotated files kept beside the active log. `0` truncates in place.
pub const LOG_MAX_ARCHIVES: u64 = 16;
/// `max_bytes * (archives + 1)` must fit this, which is well under the
/// smallest FAT image we boot (8 MiB on the raspi4b test card).
pub const LOG_MAX_TOTAL: u64 = 1_048_576;

/// DNS cache slots. Each slot reserves 800 bytes plus a bucket word, and the
/// DNS task heap is 1536 KiB with five 16 KiB thread stacks, so 1024 is the
/// largest pool that still leaves room for packets.
pub const CACHE_MIN: u64 = 1;
pub const CACHE_MAX: u64 = 1024;
pub const CACHE_DEFAULT: u64 = 1024;
/// How often the DNS task returns expired cache slots to the free list.
pub const REAP_MIN: u64 = 1;
pub const REAP_MAX: u64 = 3_600;
pub const REAP_DEFAULT: u64 = 5;
/// How often the DNS task probes each upstream. A missing key keeps 5.
pub const PROBE_MIN: u64 = 1;
pub const PROBE_MAX: u64 = 3_600;
pub const PROBE_DEFAULT: u64 = 5;
pub const RECORD_MAX: usize = 64;
pub const UPSTREAM_MAX: usize = 8;
pub const BLOCK_MAX: usize = 32;
pub const NAME_MAX: usize = 64;
pub const SECRET_MAX: usize = 32;
pub const TTL_MAX: u64 = 86_400;
pub const PORT_MAX: u64 = 65_535;
/// VNC listens here unless `display.vnc_port` says otherwise.
pub const VNC_PORT_DEFAULT: u64 = 5900;
/// RFB password, zero means no authentication. Eight bytes is the DES key.
pub const VNC_PASS_MAX: usize = 8;
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

/// Pretty-print `value` when the result fits in [`UPLOAD_MAX`].
///
/// Otherwise emit a compact document if that fits. The `Err` path means
/// neither form fits, so the caller must leave the previous file untouched.
/// Strings are escaped without dropping characters. Object key order and
/// array order are kept.
pub fn format_document(value: &Value) -> Result<String, Error> {
    let mut buf = DocBuf::new();
    write_value(&mut buf, value, 0, true);
    if buf.fail == Fail::None {
        buf.push_str("\n");
    }
    if buf.fail == Fail::None {
        return Ok(buf.out);
    }
    if buf.fail == Fail::Size {
        buf.reset();
        write_value(&mut buf, value, 0, false);
        if buf.fail == Fail::None {
            return Ok(buf.out);
        }
    }
    Err(match buf.fail {
        Fail::Value => Error::new("$", "formatted config cannot preserve a value"),
        Fail::Size | Fail::None => Error::new("$", "formatted config exceeds 4096 bytes"),
    })
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
    Field {
        name: "speed",
        required: false,
        kind: FieldKind::U64 { min: 0, max: 1000 },
    },
    Field {
        name: "arp_secs",
        required: false,
        kind: FieldKind::U64 { min: 0, max: 60 },
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
        name: "cache_reap_interval_secs",
        required: true,
        kind: FieldKind::U64 {
            min: REAP_MIN,
            max: REAP_MAX,
        },
    },
    Field {
        name: "probe_interval_secs",
        required: false,
        kind: FieldKind::U64 {
            min: PROBE_MIN,
            max: PROBE_MAX,
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

const DISPLAY_FIELDS: &[Field] = &[
    Field {
        name: "vnc",
        required: true,
        kind: FieldKind::U64 { min: 0, max: 1 },
    },
    Field {
        name: "vnc_port",
        required: true,
        kind: FieldKind::U64 {
            min: 1,
            max: PORT_MAX,
        },
    },
    Field {
        name: "vnc_password",
        required: true,
        kind: FieldKind::Str {
            min: 0,
            max: VNC_PASS_MAX,
        },
    },
    Field {
        name: "hdmi",
        required: true,
        kind: FieldKind::U64 { min: 0, max: 1 },
    },
    Field {
        name: "hdmi_reset",
        required: false,
        kind: FieldKind::U64 { min: 0, max: 1 },
    },
];

const BOARD_FIELDS: &[Field] = &[
    Field {
        name: "detect",
        required: true,
        kind: FieldKind::U64 { min: 0, max: 1 },
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
    Field {
        name: "display",
        required: true,
        kind: FieldKind::Object(DISPLAY_FIELDS),
    },
    Field {
        name: "board",
        required: false,
        kind: FieldKind::Object(BOARD_FIELDS),
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

struct DocBuf {
    out: String,
    fail: Fail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    None,
    Size,
    Value,
}

impl DocBuf {
    fn new() -> Self {
        Self {
            out: String::with_capacity(UPLOAD_MAX),
            fail: Fail::None,
        }
    }

    fn reset(&mut self) {
        self.out.clear();
        self.fail = Fail::None;
    }

    fn push_str(&mut self, text: &str) {
        if self.fail != Fail::None {
            return;
        }
        if self.out.len().saturating_add(text.len()) > UPLOAD_MAX {
            self.fail = Fail::Size;
            return;
        }
        self.out.push_str(text);
    }

    fn push_char(&mut self, ch: char) {
        let mut tmp = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut tmp));
    }

    fn push_u64(&mut self, mut value: u64) {
        let mut buf = [0u8; 20];
        let mut index = 20;
        loop {
            index -= 1;
            buf[index] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push_str(core::str::from_utf8(&buf[index..]).unwrap_or(""));
    }
}

fn write_value(buf: &mut DocBuf, value: &Value, indent: usize, pretty: bool) {
    if buf.fail != Fail::None {
        return;
    }
    if indent > 32 {
        buf.fail = Fail::Value;
        return;
    }
    match value {
        Value::Null => buf.push_str("null"),
        Value::Bool(true) => buf.push_str("true"),
        Value::Bool(false) => buf.push_str("false"),
        Value::Number(number) => buf.push_u64(*number),
        Value::String(text) => write_string(buf, text),
        Value::Array(items) => write_array(buf, items, indent, pretty),
        Value::Object(items) => write_object(buf, items, indent, pretty),
    }
}

fn write_object(buf: &mut DocBuf, items: &[(String, Value)], indent: usize, pretty: bool) {
    if items.is_empty() {
        buf.push_str("{}");
        return;
    }
    buf.push_str("{");
    for (index, (key, value)) in items.iter().enumerate() {
        if index > 0 {
            buf.push_str(",");
        }
        if pretty {
            newline_indent(buf, indent + 1);
        }
        write_string(buf, key);
        buf.push_str(if pretty { ": " } else { ":" });
        write_value(buf, value, indent + 1, pretty);
    }
    if pretty {
        newline_indent(buf, indent);
    }
    buf.push_str("}");
}

fn write_array(buf: &mut DocBuf, items: &[Value], indent: usize, pretty: bool) {
    if items.is_empty() {
        buf.push_str("[]");
        return;
    }
    buf.push_str("[");
    for (index, value) in items.iter().enumerate() {
        if index > 0 {
            buf.push_str(",");
        }
        if pretty {
            newline_indent(buf, indent + 1);
        }
        write_value(buf, value, indent + 1, pretty);
    }
    if pretty {
        newline_indent(buf, indent);
    }
    buf.push_str("]");
}

fn newline_indent(buf: &mut DocBuf, indent: usize) {
    buf.push_str("\n");
    let mut left = indent.saturating_mul(2);
    while left > 0 {
        buf.push_str(" ");
        left -= 1;
    }
}

fn write_string(buf: &mut DocBuf, text: &str) {
    buf.push_str("\"");
    for ch in text.chars() {
        match ch {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            ch if (0x20..0x7f).contains(&(ch as u32)) => buf.push_char(ch),
            ch if (ch as u32) <= 0xffff => {
                let value = ch as u32;
                buf.push_str("\\u");
                for shift in [12, 8, 4, 0] {
                    buf.push_char(hex_digit((value >> shift) & 0xf));
                }
            }
            _ => {
                buf.fail = Fail::Value;
                return;
            }
        }
    }
    buf.push_str("\"");
}

/// Validate a main config and pretty-print it with the same rules as an upload.
/// Unknown keys are rejected. The saved text is pretty when it fits in 4096.
pub fn accept_main(text: &str) -> Result<String, Error> {
    if text.len() > UPLOAD_MAX {
        return Err(Error::new("$", "upload exceeds 4096 bytes"));
    }
    let value = parse(text)?;
    check(&value, main_fields())?;
    format_document(&value)
}

/// Raw `application/x-www-form-urlencoded` value, still percent-encoded.
pub fn form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    for pair in body.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            return Some(value);
        }
    }
    None
}

/// Percent-decode one form value. `+` is a space. The result must fit in 4096.
pub fn decode_form_value(raw: &str) -> Result<String, Error> {
    let bytes = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if out.len() >= UPLOAD_MAX {
            return Err(Error::new("$", "upload exceeds 4096 bytes"));
        }
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_byte(bytes[i + 1])?;
                let lo = hex_byte(bytes[i + 2])?;
                out.push((hi << 4 | lo) as char);
                i += 3;
            }
            b'%' => return Err(Error::new("$", "malformed form")),
            byte => {
                out.push(byte as char);
                i += 1;
            }
        }
    }
    Ok(out)
}

pub fn csrf_matches(expected: &str, body: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    match form_value(body, "csrf").and_then(|raw| decode_form_value(raw).ok()) {
        Some(got) => got == expected,
        None => false,
    }
}

pub fn reboot_confirmed(body: &str) -> bool {
    match form_value(body, "reboot") {
        Some("1") | Some("yes") | Some("on") | Some("true") => true,
        _ => false,
    }
}

pub fn posted_json(body: &str) -> Result<String, Error> {
    let raw = form_value(body, "json").ok_or_else(|| Error::new("json", "missing"))?;
    decode_form_value(raw)
}

/// What to do with a static-network change that was armed before reboot.
#[derive(Clone, Copy)]
pub struct NetWatch {
    pub pending: bool,
    pub static_mode: bool,
    pub address_up: bool,
    pub elapsed_ms: u64,
    pub limit_ms: u64,
}

pub fn net_watch(watch: NetWatch) -> &'static str {
    if !watch.pending || !watch.static_mode {
        return "idle";
    }
    if watch.address_up {
        return "hold";
    }
    if watch.elapsed_ms >= watch.limit_ms {
        return "revert";
    }
    "wait"
}

fn hex_byte(byte: u8) -> Result<u8, Error> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::new("$", "malformed form")),
    }
}

fn hex_digit(n: u32) -> char {
    let n = n as u8;
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + (n - 10)) as char
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
        assert!(log_budget(65_536, 15).is_ok());
        assert!(log_budget(4096, 0).is_ok());
        let below = parse(r#"{"version":1,"max_bytes":4095,"archives":0}"#).unwrap();
        assert!(check(&below, log_fields())
            .unwrap_err()
            .message
            .contains("out of bounds"));
        let archives = parse(r#"{"version":1,"max_bytes":4096,"archives":17}"#).unwrap();
        assert!(check(&archives, log_fields())
            .unwrap_err()
            .message
            .contains("out of bounds"));
    }

    #[test]
    fn default_log_json_is_pretty_and_stable() {
        let value = parse(DEFAULT_LOG_JSON).unwrap();
        check(&value, log_fields()).unwrap();
        assert_eq!(value.get("version").and_then(Value::as_u64), Some(1));
        assert_eq!(value.get("max_bytes").and_then(Value::as_u64), Some(4096));
        assert_eq!(value.get("archives").and_then(Value::as_u64), Some(3));
        let formatted = format_document(&value).unwrap();
        assert_eq!(formatted, DEFAULT_LOG_JSON);
        assert!(formatted.len() <= UPLOAD_MAX);
        assert_eq!(format_document(&parse(&formatted).unwrap()).unwrap(), formatted);
    }

    #[test]
    fn format_preserves_strings_and_escapes() {
        let raw = r#"{"version":1,"password":"a\"b\\c/\u0041\n\u0001"}"#;
        let value = parse(raw).unwrap();
        let text = value.get("password").and_then(Value::as_str).unwrap();
        assert_eq!(text, "a\"b\\c/A\n\u{0001}");
        let formatted = format_document(&value).unwrap();
        assert!(formatted.starts_with("{\n"));
        assert!(formatted.ends_with("\n"));
        assert!(formatted.contains("\\\""));
        assert!(formatted.contains("\\\\"));
        assert!(formatted.contains("\\n"));
        assert!(formatted.contains("\\u0001"));
        assert_eq!(parse(&formatted).unwrap(), value);
        assert_eq!(format_document(&parse(&formatted).unwrap()).unwrap(), formatted);
        assert!(formatted.len() <= UPLOAD_MAX);
    }

    #[test]
    fn whitespace_and_key_order_round_trip_without_dropping_fields() {
        let raw = "{\n\t\"archives\" : 3, \"version\" : 1, \"max_bytes\" : 4096 }\n";
        let value = parse(raw).unwrap();
        check(&value, log_fields()).unwrap();
        let formatted = format_document(&value).unwrap();
        assert_eq!(
            formatted,
            "{\n  \"archives\": 3,\n  \"version\": 1,\n  \"max_bytes\": 4096\n}\n"
        );
        assert_eq!(parse(&formatted).unwrap(), value);
    }

    #[test]
    fn pretty_form_that_exceeds_4096_falls_back_to_compact() {
        let mut items = Vec::new();
        let mut index = 0;
        while index < 300 {
            items.push((alloc::format!("k{index:03}"), Value::String(String::from("z"))));
            index += 1;
        }
        let value = Value::Object(items);
        let formatted = format_document(&value).unwrap();
        assert!(formatted.len() <= UPLOAD_MAX);
        assert!(!formatted.contains('\n'), "compact fallback must not be truncated pretty text");
        assert_eq!(parse(&formatted).unwrap(), value);
        assert_eq!(format_document(&parse(&formatted).unwrap()).unwrap(), formatted);
    }

    #[test]
    fn oversized_document_is_rejected_whole() {
        let mut items = Vec::new();
        let mut index = 0;
        while index < 400 {
            items.push(Value::String(String::from("abcdefghijklmnopqrstuvwxyz012345")));
            index += 1;
        }
        let err = format_document(&Value::Array(items)).unwrap_err();
        assert!(err.message.contains("exceeds 4096"));
        assert_eq!(UPLOAD_MAX, 4096);
    }

    #[test]
    fn duplicate_keys_and_bad_escapes_are_rejected() {
        assert!(parse(r#"{"version":1,"version":1}"#).is_err());
        assert!(parse(r#"{"version":"\u12"}"#).is_err());
        assert!(parse(r#"{"version":"\uXXXX"}"#).is_err());
        assert!(parse("{\"version\":\"\n\"}").is_err());
    }

    #[test]
    fn form_save_rejects_unknown_keys_and_checks_csrf() {
        let saved = accept_main(&sample_config("")).unwrap();
        assert!(saved.starts_with("{\n"));
        let mut extra = parse(&sample_config("")).unwrap();
        if let Value::Object(items) = &mut extra {
            items.push((String::from("extra"), Value::Number(1)));
        }
        let text = format_document(&extra).unwrap();
        assert!(accept_main(&text).unwrap_err().message.contains("unknown"));
        let json = decode_form_value("%7B%22version%22%3A1%7D").unwrap();
        assert_eq!(json, r#"{"version":1}"#);
        let body = "csrf=abcdef&reboot=1&json=%7B%7D";
        assert!(csrf_matches("abcdef", body));
        assert!(!csrf_matches("abcdef", "reboot=1&json=%7B%7D"));
        assert!(!csrf_matches("", body));
        assert!(reboot_confirmed(body));
        assert!(!reboot_confirmed("reboot=0"));
        assert_eq!(posted_json(body).unwrap(), "{}");
    }

    #[test]
    fn static_network_reverts_only_after_the_timeout() {
        let wait = NetWatch {
            pending: true,
            static_mode: true,
            address_up: false,
            elapsed_ms: 1000,
            limit_ms: 20_000,
        };
        assert_eq!(net_watch(wait), "wait");
        let revert = NetWatch {
            elapsed_ms: 20_000,
            ..wait
        };
        assert_eq!(net_watch(revert), "revert");
        let held = NetWatch {
            address_up: true,
            elapsed_ms: 20_000,
            ..wait
        };
        assert_eq!(net_watch(held), "hold");
        let dhcp = NetWatch {
            static_mode: false,
            ..wait
        };
        assert_eq!(net_watch(dhcp), "idle");
    }

    fn sample_config(probe: &str) -> String {
        alloc::format!(
            r#"{{"version":1,"network":{{"mode":"dhcp","ip":"10.0.2.15","netmask":"255.255.255.0","gateway":"10.0.2.2","timeout_ms":3000}},"dns":{{"cache_size":1024,"cache_reap_interval_secs":5,{probe}"upstreams":["8.8.8.8"],"blocklist":[],"records":[]}},"http":{{"port":80,"username":"admin","password":"admin"}},"display":{{"vnc":0,"vnc_port":5900,"vnc_password":"","hdmi":0}}}}"#
        )
    }

    #[test]
    fn probe_interval_is_optional_and_bounded() {
        let missing = parse(&sample_config("")).unwrap();
        check(&missing, main_fields()).unwrap();
        let present = parse(&sample_config(r#""probe_interval_secs":5,"#)).unwrap();
        check(&present, main_fields()).unwrap();
        assert_eq!(
            present
                .get("dns")
                .and_then(|dns| dns.get("probe_interval_secs"))
                .and_then(Value::as_u64),
            Some(PROBE_DEFAULT)
        );
        let low = parse(&sample_config(r#""probe_interval_secs":0,"#)).unwrap();
        assert!(check(&low, main_fields())
            .unwrap_err()
            .message
            .contains("out of bounds"));
        let high = parse(&sample_config(r#""probe_interval_secs":3601,"#)).unwrap();
        assert!(check(&high, main_fields())
            .unwrap_err()
            .message
            .contains("out of bounds"));
        let formatted = format_document(&present).unwrap();
        assert!(formatted.starts_with("{\n"));
        assert!(formatted.contains("\"probe_interval_secs\": 5"));
        assert_eq!(parse(&formatted).unwrap(), present);
    }

    fn with_board(detect: u64) -> String {
        let base = sample_config("");
        let mut text = String::from(&base[..base.len() - 1]);
        text.push_str(",\"board\":{\"detect\":");
        text.push_str(&alloc::format!("{detect}"));
        text.push_str("}}");
        text
    }

    #[test]
    fn board_detect_is_optional_and_only_zero_or_one() {
        let missing = parse(&sample_config("")).unwrap();
        check(&missing, main_fields()).unwrap();
        check(&parse(&with_board(0)).unwrap(), main_fields()).unwrap();
        check(&parse(&with_board(1)).unwrap(), main_fields()).unwrap();
        let err = check(&parse(&with_board(2)).unwrap(), main_fields()).unwrap_err();
        assert!(err.message.contains("out of bounds"));
    }
}
