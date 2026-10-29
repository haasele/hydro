//! `textwrap`: paragraph wrapping and dedenting.
//!
//! `dedent` follows CPython's set-of-lines algorithm exactly: blank lines are
//! normalised to empty lines and the margin is the longest leading run of
//! spaces/tabs shared by every non-blank line. `indent` splits with
//! `str.splitlines(keepends=True)` semantics and honours an optional predicate.
//!
//! `wrap`/`fill`/`shorten` and `TextWrapper` follow CPython's defaults and
//! keyword surface (`width=70`, `initial_indent`/`subsequent_indent`, tab
//! expansion, whitespace replacement and dropping, long-word and hyphen
//! breaking, `max_lines`/`placeholder`, `fix_sentence_endings`). All length
//! calculations count Unicode scalar values, matching `len()` on `str`.
//!
//! `TextWrapper` is exposed as the module's constructor function: the value
//! bound to `textwrap.TextWrapper` builds an instance of the cached native
//! class (so `type(w)`/`isinstance` see one class per runtime) whose `wrap`
//! and `fill` methods read the instance attributes CPython exposes.

use crate::builtins;
use crate::core::globals;
use crate::exc::{self, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Default wrap width, matching `textwrap.TextWrapper.width`.
const DEFAULT_WIDTH: i64 = 70;

/// Column width used when expanding tabs, matching `TextWrapper.tabsize`.
const TABSIZE: usize = 8;

/// Placeholder used by `shorten`, matching CPython's default.
const DEFAULT_PLACEHOLDER: &str = " [...]";

/// Registry key of the cached `TextWrapper` class.
const CLASS_KEY: &str = "textwrap.TextWrapper.class";

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
        "TextWrapper" => textwrapper_new(args),
        // Instance methods of the native `TextWrapper` class; ordinary
        // positional calls reach them through `_textwrapper_wrap` and
        // `_textwrapper_fill`.
        "_textwrapper_wrap" => textwrapper_method(args, "wrap"),
        "_textwrapper_fill" => textwrapper_method(args, "fill"),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'textwrap' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware `textwrap` dispatch: `wrap`, `fill`, `shorten` and
/// `TextWrapper` forward the `TextWrapper` keyword set.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "wrap" | "fill" | "shorten" | "TextWrapper" => {
            let bound = match bind_keywords(function, args, keywords) {
                Ok(bound) => bound,
                Err(error) => return error,
            };
            call(function, &bound)
        }
        _ if keywords.is_empty() => call(function, args),
        _ => type_error(format!(
            "{function}() got an unexpected keyword argument '{}'",
            keywords[0].0
        )),
    }
}

