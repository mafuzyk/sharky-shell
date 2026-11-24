//! Strict, dependency-free JSON and base64 for the agent session protocol.
//!
//! The design (§9.2) rejects a parser dependency, so this module is the whole
//! of the request-side JSON story: one value per line, one pass over bytes, no
//! recursion beyond [`MAX_DEPTH`].
//!
//! Contract, in the strict direction:
//!
//! * The input must be valid UTF-8 before parsing starts; a bad byte is
//!   [`JsonErrorKind::Utf8`], never a lossy replacement.
//! * Exactly one value per line. Leading/trailing JSON whitespace is allowed,
//!   any other trailing byte is [`JsonErrorKind::TrailingBytes`]. A UTF-8 BOM is
//!   not whitespace here and is rejected.
//! * Container nesting is capped at [`MAX_DEPTH`] levels; the 33rd rejected as
//!   [`JsonErrorKind::Depth`].
//! * Strings reject raw control characters (`< 0x20`) and every escape RFC 8259
//!   does not define. `\u` accepts surrogate pairs and rejects lone or
//!   mismatched surrogates rather than emitting replacement characters.
//! * Numbers are integers only: `-?(0|[1-9][0-9]*)` parsed as `i64`. No protocol
//!   field is a float (`schema_version`, `timeout_ms`, `wait_ms`, `grace_ms` and
//!   the limits are all integers), so fractional/exponential forms and
//!   out-of-range values are all [`JsonErrorKind::Number`]. Leading zeros are
//!   rejected as numbers too. The value type therefore has no float variant.
//! * Duplicate object keys keep the first position and the last value wins,
//!   matching the usual "last one wins" JSON reader policy.
//!
//! Every parse failure maps to the single stable protocol code `INVALID_JSON`
//! (design §3.5); the kind exists so tests and the framer can distinguish them,
//! and the framer decides whether a truncated frame at EOF is `PARTIAL_REQUEST`.
//!
//! Base64 is RFC 4648 standard alphabet with mandatory padding on decode
//! (`max_stdin_bytes` payloads arrive already encoded). Decoding is strict about
//! length, alphabet and padding placement but ignores non-canonical trailing
//! bits, which do not change the decoded bytes.
//!
//! The wire types at the bottom mirror design §3.3/§3.4 so the session driver
//! can build responses without re-deriving the envelope. Request decoding and
//! field validation belong to `agent::protocol` (implementation order step 3).

use std::collections::HashMap;
use std::fmt;

/// The only protocol version this build speaks.
pub const SCHEMA_VERSION: i64 = 1;

/// Maximum number of nested arrays/objects, per design §9.2.
pub const MAX_DEPTH: usize = 32;

/// Maximum `id` length advertised by `ready.limits`.
pub const MAX_ID_BYTES: usize = 256;

/// Default `limits.max_parallel_tasks`: the bounded task queue of a parallel
/// request (design §4). Enforced per request, so it does not need room to grow
/// dynamically.
pub const MAX_PARALLEL_TASKS: usize = 128;

/// Default `limits.max_parallel`: the largest `max_concurrency` a request may
/// ask for (design §4). It bounds *tasks*, not processes.
pub const MAX_PARALLEL: usize = 32;

/// Default `limits.max_parallel_output_bytes`: the rendered-byte budget of one
/// `type:"parallel"` response across every task stream (design §6, 8 MiB).
pub const MAX_PARALLEL_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Whether this build honors `op:"parallel"` end to end.
///
/// The encoder, decoder, limits and the wait-all/fail-fast executors exist, so
/// the op and its features are advertised. Design §3.4 and §D10 allow `ops`/`features`
/// to list only what the driver actually honors, so each capability has its own
/// flag instead of one all-or-nothing switch.
pub const PARALLEL_AVAILABLE: bool = true;

/// Whether `cancel` can address one task of a running group
/// (`{"op":"cancel","target":"p1","task":"diff"}`) or the whole group, with the
/// `reason` vocabulary. The supervisor's parallel table (design U4) arms one
/// `Pending` per task and answers `CANCEL_UNKNOWN_ID`/`CANCEL_TOO_LATE` against
/// the slots, so the feature is advertised only while that path is intact.
pub const PARALLEL_TASK_CANCEL_AVAILABLE: bool = true;

/// Whether this build honors `op:"workflow"` end to end.
pub const WORKFLOW_AVAILABLE: bool = true;

/// Granular workflow capabilities advertised in `ready.protocol.features`
/// (design §2.5). Each flips only with the unit that enforces it.
pub const WORKFLOW_RETRY_AVAILABLE: bool = true;
pub const WORKFLOW_CONDITION_AVAILABLE: bool = true;
pub const WORKFLOW_LOCK_AVAILABLE: bool = true;
pub const WORKFLOW_TASK_CANCEL_AVAILABLE: bool = true;

/// Layer 1 Code Mode capabilities advertised in `ready.protocol.features`.
pub const ARGV_AVAILABLE: bool = true;
pub const EXEC_ISOLATED_AVAILABLE: bool = true;
pub const ENV_REDACTION_AVAILABLE: bool = true;
pub const OUTPUT_PROJECTION_AVAILABLE: bool = true;
pub const CAPTURE_STORE_AVAILABLE: bool = true;

/// Default `limits.max_capture_store_entries`: bounded FIFO history retained in
/// memory for `op:"read_capture"`.
pub const MAX_CAPTURE_STORE_ENTRIES: usize = 32;

/// Default `limits.max_capture_store_bytes`: total byte budget of retained
/// raw captures across entries (16 MiB).
pub const MAX_CAPTURE_STORE_BYTES: usize = 16 * 1024 * 1024;

/// Layer 3 Code Mode availability and execution bounds.
pub const CODE_MODE_AVAILABLE: bool = true;
pub const DEFAULT_CODE_MAX_STEPS: usize = 10_000;
pub const MAX_CODE_STEPS: usize = 100_000;
pub const DEFAULT_CODE_MAX_CALLS: usize = 64;
pub const MAX_CODE_CALLS: usize = 256;

/// Phase G integration availability flags.
pub const PTY_AVAILABLE: bool = true;
pub const STREAMING_OUTPUT_AVAILABLE: bool = true;
pub const CROSS_SESSION_LOCKS_AVAILABLE: bool = true;
pub const OPT_IN_SANDBOX_AVAILABLE: bool = true;
pub const SESSION_RESUME_AVAILABLE: bool = true;
pub const MCP_AVAILABLE: bool = true;

/// Default `limits.max_workflow_attempts`: the retry ceiling of one workflow
/// node (design §5.1). Bounded so `attempts_detail` stays a small response.
pub const MAX_WORKFLOW_ATTEMPTS: usize = 8;

/// Default `limits.max_workflow_backoff_ms`: the cap of one retry delay, fixed
/// or exponential (design §5.3).
pub const MAX_WORKFLOW_BACKOFF_MS: usize = 60_000;

// ---------------------------------------------------------------------- errors

/// Why a line is not a single well-formed JSON value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonErrorKind {
    /// Nothing but whitespace (or nothing at all) was supplied.
    Empty,
    /// The bytes are not valid UTF-8.
    Utf8,
    /// The input ended in the middle of a value.
    UnexpectedEnd,
    /// A byte appeared where the grammar did not allow it.
    UnexpectedByte,
    /// A complete value was followed by non-whitespace bytes.
    TrailingBytes,
    /// Container nesting exceeded [`MAX_DEPTH`].
    Depth,
    /// A raw control character (`< 0x20`) appeared inside a string.
    ControlChar,
    /// An unknown, truncated or malformed `\` escape.
    Escape,
    /// A lone or mismatched UTF-16 surrogate in a `\u` escape.
    LoneSurrogate,
    /// A number is malformed, non-integral or outside `i64`.
    Number,
}

/// A parse failure with the byte offset where it was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError {
    pub kind: JsonErrorKind,
    pub offset: usize,
}

impl JsonError {
    fn new(kind: JsonErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }

    /// Protocol error code for this failure (design §3.5).
    pub fn code(&self) -> &'static str {
        "INVALID_JSON"
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            JsonErrorKind::Empty => "empty JSON value",
            JsonErrorKind::Utf8 => "input is not valid UTF-8",
            JsonErrorKind::UnexpectedEnd => "unexpected end of input",
            JsonErrorKind::UnexpectedByte => "unexpected byte",
            JsonErrorKind::TrailingBytes => "trailing bytes after the JSON value",
            JsonErrorKind::Depth => "JSON nesting is too deep",
            JsonErrorKind::ControlChar => "raw control character in a string",
            JsonErrorKind::Escape => "invalid string escape",
            JsonErrorKind::LoneSurrogate => "lone or mismatched UTF-16 surrogate",
            JsonErrorKind::Number => "invalid or out-of-range number",
        };
        write!(f, "{reason} at byte {}", self.offset)
    }
}

