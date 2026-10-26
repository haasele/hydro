//! The CPython compatibility target Hydro emulates.
//!
//! This is the single definition of the emulated Python version. The runtime
//! (`sys.version`, `sys.version_info`, `platform.python_version()`), the CLI
//! (`tea --version --verbose`) and the frontend's extension-wheel ABI policy
//! all derive their values from these constants, so bumping the target is a
//! one-line change here.
//!
//! The target tracks the newest CPython patch release that is not yet
//! security-only; the release policy for long-term-support lines is documented
//! in `docs/COMPATIBILITY.md`.

/// Emulated Python version as `(major, minor, micro)`.
pub const PYTHON_VERSION: (u32, u32, u32) = (3, 14, 9);

/// Emulated Python version as `major.minor.micro`.
pub const PYTHON_VERSION_STRING: &str = "3.14.9";

/// CPython ABI tag used in extension wheel module names.
pub const PYTHON_ABI_TAG: &str = "cpython-314";

#[cfg(test)]
mod tests {
    use super::{PYTHON_ABI_TAG, PYTHON_VERSION, PYTHON_VERSION_STRING};

    #[test]
    fn derived_constants_match_the_version_tuple() {
        let (major, minor, micro) = PYTHON_VERSION;
        let version = format!("{major}.{minor}.{micro}");
        let abi_tag = format!("cpython-{major}{minor}");
        assert_eq!(PYTHON_VERSION_STRING, version);
        assert_eq!(PYTHON_ABI_TAG, abi_tag);
    }
}
