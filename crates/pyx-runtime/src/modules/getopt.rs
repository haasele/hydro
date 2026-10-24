//! `getopt`: C-style command-line option parsing.
//!
//! The parser follows CPython's `Lib/getopt.py`, including GNU permutation
//! for `gnu_getopt`, the `+`/`-` prefixes on the short-option string, prefix
//! matching of long options, and the `=`/`=?` suffixes in the long-option
//! table. Errors are reported as `RuntimeError` (PyX has no `GetoptError`
//! class yet) with the message CPython would produce.

use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// How an option consumes a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    /// No argument (`-x`, `--flag`).
    None,
    /// One required argument (`-xARG`, `-x ARG`, `--name=ARG`).
    Required,
    /// An attached-only optional argument (`-xARG`, `--name[=ARG]`).
    Optional,
}

/// One parsed entry of the option list.
///
/// `Group` only appears in `gnu_getopt` calls whose short-option string starts
/// with `-`: CPython then preserves the relative order by emitting
/// `(None, [non-options...])` entries.
enum ParsedOption {
    Pair(String, String),
    Group(Vec<String>),
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

/// A list/tuple of `str`, copied so later allocations cannot invalidate it.
fn string_sequence(argument: i64, function: &str, name: &str) -> Result<Vec<String>, i64> {
    let Some(items) = value::as_list(argument).or_else(|| value::as_tuple(argument)) else {
        return Err(type_error(&format!(
            "{function}() argument '{name}' must be a list or tuple, not '{}'",
            value::type_name(argument)
        )));
    };
    let mut result = Vec::with_capacity(items.len());
    for &item in items {
        let Some(text) = value::as_str(item) else {
            return Err(type_error(&format!(
                "{function}() argument '{name}' must contain only str, not '{}'",
                value::type_name(item)
            )));
        };
        result.push(text.to_string());
    }
    Ok(result)
}

/// Optional `longopts`: a single string is accepted as a one-element list.
fn long_options(argument: i64, function: &str) -> Result<Vec<String>, i64> {
    if let Some(text) = value::as_str(argument) {
        return Ok(vec![text.to_string()]);
    }
    string_sequence(argument, function, "longopts")
}

/// `short_has_arg`: whether `-option` takes no, a required, or an optional
/// argument. `::` after the letter marks the optional form.
fn short_arg_kind(option: char, shortopts: &[char]) -> Result<ArgKind, i64> {
    for (at, &candidate) in shortopts.iter().enumerate() {
        if option != candidate || candidate == ':' {
            continue;
        }
        if shortopts.get(at + 1) != Some(&':') {
            return Ok(ArgKind::None);
        }
        return Ok(if shortopts.get(at + 2) == Some(&':') {
            ArgKind::Optional
        } else {
            ArgKind::Required
        });
    }
    Err(runtime_error(&format!("option -{option} not recognized")))
}

/// `long_has_args`: resolves a (possibly abbreviated) long option name.
fn long_arg_kind(option: &str, longopts: &[String]) -> Result<(ArgKind, String), i64> {
    let possibilities: Vec<&str> = longopts
        .iter()
        .filter(|candidate| candidate.starts_with(option))
        .map(String::as_str)
        .collect();
    if possibilities.is_empty() {
        return Err(runtime_error(&format!("option --{option} not recognized")));
    }
    if possibilities.contains(&option) {
        return Ok((ArgKind::None, option.to_string()));
    }
    let with_argument = format!("{option}=");
    if possibilities.contains(&with_argument.as_str()) {
        return Ok((ArgKind::Required, option.to_string()));
    }
    let with_optional = format!("{option}=?");
    if possibilities.contains(&with_optional.as_str()) {
        return Ok((ArgKind::Optional, option.to_string()));
    }
    if possibilities.len() > 1 {
        let joined = possibilities.join(", ");
        return Err(runtime_error(&format!(
            "option --{option} not a unique prefix; possible options: {joined}"
        )));
    }
    let unique = possibilities[0];
    if let Some(stem) = unique.strip_suffix("=?") {
        return Ok((ArgKind::Optional, stem.to_string()));
    }
    if let Some(stem) = unique.strip_suffix('=') {
        return Ok((ArgKind::Required, stem.to_string()));
    }
    Ok((ArgKind::None, unique.to_string()))
}

/// `do_long_options`: parses one `--name[=value]` token.
fn parse_long(
    options: &mut Vec<ParsedOption>,
    token: &str,
    longopts: &[String],
    rest: &mut &[String],
) -> Result<(), i64> {
    let (name, inline) = match token.split_once('=') {
        Some((name, value)) => (name, Some(value.to_string())),
        None => (token, None),
    };
    let (kind, full) = long_arg_kind(name, longopts)?;
    let value = match kind {
        ArgKind::Required => match inline {
            Some(value) => value,
            None => {
                let Some(first) = rest.first() else {
                    return Err(runtime_error(&format!("option --{full} requires argument")));
                };
                let value = first.clone();
                *rest = &rest[1..];
                value
            }
        },
        ArgKind::Optional => inline.unwrap_or_default(),
        ArgKind::None => {
            if inline.is_some() {
                return Err(runtime_error(&format!(
                    "option --{full} must not have an argument"
                )));
            }
            String::new()
        }
    };
    options.push(ParsedOption::Pair(format!("--{full}"), value));
    Ok(())
}

/// `do_short_options`: parses a cluster such as `-abc` or `-ovalue`.
fn parse_short(
    options: &mut Vec<ParsedOption>,
    token: &str,
    shortopts: &[char],
    rest: &mut &[String],
) -> Result<(), i64> {
    let chars: Vec<char> = token.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        let option = chars[at];
        at += 1;
        let kind = short_arg_kind(option, shortopts)?;
        let value = if kind == ArgKind::None {
            String::new()
        } else if at < chars.len() {
            // The rest of the cluster is the argument (`-ovalue`).
            let value: String = chars[at..].iter().collect();
            at = chars.len();
            value
        } else if kind == ArgKind::Optional {
            String::new()
        } else {
            let Some(first) = rest.first() else {
                return Err(runtime_error(&format!(
                    "option -{option} requires argument"
                )));
            };
            let value = first.clone();
            *rest = &rest[1..];
            value
        };
        options.push(ParsedOption::Pair(format!("-{option}"), value));
    }
    Ok(())
}

