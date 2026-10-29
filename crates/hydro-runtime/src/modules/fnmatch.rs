//! `fnmatch`: Unix shell-style wildcard matching.
//!
//! Linux is case-sensitive, so `fnmatch` and `fnmatchcase` are the same
//! operation here. Patterns support `*`, `?`, `[seq]`, `[!seq]` and a
//! backslash escape for the next character. Matching is iterative with a
//! single backtracking point (the most recent `*`), so ordinary inputs run in
//! `O(len(name) * len(pattern))` time and never recurse.

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

/// A parsed `[...]` bracket expression.
struct ClassBody {
    negated: bool,
    items: Vec<ClassItem>,
    /// Index just past the closing `]`.
    end: usize,
}

/// Parses a bracket expression starting at `start` (which must index `[`).
/// Returns `None` when the bracket is unterminated; CPython then treats the
/// `[` as an ordinary character.
fn parse_class(pattern: &[char], start: usize) -> Option<ClassBody> {
    let length = pattern.len();
    let mut index = start + 1;
    let mut negated = false;
    if index < length && pattern[index] == '!' {
        negated = true;
        index += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while index < length {
        // A `]` closes the class unless it is the first member (`[]]`).
        if pattern[index] == ']' && !first {
            return Some(ClassBody {
                negated,
                items,
                end: index + 1,
            });
        }
        first = false;
        let (low, next) = class_char(pattern, index);
        index = next;
        // `a-z` is a range; a trailing or leading `-` is literal.
        if index + 1 < length && pattern[index] == '-' && pattern[index + 1] != ']' {
            let (high, after) = class_char(pattern, index + 1);
            items.push(ClassItem::Range(low, high));
            index = after;
        } else {
            items.push(ClassItem::Char(low));
        }
    }
    None
}

/// Reads one character inside a bracket expression, honouring a backslash
/// escape. A trailing backslash stands for itself.
fn class_char(pattern: &[char], index: usize) -> (char, usize) {
    if pattern[index] == '\\' && index + 1 < pattern.len() {
        (pattern[index + 1], index + 2)
    } else {
        (pattern[index], index + 1)
    }
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

/// Matches one non-`*` token at `index`. Returns the next pattern index on a
/// match, or `None` when the token does not match `ch`.
fn match_token(ch: char, pattern: &[char], index: usize) -> Option<usize> {
    match pattern[index] {
        '?' => Some(index + 1),
        '[' => match parse_class(pattern, index) {
            Some(body) => {
                if class_matches(&body, ch) {
                    Some(body.end)
                } else {
                    None
                }
            }
            // Unterminated bracket: `[` is a literal.
            None => (ch == '[').then_some(index + 1),
        },
        '\\' => {
            if index + 1 < pattern.len() {
                (ch == pattern[index + 1]).then_some(index + 2)
            } else {
                (ch == '\\').then_some(index + 1)
            }
        }
        literal => (ch == literal).then_some(index + 1),
    }
}

/// Glob match over character slices.
fn glob_match(name: &[char], pattern: &[char]) -> bool {
    let (mut ni, mut pi) = (0, 0);
    // Position just past the most recent `*`, and how much of `name` it has
    // consumed. On a dead end, give the star one more character and retry.
    let mut star: Option<(usize, usize)> = None;
    while ni < name.len() {
        if pi < pattern.len() && pattern[pi] == '*' {
            while pi < pattern.len() && pattern[pi] == '*' {
                pi += 1;
            }
            star = Some((pi, ni));
            continue;
        }
        if pi < pattern.len() {
            if let Some(next) = match_token(name[ni], pattern, pi) {
                ni += 1;
                pi = next;
                continue;
            }
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
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Characters of a `str` (one entry per Unicode scalar) or `bytes`
/// (ISO-8859-1, matching CPython's bytes patterns).
fn text_chars(raw: i64, argument: &str) -> Result<Vec<char>, i64> {
    if let Some(text) = value::as_str(raw) {
        return Ok(text.chars().collect());
    }
    if let Some(bytes) = value::as_bytes(raw) {
        return Ok(bytes.iter().map(|&byte| byte as char).collect());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{argument} must be str or bytes, not {}",
            value::type_name(raw)
        ),
    ))
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
    let pattern = match text_chars(pattern, "pat") {
        Ok(chars) => chars,
        Err(error) => return error,
    };
    let values = match collect_names(names) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let mut result = Vec::new();
    for value in values {
        let chars = match text_chars(value, "name") {
            Ok(chars) => chars,
            Err(error) => return error,
        };
        if glob_match(&chars, &pattern) == keep_matches {
            result.push(value);
        }
    }
    value::list_new(result)
}

/// Escapes one literal for a regular expression, following CPython's
/// `re.escape` set.
fn escape_literal(out: &mut String, ch: char) {
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
        out.push('\\');
    }
    out.push(ch);
}

/// Escapes one character inside a bracket expression. `first` allows a
/// leading `]` to be written literally, as CPython does.
fn escape_class_char(out: &mut String, ch: char, first: bool) {
    match ch {
        '\\' => out.push_str("\\\\"),
        '[' => out.push_str("\\["),
        ']' if first => out.push(']'),
        ']' => out.push_str("\\]"),
        '-' => out.push_str("\\-"),
        '^' => out.push_str("\\^"),
        '&' | '~' | '|' => {
            out.push('\\');
            out.push(ch);
        }
        _ => out.push(ch),
    }
}

/// Renders a parsed bracket expression as regex text.
fn translate_class(body: &ClassBody) -> String {
    if body.items.is_empty() {
        // Empty strict range never matches; a negated empty range matches any
        // character, exactly like CPython.
        return if body.negated {
            ".".to_string()
        } else {
            "(?!)".to_string()
        };
    }
    let mut out = String::from("[");
    if body.negated {
        out.push('^');
    }
    for (position, item) in body.items.iter().enumerate() {
        match *item {
            ClassItem::Char(ch) => escape_class_char(&mut out, ch, position == 0),
            ClassItem::Range(low, high) => {
                escape_class_char(&mut out, low, false);
                out.push('-');
                escape_class_char(&mut out, high, false);
            }
        }
    }
    out.push(']');
    out
}

/// `fnmatch.translate`: shell pattern to an anchored, dot-all regex string.
fn translate(pattern: &[char]) -> String {
    let mut out = String::from("(?s:");
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            '*' => {
                out.push_str(".*");
                while index < pattern.len() && pattern[index] == '*' {
                    index += 1;
                }
            }
            '?' => {
                out.push('.');
                index += 1;
            }
            '[' => match parse_class(pattern, index) {
                Some(body) => {
                    out.push_str(&translate_class(&body));
                    index = body.end;
                }
                None => {
                    escape_literal(&mut out, '[');
                    index += 1;
                }
            },
            '\\' => {
                if index + 1 < pattern.len() {
                    escape_literal(&mut out, pattern[index + 1]);
                    index += 2;
                } else {
                    escape_literal(&mut out, '\\');
                    index += 1;
                }
            }
            literal => {
                escape_literal(&mut out, literal);
                index += 1;
            }
        }
    }
    out.push_str(")\\Z");
    out
}

