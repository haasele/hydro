//! `textwrap`: paragraph wrapping and dedenting.
//!
//! `dedent` follows CPython's set-of-lines algorithm exactly: blank lines are
//! normalised to empty lines and the margin is the longest leading run of
//! spaces/tabs shared by every non-blank line. `indent` splits with
//! `str.splitlines(keepends=True)` semantics and honours an optional predicate.
//!
//! `wrap`/`fill`/`shorten` use the same defaults as CPython's `TextWrapper`
//! (`width=70`, tab expansion to 8 columns, whitespace replacement and
//! dropping, long-word breaking, hyphen-aware chunk splitting). The wrapper is
//! an internal helper; the Python-visible `TextWrapper` class is not provided
//! in this batch, and the module functions are the supported surface. All
//! length calculations count Unicode scalar values, matching `len()` on `str`.

use crate::builtins;
use crate::exc::{self, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Default wrap width, matching `textwrap.TextWrapper.width`.
const DEFAULT_WIDTH: i64 = 70;

/// Column width used when expanding tabs, matching `TextWrapper.tabsize`.
const TABSIZE: usize = 8;

/// Placeholder used by `shorten`, matching CPython's default.
const DEFAULT_PLACEHOLDER: &str = " [...]";

/// Dispatches a native `textwrap` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dedent" => match text_argument(args, 0, "dedent") {
            Ok(text) => value::str_alloc(dedent(&text)),
            Err(error) => error,
        },
        "indent" => indent(args),
        "wrap" => match wrap_lines(args, "wrap") {
            Ok(lines) => value::list_new(lines.into_iter().map(value::str_alloc).collect()),
            Err(error) => error,
        },
        "fill" => match wrap_lines(args, "fill") {
            Ok(lines) => value::str_alloc(lines.join("\n")),
            Err(error) => error,
        },
        "shorten" => match shorten(args) {
            Ok(text) => value::str_alloc(text),
            Err(error) => error,
        },
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'textwrap' has no attribute '{function}'"),
        ),
    }
}

fn type_error(message: String) -> i64 {
    exc::raise_with_message(TYPE_ERROR, &message)
}

/// Copies a string positional argument out of the heap before any allocation.
fn text_argument(args: &[i64], index: usize, function: &str) -> Result<String, i64> {
    match args
        .get(index)
        .and_then(|argument| value::as_str(*argument))
    {
        Some(text) => Ok(text.to_string()),
        None => Err(type_error(format!(
            "{function}() argument {} must be str",
            index + 1
        ))),
    }
}

/// An optional `width` argument; `None` becomes `default`.
fn optional_width(args: &[i64], index: usize, function: &str, default: i64) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(default),
        Some(argument) => match value::as_integer(argument) {
            Some(width) => Ok(width),
            None => Err(type_error(format!(
                "{function}() argument {} must be int",
                index + 1
            ))),
        },
    }
}

/// Python's `str.isspace` for a single character. Rust's `char::is_whitespace`
/// misses the C0 separators `\x1c`-`\x1f`, so they are listed explicitly.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\x0b'
            | '\x0c'
            | '\r'
            | ' '
            | '\x1c'..='\x1f'
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    ) || c.is_whitespace()
}

/// Python's `str.isspace()`; the empty string is not whitespace.
fn is_blank(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_space)
}

/// Characters treated as whitespace by `TextWrapper` before wrapping.
fn is_wrap_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ')
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn trim_space(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `textwrap.dedent` with CPython's current implementation.
fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_blank: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.is_empty() && !is_blank(line))
        .collect();
    if non_blank.is_empty() {
        return lines
            .iter()
            .map(|line| if is_blank(line) { "" } else { *line })
            .collect::<Vec<_>>()
            .join("\n");
    }
    // The shortest and longest lines decide the common indentation: their
    // common leading whitespace is common to every line in between.
    let shortest = non_blank.iter().min().copied().unwrap_or("");
    let longest = non_blank.iter().max().copied().unwrap_or("");
    let mut margin = 0;
    for (index, (left, right)) in shortest.chars().zip(longest.chars()).enumerate() {
        if left != right || !matches!(left, ' ' | '\t') {
            margin = index;
            break;
        }
        margin = index + 1;
    }
    let mut out = String::with_capacity(text.len());
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if is_blank(line) {
            continue;
        }
        out.extend(line.chars().skip(margin));
    }
    out
}

