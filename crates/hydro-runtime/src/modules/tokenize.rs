//! `_tokenize`: the CPython C tokenizer surface, deliberately unimplemented.
//!
//! CPython 3.14's accelerator exposes one name, `TokenizerIter`, which turns
//! a source string into `tokenize.TokenInfo` records. Hydro's parser is not
//! the CPython tokenizer and there is no `TokenInfo` type in this runtime, so
//! constructing the iterator raises `NotImplementedError`; no token is ever
//! fabricated. Pure-Python `tokenize` code that calls
//! `_generate_tokens_from_c_tokenizer` therefore fails loudly instead of
//! producing tokens that do not describe the compiled program.
//!
//! The module itself is importable so probe code (`import _tokenize`) and the
//! `from _tokenize import TokenizerIter` import in `Lib/tokenize.py` resolve;
//! every other name stays absent and raises `AttributeError`.
//!
//! This file also owns the data behind the separate `token` module: the
//! numeric constants from CPython's `Lib/token.py`, `tok_name`,
//! `EXACT_TOKEN_TYPES`, `__all__` and the three terminal predicates. The
//! values are the 3.14 numbers; the tables are cached so repeated attribute
//! reads share identity like CPython's module globals.

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// `token` constants in `Lib/token.py` definition order. Order matters for
/// `tok_name` and `__all__` (both follow the module's insertion order).
const TOKENS: &[(&str, i64)] = &[
    ("ENDMARKER", 0),
    ("NAME", 1),
    ("NUMBER", 2),
    ("STRING", 3),
    ("NEWLINE", 4),
    ("INDENT", 5),
    ("DEDENT", 6),
    ("LPAR", 7),
    ("RPAR", 8),
    ("LSQB", 9),
    ("RSQB", 10),
    ("COLON", 11),
    ("COMMA", 12),
    ("SEMI", 13),
    ("PLUS", 14),
    ("MINUS", 15),
    ("STAR", 16),
    ("SLASH", 17),
    ("VBAR", 18),
    ("AMPER", 19),
    ("LESS", 20),
    ("GREATER", 21),
    ("EQUAL", 22),
    ("DOT", 23),
    ("PERCENT", 24),
    ("LBRACE", 25),
    ("RBRACE", 26),
    ("EQEQUAL", 27),
    ("NOTEQUAL", 28),
    ("LESSEQUAL", 29),
    ("GREATEREQUAL", 30),
    ("TILDE", 31),
    ("CIRCUMFLEX", 32),
    ("LEFTSHIFT", 33),
    ("RIGHTSHIFT", 34),
    ("DOUBLESTAR", 35),
    ("PLUSEQUAL", 36),
    ("MINEQUAL", 37),
    ("STAREQUAL", 38),
    ("SLASHEQUAL", 39),
    ("PERCENTEQUAL", 40),
    ("AMPEREQUAL", 41),
    ("VBAREQUAL", 42),
    ("CIRCUMFLEXEQUAL", 43),
    ("LEFTSHIFTEQUAL", 44),
    ("RIGHTSHIFTEQUAL", 45),
    ("DOUBLESTAREQUAL", 46),
    ("DOUBLESLASH", 47),
    ("DOUBLESLASHEQUAL", 48),
    ("AT", 49),
    ("ATEQUAL", 50),
    ("RARROW", 51),
    ("ELLIPSIS", 52),
    ("COLONEQUAL", 53),
    ("EXCLAMATION", 54),
    ("OP", 55),
    ("TYPE_IGNORE", 56),
    ("TYPE_COMMENT", 57),
    ("SOFT_KEYWORD", 58),
    ("FSTRING_START", 59),
    ("FSTRING_MIDDLE", 60),
    ("FSTRING_END", 61),
    ("TSTRING_START", 62),
    ("TSTRING_MIDDLE", 63),
    ("TSTRING_END", 64),
    ("COMMENT", 65),
    ("NL", 66),
    ("ERRORTOKEN", 67),
    ("ENCODING", 68),
    ("N_TOKENS", 69),
    ("NT_OFFSET", 256),
];

/// `EXACT_TOKEN_TYPES`: the operator spellings CPython's parser recognises,
/// in the order `Lib/token.py` defines them.
const EXACT_TOKEN_TYPES: &[(&str, i64)] = &[
    ("!", 54),
    ("!=", 28),
    ("%", 24),
    ("%=", 40),
    ("&", 19),
    ("&=", 41),
    ("(", 7),
    (")", 8),
    ("*", 16),
    ("**", 35),
    ("**=", 46),
    ("*=", 38),
    ("+", 14),
    ("+=", 36),
    (",", 12),
    ("-", 15),
    ("-=", 37),
    ("->", 51),
    (".", 23),
    ("...", 52),
    ("/", 17),
    ("//", 47),
    ("//=", 48),
    ("/=", 39),
    (":", 11),
    (":=", 53),
    (";", 13),
    ("<", 20),
    ("<<", 33),
    ("<<=", 44),
    ("<=", 29),
    ("=", 22),
    ("==", 27),
    (">", 21),
    (">=", 30),
    (">>", 34),
    (">>=", 45),
    ("@", 49),
    ("@=", 50),
    ("[", 9),
    ("]", 10),
    ("^", 32),
    ("^=", 43),
    ("{", 25),
    ("|", 18),
    ("|=", 42),
    ("}", 26),
    ("~", 31),
];

/// The five non-constant entries `Lib/token.py` starts `__all__` with.
const ALL_PREFIX: &[&str] = &[
    "tok_name",
    "ISTERMINAL",
    "ISNONTERMINAL",
    "ISEOF",
    "EXACT_TOKEN_TYPES",
];

fn int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

/// `token.<name>`: the numeric constants plus the three derived tables.
pub(crate) fn token_constant(name: &str) -> Option<i64> {
    if let Some((_, number)) = TOKENS.iter().find(|(token, _)| *token == name) {
        return Some(int(*number));
    }
    match name {
        "tok_name" => Some(tok_name()),
        "EXACT_TOKEN_TYPES" => Some(exact_token_types()),
        "__all__" => Some(all_names()),
        _ => None,
    }
}

/// `tok_name`: an insertion-ordered `{value: name}` dict, cached for identity.
fn tok_name() -> i64 {
    const KEY: &str = "token.tok_name";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let dict = value::dict_new(Vec::new());
    let roots = heap::root_values(&[dict]);
    for (name, number) in TOKENS {
        let key = int(*number);
        let item = value::str_from_bytes(name.as_bytes());
        container::dict_set(dict, key, item);
    }
    globals::registry_set(KEY, dict);
    heap::release_roots(roots);
    dict
}

/// `EXACT_TOKEN_TYPES`: `{operator spelling: token value}`, cached.
fn exact_token_types() -> i64 {
    const KEY: &str = "token.EXACT_TOKEN_TYPES";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let dict = value::dict_new(Vec::new());
    let roots = heap::root_values(&[dict]);
    for (spelling, number) in EXACT_TOKEN_TYPES {
        let key = value::str_from_bytes(spelling.as_bytes());
        let item = int(*number);
        container::dict_set(dict, key, item);
    }
    globals::registry_set(KEY, dict);
    heap::release_roots(roots);
    dict
}

/// `token.__all__`: the five helpers followed by every `tok_name` value, in
/// definition order, cached for identity.
fn all_names() -> i64 {
    const KEY: &str = "token.__all__";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    for name in ALL_PREFIX {
        let item = value::str_from_bytes(name.as_bytes());
        container::list_append(list, item);
    }
    for (name, _) in TOKENS {
        let item = value::str_from_bytes(name.as_bytes());
        container::list_append(list, item);
    }
    globals::registry_set(KEY, list);
    heap::release_roots(roots);
    list
}