/// Plain `getopt`: stops at the first non-option argument.
fn parse_strict(
    args: &[String],
    shortopts: &[char],
    longopts: &[String],
) -> Result<(Vec<ParsedOption>, Vec<String>), i64> {
    let mut options = Vec::new();
    let mut rest = args;
    while let Some(first) = rest.first().cloned() {
        if !first.starts_with('-') || first == "-" {
            break;
        }
        if first == "--" {
            rest = &rest[1..];
            break;
        }
        if let Some(name) = first.strip_prefix("--") {
            // Consume the option token before a long option can take the
            // following argument; `rest` still starts at `first` here.
            let mut tail = &rest[1..];
            parse_long(&mut options, name, longopts, &mut tail)?;
            rest = tail;
        } else {
            let mut tail = &rest[1..];
            parse_short(&mut options, &first[1..], shortopts, &mut tail)?;
            rest = tail;
        }
    }
    Ok((options, rest.to_vec()))
}

/// Emits a `(None, [...])` marker for GNU in-order mode when non-option
/// arguments have accumulated since the previous option.
fn flush_group(
    options: &mut Vec<ParsedOption>,
    program_args: &mut Vec<String>,
    return_in_order: bool,
) {
    if return_in_order && !program_args.is_empty() {
        options.push(ParsedOption::Group(std::mem::take(program_args)));
    }
}

