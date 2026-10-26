//! `sysconfig`: installation layout and build configuration.
//!
//! The values are derived from the running Hydro installation and the emulated
//! CPython version ([`hydro_ir::PYTHON_VERSION`]): `py_version`, `platform`,
//! `SOABI`, `EXT_SUFFIX`, the `posix_prefix`/`posix_home` install schemes and
//! the shared-library suffix. Values Hydro does not know (compiler flags, the
//! build directory, feature probes) return `None`, exactly like CPython for an
//! undefined variable.
//!
//! `get_paths()`/`get_path()` expand the CPython install schemes with the real
//! `base`/`platbase`/`installed_base`/`py_version_short` variables; a `vars`
//! mapping may override them.

use hydro_ir::{PYTHON_ABI_TAG, PYTHON_VERSION};

use crate::exc::{self, ATTRIBUTE_ERROR, KEY_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Keys of every install scheme, in CPython's order.
const SCHEME_KEYS: [&str; 8] = [
    "stdlib",
    "platstdlib",
    "purelib",
    "platlib",
    "include",
    "platinclude",
    "scripts",
    "data",
];

/// `posix_prefix`, CPython 3.14's default on POSIX.
const POSIX_PREFIX: [&str; 8] = [
    "{installed_base}/lib/python{py_version_short}",
    "{platbase}/lib/python{py_version_short}",
    "{base}/lib/python{py_version_short}/site-packages",
    "{platbase}/lib/python{py_version_short}/site-packages",
    "{installed_base}/include/python{py_version_short}{abiflags}",
    "{installed_platbase}/include/python{py_version_short}{abiflags}",
    "{base}/bin",
    "{base}",
];

/// `posix_home`.
const POSIX_HOME: [&str; 8] = [
    "{installed_base}/lib/python",
    "{base}/lib/python",
    "{base}/lib/python",
    "{platbase}/lib/python",
    "{installed_base}/include/python",
    "{installed_platbase}/include/python{py_version_short}{abiflags}",
    "{base}/bin",
    "{base}",
];

fn encode(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| exc::raise_with_message(exc::OVERFLOW_ERROR, "overflow"))
}

fn major_minor() -> String {
    format!("{}.{}", PYTHON_VERSION.0, PYTHON_VERSION.1)
}

fn nodot() -> String {
    format!("{}{}", PYTHON_VERSION.0, PYTHON_VERSION.1)
}

/// `py_version_short` (`"3.14"`).
fn py_version_short() -> String {
    major_minor()
}

fn abiflags() -> String {
    // Hydro has no debug/free-threaded ABI marker.
    String::new()
}

/// GNU multi-architecture triplet for the running target.
fn multiarch() -> String {
    let arch = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-linux-gnu",
        ("aarch64", "linux") => "aarch64-linux-gnu",
        ("x86", "linux") => "i386-linux-gnu",
        ("arm", "linux") => "arm-linux-gnueabihf",
        (arch, "linux") => return format!("{arch}-linux-gnu"),
        (arch, os) => return format!("{arch}-{os}"),
    };
    arch.to_string()
}

fn soabi() -> String {
    format!("{}-{}", PYTHON_ABI_TAG, multiarch())
}

fn shlib_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        ".dylib"
    } else if cfg!(windows) {
        ".dll"
    } else {
        ".so"
    }
}

/// `get_platform()`: `linux-x86_64`.
pub(crate) fn platform() -> String {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macosx",
        "windows" => "win32",
        "freebsd" => "freebsd",
        other => other,
    };
    format!("{}-{}", os, std::env::consts::ARCH)
}

fn prefix_base() -> String {
    super::installation_prefix()
}

fn installed_base() -> String {
    super::base_prefix()
}

