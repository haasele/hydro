//! `re`: thin wrappers over the `regex` crate with Python-like semantics.
//!
//! Supported: `search`, `match`, `fullmatch`, `findall`, `finditer`, `sub`,
//! `subn`, `split`, `escape` and `compile` (patterns and matches are native
//! objects with the corresponding methods). `compile` accepts the
//! `re.RegexFlag` bits it can honour (`IGNORECASE`, `MULTILINE`, `DOTALL`,
//! `VERBOSE` and their short aliases); the compiled pattern keeps them for
//! every method call. Patterns, subjects and replacements may be `str`,
//! `bytes` or `bytearray`; `bytes` patterns match byte-wise and yield `bytes`
//! slices, and mixing the two domains raises `TypeError` like CPython.
//! Patterns and match objects are immutable and hold no Hydro values. Invalid
//! patterns raise `ValueError`; non-string inputs raise `TypeError`.
//!
//! Bytes sources are stored in `PatternState`/`MatchState` with one `char` per
//! byte (Latin-1). That keeps the existing `String` payloads, makes Python
//! byte offsets equal character indices, and lets the generic `repr` code
//! slice without splitting a character. The [`BYTES_PATTERN`] bit of a
//! pattern's stored flags marks the byte domain; it is masked out of the
//! public `Pattern.flags` attribute.

use regex::{bytes as regex_bytes, Regex, RegexBuilder};

use crate::exc::{self, ATTRIBUTE_ERROR, INDEX_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Marker bit in `PatternState::flags` recording that `source` holds bytes
/// (one `char` per byte) instead of a `str` pattern.
const BYTES_PATTERN: i64 = 1 << 60;

/// State of an `re.compile()` result.
pub(crate) struct PatternState {
    /// The pattern source: a `str` pattern verbatim, or the bytes of a bytes
    /// pattern with one `char` per byte (see the module docs).
    pub(crate) source: String,
    /// The `re.RegexFlag` bits the pattern was compiled with, so every
    /// `Pattern` method recompiles with the same options. The
    /// [`BYTES_PATTERN`] marker is an internal bit and is not exposed.
    pub(crate) flags: i64,
}

/// Pattern argument after normalisation.
enum Source {
    Str(String),
    Bytes(Vec<u8>),
}

/// Subject argument after normalisation; `Bytes` covers `bytes`/`bytearray`.
enum Subject {
    Str(String),
    Bytes(Vec<u8>),
}

/// `re.RegexFlag` bits, with CPython's numeric values.
pub(crate) const TEMPLATE: i64 = 1;
pub(crate) const IGNORECASE: i64 = 2;
pub(crate) const LOCALE: i64 = 4;
pub(crate) const MULTILINE: i64 = 8;
pub(crate) const DOTALL: i64 = 16;
pub(crate) const UNICODE: i64 = 32;
pub(crate) const VERBOSE: i64 = 64;
pub(crate) const DEBUG: i64 = 128;
pub(crate) const ASCII: i64 = 256;

/// Module-level `re.RegexFlag` members (they are plain ints here).
pub(crate) fn constant(name: &str) -> i64 {
    let bits = match name {
        "NOFLAG" => 0,
        "TEMPLATE" | "T" => TEMPLATE,
        "IGNORECASE" | "I" => IGNORECASE,
        "LOCALE" | "L" => LOCALE,
        "MULTILINE" | "M" => MULTILINE,
        "DOTALL" | "S" => DOTALL,
        "UNICODE" | "U" => UNICODE,
        "VERBOSE" | "X" => VERBOSE,
        "DEBUG" => DEBUG,
        "ASCII" | "A" => ASCII,
        // `re.Pattern`/`re.Match` are the cached classes `type()` reports for
        // compiled patterns and match objects; `isinstance` resolves them
        // through the native type-name table.
        "Pattern" => return value::pattern_class(),
        "Match" => return value::match_class(),
        // The spec table only announces the names above; anything else is
        // still reported like a missing module attribute.
        _ => {
            return exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module 're' has no attribute '{name}'"),
            )
        }
    };
    value::encode_int(bits).unwrap_or(NONE)
}

/// State of a successful match: the searched text and byte positions of the
/// whole match and every capture group (`None` for an unmatched group).
///
/// For a `str` subject the positions are Python character offsets. For a
/// bytes subject `text` holds one `char` per byte and the positions are
/// `String` byte offsets (character boundaries), so slicing the encoded text
/// is always safe; the sentinel `Some((usize::MAX, usize::MAX))` appended to
/// `groups` marks the bytes domain.
pub(crate) struct MatchState {
    pub(crate) pattern: String,
    pub(crate) text: String,
    pub(crate) groups: Vec<Option<(usize, usize)>>,
}

/// Sentinel appended to `MatchState::groups` for a bytes match.
const BYTES_MATCH: Option<(usize, usize)> = Some((usize::MAX, usize::MAX));

fn is_bytes_match(state: &MatchState) -> bool {
    state.groups.last().copied() == Some(BYTES_MATCH)
}

/// Number of real groups in a match, excluding the bytes-domain sentinel.
fn match_group_count(state: &MatchState) -> usize {
    if is_bytes_match(state) {
        state.groups.len().saturating_sub(1)
    } else {
        state.groups.len()
    }
}

/// Encodes raw bytes as a `String` with one `char` per byte.
fn latin1_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// Decodes a Latin-1 encoded `String` (see [`latin1_string`]) back to bytes.
/// Characters above `U+00FF` cannot occur in an encoded source; truncating
/// them keeps this total and panic-free.
fn latin1_bytes(text: &str) -> Vec<u8> {
    text.chars().map(|ch| (ch as u32 & 0xFF) as u8).collect()
}

fn is_bytes_pattern(state: &PatternState) -> bool {
    state.flags & BYTES_PATTERN != 0
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn attribute_error(kind: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'{kind}' object has no attribute '{name}'"),
    )
}

fn regex_error(error: regex::Error) -> i64 {
    exc::raise_with_message(VALUE_ERROR, &format!("invalid regular expression: {error}"))
}

/// Returns the pattern source from a `str`, bytes-like or compiled `Pattern`
/// argument.
fn pattern_source(pattern: i64) -> Result<Source, i64> {
    if let Some(source) = value::as_str(pattern) {
        return Ok(Source::Str(source.to_string()));
    }
    if let Some(state) = value::as_pattern(pattern) {
        return Ok(if is_bytes_pattern(state) {
            Source::Bytes(latin1_bytes(&state.source))
        } else {
            Source::Str(state.source.clone())
        });
    }
    if let Some(bytes) = value::as_bytes(pattern) {
        return Ok(Source::Bytes(bytes.to_vec()));
    }
    Err(type_error(&format!(
        "first argument must be str, bytes or Pattern, not {}",
        value::type_name(pattern)
    )))
}

/// Returns the subject of a match: a `str`, or the bytes of a
/// `bytes`/`bytearray` argument.
fn subject_argument(value: i64, position: &str) -> Result<Subject, i64> {
    if let Some(text) = value::as_str(value) {
        return Ok(Subject::Str(text.to_string()));
    }
    if let Some(bytes) = value::as_bytes(value) {
        return Ok(Subject::Bytes(bytes.to_vec()));
    }
    Err(type_error(&format!(
        "{position} must be str or bytes-like, not {}",
        value::type_name(value)
    )))
}