/// GNU `getopt`: options and non-options may be intermixed.
fn parse_gnu(
    args: &[String],
    shortopts: &[char],
    longopts: &[String],
    all_options_first: bool,
    return_in_order: bool,
) -> Result<(Vec<ParsedOption>, Vec<String>), i64> {
    let mut options = Vec::new();
    let mut program_args = Vec::new();
    let mut rest = args;
    while let Some(first) = rest.first().cloned() {
        if first == "--" {
            program_args.extend_from_slice(&rest[1..]);
            break;
        }
        if let Some(name) = first.strip_prefix("--") {
            flush_group(&mut options, &mut program_args, return_in_order);
            // Consume the option token before parsing it; a no-argument long
            // option would otherwise leave `rest` unchanged forever.
            let mut tail = &rest[1..];
            parse_long(&mut options, name, longopts, &mut tail)?;
            rest = tail;
        } else if first.starts_with('-') && first != "-" {
            flush_group(&mut options, &mut program_args, return_in_order);
            let mut tail = &rest[1..];
            parse_short(&mut options, &first[1..], shortopts, &mut tail)?;
            rest = tail;
        } else if all_options_first {
            program_args.extend_from_slice(rest);
            break;
        } else {
            program_args.push(first);
            rest = &rest[1..];
        }
    }
    Ok((options, program_args))
}

/// Allocates a list holding each string, appending before the next allocation
/// so the collector always sees a consistent list.
fn strings_list(items: &[String]) -> i64 {
    let list = value::list_new(Vec::new());
    for item in items {
        let text = value::str_alloc(item.clone());
        if let Some(mut values) = value::list_items_mut(list) {
            values.push(text);
            heap::write_barrier_edge(list, text);
        }
    }
    list
}

/// Builds `(options, remaining_args)`.
fn build_result(options: &[ParsedOption], rest: &[String]) -> i64 {
    let option_list = value::list_new(Vec::new());
    for option in options {
        let entry = match option {
            ParsedOption::Pair(name, value) => {
                let name = value::str_alloc(name.clone());
                let value = value::str_alloc(value.clone());
                value::tuple_new(vec![name, value])
            }
            ParsedOption::Group(group) => {
                let group_list = strings_list(group);
                value::tuple_new(vec![NONE, group_list])
            }
        };
        if let Some(mut entries) = value::list_items_mut(option_list) {
            entries.push(entry);
            heap::write_barrier_edge(option_list, entry);
        }
    }
    let remaining = strings_list(rest);
    value::tuple_new(vec![option_list, remaining])
}

/// Shared entry point for `getopt` and `gnu_getopt`.
fn run(function: &str, args: &[i64], gnu: bool) -> i64 {
    let (Some(&args_value), Some(&shortopts_value)) = (args.first(), args.get(1)) else {
        return type_error(&format!(
            "{function}() missing required argument 'args' or 'shortopts'"
        ));
    };
    let arguments = match string_sequence(args_value, function, "args") {
        Ok(arguments) => arguments,
        Err(error) => return error,
    };
    let Some(shortopts_text) = value::as_str(shortopts_value) else {
        return type_error(&format!(
            "{function}() argument 'shortopts' must be str, not '{}'",
            value::type_name(shortopts_value)
        ));
    };
    let longopts = match args.get(2).copied() {
        None | Some(NONE) => Vec::new(),
        Some(argument) => match long_options(argument, function) {
            Ok(longopts) => longopts,
            Err(error) => return error,
        },
    };
    let parsed = if gnu {
        let (text, all_options_first, return_in_order) =
            if let Some(rest) = shortopts_text.strip_prefix('-') {
                (rest, false, true)
            } else if let Some(rest) = shortopts_text.strip_prefix('+') {
                (rest, true, false)
            } else if std::env::var_os("POSIXLY_CORRECT").is_some_and(|value| !value.is_empty()) {
                (shortopts_text, true, false)
            } else {
                (shortopts_text, false, false)
            };
        let shortopts: Vec<char> = text.chars().collect();
        parse_gnu(
            &arguments,
            &shortopts,
            &longopts,
            all_options_first,
            return_in_order,
        )
    } else {
        let shortopts: Vec<char> = shortopts_text.chars().collect();
        parse_strict(&arguments, &shortopts, &longopts)
    };
    match parsed {
        Ok((options, rest)) => build_result(&options, &rest),
        Err(error) => error,
    }
}