/// `fnmatch._translate(pat, star, question_mark)`: the two-wildcard
/// translator `glob` uses.
///
/// Returns `(fragments, star_indices)`: every literal character and wildcard
/// becomes one regex fragment, `star_indices` records the fragment index of
/// each (compressed) `*`. The class handling reuses [`parse_class`] /
/// [`translate_class`], so bracket expressions match CPython's rendering.
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
            '[' => match parse_class(pattern, index) {
                Some(body) => {
                    fragments.push(translate_class(&body));
                    index = body.end;
                }
                None => {
                    fragments.push(re_escape_char('['));
                    index += 1;
                }
            },
            literal => {
                fragments.push(re_escape_char(literal));
                index += 1;
            }
        }
    }
    (fragments, star_indices)
}

/// `re.escape` for one character (Python 3.7+ set).
fn re_escape_char(ch: char) -> String {
    if ch.is_whitespace()
        || matches!(
            ch,
            '\\' | '.'
                | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '-'
                | '&'
                | '~'
                | '#'
        )
    {
        format!("\\{ch}")
    } else {
        ch.to_string()
    }
}

/// Module-level dispatch for `fnmatch`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "fnmatch" | "fnmatchcase" => {
            if args.len() != 2 {
                return arity(function, 2, args.len());
            }
            let name = match text_chars(args[0], "name") {
                Ok(chars) => chars,
                Err(error) => return error,
            };
            let pattern = match text_chars(args[1], "pat") {
                Ok(chars) => chars,
                Err(error) => return error,
            };
            value::bool_value(glob_match(&name, &pattern))
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
            let pattern = match text_chars(args[0], "pat") {
                Ok(chars) => chars,
                Err(error) => return error,
            };
            value::str_alloc(translate(&pattern))
        }
        "_translate" => {
            if args.len() != 3 {
                return arity(function, 3, args.len());
            }
            let pattern = match text_chars(args[0], "pat") {
                Ok(chars) => chars,
                Err(error) => return error,
            };
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
        assert!(matches("a*b", "a\\*b"));
        assert!(!matches("axb", "a\\*b"));
        assert!(matches("a\\b", "a\\\\b"));
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
        assert_eq!(translated("*.py"), r"(?s:.*\.py)\Z");
        assert_eq!(translated("a?c"), r"(?s:a.c)\Z");
        assert_eq!(translated("[a-c]"), r"(?s:[a-c])\Z");
        assert_eq!(translated("[!a]"), r"(?s:[^a])\Z");
        assert_eq!(translated("[[]"), r"(?s:[\[])\Z");
        assert_eq!(translated("[]]"), r"(?s:[]])\Z");
        assert_eq!(translated("a.b"), r"(?s:a\.b)\Z");
        assert_eq!(translated("a\\*b"), r"(?s:a\*b)\Z");
        assert_eq!(translated(""), r"(?s:)\Z");
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
    fn non_string_arguments_report_type_error() {
        exc::init();
        let one = value::encode_int(1).expect("tagged int");
        assert_eq!(call("fnmatch", &[one, one]), NONE);
        assert!(exc::pending());
        exc::clear();
    }
}
