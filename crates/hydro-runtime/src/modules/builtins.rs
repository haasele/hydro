//! Native `builtins` module.
//!
//! Every name declared by [`hydro_ir::stdlib::BUILTINS`] is either forwarded
//! to the core implementation or served by the attribute-only helpers below.
//! Names covered by [`hydro_ir::Builtin::from_name`] reuse
//! `crate::builtins::call_builtin`, so `builtins.len`, `builtins.print`, ... are
//! the same implementations the compiler calls directly; arity is validated by
//! the central spec before dispatch.
//!
//! ## Explicitly unsupported names
//!
//! Hydro compiles whole modules and has no interpreter frame, no mutable
//! module namespace object and no bytecode object. The following CPython
//! builtins therefore raise a tagged exception instead of returning a
//! placeholder:
//!
//! - `globals()` / `locals()` / `vars()` without an argument: `RuntimeError`
//!   (no frame namespace exists).
//! - `compile()`: `NotImplementedError` (there is no bytecode compiler).
//! - `type(name, bases, dict)`: `NotImplementedError` (dynamic class creation
//!   is not part of the frontend's class model).
//! - `builtins.__dict__` is a snapshot of the module namespace (the same
//!   mapping `vars(builtins)` returns): the names and values are real, but
//!   mutating the snapshot does not change the module.
//!
//! `vars(instance)` returns the same fresh dict snapshot as `obj.__dict__`;
//! `vars(class)` snapshots the frozen class namespace. Hydro class namespaces
//! are immutable after creation, so no mutation path is exposed.
//!
//! `open` forwards to the runtime's `io.open`, whose positional arguments are
//! `(file, mode, encoding, newline, errors)`.
//!
//! `id()` is implementation-defined: heap values report their address, while
//! immediate values (small ints, `None`, `True`, ...) have no object, so a
//! stable synthetic value is derived from the tagged encoding.

use hydro_ir::{stdlib, Builtin};

use crate::builtins as core_builtins;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, IMPORT_ERROR, NOT_IMPLEMENTED_ERROR, RUNTIME_ERROR, TYPE_ERROR,
};
use crate::methods;
use crate::modules;
use crate::ops;
use crate::value::{self, NONE, NOT_IMPLEMENTED};

/// Dispatches an attribute-only builtin. Names present in
/// `Builtin::from_name` are handled first.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    if function == "__import__" {
        return import_from_core(args);
    }
    if function == "open" {
        return modules::io_open(args);
    }
    if let Some(builtin) = Builtin::from_name(function) {
        return core_builtins::call_builtin(builtin, args.len(), args.as_ptr());
    }
    match function {
        "object" => {
            if args.is_empty() {
                value::instance_new(value::object_class())
            } else {
                type_error(&format!(
                    "object() takes no arguments ({} given)",
                    args.len()
                ))
            }
        }
        "setattr" => setattr(args),
        "delattr" => delattr(args),
        "complex" => core_builtins::complex_new(args),
        "slice" => core_builtins::slice_new(args),
        "id" => id_value(args[0]),
        "dir" => dir(args),
        "vars" => vars(args),
        "globals" | "locals" => exc::raise_with_message(
            RUNTIME_ERROR,
            &format!("{function}() is not supported: Hydro has no frame namespace"),
        ),
        "compile" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "compile() is not supported: Hydro has no bytecode compiler",
        ),
        "issubclass" => core_builtins::call_builtin(Builtin::Issubclass, args.len(), args.as_ptr()),
        _ => missing_attribute(function),
    }
}

/// Keyword dispatch for `builtins` functions that accept arbitrary names.
///
/// `dict([source], **kwargs)` is the only builtin here whose keywords are not
/// declared parameters: every name becomes an entry layered over the optional
/// positional source.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function == "dict" {
        if args.len() > 1 {
            return type_error(&format!(
                "dict() takes at most 1 argument ({} given)",
                args.len()
            ));
        }
        return core_builtins::dict_build(args.first().copied(), keywords);
    }
    type_error(&format!(
        "builtins.{function}() does not accept keyword arguments"
    ))
}

/// Resolves a `builtins` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "None" => NONE,
        "True" => value::TRUE,
        "False" => value::FALSE,
        "NotImplemented" => NOT_IMPLEMENTED,
        "__debug__" => value::TRUE,
        // Hydro has no dedicated ellipsis object kind. A cached instance of a
        // native `ellipsis` class gives the constant stable identity and the
        // right `type()`; only its `repr` (and the absence of an `is`
        // recognizer elsewhere in the runtime) differs from CPython.
        "Ellipsis" => ellipsis_value(),
        // Native instance-backed builtin types. The class values are cached,
        // so `type(x) in (complex, slice)` and `isinstance(x, slice)` compare
        // exact identities.
        "complex" => core_builtins::complex_class(),
        "slice" => core_builtins::slice_class(),
        _ => match exception_tag(name) {
            Some(tag) => exc::exception_class(tag),
            None => missing_attribute(name),
        },
    }
}