/// Splits `text` exactly like `str.splitlines(keepends=True)`.
fn split_lines_keepends(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        let mut end = index + 1;
        if current == '\r' && chars.get(index + 1) == Some(&'\n') {
            end = index + 2;
        }
        let boundary = matches!(
            current,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if boundary {
            lines.push(chars[start..end].iter().collect());
            start = end;
        }
        index = end;
    }
    if start < chars.len() {
        lines.push(chars[start..].iter().collect());
    }
    lines
}

/// `textwrap.indent`.
fn indent(args: &[i64]) -> i64 {
    let text = match text_argument(args, 0, "indent") {
        Ok(text) => text,
        Err(error) => return error,
    };
    let prefix = match text_argument(args, 1, "indent") {
        Ok(prefix) => prefix,
        Err(error) => return error,
    };
    let predicate = args.get(2).copied().unwrap_or(NONE);
    let mut out = String::new();
    for line in split_lines_keepends(&text) {
        let selected = if predicate == NONE {
            !is_blank(&line)
        } else {
            let argument = [value::str_alloc(line.clone())];
            let result = builtins::call(predicate, 1, argument.as_ptr());
            if exc::pending() {
                return NONE;
            }
            value::truthy(result)
        };
        if selected {
            out.push_str(&prefix);
        }
        out.push_str(&line);
    }
    value::str_alloc(out)
}

