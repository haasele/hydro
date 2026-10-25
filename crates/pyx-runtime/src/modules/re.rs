//! `re`: thin wrappers over the `regex` crate with Python-like semantics.
//!
//! Supported: `search`, `match`, `fullmatch`, `findall`, `finditer`, `sub`,
//! `split`, `escape` and `compile` (patterns and matches are native objects
//! with the corresponding methods). `compile` accepts the `re.RegexFlag` bits
//! it can honour (`IGNORECASE`, `MULTILINE`, `DOTALL`, `VERBOSE` and their
//! short aliases); the compiled pattern keeps them for every method call.
//! Patterns and match objects are immutable and hold no PyX values. Invalid
//! patterns raise `ValueError`; non-string inputs raise `TypeError`.

use regex::{Regex, RegexBuilder};

use crate::exc::{self, ATTRIBUTE_ERROR, INDEX_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// State of an `re.compile()` result.
pub(crate) struct PatternState {
    pub(crate) source: String,
    /// The `re.RegexFlag` bits the pattern was compiled with, so every
    /// `Pattern` method recompiles with the same options.
    pub(crate) flags: i64,
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
pub(crate) struct MatchState {
    pub(crate) pattern: String,
    pub(crate) text: String,
    pub(crate) groups: Vec<Option<(usize, usize)>>,
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

/// Returns the pattern source from a `str` or compiled `Pattern` argument.
fn pattern_source(pattern: i64) -> Result<String, i64> {
    if let Some(source) = value::as_str(pattern) {
        return Ok(source.to_string());
    }
    if let Some(state) = value::as_pattern(pattern) {
        return Ok(state.source.clone());
    }
    Err(type_error(&format!(
        "first argument must be str or Pattern, not {}",
        value::type_name(pattern)
    )))
}

fn text_argument(value: i64, position: &str) -> Result<String, i64> {
    match value::as_str(value) {
        Some(text) => Ok(text.to_string()),
        None => Err(type_error(&format!(
            "{position} must be str, not {}",
            value::type_name(value)
        ))),
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

/// `re.search`/`re.match`/`re.fullmatch`.
fn match_mode(source: &str, text: &str, full: bool, anchored: bool, flags: i64) -> i64 {
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

/// `re.findall`: whole matches, the single group, or a tuple of groups when
/// the pattern has more than one group (CPython semantics).
fn findall(source: &str, text: &str, flags: i64) -> i64 {
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

/// `re.finditer`: every non-overlapping match as a `Match` object in a list.
///
/// The list is materialised eagerly so the result behaves like an iterable
/// for `for` loops, `list()` and indexing; each match keeps the same group
/// layout as [`match_mode`].
fn finditer(source: &str, text: &str, flags: i64) -> i64 {
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

/// Reads the optional `flags` argument of `re.compile`/`re.finditer`.
///
/// CPython accepts an integer bit set (a `RegexFlag` member or the OR of
/// several); `None` fills an omitted optional parameter in the lowered call,
/// so it maps to the default flag set.
fn flags_argument(raw: Option<i64>) -> Result<i64, i64> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    if raw == NONE {
        return Ok(0);
    }
    match value::as_integer(raw) {
        Some(flags) => Ok(flags),
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

fn sub(source: &str, flags: i64, args: &[i64]) -> i64 {
    if args.len() < 2 || args.len() > 3 {
        return type_error(&format!(
            "sub() takes 2 or 3 arguments ({} given)",
            args.len()
        ));
    }
    let text = match text_argument(args[1], "string") {
        Ok(text) => text,
        Err(err) => return err,
    };
    let limit = if args.len() == 3 {
        match count_argument(args[2], "count") {
            Ok(limit) => limit,
            Err(err) => return err,
        }
    } else {
        0
    };
    let regex = match compile_source(source, flags) {
        Ok(regex) => regex,
        Err(err) => return err,
    };
    if let Some(replacement_text) = value::as_str(args[0]) {
        let repl = replacement(replacement_text);
        let result = regex.replacen(&text, limit, repl.as_str());
        return value::str_alloc(result.into_owned());
    }
    if value::as_function(args[0]).is_none() {
        return type_error(&format!(
            "replacement must be str or callable, not {}",
            value::type_name(args[0])
        ));
    }
    // Callable replacement (`re.sub(pattern, fn, text)`): the callback gets
    // one Match object per non-overlapping match and its result is converted
    // with `str()`.
    let mut out = String::new();
    let mut last = 0usize;
    let mut replaced = 0usize;
    for captures in regex.captures_iter(&text) {
        if limit != 0 && replaced >= limit {
            break;
        }
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        let match_object = make_match(source, &text, &captures);
        let replacement_value = crate::core::builtins::call(args[0], 1, [match_object].as_ptr());
        if exc::pending() {
            return NONE;
        }
        out.push_str(&value::to_string(replacement_value));
        last = whole.end();
        replaced += 1;
        if whole.start() == whole.end() && last < text.len() {
            // Zero-width match: advance one character like CPython's engine.
            let ch = text[last..].chars().next().unwrap_or('\0');
            out.push(ch);
            last += ch.len_utf8();
        }
    }
    out.push_str(&text[last..]);
    value::str_alloc(out)
}

fn split(source: &str, flags: i64, args: &[i64]) -> i64 {
    let text = match text_argument(args[0], "string") {
        Ok(text) => text,
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
    let regex = match compile_source(source, flags) {
        Ok(regex) => regex,
        Err(err) => return err,
    };
    let result = value::list_new(Vec::new());
    if maxsplit == 0 {
        for part in regex.split(&text) {
            push_part(result, part);
        }
    } else {
        for part in regex.splitn(&text, maxsplit.saturating_add(1)) {
            push_part(result, part);
        }
    }
    result
}

/// Appends one split piece to the result list, publishing the GC edge.
fn push_part(result: i64, part: &str) {
    let item = value::str_alloc(part.to_string());
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
            let regex = match compile_source(&source, flags) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            {
                let _ = regex;
                value::pattern_new(source, flags)
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
                    let text = match text_argument(args[1], "string") {
                        Ok(text) => text,
                        Err(err) => return err,
                    };
                    match_mode(
                        &source,
                        &text,
                        function == "fullmatch",
                        function != "search",
                        0,
                    )
                }
                "findall" => {
                    let text = match text_argument(args[1], "string") {
                        Ok(text) => text,
                        Err(err) => return err,
                    };
                    findall(&source, &text, 0)
                }
                "finditer" => {
                    let text = match text_argument(args[1], "string") {
                        Ok(text) => text,
                        Err(err) => return err,
                    };
                    let flags = match flags_argument(args.get(2).copied()) {
                        Ok(flags) => flags,
                        Err(err) => return err,
                    };
                    finditer(&source, &text, flags)
                }
                "sub" => sub(&source, 0, &args[1..]),
                "split" => split(&source, 0, &args[1..]),
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
    let source = state.source.clone();
    let flags = state.flags;
    match name {
        "search" | "match" | "fullmatch" => {
            if args.is_empty() || args.len() > 3 {
                return type_error(&format!(
                    "Pattern.{name}() takes 1 to 3 arguments ({} given)",
                    args.len()
                ));
            }
            let text = match text_argument(args[0], "string") {
                Ok(text) => text,
                Err(err) => return err,
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
                None | Some(NONE) => text.chars().count(),
                Some(value) => match value::as_integer(value) {
                    Some(position) if position >= 0 => position as usize,
                    Some(_) => {
                        return exc::raise_with_message(INDEX_ERROR, "position out of range")
                    }
                    None => return type_error("endpos must be an integer"),
                },
            };
            let Some((slice, base)) = slice_bounds(&text, pos, endpos) else {
                return exc::raise_with_message(INDEX_ERROR, "position out of range");
            };
            let regex = if name == "search" {
                compile_source(&source, flags)
            } else {
                compile_anchored(&source, name == "fullmatch", flags)
            };
            let Ok(regex) = regex else {
                return exc::raise_with_message(VALUE_ERROR, "invalid regular expression");
            };
            match regex.captures(slice) {
                Some(captures) => make_match_at(&source, &text, &captures, base),
                None => NONE,
            }
        }
        "findall" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let text = match text_argument(args[0], "string") {
                Ok(text) => text,
                Err(err) => return err,
            };
            findall(&source, &text, flags)
        }
        "finditer" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let text = match text_argument(args[0], "string") {
                Ok(text) => text,
                Err(err) => return err,
            };
            finditer(&source, &text, flags)
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

/// Methods of a `Match` (`group`, `start`, `end`, `span`).
pub(crate) fn match_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_match(obj) else {
        return attribute_error("Match", name);
    };
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
            .skip(1)
            .map(|position| match position {
                Some((start, end)) => value::str_alloc(
                    state.text[byte_offset(&state.text, *start)..byte_offset(&state.text, *end)]
                        .to_string(),
                ),
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
    if group >= state.groups.len() {
        return exc::raise_with_message(INDEX_ERROR, "no such group");
    }
    let position = state.groups[group];
    match name {
        "group" => match position {
            Some((start, end)) => value::str_alloc(
                state.text[byte_offset(&state.text, start)..byte_offset(&state.text, end)]
                    .to_string(),
            ),
            None => NONE,
        },
        "start" => match position {
            Some((start, _)) => crate::ops::int_result(start as i64),
            None => crate::ops::int_result(-1),
        },
        "end" => match position {
            Some((_, end)) => crate::ops::int_result(end as i64),
            None => crate::ops::int_result(-1),
        },
        "span" => match position {
            Some((start, end)) => value::tuple_new(vec![
                crate::ops::int_result(start as i64),
                crate::ops::int_result(end as i64),
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
        "pattern" => Some(value::str_alloc(state.source.clone())),
        "flags" => Some(crate::ops::int_result(state.flags)),
        _ => None,
    }
}