fn missing_attribute(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'builtins' has no attribute '{name}'"),
    )
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Maps a builtin exception name to the runtime taxonomy tag.
fn exception_tag(name: &str) -> Option<u32> {
    Some(match name {
        "BaseException" => exc::BASE_EXCEPTION,
        "Exception" => exc::EXCEPTION,
        "ArithmeticError" => exc::ARITHMETIC_ERROR,
        "AssertionError" => exc::ASSERTION_ERROR,
        "AttributeError" => exc::ATTRIBUTE_ERROR,
        "BaseExceptionGroup" => exc::BASE_EXCEPTION_GROUP,
        "BlockingIOError" => exc::BLOCKING_IO_ERROR,
        "BrokenPipeError" => exc::BROKEN_PIPE_ERROR,
        "BufferError" => exc::BUFFER_ERROR,
        "BytesWarning" => exc::BYTES_WARNING,
        "CancelledError" => exc::CANCELLED_ERROR,
        "ChildProcessError" => exc::CHILD_PROCESS_ERROR,
        "ConnectionAbortedError" => exc::CONNECTION_ABORTED_ERROR,
        "ConnectionError" => exc::CONNECTION_ERROR,
        "ConnectionRefusedError" => exc::CONNECTION_REFUSED_ERROR,
        "ConnectionResetError" => exc::CONNECTION_RESET_ERROR,
        "DeprecationWarning" => exc::DEPRECATION_WARNING,
        "EOFError" => exc::EOF_ERROR,
        "ExceptionGroup" => exc::EXCEPTION_GROUP,
        "FileExistsError" => exc::FILE_EXISTS_ERROR,
        "FileNotFoundError" => exc::FILE_NOT_FOUND_ERROR,
        "FloatingPointError" => exc::FLOATING_POINT_ERROR,
        "FutureWarning" => exc::FUTURE_WARNING,
        "GeneratorExit" => exc::GENERATOR_EXIT,
        "ImportError" => exc::IMPORT_ERROR,
        "ImportWarning" => exc::IMPORT_WARNING,
        "IndentationError" => exc::INDENTATION_ERROR,
        "IndexError" => exc::INDEX_ERROR,
        "InterruptedError" => exc::INTERRUPTED_ERROR,
        "IsADirectoryError" => exc::IS_A_DIRECTORY_ERROR,
        "KeyError" => exc::KEY_ERROR,
        "KeyboardInterrupt" => exc::KEYBOARD_INTERRUPT,
        "LookupError" => exc::LOOKUP_ERROR,
        "MemoryError" => exc::MEMORY_ERROR,
        "ModuleNotFoundError" => exc::MODULE_NOT_FOUND_ERROR,
        "NameError" => exc::NAME_ERROR,
        "NotADirectoryError" => exc::NOT_A_DIRECTORY_ERROR,
        "NotImplementedError" => exc::NOT_IMPLEMENTED_ERROR,
        "OSError" => exc::OS_ERROR,
        // `IOError` is CPython's historical alias of `OSError`.
        "IOError" => exc::OS_ERROR,
        "OverflowError" => exc::OVERFLOW_ERROR,
        "PendingDeprecationWarning" => exc::PENDING_DEPRECATION_WARNING,
        "PermissionError" => exc::PERMISSION_ERROR,
        "ProcessLookupError" => exc::PROCESS_LOOKUP_ERROR,
        "RecursionError" => exc::RECURSION_ERROR,
        "ReferenceError" => exc::REFERENCE_ERROR,
        "ResourceWarning" => exc::RESOURCE_WARNING,
        "RuntimeError" => exc::RUNTIME_ERROR,
        "RuntimeWarning" => exc::RUNTIME_WARNING,
        "StopAsyncIteration" => exc::STOP_ASYNC_ITERATION,
        "StopIteration" => exc::STOP_ITERATION,
        "SyntaxError" => exc::SYNTAX_ERROR,
        "SyntaxWarning" => exc::SYNTAX_WARNING,
        "SystemError" => exc::SYSTEM_ERROR,
        "SystemExit" => exc::SYSTEM_EXIT,
        "TabError" => exc::TAB_ERROR,
        "TimeoutError" => exc::TIMEOUT_ERROR,
        "TypeError" => exc::TYPE_ERROR,
        "UnicodeDecodeError" => exc::UNICODE_DECODE_ERROR,
        "UnicodeEncodeError" => exc::UNICODE_ENCODE_ERROR,
        "UnicodeError" => exc::UNICODE_ERROR,
        "UnicodeTranslateError" => exc::UNICODE_TRANSLATE_ERROR,
        "UnicodeWarning" => exc::UNICODE_WARNING,
        "UserWarning" => exc::USER_WARNING,
        "ValueError" => exc::VALUE_ERROR,
        "Warning" => exc::WARNING,
        "ZeroDivisionError" => exc::ZERO_DIVISION_ERROR,
        // Runtime-internal exception tags that libraries probe by name.
        "Skipped" => exc::SKIPPED,
        "XFailed" => exc::XFAILED,
        _ => return None,
    })
}