/// CPython's str/bytes domain check: mixing the two raises `TypeError`.
fn domain_error(source: &Source, subject: &Subject) -> Option<i64> {
    match (source, subject) {
        (Source::Str(_), Subject::Bytes(_)) => Some(type_error(
            "cannot use a string pattern on a bytes-like object",
        )),
        (Source::Bytes(_), Subject::Str(_)) => Some(type_error(
            "cannot use a bytes pattern on a string-like object",
        )),
        _ => None,
    }
}

/// Compiles `source` with the Python flag bits in `flags`.
///
/// `VERBOSE`/`X` enables verbose mode (whitespace ignored and `#` starts a
/// comment), `IGNORECASE`/`I` folds case, `MULTILINE`/`M` makes `^` and `$`
/// match at line breaks, and `DOTALL`/`S` lets `.` match a newline. `ASCII`,
/// `UNICODE`, `LOCALE`, `TEMPLATE` and `DEBUG` do not change how the patterns
/// this runtime supports are parsed, so they are accepted and ignored.
///
/// One documented divergence: the `regex` crate also drops whitespace inside a
/// character class in verbose mode, while CPython keeps it literal
/// (`[a b]` matches a space in Python only).
fn compile_source(source: &str, flags: i64) -> Result<Regex, i64> {
    // CPython accepts an unescaped `[` inside a character class (`[^[]` means
    // "not `[`"), while the Rust regex parser treats it as a nested class.
    // Escape that form before handing the pattern to the parser.
    let mut normalized = escape_class_brackets(source);
    if flags & VERBOSE != 0 {
        normalized = escape_verbose_class_chars(&normalized);
    }
    let build = |pattern: &str| {
        RegexBuilder::new(pattern)
            .case_insensitive(flags & IGNORECASE != 0)
            .multi_line(flags & MULTILINE != 0)
            .dot_matches_new_line(flags & DOTALL != 0)
            .ignore_whitespace(flags & VERBOSE != 0)
            .build()
    };
    match build(&normalized) {
        Ok(regex) => Ok(regex),
        Err(error) if error.to_string().contains("look-around") => {
            // Rust's engine has no look-around. Highlighters and a few
            // libraries use assertions only to narrow a match; dropping them
            // keeps the expression usable (matching becomes more permissive).
            build(&strip_lookarounds(&normalized)).map_err(regex_error)
        }
        Err(error) => Err(regex_error(error)),
    }
}

/// Renders a bytes pattern as the ASCII pattern text understood by the Rust
/// bytes parser: ASCII bytes stay literal, while bytes at or above `0x80`
/// become `\xNN` escapes. With Unicode mode disabled those escapes match the
/// raw byte, exactly like CPython's byte patterns.
fn bytes_pattern_text(source: &[u8]) -> String {
    let mut text = String::with_capacity(source.len());
    for &byte in source {
        if byte.is_ascii() {
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("\\x{byte:02X}"));
        }
    }
    text
}

/// Compiles bytes pattern text (see [`bytes_pattern_text`]) with the same
/// normalisation and flag handling as [`compile_source`], in byte mode.
fn compile_bytes_text(pattern: &str, flags: i64) -> Result<regex_bytes::Regex, i64> {
    let mut normalized = escape_class_brackets(pattern);
    if flags & VERBOSE != 0 {
        normalized = escape_verbose_class_chars(&normalized);
    }
    let build = |pattern: &str| {
        regex_bytes::RegexBuilder::new(pattern)
            .case_insensitive(flags & IGNORECASE != 0)
            .multi_line(flags & MULTILINE != 0)
            .dot_matches_new_line(flags & DOTALL != 0)
            .ignore_whitespace(flags & VERBOSE != 0)
            // CPython byte patterns are byte-oriented: `.` matches a single
            // byte and `\w`/`\d`/case folding stay ASCII.
            .unicode(false)
            .build()
    };
    match build(&normalized) {
        Ok(regex) => Ok(regex),
        Err(error) if error.to_string().contains("look-around") => {
            build(&strip_lookarounds(&normalized)).map_err(regex_error)
        }
        Err(error) => Err(regex_error(error)),
    }
}

/// Compiles a bytes pattern.
fn compile_bytes(source: &[u8], flags: i64) -> Result<regex_bytes::Regex, i64> {
    compile_bytes_text(&bytes_pattern_text(source), flags)
}

/// Removes `(?<=...)`, `(?<!...)`, `(?=...)` and `(?!...)` assertions.
/// The body may contain escaped parentheses; nested groups are skipped.
fn strip_lookarounds(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '(' && chars.get(index + 1) == Some(&'?') {
            let marker = chars.get(index + 2).copied();
            let assertion = matches!(marker, Some('=') | Some('!'))
                || (marker == Some('<') && matches!(chars.get(index + 3), Some('=') | Some('!')));
            if assertion {
                let mut depth = 1usize;
                let mut escaped = false;
                index += 3;
                while index < chars.len() && depth > 0 {
                    let ch = chars[index];
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                    }
                    index += 1;
                }
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Escapes an unescaped `[` inside a character class; CPython treats it as a
/// literal, while the Rust parser tries to open a nested class.
fn escape_class_brackets(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_class = false;
    let mut class_started = false;
    let mut after_caret = false;
    let mut escaped = false;
    for ch in source.chars() {
        if escaped {
            out.push(ch);
            if in_class {
                class_started = true;
            }
            escaped = false;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escaped = true;
            continue;
        }
        if in_class {
            if !class_started {
                if ch == '^' && !after_caret {
                    after_caret = true;
                    out.push(ch);
                    continue;
                }
                if ch == ']' {
                    out.push('\\');
                    out.push(ch);
                    class_started = true;
                    continue;
                }
                class_started = true;
            }
            if ch == ']' {
                in_class = false;
                out.push(ch);
            } else if ch == '[' {
                out.push('\\');
                out.push(ch);
            } else {
                out.push(ch);
            }
            continue;
        }
        if ch == '[' {
            in_class = true;
            class_started = false;
            after_caret = false;
        }
        out.push(ch);
    }
    out
}

/// Escapes `#` and whitespace inside character classes for Rust's verbose
/// mode, which otherwise drops them as comments/insignificant whitespace.
/// CPython keeps both literal inside `[...]`.
fn escape_verbose_class_chars(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_class = false;
    let mut escaped = false;
    for ch in source.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escaped = true;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            } else if ch == '#' || ch.is_whitespace() {
                out.push('\\');
            }
            out.push(ch);
            continue;
        }
        if ch == '[' {
            in_class = true;
        }
        out.push(ch);
    }
    out
}

/// Compiles `source` with an anchored wrapper. The wrapper is non-capturing,
/// so user group numbers are unchanged.
fn compile_anchored(source: &str, full: bool, flags: i64) -> Result<Regex, i64> {
    let wrapped = if full {
        format!(r"\A(?:{source})\z")
    } else {
        format!(r"\A(?:{source})")
    };
    compile_source(&wrapped, flags)
}

/// [`compile_anchored`] for a bytes pattern, with the same wrapper.
fn compile_anchored_bytes(
    source: &[u8],
    full: bool,
    flags: i64,
) -> Result<regex_bytes::Regex, i64> {
    let source = bytes_pattern_text(source);
    let wrapped = if full {
        format!(r"\A(?:{source})\z")
    } else {
        format!(r"\A(?:{source})")
    };
    compile_bytes_text(&wrapped, flags)
}

