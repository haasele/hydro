//! End-to-end tests for dotted compiled-extension imports.
//!
//! A wheel-style extension inside a package directory (`pkg/sub_ext.abi3.so`)
//! must be importable without any package Python code. Shared objects whose
//! ABI tag PyX cannot load must fail at compile time, before any `dlopen`.
//! Hosts without a C toolchain or CPython development headers skip cleanly.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAIN_SOURCE: &str = "\
from pkg.sub_ext import add, tag
import pkg.sub_ext as s

print(add(2, 3))
print(tag)
print(s.add(4, 5))
print(s.tag)
";

const EXPECTED_STDOUT: &str = "\
5
abi3
9
abi3
";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-extensions-dotted-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// C toolchain needed to build the probe extension from source.
struct Toolchain {
    includes: Vec<String>,
}

impl Toolchain {
    /// Locates `cc`, `python3-config --includes` and `Python.h`; returns
    /// `None` (with a printed reason) when the host cannot build the probe.
    fn detect() -> Option<Toolchain> {
        match Command::new("cc").arg("--version").output() {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("skipping dotted extension test: `cc` is not available");
                return None;
            }
        }

        let output = match Command::new("python3-config").arg("--includes").output() {
            Ok(output) if output.status.success() => output,
            _ => {
                eprintln!("skipping dotted extension test: `python3-config` is not available");
                return None;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let includes: Vec<String> = stdout
            .split_whitespace()
            .filter(|token| token.starts_with("-I"))
            .map(str::to_owned)
            .collect();
        if includes.is_empty() {
            eprintln!("skipping dotted extension test: `python3-config --includes` is empty");
            return None;
        }

        let headers_found = includes.iter().any(|include| {
            Path::new(include.trim_start_matches("-I"))
                .join("Python.h")
                .is_file()
        });
        if !headers_found {
            eprintln!("skipping dotted extension test: Python.h is not installed");
            return None;
        }

        Some(Toolchain { includes })
    }

    /// Compiles the limited-API probe as `pkg/sub_ext.abi3.so`, renaming its
    /// init function to match the leaf component (`PyInit_sub_ext`).
    fn build_package_probe(&self, dir: &TestDir) -> PathBuf {
        let source = dir.path().join("abi3_probe.c");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/abi3_probe.c");
        std::fs::copy(&fixture, &source).expect("copy extension fixture");

        let package = dir.path().join("pkg");
        std::fs::create_dir_all(&package).expect("create package dir");
        let module = package.join("sub_ext.abi3.so");
        let mut command = Command::new("cc");
        command.arg("-shared").arg("-fPIC");
        for include in &self.includes {
            command.arg(include);
        }
        command
            .arg("-DPyInit_pyx_capi_probe=PyInit_sub_ext")
            .arg("-o")
            .arg(&module)
            .arg(&source);

        let output = command.output().expect("failed to spawn cc");
        assert!(
            output.status.success(),
            "failed to compile the abi3 probe:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        module
    }
}

/// Runs the `pyx` binary inside `dir` and returns `(code, stdout, stderr)`.
fn run_pyx(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn dotted_abi3_extension_runs_without_package_source() {
    let Some(toolchain) = Toolchain::detect() else {
        return;
    };
    let dir = TestDir::new("abi3");
    toolchain.build_package_probe(&dir);
    assert!(
        !dir.path().join("pkg/__init__.py").exists(),
        "the package must be importable without Python source"
    );
    std::fs::write(dir.path().join("main.py"), MAIN_SOURCE).expect("write main.py");

    let (code, stdout, stderr) = run_pyx(dir.path(), &["run", "main.py"]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "unexpected output:\n{stderr}");
}

#[test]
fn incompatible_wheel_tag_fails_before_dlopen() {
    let Some(_toolchain) = Toolchain::detect() else {
        return;
    };
    let dir = TestDir::new("bad-tag");
    std::fs::write(dir.path().join("bad.cpython-312-x86_64-linux-gnu.so"), b"")
        .expect("write incompatible extension stub");
    std::fs::write(dir.path().join("main.py"), "import bad as b\n").expect("write main.py");

    let (code, stdout, stderr) = run_pyx(dir.path(), &["run", "main.py"]);
    assert_ne!(code, 0, "incompatible extension must fail:\n{stderr}");
    assert!(
        stderr.contains("incompatible"),
        "diagnostic must mention incompatibility:\n{stderr}"
    );
    assert!(
        stderr.contains("cpython-312-x86_64-linux-gnu"),
        "diagnostic must name the offending tag:\n{stderr}"
    );
    assert!(
        !stderr.contains("dlopen") && !stderr.contains("cannot open shared object"),
        "the incompatible library must not be dlopen'd:\n{stderr}"
    );
    assert_eq!(stdout, "", "nothing should run:\n{stderr}");
}
