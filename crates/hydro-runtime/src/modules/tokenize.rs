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

use crate::container;
use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
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

/// Dispatches a native `_tokenize` function.
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        "TokenizerIter" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "_tokenize.TokenizerIter() is not implemented: Hydro does not run the CPython \
             C tokenizer and has no tokenize.TokenInfo values",
        ),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_tokenize' has no attribute '{function}'"),
        ),
    }
}
