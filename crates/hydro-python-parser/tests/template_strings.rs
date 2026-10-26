//! PEP 750 template-string tests for the forked parser.

use hydro_python_parser::ast::{self, Expr};
use hydro_python_parser::lexer::LexicalErrorType;
use hydro_python_parser::{
    parse_module_with_templates, ParseError, ParseErrorType, ParsedModule, TemplateErrorType,
    TemplateString,
};

fn parse_module(source: &str) -> ParsedModule {
    parse_module_with_templates(source, "<test>")
        .unwrap_or_else(|err| panic!("failed to parse {source:?}: {err}"))
}

fn parse_error(source: &str) -> ParseError {
    parse_module_with_templates(source, "<test>")
        .expect_err(&format!("expected a parse error for {source:?}"))
}

fn assigned_value(module: &ast::Mod) -> &Expr {
    let ast::Mod::Module(module) = module else {
        panic!("expected module");
    };
    let ast::Stmt::Assign(assign) = &module.body[0] else {
        panic!("expected an assignment, got {:?}", module.body[0]);
    };
    assign.value.as_ref()
}

fn only_template(parsed: &ParsedModule) -> &TemplateString {
    assert_eq!(parsed.templates.len(), 1, "{:?}", parsed.templates);
    &parsed.templates[0]
}

#[test]
fn parses_simple_template() {
    let parsed = parse_module("value = t\"hello {name}\"");
    let template = only_template(&parsed);

    assert_eq!(template.strings.len(), 2);
    assert_eq!(template.strings[0].value, "hello ");
    assert_eq!(template.strings[1].value, "");
    assert_eq!(template.interpolations.len(), 1);

    let interpolation = &template.interpolations[0];
    assert_eq!(interpolation.expression, "name");
    assert!(!interpolation.self_documenting);
    assert_eq!(interpolation.conversion, ast::ConversionFlag::None);
    assert!(interpolation.format_spec.is_none());
    assert!(matches!(
        interpolation.value.as_ref(),
        Expr::Name(name) if name.id.as_str() == "name"
    ));

    // The template literal in the AST is the documented placeholder.
    let placeholder = assigned_value(&parsed.module);
    assert_eq!(parsed.template_index_for_expr(placeholder), Some(0));
    assert_eq!(
        parsed.template_for_expr(placeholder).unwrap().range,
        template.range
    );
    assert!(hydro_python_parser::is_template_placeholder(
        match placeholder {
            Expr::Name(name) => name.id.as_str(),
            _ => panic!("expected placeholder name"),
        }
    ));
}

#[test]
fn preserves_source_spans() {
    let source = "value = t\"hello {name}!\"";
    let parsed = parse_module(source);
    let template = only_template(&parsed);

    assert_eq!(template.range.start().to_u32(), 8);
    assert_eq!(
        template.range.end().to_u32(),
        (8 + "t\"hello {name}!\"".len()) as u32
    );

    let interpolation = &template.interpolations[0];
    let brace = source.find('{').unwrap() as u32;
    assert_eq!(interpolation.range.start().to_u32(), brace);
    assert_eq!(
        interpolation.range.end().to_u32(),
        brace + "{name}".len() as u32
    );

    let name = source.find("name").unwrap() as u32;
    assert_eq!(interpolation.expression_range.start().to_u32(), name);
    assert_eq!(interpolation.expression_range.end().to_u32(), name + 4);
}

#[test]
fn conversion_and_format_spec() {
    let parsed = parse_module("value = t\"{value!r:>{width}}\"");
    let interpolation = &only_template(&parsed).interpolations[0];

    assert_eq!(interpolation.conversion, ast::ConversionFlag::Repr);

    let spec = interpolation
        .format_spec
        .as_ref()
        .expect("format spec should be present");
    let Expr::JoinedStr(joined) = spec.as_ref() else {
        panic!("expected JoinedStr format spec, got {spec:?}");
    };
    assert_eq!(joined.values.len(), 2);
    match &joined.values[0] {
        Expr::Constant(constant) => match &constant.value {
            ast::Constant::Str(text) => assert_eq!(text.as_str(), ">"),
            other => panic!("expected string constant, got {other:?}"),
        },
        other => panic!("expected constant, got {other:?}"),
    }
    assert!(matches!(&joined.values[1], Expr::FormattedValue(_)));
}

#[test]
fn static_conversion_and_format_spec() {
    let parsed = parse_module("value = t\"{value!s:.2f}\"");
    let interpolation = &only_template(&parsed).interpolations[0];

    assert_eq!(interpolation.conversion, ast::ConversionFlag::Str);
    let spec = interpolation
        .format_spec
        .as_ref()
        .expect("format spec should be present");
    let Expr::JoinedStr(joined) = spec.as_ref() else {
        panic!("expected JoinedStr format spec, got {spec:?}");
    };
    assert_eq!(joined.values.len(), 1);
    match &joined.values[0] {
        Expr::Constant(constant) => match &constant.value {
            ast::Constant::Str(text) => assert_eq!(text.as_str(), ".2f"),
            other => panic!("expected string constant, got {other:?}"),
        },
        other => panic!("expected constant, got {other:?}"),
    }
}

