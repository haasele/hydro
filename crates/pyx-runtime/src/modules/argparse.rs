//! `argparse`: a small subset of the argument parser.
//!
//! Only the common shape is supported: one `ArgumentParser`, positional
//! arguments, long/short options, `type`, `default`, `required`, `choices`,
//! `help`, `store_true` and `nargs="?"`. `parse_args()` without an explicit
//! argument reads `sys.argv[1:]` and exits with status 0 on `-h`/`--help`
//! (as a real program must). An explicit argv list never exits: `-h`/`--help`
//! prints the help text and returns a namespace carrying `_help=True`.
//! Unknown or missing arguments raise `RuntimeError`, malformed option
//! definitions raise `TypeError`/`ValueError`.
//!
//! `ArgumentParser(formatter_class=...)` is accepted and ignored; the help
//! renderer is fixed (`RawTextHelpFormatter` is provided as `None`).

use std::io::Write;

use crate::builtins;
use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::methods::KeywordArgs;
use crate::value::{self, NONE};

/// Definition of one `add_argument` call.
#[derive(Clone)]
pub(crate) struct ArgSpec {
    /// Source spellings: `["-f", "--flag"]` or `["input"]`.
    pub(crate) flags: Vec<String>,
    pub(crate) dest: String,
    pub(crate) positional: bool,
    /// Conversion function value, or `None`.
    pub(crate) type_fn: i64,
    pub(crate) default: i64,
    pub(crate) store_true: bool,
    pub(crate) required: bool,
    pub(crate) help: String,
    pub(crate) nargs_optional: bool,
    pub(crate) nargs_plus: bool,
    /// Allowed converted values, in declaration order.
    pub(crate) choices: Option<Vec<i64>>,
}

/// State of an `argparse.ArgumentParser`.
pub(crate) struct ArgParserState {
    pub(crate) prog: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) options: Vec<ArgSpec>,
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'ArgumentParser' object has no attribute '{name}'"),
    )
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "ArgumentParser" => new_parser(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'argparse' has no attribute '{function}'"),
        ),
    }
}

/// `ArgumentParser(prog=None, description=None, formatter_class=None)`.
///
/// `formatter_class` is accepted and ignored: the help renderer is fixed and
/// `RawTextHelpFormatter` is exposed as `None`.
fn new_parser(args: &[i64]) -> i64 {
    let prog = match args.first().copied().unwrap_or(NONE) {
        NONE => None,
        value => match value::as_str(value) {
            Some(text) => Some(text.to_string()),
            None => return type_error("ArgumentParser() prog must be str or None"),
        },
    };
    let description = match args.get(1).copied().unwrap_or(NONE) {
        NONE => None,
        value => match value::as_str(value) {
            Some(text) => Some(text.to_string()),
            None => return type_error("ArgumentParser() description must be str or None"),
        },
    };
    value::arg_parser_new(prog, description)
}

/// Method dispatch without keywords.
pub(crate) fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    if value::as_arg_parser(obj).is_none() {
        return attribute_error(name);
    }
    match name {
        "add_argument" => add_argument(obj, args, &[]),
        "parse_args" => parse_args(obj, args),
        "print_help" => print_help(obj),
        _ => attribute_error(name),
    }
}

/// Method dispatch with keywords; only `add_argument` takes keywords.
pub(crate) fn method_kw(obj: i64, name: &str, args: &mut [i64], keywords: &KeywordArgs) -> i64 {
    if value::as_arg_parser(obj).is_none() {
        return attribute_error(name);
    }
    match name {
        "add_argument" => add_argument(obj, args, &keywords.items),
        _ => {
            if let Some((keyword, _)) = keywords.items.first() {
                return type_error(&format!(
                    "ArgumentParser.{name}() got an unexpected keyword argument '{keyword}'"
                ));
            }
            method(obj, name, args)
        }
    }
}

/// Separator between an option and an inline value (`--flag=value`).
fn split_inline(token: &str) -> Option<(&str, &str)> {
    token.split_once('=')
}

