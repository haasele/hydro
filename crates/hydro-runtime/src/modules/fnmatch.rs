//! `fnmatch`: Unix shell-style wildcard matching.
//!
//! The translation and matching rules follow CPython 3.14's `Lib/fnmatch.py`:
//!
//! * A backslash is an ordinary character. There is no escape for wildcards.
//! * `*` and `?` translate to `.*` and `.`, with interior stars rendered as
//!   atomic `(?>.*?fixed)` groups so matching stays linear.
//! * `[...]` is translated as a regular-expression character class, using
//!   CPython's chunking rules for ranges and `re.escape` for every literal.
//! * [`translate`] ends with `\z`, not `\Z`.
//!
//! Matching uses the same class bodies as [`translate`], so wildcard behavior
//! and the emitted regular expression cannot disagree.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::iter;
use crate::value::{self, NONE};

/// One entry of a bracket expression.
#[derive(Clone, Copy)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

/// A `[...]` bracket expression parsed with regular-expression class rules.
struct ClassBody {
    negated: bool,
    items: Vec<ClassItem>,
}

/// The meaning of a scanned `[...]` bracket expression.
enum Bracket {
    /// No closing `]`: CPython emits the `[` as a literal.
    Unterminated,
    /// `(?!)`: an empty range never matches.
    Never,
    /// `.`: a negated empty range matches any character.
    Any,
    /// A real character class: its regex text and its parsed form.
    Class { text: String, body: ClassBody },
}

struct BracketScan {
    outcome: Bracket,
    /// Index just past the closing `]` (or just past the `[` when
    /// unterminated).
    end: usize,
}

/// One element of a pattern, as used by the matcher.
enum Token {
    Star,
    Any,
    Literal(char),
    Class(ClassBody),
    Never,
}

impl Token {
    fn is_star(&self) -> bool {
        matches!(self, Token::Star)
    }
}