/// One configuration variable, or `None` when the runtime does not know it.
fn config_value(name: &str) -> Option<i64> {
    let text = |s: String| Some(value::str_alloc(s));
    match name {
        "prefix" => text(prefix_base()),
        // `sys.exec_prefix` in this runtime follows the installation prefix.
        "exec_prefix" => text(installed_base()),
        "base" | "platbase" => text(prefix_base()),
        "installed_base" | "installed_platbase" => text(installed_base()),
        "py_version" => text(hydro_ir::PYTHON_VERSION_STRING.to_string()),
        "py_version_short" => text(py_version_short()),
        "py_version_nodot" => text(nodot()),
        "abiflags" => text(abiflags()),
        "platform" => text(platform()),
        "SOABI" => text(soabi()),
        "EXT_SUFFIX" => text(format!(".{}{}", soabi(), shlib_suffix())),
        "SHLIB_SUFFIX" => text(shlib_suffix().to_string()),
        "SO" => text(shlib_suffix().to_string()),
        "MULTIARCH" => text(multiarch()),
        "VERSION" => text(py_version_short()),
        "INCLUDEPY" => text(format!(
            "{}/include/python{}",
            installed_base(),
            py_version_short()
        )),
        "LIBDIR" => text(format!("{}/lib", prefix_base())),
        "LIBPL" => text(format!(
            "{}/lib/python{}/config-{}-{}",
            prefix_base(),
            py_version_short(),
            py_version_short(),
            multiarch()
        )),
        "DESTSHARED" => text(format!(
            "{}/lib/python{}/lib-dynload",
            prefix_base(),
            py_version_short()
        )),
        // Value-type probes Hydro can answer deterministically.
        "Py_DEBUG" => Some(encode(0)),
        "Py_GIL_DISABLED" => Some(encode(0)),
        "Py_ENABLE_SHARED" => Some(encode(1)),
        "HAVE_GCC_ASM_FOR_X87" => Some(encode(i64::from(cfg!(any(
            target_arch = "x86",
            target_arch = "x86_64"
        ))))),
        "SIZEOF_VOID_P" => Some(encode(std::mem::size_of::<usize>() as i64)),
        "SIZEOF_LONG" => Some(encode(std::mem::size_of::<libc::c_long>() as i64)),
        "SIZEOF_SIZE_T" => Some(encode(std::mem::size_of::<usize>() as i64)),
        "SIZEOF_TIME_T" => Some(encode(std::mem::size_of::<libc::time_t>() as i64)),
        // Compiler/build probes: unknown for a Rust-native runtime.
        _ => None,
    }
}

/// `get_config_var(name)`.
fn get_config_var(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_config_var() argument must be str, not {}",
                value::type_name(args[0])
            ),
        );
    };
    config_value(name).unwrap_or(NONE)
}

/// Names reported by `get_config_vars()` without arguments.
const CONFIG_NAMES: &[&str] = &[
    "prefix",
    "exec_prefix",
    "base",
    "platbase",
    "installed_base",
    "installed_platbase",
    "py_version",
    "py_version_short",
    "py_version_nodot",
    "abiflags",
    "platform",
    "SOABI",
    "EXT_SUFFIX",
    "SHLIB_SUFFIX",
    "SO",
    "MULTIARCH",
    "VERSION",
    "INCLUDEPY",
    "LIBDIR",
    "LIBPL",
    "DESTSHARED",
    "Py_DEBUG",
    "Py_GIL_DISABLED",
    "Py_ENABLE_SHARED",
    "SIZEOF_VOID_P",
    "SIZEOF_LONG",
    "SIZEOF_SIZE_T",
    "SIZEOF_TIME_T",
];

fn make_dict(pairs: Vec<(&str, i64)>) -> i64 {
    let dict = value::dict_new(Vec::new());
    let dict_root = heap::root_values(&[dict]);
    for (name, value_arg) in pairs {
        let key = value::str_alloc(name.to_string());
        let roots = heap::root_values(&[key, value_arg]);
        crate::container::dict_set(dict, key, value_arg);
        heap::release_roots(roots);
    }
    heap::release_roots(dict_root);
    dict
}

/// `get_config_vars(*names)`.
fn get_config_vars(args: &[i64]) -> i64 {
    if args.is_empty() {
        let pairs = CONFIG_NAMES
            .iter()
            .map(|&name| (name, config_value(name).unwrap_or(NONE)))
            .collect();
        return make_dict(pairs);
    }
    let mut pairs = Vec::with_capacity(args.len());
    for &raw in args {
        let Some(name) = value::as_str(raw) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "get_config_vars() names must be str, not {}",
                    value::type_name(raw)
                ),
            );
        };
        pairs.push((name, config_value(name).unwrap_or(NONE)));
    }
    make_dict(pairs)
}

fn scheme_table(name: &str) -> Option<&'static [&'static str; 8]> {
    match name {
        "posix_prefix" => Some(&POSIX_PREFIX),
        "posix_home" => Some(&POSIX_HOME),
        _ => None,
    }
}

/// Variables used when expanding a scheme.
fn scheme_vars(overrides: Option<i64>) -> Vec<(String, String)> {
    let mut vars = vec![
        ("base".to_string(), prefix_base()),
        ("platbase".to_string(), prefix_base()),
        ("installed_base".to_string(), installed_base()),
        ("installed_platbase".to_string(), installed_base()),
        ("py_version_short".to_string(), py_version_short()),
        (
            "py_version".to_string(),
            hydro_ir::PYTHON_VERSION_STRING.to_string(),
        ),
        ("abiflags".to_string(), abiflags()),
    ];
    if let Some(mapping) = overrides {
        if let Some(pairs) = value::as_dict(mapping) {
            for (key, item) in pairs {
                let Some(name) = value::as_str(*key) else {
                    continue;
                };
                let replacement = if let Some(text) = value::as_str(*item) {
                    text.to_string()
                } else if let Some(integer) = value::as_integer(*item) {
                    integer.to_string()
                } else {
                    continue;
                };
                match vars.iter_mut().find(|(existing, _)| existing == name) {
                    Some(entry) => entry.1 = replacement,
                    None => vars.push((name.to_string(), replacement)),
                }
            }
        }
    }
    vars
}