/// Normalises flags to a dest name: long option first, leading dashes
/// stripped, `-` replaced by `_`.
fn dest_for(flags: &[String]) -> String {
    let source = flags
        .iter()
        .find(|flag| flag.starts_with("--"))
        .or_else(|| flags.first())
        .map(String::as_str)
        .unwrap_or("");
    source.trim_start_matches('-').replace('-', "_")
}

fn add_argument(parser: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    // Split positional names from keyword bindings.
    let mut flags: Vec<String> = Vec::with_capacity(args.len());
    for &arg in args {
        let Some(name) = value::as_str(arg) else {
            return type_error(&format!(
                "add_argument() argument names must be str, not {}",
                value::type_name(arg)
            ));
        };
        if name.is_empty() {
            return value_error("argument names must not be empty");
        }
        flags.push(name.to_string());
    }
    if flags.is_empty() {
        return type_error("add_argument() requires at least one argument name");
    }

    let mut seen_keywords = std::collections::HashSet::new();
    let mut type_fn = NONE;
    let mut default = NONE;
    let mut required = false;
    let mut action: Option<String> = None;
    let mut help = String::new();
    let mut dest: Option<String> = None;
    let mut nargs_optional = false;
    let mut nargs_plus = false;
    let mut choices: Option<Vec<i64>> = None;
    for (name, argument) in keywords {
        if !seen_keywords.insert(name.as_str()) {
            return type_error(&format!(
                "add_argument() got multiple values for argument '{name}'"
            ));
        }
        match name.as_str() {
            "type" => {
                if *argument != NONE && value::as_function(*argument).is_none() {
                    return type_error("add_argument() type must be callable or None");
                }
                type_fn = *argument;
            }
            "default" => default = *argument,
            "required" => required = value::truthy(*argument),
            "action" => match value::as_str(*argument) {
                Some(text) if text == "store_true" => action = Some(text.to_string()),
                Some(text) => {
                    return value_error(&format!("unsupported action '{text}'"));
                }
                None if *argument == NONE => action = None,
                None => return type_error("add_argument() action must be str or None"),
            },
            "help" => match value::as_str(*argument) {
                Some(text) => help = text.to_string(),
                None => return type_error("add_argument() help must be str"),
            },
            "dest" => match value::as_str(*argument) {
                Some(text) => dest = Some(text.to_string()),
                None => return type_error("add_argument() dest must be str"),
            },
            "nargs" => match value::as_str(*argument) {
                Some("?") => nargs_optional = true,
                Some("+") => nargs_plus = true,
                Some(text) => {
                    return value_error(&format!("unsupported nargs '{text}'"));
                }
                None if *argument == NONE => {
                    nargs_optional = false;
                    nargs_plus = false;
                }
                None => return type_error("add_argument() nargs must be str or None"),
            },
            "choices" => {
                if *argument == NONE {
                    choices = None;
                } else if let Some(items) = value::as_list(*argument) {
                    choices = Some(items.to_vec());
                } else if let Some(items) = value::as_tuple(*argument) {
                    choices = Some(items.to_vec());
                } else {
                    return type_error("add_argument() choices must be a list or tuple");
                }
            }
            _ => {
                return type_error(&format!(
                    "add_argument() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }

    let store_true = action.as_deref() == Some("store_true");
    let positional = !flags.iter().any(|flag| flag.starts_with('-'));
    if !positional && flags.iter().any(|flag| !flag.starts_with('-')) {
        return type_error("add_argument() cannot mix positional and optional names");
    }
    if positional && flags.len() > 1 {
        return type_error("add_argument() accepts one positional name");
    }
    if store_true && positional {
        return type_error("store_true is only supported for optional arguments");
    }

    let dest = match dest {
        Some(dest) => dest,
        None if positional => flags[0].clone(),
        None => dest_for(&flags),
    };
    if dest.is_empty() {
        return value_error("could not derive a destination name");
    }

    {
        let Some(parser_state) = value::as_arg_parser(parser) else {
            return attribute_error("add_argument");
        };
        for spec in &parser_state.options {
            for flag in &flags {
                if spec.flags.iter().any(|existing| existing == flag) {
                    return type_error(&format!("argument '{flag}' is defined twice"));
                }
            }
            if spec.dest == dest && !positional && !spec.positional {
                return type_error(&format!("argument destination '{dest}' is defined twice"));
            }
        }
    }

    let choice_edges = choices.clone();
    let spec = ArgSpec {
        flags,
        dest,
        positional,
        type_fn,
        default: if store_true && default == NONE {
            value::FALSE
        } else {
            default
        },
        store_true,
        required,
        help,
        nargs_optional,
        nargs_plus,
        choices,
    };
    if let Some(mut parser_state) = value::arg_parser_mut(parser) {
        parser_state.options.push(spec);
        // The new spec introduces edges from the parser to its conversion
        // function and choices.
        if type_fn != NONE {
            crate::heap::write_barrier_edge(parser, type_fn);
        }
        if let Some(items) = choice_edges {
            for item in items {
                crate::heap::write_barrier_edge(parser, item);
            }
        }
        if default != NONE {
            crate::heap::write_barrier_edge(parser, default);
        }
    }
    NONE
}

fn argv_items(argv: i64) -> Result<Vec<i64>, i64> {
    if argv == NONE {
        return Ok(Vec::new());
    }
    if let Some(items) = value::as_list(argv) {
        return Ok(items.to_vec());
    }
    if let Some(items) = value::as_tuple(argv) {
        return Ok(items.to_vec());
    }
    Err(type_error(&format!(
        "parse_args() argument must be a list or tuple, not {}",
        value::type_name(argv)
    )))
}

fn print_help(parser: i64) -> i64 {
    let Some(state) = value::as_arg_parser(parser) else {
        return attribute_error("print_help");
    };
    let prog = state.prog.clone().unwrap_or_else(|| "program".to_string());
    let description = state.description.clone();
    let specs = state.options.clone();

    let mut usage = format!("usage: {prog} [-h]");
    for spec in &specs {
        if spec.positional {
            usage.push(' ');
            usage.push_str(&if spec.nargs_optional {
                format!("[{}]", spec.dest)
            } else {
                spec.dest.clone()
            });
        } else {
            usage.push(' ');
            usage.push('[');
            usage.push_str(&spec.flags.join(" "));
            if !spec.store_true {
                usage.push(' ');
                usage.push_str(&spec.dest.to_uppercase());
            }
            usage.push(']');
        }
    }

    let mut text = format!("{usage}\n");
    if let Some(description) = description {
        text.push('\n');
        text.push_str(&description);
        text.push('\n');
    }
    text.push_str("\noptions:\n");
    text.push_str("  -h, --help  show this help message and exit\n");
    for spec in &specs {
        if spec.positional {
            continue;
        }
        let mut label = spec.flags.join(", ");
        if !spec.store_true {
            label.push(' ');
            label.push_str(&spec.dest.to_uppercase());
        }
        text.push_str(&format!("  {label}"));
        if !spec.help.is_empty() {
            text.push_str("  ");
            text.push_str(&spec.help);
        }
        text.push('\n');
    }
    if specs.iter().any(|spec| spec.positional) {
        text.push_str("\npositional arguments:\n");
        for spec in specs.iter().filter(|spec| spec.positional) {
            text.push_str(&format!("  {}", spec.dest));
            if !spec.help.is_empty() {
                text.push_str("  ");
                text.push_str(&spec.help);
            }
            text.push('\n');
        }
    }

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
    NONE
}

fn parse_args(parser: i64, args: &[i64]) -> i64 {
    if args.len() > 1 {
        return type_error(&format!(
            "ArgumentParser.parse_args() takes at most one argument ({} given)",
            args.len()
        ));
    }
    // Parsed values and default-argv tokens stay reachable from this list so
    // a collection triggered by a later allocation cannot free them.
    let roots = value::list_new(Vec::new());
    // No explicit argv: read `sys.argv[1:]` like CPython. In that mode
    // `-h`/`--help` terminates the program; an explicit list keeps the
    // non-exiting `_help` namespace used by tests and embedded callers.
    let default_argv = args.is_empty();
    let argv = if default_argv {
        let mut tokens = Vec::new();
        for argument in crate::argv().into_iter().skip(1) {
            let token = value::str_alloc(argument);
            root_value(roots, token);
            tokens.push(token);
        }
        tokens
    } else {
        match argv_items(args[0]) {
            Ok(argv) => argv,
            Err(err) => return err,
        }
    };
    let (prog, description, specs) = {
        let Some(state) = value::as_arg_parser(parser) else {
            return attribute_error("parse_args");
        };
        (
            state.prog.clone(),
            state.description.clone(),
            state.options.clone(),
        )
    };
    let _ = (prog, description);

    // `-h`/`--help` prints the help text. A real invocation (no explicit
    // argv) exits successfully, mirroring argparse's automatic help action;
    // an explicit list keeps the non-exiting `_help` namespace.
    if argv
        .iter()
        .any(|token| matches!(value::as_str(*token), Some("-h") | Some("--help")))
    {
        print_help(parser);
        if default_argv {
            std::process::exit(0);
        }
        return value::namespace_new(vec![("_help".to_string(), value::TRUE)]);
    }

    let mut values: Vec<Option<i64>> = vec![None; specs.len()];
    let mut index = 0usize;
    while index < argv.len() {
        let token = argv[index];
        let Some(text) = value::as_str(token) else {
            return type_error(&format!(
                "parse_args() arguments must be str, not {}",
                value::type_name(token)
            ));
        };
        if text.starts_with('-') && text != "-" {
            let (name, inline) = match split_inline(text) {
                Some((name, value)) => (name, Some(value)),
                None => (text, None),
            };
            let Some(position) = specs
                .iter()
                .position(|spec| !spec.positional && spec.flags.iter().any(|flag| flag == name))
            else {
                // A negative number can be a positional value.
                if inline.is_none() && text.parse::<f64>().is_ok() {
                    if let Some(spec_index) = next_positional(&specs, &values) {
                        let converted = match convert(&specs[spec_index], token) {
                            Ok(value) => value,
                            Err(err) => return err,
                        };
                        root_value(roots, converted);
                        values[spec_index] = Some(converted);
                        index += 1;
                        continue;
                    }
                }
                return runtime_error(&format!("unrecognized arguments: {text}"));
            };
            let spec = &specs[position];
            if spec.store_true {
                if inline.is_some() {
                    return value_error(&format!("argument {name} does not take a value"));
                }
                values[position] = Some(value::TRUE);
                index += 1;
                continue;
            }
            if spec.nargs_plus {
                if inline.is_some() {
                    return value_error(&format!("argument {name} does not take an inline value"));
                }
                let mut collected: Vec<i64> = Vec::new();
                index += 1;
                while index < argv.len() && !is_known_flag(&specs, argv[index]) {
                    let converted = match convert(spec, argv[index]) {
                        Ok(value) => value,
                        Err(err) => return err,
                    };
                    if let Err(err) = validate_choice(spec, converted) {
                        return err;
                    }
                    root_value(roots, converted);
                    collected.push(converted);
                    index += 1;
                }
                if collected.is_empty() {
                    return runtime_error(&format!(
                        "argument {name}: expected at least one argument"
                    ));
                }
                let list = value::list_new(collected);
                root_value(roots, list);
                values[position] = Some(list);
                continue;
            }
            let raw_value = if let Some(inline) = inline {
                value::str_alloc(inline.to_string())
            } else if spec.nargs_optional
                && (index + 1 >= argv.len() || is_known_flag(&specs, argv[index + 1]))
            {
                // Optional value omitted: fall back to the declared default.
                values[position] = Some(spec.default);
                index += 1;
                continue;
            } else if let Some(&next) = argv.get(index + 1) {
                index += 1;
                next
            } else {
                return runtime_error(&format!("argument {name}: expected one argument"));
            };
            let converted = match convert(spec, raw_value) {
                Ok(value) => value,
                Err(err) => return err,
            };
            if let Err(err) = validate_choice(spec, converted) {
                return err;
            }
            root_value(roots, converted);
            values[position] = Some(converted);
            index += 1;
            continue;
        }

        let Some(position) = next_positional(&specs, &values) else {
            return runtime_error(&format!("unrecognized arguments: {text}"));
        };
        let spec = &specs[position];
        let converted = match convert(spec, token) {
            Ok(value) => value,
            Err(err) => return err,
        };
        if let Err(err) = validate_choice(spec, converted) {
            return err;
        }
        root_value(roots, converted);
        values[position] = Some(converted);
        index += 1;
    }

    let mut pairs: Vec<(String, i64)> = Vec::with_capacity(specs.len());
    for (position, spec) in specs.iter().enumerate() {
        let parsed = match values[position] {
            Some(value) => Some(value),
            None if spec.positional => {
                if spec.nargs_optional {
                    None
                } else {
                    return runtime_error(&format!(
                        "the following arguments are required: {}",
                        spec.dest
                    ));
                }
            }
            None if spec.required => {
                return runtime_error(&format!(
                    "the following arguments are required: {}",
                    spec.flags.first().map(String::as_str).unwrap_or(&spec.dest)
                ));
            }
            None => None,
        };
        let parsed = match parsed {
            Some(value) => value,
            None => spec.default,
        };
        pairs.push((spec.dest.clone(), parsed));
    }
    value::namespace_new(pairs)
}

/// Index of the next positional spec that has not been filled yet.
fn next_positional(specs: &[ArgSpec], values: &[Option<i64>]) -> Option<usize> {
    specs
        .iter()
        .enumerate()
        .find(|(index, spec)| spec.positional && values[*index].is_none())
        .map(|(index, _)| index)
}

/// Keeps a freshly parsed value alive while the remaining arguments are
/// converted (allocations may collect).
fn root_value(roots: i64, value: i64) {
    if let Some(mut items) = value::list_items_mut(roots) {
        items.push(value);
        crate::heap::write_barrier_edge(roots, value);
    }
}

/// Whether `token` names a declared option (including its inline form).
fn is_known_flag(specs: &[ArgSpec], token: i64) -> bool {
    let Some(text) = value::as_str(token) else {
        return false;
    };
    let name = split_inline(text).map(|(name, _)| name).unwrap_or(text);
    specs
        .iter()
        .any(|spec| !spec.positional && spec.flags.iter().any(|flag| flag == name))
}

fn convert(spec: &ArgSpec, raw: i64) -> Result<i64, i64> {
    if value::as_str(raw).is_none() {
        return Err(type_error("parse_args() arguments must be str"));
    }
    if spec.type_fn == NONE {
        return Ok(raw);
    }
    let argv = [raw];
    let converted = builtins::call(spec.type_fn, 1, argv.as_ptr());
    if exc::pending() {
        return Err(NONE);
    }
    Ok(converted)
}

fn validate_choice(spec: &ArgSpec, candidate: i64) -> Result<(), i64> {
    let Some(choices) = &spec.choices else {
        return Ok(());
    };
    if choices
        .iter()
        .any(|choice| container::equals(*choice, candidate))
    {
        return Ok(());
    }
    let rendered: Vec<String> = choices.iter().map(|choice| value::repr(*choice)).collect();
    Err(value_error(&format!(
        "invalid choice: {} (choose from {})",
        value::repr(candidate),
        rendered.join(", ")
    )))
}

/// Data attributes of an `ArgumentParser` (`prog`/`description`).
pub(crate) fn parser_attr(obj: i64, attr: &str) -> Option<i64> {
    let state = value::as_arg_parser(obj)?;
    match attr {
        "prog" => Some(match &state.prog {
            Some(prog) => value::str_alloc(prog.clone()),
            None => value::str_alloc("program".to_string()),
        }),
        "description" => Some(match &state.description {
            Some(description) => value::str_alloc(description.clone()),
            None => NONE,
        }),
        _ => None,
    }
}