impl std::error::Error for JsonError {}

// ----------------------------------------------------------------------- value

/// A JSON value. Objects preserve insertion order; duplicate keys are collapsed
/// by the parser (last value wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Looks up a member of an object; `None` for every other variant.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Compact single-line JSON encoding.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(value) => out.push_str(&value.to_string()),
            Value::String(value) => quote_into(value, out),
            Value::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    value.write_to(out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    quote_into(key, out);
                    out.push(':');
                    value.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

/// Encodes a string as a JSON string literal, the same escaping every response
/// field uses. Kept here so `cli::json_string` and the protocol encoder share
/// one implementation instead of two drifting copies.
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    quote_into(value, &mut out);
    out
}

fn quote_into(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            character if character < '\u{20}' => {
                out.push_str("\\u");
                out.push_str(&format!("{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

// --------------------------------------------------------------------- parsing

/// Parses one line (newline already removed by the framer) as exactly one JSON
/// value.
pub fn parse_line(line: &[u8]) -> Result<Value, JsonError> {
    let text = std::str::from_utf8(line).map_err(|error| {
        JsonError::new(JsonErrorKind::Utf8, error.valid_up_to())
    })?;
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.parse_value_at_top()
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn parse_value_at_top(&mut self) -> Result<Value, JsonError> {
        self.skip_ws();
        if self.at_end() {
            return Err(self.error(JsonErrorKind::Empty));
        }
        let value = self.parse_value(0)?;
        self.skip_ws();
        if !self.at_end() {
            return Err(self.error(JsonErrorKind::TrailingBytes));
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(Value::String),
            Some(b't') => self.parse_keyword(b"true", Value::Bool(true)),
            Some(b'f') => self.parse_keyword(b"false", Value::Bool(false)),
            Some(b'n') => self.parse_keyword(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error(JsonErrorKind::UnexpectedByte)),
            None => Err(self.error(JsonErrorKind::UnexpectedEnd)),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth >= MAX_DEPTH {
            return Err(self.error(JsonErrorKind::Depth));
        }
        self.pos += 1;
        let mut entries: Vec<(String, Value)> = Vec::new();
        // Key -> position in `entries`, valid for this object only. `entries`
        // keeps insertion order (first occurrence wins for position), while the
        // index makes duplicate detection O(1) instead of rescanning the Vec.
        // Keys are cloned once per distinct key; that allocation is bounded by
        // the line size, so parsing stays linear overall. Borrowing keys from
        // `entries` would not work because pushing to the Vec can reallocate.
        let mut index: HashMap<String, usize> = HashMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'"') => {}
                Some(_) => return Err(self.error(JsonErrorKind::UnexpectedByte)),
                None => return Err(self.error(JsonErrorKind::UnexpectedEnd)),
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.unexpected(JsonErrorKind::UnexpectedByte));
            }
            self.pos += 1;
            let value = self.parse_value(depth + 1)?;
            match index.get(&key) {
                Some(&position) => entries[position].1 = value,
                None => {
                    index.insert(key.clone(), entries.len());
                    entries.push((key, value));
                }
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(entries));
                }
                Some(_) => return Err(self.error(JsonErrorKind::UnexpectedByte)),
                None => return Err(self.error(JsonErrorKind::UnexpectedEnd)),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth >= MAX_DEPTH {
            return Err(self.error(JsonErrorKind::Depth));
        }
        self.pos += 1;
        let mut values = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(values));
        }
        loop {
            values.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(values));
                }
                Some(_) => return Err(self.error(JsonErrorKind::UnexpectedByte)),
                None => return Err(self.error(JsonErrorKind::UnexpectedEnd)),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        // The caller checked the opening quote.
        self.pos += 1;
        let mut out = String::new();
        let mut run_start = self.pos;
        loop {
            match self.peek() {
                Some(b'"') => {
                    out.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    out.push(self.parse_escape()?);
                    run_start = self.pos;
                }
                Some(byte) if byte < 0x20 => {
                    return Err(self.error(JsonErrorKind::ControlChar));
                }
                Some(_) => self.pos += 1,
                None => return Err(self.error(JsonErrorKind::UnexpectedEnd)),
            }
        }
    }

    fn parse_escape(&mut self) -> Result<char, JsonError> {
        let Some(byte) = self.peek() else {
            return Err(self.error(JsonErrorKind::UnexpectedEnd));
        };
        self.pos += 1;
        match byte {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'/' => Ok('/'),
            b'b' => Ok('\u{08}'),
            b'f' => Ok('\u{0c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => self.parse_unicode_escape(),
            _ => Err(self.error(JsonErrorKind::Escape)),
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let unit = self.parse_hex4()?;
        match unit {
            0xD800..=0xDBFF => {
                if self.peek() != Some(b'\\') {
                    return Err(self.error(JsonErrorKind::LoneSurrogate));
                }
                self.pos += 1;
                if self.peek() != Some(b'u') {
                    return Err(self.error(JsonErrorKind::LoneSurrogate));
                }
                self.pos += 1;
                let low = self.parse_hex4()?;
                if !(0xDC00..=0xDFFF).contains(&low) {
                    return Err(self.error(JsonErrorKind::LoneSurrogate));
                }
                let code = 0x1_0000
                    + ((u32::from(unit) - 0xD800) << 10)
                    + (u32::from(low) - 0xDC00);
                char::from_u32(code).ok_or_else(|| self.error(JsonErrorKind::LoneSurrogate))
            }
            0xDC00..=0xDFFF => Err(self.error(JsonErrorKind::LoneSurrogate)),
            _ => char::from_u32(u32::from(unit))
                .ok_or_else(|| self.error(JsonErrorKind::LoneSurrogate)),
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let Some(byte) = self.peek() else {
                return Err(self.error(JsonErrorKind::Escape));
            };
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a') + 10,
                b'A'..=b'F' => u16::from(byte - b'A') + 10,
                _ => return Err(self.error(JsonErrorKind::Escape)),
            };
            value = (value << 4) | digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return Err(if self.at_end() {
                self.error(JsonErrorKind::UnexpectedEnd)
            } else {
                self.error(JsonErrorKind::UnexpectedByte)
            });
        }
        let digits = &self.bytes[digits_start..self.pos];
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(self.error(JsonErrorKind::Number));
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(self.error(JsonErrorKind::Number));
        }
        self.text[start..self.pos]
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| self.error(JsonErrorKind::Number))
    }

    fn parse_keyword(&mut self, word: &[u8], value: Value) -> Result<Value, JsonError> {
        let remaining = &self.bytes[self.pos..];
        if remaining.starts_with(word) {
            self.pos += word.len();
            return Ok(value);
        }
        if word.starts_with(remaining) {
            Err(self.error(JsonErrorKind::UnexpectedEnd))
        } else {
            Err(self.error(JsonErrorKind::UnexpectedByte))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn error(&self, kind: JsonErrorKind) -> JsonError {
        JsonError::new(kind, self.pos)
    }

    fn unexpected(&self, kind: JsonErrorKind) -> JsonError {
        if self.at_end() {
            self.error(JsonErrorKind::UnexpectedEnd)
        } else {
            self.error(kind)
        }
    }
}

// ---------------------------------------------------------------------- base64

/// Failure of [`base64_decode`]; the protocol reports it as `INVALID_ENCODING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64Error {
    /// The input length is not a multiple of four.
    Length,
    /// A byte outside the standard alphabet appeared.
    Alphabet,
    /// `=` appeared anywhere but at the end of the last group.
    Padding,
}

impl Base64Error {
    /// Protocol error code for this failure (design §3.5).
    pub fn code(&self) -> &'static str {
        "INVALID_ENCODING"
    }
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Base64Error::Length => "base64 input length is not a multiple of four",
            Base64Error::Alphabet => "base64 input contains a character outside the alphabet",
            Base64Error::Padding => "base64 padding is misplaced",
        };
        f.write_str(reason)
    }
}