/// Parses a bracket expression starting at `start` (which must index `[`).
fn scan_bracket(pattern: &[char], start: usize) -> BracketScan {
    let length = pattern.len();
    let mut close = start + 1;
    if close < length && pattern[close] == '!' {
        close += 1;
    }
    if close < length && pattern[close] == ']' {
        close += 1;
    }
    while close < length && pattern[close] != ']' {
        close += 1;
    }
    if close >= length {
        return BracketScan {
            outcome: Bracket::Unterminated,
            end: start + 1,
        };
    }

    let inner = start + 1;
    let mut stuff: Vec<char> = pattern[inner..close].to_vec();
    if !stuff.contains(&'-') {
        // No ranges: only backslashes need doubling.
        let mut escaped = Vec::with_capacity(stuff.len() * 2);
        for ch in stuff {
            if ch == '\\' {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        stuff = escaped;
    } else {
        // Split the body into range chunks exactly like CPython: each hyphen
        // is a range separator and three characters are skipped afterwards.
        let mut chunks: Vec<Vec<char>> = Vec::new();
        let mut chunk_start = inner;
        let mut scan = if pattern[inner] == '!' {
            inner + 2
        } else {
            inner + 1
        };
        loop {
            if scan >= close {
                break;
            }
            match pattern[scan..close].iter().position(|&ch| ch == '-') {
                Some(offset) => {
                    let hyphen = scan + offset;
                    chunks.push(pattern[chunk_start..hyphen].to_vec());
                    chunk_start = hyphen + 1;
                    scan = hyphen + 3;
                }
                None => break,
            }
        }
        let tail = pattern[chunk_start..close].to_vec();
        if tail.is_empty() {
            // `[a-]`: the trailing hyphen belongs to the previous chunk.
            if let Some(chunk) = chunks.last_mut() {
                chunk.push('-');
            }
        } else {
            chunks.push(tail);
        }
        // A range whose upper bound is below its lower bound is empty; drop it
        // the way CPython does, merging the neighbours.
        let mut index = chunks.len();
        while index >= 2 {
            index -= 1;
            if chunks[index - 1].is_empty() || chunks[index].is_empty() {
                continue;
            }
            let lower = chunks[index - 1][chunks[index - 1].len() - 1];
            let upper = chunks[index][0];
            if lower > upper {
                let mut merged = chunks[index - 1][..chunks[index - 1].len() - 1].to_vec();
                merged.extend_from_slice(&chunks[index][1..]);
                chunks[index - 1] = merged;
                chunks.remove(index);
            }
        }
        // Join the chunks with the (unescaped) range hyphens; every hyphen
        // *inside* a chunk is a literal and must be escaped.
        let mut joined = Vec::new();
        for (position, chunk) in chunks.iter().enumerate() {
            if position > 0 {
                joined.push('-');
            }
            for &ch in chunk {
                match ch {
                    '\\' => {
                        joined.push('\\');
                        joined.push('\\');
                    }
                    '-' => {
                        joined.push('\\');
                        joined.push('-');
                    }
                    _ => joined.push(ch),
                }
            }
        }
        stuff = joined;
    }

    let end = close + 1;
    if stuff.is_empty() {
        return BracketScan {
            outcome: Bracket::Never,
            end,
        };
    }
    if stuff.len() == 1 && stuff[0] == '!' {
        return BracketScan {
            outcome: Bracket::Any,
            end,
        };
    }

    // Escape set operations so `&&`, `~~` and `||` stay literal.
    let mut escaped = Vec::with_capacity(stuff.len() + 4);
    for &ch in &stuff {
        if matches!(ch, '&' | '~' | '|') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    let body_chars: Vec<char> = if escaped[0] == '!' {
        let mut body = Vec::with_capacity(escaped.len());
        body.push('^');
        body.extend_from_slice(&escaped[1..]);
        body
    } else if escaped[0] == '^' || escaped[0] == '[' {
        let mut body = Vec::with_capacity(escaped.len() + 1);
        body.push('\\');
        body.extend_from_slice(&escaped);
        body
    } else {
        escaped
    };

    let text = format!("[{}]", body_chars.iter().copied().collect::<String>());
    let body = parse_class_body(&body_chars);
    BracketScan {
        outcome: Bracket::Class { text, body },
        end,
    }
}

/// Reads one character inside a regex class body, honouring `\` escapes.
fn class_atom(body: &[char], index: usize) -> (char, usize) {
    if body[index] == '\\' && index + 1 < body.len() {
        (body[index + 1], index + 2)
    } else {
        (body[index], index + 1)
    }
}

/// Parses a regex class body (the text between `[` and `]`) into ranges.
fn parse_class_body(body: &[char]) -> ClassBody {
    let mut negated = false;
    let mut index = 0;
    if !body.is_empty() && body[0] == '^' {
        negated = true;
        index = 1;
    }
    let mut items = Vec::new();
    while index < body.len() {
        let (low, next) = class_atom(body, index);
        index = next;
        // `-` forms a range only when a character follows it.
        if index + 1 < body.len() && body[index] == '-' {
            let (high, after) = class_atom(body, index + 1);
            items.push(ClassItem::Range(low, high));
            index = after;
        } else {
            items.push(ClassItem::Char(low));
        }
    }
    ClassBody { negated, items }
}

fn class_matches(body: &ClassBody, ch: char) -> bool {
    let mut matched = body.items.iter().any(|item| match *item {
        ClassItem::Char(expected) => expected == ch,
        ClassItem::Range(low, high) => low <= ch && ch <= high,
    });
    if body.negated {
        matched = !matched;
    }
    matched
}

/// Builds the matcher's token list for `pattern`.
fn tokenize(pattern: &[char]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            '*' => {
                while index < pattern.len() && pattern[index] == '*' {
                    index += 1;
                }
                tokens.push(Token::Star);
            }
            '?' => {
                tokens.push(Token::Any);
                index += 1;
            }
            '[' => {
                let scan = scan_bracket(pattern, index);
                match scan.outcome {
                    Bracket::Unterminated => {
                        tokens.push(Token::Literal('['));
                        index += 1;
                    }
                    Bracket::Never => {
                        tokens.push(Token::Never);
                        index = scan.end;
                    }
                    Bracket::Any => {
                        tokens.push(Token::Any);
                        index = scan.end;
                    }
                    Bracket::Class { body, .. } => {
                        tokens.push(Token::Class(body));
                        index = scan.end;
                    }
                }
            }
            literal => {
                tokens.push(Token::Literal(literal));
                index += 1;
            }
        }
    }
    tokens
}

fn token_matches(token: &Token, ch: char) -> bool {
    match token {
        Token::Literal(expected) => *expected == ch,
        Token::Any => true,
        Token::Class(body) => class_matches(body, ch),
        Token::Never | Token::Star => false,
    }
}

/// Glob match over character slices, with a single backtracking point (the
/// most recent `*`), so ordinary inputs run in `O(len(name) * len(pattern))`
/// time and never recurse.
fn glob_match(name: &[char], tokens: &[Token]) -> bool {
    let (mut ni, mut pi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < name.len() {
        if pi < tokens.len() && tokens[pi].is_star() {
            pi += 1;
            star = Some((pi, ni));
            continue;
        }
        if pi < tokens.len() && token_matches(&tokens[pi], name[ni]) {
            ni += 1;
            pi += 1;
            continue;
        }
        match star {
            Some((star_pi, consumed)) if consumed < name.len() => {
                star = Some((star_pi, consumed + 1));
                ni = consumed + 1;
                pi = star_pi;
            }
            _ => return false,
        }
    }
    while pi < tokens.len() && tokens[pi].is_star() {
        pi += 1;
    }
    pi == tokens.len()
}

/// Whether the value is a `str` (as opposed to bytes-like).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Domain {
    Str,
    Bytes,
}

/// Characters of a `str` (one entry per Unicode scalar) or bytes-like value
/// (ISO-8859-1, matching CPython's bytes patterns), plus its domain.
fn text_chars(raw: i64, argument: &str) -> Result<(Vec<char>, Domain), i64> {
    if let Some(text) = value::as_str(raw) {
        return Ok((text.chars().collect(), Domain::Str));
    }
    if let Some(bytes) = value::as_bytes(raw) {
        return Ok((
            bytes.iter().map(|&byte| byte as char).collect(),
            Domain::Bytes,
        ));
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{argument} must be str or bytes, not {}",
            value::type_name(raw)
        ),
    ))
}