fn ellipsis_value() -> i64 {
    if let Some(cached) = globals::registry_get("builtins.Ellipsis") {
        return cached;
    }
    let class = value::class_new("ellipsis".to_string(), NONE, Vec::new());
    let instance = value::instance_new(class);
    globals::registry_set("builtins.Ellipsis", instance);
    instance
}

fn setattr(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[1]) else {
        return type_error(&format!(
            "attribute name must be string, not '{}'",
            value::type_name(args[1])
        ));
    };
    methods::setattr(args[0], name.as_ptr(), name.len(), args[2])
}

fn delattr(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[1]) else {
        return type_error(&format!(
            "attribute name must be string, not '{}'",
            value::type_name(args[1])
        ));
    };
    methods::delete_attr(args[0], name.as_ptr(), name.len())
}

fn id_value(v: i64) -> i64 {
    if value::is_heap(v) {
        // Heap values are their own identity; the tag bits are masked off.
        return ops::int_result(v & !value::TAG_MASK);
    }
    // Immediates are plain tagged integers/specials, not objects. XOR with a
    // constant whose low bits are `0b111` keeps the result injective over
    // immediates and away from the heap tag (`0b010`), so it can never equal a
    // heap object's id.
    ops::int_result((v as u64 ^ 0x9e37_79b9_7f4a_7c17u64) as i64)
}

/// `dir()`.
///
/// Classes, instances and native modules have enumerable namespaces; every
/// other value raises `NotImplementedError` because this runtime has no
/// reflection table of builtin type methods.
fn dir(args: &[i64]) -> i64 {
    if args.is_empty() {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "dir() without arguments is not supported: Hydro has no frame namespace",
        );
    }
    let object = args[0];
    let mut names: Vec<String> = Vec::new();
    if let Some(state) = value::as_instance(object) {
        names.push("__class__".to_string());
        names.push("__dict__".to_string());
        {
            let attrs = state.lock_attrs();
            names.extend(attrs.iter().map(|(name, _)| name.clone()));
        }
        collect_class_names(state.class, &mut names);
    } else if value::as_class(object).is_some() {
        names.push("__name__".to_string());
        names.push("__class__".to_string());
        collect_class_names(object, &mut names);
    } else if let Some(module) = value::as_module(object) {
        let module_name = module.name().to_string();
        if module_name == "errno" {
            return exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                "dir() cannot enumerate 'errno': its names are resolved on demand",
            );
        }
        names.push("__name__".to_string());
        match stdlib::module(&module_name) {
            Some(spec) => {
                names.extend(spec.functions.iter().map(|f| f.name.to_string()));
                names.extend(spec.constants.iter().map(|c| c.name.to_string()));
            }
            None => {
                return exc::raise_with_message(
                    NOT_IMPLEMENTED_ERROR,
                    &format!(
                        "dir() cannot enumerate user module '{module_name}': Hydro keeps globals in a side store"
                    ),
                )
            }
        }
    } else {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!(
                "dir() is not supported for '{}' objects in Hydro",
                value::type_name(object)
            ),
        );
    }
    names.sort_unstable();
    names.dedup();
    // Append each name as it is allocated: a plain `Vec` buffer is invisible
    // to the conservative stack scan, so batching the allocations could let an
    // earlier name be collected.
    let result = value::list_new(Vec::new());
    for name in names {
        let text = value::str_alloc(name);
        let roots = crate::heap::root_values(&[text]);
        crate::container::list_append(result, text);
        crate::heap::release_roots(roots);
    }
    result
}