/// Binds the keyword names of the `TextWrapper` surface into their positional
/// slots. `width` is the first option for `wrap`/`fill`/`shorten` (after the
/// text) and the first parameter of `TextWrapper`.
fn bind_keywords(
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<Vec<i64>, i64> {
    let first_option = if matches!(function, "wrap" | "fill" | "shorten") {
        1
    } else {
        0
    };
    // `shorten` fills `max_lines=1` itself; a caller may still override it.
    let mut slots = args.to_vec();
    while slots.len() < first_option {
        slots.push(NONE);
    }
    for (name, argument) in keywords {
        let Some(offset) = wrapper_option_index(name) else {
            return Err(type_error(format!(
                "{function}() got an unexpected keyword argument '{name}'"
            )));
        };
        let index = first_option + offset;
        while slots.len() < index {
            slots.push(NONE);
        }
        if slots.len() > index {
            if slots[index] != NONE {
                return Err(type_error(format!(
                    "{function}() got multiple values for argument '{name}'"
                )));
            }
            slots[index] = *argument;
        } else {
            slots.push(*argument);
        }
    }
    // Keep the full slot layout so callers can distinguish a three-argument
    // positional `shorten(text, width, placeholder)` call from a keyword call.
    while slots.len() < first_option + 12 {
        slots.push(NONE);
    }
    Ok(slots)
}

/// Position of a `TextWrapper` option in the constructor/`**kwargs` order.
fn wrapper_option_index(name: &str) -> Option<usize> {
    Some(match name {
        "width" => 0,
        "initial_indent" => 1,
        "subsequent_indent" => 2,
        "expand_tabs" => 3,
        "replace_whitespace" => 4,
        "fix_sentence_endings" => 5,
        "break_long_words" => 6,
        "break_on_hyphens" => 7,
        "drop_whitespace" => 8,
        "tabsize" => 9,
        "max_lines" => 10,
        "placeholder" => 11,
        _ => return None,
    })
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

/// `str.expandtabs(tabsize)`: tabs advance to the next tab stop, and `\n`/`\r`
/// reset the column. A non-positive size removes tabs.
fn expand_tabs(text: &str, tabsize: i64) -> String {
    let mut expanded = String::with_capacity(text.len());
    let mut column = 0usize;
    for character in text.chars() {
        match character {
            '\t' => {
                if tabsize > 0 {
                    let tabsize = usize::try_from(tabsize).unwrap_or(usize::MAX);
                    let spaces = tabsize - column % tabsize;
                    for _ in 0..spaces {
                        expanded.push(' ');
                    }
                    column += spaces;
                }
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
}

/// The configuration of a `TextWrapper` instance, matching CPython's
/// constructor and instance attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Wrapper {
    width: i64,
    initial_indent: String,
    subsequent_indent: String,
    expand_tabs: bool,
    replace_whitespace: bool,
    fix_sentence_endings: bool,
    break_long_words: bool,
    break_on_hyphens: bool,
    drop_whitespace: bool,
    tabsize: i64,
    max_lines: Option<usize>,
    placeholder: String,
}

impl Default for Wrapper {
    fn default() -> Wrapper {
        Wrapper {
            width: DEFAULT_WIDTH,
            initial_indent: String::new(),
            subsequent_indent: String::new(),
            expand_tabs: true,
            replace_whitespace: true,
            fix_sentence_endings: false,
            break_long_words: true,
            break_on_hyphens: true,
            drop_whitespace: true,
            tabsize: TABSIZE as i64,
            max_lines: None,
            placeholder: DEFAULT_PLACEHOLDER.to_string(),
        }
    }
}

impl Wrapper {
    /// `TextWrapper._munge_whitespace`.
    fn munge_whitespace(&self, text: &str) -> String {
        let expanded = if self.expand_tabs {
            expand_tabs(text, self.tabsize)
        } else {
            text.to_string()
        };
        if self.replace_whitespace {
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
        } else {
            expanded
        }
    }

    /// `TextWrapper._split`: whitespace runs plus word chunks, with hyphen
    /// splitting only when `break_on_hyphens` allows it.
    fn split(&self, text: &str) -> Vec<String> {
        if self.break_on_hyphens {
            return split_chunks(text);
        }
        split_whitespace_runs(text)
    }

    /// `TextWrapper._fix_sentence_endings`: a chunk ending a sentence followed
    /// by a single space gets two spaces.
    fn fix_sentence_endings(&self, chunks: &mut [String]) {
        let mut index = 0;
        while index + 1 < chunks.len() {
            if chunks[index + 1] == " " && ends_sentence(&chunks[index]) {
                chunks[index + 1] = "  ".to_string();
                index += 2;
            } else {
                index += 1;
            }
        }
    }

    /// `TextWrapper.wrap`.
    fn wrap(&self, text: &str) -> Result<Vec<String>, i64> {
        let munged = self.munge_whitespace(text);
        let mut chunks = self.split(&munged);
        if self.fix_sentence_endings {
            self.fix_sentence_endings(&mut chunks);
        }
        self.wrap_chunks(chunks)
    }
}

/// `TextWrapper.wordsep_simple_re`: whitespace runs and whole words, without
/// hyphen or em-dash splitting.
fn split_whitespace_runs(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let whitespace = is_wrap_space(chars[index]);
        let start = index;
        while index < chars.len() && is_wrap_space(chars[index]) == whitespace {
            index += 1;
        }
        chunks.push(chars[start..index].iter().collect());
    }
    chunks
}

/// `TextWrapper.sentence_end_re = [a-z][.?!]["']?\z`.
fn ends_sentence(chunk: &str) -> bool {
    let chars: Vec<char> = chunk.chars().collect();
    let mut end = chars.len();
    if end > 0 && matches!(chars[end - 1], '"' | '\'') {
        end -= 1;
    }
    if end < 2 {
        return false;
    }
    matches!(chars[end - 1], '.' | '!' | '?') && chars[end - 2].is_ascii_lowercase()
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

/// `TextWrapper._handle_long_word`.
fn handle_long_word(
    break_long_words: bool,
    break_on_hyphens: bool,
    chunks: &mut Vec<String>,
    cur_line: &mut Vec<String>,
    cur_len: usize,
    width: usize,
) {
    if !break_long_words {
        // The word is left intact; it may overflow the line.
        if cur_line.is_empty() {
            if let Some(chunk) = chunks.pop() {
                cur_line.push(chunk);
            }
        }
        return;
    }
    let space_left = if width < 1 {
        1
    } else {
        width.saturating_sub(cur_len)
    };
    let Some(chunk) = chunks.last().cloned() else {
        return;
    };
    let chars: Vec<char> = chunk.chars().collect();
    let mut end = space_left.min(chars.len());
    if break_on_hyphens && chars.len() > space_left {
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
    if !head.is_empty() {
        cur_line.push(head);
    }
    if let Some(last) = chunks.last_mut() {
        *last = tail;
        if last.is_empty() {
            chunks.pop();
        }
    }
}

impl Wrapper {
    /// `TextWrapper._wrap_chunks`.
    fn wrap_chunks(&self, mut chunks: Vec<String>) -> Result<Vec<String>, i64> {
        if self.width <= 0 {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                &format!("invalid width {} (must be > 0)", self.width),
            ));
        }
        let width = usize::try_from(self.width).unwrap_or(usize::MAX);
        let placeholder_head = self.placeholder.trim_start_matches(is_space);
        if let Some(max_lines) = self.max_lines {
            let indent = if max_lines > 1 {
                &self.subsequent_indent
            } else {
                &self.initial_indent
            };
            if char_len(indent) + char_len(placeholder_head) > width {
                return Err(exc::raise_with_message(
                    VALUE_ERROR,
                    "placeholder too large for max width",
                ));
            }
        }
        chunks.reverse();
        let mut lines: Vec<String> = Vec::new();
        'outer: while !chunks.is_empty() {
            let mut cur_line: Vec<String> = Vec::new();
            let mut cur_len = 0usize;
            let indent = if lines.is_empty() {
                &self.initial_indent
            } else {
                &self.subsequent_indent
            };
            let width = width.saturating_sub(char_len(indent));
            if self.drop_whitespace
                && !lines.is_empty()
                && chunks.last().is_some_and(|c| is_blank(c))
            {
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
                handle_long_word(
                    self.break_long_words,
                    self.break_on_hyphens,
                    &mut chunks,
                    &mut cur_line,
                    cur_len,
                    width,
                );
                cur_len = cur_line.iter().map(|chunk| char_len(chunk)).sum();
            }
            if self.drop_whitespace && cur_line.last().is_some_and(|chunk| is_blank(chunk)) {
                if let Some(removed) = cur_line.pop() {
                    cur_len = cur_len.saturating_sub(char_len(&removed));
                }
            }
            if cur_line.is_empty() {
                continue;
            }
            let fits = self.max_lines.is_none()
                || lines.len() + 1 < self.max_lines.unwrap_or(0)
                || ((chunks.is_empty()
                    || (self.drop_whitespace
                        && chunks.len() == 1
                        && chunks.last().is_some_and(|chunk| is_blank(chunk))))
                    && cur_len <= width);
            if fits {
                lines.push(format!("{indent}{}", cur_line.concat()));
                continue;
            }
            // Truncation: replace the tail of this line (or a previous line)
            // with the placeholder, like CPython's `_wrap_chunks` max_lines
            // path.
            loop {
                let last_fits = cur_line.last().is_some_and(|last| {
                    !is_blank(last) && cur_len + char_len(&self.placeholder) <= width
                });
                if last_fits {
                    cur_line.push(self.placeholder.clone());
                    lines.push(format!("{indent}{}", cur_line.concat()));
                    break 'outer;
                }
                let Some(last) = cur_line.pop() else {
                    if let Some(previous) = lines.last_mut() {
                        let trimmed = trim_space(previous).to_string();
                        if char_len(&trimmed) + char_len(&self.placeholder)
                            <= usize::try_from(self.width).unwrap_or(usize::MAX)
                        {
                            *previous = format!("{trimmed}{}", self.placeholder);
                            break 'outer;
                        }
                    }
                    lines.push(format!("{indent}{placeholder_head}"));
                    break 'outer;
                };
                cur_len = cur_len.saturating_sub(char_len(&last));
            }
        }
        Ok(lines)
    }
}

/// Reads a string option from the positional slot layout.
fn option_text(args: &[i64], index: usize, function: &str, default: &str) -> Result<String, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(default.to_string()),
        Some(argument) => match value::as_str(argument) {
            Some(text) => Ok(text.to_string()),
            None => Err(type_error(format!(
                "{function}() argument {} must be str",
                index + 1
            ))),
        },
    }
}