fn domain_mismatch(pattern: Domain) -> i64 {
    let message = match pattern {
        Domain::Str => "cannot use a string pattern on a bytes-like object",
        Domain::Bytes => "cannot use a bytes pattern on a string-like object",
    };
    exc::raise_with_message(TYPE_ERROR, message)
}

fn arity(function: &str, expected: usize, given: usize) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("{function}() takes exactly {expected} argument(s) ({given} given)"),
    )
}

/// Collects the values iterated by `names`, copying lists and tuples so a
/// later allocation cannot invalidate a borrowed slice.
fn collect_names(names: i64) -> Result<Vec<i64>, i64> {
    if let Some(items) = value::as_list(names) {
        return Ok(items.to_vec());
    }
    if let Some(items) = value::as_tuple(names) {
        return Ok(items.to_vec());
    }
    let iterator = iter::iter_of(names);
    if exc::pending() {
        return Err(NONE);
    }
    let mut result = Vec::new();
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if status == 0 {
            if exc::pending() {
                return Err(NONE);
            }
            break;
        }
        result.push(item);
    }
    Ok(result)
}

fn filter(names: i64, pattern: i64, keep_matches: bool) -> i64 {
    let (pattern, pattern_domain) = match text_chars(pattern, "pat") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let tokens = tokenize(&pattern);
    let values = match collect_names(names) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let mut result = Vec::new();
    for value in values {
        let chars = match text_chars(value, "name") {
            Ok((chars, domain)) => {
                if domain != pattern_domain {
                    return domain_mismatch(pattern_domain);
                }
                chars
            }
            Err(error) => return error,
        };
        if glob_match(&chars, &tokens) == keep_matches {
            result.push(value);
        }
    }
    value::list_new(result)
}