/// Converts a UTF-8 byte offset to a Python character offset.
fn char_offset(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

/// Converts a Python character offset to a UTF-8 byte offset.
fn byte_offset(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn make_match(pattern: &str, text: &str, captures: &regex::Captures<'_>) -> i64 {
    make_match_at(pattern, text, captures, 0)
}

fn make_match_at(pattern: &str, text: &str, captures: &regex::Captures<'_>, base: usize) -> i64 {
    let groups = captures
        .iter()
        .map(|group| {
            group.map(|m| {
                (
                    char_offset(text, m.start() + base),
                    char_offset(text, m.end() + base),
                )
            })
        })
        .collect();
    value::match_new(pattern.to_string(), text.to_string(), groups)
}

/// Builds a `Match` for a bytes subject. `text` is the full raw subject;
/// `base` is the byte offset of the searched slice inside it.
fn make_match_bytes(source: &[u8], text: &[u8], captures: &regex_bytes::Captures<'_>) -> i64 {
    make_match_bytes_at(source, text, captures, 0)
}

fn make_match_bytes_at(
    source: &[u8],
    text: &[u8],
    captures: &regex_bytes::Captures<'_>,
    base: usize,
) -> i64 {
    let encoded = latin1_string(text);
    let mut groups: Vec<Option<(usize, usize)>> = captures
        .iter()
        .map(|group| {
            group.map(|m| {
                (
                    byte_offset(&encoded, m.start() + base),
                    byte_offset(&encoded, m.end() + base),
                )
            })
        })
        .collect();
    // Marks the bytes domain; the positions above are encoded-text offsets so
    // generic string slicing stays on character boundaries.
    groups.push(BYTES_MATCH);
    value::match_new(latin1_string(source), encoded, groups)
}

/// Converts Python character offsets into a UTF-8 byte range, returning the
/// matched slice and its absolute byte base.
fn slice_bounds(text: &str, pos: usize, endpos: usize) -> Option<(&str, usize)> {
    let byte_at = |character: usize| -> Option<usize> {
        if character == 0 {
            return Some(0);
        }
        text.char_indices()
            .nth(character)
            .map(|(byte, _)| byte)
            .or_else(|| (character == text.chars().count()).then_some(text.len()))
    };
    let start = byte_at(pos)?;
    let end = byte_at(endpos)?;
    (start <= end).then(|| (&text[start..end], start))
}

/// [`slice_bounds`] for a bytes subject: `pos`/`endpos` are byte offsets.
fn slice_bounds_bytes(text: &[u8], pos: usize, endpos: usize) -> Option<(&[u8], usize)> {
    if pos > endpos || endpos > text.len() {
        return None;
    }
    Some((&text[pos..endpos], pos))
}

/// `re.search`/`re.match`/`re.fullmatch`.
fn match_mode(source: &Source, subject: &Subject, full: bool, anchored: bool, flags: i64) -> i64 {
    match (source, subject) {
        (Source::Str(source), Subject::Str(text)) => {
            let regex = if anchored {
                match compile_anchored(source, full, flags) {
                    Ok(regex) => regex,
                    Err(err) => return err,
                }
            } else {
                match compile_source(source, flags) {
                    Ok(regex) => regex,
                    Err(err) => return err,
                }
            };
            match regex.captures(text) {
                Some(captures) => make_match(source, text, &captures),
                None => NONE,
            }
        }
        (Source::Bytes(source), Subject::Bytes(text)) => {
            let regex = if anchored {
                match compile_anchored_bytes(source, full, flags) {
                    Ok(regex) => regex,
                    Err(err) => return err,
                }
            } else {
                match compile_bytes(source, flags) {
                    Ok(regex) => regex,
                    Err(err) => return err,
                }
            };
            match regex.captures(text) {
                Some(captures) => make_match_bytes(source, text, &captures),
                None => NONE,
            }
        }
        _ => domain_error(source, subject).unwrap_or(NONE),
    }
}

/// Builds a `bytes` object from a bytes-regex group, or `fallback` when the
/// group did not participate in the match.
fn bytes_match_value(
    captures: &regex_bytes::Captures<'_>,
    index: usize,
    fallback: Option<i64>,
) -> i64 {
    match captures.get(index) {
        Some(m) => value::bytes_new(m.as_bytes().to_vec()),
        None => fallback.unwrap_or_else(|| value::bytes_new(Vec::new())),
    }
}

/// `re.findall`: whole matches, the single group, or a tuple of groups when
/// the pattern has more than one group (CPython semantics).
fn findall(source: &Source, subject: &Subject, flags: i64) -> i64 {
    match (source, subject) {
        (Source::Str(source), Subject::Str(text)) => findall_str(source, text, flags),
        (Source::Bytes(source), Subject::Bytes(text)) => findall_bytes(source, text, flags),
        _ => domain_error(source, subject).unwrap_or(NONE),
    }
}

fn findall_str(source: &str, text: &str, flags: i64) -> i64 {
    let regex = match compile_source(source, flags) {
        Ok(regex) => regex,
        Err(err) => return err,
    };
    let groups = regex.captures_len().saturating_sub(1);
    let result = value::list_new(Vec::new());
    for captures in regex.captures_iter(text) {
        let item = if groups == 0 {
            captures
                .get(0)
                .map(|m| value::str_alloc(m.as_str().to_string()))
                .unwrap_or(NONE)
        } else if groups == 1 {
            captures
                .get(1)
                .map(|m| value::str_alloc(m.as_str().to_string()))
                .unwrap_or_else(|| value::str_alloc(String::new()))
        } else {
            let parts: Vec<i64> = (1..=groups)
                .map(|index| {
                    captures
                        .get(index)
                        .map(|m| value::str_alloc(m.as_str().to_string()))
                        .unwrap_or_else(|| value::str_alloc(String::new()))
                })
                .collect();
            value::tuple_new(parts)
        };
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

fn findall_bytes(source: &[u8], text: &[u8], flags: i64) -> i64 {
    let regex = match compile_bytes(source, flags) {
        Ok(regex) => regex,
        Err(err) => return err,
    };
    let groups = regex.captures_len().saturating_sub(1);
    let result = value::list_new(Vec::new());
    for captures in regex.captures_iter(text) {
        let item = if groups == 0 {
            bytes_match_value(&captures, 0, None)
        } else if groups == 1 {
            bytes_match_value(&captures, 1, None)
        } else {
            let parts: Vec<i64> = (1..=groups)
                .map(|index| bytes_match_value(&captures, index, None))
                .collect();
            value::tuple_new(parts)
        };
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

/// `re.finditer`: every non-overlapping match as a `Match` object in a list.
///
/// The list is materialised eagerly so the result behaves like an iterable
/// for `for` loops, `list()` and indexing; each match keeps the same group
/// layout as [`match_mode`].
fn finditer(source: &Source, subject: &Subject, flags: i64) -> i64 {
    match (source, subject) {
        (Source::Str(source), Subject::Str(text)) => {
            let regex = match compile_source(source, flags) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            let result = value::list_new(Vec::new());
            for captures in regex.captures_iter(text) {
                let item = make_match(source, text, &captures);
                if let Some(mut items) = value::list_items_mut(result) {
                    items.push(item);
                    crate::heap::write_barrier_edge(result, item);
                }
            }
            result
        }
        (Source::Bytes(source), Subject::Bytes(text)) => {
            let regex = match compile_bytes(source, flags) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            let result = value::list_new(Vec::new());
            for captures in regex.captures_iter(text) {
                let item = make_match_bytes(source, text, &captures);
                if let Some(mut items) = value::list_items_mut(result) {
                    items.push(item);
                    crate::heap::write_barrier_edge(result, item);
                }
            }
            result
        }
        _ => domain_error(source, subject).unwrap_or(NONE),
    }
}

/// Converts Python replacement syntax (`\1`, `\\`, `$`) to the `regex`
/// crate's replacement syntax (`$1`, `\`, `$$`).
fn replacement(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.peek().copied() {
                Some(next) if next.is_ascii_digit() => {
                    let mut number = String::new();
                    while let Some(digit) = chars.peek().copied() {
                        if !digit.is_ascii_digit() {
                            break;
                        }
                        number.push(digit);
                        chars.next();
                    }
                    out.push_str("${");
                    out.push_str(&number);
                    out.push('}');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                Some('g') => {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        chars.next();
                        let mut name = String::new();
                        for ch in chars.by_ref() {
                            if ch == '>' {
                                break;
                            }
                            name.push(ch);
                        }
                        out.push_str("${");
                        out.push_str(&name);
                        out.push('}');
                    } else {
                        out.push_str("\\g");
                    }
                }
                _ => out.push('\\'),
            },
            '$' => out.push_str("$$"),
            _ => out.push(ch),
        }
    }
    out
}

/// [`replacement`] for a bytes replacement template.
fn replacement_bytes(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        match text[index] {
            b'\\' => match text.get(index + 1).copied() {
                Some(next) if next.is_ascii_digit() => {
                    let start = index + 1;
                    let mut end = start;
                    while end < text.len() && text[end].is_ascii_digit() {
                        end += 1;
                    }
                    out.extend_from_slice(b"${");
                    out.extend_from_slice(&text[start..end]);
                    out.push(b'}');
                    index = end;
                }
                Some(b'\\') => {
                    out.push(b'\\');
                    index += 2;
                }
                Some(b'g') => match text.get(index + 2).copied() {
                    Some(b'<') => {
                        let start = index + 3;
                        let mut end = start;
                        while end < text.len() && text[end] != b'>' {
                            end += 1;
                        }
                        out.extend_from_slice(b"${");
                        out.extend_from_slice(&text[start..end]);
                        out.push(b'}');
                        index = if end < text.len() { end + 1 } else { end };
                    }
                    _ => {
                        out.extend_from_slice(b"\\g");
                        index += 2;
                    }
                },
                _ => {
                    out.push(b'\\');
                    index += 1;
                }
            },
            b'$' => {
                out.extend_from_slice(b"$$");
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// Reads the optional `flags` argument of `re.compile`/`re.finditer`.
///
/// CPython accepts an integer bit set (a `RegexFlag` member or the OR of
/// several); `None` fills an omitted optional parameter in the lowered call,
/// so it maps to the default flag set. The internal [`BYTES_PATTERN`] bit is
/// reserved and masked out of user-supplied flags.
fn flags_argument(raw: Option<i64>) -> Result<i64, i64> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    if raw == NONE {
        return Ok(0);
    }
    match value::as_integer(raw) {
        Some(flags) => Ok(flags & !BYTES_PATTERN),
        None => Err(type_error(&format!(
            "flags must be an integer, not {}",
            value::type_name(raw)
        ))),
    }
}

fn count_argument(raw: i64, name: &str) -> Result<usize, i64> {
    if raw == NONE {
        return Ok(0);
    }
    match value::as_integer(raw) {
        Some(n) if n >= 0 => Ok(n as usize),
        Some(_) => Ok(0),
        None => Err(type_error(&format!(
            "{name} must be an integer, not {}",
            value::type_name(raw)
        ))),
    }
}

/// Result of a substitution: the new text (same domain as the subject) and
/// the number of replacements performed.
enum Replaced {
    Str(String),
    Bytes(Vec<u8>),
}

/// `re.sub`/`re.subn` workhorse: expands a replacement template, or calls a
/// replacement function, for every match.
fn substitute(
    source: &Source,
    subject: &Subject,
    flags: i64,
    replacement_arg: i64,
    limit: usize,
) -> Result<(Replaced, usize), i64> {
    match (source, subject) {
        (Source::Str(source), Subject::Str(text)) => {
            let regex = compile_source(source, flags)?;
            if let Some(template) = value::as_str(replacement_arg) {
                let template = replacement(template);
                let (out, count) = replace_str_template(&regex, text, &template, limit);
                return Ok((Replaced::Str(out), count));
            }
            if value::as_function(replacement_arg).is_none() {
                return Err(type_error(&format!(
                    "replacement must be str or callable, not {}",
                    value::type_name(replacement_arg)
                )));
            }
            let (out, count) = replace_str_callable(&regex, text, source, replacement_arg, limit)?;
            Ok((Replaced::Str(out), count))
        }
        (Source::Bytes(source), Subject::Bytes(text)) => {
            let regex = compile_bytes(source, flags)?;
            if let Some(template) = value::as_bytes(replacement_arg) {
                let template = replacement_bytes(template);
                let (out, count) = replace_bytes_template(&regex, text, &template, limit);
                return Ok((Replaced::Bytes(out), count));
            }
            if value::as_function(replacement_arg).is_none() {
                return Err(type_error(&format!(
                    "replacement must be bytes or callable, not {}",
                    value::type_name(replacement_arg)
                )));
            }
            let (out, count) =
                replace_bytes_callable(&regex, text, source, replacement_arg, limit)?;
            Ok((Replaced::Bytes(out), count))
        }
        _ => Err(domain_error(source, subject).unwrap_or(NONE)),
    }
}

fn replace_str_template(
    regex: &Regex,
    text: &str,
    template: &str,
    limit: usize,
) -> (String, usize) {
    let mut out = String::new();
    let mut last = 0usize;
    let mut count = 0usize;
    for captures in regex.captures_iter(text) {
        if limit != 0 && count >= limit {
            break;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        captures.expand(template, &mut out);
        last = whole.end();
        count += 1;
        if whole.start() == whole.end() && last < text.len() {
            // Zero-width match: advance one character like CPython's engine.
            let ch = text[last..].chars().next().unwrap_or('\0');
            out.push(ch);
            last += ch.len_utf8();
        }
    }
    out.push_str(&text[last..]);
    (out, count)
}

fn replace_str_callable(
    regex: &Regex,
    text: &str,
    pattern: &str,
    function: i64,
    limit: usize,
) -> Result<(String, usize), i64> {
    // Callable replacement (`re.sub(pattern, fn, text)`): the callback gets
    // one Match object per non-overlapping match and its result is converted
    // with `str()`.
    let mut out = String::new();
    let mut last = 0usize;
    let mut count = 0usize;
    for captures in regex.captures_iter(text) {
        if limit != 0 && count >= limit {
            break;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        let match_object = make_match(pattern, text, &captures);
        let replacement_value = crate::core::builtins::call(function, 1, [match_object].as_ptr());
        if exc::pending() {
            return Err(NONE);
        }
        out.push_str(&value::to_string(replacement_value));
        last = whole.end();
        count += 1;
        if whole.start() == whole.end() && last < text.len() {
            let ch = text[last..].chars().next().unwrap_or('\0');
            out.push(ch);
            last += ch.len_utf8();
        }
    }
    out.push_str(&text[last..]);
    Ok((out, count))
}

fn replace_bytes_template(
    regex: &regex_bytes::Regex,
    text: &[u8],
    template: &[u8],
    limit: usize,
) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut last = 0usize;
    let mut count = 0usize;
    for captures in regex.captures_iter(text) {
        if limit != 0 && count >= limit {
            break;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.extend_from_slice(&text[last..whole.start()]);
        captures.expand(template, &mut out);
        last = whole.end();
        count += 1;
        if whole.start() == whole.end() && last < text.len() {
            // Zero-width match: advance one byte like CPython's engine.
            out.push(text[last]);
            last += 1;
        }
    }
    out.extend_from_slice(&text[last..]);
    (out, count)
}

fn replace_bytes_callable(
    regex: &regex_bytes::Regex,
    text: &[u8],
    pattern: &[u8],
    function: i64,
    limit: usize,
) -> Result<(Vec<u8>, usize), i64> {
    // The callback receives a Match over the bytes subject and must return a
    // bytes-like value; a `str` result is a TypeError like CPython.
    let mut out = Vec::new();
    let mut last = 0usize;
    let mut count = 0usize;
    for captures in regex.captures_iter(text) {
        if limit != 0 && count >= limit {
            break;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.extend_from_slice(&text[last..whole.start()]);
        let match_object = make_match_bytes(pattern, text, &captures);
        let replacement_value = crate::core::builtins::call(function, 1, [match_object].as_ptr());
        if exc::pending() {
            return Err(NONE);
        }
        let Some(replacement) = value::as_bytes(replacement_value) else {
            return Err(type_error(&format!(
                "expected bytes instance, {} found",
                value::type_name(replacement_value)
            )));
        };
        out.extend_from_slice(replacement);
        last = whole.end();
        count += 1;
        if whole.start() == whole.end() && last < text.len() {
            out.push(text[last]);
            last += 1;
        }
    }
    out.extend_from_slice(&text[last..]);
    Ok((out, count))
}

/// Validates the shared `sub`/`subn` arguments (replacement, subject,
/// optional count) and performs the substitution.
fn substitute_arguments(
    source: &Source,
    flags: i64,
    args: &[i64],
    name: &str,
) -> Result<(Replaced, usize), i64> {
    if args.len() < 2 || args.len() > 3 {
        return Err(type_error(&format!(
            "{name}() takes 2 or 3 arguments ({} given)",
            args.len()
        )));
    }
    let subject = subject_argument(args[1], "string")?;
    let limit = if args.len() == 3 {
        count_argument(args[2], "count")?
    } else {
        0
    };
    if let Some(err) = domain_error(source, &subject) {
        return Err(err);
    }
    substitute(source, &subject, flags, args[0], limit)
}

fn sub(source: &Source, flags: i64, args: &[i64]) -> i64 {
    match substitute_arguments(source, flags, args, "sub") {
        Ok((Replaced::Str(text), _)) => value::str_alloc(text),
        Ok((Replaced::Bytes(bytes), _)) => value::bytes_new(bytes),
        Err(err) => err,
    }
}

fn subn(source: &Source, flags: i64, args: &[i64]) -> i64 {
    match substitute_arguments(source, flags, args, "subn") {
        Ok((Replaced::Str(text), count)) => value::tuple_new(vec![
            value::str_alloc(text),
            crate::ops::int_result(count as i64),
        ]),
        Ok((Replaced::Bytes(bytes), count)) => value::tuple_new(vec![
            value::bytes_new(bytes),
            crate::ops::int_result(count as i64),
        ]),
        Err(err) => err,
    }
}

fn split(source: &Source, flags: i64, args: &[i64]) -> i64 {
    let subject = match subject_argument(args[0], "string") {
        Ok(subject) => subject,
        Err(err) => return err,
    };
    let maxsplit = if args.len() == 2 {
        match count_argument(args[1], "maxsplit") {
            Ok(limit) => limit,
            Err(err) => return err,
        }
    } else {
        0
    };
    if let Some(err) = domain_error(source, &subject) {
        return err;
    }
    match (source, &subject) {
        (Source::Str(source), Subject::Str(text)) => {
            let regex = match compile_source(source, flags) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            let result = value::list_new(Vec::new());
            if maxsplit == 0 {
                for part in regex.split(text) {
                    push_part(result, part);
                }
            } else {
                for part in regex.splitn(text, maxsplit.saturating_add(1)) {
                    push_part(result, part);
                }
            }
            result
        }
        (Source::Bytes(source), Subject::Bytes(text)) => {
            let regex = match compile_bytes(source, flags) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            let result = value::list_new(Vec::new());
            if maxsplit == 0 {
                for part in regex.split(text) {
                    push_part_bytes(result, part);
                }
            } else {
                for part in regex.splitn(text, maxsplit.saturating_add(1)) {
                    push_part_bytes(result, part);
                }
            }
            result
        }
        _ => domain_error(source, &subject).unwrap_or(NONE),
    }
}

/// Appends one split piece to the result list, publishing the GC edge.
fn push_part(result: i64, part: &str) {
    let item = value::str_alloc(part.to_string());
    if let Some(mut items) = value::list_items_mut(result) {
        items.push(item);
        crate::heap::write_barrier_edge(result, item);
    }
}

/// [`push_part`] for a bytes split.
fn push_part_bytes(result: i64, part: &[u8]) {
    let item = value::bytes_new(part.to_vec());
    if let Some(mut items) = value::list_items_mut(result) {
        items.push(item);
        crate::heap::write_barrier_edge(result, item);
    }
}

/// Module-level functions.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "compile" => {
            // Already-compiled patterns are returned unchanged; their own
            // flags travel with them.
            if value::as_pattern(args[0]).is_some() {
                return args[0];
            }
            let flags = match flags_argument(args.get(1).copied()) {
                Ok(flags) => flags,
                Err(err) => return err,
            };
            let source = match pattern_source(args[0]) {
                Ok(source) => source,
                Err(err) => return err,
            };
            match source {
                Source::Str(source) => {
                    if let Err(err) = compile_source(&source, flags) {
                        return err;
                    }
                    value::pattern_new(source, flags)
                }
                Source::Bytes(source) => {
                    if let Err(err) = compile_bytes(&source, flags) {
                        return err;
                    }
                    value::pattern_new(latin1_string(&source), flags | BYTES_PATTERN)
                }
            }
        }
        "escape" => match value::as_str(args[0]) {
            Some(text) => value::str_alloc(regex::escape(text)),
            None => type_error(&format!(
                "escape() argument must be str, not {}",
                value::type_name(args[0])
            )),
        },
        _ => {
            let source = match pattern_source(args[0]) {
                Ok(source) => source,
                Err(err) => return err,
            };
            match function {
                "search" | "match" | "fullmatch" => {
                    let subject = match subject_argument(args[1], "string") {
                        Ok(subject) => subject,
                        Err(err) => return err,
                    };
                    // `re.search(pattern, string, flags=0)`: CPython's
                    // trailing positional flags argument (`pandas` passes
                    // `re.I` this way).
                    let flags = match flags_argument(args.get(2).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    if let Some(err) = domain_error(&source, &subject) {
                        return err;
                    }
                    match_mode(
                        &source,
                        &subject,
                        function == "fullmatch",
                        function != "search",
                        flags,
                    )
                }
                "findall" => {
                    let subject = match subject_argument(args[1], "string") {
                        Ok(subject) => subject,
                        Err(err) => return err,
                    };
                    let flags = match flags_argument(args.get(2).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    findall(&source, &subject, flags)
                }
                "finditer" => {
                    let subject = match subject_argument(args[1], "string") {
                        Ok(subject) => subject,
                        Err(err) => return err,
                    };
                    let flags = match flags_argument(args.get(2).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    finditer(&source, &subject, flags)
                }
                // `sub`/`subn`: the arguments after the pattern are
                // replacement, string, count and (positionally) flags.
                "sub" | "subn" => {
                    let flags = match flags_argument(args.get(4).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    let rest = args.get(1..args.len().min(4)).unwrap_or(&[]);
                    if function == "sub" {
                        sub(&source, flags, rest)
                    } else {
                        subn(&source, flags, rest)
                    }
                }
                // `split`: pattern, string, maxsplit and (positionally) flags.
                "split" => {
                    let flags = match flags_argument(args.get(3).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    let rest = args.get(1..args.len().min(3)).unwrap_or(&[]);
                    split(&source, flags, rest)
                }
                _ => exc::raise_with_message(
                    ATTRIBUTE_ERROR,
                    &format!("module 're' has no attribute '{function}'"),
                ),
            }
        }
    }
}

/// Methods of a compiled `Pattern`.
pub(crate) fn pattern_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_pattern(obj) else {
        return attribute_error("Pattern", name);
    };
    let source = if is_bytes_pattern(state) {
        Source::Bytes(latin1_bytes(&state.source))
    } else {
        Source::Str(state.source.clone())
    };
    let flags = state.flags & !BYTES_PATTERN;
    match name {
        "search" | "match" | "fullmatch" => {
            if args.is_empty() || args.len() > 3 {
                return type_error(&format!(
                    "Pattern.{name}() takes 1 to 3 arguments ({} given)",
                    args.len()
                ));
            }
            let subject = match subject_argument(args[0], "string") {
                Ok(subject) => subject,
                Err(err) => return err,
            };
            if let Some(err) = domain_error(&source, &subject) {
                return err;
            }
            let length = match &subject {
                Subject::Str(text) => text.chars().count(),
                Subject::Bytes(text) => text.len(),
            };
            let pos = match args.get(1).copied() {
                None | Some(NONE) => 0,
                Some(value) => match value::as_integer(value) {
                    Some(position) if position >= 0 => position as usize,
                    Some(_) => {
                        return exc::raise_with_message(INDEX_ERROR, "position out of range")
                    }
                    None => return type_error("position must be an integer"),
                },
            };
            let endpos = match args.get(2).copied() {
                None | Some(NONE) => length,
                Some(value) => match value::as_integer(value) {
                    Some(position) if position >= 0 => position as usize,
                    Some(_) => {
                        return exc::raise_with_message(INDEX_ERROR, "position out of range")
                    }
                    None => return type_error("endpos must be an integer"),
                },
            };
            match (&source, &subject) {
                (Source::Str(source), Subject::Str(text)) => {
                    let Some((slice, base)) = slice_bounds(text, pos, endpos) else {
                        return exc::raise_with_message(INDEX_ERROR, "position out of range");
                    };
                    let regex = if name == "search" {
                        compile_source(source, flags)
                    } else {
                        compile_anchored(source, name == "fullmatch", flags)
                    };
                    let Ok(regex) = regex else {
                        return exc::raise_with_message(VALUE_ERROR, "invalid regular expression");
                    };
                    match regex.captures(slice) {
                        Some(captures) => make_match_at(source, text, &captures, base),
                        None => NONE,
                    }
                }
                (Source::Bytes(source), Subject::Bytes(text)) => {
                    let Some((slice, base)) = slice_bounds_bytes(text, pos, endpos) else {
                        return exc::raise_with_message(INDEX_ERROR, "position out of range");
                    };
                    let regex = if name == "search" {
                        compile_bytes(source, flags)
                    } else {
                        compile_anchored_bytes(source, name == "fullmatch", flags)
                    };
                    let Ok(regex) = regex else {
                        return exc::raise_with_message(VALUE_ERROR, "invalid regular expression");
                    };
                    match regex.captures(slice) {
                        Some(captures) => make_match_bytes_at(source, text, &captures, base),
                        None => NONE,
                    }
                }
                _ => domain_error(&source, &subject).unwrap_or(NONE),
            }
        }
        "findall" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let subject = match subject_argument(args[0], "string") {
                Ok(subject) => subject,
                Err(err) => return err,
            };
            findall(&source, &subject, flags)
        }
        "finditer" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let subject = match subject_argument(args[0], "string") {
                Ok(subject) => subject,
                Err(err) => return err,
            };
            finditer(&source, &subject, flags)
        }
        "sub" => {
            if args.len() < 2 || args.len() > 3 {
                return type_error(&format!(
                    "Pattern.sub() takes 2 to 3 arguments ({} given)",
                    args.len()
                ));
            }
            sub(&source, flags, args)
        }
        "split" => {
            if args.is_empty() || args.len() > 2 {
                return type_error(&format!(
                    "Pattern.split() takes 1 to 2 arguments ({} given)",
                    args.len()
                ));
            }
            split(&source, flags, args)
        }
        _ => attribute_error("Pattern", name),
    }
}

fn pattern_arity(method: &str, argc: usize, expected: usize) -> Option<i64> {
    (argc != expected).then(|| {
        type_error(&format!(
            "Pattern.{method}() takes exactly {expected} argument(s) ({argc} given)"
        ))
    })
}

/// Extracts one group, returning `str` or `bytes` according to the match
/// domain.
fn match_group_value(state: &MatchState, start: usize, end: usize, bytes: bool) -> i64 {
    if bytes {
        let start = start.min(state.text.len());
        let end = end.min(state.text.len()).max(start);
        value::bytes_new(latin1_bytes(&state.text[start..end]))
    } else {
        value::str_alloc(
            state.text[byte_offset(&state.text, start)..byte_offset(&state.text, end)].to_string(),
        )
    }
}

/// Converts a stored group position to the Python-visible offset. Bytes
/// matches store encoded-text offsets, so the byte offset is the number of
/// encoded characters before it.
fn match_offset(state: &MatchState, offset: usize, bytes: bool) -> usize {
    if bytes {
        char_offset(&state.text, offset)
    } else {
        offset
    }
}

/// Methods of a `Match` (`group`, `start`, `end`, `span`).
pub(crate) fn match_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_match(obj) else {
        return attribute_error("Match", name);
    };
    if name == "groupdict" {
        if !args.is_empty() {
            return type_error(&format!(
                "Match.groupdict() takes no arguments ({} given)",
                args.len()
            ));
        }
        return value::dict_new(Vec::new());
    }
    let bytes = is_bytes_match(state);
    if name == "groups" {
        if args.len() > 1 {
            return type_error(&format!(
                "Match.groups() takes at most 1 argument ({} given)",
                args.len()
            ));
        }
        let default = args.first().copied().unwrap_or(NONE);
        let groups = state
            .groups
            .iter()
            .take(match_group_count(state))
            .skip(1)
            .map(|position| match position {
                Some((start, end)) => match_group_value(state, *start, *end, bytes),
                None => default,
            })
            .collect();
        return value::tuple_new(groups);
    }
    if args.len() > 1 {
        return type_error(&format!(
            "Match.{name}() takes at most 1 argument ({} given)",
            args.len()
        ));
    }
    let group = match args.first().copied() {
        None | Some(NONE) => 0,
        Some(raw) => match value::as_integer(raw) {
            Some(group) if group >= 0 => group as usize,
            Some(_) => {
                return exc::raise_with_message(INDEX_ERROR, "no such group");
            }
            None => {
                return type_error(&format!(
                    "group index must be an integer, not {}",
                    value::type_name(raw)
                ))
            }
        },
    };
    if group >= match_group_count(state) {
        return exc::raise_with_message(INDEX_ERROR, "no such group");
    }
    let position = state.groups[group];
    match name {
        "group" => match position {
            Some((start, end)) => match_group_value(state, start, end, bytes),
            None => NONE,
        },
        "start" => match position {
            Some((start, _)) => crate::ops::int_result(match_offset(state, start, bytes) as i64),
            None => crate::ops::int_result(-1),
        },
        "end" => match position {
            Some((_, end)) => crate::ops::int_result(match_offset(state, end, bytes) as i64),
            None => crate::ops::int_result(-1),
        },
        "span" => match position {
            Some((start, end)) => value::tuple_new(vec![
                crate::ops::int_result(match_offset(state, start, bytes) as i64),
                crate::ops::int_result(match_offset(state, end, bytes) as i64),
            ]),
            None => value::tuple_new(vec![crate::ops::int_result(-1), crate::ops::int_result(-1)]),
        },
        _ => attribute_error("Match", name),
    }
}

/// `pattern` and `flags` data attributes of a compiled pattern.
pub(crate) fn pattern_attr(obj: i64, attr: &str) -> Option<i64> {
    let state = value::as_pattern(obj)?;
    match attr {
        "pattern" => Some(if is_bytes_pattern(state) {
            value::bytes_new(latin1_bytes(&state.source))
        } else {
            value::str_alloc(state.source.clone())
        }),
        "flags" => Some(crate::ops::int_result(state.flags & !BYTES_PATTERN)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> i64 {
        value::str_alloc(raw.to_string())
    }

    #[test]
    fn pattern_and_match_classes_describe_compiled_values() {
        exc::init();
        let compiled = call("compile", &[text("a+")]);
        assert!(!exc::pending());
        assert!(value::as_pattern(compiled).is_some());

        // `re.Pattern`/`re.Match` are the classes `type()` reports, so
        // identity and `isinstance` agree.
        let pattern_class = constant("Pattern");
        let match_class = constant("Match");
        assert_eq!(value::class_display_name(pattern_class), Some("Pattern"));
        assert_eq!(value::class_display_name(match_class), Some("Match"));
        assert_eq!(value::type_class(compiled), pattern_class);
        assert!(value::truthy(crate::builtins::isinstance(
            compiled,
            pattern_class
        )));
        assert!(!value::truthy(crate::builtins::isinstance(
            compiled,
            match_class
        )));

        let found = call("search", &[compiled, text("caa")]);
        assert!(!exc::pending());
        assert!(value::as_match(found).is_some());
        assert_eq!(value::type_class(found), match_class);
        assert!(value::truthy(crate::builtins::isinstance(
            found,
            match_class
        )));
        assert!(!value::truthy(crate::builtins::isinstance(
            found,
            pattern_class
        )));

        // Both classes are abstract: calling them reports TypeError.
        crate::builtins::call(pattern_class, 1, [text("a")].as_ptr());
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();
    }

    fn bytes_value(raw: &[u8]) -> i64 {
        value::bytes_new(raw.to_vec())
    }

    fn bytearray_value(raw: &[u8]) -> i64 {
        value::bytearray_new(raw.to_vec())
    }

    fn int(raw: i64) -> i64 {
        value::encode_int(raw).expect("int result")
    }

    fn as_bytes(actual: i64) -> Vec<u8> {
        value::as_bytes(actual)
            .unwrap_or_else(|| panic!("expected bytes, got {}", value::type_name(actual)))
            .to_vec()
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn str_patterns_keep_str_semantics() {
        exc::init();

        let found = call("search", &[text("a+"), text("caa")]);
        assert!(!exc::pending());
        assert_eq!(value::as_str(match_method(found, "group", &[])), Some("aa"));
        let span = value::as_tuple(match_method(found, "span", &[])).unwrap();
        assert_eq!(
            span.iter()
                .map(|&v| value::as_integer(v).unwrap())
                .collect::<Vec<_>>(),
            vec![1, 3]
        );

        let all = call("findall", &[text("(a)(b)"), text("abab")]);
        let items = value::as_list(all).unwrap();
        assert_eq!(items.len(), 2);
        let pair = value::as_tuple(items[0]).unwrap();
        assert_eq!(value::as_str(pair[0]), Some("a"));
        assert_eq!(value::as_str(pair[1]), Some("b"));

        let pieces = call("split", &[text("\r\n"), text("a\r\nb\r\n")]);
        let pieces: Vec<_> = value::as_list(pieces)
            .unwrap()
            .iter()
            .map(|&v| value::as_str(v).unwrap().to_string())
            .collect();
        assert_eq!(
            pieces,
            vec!["a".to_string(), "b".to_string(), String::new()]
        );

        let replaced = call("sub", &[text("b+"), text("-"), text("abbbc")]);
        assert_eq!(value::as_str(replaced), Some("a-c"));
    }

    #[test]
    fn compile_accepts_bytes_and_bytearray_patterns() {
        exc::init();
        for pattern in [bytes_value(b"a+"), bytearray_value(b"a+")] {
            let compiled = call("compile", &[pattern]);
            assert!(!exc::pending());
            let state = value::as_pattern(compiled).expect("Pattern");
            assert!(is_bytes_pattern(state));
            let attr = pattern_attr(compiled, "pattern").expect("pattern attribute");
            assert_eq!(as_bytes(attr), b"a+");
            let flags = pattern_attr(compiled, "flags").expect("flags attribute");
            assert_eq!(value::as_integer(flags), Some(0));
            // The internal marker never leaks into the public attribute.
            assert!(!value::repr(compiled).is_empty());
        }
        // Compiled patterns compare by identity, like CPython.
        let first = call("compile", &[bytes_value(b"a+")]);
        let second = call("compile", &[bytes_value(b"a+")]);
        assert!(crate::core::container::equals(first, first));
        assert!(!crate::core::container::equals(first, second));

        // User flags survive; the internal bytes marker is masked out.
        let ascii_pattern = call("compile", &[bytes_value(b"a+"), int(ASCII)]);
        let flags = pattern_attr(ascii_pattern, "flags").expect("flags attribute");
        assert_eq!(value::as_integer(flags), Some(ASCII));
        assert!(is_bytes_pattern(value::as_pattern(ascii_pattern).unwrap()));
        exc::init();
    }

    #[test]
    fn bytes_search_match_and_fullmatch() {
        exc::init();
        let found = call("search", &[bytes_value(b"a(b+)"), bytes_value(b"xabbby")]);
        assert!(!exc::pending());
        assert!(value::as_match(found).is_some());
        assert_eq!(as_bytes(match_method(found, "group", &[])), b"abbb");
        assert_eq!(as_bytes(match_method(found, "group", &[int(1)])), b"bbb");
        let span = value::as_tuple(match_method(found, "span", &[])).unwrap();
        assert_eq!(
            span.iter()
                .map(|&v| value::as_integer(v).unwrap())
                .collect::<Vec<_>>(),
            vec![1, 5]
        );

        let anchored = call("match", &[bytes_value(b"a+"), bytes_value(b"aaab")]);
        assert_eq!(as_bytes(match_method(anchored, "group", &[])), b"aaa");
        assert_eq!(
            call("match", &[bytes_value(b"a+"), bytes_value(b"baaa")]),
            NONE
        );

        let full = call("fullmatch", &[bytes_value(b"a+"), bytes_value(b"aaa")]);
        assert!(value::as_match(full).is_some());
        assert_eq!(
            call("fullmatch", &[bytes_value(b"a+"), bytes_value(b"aaab")]),
            NONE
        );

        // Case folding stays ASCII for byte patterns.
        let folded = call(
            "search",
            &[bytes_value(b"abc"), bytes_value(b"xABCy"), int(IGNORECASE)],
        );
        assert_eq!(as_bytes(match_method(folded, "group", &[])), b"ABC");
        exc::init();
    }

    #[test]
    fn bytes_finditer_and_groups() {
        exc::init();
        let matches = call("finditer", &[bytes_value(b"a."), bytes_value(b"abac")]);
        assert!(!exc::pending());
        let items = value::as_list(matches).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(as_bytes(match_method(items[0], "group", &[])), b"ab");
        assert_eq!(as_bytes(match_method(items[1], "group", &[])), b"ac");
        let first_span = value::as_tuple(match_method(items[0], "span", &[])).unwrap();
        assert_eq!(value::as_integer(first_span[0]), Some(0));
        assert_eq!(value::as_integer(first_span[1]), Some(2));

        // An optional group that did not participate is None in groups(),
        // while findall substitutes the empty bytes object.
        let optional = call("fullmatch", &[bytes_value(b"(a)(b)?"), bytes_value(b"a")]);
        let groups = value::as_tuple(match_method(optional, "groups", &[])).unwrap();
        assert_eq!(as_bytes(groups[0]), b"a");
        assert_eq!(groups[1], NONE);

        let found = call("findall", &[bytes_value(b"(a)(b)?"), bytes_value(b"a")]);
        let items = value::as_list(found).unwrap();
        let pair = value::as_tuple(items[0]).unwrap();
        assert_eq!(as_bytes(pair[0]), b"a");
        assert_eq!(as_bytes(pair[1]), b"");
        exc::init();
    }

    #[test]
    fn bytes_findall_shapes() {
        exc::init();
        let whole = call("findall", &[bytes_value(b"a."), bytes_value(b"abac")]);
        let whole: Vec<Vec<u8>> = value::as_list(whole)
            .unwrap()
            .iter()
            .map(|&v| as_bytes(v))
            .collect();
        assert_eq!(whole, vec![b"ab".to_vec(), b"ac".to_vec()]);

        let single = call("findall", &[bytes_value(b"a(b)"), bytes_value(b"abx")]);
        let single = value::as_list(single).unwrap();
        assert_eq!(as_bytes(single[0]), b"b");
        exc::init();
    }

    #[test]
    fn bytes_split_on_crlf() {
        exc::init();
        let pieces = call("split", &[bytes_value(b"\r\n"), bytes_value(b"a\r\nb\r\n")]);
        let pieces: Vec<Vec<u8>> = value::as_list(pieces)
            .unwrap()
            .iter()
            .map(|&v| as_bytes(v))
            .collect();
        assert_eq!(pieces, vec![b"a".to_vec(), b"b".to_vec(), Vec::new()]);

        let limited = call(
            "split",
            &[bytes_value(b"\r\n"), bytes_value(b"a\r\nb\r\nc"), int(1)],
        );
        let limited: Vec<Vec<u8>> = value::as_list(limited)
            .unwrap()
            .iter()
            .map(|&v| as_bytes(v))
            .collect();
        assert_eq!(limited, vec![b"a".to_vec(), b"b\r\nc".to_vec()]);

        let compiled = call("compile", &[bytes_value(b"\r\n")]);
        let pieces = pattern_method(compiled, "split", &[bytes_value(b"a\r\nb")]);
        let pieces: Vec<Vec<u8>> = value::as_list(pieces)
            .unwrap()
            .iter()
            .map(|&v| as_bytes(v))
            .collect();
        assert_eq!(pieces, vec![b"a".to_vec(), b"b".to_vec()]);
        exc::init();
    }

    #[test]
    fn bytes_sub_and_subn() {
        exc::init();
        let replaced = call(
            "sub",
            &[bytes_value(b"b+"), bytes_value(b"-"), bytes_value(b"abbbc")],
        );
        assert_eq!(as_bytes(replaced), b"a-c");

        let backrefs = call(
            "sub",
            &[
                bytes_value(b"(a)(b)"),
                bytes_value(b"\\2\\1"),
                bytes_value(b"ab"),
            ],
        );
        assert_eq!(as_bytes(backrefs), b"ba");

        let counted = call(
            "sub",
            &[
                bytes_value(b"a"),
                bytes_value(b"x"),
                bytes_value(b"aaa"),
                int(2),
            ],
        );
        assert_eq!(as_bytes(counted), b"xxa");

        let (result, count) = {
            let pair = call(
                "subn",
                &[bytes_value(b"a"), bytes_value(b"x"), bytes_value(b"aaa")],
            );
            let pair = value::as_tuple(pair).expect("subn tuple");
            (as_bytes(pair[0]), value::as_integer(pair[1]).unwrap())
        };
        assert_eq!(result, b"xxx");
        assert_eq!(count, 3);

        // bytearray replacement templates and subjects work like bytes.
        let replaced = call(
            "sub",
            &[
                bytearray_value(b"b+"),
                bytearray_value(b"-"),
                bytearray_value(b"abbbc"),
            ],
        );
        assert_eq!(as_bytes(replaced), b"a-c");
        exc::init();
    }

    #[test]
    fn mixed_domains_raise_type_error() {
        exc::init();
        call("search", &[text("a"), bytes_value(b"a")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(
            message(),
            "cannot use a string pattern on a bytes-like object"
        );
        exc::init();

        call("search", &[bytes_value(b"a"), text("a")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(
            message(),
            "cannot use a bytes pattern on a string-like object"
        );
        exc::init();

        let compiled = call("compile", &[bytes_value(b"a")]);
        pattern_method(compiled, "match", &[text("a")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        call("compile", &[int(3)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        call("sub", &[bytes_value(b"a"), text("x"), bytes_value(b"a")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(message(), "replacement must be bytes or callable, not str");
        exc::init();
    }

    #[test]
    fn bytearray_subjects_stay_in_the_bytes_domain() {
        exc::init();
        let found = call("search", &[bytearray_value(b"a+"), bytearray_value(b"caa")]);
        assert!(!exc::pending());
        assert_eq!(as_bytes(match_method(found, "group", &[])), b"aa");
        let pieces = call("split", &[bytes_value(b","), bytearray_value(b"a,b")]);
        let pieces: Vec<Vec<u8>> = value::as_list(pieces)
            .unwrap()
            .iter()
            .map(|&v| as_bytes(v))
            .collect();
        assert_eq!(pieces, vec![b"a".to_vec(), b"b".to_vec()]);
        exc::init();
    }

    #[test]
    fn bytes_patterns_handle_non_ascii_bytes() {
        exc::init();
        let compiled = call("compile", &[bytes_value(b"(\xff)")]);
        let found = call("search", &[compiled, bytes_value(b"x\xffy")]);
        assert!(!exc::pending());
        assert_eq!(as_bytes(match_method(found, "group", &[int(1)])), b"\xff");
        let span = value::as_tuple(match_method(found, "span", &[int(1)])).unwrap();
        assert_eq!(value::as_integer(span[0]), Some(1));
        assert_eq!(value::as_integer(span[1]), Some(2));

        // `repr` must not split an encoded character or panic.
        assert!(value::repr(found).contains("re.Match"));
        assert!(value::repr(compiled).contains("re.compile"));

        // Invalid bytes patterns raise ValueError, never panic.
        call("compile", &[bytes_value(b"(")]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        exc::init();
    }
}
