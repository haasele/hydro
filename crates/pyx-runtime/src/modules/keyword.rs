//! `keyword`: the Python keyword tables and membership tests.
//!
//! `kwlist`/`softkwlist` return fresh lists on every access, exactly like the
//! module attributes in CPython. `iskeyword`/`issoftkeyword` test against the
//! static tables (never the mutable lists) and behave like CPython's
//! `frozenset.__contains__`: any non-string value is simply absent, so it
//! returns `False` rather than raising.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{self, FALSE, NONE, TRUE};

/// `keyword.kwlist` for CPython 3.14, in grammar order.
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// `keyword.softkwlist` for CPython 3.14.
const SOFT_KEYWORDS: &[&str] = &["_", "case", "match", "type"];

/// `keyword.kwlist`: a fresh list of tagged strings on every access.
pub(crate) fn kwlist() -> i64 {
    names(KEYWORDS)
}

/// `keyword.softkwlist`: a fresh list of tagged strings on every access.
pub(crate) fn softkwlist() -> i64 {
    names(SOFT_KEYWORDS)
}

fn names(table: &[&str]) -> i64 {
    // Allocate the list first and keep it in a tagged local: appending through
    // the write barrier keeps every element reachable if the loop allocates
    // enough to trigger a collection.
    let list = value::list_new(Vec::with_capacity(table.len()));
    for &name in table {
        let item = value::str_alloc(name.to_string());
        container::list_append(list, item);
    }
    list
}

fn iskeyword(argument: i64) -> i64 {
    match value::as_str(argument) {
        Some(name) if KEYWORDS.contains(&name) => TRUE,
        _ => FALSE,
    }
}

fn issoftkeyword(argument: i64) -> i64 {
    match value::as_str(argument) {
        Some(name) if SOFT_KEYWORDS.contains(&name) => TRUE,
        _ => FALSE,
    }
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    let argument = args.first().copied().unwrap_or(NONE);
    match function {
        "iskeyword" => iskeyword(argument),
        "issoftkeyword" => issoftkeyword(argument),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'keyword' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: i64) -> Vec<&'static str> {
        value::as_list(list)
            .expect("list value")
            .iter()
            .map(|item| value::as_str(*item).expect("string element"))
            .collect()
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    #[test]
    fn lists_match_cpython() {
        exc::init();
        let keywords = strings(kwlist());
        assert_eq!(keywords.len(), 35);
        assert_eq!(keywords.first().copied(), Some("False"));
        assert_eq!(keywords.last().copied(), Some("yield"));
        assert_eq!(strings(softkwlist()), ["_", "case", "match", "type"]);
        assert!(!exc::pending());
    }

    #[test]
    fn lists_are_fresh_on_every_access() {
        exc::init();
        let first = kwlist();
        let second = kwlist();
        assert_ne!(first, second);
        assert_eq!(strings(first), strings(second));
        // Mutating one module-level list must not change the other copy.
        container::list_append(first, text("not_a_keyword"));
        assert_eq!(strings(second).len(), 35);
        assert_eq!(strings(kwlist()).len(), 35);
        assert!(!exc::pending());
    }

    #[test]
    fn membership_uses_the_static_tables() {
        exc::init();
        let list = kwlist();
        container::list_append(list, text("not_a_keyword"));
        assert_eq!(call("iskeyword", &[text("not_a_keyword")]), FALSE);
        assert_eq!(call("iskeyword", &[text("False")]), TRUE);
        assert_eq!(call("iskeyword", &[text("yield")]), TRUE);
        assert_eq!(call("iskeyword", &[text("match")]), FALSE);
        assert_eq!(call("issoftkeyword", &[text("match")]), TRUE);
        assert_eq!(call("issoftkeyword", &[text("False")]), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn non_string_values_are_not_keywords() {
        exc::init();
        let number = value::encode_int(42).expect("tagged int");
        let empty_list = value::list_new(Vec::new());
        assert_eq!(call("iskeyword", &[number]), FALSE);
        assert_eq!(call("iskeyword", &[empty_list]), FALSE);
        assert_eq!(call("issoftkeyword", &[NONE]), FALSE);
        assert!(!exc::pending());
    }
}
