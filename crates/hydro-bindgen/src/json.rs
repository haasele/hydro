//! Minimal, dependency-free JSON parser for `clang -ast-dump=json` output.
//!
//! The AST dump is large, so the parser is allocation-conscious but strict:
//! it accepts exactly RFC 8259 JSON, preserves numbers as their source text
//! (clang emits integers wider than f64 can represent) and never panics on
//! malformed input.

use std::fmt;

/// A parsed JSON value. Objects keep their member order, which makes
/// diagnostics deterministic.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Raw number token, e.g. `42`, `-7` or `1.5e3`.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Object member lookup; the first matching key wins (clang never
    /// duplicates keys).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(values) => Some(values),
            _ => None,
        }
    }

    /// Parse a number token as `u64`; `None` when it is not an unsigned
    /// integer that fits.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Number(token) => token.parse::<u64>().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(token) => token.parse::<i64>().ok(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

/// Parse a complete JSON document. Trailing whitespace is allowed; any other
/// trailing byte is an error.
pub fn parse(input: &str) -> Result<Json, JsonError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &'static str) -> JsonError {
        JsonError {
            offset: self.pos,
            message,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), JsonError> {
        if self.bump() == Some(byte) {
            Ok(())
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expected a JSON value")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number fraction"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // The slice is ASCII by construction.
        let token = std::str::from_utf8(&self.bytes[start..self.pos])
            .expect("number token is ASCII")
            .to_owned();
        Ok(Json::Number(token))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => return Ok(out),
                Some(b'\\') => self.escape(&mut out)?,
                Some(byte) if byte < 0x20 => {
                    return Err(self.error("control character in string"));
                }
                Some(_) => {
                    // Non-ASCII bytes are only valid as UTF-8 sequences; let
                    // the standard library validate the full run.
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while end < self.bytes.len()
                        && self.bytes[end] != b'"'
                        && self.bytes[end] != b'\\'
                        && self.bytes[end] >= 0x20
                    {
                        end += 1;
                    }
                    match std::str::from_utf8(&self.bytes[start..end]) {
                        Ok(chunk) => {
                            out.push_str(chunk);
                            self.pos = end;
                        }
                        Err(_) => return Err(self.error("invalid UTF-8 in string")),
                    }
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        match self.bump() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{0008}'),
            Some(b'f') => out.push('\u{000C}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => {
                let first = self.hex4()?;
                let scalar = if (0xD800..=0xDBFF).contains(&first) {
                    if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                        return Err(self.error("unpaired UTF-16 surrogate"));
                    }
                    let second = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(self.error("invalid UTF-16 surrogate pair"));
                    }
                    0x1_0000 + (((first - 0xD800) as u32) << 10) + (second - 0xDC00) as u32
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(self.error("unpaired UTF-16 surrogate"));
                } else {
                    first as u32
                };
                match char::from_u32(scalar) {
                    Some(ch) => out.push(ch),
                    None => return Err(self.error("invalid Unicode escape")),
                }
            }
            Some(_) => return Err(self.error("invalid escape sequence")),
            None => return Err(self.error("unterminated escape sequence")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, JsonError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.bump() {
                Some(byte @ b'0'..=b'9') => byte - b'0',
                Some(byte @ b'a'..=b'f') => byte - b'a' + 10,
                Some(byte @ b'A'..=b'F') => byte - b'A' + 10,
                _ => return Err(self.error("invalid \\u escape")),
            };
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => return Ok(Json::Array(values)),
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected object key"));
            }
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.value()?;
            members.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => return Ok(Json::Object(members)),
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_and_containers() {
        let value = parse(r#"{"a":[1,2.5e3,true,null,"x"],"b":false}"#).unwrap();
        assert_eq!(value.get("b"), Some(&Json::Bool(false)));
        let array = value.get("a").unwrap().as_array().unwrap();
        assert_eq!(array[0].as_u64(), Some(1));
        assert_eq!(array[1], Json::Number("2.5e3".into()));
        assert_eq!(array[2], Json::Bool(true));
        assert_eq!(array[3], Json::Null);
        assert_eq!(array[4].as_str(), Some("x"));
    }

    #[test]
    fn decodes_escapes_and_surrogates() {
        let value = parse(r#""a\"b\\c\/d\n\u00e9\ud83d\ude00""#).unwrap();
        assert_eq!(value.as_str().unwrap(), "a\"b\\c/d\n\u{e9}\u{1F600}");
    }

    #[test]
    fn preserves_wide_integer_tokens() {
        let value = parse("18446744073709551615").unwrap();
        assert_eq!(value.as_u64(), Some(u64::MAX));
        assert_eq!(value.as_i64(), None);
    }

    #[test]
    fn rejects_malformed_documents() {
        for input in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "true false",
            "\"unterminated",
            "\"bad\\q\"",
            "\"\\ud800\"",
            "01",
            "1.",
            "nul",
        ] {
            assert!(parse(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn error_reports_offset() {
        let error = parse("[1, x]").unwrap_err();
        assert!(error.offset >= 1);
        assert!(error.to_string().contains("byte"));
    }
}