fn expand(template: &str, vars: &[(String, String)]) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let name = &tail[..end];
        match vars.iter().find(|(existing, _)| existing == name) {
            Some((_, replacement)) => result.push_str(replacement),
            None => {
                result.push('{');
                result.push_str(name);
                result.push('}');
            }
        }
        rest = &tail[end + 1..];
    }
    result.push_str(rest);
    result
}

fn scheme_argument(args: &[i64], index: usize) -> Result<String, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => match std::env::consts::OS {
            "windows" => Ok("nt".to_string()),
            _ => Ok("posix_prefix".to_string()),
        },
        Some(raw) => match value::as_str(raw) {
            Some(name) => Ok(name.to_string()),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!("scheme must be str, not {}", value::type_name(raw)),
            )),
        },
    }
}

/// `get_paths(scheme=None, vars=None, expand=True)`.
fn get_paths(args: &[i64]) -> i64 {
    let scheme = match scheme_argument(args, 0) {
        Ok(scheme) => scheme,
        Err(error) => return error,
    };
    let Some(table) = scheme_table(&scheme) else {
        return exc::raise_with_message(VALUE_ERROR, &format!("unknown scheme '{scheme}'"));
    };
    let expand_paths = args.get(2).copied().is_none_or(value::truthy);
    let vars = scheme_vars(args.get(1).copied().filter(|raw| *raw != NONE));
    let pairs = SCHEME_KEYS
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let template = table[index];
            let path = if expand_paths {
                expand(template, &vars)
            } else {
                template.to_string()
            };
            (*key, value::str_alloc(path))
        })
        .collect();
    make_dict(pairs)
}

/// `get_path(name, scheme=None, vars=None, expand=True)`.
fn get_path(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("path name must be str, not {}", value::type_name(args[0])),
        );
    };
    let scheme = match scheme_argument(args, 1) {
        Ok(scheme) => scheme,
        Err(error) => return error,
    };
    let Some(table) = scheme_table(&scheme) else {
        return exc::raise_with_message(VALUE_ERROR, &format!("unknown scheme '{scheme}'"));
    };
    let Some(index) = SCHEME_KEYS.iter().position(|key| *key == name) else {
        return exc::raise_with_message(KEY_ERROR, &format!("unknown path name '{name}'"));
    };
    let expand_path = args.get(3).copied().is_none_or(value::truthy);
    let vars = scheme_vars(args.get(2).copied().filter(|raw| *raw != NONE));
    let template = table[index];
    let path = if expand_path {
        expand(template, &vars)
    } else {
        template.to_string()
    };
    value::str_alloc(path)
}

fn get_python_version(_args: &[i64]) -> i64 {
    value::str_alloc(major_minor())
}

fn get_platform(_args: &[i64]) -> i64 {
    value::str_alloc(platform())
}

fn get_scheme_names(_args: &[i64]) -> i64 {
    let names = vec![
        value::str_alloc("posix_prefix".to_string()),
        value::str_alloc("posix_home".to_string()),
    ];
    value::tuple_new(names)
}

fn get_default_scheme(_args: &[i64]) -> i64 {
    value::str_alloc(if cfg!(windows) {
        "nt".to_string()
    } else {
        "posix_prefix".to_string()
    })
}

fn is_python_build(_args: &[i64]) -> i64 {
    value::FALSE
}

/// Dispatches a native `sysconfig` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "get_config_var" | "_get_config_var" => get_config_var(args),
        "get_config_vars" | "_get_config_vars" => get_config_vars(args),
        "get_paths" | "_get_paths" => get_paths(args),
        "get_path" | "_get_path" => get_path(args),
        "get_python_version" => get_python_version(args),
        "get_platform" => get_platform(args),
        "get_scheme_names" => get_scheme_names(args),
        "get_default_scheme" | "get_preferred_scheme" => get_default_scheme(args),
        "is_python_build" => is_python_build(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'sysconfig' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `sysconfig` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    let text = |s: String| value::str_alloc(s);
    match name {
        "PREFIX" | "prefix" => text(prefix_base()),
        "EXEC_PREFIX" | "exec_prefix" => text(installed_base()),
        "BASE_PREFIX" | "BASE_EXEC_PREFIX" => text(installed_base()),
        "installed_base" | "installed_platbase" | "platbase" => text(installed_base()),
        "py_version" => text(hydro_ir::PYTHON_VERSION_STRING.to_string()),
        "py_version_short" => text(py_version_short()),
        "py_version_nodot" => text(nodot()),
        "abiflags" => text(abiflags()),
        "platform" => text(platform()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'sysconfig' has no attribute '{name}'"),
        ),
    }
}