impl std::error::Error for Base64Error {}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let packed = (first << 16) | (second << 8) | third;
        out.push(BASE64_ALPHABET[(packed >> 18) as usize & 0x3f] as char);
        out.push(BASE64_ALPHABET[(packed >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[(packed >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[packed as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decodes standard base64, requiring padding. Non-canonical trailing bits are
/// discarded, exactly as the bytes require them to be.
pub fn base64_decode(input: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(Base64Error::Length);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let groups = bytes.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    for (index, group) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == groups;
        if group[0] == b'=' || group[1] == b'=' {
            return Err(Base64Error::Padding);
        }
        if group[2] == b'=' {
            if group[3] != b'=' || !last {
                return Err(Base64Error::Padding);
            }
        } else if group[3] == b'=' && !last {
            return Err(Base64Error::Padding);
        }
        let first = u32::from(base64_digit(group[0])?);
        let second = u32::from(base64_digit(group[1])?);
        let third = if group[2] == b'=' {
            0
        } else {
            u32::from(base64_digit(group[2])?)
        };
        let fourth = if group[3] == b'=' {
            0
        } else {
            u32::from(base64_digit(group[3])?)
        };
        let packed = (first << 18) | (second << 12) | (third << 6) | fourth;
        out.push((packed >> 16) as u8);
        if group[2] != b'=' {
            out.push((packed >> 8) as u8);
        }
        if group[3] != b'=' {
            out.push(packed as u8);
        }
    }
    Ok(out)
}

fn base64_digit(byte: u8) -> Result<u8, Base64Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Base64Error::Alphabet),
    }
}

// ------------------------------------------------------------------ wire types

fn request_head(id: &str, op: &str) -> String {
    let mut out = String::from("{\"schema_version\":");
    out.push_str(&SCHEMA_VERSION.to_string());
    out.push_str(",\"id\":");
    quote_into(id, &mut out);
    out.push_str(",\"op\":\"");
    out.push_str(op);
    out.push('"');
    out
}

fn response_head(kind: &str) -> String {
    let mut out = String::from("{\"schema_version\":");
    out.push_str(&SCHEMA_VERSION.to_string());
    out.push_str(",\"type\":\"");
    out.push_str(kind);
    out.push('"');
    out
}

/// Like [`response_head`], but always carries `id`, as `null` when the response
/// cannot be attributed (design §3.1/§6.3).
fn response_head_with_id(kind: &str, id: Option<&str>) -> String {
    let mut out = String::from("{\"schema_version\":");
    out.push_str(&SCHEMA_VERSION.to_string());
    out.push_str(",\"id\":");
    match id {
        Some(id) => quote_into(id, &mut out),
        None => out.push_str("null"),
    }
    out.push_str(",\"type\":\"");
    out.push_str(kind);
    out.push('"');
    out
}

fn add_str(out: &mut String, key: &str, value: &str) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    quote_into(value, out);
}

fn add_int(out: &mut String, key: &str, value: i64) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&value.to_string());
}

fn add_bool(out: &mut String, key: &str, value: bool) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    out.push_str(if value { "true" } else { "false" });
}

fn add_value(out: &mut String, key: &str, value: &Value) {
    out.push_str(",\"");
    out.push_str(key);
    out.push_str("\":");
    value.write_to(out);
}

fn close_object(mut out: String) -> String {
    out.push('}');
    out
}

/// A request line, per design §3.3. Encoding only: the driver decodes incoming
/// lines through `Value`, and `agent::protocol` owns the field validation that
/// yields `INVALID_REQUEST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Exec(ExecRequest),
    State(StateRequest),
    Cancel(CancelRequest),
    Close(CloseRequest),
    Parallel(ParallelRequest),
}

impl Request {
    pub fn encode(&self) -> String {
        match self {
            Request::Exec(request) => request.encode(),
            Request::State(request) => request.encode(),
            Request::Cancel(request) => request.encode(),
            Request::Close(request) => request.encode(),
            Request::Parallel(request) => request.encode(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub id: String,
    pub command: String,
    pub stdin: Option<Stdin>,
    pub timeout_ms: Option<i64>,
    pub env_delta: bool,
    pub tty: bool,
    pub stream: bool,
    pub lock: Option<String>,
    pub sandbox: Option<Value>,
}

impl ExecRequest {
    pub fn new(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            stdin: None,
            timeout_ms: None,
            env_delta: true,
            tty: false,
            stream: false,
            lock: None,
            sandbox: None,
        }
    }

    fn encode(&self) -> String {
        let mut out = request_head(&self.id, "exec");
        add_str(&mut out, "command", &self.command);
        if let Some(stdin) = &self.stdin {
            match stdin {
                Stdin::Utf8(data) => add_str(&mut out, "stdin", data),
                Stdin::Base64(data) => {
                    add_str(&mut out, "stdin", &base64_encode(data));
                    add_str(&mut out, "stdin_encoding", "base64");
                }
            }
        }
        if let Some(timeout_ms) = self.timeout_ms {
            add_int(&mut out, "timeout_ms", timeout_ms);
        }
        add_bool(&mut out, "env_delta", self.env_delta);
        if self.tty {
            add_bool(&mut out, "tty", true);
        }
        if self.stream {
            add_bool(&mut out, "stream", true);
        }
        if let Some(lock) = &self.lock {
            add_str(&mut out, "lock", lock);
        }
        if let Some(sandbox) = &self.sandbox {
            out.push_str(",\"sandbox\":");
            out.push_str(&sandbox.encode());
        }
        close_object(out)
    }
}

/// Per-request stdin payload, either literal UTF-8 or raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdin {
    Utf8(String),
    Base64(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRequest {
    pub id: String,
    pub include: Vec<StateSection>,
}

impl StateRequest {
    fn encode(&self) -> String {
        let mut out = request_head(&self.id, "state");
        out.push_str(",\"include\":[");
        for (index, section) in self.include.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            quote_into(section.as_str(), &mut out);
        }
        out.push(']');
        close_object(out)
    }
}

/// Sections a client may ask `state` to include (design §3.3/§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateSection {
    Env,
    Variables,
    Functions,
    Aliases,
    Traps,
    Jobs,
}

impl StateSection {
    pub fn as_str(&self) -> &'static str {
        match self {
            StateSection::Env => "env",
            StateSection::Variables => "variables",
            StateSection::Functions => "functions",
            StateSection::Aliases => "aliases",
            StateSection::Traps => "traps",
            StateSection::Jobs => "jobs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    pub id: String,
    pub target: String,
    /// One task of a parallel request; absent cancels the whole request.
    pub task: Option<String>,
    pub grace_ms: Option<i64>,
}

impl CancelRequest {
    fn encode(&self) -> String {
        let mut out = request_head(&self.id, "cancel");
        add_str(&mut out, "target", &self.target);
        if let Some(task) = &self.task {
            add_str(&mut out, "task", task);
        }
        if let Some(grace_ms) = self.grace_ms {
            add_int(&mut out, "grace_ms", grace_ms);
        }
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRequest {
    pub id: String,
    pub wait_ms: Option<i64>,
}

impl CloseRequest {
    fn encode(&self) -> String {
        let mut out = request_head(&self.id, "close");
        if let Some(wait_ms) = self.wait_ms {
            add_int(&mut out, "wait_ms", wait_ms);
        }
        close_object(out)
    }
}

// ------------------------------------------------------------- parallel (D1)

/// Group failure policy of `op:"parallel"` (design §2.1). Default `wait-all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelPolicy {
    WaitAll,
    FailFast,
}

impl ParallelPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParallelPolicy::WaitAll => "wait-all",
            ParallelPolicy::FailFast => "fail-fast",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelRequest {
    pub id: String,
    pub policy: ParallelPolicy,
    pub max_concurrency: usize,
    /// Group wall clock, counted from the first spawn; `None` means unbounded.
    pub timeout_ms: Option<i64>,
    pub tasks: Vec<TaskRequest>,
}

impl ParallelRequest {
    fn encode(&self) -> String {
        let mut out = request_head(&self.id, "parallel");
        add_str(&mut out, "policy", self.policy.as_str());
        add_int(&mut out, "max_concurrency", self.max_concurrency as i64);
        if let Some(timeout_ms) = self.timeout_ms {
            add_int(&mut out, "timeout_ms", timeout_ms);
        }
        out.push_str(",\"tasks\":[");
        for (index, task) in self.tasks.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&task.encode());
        }
        out.push(']');
        close_object(out)
    }
}

/// One task of a `parallel` request (design §2.1). `env` is applied on top of
/// the session snapshot inside the child; `None` unsets a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRequest {
    pub id: String,
    pub command: String,
    pub stdin: Option<Stdin>,
    pub timeout_ms: Option<i64>,
    pub cwd: Option<String>,
    pub env: Vec<(String, Option<String>)>,
    /// Defaults to `false`, unlike `exec` (design §2.1).
    pub env_delta: bool,
    pub tty: bool,
    pub sandbox: Option<Value>,
}