/// Expands tabs (to 8-column stops) and replaces wrapping whitespace with
/// spaces, like `TextWrapper._munge_whitespace`.
fn munge_whitespace(text: &str) -> String {
    let mut expanded = String::with_capacity(text.len());
    let mut column = 0usize;
    for character in text.chars() {
        match character {
            '\t' => {
                let spaces = TABSIZE - column % TABSIZE;
                for _ in 0..spaces {
                    expanded.push(' ');
                }
                column += spaces;
            }
            '\n' | '\r' => {
                expanded.push(character);
                column = 0;
            }
            other => {
                expanded.push(other);
                column += 1;
            }
        }
    }
    expanded
        .chars()
        .map(|character| {
            if is_wrap_space(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `[^\d\W]`: a word character that is not a digit.
fn is_letter(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// The `(?= letter -? letter)` lookahead that follows a hyphen split: at
/// least two letters remain, optionally separated by one hyphen.
fn has_hyphen_tail(run: &[char], after: usize) -> bool {
    if !run.get(after).copied().is_some_and(is_letter) {
        return false;
    }
    match run.get(after + 1).copied() {
        Some(character) if is_letter(character) => true,
        Some('-') => run.get(after + 2).copied().is_some_and(is_letter),
        _ => false,
    }
}

/// `[\w!"'&.,?]`, the lookbehind class for em-dashes.
fn is_word_punct(c: char) -> bool {
    is_word_char(c) || matches!(c, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

/// Splits one non-whitespace run on hyphen and em-dash boundaries, mirroring
/// `TextWrapper.wordsep_re`.
fn split_run(run: &[char], chunks: &mut Vec<String>) {
    let length = run.len();
    let mut start = 0;
    let mut index = 0;
    while index < length {
        if run[index] != '-' {
            index += 1;
            continue;
        }
        let mut after = index;
        while after < length && run[after] == '-' {
            after += 1;
        }
        let dashes = after - index;
        let previous = index.checked_sub(1).map(|position| run[position]);
        // Em-dash between words: `(?<=word_punct) -{2,} (?=\w)`.
        if dashes >= 2
            && previous.is_some_and(is_word_punct)
            && run.get(after).copied().is_some_and(is_word_char)
        {
            if index > start {
                chunks.push(run[start..index].iter().collect());
            }
            chunks.push(run[index..after].iter().collect());
            start = after;
            index = after;
            continue;
        }
        // Hyphenated word: `letter{2}-` or `letter-letter-` before the dash,
        // with a letter after it.
        let letter_pair = index >= 2 && is_letter(run[index - 1]) && is_letter(run[index - 2]);
        let letter_dash_letter = index >= 3
            && is_letter(run[index - 1])
            && run[index - 2] == '-'
            && is_letter(run[index - 3]);
        if dashes == 1 && (letter_pair || letter_dash_letter) && has_hyphen_tail(run, after) {
            chunks.push(run[start..after].iter().collect());
            start = after;
        }
        index = after;
    }
    if start < length {
        chunks.push(run[start..].iter().collect());
    }
}

/// `TextWrapper._split`: whitespace runs and word chunks.
fn split_chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if is_wrap_space(chars[index]) {
            let start = index;
            while index < chars.len() && is_wrap_space(chars[index]) {
                index += 1;
            }
            chunks.push(chars[start..index].iter().collect());
            continue;
        }
        let start = index;
        while index < chars.len() && !is_wrap_space(chars[index]) {
            index += 1;
        }
        split_run(&chars[start..index], &mut chunks);
    }
    chunks
}

/// Splits `text` at scalar index `at`.
fn split_at_chars(text: &str, at: usize) -> (String, String) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(at).collect();
    (head, chars.collect())
}

/// `TextWrapper._handle_long_word` with `break_long_words=True` and
/// `break_on_hyphens=True`.
fn handle_long_word(
    chunks: &mut [String],
    cur_line: &mut Vec<String>,
    cur_len: usize,
    width: usize,
) {
    let space_left = if width < 1 {
        1
    } else {
        width.saturating_sub(cur_len)
    };
    if space_left == 0 {
        return;
    }
    let Some(chunk) = chunks.last().cloned() else {
        return;
    };
    let mut end = space_left;
    let chars: Vec<char> = chunk.chars().collect();
    if chars.len() > space_left {
        // Break after the last hyphen inside the available space, as long as
        // there are non-hyphen characters before it.
        if let Some(hyphen) = chars[..space_left.min(chars.len())]
            .iter()
            .rposition(|character| *character == '-')
        {
            if hyphen > 0 && chars[..hyphen].iter().any(|character| *character != '-') {
                end = hyphen + 1;
            }
        }
    }
    let (head, tail) = split_at_chars(&chunk, end);
    cur_line.push(head);
    if let Some(last) = chunks.last_mut() {
        *last = tail;
    }
}

/// `TextWrapper._wrap_chunks` with the default options.
fn wrap_chunks(
    mut chunks: Vec<String>,
    width: usize,
    max_lines: Option<usize>,
    placeholder: &str,
) -> Result<Vec<String>, i64> {
    let placeholder_head = placeholder.trim_start_matches(is_space);
    if max_lines.is_some() && char_len(placeholder_head) > width {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "placeholder too large for max width",
        ));
    }
    chunks.reverse();
    let mut lines: Vec<String> = Vec::new();
    'outer: while !chunks.is_empty() {
        let mut cur_line: Vec<String> = Vec::new();
        let mut cur_len = 0usize;
        if !lines.is_empty() && chunks.last().is_some_and(|chunk| is_blank(chunk)) {
            chunks.pop();
            if chunks.is_empty() {
                break;
            }
        }
        while let Some(chunk) = chunks.last() {
            let length = char_len(chunk);
            if cur_len + length <= width {
                cur_len += length;
                cur_line.push(chunks.pop().unwrap_or_default());
            } else {
                break;
            }
        }
        if chunks.last().is_some_and(|chunk| char_len(chunk) > width) {
            handle_long_word(&mut chunks, &mut cur_line, cur_len, width);
            cur_len = cur_line.iter().map(|chunk| char_len(chunk)).sum();
        }
        if cur_line.last().is_some_and(|chunk| is_blank(chunk)) {
            if let Some(removed) = cur_line.pop() {
                cur_len = cur_len.saturating_sub(char_len(&removed));
            }
        }
        if cur_line.is_empty() {
            continue;
        }
        let fits = max_lines.is_none()
            || lines.len() + 1 < max_lines.unwrap_or(0)
            || ((chunks.is_empty()
                || (chunks.len() == 1 && chunks.last().is_some_and(|chunk| is_blank(chunk))))
                && cur_len <= width);
        if fits {
            lines.push(cur_line.concat());
            continue;
        }
        // Truncation: replace the tail of this line (or a previous line) with
        // the placeholder, like CPython's `_wrap_chunks` max_lines path.
        loop {
            let last_fits = cur_line
                .last()
                .is_some_and(|last| !is_blank(last) && cur_len + char_len(placeholder) <= width);
            if last_fits {
                cur_line.push(placeholder.to_string());
                lines.push(cur_line.concat());
                break 'outer;
            }
            let Some(last) = cur_line.pop() else {
                if let Some(previous) = lines.last_mut() {
                    let trimmed = trim_space(previous).to_string();
                    if char_len(&trimmed) + char_len(placeholder) <= width {
                        *previous = format!("{trimmed}{placeholder}");
                        break 'outer;
                    }
                }
                lines.push(placeholder_head.to_string());
                break 'outer;
            };
            cur_len = cur_len.saturating_sub(char_len(&last));
        }
    }
    Ok(lines)
}

fn wrap_text(
    text: &str,
    width: i64,
    max_lines: Option<usize>,
    placeholder: &str,
) -> Result<Vec<String>, i64> {
    if width <= 0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("invalid width {width} (must be > 0)"),
        ));
    }
    let munged = munge_whitespace(text);
    let chunks = split_chunks(&munged);
    wrap_chunks(chunks, width as usize, max_lines, placeholder)
}

