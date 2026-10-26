//! Integration tests for the process-global function-entry profiler ABI.
//!
//! All tests share one process-global counter map, so they serialise through
//! [`LOCK`] and start with `hydro_profile_reset`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hydro_runtime::abi;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn hit(name: &str) {
    abi::hydro_profile_hit(name.as_ptr(), name.len());
}

fn dump(path: &Path) -> i32 {
    let text = path.to_str().expect("temporary path is valid UTF-8");
    abi::hydro_profile_dump(text.as_ptr(), text.len())
}

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hydro-profile-test-{}-{label}.txt",
        std::process::id()
    ))
}

#[test]
fn hit_reset_dump_roundtrip() {
    let _guard = lock();
    abi::hydro_profile_reset();

    hit("beta");
    hit("beta");
    hit("alpha");

    let path = temp_path("roundtrip");
    assert_eq!(dump(&path), 0);
    let text = std::fs::read_to_string(&path).expect("dump file is readable");
    assert_eq!(text, "# hydro-profile-v1\n1\talpha\n2\tbeta\n");

    // Reset clears every counter, including absent ones.
    abi::hydro_profile_reset();
    assert_eq!(dump(&path), 0);
    let text = std::fs::read_to_string(&path).expect("dump file is readable");
    assert_eq!(text, "# hydro-profile-v1\n");

    let _ = std::fs::remove_file(path);
}

#[test]
fn dump_sanitizes_record_separators() {
    let _guard = lock();
    abi::hydro_profile_reset();

    hit("hot\tfn\nname");
    hit("z");

    let path = temp_path("sanitize");
    assert_eq!(dump(&path), 0);
    let text = std::fs::read_to_string(&path).expect("dump file is readable");
    assert_eq!(text, "# hydro-profile-v1\n1\thot fn name\n1\tz\n");

    let _ = std::fs::remove_file(path);
}

#[test]
fn malformed_paths_and_names_never_panic() {
    let _guard = lock();
    abi::hydro_profile_reset();

    // Null pointer with a non-zero length: ignored, counter map untouched.
    abi::hydro_profile_hit(std::ptr::null(), 5);
    // Non-UTF-8 name: ignored.
    let invalid_name = [0xff_u8, 0xfe];
    abi::hydro_profile_hit(invalid_name.as_ptr(), invalid_name.len());
    // Non-UTF-8 path: -1.
    let invalid = [0xff_u8, 0xfe];
    assert_eq!(abi::hydro_profile_dump(invalid.as_ptr(), invalid.len()), -1);
    // Null path pointer: -1.
    assert_eq!(abi::hydro_profile_dump(std::ptr::null(), 3), -1);
    // Missing parent directory: -1.
    let missing = temp_path("missing/child");
    assert_eq!(dump(&missing), -1);

    // The failed calls left the (empty) profile intact and dumpable.
    let path = temp_path("after-errors");
    assert_eq!(dump(&path), 0);
    let text = std::fs::read_to_string(&path).expect("dump file is readable");
    assert_eq!(text, "# hydro-profile-v1\n");
    let _ = std::fs::remove_file(path);
}