impl TaskRequest {
    fn encode(&self) -> String {
        let mut out = String::from("{\"id\":");
        quote_into(&self.id, &mut out);
        add_str(&mut out, "command", &self.command);
        if let Some(stdin) = &self.stdin {
            match stdin {
                Stdin::Utf8(data) => add_str(&mut out, "stdin", data),
                Stdin::Base64(data) => {
                    add_str(&mut out, "stdin", &base64_encode(data));
                    add_str(&mut out, "stdin_encoding", "base64");
                }
            }
        }
        if let Some(timeout_ms) = self.timeout_ms {
            add_int(&mut out, "timeout_ms", timeout_ms);
        }
        if let Some(cwd) = &self.cwd {
            add_str(&mut out, "cwd", cwd);
        }
        if !self.env.is_empty() {
            out.push_str(",\"env\":{");
            for (index, (name, value)) in self.env.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                quote_into(name, &mut out);
                out.push(':');
                match value {
                    Some(value) => quote_into(value, &mut out),
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        if self.env_delta {
            add_bool(&mut out, "env_delta", true);
        }
        if self.tty {
            add_bool(&mut out, "tty", true);
        }
        if let Some(sandbox) = &self.sandbox {
            out.push_str(",\"sandbox\":");
            out.push_str(&sandbox.encode());
        }
        close_object(out)
    }
}

/// What happens to the **dependents** of a workflow node that did not succeed
/// (workflow design §4.3). Orthogonal to [`ParallelPolicy`], which decides
/// whether a failure stops the whole group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Dependents are skipped (the default).
    Skip,
    /// The run stops; nodes that never became runnable are `blocked`.
    Fail,
    /// Dependents run anyway, as if the edge did not exist.
    Continue,
}

impl FailurePolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailurePolicy::Skip => "skip",
            FailurePolicy::Fail => "fail",
            FailurePolicy::Continue => "continue",
        }
    }
}

/// The single `exit_code` operator of a workflow `condition` (workflow design §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitCodeMatcher {
    Eq(i64),
    Ne(i64),
    /// `1..=8` statuses, so the gate stays a lookup and not an expression.
    In(Vec<i64>),
}

/// A response line, per design §3.4. Every variant except `Ready` echoes the
/// request `id`; protocol errors that cannot be attributed carry `null`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // encoded once and written immediately
pub enum Response {
    Ready(Ready),
    StreamChunk(StreamChunk),
    Result(ExecResult),
    Parallel(ParallelResult),
    Workflow(WorkflowResult),
    State(StateResult),
    Capture(CaptureResult),
    Code(CodeResult),
    Ack(Ack),
    Closed(Closed),
    Error(ErrorResponse),
}

