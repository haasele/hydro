//! `ast.literal_eval`: parsing Python literals from text.
//!
//! `rich.markup` parses `@handler(...)` parameters with `literal_eval`, so the
//! accepted grammar covers the literals that can appear there: integers,
//! floats, strings (single or double quoted, with the common escapes), `True`,
//! `False`, `None`, lists, tuples and dicts, plus a leading sign on a number.
//! Anything else raises `ValueError` (CPython raises `SyntaxError` for bad
//! syntax and `ValueError` for non-literals; both are caught by callers).
//!
//! The parser builds containers as it goes but recurses on nesting;
//! [`MAX_LITERAL_DEPTH`] bounds the recursion.

use crate::container;
use crate::exc::{self, TYPE_ERROR, VALUE_ERROR};
use crate::ops;
use crate::value::{self, FALSE, NONE, TRUE};

/// Maximum container nesting accepted by [`parse_literal`].
const MAX_LITERAL_DEPTH: usize = 64;

/// Dispatches a native `ast` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "literal_eval" => literal_eval(args[0]),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'ast' has no attribute '{function}'"),
        ),
    }
}

/// `ast.literal_eval(text)`: evaluates a Python literal.
fn literal_eval(value_arg: i64) -> i64 {
    let Some(source) = value::as_str(value_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "literal_eval() argument must be str, not {}",
                value::type_name(value_arg)
            ),
        );
    };
    // Copy before parsing: the parser allocates (strings, containers), and a
    // borrow into the source object would then span those allocations.
    let source = source.to_string();
    let mut parser = LiteralParser::new(&source);
    match parser.parse_document() {
        Ok(value) => value,
        Err(message) => {
            // `dict_set` reports an unhashable key itself; keep that pending
            // exception instead of replacing it with a generic parse error.
            if exc::pending() {
                return NONE;
            }
            exc::raise_with_message(VALUE_ERROR, &format!("malformed node or string: {message}"))
        }
    }
}

/// Recursive-descent parser over the characters of one literal expression.
struct LiteralParser {
    chars: Vec<char>,
    position: usize,
}

impl LiteralParser {
    fn new(source: &str) -> LiteralParser {
        LiteralParser {
            chars: source.chars().collect(),
            position: 0,
        }
    }