#[test]
fn interpolation_expression_preserves_source_text() {
    // CPython's `Interpolation.str` keeps the original source text, including
    // surrounding whitespace; it is not trimmed.
    let parsed = parse_module("value = t\"a{ name }b\"");
    let template = only_template(&parsed);

    assert_eq!(template.strings[0].value, "a");
    assert_eq!(template.strings[1].value, "b");
    assert_eq!(template.interpolations[0].expression, " name ");
}

#[test]
fn debug_specifier_rewrites_static_part() {
    let parsed = parse_module("value = t\"{value=}\"");
    let template = only_template(&parsed);
    let interpolation = &template.interpolations[0];

    assert_eq!(template.strings[0].value, "value=");
    assert_eq!(template.strings[1].value, "");
    assert!(interpolation.self_documenting);
    assert_eq!(interpolation.conversion, ast::ConversionFlag::Repr);
    assert_eq!(interpolation.expression, "value");
}

#[test]
fn debug_specifier_keeps_expression_whitespace() {
    let parsed = parse_module("value = t\"{value = }\"");
    let template = only_template(&parsed);
    let interpolation = &template.interpolations[0];

    assert_eq!(template.strings[0].value, "value = ");
    assert_eq!(interpolation.expression, "value ");
    assert_eq!(interpolation.conversion, ast::ConversionFlag::Repr);
}

#[test]
fn raw_templates_preserve_escapes() {
    let parsed = parse_module(r#"value = rt"{name}\n""#);
    let template = only_template(&parsed);
    assert!(template.raw);
    assert_eq!(template.strings[0].value, "");
    assert_eq!(template.strings[1].value, "\\n");
    assert_eq!(template.interpolations[0].expression, "name");
}

#[test]
fn implicit_concatenation_of_templates() {
    let parsed = parse_module("value = t\"a\" t\"{b}\"");
    let template = only_template(&parsed);

    assert_eq!(template.strings.len(), 2);
    assert_eq!(template.interpolations.len(), 1);
    assert_eq!(template.strings[0].value, "a");
    assert_eq!(template.strings[1].value, "");
    assert_eq!(template.interpolations[0].expression, "b");
}

#[test]
fn nested_template_strings_are_resolved() {
    let parsed = parse_module(r#"value = t"outer {t"inner {x}"}""#);
    assert_eq!(parsed.templates.len(), 2, "{:?}", parsed.templates);

    let outer = &parsed.templates[0];
    let inner = &parsed.templates[1];

    assert_eq!(outer.interpolations.len(), 1);
    assert_eq!(inner.interpolations.len(), 1);
    assert_eq!(inner.interpolations[0].expression, "x");
    assert_eq!(outer.interpolations[0].expression, r#"t"inner {x}""#);

    // The nested template appears as a placeholder inside the outer
    // interpolation expression and is resolvable through the same API.
    let resolved = parsed
        .template_for_expr(outer.interpolations[0].value.as_ref())
        .expect("nested template placeholder should resolve");
    assert_eq!(resolved.range, inner.range);
}

#[test]
fn nested_templates_inside_larger_expressions_are_resolved() {
    // The nested template is not the root of the interpolation expression, so
    // this exercises traversal of child expressions during resolution.
    let parsed = parse_module(r#"value = t"{wrap(t"inner {x}")}""#);
    assert_eq!(parsed.templates.len(), 2, "{:?}", parsed.templates);

    let outer = &parsed.templates[0];
    let inner = &parsed.templates[1];
    assert_eq!(inner.interpolations[0].expression, "x");
    assert_eq!(outer.interpolations[0].expression, r#"wrap(t"inner {x}")"#);

    let Expr::Call(call) = outer.interpolations[0].value.as_ref() else {
        panic!("expected a call expression");
    };
    assert_eq!(call.args.len(), 1);
    let resolved = parsed
        .template_for_expr(&call.args[0])
        .expect("nested template argument should resolve");
    assert_eq!(resolved.range, inner.range);
}

#[test]
fn malformed_templates_are_tagged_errors() {
    for source in [
        "value = t\"{name\"",
        "value = t\"hello {name\"",
        "value = t\"}\"",
        "value = t\"{}\"",
        "value = t\"{name!x}\"",
    ] {
        let error = parse_error(source);
        assert!(
            matches!(&error.error, ParseErrorType::Lexical(_)),
            "{source:?} produced {error:?}"
        );
    }
}

#[test]
fn mixed_concatenation_is_a_tagged_error() {
    for source in [
        "value = t\"a\" \"b\"",
        "value = \"a\" t\"b\"",
        "value = t\"a\" f\"b\"",
        "value = b\"a\" t\"b\"",
    ] {
        let error = parse_error(source);
        assert!(
            matches!(
                &error.error,
                ParseErrorType::Lexical(LexicalErrorType::TemplateError(
                    TemplateErrorType::MixedConcatenation
                ))
            ),
            "{source:?} produced {error:?}"
        );
    }
}