impl Response {
    pub fn encode(&self) -> String {
        match self {
            Response::Ready(response) => response.encode(),
            Response::StreamChunk(response) => response.encode(),
            Response::Result(response) => response.encode(),
            Response::Parallel(response) => response.encode(),
            Response::Workflow(response) => response.encode(),
            Response::State(response) => response.encode(),
            Response::Capture(response) => response.encode(),
            Response::Code(response) => response.encode(),
            Response::Ack(response) => response.encode(),
            Response::Closed(response) => response.encode(),
            Response::Error(response) => response.encode(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub session: String,
    pub pid: i64,
    pub shell_version: String,
    pub protocol: ProtocolInfo,
}

impl Ready {
    pub fn new(session: impl Into<String>, pid: i64, shell_version: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            pid,
            shell_version: shell_version.into(),
            protocol: ProtocolInfo::default(),
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head("ready");
        add_str(&mut out, "session", &self.session);
        add_int(&mut out, "pid", self.pid);
        add_str(&mut out, "shell_version", &self.shell_version);
        out.push_str(",\"protocol\":");
        out.push_str(&self.protocol.encode());
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub versions: Vec<i64>,
    pub ops: Vec<String>,
    pub features: Vec<String>,
    pub limits: Limits,
    pub cancel_grace_ms: Option<i64>,
}

impl Default for ProtocolInfo {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl ProtocolInfo {
    /// The advertised contract for `limits`. `ready` lists only what the
    /// driver honors (design §3.4/§D10), so the parallel entries are gated on
    /// the availability constants; the limits are advertised regardless,
    /// because the decoder already enforces them and a client must not
    /// hardcode them.
    pub fn new(limits: Limits) -> Self {
        let mut ops: Vec<String> = ["exec", "state", "cancel", "close"]
            .iter()
            .map(|op| (*op).to_string())
            .collect();
        let mut features: Vec<String> = [
            "stdin_payload",
            "env_delta",
            "binary_base64",
            "timeout",
            "cancel",
            "process_groups",
        ]
        .iter()
        .map(|feature| (*feature).to_string())
        .collect();
        if PARALLEL_AVAILABLE {
            ops.push("parallel".to_string());
            features.push("parallel".to_string());
            features.push("parallel_env_delta".to_string());
            if PARALLEL_TASK_CANCEL_AVAILABLE {
                features.push("parallel_task_cancel".to_string());
            }
        }
        if WORKFLOW_AVAILABLE {
            ops.push("workflow".to_string());
            features.push("workflow".to_string());
            if WORKFLOW_RETRY_AVAILABLE {
                features.push("workflow_retry".to_string());
            }
            if WORKFLOW_CONDITION_AVAILABLE {
                features.push("workflow_condition".to_string());
            }
            if WORKFLOW_LOCK_AVAILABLE {
                features.push("workflow_lock".to_string());
            }
            if WORKFLOW_TASK_CANCEL_AVAILABLE {
                features.push("workflow_task_cancel".to_string());
            }
        }
        if ARGV_AVAILABLE {
            features.push("argv".to_string());
        }
        if EXEC_ISOLATED_AVAILABLE {
            features.push("exec_isolated".to_string());
        }
        if ENV_REDACTION_AVAILABLE {
            features.push("env_redaction".to_string());
        }
        if OUTPUT_PROJECTION_AVAILABLE {
            features.push("output_projection".to_string());
        }
        if CAPTURE_STORE_AVAILABLE {
            ops.push("read_capture".to_string());
            features.push("capture_store".to_string());
        }
        if CODE_MODE_AVAILABLE {
            ops.push("code".to_string());
            features.push("code_mode".to_string());
        }
        if PTY_AVAILABLE {
            features.push("pty".to_string());
        }
        if STREAMING_OUTPUT_AVAILABLE {
            features.push("streaming_output".to_string());
        }
        if CROSS_SESSION_LOCKS_AVAILABLE {
            features.push("cross_session_locks".to_string());
        }
        if OPT_IN_SANDBOX_AVAILABLE {
            features.push("opt_in_sandbox".to_string());
        }
        if SESSION_RESUME_AVAILABLE {
            features.push("session_resume".to_string());
        }
        if MCP_AVAILABLE {
            features.push("mcp".to_string());
        }
        Self {
            versions: vec![SCHEMA_VERSION],
            ops,
            features,
            limits,
            cancel_grace_ms: Some(2000),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut limits = vec![
            (
                "max_line_bytes".to_string(),
                Value::Int(self.limits.max_line_bytes as i64),
            ),
            (
                "max_stdin_bytes".to_string(),
                Value::Int(self.limits.max_stdin_bytes as i64),
            ),
            (
                "max_capture_bytes".to_string(),
                Value::Int(self.limits.max_capture_bytes as i64),
            ),
            (
                "max_id_bytes".to_string(),
                Value::Int(self.limits.max_id_bytes as i64),
            ),
            (
                "max_parallel_tasks".to_string(),
                Value::Int(self.limits.max_parallel_tasks as i64),
            ),
            (
                "max_parallel".to_string(),
                Value::Int(self.limits.max_parallel as i64),
            ),
            (
                "max_parallel_output_bytes".to_string(),
                Value::Int(self.limits.max_parallel_output_bytes as i64),
            ),
        ];
        // Advertised only once the executor exists: a limit for a request the
        // build answers with `UNKNOWN_OP` would advertise a capability it
        // cannot honor (design §3.4).
        if WORKFLOW_AVAILABLE {
            limits.push((
                "max_workflow_attempts".to_string(),
                Value::Int(self.limits.max_workflow_attempts as i64),
            ));
            limits.push((
                "max_workflow_backoff_ms".to_string(),
                Value::Int(self.limits.max_workflow_backoff_ms as i64),
            ));
        }
        if CAPTURE_STORE_AVAILABLE {
            limits.push((
                "max_capture_store_entries".to_string(),
                Value::Int(self.limits.max_capture_store_entries as i64),
            ));
            limits.push((
                "max_capture_store_bytes".to_string(),
                Value::Int(self.limits.max_capture_store_bytes as i64),
            ));
        }
        if CODE_MODE_AVAILABLE {
            limits.push((
                "max_code_steps".to_string(),
                Value::Int(self.limits.max_code_steps as i64),
            ));
            limits.push((
                "max_code_calls".to_string(),
                Value::Int(self.limits.max_code_calls as i64),
            ));
        }
        let mut entries = vec![
            (
                "versions".to_string(),
                Value::Array(self.versions.iter().copied().map(Value::Int).collect()),
            ),
            (
                "ops".to_string(),
                Value::Array(self.ops.iter().cloned().map(Value::String).collect()),
            ),
            (
                "features".to_string(),
                Value::Array(self.features.iter().cloned().map(Value::String).collect()),
            ),
            ("limits".to_string(), Value::Object(limits)),
        ];
        if let Some(grace_ms) = self.cancel_grace_ms {
            entries.push(("cancel_grace_ms".to_string(), Value::Int(grace_ms)));
        }
        Value::Object(entries)
    }

    fn encode(&self) -> String {
        self.to_value().encode()
    }
}

fn write_quoted_list(out: &mut String, values: &[String]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        quote_into(value, out);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_line_bytes: usize,
    pub max_stdin_bytes: usize,
    pub max_capture_bytes: usize,
    pub max_id_bytes: usize,
    /// Tasks accepted in one `parallel` request (design §4).
    pub max_parallel_tasks: usize,
    /// Largest accepted `parallel.max_concurrency`, and the cap of the driver's
    /// spawn window (design §4).
    pub max_parallel: usize,
    /// Rendered bytes one `parallel` response may hold across all task streams
    /// (design §6).
    pub max_parallel_output_bytes: usize,
    /// Largest `tasks[].retry.max_attempts` of a `workflow` request (design §5).
    pub max_workflow_attempts: usize,
    /// Cap of one workflow retry delay, including the exponential growth
    /// (design §5.3).
    pub max_workflow_backoff_ms: usize,
    /// Bounded FIFO entries kept in the session capture store.
    pub max_capture_store_entries: usize,
    /// Total retained raw bytes across entries in the session capture store.
    pub max_capture_store_bytes: usize,
    /// Hard ceiling on `max_steps` for `op:"code"`.
    pub max_code_steps: usize,
    /// Hard ceiling on `max_calls` for `op:"code"`.
    pub max_code_calls: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_stdin_bytes: 1024 * 1024,
            max_capture_bytes: 4 * 1024 * 1024,
            max_id_bytes: MAX_ID_BYTES,
            max_parallel_tasks: MAX_PARALLEL_TASKS,
            max_parallel: MAX_PARALLEL,
            max_parallel_output_bytes: MAX_PARALLEL_OUTPUT_BYTES,
            max_workflow_attempts: MAX_WORKFLOW_ATTEMPTS,
            max_workflow_backoff_ms: MAX_WORKFLOW_BACKOFF_MS,
            max_capture_store_entries: MAX_CAPTURE_STORE_ENTRIES,
            max_capture_store_bytes: MAX_CAPTURE_STORE_BYTES,
            max_code_steps: MAX_CODE_STEPS,
            max_code_calls: MAX_CODE_CALLS,
        }
    }
}

/// Metadata summarizing runtime-side `output` projection on UTF-8 streams.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectionSummary {
    pub stdout_total_lines: Option<usize>,
    pub stdout_returned_lines: Option<usize>,
    pub stdout_matched_lines: Option<usize>,
    pub stderr_total_lines: Option<usize>,
    pub stderr_returned_lines: Option<usize>,
    pub stderr_matched_lines: Option<usize>,
}

impl ProjectionSummary {
    pub fn is_empty(&self) -> bool {
        self.stdout_total_lines.is_none()
            && self.stdout_returned_lines.is_none()
            && self.stdout_matched_lines.is_none()
            && self.stderr_total_lines.is_none()
            && self.stderr_returned_lines.is_none()
            && self.stderr_matched_lines.is_none()
    }

    fn encode_into(&self, out: &mut String) {
        let fields: [(&str, Option<usize>); 6] = [
            ("stdout_total_lines", self.stdout_total_lines),
            ("stdout_returned_lines", self.stdout_returned_lines),
            ("stdout_matched_lines", self.stdout_matched_lines),
            ("stderr_total_lines", self.stderr_total_lines),
            ("stderr_returned_lines", self.stderr_returned_lines),
            ("stderr_matched_lines", self.stderr_matched_lines),
        ];
        out.push('{');
        let mut first = true;
        for (name, val) in fields {
            if let Some(count) = val {
                if !first {
                    out.push(',');
                }
                first = false;
                quote_into(name, out);
                out.push(':');
                out.push_str(&count.to_string());
            }
        }
        out.push('}');
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsDelta {
    pub created: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub id: String,
    pub status: i64,
    pub termination: Termination,
    pub signal: Option<String>,
    pub duration_ms: i64,
    /// Additive microsecond field: same measurement as `duration_ms`, without
    /// the millisecond rounding that loses short commands.
    pub duration_us: i64,
    pub cwd: String,
    pub stdout: Captured,
    pub stderr: Captured,
    pub env_delta: Option<Vec<(String, Option<String>)>>,
    pub pgid: Option<i64>,
    pub capture_id: Option<String>,
    pub projection: Option<ProjectionSummary>,
    pub parsed: Option<Value>,
    pub fs_delta: Option<FsDelta>,
}

impl ExecResult {
    pub fn new(
        id: impl Into<String>,
        status: i64,
        duration_ms: i64,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            status,
            termination: Termination::Exited,
            signal: None,
            duration_ms,
            duration_us: 0,
            cwd: cwd.into(),
            stdout: Captured::default(),
            stderr: Captured::default(),
            env_delta: None,
            pgid: None,
            capture_id: None,
            projection: None,
            parsed: None,
            fs_delta: None,
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("result", Some(&self.id));
        add_int(&mut out, "status", self.status);
        add_str(&mut out, "termination", self.termination.as_str());
        if let Some(signal) = &self.signal {
            add_str(&mut out, "signal", signal);
        }
        add_int(&mut out, "duration_ms", self.duration_ms);
        add_int(&mut out, "duration_us", self.duration_us);
        add_str(&mut out, "cwd", &self.cwd);
        add_stream(&mut out, "stdout", &self.stdout);
        add_stream(&mut out, "stderr", &self.stderr);
        if let Some(delta) = &self.env_delta {
            out.push_str(",\"env_delta\":{");
            for (index, (name, value)) in delta.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                quote_into(name, &mut out);
                out.push(':');
                match value {
                    Some(value) => quote_into(value, &mut out),
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        if let Some(pgid) = self.pgid {
            add_int(&mut out, "pgid", pgid);
        }
        if let Some(capture_id) = &self.capture_id {
            add_str(&mut out, "capture_id", capture_id);
        }
        if let Some(projection) = &self.projection
            && !projection.is_empty()
        {
            out.push_str(",\"projection\":");
            projection.encode_into(&mut out);
        }
        if let Some(parsed) = &self.parsed {
            out.push_str(",\"parsed\":");
            parsed.write_to(&mut out);
        }
        if let Some(fs_delta) = &self.fs_delta {
            out.push_str(",\"fs_delta\":{\"created\":");
            Value::Array(
                fs_delta
                    .created
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            )
            .write_to(&mut out);
            out.push_str(",\"modified\":");
            Value::Array(
                fs_delta
                    .modified
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            )
            .write_to(&mut out);
            out.push_str(",\"removed\":");
            Value::Array(
                fs_delta
                    .removed
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            )
            .write_to(&mut out);
            add_bool(&mut out, "truncated", fs_delta.truncated);
            out.push('}');
        }
        close_object(out)
    }
}

/// How the command ended; `status` carries the numeric side (design §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited,
    Signaled,
    Timeout,
    Cancelled,
    /// Additive for `parallel` (design §2.3): the task never ran. `exec` never
    /// produces it, so no `exec` client can start observing it.
    SpawnError,
    /// Additive for `workflow` (workflow design §2.3): never started because a
    /// dependency did not succeed or `condition` evaluated false.
    Skipped,
}

impl Termination {
    pub fn as_str(&self) -> &'static str {
        match self {
            Termination::Exited => "exited",
            Termination::Signaled => "signaled",
            Termination::Timeout => "timeout",
            Termination::Cancelled => "cancelled",
            Termination::SpawnError => "spawn_error",
            Termination::Skipped => "skipped",
        }
    }
}

/// One captured stream: its bytes and the real size before truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub data: StreamData,
    pub bytes: usize,
    pub truncated: bool,
}

impl Default for Captured {
    fn default() -> Self {
        Self {
            data: StreamData::Utf8(String::new()),
            bytes: 0,
            truncated: false,
        }
    }
}

/// Valid UTF-8 stays a string; anything else is base64 with the exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamData {
    Utf8(String),
    Base64(Vec<u8>),
}

impl StreamData {
    fn encoding(&self) -> &'static str {
        match self {
            StreamData::Utf8(_) => "utf8",
            StreamData::Base64(_) => "base64",
        }
    }

    fn render(&self) -> String {
        match self {
            StreamData::Utf8(text) => text.clone(),
            StreamData::Base64(bytes) => base64_encode(bytes),
        }
    }
}

fn add_stream(out: &mut String, prefix: &str, captured: &Captured) {
    add_str(out, prefix, &captured.data.render());
    add_str(
        out,
        &format!("{prefix}_encoding"),
        captured.data.encoding(),
    );
    add_int(out, &format!("{prefix}_bytes"), captured.bytes as i64);
    add_bool(
        out,
        &format!("{prefix}_truncated"),
        captured.truncated,
    );
}

// ------------------------------------------------------ parallel response (D1)

/// Coarse group outcome; never a substitute for the per-task detail (design
/// §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupState {
    Completed,
    Cancelled,
    Failed,
}

impl GroupState {
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupState::Completed => "completed",
            GroupState::Cancelled => "cancelled",
            GroupState::Failed => "failed",
        }
    }
}