/// `re.escape` for one character: CPython 3.14 escapes exactly this set.
fn re_escape_char(ch: char) -> String {
    if matches!(
        ch,
        '(' | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '?'
            | '*'
            | '+'
            | '-'
            | '|'
            | '^'
            | '$'
            | '\\'
            | '.'
            | '&'
            | '~'
            | '#'
            | ' '
            | '\t'
            | '\n'
            | '\r'
            | '\u{0b}'
            | '\u{0c}'
    ) {
        format!("\\{ch}")
    } else {
        ch.to_string()
    }
}

/// `fnmatch._translate(pat, star, question_mark)`: the two-wildcard
/// translator `glob` uses.
///
/// Returns `(fragments, star_indices)`: every literal character and wildcard
/// becomes one regex fragment, `star_indices` records the fragment index of
/// each (compressed) `*`.
fn translate_parts(pattern: &[char], star: &str, question_mark: &str) -> (Vec<String>, Vec<i64>) {
    let mut fragments: Vec<String> = Vec::new();
    let mut star_indices: Vec<i64> = Vec::new();
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            '*' => {
                star_indices.push(fragments.len() as i64);
                fragments.push(star.to_string());
                while index < pattern.len() && pattern[index] == '*' {
                    index += 1;
                }
            }
            '?' => {
                fragments.push(question_mark.to_string());
                index += 1;
            }
            '[' => {
                let scan = scan_bracket(pattern, index);
                match scan.outcome {
                    Bracket::Unterminated => {
                        fragments.push(re_escape_char('['));
                        index += 1;
                    }
                    Bracket::Never => {
                        fragments.push("(?!)".to_string());
                        index = scan.end;
                    }
                    Bracket::Any => {
                        fragments.push(".".to_string());
                        index = scan.end;
                    }
                    Bracket::Class { text, .. } => {
                        fragments.push(text);
                        index = scan.end;
                    }
                }
            }
            literal => {
                fragments.push(re_escape_char(literal));
                index += 1;
            }
        }
    }
    (fragments, star_indices)
}

/// `fnmatch._join_translated_parts`: joins fragments, turning interior
/// `* fixed` pairs into atomic lazy groups and the final star into `.*`.
fn join_translated_parts(parts: &[String], star_indices: &[i64]) -> String {
    if star_indices.is_empty() {
        return format!("(?s:{})\\z", parts.concat());
    }
    let first = star_indices[0] as usize;
    let mut buffer: Vec<String> = parts[..first].to_vec();
    let mut index = first + 1;
    for &star in &star_indices[1..] {
        let star = star as usize;
        buffer.push("(?>.*?".to_string());
        buffer.extend(parts[index..star].iter().cloned());
        buffer.push(")".to_string());
        index = star + 1;
    }
    buffer.push(".*".to_string());
    buffer.extend(parts[index..].iter().cloned());
    format!("(?s:{})\\z", buffer.concat())
}

/// `fnmatch.translate`: shell pattern to an anchored, dot-all regex string.
fn translate(pattern: &[char]) -> String {
    let (parts, star_indices) = translate_parts(pattern, "*", ".");
    join_translated_parts(&parts, &star_indices)
}