    /// Parses one value and rejects trailing non-whitespace input.
    fn parse_document(&mut self) -> Result<i64, String> {
        let parsed = self.parse_value(0)?;
        self.skip_whitespace();
        if self.position != self.chars.len() {
            return Err("unexpected trailing characters".to_string());
        }
        Ok(parsed)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += 1;
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += 1;
        }
    }

    /// Consumes `expected` after optional whitespace, reporting a parse error
    /// when it is absent.
    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_whitespace();
        if self.advance() == Some(expected) {
            Ok(())
        } else {
            Err(format!("expected '{expected}'"))
        }
    }

    /// Consumes `expected` when present (after optional whitespace).
    fn eat(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<i64, String> {
        if depth > MAX_LITERAL_DEPTH {
            return Err("literal is nested too deeply".to_string());
        }
        self.skip_whitespace();
        match self.peek() {
            None => Err("unexpected end of input".to_string()),
            Some('[') => self.parse_list(depth),
            Some('(') => self.parse_tuple(depth),
            Some('{') => self.parse_dict(depth),
            Some('"' | '\'') => self.parse_string().map(value::str_alloc),
            Some('+' | '-') => self.parse_signed_value(depth),
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.parse_number(),
            Some(ch) if ch.is_alphabetic() || ch == '_' => self.parse_name(),
            Some(other) => Err(format!("unexpected character '{other}'")),
        }
    }

    /// Unary `+`/`-`: only numeric literals may carry a sign in
    /// `literal_eval`.
    fn parse_signed_value(&mut self, depth: usize) -> Result<i64, String> {
        let negative = self.advance() == Some('-');
        self.skip_whitespace();
        let parsed = self.parse_value(depth + 1)?;
        if let Some(integer) = value::as_integer(parsed) {
            let signed = if negative {
                integer.checked_neg().ok_or("integer literal too large")?
            } else {
                integer
            };
            return Ok(ops::int_result(signed));
        }
        if let Some(float) = value::as_float(parsed) {
            return Ok(value::float_new(if negative { -float } else { float }));
        }
        Err("a sign must precede a number".to_string())
    }

    fn parse_name(&mut self) -> Result<i64, String> {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            self.position += 1;
        }
        let name: String = self.chars[start..self.position].iter().collect();
        match name.as_str() {
            "True" => Ok(TRUE),
            "False" => Ok(FALSE),
            "None" => Ok(NONE),
            _ => Err(format!("unknown name '{name}'")),
        }
    }

    fn parse_number(&mut self) -> Result<i64, String> {
        let start = self.position;
        let mut previous = '\0';
        while let Some(ch) = self.peek() {
            let numeric = ch.is_ascii_digit() || ch == '.';
            let exponent_marker = matches!(ch, 'e' | 'E');
            let exponent_sign = matches!(ch, '+' | '-') && matches!(previous, 'e' | 'E');
            if !(numeric || exponent_marker || exponent_sign) {
                break;
            }
            previous = ch;
            self.position += 1;
        }
        if start == self.position {
            return Err("expected a number".to_string());
        }
        let text: String = self.chars[start..self.position].iter().collect();
        if text.contains('.') || text.contains('e') || text.contains('E') {
            let parsed: f64 = text
                .parse()
                .map_err(|_| format!("invalid float '{text}'"))?;
            return Ok(value::float_new(parsed));
        }
        let parsed: i64 = text
            .parse()
            .map_err(|_| format!("invalid integer '{text}'"))?;
        Ok(ops::int_result(parsed))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self.advance().ok_or("unterminated string")?;
        let mut out = String::new();
        loop {
            let ch = self.advance().ok_or("unterminated string")?;
            if ch == quote {
                return Ok(out);
            }
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            let escape = self.advance().ok_or("unterminated escape")?;
            match escape {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                'x' => out.push(self.parse_hex_escape(2)?),
                'u' => out.push(self.parse_hex_escape(4)?),
                // Python preserves unknown escapes verbatim.
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
    }

    /// Reads `digits` hexadecimal digits and returns the scalar they encode.
    fn parse_hex_escape(&mut self, digits: usize) -> Result<char, String> {
        let mut code = 0u32;
        for _ in 0..digits {
            let ch = self.advance().ok_or("truncated escape")?;
            let digit = ch.to_digit(16).ok_or("invalid hexadecimal escape")?;
            code = code * 16 + digit;
        }
        char::from_u32(code).ok_or_else(|| "invalid Unicode escape".to_string())
    }

    fn parse_list(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('[')?;
        let result = value::list_new(Vec::new());
        if self.eat(']') {
            return Ok(result);
        }
        loop {
            let item = self.parse_value(depth + 1)?;
            push_item(result, item);
            if self.eat(',') {
                if self.eat(']') {
                    return Ok(result);
                }
                continue;
            }
            self.expect(']')?;
            return Ok(result);
        }
    }

    fn parse_tuple(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('(')?;
        if self.eat(')') {
            return Ok(value::tuple_new(Vec::new()));
        }
        // Elements are accumulated in a rooted list; `(value)` without a
        // trailing comma is plain grouping.
        let result = value::list_new(Vec::new());
        let mut had_comma = false;
        loop {
            let item = self.parse_value(depth + 1)?;
            push_item(result, item);
            if self.eat(',') {
                had_comma = true;
                if self.eat(')') {
                    break;
                }
                continue;
            }
            self.expect(')')?;
            break;
        }
        let items = value::as_list(result)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        if items.len() == 1 && !had_comma {
            return Ok(items[0]);
        }
        Ok(value::tuple_new(items))
    }

    fn parse_dict(&mut self, depth: usize) -> Result<i64, String> {
        self.expect('{')?;
        let result = value::dict_new(Vec::new());
        if self.eat('}') {
            return Ok(result);
        }
        loop {
            let key = self.parse_value(depth + 1)?;
            self.expect(':')?;
            let item = self.parse_value(depth + 1)?;
            container::dict_set(result, key, item);
            if exc::pending() {
                return Err("unhashable dict key".to_string());
            }
            if self.eat(',') {
                if self.eat('}') {
                    return Ok(result);
                }
                continue;
            }
            self.expect('}')?;
            return Ok(result);
        }
    }
}

/// Appends `item` to `list`, recording the GC edge.
fn push_item(list: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(item);
        crate::heap::write_barrier_edge(list, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_dict, as_list, as_tuple, encode_int};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn evaluate(source: &str) -> i64 {
        call("literal_eval", &[text(source)])
    }

    #[test]
    fn scalars_are_parsed() {
        exc::init();
        assert_eq!(value::as_integer(evaluate("42")), Some(42));
        assert_eq!(value::as_integer(evaluate("-7")), Some(-7));
        assert_eq!(value::as_integer(evaluate("+3")), Some(3));
        assert_eq!(value::as_float(evaluate("2.5")), Some(2.5));
        assert_eq!(value::as_float(evaluate("-1e2")), Some(-100.0));
        assert_eq!(value::as_str(evaluate("'red'")), Some("red"));
        assert_eq!(value::as_str(evaluate("\"a\\nb\"")), Some("a\nb"));
        assert_eq!(evaluate("True"), TRUE);
        assert_eq!(evaluate("False"), FALSE);
        assert_eq!(evaluate("None"), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn containers_are_parsed() {
        exc::init();
        let list = evaluate("[1, 2, 3]");
        let items = as_list(list).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(value::as_integer(items[2]), Some(3));

        let tuple = evaluate("('a', 1)");
        let items = as_tuple(tuple).expect("tuple");
        assert_eq!(value::as_str(items[0]), Some("a"));
        assert_eq!(value::as_integer(items[1]), Some(1));

        assert_eq!(as_tuple(evaluate("()")).map(|items| items.len()), Some(0));
        assert_eq!(value::as_integer(evaluate("(5)")), Some(5));
        assert_eq!(as_tuple(evaluate("(5,)")).map(|items| items.len()), Some(1));

        let dict = evaluate("{'k': [1, 2]}");
        let pairs = as_dict(dict).expect("dict");
        assert_eq!(pairs.len(), 1);
        assert_eq!(value::as_str(pairs[0].0), Some("k"));
        assert_eq!(as_list(pairs[0].1).map(|items| items.len()), Some(2));
        assert!(!exc::pending());
    }

    #[test]
    fn malformed_input_raises_value_error() {
        exc::init();
        assert_eq!(evaluate("1 + 2"), NONE);
        assert!(exc::pending());
        exc::clear();
        assert_eq!(evaluate("1 2"), NONE);
        assert!(exc::pending());
        exc::clear();
        assert_eq!(evaluate("{'k' 1}"), NONE);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn non_string_argument_raises_type_error() {
        exc::init();
        let number = encode_int(1).expect("small int");
        call("literal_eval", &[number]);
        assert!(exc::pending());
        exc::clear();
    }
}