/// Coarse lifecycle outcome of one task (design §2.3/§7). `completed` includes
/// a non-zero exit and a signal death that supervision did not cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Completed,
    Cancelled,
    Timeout,
    SpawnError,
    Skipped,
    Blocked,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Completed => "completed",
            TaskState::Cancelled => "cancelled",
            TaskState::Timeout => "timeout",
            TaskState::SpawnError => "spawn_error",
            TaskState::Skipped => "skipped",
            TaskState::Blocked => "blocked",
        }
    }
}

/// Why supervision ended or omitted a task; present only when it did (design §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskReason {
    FailFast,
    GroupCancel,
    TaskCancel,
    GroupTimeout,
    DependencyFailed,
    ConditionFalse,
    FailurePolicy,
}

impl TaskReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskReason::FailFast => "fail_fast",
            TaskReason::GroupCancel => "group_cancel",
            TaskReason::TaskCancel => "task_cancel",
            TaskReason::GroupTimeout => "group_timeout",
            TaskReason::DependencyFailed => "dependency_failed",
            TaskReason::ConditionFalse => "condition_false",
            TaskReason::FailurePolicy => "failure_policy",
        }
    }
}

/// The per-task error object of design §2.3: `{"code","message"}`, carried on
/// `spawn_error` only. Deliberately not [`ProtocolError`]: a task error never
/// has a `fatal` bit, because it never ends the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskError {
    pub code: ErrorCode,
    pub message: String,
}

/// One response to `op:"parallel"` (design §2.3): the group envelope plus one
/// [`TaskResult`] per requested task, in request order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelResult {
    pub id: String,
    pub policy: ParallelPolicy,
    pub state: GroupState,
    /// Aggregate status by design §2.4: `0`, or the status of the first failed
    /// task in request order.
    pub status: i64,
    pub duration_ms: i64,
    /// Additive microsecond field, mirroring [`ExecResult::duration_us`].
    pub duration_us: i64,
    /// Echoed after validation, so it is the window that was used.
    pub max_concurrency: usize,
    /// Tasks that actually spawned; less than `tasks.len()` only after
    /// `fail-fast`, a cancel or a group deadline.
    pub started_tasks: usize,
    /// True when the response budget cut or dropped any task stream.
    pub output_truncated: bool,
    pub tasks: Vec<TaskResult>,
}

impl ParallelResult {
    pub fn new(id: impl Into<String>, policy: ParallelPolicy) -> Self {
        Self {
            id: id.into(),
            policy,
            state: GroupState::Completed,
            status: 0,
            duration_ms: 0,
            duration_us: 0,
            max_concurrency: 0,
            started_tasks: 0,
            output_truncated: false,
            tasks: Vec::new(),
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("parallel", Some(&self.id));
        add_str(&mut out, "op", "parallel");
        add_str(&mut out, "policy", self.policy.as_str());
        add_str(&mut out, "state", self.state.as_str());
        add_int(&mut out, "status", self.status);
        add_int(&mut out, "duration_ms", self.duration_ms);
        add_int(&mut out, "duration_us", self.duration_us);
        add_int(&mut out, "max_concurrency", self.max_concurrency as i64);
        add_int(&mut out, "started_tasks", self.started_tasks as i64);
        add_bool(&mut out, "output_truncated", self.output_truncated);
        out.push_str(",\"tasks\":[");
        for (index, task) in self.tasks.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            task.encode_into(&mut out);
        }
        out.push(']');
        close_object(out)
    }
}

/// One task of a `type:"parallel"` response: the [`ExecResult`] field set plus
/// `state`, `reason` and `error` (design §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub id: String,
    pub state: TaskState,
    pub termination: Termination,
    pub status: i64,
    pub signal: Option<String>,
    pub reason: Option<TaskReason>,
    pub error: Option<TaskError>,
    pub duration_ms: i64,
    pub duration_us: i64,
    pub cwd: String,
    pub stdout: Captured,
    pub stderr: Captured,
    /// Absent unless the task asked for it (`env_delta` defaults to `false`).
    pub env_delta: Option<Vec<(String, Option<String>)>>,
    /// Why a requested delta is absent, when it is: the child drops it instead
    /// of writing a status record larger than its own bound, and this field is
    /// how the response says so. Absent when the delta was delivered or never
    /// requested; additive, no schema bump.
    pub env_delta_omitted: Option<String>,
    /// The task leader's process group, which is what a cancel signals.
    pub pgid: Option<i64>,
    pub capture_id: Option<String>,
    pub projection: Option<ProjectionSummary>,
}

impl TaskResult {
    pub fn new(id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: TaskState::Completed,
            termination: Termination::Exited,
            status: 0,
            signal: None,
            reason: None,
            error: None,
            duration_ms: 0,
            duration_us: 0,
            cwd: cwd.into(),
            stdout: Captured::default(),
            stderr: Captured::default(),
            env_delta: None,
            env_delta_omitted: None,
            pgid: None,
            capture_id: None,
            projection: None,
        }
    }

    fn encode_into(&self, out: &mut String) {
        out.push('{');
        self.encode_fields_into(out);
        out.push('}');
    }

    fn encode_fields_into(&self, out: &mut String) {
        out.push_str("\"id\":");
        quote_into(&self.id, out);
        add_str(out, "state", self.state.as_str());
        add_str(out, "termination", self.termination.as_str());
        add_int(out, "status", self.status);
        if let Some(signal) = &self.signal {
            add_str(out, "signal", signal);
        }
        if let Some(reason) = self.reason {
            add_str(out, "reason", reason.as_str());
        }
        if let Some(error) = &self.error {
            out.push_str(",\"error\":{\"code\":\"");
            out.push_str(error.code.as_str());
            out.push('"');
            add_str(out, "message", &error.message);
            out.push('}');
        }
        add_int(out, "duration_ms", self.duration_ms);
        add_int(out, "duration_us", self.duration_us);
        add_str(out, "cwd", &self.cwd);
        add_stream(out, "stdout", &self.stdout);
        add_stream(out, "stderr", &self.stderr);
        if let Some(delta) = &self.env_delta {
            out.push_str(",\"env_delta\":{");
            for (index, (name, value)) in delta.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                quote_into(name, out);
                out.push(':');
                match value {
                    Some(value) => quote_into(value, out),
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        if let Some(reason) = &self.env_delta_omitted {
            add_str(out, "env_delta_omitted", reason);
        }
        if let Some(pgid) = self.pgid {
            add_int(out, "pgid", pgid);
        }
        if let Some(capture_id) = &self.capture_id {
            add_str(out, "capture_id", capture_id);
        }
        if let Some(projection) = &self.projection
            && !projection.is_empty()
        {
            out.push_str(",\"projection\":");
            projection.encode_into(out);
        }
    }
}

// ------------------------------------------------------ workflow response (E1)

/// Echo of one node's `condition` plus its evaluation outcome (`"true"`/`"false"`)
/// when evaluated (workflow design §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionEcho {
    pub task: String,
    pub exit_code: Option<ExitCodeMatcher>,
    pub stdout_contains: Option<String>,
    pub stdout_equals: Option<String>,
    pub stderr_contains: Option<String>,
    pub outcome: Option<bool>,
}