/// Dispatches `getopt.function(...)` calls.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "getopt" => run(function, args, false),
        "gnu_getopt" => run(function, args, true),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'getopt' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(item: &str) -> i64 {
        value::str_alloc(item.to_string())
    }

    fn strings(items: &[&str]) -> i64 {
        value::list_new(items.iter().map(|item| text(item)).collect())
    }

    fn invoke(function: &str, args: &[&str], shortopts: &str, longopts: Option<&[&str]>) -> i64 {
        let mut argv = vec![strings(args), text(shortopts)];
        if let Some(longopts) = longopts {
            argv.push(strings(longopts));
        }
        call(function, &argv)
    }

    fn getopt(args: &[&str], shortopts: &str) -> i64 {
        invoke("getopt", args, shortopts, None)
    }

    fn gnu(args: &[&str], shortopts: &str, longopts: &[&str]) -> i64 {
        invoke("gnu_getopt", args, shortopts, Some(longopts))
    }

    fn pairs(result: i64) -> (Vec<(String, String)>, Vec<String>) {
        let outer = value::as_tuple(result).expect("result tuple");
        assert_eq!(outer.len(), 2);
        let options = value::as_list(outer[0]).expect("options list");
        let rest = value::as_list(outer[1]).expect("remaining list");
        let options = options
            .iter()
            .map(|&entry| {
                let pair = value::as_tuple(entry).expect("option pair");
                assert_eq!(pair.len(), 2);
                (
                    value::as_str(pair[0]).expect("option name").to_string(),
                    value::as_str(pair[1]).expect("option value").to_string(),
                )
            })
            .collect();
        let rest = rest
            .iter()
            .map(|&item| value::as_str(item).expect("argument").to_string())
            .collect();
        (options, rest)
    }

    fn check(result: i64, options: &[(&str, &str)], rest: &[&str]) {
        assert!(!exc::pending(), "unexpected exception");
        let (actual, remaining) = pairs(result);
        let expected: Vec<(String, String)> = options
            .iter()
            .map(|&(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(actual, expected);
        let expected: Vec<String> = rest.iter().map(|item| (*item).to_string()).collect();
        assert_eq!(remaining, expected);
    }

    fn error_message(result: i64) -> String {
        assert_eq!(result, NONE);
        assert!(exc::pending(), "expected an exception");
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);
        let message = value::to_string(exc::pending_value());
        exc::clear();
        message
    }

    #[test]
    fn short_options_require_arguments() {
        exc::init();
        check(getopt(&["-a", "x", "b"], "a:"), &[("-a", "x")], &["b"]);
        check(
            getopt(&["-ab", "v"], "ab:"),
            &[("-a", ""), ("-b", "v")],
            &[],
        );
        // A missing argument is reported as a RuntimeError.
        let message = error_message(getopt(&["-a"], "a:"));
        assert_eq!(message, "option -a requires argument");
        let message = error_message(getopt(&["-z"], "a:"));
        assert_eq!(message, "option -z not recognized");
    }

    #[test]
    fn optional_short_arguments_attach_only() {
        exc::init();
        check(getopt(&["-afoo"], "a::"), &[("-a", "foo")], &[]);
        check(getopt(&["-a"], "a::"), &[("-a", "")], &[]);
        // The next token is not consumed for an optional argument.
        check(getopt(&["-a", "foo"], "a::"), &[("-a", "")], &["foo"]);
        check(getopt(&["-a", ""], "a:"), &[("-a", "")], &[]);
    }

    #[test]
    fn long_options_and_prefixes() {
        exc::init();
        check(
            invoke("getopt", &["--foo=1"], "", Some(&["foo="])),
            &[("--foo", "1")],
            &[],
        );
        check(
            invoke("getopt", &["--foo", "x"], "", Some(&["foo="])),
            &[("--foo", "x")],
            &[],
        );
        check(
            invoke("getopt", &["--foo="], "", Some(&["foo="])),
            &[("--foo", "")],
            &[],
        );
        // Unique prefix resolves to the full option name.
        check(
            invoke("getopt", &["--foo", "x"], "", Some(&["foobar="])),
            &[("--foobar", "x")],
            &[],
        );
        check(
            invoke("getopt", &["--bar"], "", Some(&["bar=?", "baz"])),
            &[("--bar", "")],
            &[],
        );
        check(
            invoke("getopt", &["--flag"], "", Some(&["flag"])),
            &[("--flag", "")],
            &[],
        );
    }

    #[test]
    fn long_option_errors() {
        exc::init();
        let message = error_message(invoke("getopt", &["--zzz"], "", Some(&["foo"])));
        assert_eq!(message, "option --zzz not recognized");

        let message = error_message(invoke("getopt", &["--foo"], "", Some(&["foo="])));
        assert_eq!(message, "option --foo requires argument");

        let message = error_message(invoke("getopt", &["--flag=1"], "", Some(&["flag"])));
        assert_eq!(message, "option --flag must not have an argument");

        let message = error_message(invoke("getopt", &["--f"], "", Some(&["foo", "fob"])));
        assert_eq!(
            message,
            "option --f not a unique prefix; possible options: foo, fob"
        );
    }

    #[test]
    fn terminator_and_non_option_stops() {
        exc::init();
        check(getopt(&["-a", "--", "-b"], "a"), &[("-a", "")], &["-b"]);
        check(getopt(&["x", "-a"], "a"), &[], &["x", "-a"]);
        check(getopt(&["-", "-a"], "a"), &[], &["-", "-a"]);
        // Short clusters still split flags.
        check(getopt(&["-ab"], "ab"), &[("-a", ""), ("-b", "")], &[]);
    }

    #[test]
    fn gnu_permutation_and_in_order_groups() {
        exc::init();
        check(
            gnu(&["-a", "x", "y", "--b"], "a:", &["b"]),
            &[("-a", "x"), ("--b", "")],
            &["y"],
        );
        check(gnu(&["x", "-a", "y"], "a:", &[]), &[("-a", "y")], &["x"]);
        // `+` disables permutation: everything from the first operand on is
        // left untouched.
        check(
            gnu(&["-a", "x", "y", "--b"], "+a:", &["b"]),
            &[("-a", "x")],
            &["y", "--b"],
        );

        // `-` keeps options and operands in their original order.
        let result = gnu(&["-a", "x", "y", "--b"], "-a:", &["b"]);
        assert!(!exc::pending());
        let outer = value::as_tuple(result).expect("tuple");
        let entries = value::as_list(outer[0]).expect("options");
        assert_eq!(entries.len(), 3);
        let first = value::as_tuple(entries[0]).expect("pair");
        assert_eq!(value::as_str(first[0]), Some("-a"));
        assert_eq!(value::as_str(first[1]), Some("x"));
        let group = value::as_tuple(entries[1]).expect("group");
        assert_eq!(group[0], NONE);
        let items = value::as_list(group[1]).expect("group list");
        assert_eq!(value::as_str(items[0]), Some("y"));
        let last = value::as_tuple(entries[2]).expect("pair");
        assert_eq!(value::as_str(last[0]), Some("--b"));
        assert_eq!(value::as_str(last[1]), Some(""));
        assert_eq!(value::as_list(outer[1]).map(|items| items.len()), Some(0));
    }

    #[test]
    fn type_errors_do_not_panic() {
        exc::init();
        assert_eq!(call("getopt", &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call("getopt", &[text("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        // `args` must be a sequence of strings.
        assert_eq!(
            call("getopt", &[value::encode_int(1).expect("int"), text("a")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(
            call(
                "getopt",
                &[strings(&["-a"]), value::encode_int(1).expect("int")]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call("unknown", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