/// Appends the method/attribute names of `class` and its bases.
fn collect_class_names(class: i64, names: &mut Vec<String>) {
    let mut current = class;
    for _ in 0..1024 {
        let Some(state) = value::as_class(current) else {
            return;
        };
        names.extend(state.methods.iter().map(|(name, _, _)| name.clone()));
        names.extend(state.attrs.iter().map(|(name, _)| name.clone()));
        if state.base == NONE {
            return;
        }
        current = state.base;
    }
}

/// `vars([object])`: the instance/class attribute table, or a function value's
/// live `__dict__`. Module and builtin namespaces have no real mapping, so they
/// raise `TypeError` like CPython's "argument must have `__dict__`" path.
fn vars(args: &[i64]) -> i64 {
    if args.is_empty() {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "vars() without arguments is not supported: Hydro has no frame namespace",
        );
    }
    let object = args[0];
    let pairs: Vec<(String, i64)> = if value::as_instance(object).is_some() {
        // Match `obj.__dict__`: `instance_dict` snapshots the attribute table.
        let Some(mapping) = value::instance_dict(object) else {
            return type_error("instance has no __dict__");
        };
        return mapping;
    } else if let Some(state) = value::as_class(object) {
        let mut pairs = Vec::with_capacity(state.methods.len() + state.attrs.len());
        pairs.extend(
            state
                .methods
                .iter()
                .map(|(name, function, _)| (name.clone(), *function)),
        );
        pairs.extend(state.attrs.iter().cloned());
        pairs
    } else if value::function_has_attributes(object) {
        // Function values have a real `__dict__`; `vars(f)` is that live
        // mapping, like CPython, not a snapshot.
        return value::function_dict(object).unwrap_or_else(|| value::dict_new(Vec::new()));
    } else {
        return type_error(&format!(
            "vars() argument must have __dict__ attribute, not '{}'",
            value::type_name(object)
        ));
    };
    // Insert one entry at a time and root the freshly allocated key until the
    // dict owns it (see the comment in `dir`).
    let namespace = value::dict_new(Vec::new());
    for (name, item) in pairs {
        let key = value::str_alloc(name);
        let roots = crate::heap::root_values(&[key, item]);
        crate::container::dict_set(namespace, key, item);
        crate::heap::release_roots(roots);
    }
    namespace
}