/// Module-level dispatch for `fnmatch`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "fnmatch" | "fnmatchcase" => {
            if args.len() != 2 {
                return arity(function, 2, args.len());
            }
            let (name, name_domain) = match text_chars(args[0], "name") {
                Ok(value) => value,
                Err(error) => return error,
            };
            let (pattern, pattern_domain) = match text_chars(args[1], "pat") {
                Ok(value) => value,
                Err(error) => return error,
            };
            if name_domain != pattern_domain {
                return domain_mismatch(pattern_domain);
            }
            value::bool_value(glob_match(&name, &tokenize(&pattern)))
        }
        "filter" | "filterfalse" => {
            if args.len() != 2 {
                return arity(function, 2, args.len());
            }
            filter(args[0], args[1], function == "filter")
        }
        "translate" => {
            if args.len() != 1 {
                return arity(function, 1, args.len());
            }
            let text = match value::as_str(args[0]) {
                Some(text) => text,
                None => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "translate() argument must be str, not {}",
                            value::type_name(args[0])
                        ),
                    )
                }
            };
            let pattern: Vec<char> = text.chars().collect();
            value::str_alloc(translate(&pattern))
        }
        "_translate" => {
            if args.len() != 3 {
                return arity(function, 3, args.len());
            }
            let text = match value::as_str(args[0]) {
                Some(text) => text,
                None => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("pat must be str, not {}", value::type_name(args[0])),
                    )
                }
            };
            let pattern: Vec<char> = text.chars().collect();
            let star = match value::as_str(args[1]) {
                Some(text) => text.to_string(),
                None => {
                    return exc::raise_with_message(
                        exc::TYPE_ERROR,
                        &format!("star must be str, not {}", value::type_name(args[1])),
                    )
                }
            };
            let question = match value::as_str(args[2]) {
                Some(text) => text.to_string(),
                None => {
                    return exc::raise_with_message(
                        exc::TYPE_ERROR,
                        &format!(
                            "question_mark must be str, not {}",
                            value::type_name(args[2])
                        ),
                    )
                }
            };
            let (fragments, star_indices) = translate_parts(&pattern, &star, &question);
            // Build both lists incrementally: each allocated string/number is
            // owned by the list before the next allocation can collect it.
            let fragment_list = value::list_new(Vec::new());
            let index_list = value::list_new(Vec::new());
            let roots = crate::heap::root_values(&[fragment_list, index_list]);
            for fragment in fragments {
                let item = value::str_alloc(fragment);
                container::list_append(fragment_list, item);
            }
            for index in star_indices {
                let item = value::encode_int(index).unwrap_or(NONE);
                container::list_append(index_list, item);
            }
            let result = value::tuple_new(vec![fragment_list, index_list]);
            crate::heap::release_roots(roots);
            result
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'fnmatch' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> i64 {
        value::str_from_bytes(value.as_bytes())
    }

    fn matches(name: &str, pattern: &str) -> bool {
        call("fnmatch", &[text(name), text(pattern)]) == value::TRUE
    }

    fn translated(pattern: &str) -> String {
        value::as_str(call("translate", &[text(pattern)]))
            .expect("translate returns str")
            .to_string()
    }

    #[test]
    fn wildcards_match_like_cpython() {
        assert!(matches("foo.py", "*.py"));
        assert!(matches(".bashrc", "*"));
        assert!(matches("abc", "a?c"));
        assert!(!matches("ac", "a?c"));
        assert!(matches("abc", "a*c"));
        assert!(matches("abc", "a**c"));
        assert!(matches("abc", "*"));
        assert!(matches("", "*"));
        assert!(!matches("abc", "a?"));
        assert!(!matches("abc", "b*"));
        // A backslash is an ordinary character, not an escape.
        assert!(!matches("a*b", "a\\*b"));
        assert!(matches("a\\b", "a\\*b"));
        assert!(matches("a\\xb", "a\\*b"));
        assert!(!matches("a\\b", "a\\\\b"));
        assert!(matches("a\\\\b", "a\\\\b"));
        assert!(!matches("*", "\\*"));
        assert!(matches("\\x", "\\*"));
        assert!(matches("abc", "a[bc]c"));
    }

    #[test]
    fn brackets_and_ranges() {
        assert!(matches("a", "[abc]"));
        assert!(matches("b", "[a-c]"));
        assert!(!matches("d", "[a-c]"));
        assert!(matches("d", "[!a-c]"));
        assert!(!matches("a", "[!a-c]"));
        assert!(matches("]", "[]]"));
        assert!(matches("^", "[\\^]"));
        assert!(matches("x", "[x-]"));
        assert!(matches("-", "[x-]"));
        assert!(matches("[", "[[]"));
        // Unterminated brackets are literal.
        assert!(matches("[", "["));
        assert!(matches("[!]", "[!]"));
        assert!(!matches("a", "[!]"));
        // A reversed range matches nothing, and a negated one matches all.
        assert!(!matches("a", "[z-a]"));
        assert!(matches("a", "[!z-a]"));
    }

    #[test]
    fn translate_has_cpython_shape() {
        assert_eq!(translated("*.py"), r"(?s:.*\.py)\z");
        assert_eq!(translated("a?c"), r"(?s:a.c)\z");
        assert_eq!(translated("[a-c]"), r"(?s:[a-c])\z");
        assert_eq!(translated("[!a]"), r"(?s:[^a])\z");
        assert_eq!(translated("[[]"), r"(?s:[\[])\z");
        assert_eq!(translated("[]]"), r"(?s:[]])\z");
        assert_eq!(translated("a.b"), r"(?s:a\.b)\z");
        assert_eq!(translated("a\\*b"), r"(?s:a\\.*b)\z");
        assert_eq!(translated("**a*a****a"), r"(?s:(?>.*?a)(?>.*?a).*a)\z");
        assert_eq!(translated(""), r"(?s:)\z");
    }

    #[test]
    fn filter_and_filterfalse_keep_original_values() {
        let names = value::list_new(vec![text("a.py"), text("b.txt"), text("c.py")]);
        let pattern = text("*.py");
        let kept = call("filter", &[names, pattern]);
        let items = value::as_list(kept).expect("list").to_vec();
        assert_eq!(items.len(), 2);
        assert_eq!(value::as_str(items[0]), Some("a.py"));
        assert_eq!(value::as_str(items[1]), Some("c.py"));
        let dropped = call("filterfalse", &[names, pattern]);
        let items = value::as_list(dropped).expect("list").to_vec();
        assert_eq!(items.len(), 1);
        assert_eq!(value::as_str(items[0]), Some("b.txt"));
    }

    #[test]
    fn filter_does_not_treat_backslash_as_a_separator() {
        let names = value::list_new(vec![text("usr/bin"), text("usr"), text("usr\\lib")]);
        let kept = call("filter", &[names, text("usr\\*")]);
        let items = value::as_list(kept).expect("list").to_vec();
        assert_eq!(items.len(), 1);
        assert_eq!(value::as_str(items[0]), Some("usr\\lib"));
        let dropped = call("filterfalse", &[names, text("usr/*")]);
        let items = value::as_list(dropped).expect("list").to_vec();
        assert_eq!(items.len(), 2);
        assert_eq!(value::as_str(items[0]), Some("usr"));
        assert_eq!(value::as_str(items[1]), Some("usr\\lib"));
    }

    #[test]
    fn mixed_str_and_bytes_raise_type_error() {
        exc::init();
        let names = value::list_new(vec![text("test")]);
        let bytes_pattern = value::bytes_new(b"*".to_vec());
        assert_eq!(call("filter", &[names, bytes_pattern]), NONE);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn non_string_arguments_report_type_error() {
        exc::init();
        let one = value::encode_int(1).expect("tagged int");
        assert_eq!(call("fnmatch", &[one, one]), NONE);
        assert!(exc::pending());
        exc::clear();
    }
}