impl ConditionEcho {
    fn encode_into(&self, out: &mut String) {
        out.push_str("{\"task\":");
        quote_into(&self.task, out);
        if let Some(exit_code) = &self.exit_code {
            out.push_str(",\"exit_code\":{");
            match exit_code {
                ExitCodeMatcher::Eq(code) => {
                    out.push_str("\"eq\":");
                    out.push_str(&code.to_string());
                }
                ExitCodeMatcher::Ne(code) => {
                    out.push_str("\"ne\":");
                    out.push_str(&code.to_string());
                }
                ExitCodeMatcher::In(codes) => {
                    out.push_str("\"in\":[");
                    for (index, code) in codes.iter().enumerate() {
                        if index > 0 {
                            out.push(',');
                        }
                        out.push_str(&code.to_string());
                    }
                    out.push(']');
                }
            }
            out.push('}');
        }
        if let Some(needle) = &self.stdout_contains {
            add_str(out, "stdout_contains", needle);
        }
        if let Some(expected) = &self.stdout_equals {
            add_str(out, "stdout_equals", expected);
        }
        if let Some(needle) = &self.stderr_contains {
            add_str(out, "stderr_contains", needle);
        }
        if let Some(outcome) = self.outcome {
            add_str(out, "outcome", if outcome { "true" } else { "false" });
        }
        out.push('}');
    }
}

/// Summary of one started attempt on a workflow node (workflow design §9.1).
/// Superseded attempts retain sizes only, never stream text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptDetail {
    pub n: usize,
    pub started_ms: i64,
    pub duration_ms: i64,
    pub status: i64,
    pub termination: Termination,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl AttemptDetail {
    fn encode_into(&self, out: &mut String) {
        out.push_str("{\"n\":");
        out.push_str(&self.n.to_string());
        add_int(out, "started_ms", self.started_ms);
        add_int(out, "duration_ms", self.duration_ms);
        add_int(out, "status", self.status);
        add_str(out, "termination", self.termination.as_str());
        add_int(out, "stdout_bytes", self.stdout_bytes as i64);
        add_int(out, "stderr_bytes", self.stderr_bytes as i64);
        out.push('}');
    }
}

/// One node of a `type:"workflow"` response (workflow design §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTaskResult {
    pub base: TaskResult,
    pub depends_on: Vec<String>,
    pub lock: Option<String>,
    pub condition: Option<ConditionEcho>,
    pub attempts: usize,
    pub attempts_detail: Vec<AttemptDetail>,
}

impl WorkflowTaskResult {
    fn encode_into(&self, out: &mut String) {
        out.push('{');
        self.base.encode_fields_into(out);
        out.push_str(",\"depends_on\":[");
        write_quoted_list(out, &self.depends_on);
        out.push(']');
        if let Some(lock) = &self.lock {
            add_str(out, "lock", lock);
        }
        if let Some(condition) = &self.condition {
            out.push_str(",\"condition\":");
            condition.encode_into(out);
        }
        add_int(out, "attempts", self.attempts as i64);
        out.push_str(",\"attempts_detail\":[");
        for (index, detail) in self.attempts_detail.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            detail.encode_into(out);
        }
        out.push(']');
        out.push('}');
    }
}

/// One response to `op:"workflow"` (workflow design §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResult {
    pub id: String,
    pub policy: ParallelPolicy,
    pub failure_policy: FailurePolicy,
    pub state: GroupState,
    pub status: i64,
    pub duration_ms: i64,
    pub duration_us: i64,
    pub max_concurrency: usize,
    pub started_tasks: usize,
    pub attempts_total: usize,
    pub skipped_tasks: usize,
    pub blocked_tasks: usize,
    pub output_truncated: bool,
    pub order: Vec<String>,
    pub tasks: Vec<WorkflowTaskResult>,
}

impl WorkflowResult {
    pub fn new(
        id: impl Into<String>,
        policy: ParallelPolicy,
        failure_policy: FailurePolicy,
    ) -> Self {
        Self {
            id: id.into(),
            policy,
            failure_policy,
            state: GroupState::Completed,
            status: 0,
            duration_ms: 0,
            duration_us: 0,
            max_concurrency: 0,
            started_tasks: 0,
            attempts_total: 0,
            skipped_tasks: 0,
            blocked_tasks: 0,
            output_truncated: false,
            order: Vec::new(),
            tasks: Vec::new(),
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("workflow", Some(&self.id));
        add_str(&mut out, "op", "workflow");
        add_str(&mut out, "policy", self.policy.as_str());
        add_str(&mut out, "failure_policy", self.failure_policy.as_str());
        add_str(&mut out, "state", self.state.as_str());
        add_int(&mut out, "status", self.status);
        add_int(&mut out, "duration_ms", self.duration_ms);
        add_int(&mut out, "duration_us", self.duration_us);
        add_int(&mut out, "max_concurrency", self.max_concurrency as i64);
        add_int(&mut out, "started_tasks", self.started_tasks as i64);
        add_int(&mut out, "attempts_total", self.attempts_total as i64);
        add_int(&mut out, "skipped_tasks", self.skipped_tasks as i64);
        add_int(&mut out, "blocked_tasks", self.blocked_tasks as i64);
        add_bool(&mut out, "output_truncated", self.output_truncated);
        out.push_str(",\"order\":[");
        write_quoted_list(&mut out, &self.order);
        out.push_str("],\"tasks\":[");
        for (index, task) in self.tasks.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            task.encode_into(&mut out);
        }
        out.push(']');
        close_object(out)
    }
}

/// One slice retrieved from the bounded session capture store via `op:"read_capture"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    pub id: String,
    pub capture_id: String,
    pub stream: String,
    pub data: StreamData,
    pub total_bytes: usize,
    pub stored_bytes: usize,
    pub total_lines: Option<usize>,
    pub line_start: Option<usize>,
    pub returned_lines: Option<usize>,
    pub matched_lines: Option<usize>,
    pub byte_offset: Option<usize>,
    pub truncated: bool,
}

impl CaptureResult {
    fn encode(&self) -> String {
        let mut out = response_head_with_id("capture", Some(&self.id));
        add_str(&mut out, "capture_id", &self.capture_id);
        add_str(&mut out, "stream", &self.stream);
        add_str(&mut out, "encoding", self.data.encoding());
        add_str(&mut out, "data", &self.data.render());
        add_int(&mut out, "total_bytes", self.total_bytes as i64);
        add_int(&mut out, "stored_bytes", self.stored_bytes as i64);
        if let Some(total_lines) = self.total_lines {
            add_int(&mut out, "total_lines", total_lines as i64);
        }
        if let Some(line_start) = self.line_start {
            add_int(&mut out, "line_start", line_start as i64);
        }
        if let Some(returned_lines) = self.returned_lines {
            add_int(&mut out, "returned_lines", returned_lines as i64);
        }
        if let Some(matched_lines) = self.matched_lines {
            add_int(&mut out, "matched_lines", matched_lines as i64);
        }
        if let Some(byte_offset) = self.byte_offset {
            add_int(&mut out, "byte_offset", byte_offset as i64);
        }
        add_bool(&mut out, "truncated", self.truncated);
        close_object(out)
    }
}

/// Truthful execution audit emitted on every `type:"code"` response so script
/// transforms can never conceal failing child executions or raw captures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodeAudit {
    pub calls: usize,
    pub failed_calls: usize,
    pub cancelled_calls: usize,
    pub first_failure_status: Option<i64>,
    pub steps: usize,
    pub captures: Vec<String>,
}

impl CodeAudit {
    fn encode_into(&self, out: &mut String) {
        out.push_str("{\"calls\":");
        out.push_str(&self.calls.to_string());
        add_int(out, "failed_calls", self.failed_calls as i64);
        add_int(out, "cancelled_calls", self.cancelled_calls as i64);
        match self.first_failure_status {
            Some(status) => add_int(out, "first_failure_status", status),
            None => out.push_str(",\"first_failure_status\":null"),
        }
        add_int(out, "steps", self.steps as i64);
        out.push_str(",\"captures\":[");
        write_quoted_list(out, &self.captures);
        out.push_str("]}");
    }
}

/// Single response object for `op:"code"` and `sharky code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeResult {
    pub id: String,
    pub status: i64,
    pub termination: Termination,
    pub duration_ms: i64,
    pub duration_us: i64,
    pub value: Value,
    pub error: Option<String>,
    pub audit: CodeAudit,
}