/// `__import__(name, globals=None, locals=None, fromlist=(), level=0)`.
///
/// The entry point for both call shapes: `builtins.__import__(...)` (through
/// [`call`]), the compiler's direct `__import__` lowering and the runtime
/// keyword binder all reach it with a positional five-slot vector. Missing
/// optional slots are `None`, which is how the defaults (`globals=None`,
/// `locals=None`, `fromlist=()`, `level=0`) are spelled. `globals` and
/// `locals` are accepted for signature compatibility only; resolution never
/// consults a caller frame.
///
/// Resolution is registry-based, matching `importlib.import_module`: names the
/// program cannot see statically still resolve to their already-loaded module
/// value, and unknown names raise `ModuleNotFoundError`.
pub(crate) fn import_from_core(args: &[i64]) -> i64 {
    if args.len() > 5 {
        return type_error(&format!(
            "__import__() takes at most 5 arguments ({} given)",
            args.len()
        ));
    }
    let Some(name_value) = args.first().copied() else {
        return type_error("__import__() missing required argument 'name' (pos 1)");
    };
    let Some(name) = value::as_str(name_value) else {
        return type_error(&format!(
            "__import__() argument 1 must be str, not '{}'",
            value::type_name(name_value)
        ));
    };
    let name = name.to_string();
    let level = args.get(4).copied().unwrap_or(NONE);
    let relative = match value::as_integer(level) {
        Some(level) => level != 0,
        None => value::truthy(level),
    };
    if relative {
        return exc::raise_with_message(
            IMPORT_ERROR,
            "relative imports are not supported by Hydro",
        );
    }
    // `fromlist` requests the submodule instead of its top-level package. Its
    // truthiness follows Python's, so a list, tuple or string with any element
    // selects the full dotted module and an empty one the top-level package.
    let fromlist = args.get(3).copied().unwrap_or(NONE);
    let want_full = fromlist != NONE && value::truthy(fromlist);
    if !want_full {
        if let Some((top, _)) = name.split_once('.') {
            if let Some(module) = modules::registry_lookup(top) {
                return module;
            }
        }
    }
    if let Some(module) = modules::registry_lookup(&name) {
        return module;
    }
    // Defer to the import machinery for the exact `ModuleNotFoundError`.
    modules::importlib::call("import_module", &[name_value])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> i64 {
        value::str_from_bytes(text.as_bytes())
    }

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn import_function() -> i64 {
        value::builtin_function_new(Builtin::Import)
    }

    fn pending_message() -> String {
        value::to_string(exc::pending_value())
    }

    /// Calls `__import__` through the runtime keyword binder, which is the
    /// path a first-class `__import__` value (or a keyword call the frontend
    /// cannot bind statically) takes.
    fn call_import(positional: &[i64], pairs: &[i64]) -> i64 {
        assert_eq!(pairs.len() % 2, 0, "keyword pairs are name/value tuples");
        let function = import_function();
        core_builtins::call_kw(
            function,
            positional.len(),
            positional.as_ptr(),
            pairs.len() / 2,
            pairs.as_ptr(),
        )
    }

    #[test]
    fn fromlist_list_selects_the_dotted_module() {
        exc::init();
        let name = string("os.path");
        let star = string("*");
        let fromlist = value::list_new(vec![star]);
        let pairs = [string("fromlist"), fromlist];
        let module = call_import(std::slice::from_ref(&name), &pairs);
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
        assert_eq!(
            module,
            modules::registry_lookup("os.path").expect("os.path is registered")
        );
    }

    #[test]
    fn fromlist_string_selects_the_dotted_module() {
        exc::init();
        let name = string("os.path");
        let fromlist = string("*");
        let pairs = [string("fromlist"), fromlist];
        let module = call_import(std::slice::from_ref(&name), &pairs);
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
        assert_eq!(
            module,
            modules::registry_lookup("os.path").expect("os.path is registered")
        );
    }

    #[test]
    fn level_zero_binds_by_keyword_with_globals_and_locals() {
        exc::init();
        let name = string("os.path");
        let globals = value::dict_new(Vec::new());
        let locals = value::dict_new(Vec::new());
        let fromlist = value::tuple_new(vec![string("abspath")]);
        // The parameter order need not match the declaration: the binder maps
        // every name onto its slot.
        let pairs = [
            string("level"),
            int(0),
            string("fromlist"),
            fromlist,
            string("locals"),
            locals,
            string("globals"),
            globals,
        ];
        let module = call_import(std::slice::from_ref(&name), &pairs);
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
        assert_eq!(
            module,
            modules::registry_lookup("os.path").expect("os.path is registered")
        );
    }

    #[test]
    fn duplicate_keyword_reports_multiple_values() {
        exc::init();
        let name = string("os.path");
        let pairs = [string("level"), int(0), string("level"), int(0)];
        let _ = call_import(std::slice::from_ref(&name), &pairs);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "__import__() got multiple values for argument 'level'"
        );
        exc::clear();
    }

    #[test]
    fn positional_and_keyword_conflict_reports_multiple_values() {
        exc::init();
        let args = [string("os.path")];
        let pairs = [string("name"), string("os.path")];
        let _ = call_import(&args, &pairs);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "__import__() got multiple values for argument 'name'"
        );
        exc::clear();
    }

    #[test]
    fn unknown_keyword_is_rejected() {
        exc::init();
        let name = string("os.path");
        let pairs = [string("bogus"), int(1)];
        let _ = call_import(std::slice::from_ref(&name), &pairs);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "__import__() got an unexpected keyword argument 'bogus'"
        );
        exc::clear();
    }

    #[test]
    fn missing_name_is_reported() {
        exc::init();
        let pairs = [string("level"), int(0)];
        let _ = call_import(&[], &pairs);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "__import__() missing required argument 'name' (pos 1)"
        );
        exc::clear();
    }

    #[test]
    fn positional_five_slot_vector_selects_the_dotted_module() {
        exc::init();
        // The compiled direct `__import__` lowering passes the five slots
        // positionally: name, globals, locals, fromlist, level.
        let args = [
            string("os.path"),
            NONE,
            NONE,
            value::tuple_new(vec![string("*")]),
            int(0),
        ];
        let module = core_builtins::call_builtin(Builtin::Import, args.len(), args.as_ptr());
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
        assert_eq!(
            module,
            modules::registry_lookup("os.path").expect("os.path is registered")
        );
    }

    #[test]
    fn iter_callable_forwards_to_the_core_iterator() {
        exc::init();
        // `list()` with no arguments returns a fresh empty list per call.
        let source = value::builtin_function_new(Builtin::List);
        let sentinel = value::list_new(Vec::new());
        let iterator = call("iter", &[source, sentinel]);
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
        assert_eq!(value::type_name(iterator), "iterator");

        // The first result already equals the sentinel, so iteration ends.
        assert_eq!(call("next", &[iterator]), NONE);
        assert_eq!(exc::pending_tag(), exc::STOP_ITERATION as i64);
        exc::clear();
    }
}
