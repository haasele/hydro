//! Process-global function-entry profile counters.
//!
//! Generated code calls `hydro_profile_hit(name_ptr, name_len)` at the entry
//! of every real IR function when `hydro_codegen::CodegenOptions::collect_profile`
//! is set. The counts are used by Tea's flavor/PGO flow to feed
//! `CodegenOptions::hot_functions` back into code generation.
//!
//! Storage is a process-global `Mutex<HashMap<String, u64>>`; the entry points
//! are the C ABI functions in [`crate::abi`]. None of them panics: a null or
//! non-UTF-8 name is ignored and a failed file write reports `-1`.
//!
//! Dump format (`hydro_profile_dump`):
//!
//! ```text
//! # hydro-profile-v1
//! <count>\t<name>\n
//! ```
//!
//! Lines are sorted by name. Tabs and newlines in a name are replaced with
//! spaces so every record stays one tab-separated line.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex, MutexGuard};

static COUNTS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn counts() -> MutexGuard<'static, HashMap<String, u64>> {
    // The counter map is plain data; a panic while holding the lock cannot
    // leave it inconsistent, so recover the guard instead of propagating.
    COUNTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drop every recorded count.
pub(crate) fn reset() {
    counts().clear();
}

/// Record one entry into `name`.
pub(crate) fn hit(name: &str) {
    let mut counts = counts();
    let count = counts.entry(name.to_owned()).or_insert(0);
    *count = count.saturating_add(1);
}

/// Replace record separators in a name so the dump stays line-oriented.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '\t' | '\n' => ' ',
            other => other,
        })
        .collect()
}

/// Write the profile to `path` (`# hydro-profile-v1`, then one
/// `<count>\t<name>` record per line sorted by name).
///
/// Returns 0 on success and -1 when the path is not valid UTF-8, cannot be
/// created or cannot be written. The file is written synchronously and closed
/// before returning; no panic escapes on IO failure.
pub(crate) fn dump(path: &str) -> i32 {
    // Snapshot and release the lock before touching the filesystem so a slow
    // disk never blocks generated code running on other threads.
    let mut records: Vec<(String, u64)> = {
        let counts = counts();
        counts
            .iter()
            .map(|(name, count)| (sanitize_name(name), *count))
            .collect()
    };
    records.sort_by_key(|record| record.0.clone());

    let mut text = String::from("# hydro-profile-v1\n");
    for (name, count) in records {
        // Writing into a `String` is infallible.
        let _ = writeln!(text, "{count}\t{name}");
    }

    let mut file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(_) => return -1,
    };
    if file.write_all(text.as_bytes()).is_err() {
        return -1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_record_separators() {
        assert_eq!(sanitize_name("plain"), "plain");
        assert_eq!(sanitize_name("a\tb\nc"), "a b c");
        // Only the record delimiters are rewritten.
        assert_eq!(sanitize_name("a\rb"), "a\rb");
    }
}