impl CodeResult {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: 0,
            termination: Termination::Exited,
            duration_ms: 0,
            duration_us: 0,
            value: Value::Null,
            error: None,
            audit: CodeAudit::default(),
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("code", Some(&self.id));
        add_int(&mut out, "status", self.status);
        add_str(&mut out, "termination", self.termination.as_str());
        add_int(&mut out, "duration_ms", self.duration_ms);
        add_int(&mut out, "duration_us", self.duration_us);
        add_value(&mut out, "value", &self.value);
        if let Some(error) = &self.error {
            add_str(&mut out, "error", error);
        }
        out.push_str(",\"audit\":");
        self.audit.encode_into(&mut out);
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateResult {
    pub id: String,
    pub cwd: String,
    pub last_status: i64,
    pub pid: i64,
    pub jobs: Vec<JobSummary>,
    pub options: ShellOptions,
    /// Requested sections (`env`, `variables`, `functions`, `aliases`,
    /// `traps`), already rendered as JSON by the snapshot code.
    pub sections: Vec<(String, Value)>,
}

impl StateResult {
    fn encode(&self) -> String {
        let mut out = response_head_with_id("state", Some(&self.id));
        add_str(&mut out, "cwd", &self.cwd);
        add_int(&mut out, "last_status", self.last_status);
        add_int(&mut out, "pid", self.pid);
        out.push_str(",\"jobs\":[");
        for (index, job) in self.jobs.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            job.encode_into(&mut out);
        }
        out.push_str("],\"options\":");
        self.options.encode_into(&mut out);
        for (name, value) in &self.sections {
            add_value(&mut out, name, value);
        }
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    pub id: i64,
    pub pgid: i64,
    pub pids: Vec<i64>,
    pub command: String,
    pub state: String,
}

impl JobSummary {
    fn encode_into(&self, out: &mut String) {
        let mut job = String::from("{\"id\":");
        job.push_str(&self.id.to_string());
        add_int(&mut job, "pgid", self.pgid);
        job.push_str(",\"pids\":[");
        for (index, pid) in self.pids.iter().enumerate() {
            if index > 0 {
                job.push(',');
            }
            job.push_str(&pid.to_string());
        }
        job.push(']');
        add_str(&mut job, "command", &self.command);
        add_str(&mut job, "state", &self.state);
        out.push_str(&close_object(job));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellOptions {
    pub errexit: bool,
    pub nounset: bool,
    pub pipefail: bool,
    pub noclobber: bool,
    pub xtrace: bool,
}

impl ShellOptions {
    fn encode_into(&self, out: &mut String) {
        out.push_str("{\"errexit\":");
        out.push_str(if self.errexit { "true" } else { "false" });
        add_bool(out, "nounset", self.nounset);
        add_bool(out, "pipefail", self.pipefail);
        add_bool(out, "noclobber", self.noclobber);
        add_bool(out, "xtrace", self.xtrace);
        out.push('}');
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    pub id: String,
    pub stream: String,
    pub seq: i64,
    pub data: Captured,
}

impl StreamChunk {
    fn encode(&self) -> String {
        let mut out = response_head_with_id(&self.stream, Some(&self.id));
        add_int(&mut out, "seq", self.seq);
        add_str(&mut out, "data", &self.data.data.render());
        add_str(&mut out, "encoding", self.data.data.encoding());
        add_int(&mut out, "bytes", self.data.bytes as i64);
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub id: String,
    pub op: String,
    pub outcome: String,
}

impl Ack {
    pub fn delivered(id: impl Into<String>, op: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            op: op.into(),
            outcome: "delivered".to_string(),
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("ack", Some(&self.id));
        add_str(&mut out, "op", &self.op);
        add_str(&mut out, "outcome", &self.outcome);
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub id: Option<String>,
    pub status: i64,
}

impl Closed {
    fn encode(&self) -> String {
        let mut out = response_head_with_id("closed", self.id.as_deref());
        add_int(&mut out, "status", self.status);
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    pub id: Option<String>,
    pub error: ProtocolError,
}

impl ErrorResponse {
    pub fn new(id: Option<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            error: ProtocolError {
                code,
                message: message.into(),
                fatal: code.is_fatal(),
            },
        }
    }

    fn encode(&self) -> String {
        let mut out = response_head_with_id("error", self.id.as_deref());
        let mut error = String::from("{\"code\":\"");
        error.push_str(self.error.code.as_str());
        error.push('"');
        add_str(&mut error, "message", &self.error.message);
        add_bool(&mut error, "fatal", self.error.fatal);
        out.push_str(",\"error\":");
        out.push_str(&close_object(error));
        close_object(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub fatal: bool,
}

/// The stable error taxonomy of design §3.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidJson,
    InvalidRequest,
    UnsupportedSchemaVersion,
    UnknownOp,
    DuplicateId,
    InvalidId,
    MessageTooLarge,
    StdinTooLarge,
    InvalidEncoding,
    QueueFull,
    DuplicateTaskId,
    TooManyTasks,
    SpawnFailed,
    PartialRequest,
    CancelUnknownId,
    CancelTooLate,
    SessionClosed,
    ProtocolFdLost,
    Internal,
    // Additive for `op:"workflow"` (design §2.2): the DAG is refused before
    // anything runs, so the session keeps serving.
    WorkflowCycle,
    InvalidDependency,
    InvalidCondition,
    // Additive for `op:"read_capture"` (Code Mode §1.5): evicted or missing id.
    CaptureNotFound,
    // Additive for 4.0.2 policy guardrails.
    PolicyDenied,
    RequiresApproval,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InvalidJson => "INVALID_JSON",
            ErrorCode::InvalidRequest => "INVALID_REQUEST",
            ErrorCode::UnsupportedSchemaVersion => "UNSUPPORTED_SCHEMA_VERSION",
            ErrorCode::UnknownOp => "UNKNOWN_OP",
            ErrorCode::DuplicateId => "DUPLICATE_ID",
            ErrorCode::InvalidId => "INVALID_ID",
            ErrorCode::MessageTooLarge => "MESSAGE_TOO_LARGE",
            ErrorCode::StdinTooLarge => "STDIN_TOO_LARGE",
            ErrorCode::InvalidEncoding => "INVALID_ENCODING",
            // Additive: the pump queue is full of frames nobody can answer yet,
            // so this request is answered on the spot and may be retried.
            ErrorCode::QueueFull => "QUEUE_FULL",
            // Additive for `op:"parallel"` (design §2.2/§2.3): the first two
            // reject the request, the third labels one task's spawn failure.
            ErrorCode::DuplicateTaskId => "DUPLICATE_TASK_ID",
            ErrorCode::TooManyTasks => "TOO_MANY_TASKS",
            ErrorCode::SpawnFailed => "SPAWN_FAILED",
            ErrorCode::PartialRequest => "PARTIAL_REQUEST",
            ErrorCode::CancelUnknownId => "CANCEL_UNKNOWN_ID",
            ErrorCode::CancelTooLate => "CANCEL_TOO_LATE",
            ErrorCode::SessionClosed => "SESSION_CLOSED",
            ErrorCode::ProtocolFdLost => "PROTOCOL_FD_LOST",
            ErrorCode::Internal => "INTERNAL",
            // Additive workflow codes (design §2.2). Adding a code is forward
            // compatible; changing one is not.
            ErrorCode::WorkflowCycle => "WORKFLOW_CYCLE",
            ErrorCode::InvalidDependency => "INVALID_DEPENDENCY",
            ErrorCode::InvalidCondition => "INVALID_CONDITION",
            ErrorCode::CaptureNotFound => "CAPTURE_NOT_FOUND",
            ErrorCode::PolicyDenied => "POLICY_DENIED",
            ErrorCode::RequiresApproval => "REQUIRES_APPROVAL",
        }
    }

    /// Whether the session must stop after sending this error.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ErrorCode::PartialRequest
                | ErrorCode::SessionClosed
                | ErrorCode::ProtocolFdLost
                | ErrorCode::Internal
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_error_codes_are_stable_and_non_fatal() {
        for (code, text) in [
            (ErrorCode::WorkflowCycle, "WORKFLOW_CYCLE"),
            (ErrorCode::InvalidDependency, "INVALID_DEPENDENCY"),
            (ErrorCode::InvalidCondition, "INVALID_CONDITION"),
        ] {
            assert_eq!(code.as_str(), text);
            // A refused DAG says nothing about the session's health.
            assert!(!code.is_fatal(), "{text} must not end the session");
        }
    }

    #[test]
    fn workflow_limits_and_ops_are_advertised() {
        const { assert!(WORKFLOW_AVAILABLE, "U2 flipped the workflow executor live") };
        let limits = Limits::default();
        assert_eq!(limits.max_workflow_attempts, 8);
        assert_eq!(limits.max_workflow_backoff_ms, 60_000);

        let ready = ProtocolInfo::new(limits).encode();
        assert!(ready.contains("\"workflow\""), "{ready}");
        assert!(ready.contains("max_workflow_attempts"), "{ready}");
        assert!(ready.contains("max_workflow_backoff_ms"), "{ready}");
    }
}
