//! PEP 701 (CPython 3.12+) f-string syntax tests for the forked parser.

use hydro_python_parser::ast::{self, Expr};
use hydro_python_parser::Parse;

fn parse_expr(source: &str) -> Expr {
    Expr::parse(source, "<test>").unwrap_or_else(|err| panic!("failed to parse {source:?}: {err}"))
}

fn single_formatted_value(expr: &Expr) -> &ast::ExprFormattedValue {
    let Expr::JoinedStr(joined) = expr else {
        panic!("expected a JoinedStr, got {expr:?}");
    };
    assert_eq!(joined.values.len(), 1, "expected one value: {joined:?}");
    let Expr::FormattedValue(value) = &joined.values[0] else {
        panic!("expected a FormattedValue, got {:?}", joined.values[0]);
    };
    value
}

#[test]
fn same_quote_reuse_in_fstring_expressions() {
    // The classic pre-PEP 701 rejection: the inner string uses the same quote
    // as the enclosing f-string.
    for source in [r#"f"{x['key']}""#, r#"f"{x["key"]}""#] {
        let expr = parse_expr(source);
        let value = single_formatted_value(&expr);
        let Expr::Subscript(subscript) = value.value.as_ref() else {
            panic!("expected subscript in {source:?}: {value:?}");
        };
        let Expr::Name(name) = subscript.value.as_ref() else {
            panic!("expected name in {source:?}: {subscript:?}");
        };
        assert_eq!(name.id.as_str(), "x");
    }
}

#[test]
fn nested_fstrings_with_reused_quotes() {
    let expr = parse_expr(r#"f"{f"{x}"}""#);
    let outer = single_formatted_value(&expr);
    let Expr::JoinedStr(inner) = outer.value.as_ref() else {
        panic!("expected nested f-string, got {outer:?}");
    };
    assert_eq!(inner.values.len(), 1);
    assert!(matches!(
        &inner.values[0],
        Expr::FormattedValue(value)
            if matches!(value.value.as_ref(), Expr::Name(name) if name.id.as_str() == "x")
    ));
}

#[test]
fn nested_fstring_with_indexing_and_quote_reuse() {
    let expr = parse_expr(r#"f"outer {f"inner {x["key"]}"} tail""#);
    let Expr::JoinedStr(outer) = &expr else {
        panic!("expected JoinedStr, got {expr:?}");
    };
    let outer_value = outer
        .values
        .iter()
        .find_map(|value| match value {
            Expr::FormattedValue(value) => Some(value),
            _ => None,
        })
        .expect("outer interpolation");
    let Expr::JoinedStr(inner) = outer_value.value.as_ref() else {
        panic!("expected nested f-string, got {outer_value:?}");
    };
    let inner_value = inner
        .values
        .iter()
        .find_map(|value| match value {
            Expr::FormattedValue(value) => Some(value),
            _ => None,
        })
        .expect("inner interpolation");
    assert!(matches!(inner_value.value.as_ref(), Expr::Subscript(_)));
}

#[test]
fn backslashes_in_replacement_fields() {
    // PEP 701 allows backslashes in the expression part.
    let expr = parse_expr(r#"f"{'\n' + x}""#);
    let value = single_formatted_value(&expr);
    let Expr::BinOp(_) = value.value.as_ref() else {
        panic!("expected BinOp, got {value:?}");
    };
}

#[test]
fn comments_in_triple_quoted_replacement_fields() {
    let expr = parse_expr("f\"\"\"{x  # a comment\n}\"\"\"");
    let value = single_formatted_value(&expr);
    assert!(matches!(value.value.as_ref(), Expr::Name(name) if name.id.as_str() == "x"));
}

#[test]
fn format_spec_with_nested_expression_and_reused_quotes() {
    let expr = parse_expr(r#"f"{x:{"^10"}}""#);
    let value = single_formatted_value(&expr);
    let spec = value.format_spec.as_ref().expect("expected a format spec");
    let Expr::JoinedStr(joined) = spec.as_ref() else {
        panic!("expected JoinedStr format spec, got {spec:?}");
    };
    assert_eq!(joined.values.len(), 1);
    assert!(matches!(&joined.values[0], Expr::FormattedValue(_)));
}