/// Dispatches a `token` predicate. The constants are resolved through
/// [`token_constant`], not this function.
pub(crate) fn token_call(function: &str, args: &[i64]) -> i64 {
    let Some(&arg) = args.first() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("token.{function}() takes exactly one argument (0 given)"),
        );
    };
    let Some(number) = value::as_integer(arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "token.{function}() argument must be an integer, not {}",
                value::type_name(arg)
            ),
        );
    };
    match function {
        "ISTERMINAL" => value::bool_value(number < 256),
        "ISNONTERMINAL" => value::bool_value(number >= 256),
        "ISEOF" => value::bool_value(number == 0),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'token' has no attribute '{function}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// `_tokenize`: the CPython tokenizer
// -------------------------------------------------------------------------

const ENDMARKER: i64 = 0;
const TOK_NAME: i64 = 1;
const TOK_NUMBER: i64 = 2;
const TOK_STRING: i64 = 3;
const NEWLINE: i64 = 4;
const INDENT: i64 = 5;
const DEDENT: i64 = 6;
const OP: i64 = 55;
const FSTRING_START: i64 = 59;
const FSTRING_MIDDLE: i64 = 60;
const FSTRING_END: i64 = 61;
const TSTRING_START: i64 = 62;
const TSTRING_MIDDLE: i64 = 63;
const TSTRING_END: i64 = 64;
const COMMENT: i64 = 65;
const NL: i64 = 66;
const ERRORTOKEN: i64 = 67;

const DOT: i64 = 23;
const ELLIPSIS: i64 = 52;

const MAXINDENT: usize = 100;
const MAXLEVEL: usize = 200;
const MAXFSTRINGLEVEL: usize = 150;
const MAX_EXPR_NESTING: i64 = 3;

fn is_string_lit(kind: i64) -> bool {
    matches!(kind, TOK_STRING | FSTRING_MIDDLE | TSTRING_MIDDLE)
}

fn is_digit(c: Option<char>) -> bool {
    c.is_some_and(|ch| ch.is_ascii_digit())
}

fn is_hex_digit(c: Option<char>) -> bool {
    c.is_some_and(|ch| ch.is_ascii_hexdigit())
}

fn is_ident_start(c: Option<char>) -> bool {
    matches!(c, Some(ch) if ch == '_' || ch.is_ascii_alphabetic() || (ch as u32) >= 128)
}

fn is_ident_char(c: Option<char>) -> bool {
    matches!(c, Some(ch) if ch == '_' || ch.is_ascii_alphanumeric() || (ch as u32) >= 128)
}

/// CPython's `Py_UNICODE_ISPRINTABLE`, approximated for error messages.
fn is_printable(ch: char) -> bool {
    if ch.is_control() {
        return false;
    }
    !matches!(
        ch,
        '\u{a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Done {
    Ok,
    Eof,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeKind {
    Regular,
    FString,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrKind {
    F,
    T,
}

#[derive(Clone)]
struct FMode {
    kind: ModeKind,
    string_kind: StrKind,
    quote: char,
    quote_size: usize,
    raw: bool,
    start: usize,
    multi_line_start: usize,
    first_line: i64,
    curly_bracket_depth: i64,
    curly_bracket_expr_start_depth: i64,
    in_format_spec: bool,
    in_debug: bool,
}

impl FMode {
    fn regular() -> FMode {
        FMode {
            kind: ModeKind::Regular,
            string_kind: StrKind::F,
            quote: '\0',
            quote_size: 0,
            raw: false,
            start: 0,
            multi_line_start: 0,
            first_line: 0,
            curly_bracket_depth: 0,
            curly_bracket_expr_start_depth: -1,
            in_format_spec: false,
            in_debug: false,
        }
    }
}

struct Token {
    kind: i64,
    text: String,
    start: (i64, i64),
    end: (i64, i64),
    line: String,
}

struct LexError {
    tag: u32,
    message: String,
    lineno: i64,
    offset: i64,
    end_lineno: Option<i64>,
    end_offset: Option<i64>,
    text: String,
}

struct Lexer {
    src: Vec<char>,
    pos: usize,
    started: bool,
    line_start: usize,
    line_end: usize,
    lineno: i64,
    col_offset: i64,
    atbol: bool,
    indstack: Vec<i64>,
    altindstack: Vec<i64>,
    indent: usize,
    pendin: i64,
    level: usize,
    parenstack: Vec<(char, i64)>,
    cont_line: bool,
    comment_newline: bool,
    implicit_newline: bool,
    extra: bool,
    done: Done,
    first_lineno: i64,
    multi_line_start: usize,
    modes: Vec<FMode>,
    tokens: Vec<Token>,
    error: Option<LexError>,
    start: Option<usize>,
    buf_start: usize,
    buf_pending: bool,
    cache_lineno: i64,
    cache_line: String,
}

impl Lexer {
    fn new(source: &str, extra: bool, implicit_newline: bool) -> Lexer {
        Lexer {
            src: source.chars().collect(),
            pos: 0,
            started: false,
            line_start: 0,
            line_end: 0,
            lineno: 0,
            col_offset: 0,
            atbol: true,
            indstack: vec![0; MAXINDENT],
            altindstack: vec![0; MAXINDENT],
            indent: 0,
            pendin: 0,
            level: 0,
            parenstack: Vec::new(),
            cont_line: false,
            comment_newline: false,
            implicit_newline,
            extra,
            done: Done::Ok,
            first_lineno: 0,
            multi_line_start: 0,
            modes: vec![FMode::regular()],
            tokens: Vec::new(),
            error: None,
            start: None,
            buf_start: 0,
            buf_pending: false,
            cache_lineno: 0,
            cache_line: String::new(),
        }
    }

    fn find_line_end(&self, start: usize) -> usize {
        let mut index = start;
        while index < self.src.len() {
            if self.src[index] == '\n' {
                return index + 1;
            }
            index += 1;
        }
        self.src.len()
    }

    /// Exclusive end of the current line's text (the artificial newline of an
    /// unterminated last line is not part of any token's `line` field).
    fn line_text_end(&self) -> usize {
        if self.implicit_newline && self.line_end == self.src.len() {
            self.line_end.saturating_sub(1)
        } else {
            self.line_end
        }
    }

    fn check_null_bytes(&mut self) -> bool {
        if self.src[self.line_start.min(self.src.len())..self.line_end.min(self.src.len())]
            .iter()
            .any(|ch| *ch == '\0')
        {
            self.set_error(
                exc::SYNTAX_ERROR,
                "source code cannot contain null bytes".to_string(),
            );
            return false;
        }
        true
    }

    fn nextc(&mut self) -> Option<char> {
        if !self.started {
            self.started = true;
            if self.src.is_empty() {
                if self.done == Done::Ok {
                    self.done = Done::Eof;
                }
                return None;
            }
            self.lineno = 1;
            self.line_start = 0;
            self.line_end = self.find_line_end(0);
            if self.buf_pending {
                self.buf_start = 0;
                self.buf_pending = false;
            }
            if !self.check_null_bytes() {
                return None;
            }
        }
        if self.pos >= self.src.len() {
            if self.done == Done::Ok {
                self.done = Done::Eof;
            }
            return None;
        }
        if self.pos >= self.line_end {
            self.line_start = self.line_end;
            self.lineno += 1;
            self.line_end = self.find_line_end(self.line_start);
            if self.buf_pending {
                self.buf_start = self.line_start;
                self.buf_pending = false;
            }
            if !self.check_null_bytes() {
                return None;
            }
            if self.pos >= self.src.len() {
                if self.done == Done::Ok {
                    self.done = Done::Eof;
                }
                return None;
            }
        }
        if self.buf_pending {
            self.buf_start = self.line_start;
            self.buf_pending = false;
        }
        let ch = self.src[self.pos];
        self.pos += 1;
        self.col_offset += 1;
        Some(ch)
    }

    fn backup(&mut self, c: Option<char>) {
        if c.is_some() && self.pos > 0 {
            self.pos -= 1;
            if self.col_offset > 0 {
                self.col_offset -= 1;
            }
        }
    }

    fn set_error(&mut self, tag: u32, message: String) {
        if self.error.is_some() {
            return;
        }
        let stop = self.line_text_end();
        let start = self.line_start.min(stop);
        let text: String = self.src[start..stop].iter().collect();
        let offset = (self.pos.saturating_sub(self.line_start)) as i64;
        self.error = Some(LexError {
            tag,
            message,
            lineno: self.lineno,
            offset,
            end_lineno: None,
            end_offset: None,
            text,
        });
        self.done = Done::Error;
    }

    fn syntax_error(&mut self, message: String) {
        self.set_error(exc::SYNTAX_ERROR, message);
    }

    /// `_PyLexer_token_setup` + `tokenizeriter_next`: converts the internal
    /// token into the `TokenInfo` values `tokenize` builds.
    fn push(&mut self, kind: i64, start: Option<usize>, end: Option<usize>) {
        let mut kind = kind;
        let is_extra = self.extra;
        let reported_lineno = if is_string_lit(kind) {
            self.first_lineno
        } else {
            self.lineno
        };
        let mut lineno = reported_lineno;
        let mut end_lineno = self.lineno;
        let mut col = -1i64;
        let mut end_col = -1i64;

        let trailing = kind == ENDMARKER || (kind == DEDENT && self.done == Done::Eof);
        let suppressed_line = is_extra && trailing;

        let line = if suppressed_line {
            String::new()
        } else if self.lineno != self.cache_lineno {
            let line_ref = if is_string_lit(kind) {
                self.multi_line_start
            } else {
                self.line_start
            };
            let stop = if self.done == Done::Eof && self.pos >= self.src.len() {
                self.line_start.min(self.src.len())
            } else {
                self.line_text_end()
            };
            let start_ref = line_ref.min(stop);
            let computed: String = self.src[start_ref..stop].iter().collect();
            self.cache_line = computed.clone();
            computed
        } else {
            self.cache_line.clone()
        };

        if let Some(s) = start {
            let line_ref = if is_string_lit(kind) {
                self.multi_line_start
            } else {
                self.line_start
            };
            if s >= line_ref {
                col = (s - line_ref) as i64;
            }
        }
        if let Some(e) = end {
            if e >= self.line_start {
                end_col = (e - self.line_start) as i64;
            }
        }

        let mut text: String = match (start, end) {
            (Some(s), Some(e)) if e >= s => self.src[s..e].iter().collect(),
            _ => String::new(),
        };

        if is_extra {
            if trailing {
                lineno += 1;
                end_lineno = lineno;
                col = 0;
                end_col = 0;
            }
            if kind > DEDENT && kind < OP {
                kind = OP;
            } else if kind == NEWLINE {
                if !self.implicit_newline {
                    let first = start.and_then(|s| self.src.get(s)).copied();
                    text = if first == Some('\r') {
                        "\r\n".to_string()
                    } else {
                        "\n".to_string()
                    };
                }
                if end_col >= 0 {
                    end_col += 1;
                }
            } else if kind == NL && self.implicit_newline {
                text = String::new();
            }
        }

        self.cache_lineno = reported_lineno;
        self.tokens.push(Token {
            kind,
            text,
            start: (lineno, col),
            end: (end_lineno, end_col),
            line,
        });
    }

    fn mode_index(&self) -> usize {
        self.modes.len() - 1
    }

    fn current_mode(&self) -> &FMode {
        &self.modes[self.mode_index()]
    }

    fn inside_fstring(&self) -> bool {
        self.modes.len() > 1
    }

    fn inside_fstring_expr(&self) -> bool {
        self.current_mode().curly_bracket_expr_start_depth >= 0
    }

    fn inside_fstring_expr_at_top(&self) -> bool {
        let mode = self.current_mode();
        mode.curly_bracket_depth - mode.curly_bracket_expr_start_depth == 1
    }

    fn fstring_prefix(&self) -> char {
        if self.current_mode().string_kind == StrKind::T {
            't'
        } else {
            'f'
        }
    }

    fn get_token(&mut self) -> i64 {
        if self.error.is_some() {
            return ERRORTOKEN;
        }
        if self.current_mode().kind == ModeKind::Regular {
            self.normal_mode()
        } else {
            self.fstring_mode()
        }
    }

    fn continuation_line(&mut self) -> bool {
        let mut c = self.nextc();
        if c == Some('\r') {
            c = self.nextc();
        }
        if c != Some('\n') {
            self.set_error(
                exc::SYNTAX_ERROR,
                "unexpected character after line continuation character".to_string(),
            );
            return false;
        }
        c = self.nextc();
        if c.is_none() {
            if self.done == Done::Ok {
                self.done = Done::Eof;
            }
            self.pos = self.src.len();
            return false;
        }
        self.backup(c);
        true
    }

    fn decimal_tail(&mut self) -> Result<Option<char>, ()> {
        let mut c;
        loop {
            loop {
                c = self.nextc();
                if !is_digit(c) {
                    break;
                }
            }
            if c != Some('_') {
                break;
            }
            c = self.nextc();
            if !is_digit(c) {
                self.backup(c);
                self.syntax_error("invalid decimal literal".to_string());
                return Err(());
            }
        }
        Ok(c)
    }

    fn lookahead(&mut self, test: &str) -> bool {
        let chars: Vec<char> = test.chars().collect();
        let mut index = 0usize;
        loop {
            let c = self.nextc();
            if index >= chars.len() {
                let result = !is_ident_char(c);
                self.backup(c);
                while index > 0 {
                    index -= 1;
                    self.backup(Some(chars[index]));
                }
                return result;
            }
            if c == Some(chars[index]) {
                index += 1;
                continue;
            }
            self.backup(c);
            while index > 0 {
                index -= 1;
                self.backup(Some(chars[index]));
            }
            return false;
        }
    }

    fn verify_end_of_number(&mut self, c: Option<char>, kind: &str) -> bool {
        if self.extra {
            return true;
        }
        let Some(ch) = c else {
            return true;
        };
        let invalid = match ch {
            'a' => self.lookahead("nd"),
            'e' => self.lookahead("lse"),
            'f' => self.lookahead("or"),
            'i' => {
                let c2 = self.nextc();
                let matched = matches!(c2, Some('f') | Some('n') | Some('s'));
                self.backup(c2);
                matched
            }
            'o' => self.lookahead("r"),
            'n' => self.lookahead("ot"),
            _ => false,
        };
        if invalid {
            self.backup(Some(ch));
            self.nextc();
            return true;
        }
        if (ch as u32) < 128 && is_ident_char(Some(ch)) {
            self.backup(Some(ch));
            self.syntax_error(format!("invalid {kind} literal"));
            return false;
        }
        true
    }

    /// `_PyToken_OneChar`-style finish: backs the lookahead up and emits the
    /// number token.
    fn finish_number(&mut self, c: Option<char>) {
        self.backup(c);
        let start = self.start;
        let end = Some(self.pos);
        self.push(TOK_NUMBER, start, end);
    }

    fn exponent(&mut self, e: Option<char>) -> Result<Option<Option<char>>, ()> {
        let mut c = self.nextc();
        if c == Some('+') || c == Some('-') {
            c = self.nextc();
            if !is_digit(c) {
                self.backup(c);
                self.syntax_error("invalid decimal literal".to_string());
                return Err(());
            }
        } else if !is_digit(c) {
            self.backup(c);
            if !self.verify_end_of_number(e, "decimal") {
                return Err(());
            }
            self.backup(e);
            return Ok(None);
        }
        Ok(Some(self.decimal_tail()?))
    }

    /// Scans a numeric literal starting at `first` (the current lookahead).
    /// `from_dot` marks a literal whose first character was `.`.
    fn scan_number(&mut self, first: Option<char>, from_dot: bool) -> bool {
        let mut c = first;
        if !from_dot && c == Some('0') {
            c = self.nextc();
            if matches!(c, Some('x') | Some('X')) {
                c = self.nextc();
                loop {
                    if c == Some('_') {
                        c = self.nextc();
                    }
                    if !is_hex_digit(c) {
                        self.backup(c);
                        self.syntax_error("invalid hexadecimal literal".to_string());
                        return false;
                    }
                    loop {
                        c = self.nextc();
                        if !is_hex_digit(c) {
                            break;
                        }
                    }
                    if c != Some('_') {
                        break;
                    }
                }
                if !self.verify_end_of_number(c, "hexadecimal") {
                    return false;
                }
                self.finish_number(c);
                return true;
            } else if matches!(c, Some('o') | Some('O')) {
                c = self.nextc();
                loop {
                    if c == Some('_') {
                        c = self.nextc();
                    }
                    if !matches!(c, Some(d) if ('0'..='7').contains(&d)) {
                        if is_digit(c) {
                            let d = c.unwrap_or('?');
                            self.syntax_error(format!("invalid digit '{d}' in octal literal"));
                        } else {
                            self.backup(c);
                            self.syntax_error("invalid octal literal".to_string());
                        }
                        return false;
                    }
                    loop {
                        c = self.nextc();
                        if !matches!(c, Some(d) if ('0'..='7').contains(&d)) {
                            break;
                        }
                    }
                    if c != Some('_') {
                        break;
                    }
                }
                if is_digit(c) {
                    let d = c.unwrap_or('?');
                    self.syntax_error(format!("invalid digit '{d}' in octal literal"));
                    return false;
                }
                if !self.verify_end_of_number(c, "octal") {
                    return false;
                }
                self.finish_number(c);
                return true;
            } else if matches!(c, Some('b') | Some('B')) {
                c = self.nextc();
                loop {
                    if c == Some('_') {
                        c = self.nextc();
                    }
                    if !matches!(c, Some('0') | Some('1')) {
                        if is_digit(c) {
                            let d = c.unwrap_or('?');
                            self.syntax_error(format!("invalid digit '{d}' in binary literal"));
                        } else {
                            self.backup(c);
                            self.syntax_error("invalid binary literal".to_string());
                        }
                        return false;
                    }
                    loop {
                        c = self.nextc();
                        if !matches!(c, Some('0') | Some('1')) {
                            break;
                        }
                    }
                    if c != Some('_') {
                        break;
                    }
                }
                if is_digit(c) {
                    let d = c.unwrap_or('?');
                    self.syntax_error(format!("invalid digit '{d}' in binary literal"));
                    return false;
                }
                if !self.verify_end_of_number(c, "binary") {
                    return false;
                }
                self.finish_number(c);
                return true;
            }
            let mut nonzero = false;
            loop {
                if c == Some('_') {
                    c = self.nextc();
                    if !is_digit(c) {
                        self.backup(c);
                        self.syntax_error("invalid decimal literal".to_string());
                        return false;
                    }
                }
                if c != Some('0') {
                    break;
                }
                c = self.nextc();
            }
            if is_digit(c) {
                nonzero = true;
                c = match self.decimal_tail() {
                    Ok(value) => value,
                    Err(()) => return false,
                };
            }
            if c == Some('.') {
                c = self.nextc();
                if is_digit(c) {
                    c = match self.decimal_tail() {
                        Ok(value) => value,
                        Err(()) => return false,
                    };
                }
                if matches!(c, Some('e') | Some('E')) {
                    match self.exponent(c) {
                        Ok(Some(value)) => c = value,
                        Ok(None) => {
                            let start = self.start;
                            let end = Some(self.pos);
                            self.push(TOK_NUMBER, start, end);
                            return true;
                        }
                        Err(()) => return false,
                    }
                }
                if matches!(c, Some('j') | Some('J')) {
                    c = self.nextc();
                    if !self.verify_end_of_number(c, "imaginary") {
                        return false;
                    }
                } else if !self.verify_end_of_number(c, "decimal") {
                    return false;
                }
            } else if matches!(c, Some('e') | Some('E')) {
                match self.exponent(c) {
                    Ok(Some(value)) => c = value,
                    Ok(None) => {
                        let start = self.start;
                        let end = Some(self.pos);
                        self.push(TOK_NUMBER, start, end);
                        return true;
                    }
                    Err(()) => return false,
                }
                if matches!(c, Some('j') | Some('J')) {
                    c = self.nextc();
                    if !self.verify_end_of_number(c, "imaginary") {
                        return false;
                    }
                } else if !self.verify_end_of_number(c, "decimal") {
                    return false;
                }
            } else if matches!(c, Some('j') | Some('J')) {
                c = self.nextc();
                if !self.verify_end_of_number(c, "imaginary") {
                    return false;
                }
            } else if nonzero && !self.extra {
                self.backup(c);
                self.syntax_error(
                    "leading zeros in decimal integer literals are not permitted; \
                     use an 0o prefix for octal integers"
                        .to_string(),
                );
                return false;
            } else if !self.verify_end_of_number(c, "decimal") {
                return false;
            }
            self.finish_number(c);
            return true;
        }

        c = match self.decimal_tail() {
            Ok(value) => value,
            Err(()) => return false,
        };
        if !from_dot && c == Some('.') {
            c = self.nextc();
            if is_digit(c) {
                c = match self.decimal_tail() {
                    Ok(value) => value,
                    Err(()) => return false,
                };
            }
        }
        if matches!(c, Some('e') | Some('E')) {
            match self.exponent(c) {
                Ok(Some(value)) => c = value,
                Ok(None) => {
                    let start = self.start;
                    let end = Some(self.pos);
                    self.push(TOK_NUMBER, start, end);
                    return true;
                }
                Err(()) => return false,
            }
        }
        if matches!(c, Some('j') | Some('J')) {
            c = self.nextc();
            if !self.verify_end_of_number(c, "imaginary") {
                return false;
            }
        } else if !self.verify_end_of_number(c, "decimal") {
            return false;
        }
        self.finish_number(c);
        true
    }

    fn scan_string(&mut self, quote: char) -> i64 {
        let mut quote_size = 1usize;
        let mut end_quote_size = 0usize;
        let mut has_escaped_quote = false;
        self.first_lineno = self.lineno;
        self.multi_line_start = self.line_start;
        let mut c = self.nextc();
        if c == Some(quote) {
            c = self.nextc();
            if c == Some(quote) {
                quote_size = 3;
            } else {
                end_quote_size = 1;
            }
        }
        if c != Some(quote) {
            self.backup(c);
        }
        while end_quote_size != quote_size {
            c = self.nextc();
            if self.error.is_some() {
                return ERRORTOKEN;
            }
            if c.is_none() || (quote_size == 1 && c == Some('\n')) {
                let start_line = self.first_lineno;
                if quote_size == 3 {
                    self.syntax_error(format!(
                        "unterminated triple-quoted string literal (detected at line {start_line})"
                    ));
                } else if has_escaped_quote {
                    self.syntax_error(format!(
                        "unterminated string literal (detected at line {start_line}); \
                         perhaps you escaped the end quote?"
                    ));
                } else {
                    self.syntax_error(format!(
                        "unterminated string literal (detected at line {start_line})"
                    ));
                }
                return ERRORTOKEN;
            }
            if c == Some(quote) {
                end_quote_size += 1;
            } else {
                end_quote_size = 0;
                if c == Some('\\') {
                    c = self.nextc();
                    if c == Some(quote) {
                        has_escaped_quote = true;
                    }
                    if c == Some('\r') {
                        c = self.nextc();
                    }
                }
            }
        }
        let start = self.start.unwrap_or(self.pos);
        let end = self.pos;
        self.push(TOK_STRING, Some(start), Some(end));
        TOK_STRING
    }

    /// `f_string_quote`: enters f-string/t-string mode for a literal whose
    /// prefixes have just been consumed.
    fn start_fstring(&mut self, quote: char) -> i64 {
        let mut quote_size = 1usize;
        self.first_lineno = self.lineno;
        self.multi_line_start = self.line_start;
        let after_quote = self.nextc();
        if after_quote == Some(quote) {
            let after_after = self.nextc();
            if after_after == Some(quote) {
                quote_size = 3;
            } else {
                self.backup(after_after);
                self.backup(after_quote);
            }
        }
        if after_quote != Some(quote) {
            self.backup(after_quote);
        }
        let start = self.start.unwrap_or(self.pos);
        let end = self.pos;
        if self.modes.len() + 1 >= MAXFSTRINGLEVEL {
            self.syntax_error("too many nested f-strings or t-strings".to_string());
            return ERRORTOKEN;
        }
        let prefix = self
            .src
            .get(start)
            .copied()
            .unwrap_or('f')
            .to_ascii_lowercase();
        let mut string_kind = StrKind::F;
        let mut raw = false;
        let second = self
            .src
            .get(start + 1)
            .copied()
            .map(|ch| ch.to_ascii_lowercase());
        match prefix {
            't' => {
                raw = second == Some('r');
                string_kind = StrKind::T;
            }
            'f' => {
                raw = second == Some('r');
            }
            'r' => {
                raw = true;
                if second == Some('t') {
                    string_kind = StrKind::T;
                }
            }
            _ => {}
        }
        let mode = FMode {
            kind: ModeKind::FString,
            string_kind,
            quote,
            quote_size,
            raw,
            start,
            multi_line_start: self.line_start,
            first_line: self.lineno,
            curly_bracket_depth: 0,
            curly_bracket_expr_start_depth: -1,
            in_format_spec: false,
            in_debug: false,
        };
        self.modes.push(mode);
        let kind = if string_kind == StrKind::T {
            TSTRING_START
        } else {
            FSTRING_START
        };
        self.push(kind, Some(start), Some(end));
        kind
    }

    fn fstring_error(&mut self, message: String) -> i64 {
        self.syntax_error(message);
        ERRORTOKEN
    }

    fn fstring_mode(&mut self) -> i64 {
        let entry = self.pos;
        self.start = Some(entry);
        self.first_lineno = self.lineno;
        let start_char = self.nextc();
        if start_char == Some('{') {
            let peek1 = self.nextc();
            self.backup(peek1);
            let double = peek1 == Some('{');
            self.backup(start_char);
            if !double {
                let index = self.mode_index();
                self.modes[index].curly_bracket_expr_start_depth += 1;
                if self.modes[index].curly_bracket_expr_start_depth >= MAX_EXPR_NESTING {
                    return self.fstring_error("expressions nested too deeply".to_string());
                }
                self.modes[index].kind = ModeKind::Regular;
                return self.normal_mode();
            }
        } else {
            self.backup(start_char);
        }

        let quote = self.current_mode().quote;
        let quote_size = self.current_mode().quote_size;
        for _ in 0..quote_size {
            let q = self.nextc();
            if q != Some(quote) {
                self.backup(q);
                return self.fstring_middle();
            }
        }
        let kind = if self.current_mode().string_kind == StrKind::T {
            TSTRING_END
        } else {
            FSTRING_END
        };
        let end = self.pos;
        self.modes.pop();
        self.push(kind, Some(entry), Some(end));
        kind
    }

    fn middle_kind(&self, index: usize) -> i64 {
        if self.modes[index].string_kind == StrKind::T {
            TSTRING_MIDDLE
        } else {
            FSTRING_MIDDLE
        }
    }

    fn fstring_middle(&mut self) -> i64 {
        let index = self.mode_index();
        self.multi_line_start = self.line_start;
        let mut end_quote_size = 0usize;
        let mut unicode_escape = false;
        while end_quote_size != self.modes[index].quote_size {
            let c = self.nextc();
            if self.error.is_some() {
                return ERRORTOKEN;
            }
            let quote_size = self.modes[index].quote_size;
            let quote = self.modes[index].quote;
            let raw = self.modes[index].raw;
            let in_format_spec = self.modes[index].in_format_spec;
            let prefix = if self.modes[index].string_kind == StrKind::T {
                't'
            } else {
                'f'
            };
            if c.is_none() || (quote_size == 1 && c == Some('\n')) {
                if in_format_spec && c == Some('\n') {
                    if quote_size == 1 {
                        return self.fstring_error(format!(
                            "newlines are not allowed in format specifiers for single \
                             quoted {prefix}-strings"
                        ));
                    }
                    self.backup(c);
                    self.modes[index].kind = ModeKind::Regular;
                    self.modes[index].in_format_spec = false;
                    let start = self.start.unwrap_or(self.pos);
                    let end = self.pos;
                    let kind = self.middle_kind(index);
                    self.push(kind, Some(start), Some(end));
                    return kind;
                }
                let start_line = self.modes[index].first_line;
                if quote_size == 3 {
                    return self.fstring_error(format!(
                        "unterminated triple-quoted {prefix}-string literal \
                         (detected at line {start_line})"
                    ));
                }
                return self.fstring_error(format!(
                    "unterminated {prefix}-string literal (detected at line {start_line})"
                ));
            }
            if c == Some(quote) {
                end_quote_size += 1;
                continue;
            } else {
                end_quote_size = 0;
            }
            if c == Some('{') {
                let peek = self.nextc();
                if peek != Some('{') || in_format_spec {
                    self.backup(peek);
                    self.backup(c);
                    self.modes[index].curly_bracket_expr_start_depth += 1;
                    if self.modes[index].curly_bracket_expr_start_depth >= MAX_EXPR_NESTING {
                        return self.fstring_error("expressions nested too deeply".to_string());
                    }
                    self.modes[index].kind = ModeKind::Regular;
                    self.modes[index].in_format_spec = false;
                    let start = self.start.unwrap_or(self.pos);
                    let end = self.pos;
                    let kind = self.middle_kind(index);
                    self.push(kind, Some(start), Some(end));
                    return kind;
                } else {
                    let start = self.start.unwrap_or(self.pos);
                    let end = self.pos.saturating_sub(1);
                    let kind = self.middle_kind(index);
                    self.push(kind, Some(start), Some(end));
                    return kind;
                }
            } else if c == Some('}') {
                if unicode_escape {
                    let start = self.start.unwrap_or(self.pos);
                    let end = self.pos;
                    let kind = self.middle_kind(index);
                    self.push(kind, Some(start), Some(end));
                    return kind;
                }
                let peek = self.nextc();
                let cursor = self.modes[index].curly_bracket_depth;
                if peek == Some('}') && !in_format_spec && cursor == 0 {
                    let start = self.start.unwrap_or(self.pos);
                    let end = self.pos.saturating_sub(1);
                    let kind = self.middle_kind(index);
                    self.push(kind, Some(start), Some(end));
                    return kind;
                }
                self.backup(peek);
                self.backup(c);
                self.modes[index].kind = ModeKind::Regular;
                self.modes[index].in_format_spec = false;
                let start = self.start.unwrap_or(self.pos);
                let end = self.pos;
                let kind = self.middle_kind(index);
                self.push(kind, Some(start), Some(end));
                return kind;
            } else if c == Some('\\') {
                let mut peek = self.nextc();
                if peek == Some('\r') {
                    peek = self.nextc();
                }
                if peek == Some('{') || peek == Some('}') {
                    // CPython warns about the invalid escape; the warning does
                    // not affect the token stream.
                    self.backup(peek);
                    continue;
                }
                if !raw && peek == Some('N') {
                    let after = self.nextc();
                    if after == Some('{') {
                        unicode_escape = true;
                    } else {
                        self.backup(after);
                    }
                }
            }
        }
        let quote_size = self.modes[index].quote_size;
        let quote = self.modes[index].quote;
        for _ in 0..quote_size {
            self.backup(Some(quote));
        }
        let start = self.start.unwrap_or(self.pos);
        let end = self.pos;
        let kind = self.middle_kind(index);
        self.push(kind, Some(start), Some(end));
        kind
    }

    fn normal_mode(&mut self) -> i64 {
        let mut blankline = false;
        self.start = None;
        self.buf_pending = true;
        let mut c;

        if self.atbol {
            let mut col: i64 = 0;
            let mut altcol: i64 = 0;
            self.atbol = false;
            let mut cont_line_col: i64 = 0;
            loop {
                c = self.nextc();
                if self.error.is_some() {
                    return ERRORTOKEN;
                }
                match c {
                    Some(' ') => {
                        col += 1;
                        altcol += 1;
                    }
                    Some('\t') => {
                        col = (col / 8 + 1) * 8;
                        altcol += 1;
                    }
                    Some('\x0c') => {
                        col = 0;
                        altcol = 0;
                    }
                    Some('\\') => {
                        if cont_line_col == 0 {
                            cont_line_col = col;
                        }
                        if !self.continuation_line() {
                            return ERRORTOKEN;
                        }
                    }
                    _ => break,
                }
            }
            self.backup(c);
            if matches!(c, Some('#') | Some('\n') | Some('\r')) {
                blankline = true;
            }
            if !blankline && self.level == 0 {
                let col = if cont_line_col != 0 { cont_line_col } else { col };
                let altcol = if cont_line_col != 0 {
                    cont_line_col
                } else {
                    altcol
                };
                if col == self.indstack[self.indent] {
                    if altcol != self.altindstack[self.indent] {
                        self.set_error(
                            exc::TAB_ERROR,
                            "inconsistent use of tabs and spaces in indentation".to_string(),
                        );
                        return ERRORTOKEN;
                    }
                } else if col > self.indstack[self.indent] {
                    if self.indent + 1 >= MAXINDENT {
                        self.set_error(
                            exc::INDENTATION_ERROR,
                            "too many levels of indentation".to_string(),
                        );
                        return ERRORTOKEN;
                    }
                    if altcol <= self.altindstack[self.indent] {
                        self.set_error(
                            exc::TAB_ERROR,
                            "inconsistent use of tabs and spaces in indentation".to_string(),
                        );
                        return ERRORTOKEN;
                    }
                    self.pendin += 1;
                    self.indent += 1;
                    self.indstack[self.indent] = col;
                    self.altindstack[self.indent] = altcol;
                } else {
                    while self.indent > 0 && col < self.indstack[self.indent] {
                        self.pendin -= 1;
                        self.indent -= 1;
                    }
                    if col != self.indstack[self.indent] {
                        let stop = self.line_text_end();
                        let offset = (stop.saturating_sub(self.line_start)) as i64;
                        self.set_error(
                            exc::INDENTATION_ERROR,
                            "unindent does not match any outer indentation level".to_string(),
                        );
                        if let Some(error) = self.error.as_mut() {
                            error.offset = offset;
                        }
                        self.pos = self.line_end;
                        return ERRORTOKEN;
                    }
                    if altcol != self.altindstack[self.indent] {
                        self.set_error(
                            exc::TAB_ERROR,
                            "inconsistent use of tabs and spaces in indentation".to_string(),
                        );
                        return ERRORTOKEN;
                    }
                }
            }
        }

        self.start = Some(self.pos);
        if self.pendin != 0 {
            if self.pendin < 0 {
                self.pendin += 1;
                let p = self.pos;
                let (start, end) = if self.extra {
                    (Some(p), Some(p))
                } else {
                    (None, None)
                };
                self.push(DEDENT, start, end);
                return DEDENT;
            }
            self.pendin -= 1;
            let (start, end) = if self.extra {
                (Some(self.buf_start), Some(self.pos))
            } else {
                (None, None)
            };
            self.push(INDENT, start, end);
            return INDENT;
        }

        let peek = self.nextc();
        self.backup(peek);

        'again: loop {
            self.start = None;
            loop {
                c = self.nextc();
                if !matches!(c, Some(' ') | Some('\t') | Some('\x0c')) {
                    break;
                }
            }
            self.start = if c.is_some() { Some(self.pos - 1) } else { None };

            if c == Some('#') {
                while !matches!(c, None | Some('\n') | Some('\r')) {
                    c = self.nextc();
                    if self.error.is_some() {
                        return ERRORTOKEN;
                    }
                }
                if self.extra {
                    self.backup(c);
                    let start = self.start;
                    let end = Some(self.pos);
                    self.comment_newline = blankline;
                    self.push(COMMENT, start, end);
                    return COMMENT;
                }
            }

            if c.is_none() {
                if self.level > 0 {
                    self.syntax_error("unexpected EOF in multi-line statement".to_string());
                    return ERRORTOKEN;
                }
                if self.done == Done::Eof {
                    self.push(ENDMARKER, None, None);
                    return ENDMARKER;
                }
                return ERRORTOKEN;
            }

            if is_ident_start(c) {
                let mut saw_b = false;
                let mut saw_r = false;
                let mut saw_u = false;
                let mut saw_f = false;
                let mut saw_t = false;
                loop {
                    if !saw_b && matches!(c, Some('b') | Some('B')) {
                        saw_b = true;
                    } else if !saw_u && matches!(c, Some('u') | Some('U')) {
                        saw_u = true;
                    } else if !saw_r && matches!(c, Some('r') | Some('R')) {
                        saw_r = true;
                    } else if !saw_f && matches!(c, Some('f') | Some('F')) {
                        saw_f = true;
                    } else if !saw_t && matches!(c, Some('t') | Some('T')) {
                        saw_t = true;
                    } else {
                        break;
                    }
                    c = self.nextc();
                    if matches!(c, Some('"') | Some('\'')) {
                        let incompatible = (saw_u && (saw_b || saw_r || saw_f || saw_t))
                            || (saw_b && (saw_f || saw_t))
                            || (saw_f && saw_t);
                        if incompatible {
                            self.syntax_error("incompatible string prefixes".to_string());
                            return ERRORTOKEN;
                        }
                        let quote = c.unwrap_or('\'');
                        if saw_f || saw_t {
                            return self.start_fstring(quote);
                        }
                        return self.scan_string(quote);
                    }
                }
                let mut nonascii = false;
                while is_ident_char(c) {
                    if c.is_some_and(|ch| (ch as u32) >= 128) {
                        nonascii = true;
                    }
                    c = self.nextc();
                }
                self.backup(c);
                if nonascii {
                    let start = self.start.unwrap_or(0);
                    let stop = self.pos;
                    let name: String = self.src[start..stop].iter().collect();
                    if let Some(bad) =
                        name.chars().find(|ch| *ch != '_' && !ch.is_alphanumeric())
                    {
                        if is_printable(bad) {
                            self.syntax_error(format!(
                                "invalid character '{bad}' (U+{:04X})",
                                bad as u32
                            ));
                        } else {
                            self.syntax_error(format!(
                                "invalid non-printable character U+{:04X}",
                                bad as u32
                            ));
                        }
                        return ERRORTOKEN;
                    }
                }
                let start = self.start;
                let end = Some(self.pos);
                self.push(TOK_NAME, start, end);
                return TOK_NAME;
            }

            if c == Some('\r') {
                c = self.nextc();
            }

            if c == Some('\n') {
                self.atbol = true;
                if blankline || self.level > 0 {
                    if self.extra {
                        if self.comment_newline {
                            self.comment_newline = false;
                        }
                        let start = self.start;
                        let end = Some(self.pos);
                        self.push(NL, start, end);
                        return NL;
                    }
                    continue 'again;
                }
                if self.comment_newline && self.extra {
                    self.comment_newline = false;
                    let start = self.start;
                    let end = Some(self.pos);
                    self.push(NL, start, end);
                    return NL;
                }
                let start = self.start;
                let end = Some(self.pos.saturating_sub(1));
                self.cont_line = false;
                self.push(NEWLINE, start, end);
                return NEWLINE;
            }

            if c == Some('.') {
                c = self.nextc();
                if is_digit(c) {
                    if !self.scan_number(c, true) {
                        return ERRORTOKEN;
                    }
                    return TOK_NUMBER;
                }
                if c == Some('.') {
                    c = self.nextc();
                    if c == Some('.') {
                        let start = self.start;
                        let end = Some(self.pos);
                        self.push(ELLIPSIS, start, end);
                        return ELLIPSIS;
                    }
                    self.backup(c);
                    self.backup(Some('.'));
                } else {
                    self.backup(c);
                }
                let start = self.start;
                let end = Some(self.pos);
                self.push(DOT, start, end);
                return DOT;
            }

            if is_digit(c) {
                if !self.scan_number(c, false) {
                    return ERRORTOKEN;
                }
                return TOK_NUMBER;
            }

            if matches!(c, Some('\'') | Some('"')) {
                let quote = c.unwrap_or('\'');
                return self.scan_string(quote);
            }

            if c == Some('\\') {
                if !self.continuation_line() {
                    return ERRORTOKEN;
                }
                self.cont_line = true;
                continue 'again;
            }

            let ch = c.unwrap_or('\0');

            if matches!(ch, ':' | '}' | '!' | '{')
                && self.inside_fstring()
                && self.inside_fstring_expr()
            {
                let index = self.mode_index();
                let depth = self.modes[index].curly_bracket_depth;
                let cursor = depth - if ch != '{' { 1 } else { 0 };
                let _ = self.modes[index].in_format_spec;
                if ch == ':' && cursor == self.modes[index].curly_bracket_expr_start_depth {
                    self.modes[index].kind = ModeKind::FString;
                    self.modes[index].in_format_spec = true;
                    let start = self.start;
                    let end = Some(self.pos);
                    let kind = one_char(ch);
                    self.push(kind, start, end);
                    return kind;
                }
            }

            let c2 = self.nextc();
            let mut current = two_chars(ch, c2);
            if ch == '<' && c2 == Some('>') {
                current = OP;
            }
            if current != OP {
                let c3 = self.nextc();
                let current3 = three_chars(ch, c2, c3);
                if current3 != OP {
                    current = current3;
                } else {
                    self.backup(c3);
                }
                let start = self.start;
                let end = Some(self.pos);
                self.push(current, start, end);
                return current;
            }
            self.backup(c2);

            match ch {
                '(' | '[' | '{' => {
                    if self.level >= MAXLEVEL {
                        self.syntax_error("too many nested parentheses".to_string());
                        return ERRORTOKEN;
                    }
                    let line = self.lineno;
                    self.parenstack.push((ch, line));
                    self.level += 1;
                    if self.inside_fstring() {
                        let index = self.mode_index();
                        self.modes[index].curly_bracket_depth += 1;
                    }
                }
                ')' | ']' | '}' => {
                    if self.inside_fstring()
                        && self.current_mode().curly_bracket_depth == 0
                        && ch == '}'
                    {
                        let prefix = self.fstring_prefix();
                        return self.fstring_error(format!(
                            "{prefix}-string: single '}}' is not allowed"
                        ));
                    }
                    if !self.extra && self.level == 0 {
                        self.syntax_error(format!("unmatched '{ch}'"));
                        return ERRORTOKEN;
                    }
                    if self.level > 0 {
                        self.level -= 1;
                        let (opening, open_line) = self.parenstack.pop().unwrap_or(('(', 0));
                        let matched = (opening == '(' && ch == ')')
                            || (opening == '[' && ch == ']')
                            || (opening == '{' && ch == '}');
                        if !self.extra && !matched {
                            if open_line == self.lineno {
                                self.syntax_error(format!(
                                    "closing parenthesis '{ch}' does not match opening parenthesis '{opening}'"
                                ));
                            } else {
                                self.syntax_error(format!(
                                    "closing parenthesis '{ch}' does not match opening parenthesis '{opening}' on line {open_line}"
                                ));
                            }
                            return ERRORTOKEN;
                        }
                    }
                    if self.inside_fstring() {
                        let index = self.mode_index();
                        self.modes[index].curly_bracket_depth -= 1;
                        if self.modes[index].curly_bracket_depth < 0 {
                            let prefix = self.fstring_prefix();
                            return self.fstring_error(format!(
                                "{prefix}-string: unmatched '{ch}'"
                            ));
                        }
                        if ch == '}'
                            && self.modes[index].curly_bracket_depth
                                == self.modes[index].curly_bracket_expr_start_depth
                        {
                            self.modes[index].curly_bracket_expr_start_depth -= 1;
                            self.modes[index].kind = ModeKind::FString;
                            self.modes[index].in_format_spec = false;
                            self.modes[index].in_debug = false;
                        }
                    }
                }
                _ => {}
            }

            if !is_printable(ch) {
                self.syntax_error(format!(
                    "invalid non-printable character U+{:04X}",
                    ch as u32
                ));
                return ERRORTOKEN;
            }

            if ch == '=' && self.inside_fstring() && self.inside_fstring_expr_at_top() {
                let index = self.mode_index();
                self.modes[index].in_debug = true;
            }

            let start = self.start;
            let end = Some(self.pos);
            let kind = one_char(ch);
            self.push(kind, start, end);
            return kind;
        }
    }
}

/// `_PyToken_OneChar`.
fn one_char(c: char) -> i64 {
    match c {
        '(' => 7,
        ')' => 8,
        '[' => 9,
        ']' => 10,
        ':' => 11,
        ',' => 12,
        ';' => 13,
        '+' => 14,
        '-' => 15,
        '*' => 16,
        '/' => 17,
        '|' => 18,
        '&' => 19,
        '<' => 20,
        '>' => 21,
        '=' => 22,
        '.' => 23,
        '%' => 24,
        '{' => 25,
        '}' => 26,
        '~' => 31,
        '^' => 32,
        '@' => 49,
        '!' => 54,
        _ => OP,
    }
}

/// `_PyToken_TwoChars`.
fn two_chars(c1: char, c2: Option<char>) -> i64 {
    match (c1, c2) {
        ('!', Some('=')) => 28,
        ('%', Some('=')) => 40,
        ('&', Some('=')) => 41,
        ('*', Some('*')) => 35,
        ('*', Some('=')) => 38,
        ('+', Some('=')) => 36,
        ('-', Some('=')) => 37,
        ('-', Some('>')) => 51,
        ('/', Some('/')) => 47,
        ('/', Some('=')) => 39,
        (':', Some('=')) => 53,
        ('<', Some('<')) => 33,
        ('<', Some('=')) => 29,
        ('<', Some('>')) => 28,
        ('=', Some('=')) => 27,
        ('>', Some('=')) => 30,
        ('>', Some('>')) => 34,
        ('@', Some('=')) => 50,
        ('^', Some('=')) => 43,
        ('|', Some('=')) => 42,
        _ => OP,
    }
}

/// `_PyToken_ThreeChars`.
fn three_chars(c1: char, c2: Option<char>, c3: Option<char>) -> i64 {
    match (c1, c2, c3) {
        ('*', Some('*'), Some('=')) => 46,
        ('.', Some('.'), Some('.')) => 52,
        ('/', Some('/'), Some('=')) => 48,
        ('<', Some('<'), Some('=')) => 44,
        ('>', Some('>'), Some('=')) => 45,
        _ => OP,
    }
}

// -------------------------------------------------------------------------
// `_tokenize.TokenizerIter`
// -------------------------------------------------------------------------

const CLASS_KEY: &str = "_tokenize.TokenizerIter.class";
const TOKENS_ATTR: &str = "\u{0}tokens";
const INDEX_ATTR: &str = "\u{0}index";
const ERROR_ATTR: &str = "\u{0}error";
const SOURCE_ATTR: &str = "\u{0}source";
const EXTRA_ATTR: &str = "\u{0}extra";

fn tokenizer_class() -> i64 {
    if let Some(cached) = globals::registry_get(CLASS_KEY) {
        return cached;
    }
    let native =
        |name: &'static str, arity: usize| value::native_function_new("_tokenize", name, arity);
    let methods = vec![
        ("__next__".to_string(), native("_tokenizeriter_next", 1)),
        ("__iter__".to_string(), native("_tokenizeriter_iter", 1)),
        (
            "set_extra_tokens".to_string(),
            native("_tokenizeriter_set_extra_tokens", 2),
        ),
    ];
    let functions: Vec<i64> = methods.iter().map(|(_, function)| *function).collect();
    let roots = heap::root_values(&functions);
    let class = value::class_new("TokenizerIter".to_string(), NONE, methods);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(CLASS_KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn decode_utf16(bytes: &[u8]) -> String {
    let (little_endian, body) = if bytes.starts_with(&[0xFF, 0xFE]) {
        (true, &bytes[2..])
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (false, &bytes[2..])
    } else {
        (cfg!(target_endian = "little"), bytes)
    };
    let mut units: Vec<u16> = Vec::with_capacity(body.len() / 2);
    for pair in body.chunks_exact(2) {
        units.push(if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        });
    }
    let mut out = String::new();
    for unit in std::char::decode_utf16(units.into_iter()) {
        out.push(unit.unwrap_or('\u{fffd}'));
    }
    out
}

fn decode_bytes(bytes: &[u8], encoding: &str) -> Option<String> {
    let name = encoding.to_ascii_lowercase().replace('_', "-");
    if name == "utf-8-sig" {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return String::from_utf8(bytes[3..].to_vec()).ok();
        }
        return String::from_utf8(bytes.to_vec()).ok();
    }
    if name.starts_with("utf-8") || name == "utf8" {
        return String::from_utf8(bytes.to_vec()).ok();
    }
    if name.starts_with("latin-1")
        || name.starts_with("iso-8859-1")
        || name.starts_with("iso-latin-1")
    {
        return Some(decode_latin1(bytes));
    }
    if name.starts_with("utf-16") {
        return Some(decode_utf16(bytes));
    }
    String::from_utf8(bytes.to_vec()).ok()
}

/// Reads every line from the `readline` callable, decoding byte streams with
/// the requested encoding. An empty line or a raised `StopIteration` marks end
/// of input (like CPython's `readline`-driven underflow).
fn read_source(readline: i64, encoding: Option<&str>) -> Result<(String, bool), i64> {
    let mut source = String::new();
    let mut implicit_newline = false;
    loop {
        let line = builtins::call(readline, 0, std::ptr::null());
        if exc::pending() {
            if exc::pending_tag() == exc::STOP_ITERATION as i64 {
                exc::clear();
                break;
            }
            return Err(NONE);
        }
        let chunk = match encoding {
            Some(name) => match value::as_bytes(line) {
                Some(bytes) => match decode_bytes(bytes, name) {
                    Some(text) => text,
                    None => {
                        return Err(exc::raise_with_message(
                            exc::UNICODE_DECODE_ERROR,
                            "invalid or missing encoding declaration",
                        ))
                    }
                },
                None => {
                    return Err(exc::raise_with_message(
                        exc::TYPE_ERROR,
                        "readline() returned a non-bytes object",
                    ))
                }
            },
            None => match value::as_str(line) {
                Some(text) => text.to_string(),
                None => {
                    return Err(exc::raise_with_message(
                        exc::TYPE_ERROR,
                        "readline() returned a non-string object",
                    ))
                }
            },
        };
        if chunk.is_empty() {
            break;
        }
        source.push_str(&chunk);
        if chunk.ends_with('\n') {
            implicit_newline = false;
        } else {
            source.push('\n');
            implicit_newline = true;
        }
    }
    Ok((source, implicit_newline))
}

fn token_value(token: &Token) -> i64 {
    let int = |n: i64| value::encode_int(n).unwrap_or(NONE);
    value::tuple_new(vec![
        int(token.kind),
        value::str_alloc(token.text.clone()),
        value::tuple_new(vec![int(token.start.0), int(token.start.1)]),
        value::tuple_new(vec![int(token.end.0), int(token.end.1)]),
        value::str_alloc(token.line.clone()),
    ])
}

/// Runs the tokenizer over the whole source, keeping the tokens produced
/// before any lexical error.
fn run_lexer(source: &str, extra: bool, implicit_newline: bool) -> (Vec<Token>, Option<LexError>) {
    if source.is_empty() && !extra {
        return (Vec::new(), None);
    }
    let mut lexer = Lexer::new(source, extra, implicit_newline);
    loop {
        let kind = lexer.get_token();
        if lexer.error.is_some() {
            return (lexer.tokens, lexer.error);
        }
        if kind == ENDMARKER || kind == ERRORTOKEN {
            return (lexer.tokens, None);
        }
    }
}

fn error_value(error: &LexError) -> i64 {
    let int = |n: i64| value::encode_int(n).unwrap_or(NONE);
    let mut keywords = vec![
        ("lineno".to_string(), int(error.lineno)),
        ("offset".to_string(), int(error.offset)),
        ("text".to_string(), value::str_alloc(error.text.clone())),
    ];
    if let (Some(end_lineno), Some(end_offset)) = (error.end_lineno, error.end_offset) {
        keywords.push(("end_lineno".to_string(), int(end_lineno)));
        keywords.push(("end_offset".to_string(), int(end_offset)));
    }
    exc::exception_value_with_keywords(
        error.tag,
        &error.message,
        vec![error.message.clone()],
        keywords,
    )
}

fn store_tokens(
    instance: i64,
    source: &str,
    extra: bool,
    tokens: &[Token],
    error: Option<&LexError>,
) {
    let items: Vec<i64> = tokens.iter().map(token_value).collect();
    let list = value::list_new(items);
    let roots = heap::root_values(&[list]);
    let error_object = match error {
        Some(error) => error_value(error),
        None => NONE,
    };
    let error_roots = heap::root_values(&[error_object]);
    let index = value::encode_int(0).unwrap_or(NONE);
    value::instance_setattr(instance, TOKENS_ATTR.to_string(), list);
    value::instance_setattr(instance, INDEX_ATTR.to_string(), index);
    value::instance_setattr(instance, ERROR_ATTR.to_string(), error_object);
    value::instance_setattr(
        instance,
        SOURCE_ATTR.to_string(),
        value::str_alloc(source.to_string()),
    );
    value::instance_setattr(
        instance,
        EXTRA_ATTR.to_string(),
        value::bool_value(extra),
    );
    heap::release_roots(error_roots);
    heap::release_roots(roots);
}

fn tokenizer_iter_new(args: &[i64]) -> i64 {
    let readline = args.first().copied().unwrap_or(NONE);
    let encoding = args
        .get(1)
        .copied()
        .filter(|argument| *argument != NONE)
        .and_then(value::as_str)
        .map(|text| text.to_string());
    let extra = args
        .get(2)
        .copied()
        .is_some_and(|argument| argument != NONE && value::truthy(argument));
    let (source, implicit_newline) = match read_source(readline, encoding.as_deref()) {
        Ok(result) => result,
        Err(error) => return error,
    };
    let (tokens, error) = run_lexer(&source, extra, implicit_newline);
    let class = tokenizer_class();
    if exc::pending() {
        return NONE;
    }
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance]);
    store_tokens(instance, &source, extra, &tokens, error.as_ref());
    heap::release_roots(roots);
    instance
}

fn tokenizer_iter_next(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(exc::TYPE_ERROR, "missing self");
    };
    let tokens = value::instance_attr(instance, TOKENS_ATTR).unwrap_or(NONE);
    let index = value::instance_attr(instance, INDEX_ATTR)
        .and_then(value::as_integer)
        .unwrap_or(0);
    let items: &[i64] = value::as_list(tokens).unwrap_or(&[]);
    if index >= 0 && (index as usize) < items.len() {
        let item = items[index as usize];
        value::instance_setattr(
            instance,
            INDEX_ATTR.to_string(),
            value::encode_int(index + 1).unwrap_or(NONE),
        );
        return item;
    }
    let error = value::instance_attr(instance, ERROR_ATTR).unwrap_or(NONE);
    if error != NONE {
        value::instance_setattr(instance, ERROR_ATTR.to_string(), NONE);
        return exc::raise_value(error);
    }
    exc::raise_with_message(exc::STOP_ITERATION, "")
}

fn tokenizer_iter_iter(args: &[i64]) -> i64 {
    args.first().copied().unwrap_or(NONE)
}

fn tokenizer_iter_set_extra_tokens(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(exc::TYPE_ERROR, "missing self");
    };
    let new_extra = args.get(1).copied().is_some_and(value::truthy);
    let old_extra = value::instance_attr(instance, EXTRA_ATTR).is_some_and(value::truthy);
    if new_extra != old_extra {
        let source = value::instance_attr(instance, SOURCE_ATTR)
            .and_then(value::as_str)
            .unwrap_or("")
            .to_string();
        let implicit = source.ends_with('\n') && !source.is_empty();
        let (tokens, error) = run_lexer(&source, new_extra, implicit);
        store_tokens(instance, &source, new_extra, &tokens, error.as_ref());
    }
    NONE
}

/// Dispatches a native `_tokenize` entry point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "TokenizerIter" => tokenizer_iter_new(args),
        "_tokenizeriter_next" => tokenizer_iter_next(args),
        "_tokenizeriter_iter" => tokenizer_iter_iter(args),
        "_tokenizeriter_set_extra_tokens" => tokenizer_iter_set_extra_tokens(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_tokenize' has no attribute '{function}'"),
        ),
    }
}
