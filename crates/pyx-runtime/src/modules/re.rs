//! `re`: thin wrappers over the `regex` crate with Python-like semantics.
//!
//! Supported: `search`, `match`, `fullmatch`, `findall`, `sub`, `split`,
//! `escape` and `compile` (patterns and matches are native objects with the
//! corresponding methods). Patterns and match objects are immutable and hold
//! no PyX values. Invalid patterns raise `ValueError`; non-string inputs raise
//! `TypeError`.

use regex::Regex;

use crate::exc::{self, ATTRIBUTE_ERROR, INDEX_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// State of an `re.compile()` result.
pub(crate) struct PatternState {
    pub(crate) source: String,
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

fn compile_source(source: &str) -> Result<Regex, i64> {
    Regex::new(source).map_err(regex_error)
}

/// Compiles `source` with an anchored wrapper. The wrapper is non-capturing,
/// so user group numbers are unchanged.
fn compile_anchored(source: &str, full: bool) -> Result<Regex, i64> {
    let wrapped = if full {
        format!(r"\A(?:{source})\z")
    } else {
        format!(r"\A(?:{source})")
    };
    Regex::new(&wrapped).map_err(regex_error)
}

fn make_match(pattern: &str, text: &str, captures: &regex::Captures<'_>) -> i64 {
    let groups = captures
        .iter()
        .map(|group| group.map(|m| (m.start(), m.end())))
        .collect();
    value::match_new(pattern.to_string(), text.to_string(), groups)
}

/// `re.search`/`re.match`/`re.fullmatch`.
fn match_mode(source: &str, text: &str, full: bool, anchored: bool) -> i64 {
    let regex = if anchored {
        match compile_anchored(source, full) {
            Ok(regex) => regex,
            Err(err) => return err,
        }
    } else {
        match compile_source(source) {
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
fn findall(source: &str, text: &str) -> i64 {
    let regex = match compile_source(source) {
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

fn sub(source: &str, args: &[i64]) -> i64 {
    let repl = match value::as_str(args[0]) {
        Some(text) => replacement(text),
        None => {
            return type_error(&format!(
                "replacement must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
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
    let regex = match compile_source(source) {
        Ok(regex) => regex,
        Err(err) => return err,
    };
    let result = regex.replacen(&text, limit, repl.as_str());
    value::str_alloc(result.into_owned())
}

fn split(source: &str, args: &[i64]) -> i64 {
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
    let regex = match compile_source(source) {
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
            if value::as_pattern(args[0]).is_some() {
                return args[0];
            }
            let source = match pattern_source(args[0]) {
                Ok(source) => source,
                Err(err) => return err,
            };
            let regex = match compile_source(&source) {
                Ok(regex) => regex,
                Err(err) => return err,
            };
            {
                let _ = regex;
                value::pattern_new(source)
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
                    )
                }
                "findall" => {
                    let text = match text_argument(args[1], "string") {
                        Ok(text) => text,
                        Err(err) => return err,
                    };
                    findall(&source, &text)
                }
                "sub" => sub(&source, &args[1..]),
                "split" => split(&source, &args[1..]),
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
    match name {
        "search" | "match" | "fullmatch" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let text = match text_argument(args[0], "string") {
                Ok(text) => text,
                Err(err) => return err,
            };
            match_mode(&source, &text, name == "fullmatch", name != "search")
        }
        "findall" => {
            if let Some(err) = pattern_arity(name, args.len(), 1) {
                return err;
            }
            let text = match text_argument(args[0], "string") {
                Ok(text) => text,
                Err(err) => return err,
            };
            findall(&source, &text)
        }
        "sub" => {
            if args.len() < 2 || args.len() > 3 {
                return type_error(&format!(
                    "Pattern.sub() takes 2 to 3 arguments ({} given)",
                    args.len()
                ));
            }
            sub(&source, args)
        }
        "split" => {
            if args.is_empty() || args.len() > 2 {
                return type_error(&format!(
                    "Pattern.split() takes 1 to 2 arguments ({} given)",
                    args.len()
                ));
            }
            split(&source, args)
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
            Some((start, end)) => value::str_alloc(state.text[start..end].to_string()),
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

/// `pattern` data attribute of a compiled pattern.
pub(crate) fn pattern_attr(obj: i64, attr: &str) -> Option<i64> {
    let state = value::as_pattern(obj)?;
    match attr {
        "pattern" => Some(value::str_alloc(state.source.clone())),
        _ => None,
    }
}
