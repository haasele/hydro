//! End-to-end precise-GC test through the `pyx` binary (P2-06/P2-07).
//!
//! Runs a compiled program that allocates a known amount of short-lived
//! temporaries under `PYX_GC_PRECISE=1` (strict precise-root mode) and a small
//! collection threshold, then reads the collector report emitted by
//! `pyx gc --json`:
//!
//! * the program's output must be exact, proving its live locals survived
//!   every collection;
//! * the report must say `precise: true` (strict mode was really active);
//! * `freed_total` must be non-zero and `live_objects` must stay small,
//!   proving dead temporaries were reclaimed rather than retained by their
//!   stale shadow-frame slots or by conservative stack scanning.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_source(test_name: &str, source: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pyx-precise-gc-{}-{test_name}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    let path = dir.join("main.py");
    std::fs::write(&path, source).expect("failed to write source");
    path
}

fn run_with_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("failed to spawn pyx")
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
}

/// Reads one unsigned integer field of the JSON report line.
fn json_int(report: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("`{key}` missing from report: {report}"))
        + needle.len();
    let digits: String = report[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    assert!(!digits.is_empty(), "`{key}` is not an integer: {report}");
    digits.parse().expect("integer field")
}

fn json_bool(report: &str, key: &str) -> bool {
    let needle = format!("\"{key}\":");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("`{key}` missing from report: {report}"))
        + needle.len();
    report[start..].starts_with("true")
}

#[test]
fn generated_program_reclaims_dead_temporaries_in_strict_mode() {
    let source = "\
def make_pair(n):
    return [n, n + 1]

total = 0
for i in range(200):
    pair = make_pair(i)
    total += pair[1]
print(total)
";
    let path = write_source("dead-temp", source);
    let output = run_with_env(
        &["gc", "--json", path.to_str().expect("UTF-8 temp path")],
        &[("PYX_GC_PRECISE", "1"), ("PYX_GC_THRESHOLD", "16")],
    );
    cleanup(&path);

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "pyx gc failed: {stderr}");

    // Program output (locals survived) precedes the JSON report line.
    let trimmed = stdout.trim_end_matches('\n');
    let report_start = trimmed.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let report = &trimmed[report_start..];
    assert!(
        report.starts_with('{') && report.ends_with('}'),
        "last stdout line is not the JSON report: {report}"
    );
    let program_output = trimmed[..report_start].trim_end();
    assert_eq!(
        program_output, "20100",
        "locals did not survive strict collection:\n{stdout}"
    );

    assert!(json_bool(report, "precise"), "strict mode not reported");
    let freed = json_int(report, "freed_total");
    let live = json_int(report, "live_objects");
    let collections = json_int(report, "collections");
    assert!(collections > 0, "no collection ran: {report}");
    assert!(freed > 0, "no dead temporary was reclaimed: {report}");
    assert!(
        live < 200,
        "{} objects stayed live out of 200 temporary pairs: {report}",
        live
    );
}
