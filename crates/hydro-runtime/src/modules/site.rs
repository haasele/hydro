//! `site`: the importable site stub.
//!
//! Hydro never adds site directories: imports resolve against the module
//! search roots fixed by the build/harness, not against `.pth` files or a
//! user site directory. The module therefore reports the truthful values:
//!
//! * `ENABLE_USER_SITE = False` (no user site directory is added);
//! * `USER_SITE` / `USER_BASE = None` (there is no user site to name);
//! * `PREFIXES = [prefix, exec_prefix]`, built from the same installation
//!   prefix `sys.prefix`/`sys.exec_prefix` report;
//! * `main()` performs no work and returns `None`, matching a runtime with no
//!   site processing to do.
//!
//! `addsitedir()` raises `NotImplementedError`: the import system consults no
//! runtime-mutable directory list, so silently accepting a path would claim
//! that packages from it become importable.

use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'site' has no attribute '{name}'"),
    )
}

/// `site.PREFIXES`: `[sys.prefix, sys.exec_prefix]`.
fn prefixes() -> i64 {
    let prefix = crate::modules::installation_prefix();
    let exec_prefix = crate::modules::base_prefix();
    let start = heap::root_values(&[]);
    let prefix = value::str_alloc(prefix);
    heap::push_scoped_root(prefix);
    let exec_prefix = value::str_alloc(exec_prefix);
    heap::push_scoped_root(exec_prefix);
    let result = value::list_new(vec![prefix, exec_prefix]);
    heap::release_roots(start);
    result
}

/// Materialises a `site` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        // No user site directory is added.
        "ENABLE_USER_SITE" => value::FALSE,
        "USER_SITE" | "USER_BASE" => NONE,
        "PREFIXES" => prefixes(),
        _ => attribute_error(name),
    }
}

/// Dispatches a native `site` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // Nothing is added to the module search path, so there is no work to
        // do (and no error to report) for both the no-argument and
        // `known_paths` forms.
        "main" => NONE,
        "addsitedir" => {
            let path = args.first().copied().unwrap_or(NONE);
            if value::as_str(path).is_none() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "addsitedir() argument 1 must be a str, not {}",
                        value::type_name(path)
                    ),
                );
            }
            exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                "site.addsitedir() is not implemented: Hydro's import system does not scan \
                 site directories at runtime",
            )
        }
        _ => attribute_error(function),
    }
}