/// Reads an integer option from the positional slot layout.
fn option_int(args: &[i64], index: usize, function: &str, default: i64) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(default),
        Some(argument) => match value::as_integer(argument) {
            Some(number) => Ok(number),
            None => Err(type_error(format!(
                "{function}() argument {} must be int",
                index + 1
            ))),
        },
    }
}

/// Reads a boolean option from the positional slot layout (`None` keeps the
/// default, matching the spec binder's padding).
fn option_bool(args: &[i64], index: usize, default: bool) -> bool {
    match args.get(index).copied() {
        None | Some(NONE) => default,
        Some(argument) => value::truthy(argument),
    }
}

/// Builds a [`Wrapper`] from the positional slot layout shared by `wrap`,
/// `fill`, `shorten` and `TextWrapper`. `first` is the slot holding `width`.
fn wrapper_from_args(args: &[i64], first: usize, function: &str) -> Result<Wrapper, i64> {
    let defaults = Wrapper::default();
    let slot = |offset: usize| first + offset;
    let max_lines = match args.get(slot(10)).copied() {
        None | Some(NONE) => None,
        Some(argument) => match value::as_integer(argument) {
            Some(lines) if lines >= 0 => Some(usize::try_from(lines).unwrap_or(usize::MAX)),
            Some(lines) => {
                return Err(type_error(format!(
                    "{function}() max_lines must be non-negative (got {lines})"
                )))
            }
            None => {
                return Err(type_error(format!(
                    "{function}() argument {} must be int",
                    slot(10) + 1
                )))
            }
        },
    };
    Ok(Wrapper {
        width: option_int(args, slot(0), function, defaults.width)?,
        initial_indent: option_text(args, slot(1), function, &defaults.initial_indent)?,
        subsequent_indent: option_text(args, slot(2), function, &defaults.subsequent_indent)?,
        expand_tabs: option_bool(args, slot(3), defaults.expand_tabs),
        replace_whitespace: option_bool(args, slot(4), defaults.replace_whitespace),
        fix_sentence_endings: option_bool(args, slot(5), defaults.fix_sentence_endings),
        break_long_words: option_bool(args, slot(6), defaults.break_long_words),
        break_on_hyphens: option_bool(args, slot(7), defaults.break_on_hyphens),
        drop_whitespace: option_bool(args, slot(8), defaults.drop_whitespace),
        tabsize: option_int(args, slot(9), function, defaults.tabsize)?,
        max_lines,
        placeholder: option_text(args, slot(11), function, &defaults.placeholder)?,
    })
}