fn wrap_lines(args: &[i64], function: &str) -> Result<Vec<String>, i64> {
    let text = text_argument(args, 0, function)?;
    let width = optional_width(args, 1, function, DEFAULT_WIDTH)?;
    wrap_text(&text, width, None, DEFAULT_PLACEHOLDER)
}

/// `shorten`: collapse whitespace, then wrap to a single line.
fn shorten(args: &[i64]) -> Result<String, i64> {
    let text = text_argument(args, 0, "shorten")?;
    let width = match args.get(1).copied() {
        None | Some(NONE) => {
            return Err(type_error(
                "shorten() missing required argument: 'width'".to_string(),
            ))
        }
        Some(argument) => match value::as_integer(argument) {
            Some(width) => width,
            None => return Err(type_error("shorten() argument 2 must be int".to_string())),
        },
    };
    let placeholder = match args.get(2).copied() {
        None | Some(NONE) => DEFAULT_PLACEHOLDER.to_string(),
        Some(argument) => match value::as_str(argument) {
            Some(text) => text.to_string(),
            None => return Err(type_error("shorten() argument 3 must be str".to_string())),
        },
    };
    let collapsed = trim_space(&text)
        .split(is_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let lines = wrap_text(&collapsed, width, Some(1), &placeholder)?;
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> i64 {
        value::str_alloc(raw.to_string())
    }

    fn strings(list: i64) -> Vec<String> {
        value::as_list(list)
            .expect("list")
            .iter()
            .map(|item| value::as_str(*item).expect("str").to_string())
            .collect()
    }

    #[test]
    fn dedent_matches_cpython() {
        assert_eq!(dedent("  a\n   b\n\n  c\n"), "a\n b\n\nc\n");
        assert_eq!(dedent("\n\tfoo\n\tbar\n\t\tbaz\n"), "\nfoo\nbar\n\tbaz\n");
        assert_eq!(dedent("  \n\t\n  a\n"), "\n\na\n");
        assert_eq!(dedent("  a\n\tb\n"), "  a\n\tb\n");
        assert_eq!(dedent("a\n b\n  c\n"), "a\n b\n  c\n");
        assert_eq!(dedent(""), "");
        assert_eq!(dedent("   \n\t\n"), "\n\n");
        // A common margin is removed even when later lines are deeper.
        assert_eq!(dedent("  if x:\n    y\n"), "if x:\n  y\n");
    }

    #[test]
    fn indent_default_and_predicate() {
        exc::init();
        assert_eq!(indent_text("a\n\nb\n", "> ", NONE), "> a\n\n> b\n");
        // `dedent` returns its argument unchanged for these lines, so it is
        // truthy for every call and stands in for `lambda line: True`.
        assert_eq!(
            indent_text(
                "a\n\nb\n",
                "> ",
                value::native_function_new("textwrap", "dedent", 1)
            ),
            "> a\n> \n> b\n"
        );
        assert_eq!(indent_text("a\r\nb", "  ", NONE), "  a\r\n  b");
        assert_eq!(indent_text("", "> ", NONE), "");
    }

    fn indent_text(text: &str, prefix: &str, predicate: i64) -> String {
        let args = [text_value(text), text_value(prefix), predicate];
        let result = indent(&args);
        assert!(!exc::pending());
        value::as_str(result).expect("str").to_string()
    }

    fn text_value(text: &str) -> i64 {
        value::str_alloc(text.to_string())
    }

    #[test]
    fn wrap_and_fill_match_cpython() {
        assert_eq!(wrap("Hello world", 10), ["Hello", "world"]);
        assert_eq!(wrap("  Hello  world  ", 10), ["  Hello", "world"]);
        assert_eq!(wrap("Hello-World foo", 10), ["Hello-", "World foo"]);
        assert_eq!(wrap("look -- here", 6), ["look", "--", "here"]);
        assert_eq!(wrap("look--here", 6), ["look--", "here"]);
        assert_eq!(wrap("aaa-bbb-ccc", 5), ["aaa-", "bbb-", "ccc"]);
        assert_eq!(wrap("a-b c", 3), ["a-b", "c"]);
        assert_eq!(wrap("x-y-z", 5), ["x-y-z"]);
        assert_eq!(wrap("one  two   three", 7), ["one", "two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a  b", 1), ["a", "b"]);
        assert!(wrap("", 10).is_empty());
        assert_eq!(
            fill("Hello world\nsecond line here", 12),
            "Hello world\nsecond line\nhere"
        );
        assert_eq!(fill("\tfoo\n\tbar", 3), "foo\nbar");
    }

    fn wrap(text: &str, width: i64) -> Vec<String> {
        wrap_lines(
            &[text_value(text), value::encode_int(width).unwrap()],
            "wrap",
        )
        .expect("wrap result")
    }

    fn fill(text: &str, width: i64) -> String {
        wrap(text, width).join("\n")
    }

    #[test]
    fn shorten_matches_cpython() {
        assert_eq!(shorten_text("Hello world", 20, None), "Hello world");
        assert_eq!(shorten_text("Hello world", 11, None), "Hello world");
        assert_eq!(shorten_text("Hello world", 10, None), "[...]");
        assert_eq!(
            shorten_text("Hello  world!  how are you", 15, None),
            "Hello [...]"
        );
        assert_eq!(
            shorten_text("Hello world this is long", 14, None),
            "Hello [...]"
        );
        assert_eq!(shorten_text("Hello world", 11, Some("...")), "Hello world");
        assert_eq!(shorten_text("abc def", 6, Some("...")), "abc...");
    }

    fn shorten_text(text: &str, width: i64, placeholder: Option<&str>) -> String {
        let mut args = vec![text_value(text), value::encode_int(width).unwrap()];
        if let Some(placeholder) = placeholder {
            args.push(text_value(placeholder));
        }
        match shorten(&args) {
            Ok(text) if !exc::pending() => text,
            _ => panic!("shorten failed: {:?}", exc::pending_value()),
        }
    }

    #[test]
    fn module_calls_and_errors() {
        exc::init();
        let result = call("dedent", &[text("  a\n  b\n")]);
        assert_eq!(value::as_str(result), Some("a\nb\n"));
        let lines = call(
            "wrap",
            &[text("one two three"), value::encode_int(3).unwrap()],
        );
        assert_eq!(strings(lines), ["one", "two", "thr", "ee"]);
        assert!(!exc::pending());

        assert_eq!(call("wrap", &[text("x"), text("wide")]), NONE);
        assert!(exc::pending());
        exc::clear();

        assert_eq!(
            call("wrap", &[text("x"), value::encode_int(0).unwrap()]),
            NONE
        );
        assert!(exc::pending());
        exc::clear();

        let number = value::encode_int(1).unwrap();
        assert_eq!(call("dedent", &[number]), NONE);
        assert!(exc::pending());
        exc::clear();

        assert_eq!(call("nope", &[]), NONE);
        assert!(exc::pending());
        exc::clear();
    }
}