fn wrap_lines(args: &[i64], function: &str) -> Result<Vec<String>, i64> {
    let text = text_argument(args, 0, function)?;
    let options = wrapper_from_args(args, 1, function)?;
    options.wrap(&text)
}

/// `shorten`: collapse whitespace, then wrap to a single line. The third
/// positional argument is the legacy placeholder slot; `placeholder=` binds
/// through [`call_kw`].
fn shorten(args: &[i64]) -> Result<String, i64> {
    let text = text_argument(args, 0, "shorten")?;
    if args.get(1).copied().is_none_or(|width| width == NONE) {
        return Err(type_error(
            "shorten() missing required argument: 'width'".to_string(),
        ));
    }
    let mut options = wrapper_from_args(args, 1, "shorten")?;
    // A three-argument positional call keeps the historical
    // `shorten(text, width, placeholder)` spelling (the spec passes keyword
    // calls with the full 13-slot layout, so this cannot shadow
    // `initial_indent=`).
    if args.len() == 3 && args[2] != NONE {
        match value::as_str(args[2]) {
            Some(placeholder) => {
                options.placeholder = placeholder.to_string();
                options.initial_indent = String::new();
            }
            None => return Err(type_error("shorten() argument 3 must be str".to_string())),
        }
    }
    options.max_lines = Some(1);
    let collapsed = trim_space(&text)
        .split(is_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let lines = options.wrap(&collapsed)?;
    Ok(lines.join("\n"))
}

// -------------------------------------------------------------------------
// TextWrapper
// -------------------------------------------------------------------------

/// Stores the wrapper options as the instance attributes CPython exposes.
fn store_options(instance: i64, options: &Wrapper) {
    let set = |name: &str, item: i64| value::instance_setattr(instance, name.to_string(), item);
    set("width", value::encode_int(options.width).unwrap_or(NONE));
    set(
        "initial_indent",
        value::str_alloc(options.initial_indent.clone()),
    );
    set(
        "subsequent_indent",
        value::str_alloc(options.subsequent_indent.clone()),
    );
    set("expand_tabs", value::bool_value(options.expand_tabs));
    set(
        "replace_whitespace",
        value::bool_value(options.replace_whitespace),
    );
    set(
        "fix_sentence_endings",
        value::bool_value(options.fix_sentence_endings),
    );
    set(
        "break_long_words",
        value::bool_value(options.break_long_words),
    );
    set(
        "break_on_hyphens",
        value::bool_value(options.break_on_hyphens),
    );
    set(
        "drop_whitespace",
        value::bool_value(options.drop_whitespace),
    );
    set(
        "tabsize",
        value::encode_int(options.tabsize).unwrap_or(NONE),
    );
    set(
        "max_lines",
        match options.max_lines {
            Some(lines) => {
                value::encode_int(i64::try_from(lines).unwrap_or(i64::MAX)).unwrap_or(NONE)
            }
            None => NONE,
        },
    );
    set("placeholder", value::str_alloc(options.placeholder.clone()));
}

/// Reads the wrapper options back from an instance, falling back to the
/// defaults for attributes removed or never set.
fn options_of(instance: i64) -> Wrapper {
    let mut options = Wrapper::default();
    if let Some(value) = value::instance_attr(instance, "width") {
        if let Some(width) = value::as_integer(value) {
            options.width = width;
        }
    }
    for (name, target) in [
        ("initial_indent", &mut options.initial_indent),
        ("subsequent_indent", &mut options.subsequent_indent),
        ("placeholder", &mut options.placeholder),
    ] {
        if let Some(value) = value::instance_attr(instance, name) {
            if let Some(text) = value::as_str(value) {
                *target = text.to_string();
            }
        }
    }
    for (name, target) in [
        ("expand_tabs", &mut options.expand_tabs),
        ("replace_whitespace", &mut options.replace_whitespace),
        ("fix_sentence_endings", &mut options.fix_sentence_endings),
        ("break_long_words", &mut options.break_long_words),
        ("break_on_hyphens", &mut options.break_on_hyphens),
        ("drop_whitespace", &mut options.drop_whitespace),
    ] {
        if let Some(value) = value::instance_attr(instance, name) {
            *target = value::truthy(value);
        }
    }
    if let Some(value) = value::instance_attr(instance, "tabsize") {
        if let Some(tabsize) = value::as_integer(value) {
            options.tabsize = tabsize;
        }
    }
    if let Some(value) = value::instance_attr(instance, "max_lines") {
        options.max_lines = value::as_integer(value).and_then(|lines| usize::try_from(lines).ok());
    }
    options
}

/// The cached native `TextWrapper` class: one class per runtime so
/// `isinstance` and `type(w)` stay consistent.
fn textwrapper_class() -> i64 {
    if let Some(cached) = globals::registry_get(CLASS_KEY) {
        return cached;
    }
    let specs: [(&str, &'static str, usize); 2] = [
        ("wrap", "_textwrapper_wrap", 2),
        ("fill", "_textwrapper_fill", 2),
    ];
    let mut values = Vec::with_capacity(specs.len());
    let mut methods = Vec::with_capacity(specs.len());
    for (name, native, arity) in specs {
        let function = value::native_function_new("textwrap", native, arity);
        values.push(function);
        methods.push((name.to_string(), function));
    }
    let method_roots = heap::root_values(&values);
    let class = value::class_new("TextWrapper".to_string(), NONE, methods);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(CLASS_KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// `TextWrapper(**kwargs)`: builds an instance of the cached class with the
/// option attributes installed.
fn textwrapper_new(args: &[i64]) -> i64 {
    let options = match wrapper_from_args(args, 0, "TextWrapper") {
        Ok(options) => options,
        Err(error) => return error,
    };
    let class = textwrapper_class();
    if exc::pending() {
        return NONE;
    }
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance]);
    store_options(instance, &options);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    instance
}

/// `TextWrapper.wrap`/`TextWrapper.fill`.
fn textwrapper_method(args: &[i64], method: &str) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error(format!("TextWrapper.{method}() missing self"));
    };
    let text = match text_argument(args, 1, method) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let options = options_of(instance);
    match options.wrap(&text) {
        Ok(lines) if method == "fill" => value::str_alloc(lines.join("\n")),
        Ok(lines) => value::list_new(lines.into_iter().map(value::str_alloc).collect()),
        Err(error) => error,
    }
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

    fn int(number: i64) -> i64 {
        value::encode_int(number).unwrap()
    }

    fn keyword(name: &str, argument: i64) -> (String, i64) {
        (name.to_string(), argument)
    }

    #[test]
    fn wrap_and_fill_accept_textwrapper_keywords() {
        exc::init();
        let result = call_kw(
            "fill",
            &[text("hello world")],
            &[
                keyword("width", int(20)),
                keyword("initial_indent", text("* ")),
            ],
        );
        assert_eq!(value::as_str(result), Some("* hello world"));
        assert!(!exc::pending());

        let lines = call_kw(
            "wrap",
            &[text("aa bb cc")],
            &[
                keyword("width", int(4)),
                keyword("subsequent_indent", text("..")),
            ],
        );
        assert_eq!(strings(lines), ["aa", "..bb", "..cc"]);

        // Tab expansion is off, but whitespace replacement still turns the
        // tab into a single space.
        let result = call_kw(
            "fill",
            &[text("a\tb")],
            &[
                keyword("width", int(5)),
                keyword("expand_tabs", value::FALSE),
            ],
        );
        assert_eq!(value::as_str(result), Some("a b"));

        let result = call_kw(
            "fill",
            &[text("a\tb")],
            &[
                keyword("width", int(5)),
                keyword("expand_tabs", value::FALSE),
                keyword("replace_whitespace", value::FALSE),
            ],
        );
        assert_eq!(value::as_str(result), Some("a\tb"));

        let lines = call_kw(
            "wrap",
            &[text("aa  bb   cc")],
            &[
                keyword("width", int(4)),
                keyword("drop_whitespace", value::FALSE),
            ],
        );
        assert_eq!(strings(lines), ["aa  ", "bb", "   ", "cc"]);

        let lines = call_kw(
            "wrap",
            &[text("abcdefghij")],
            &[
                keyword("width", int(5)),
                keyword("break_long_words", value::FALSE),
            ],
        );
        assert_eq!(strings(lines), ["abcdefghij"]);

        assert_eq!(wrap("aaa-bbb-ccc", 5), ["aaa-", "bbb-", "ccc"]);
        let lines = call_kw(
            "wrap",
            &[text("aaa-bbb-ccc")],
            &[
                keyword("width", int(5)),
                keyword("break_on_hyphens", value::FALSE),
            ],
        );
        assert_eq!(strings(lines), ["aaa-b", "bb-cc", "c"]);

        let result = call_kw(
            "fill",
            &[text("aa bb cc dd")],
            &[
                keyword("width", int(6)),
                keyword("max_lines", int(1)),
                keyword("placeholder", text("...")),
            ],
        );
        assert_eq!(value::as_str(result), Some("aa..."));

        let lines = call_kw(
            "wrap",
            &[text("One. Two. Three. Four.")],
            &[
                keyword("width", int(12)),
                keyword("fix_sentence_endings", value::TRUE),
            ],
        );
        assert_eq!(strings(lines), ["One.  Two.", "Three.", "Four."]);
        assert!(!exc::pending());
    }

    #[test]
    fn textwrapper_instances_wrap_and_fill() {
        exc::init();
        let wrapper = call(
            "TextWrapper",
            &[int(10), text("> "), text(".."), value::FALSE],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_integer(value::instance_attr(wrapper, "width").expect("width")),
            Some(10)
        );
        assert_eq!(
            value::as_str(value::instance_attr(wrapper, "initial_indent").expect("indent")),
            Some("> ")
        );
        assert_eq!(
            value::instance_attr(wrapper, "expand_tabs").map(value::truthy),
            Some(false)
        );

        let lines = call("_textwrapper_wrap", &[wrapper, text("hello world")]);
        assert_eq!(strings(lines), ["> hello", "..world"]);
        let filled = call("_textwrapper_fill", &[wrapper, text("hello world")]);
        assert_eq!(value::as_str(filled), Some("> hello\n..world"));

        // The class is cached, so repeated construction shares its type.
        assert_eq!(textwrapper_class(), textwrapper_class());
        assert_eq!(
            value::instance_class_value(wrapper),
            Some(textwrapper_class())
        );
        assert!(!exc::pending());
    }

    #[test]
    fn textwrapper_keywords_bind_through_call_kw() {
        exc::init();
        let wrapper = call_kw(
            "TextWrapper",
            &[],
            &[
                keyword("width", int(6)),
                keyword("subsequent_indent", text("- ")),
            ],
        );
        assert!(!exc::pending());
        let filled = call("_textwrapper_fill", &[wrapper, text("a bb cc dd")]);
        assert_eq!(value::as_str(filled), Some("a bb\n- cc\n- dd"));

        assert_eq!(
            call_kw("wrap", &[text("x")], &[keyword("bogus", int(1))]),
            NONE
        );
        assert!(exc::pending());
        exc::clear();
    }
}
