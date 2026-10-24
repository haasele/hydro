//! End-to-end test for a multi-phase (`PyModuleDef_Init`) abi3 extension.
//!
//! The C fixture returns a `PyModuleDef` from its `PyInit_*` entry point and
//! adds an attribute from a `Py_mod_exec` slot. PyX must create the module
//! from the definition, run the slot and then behave exactly like it does for
//! a single-phase module, under both `pyx run` and an AOT-linked binary.
//! Hosts without a C toolchain or CPython development headers skip cleanly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Console output of the probe program, in call order.
const EXPECTED_STDOUT: &str = "\
multiphase
42
multiphase
42
42
";

const MAIN_SOURCE: &str = "\
import pyx_multiphase_probe as p
print(p.tag)
print(p.answer())

from pyx_multiphase_probe import answer, tag

print(tag)
print(answer())
print(answer())
";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-extensions-multiphase-{}-{test_name}-{unique}",
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

/// C toolchain and Python headers needed to build the probe extension.
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
                eprintln!("skipping multi-phase extension test: `cc` is not available");
                return None;
            }
        }

        let output = match Command::new("python3-config").arg("--includes").output() {
            Ok(output) if output.status.success() => output,
            _ => {
                eprintln!("skipping multi-phase extension test: `python3-config` is not available");
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
            eprintln!("skipping multi-phase extension test: `python3-config --includes` is empty");
            return None;
        }

        let headers_found = includes.iter().any(|include| {
            Path::new(include.trim_start_matches("-I"))
                .join("Python.h")
                .is_file()
        });
        if !headers_found {
            eprintln!("skipping multi-phase extension test: Python.h is not installed");
            return None;
        }

        Some(Toolchain { includes })
    }

    /// Compiles the multi-phase probe into the temp dir. No `libpython` is
    /// linked: the module only uses the stable C API, which the host process
    /// provides at run time.
    fn build_probe(&self, dir: &TestDir) -> PathBuf {
        let source = dir.path().join("abi3_multiphase.c");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/abi3_multiphase.c");
        std::fs::copy(&fixture, &source).expect("copy extension fixture");

        let module = dir.path().join("pyx_multiphase_probe.abi3.so");
        let mut command = Command::new("cc");
        command.arg("-shared").arg("-fPIC");
        for include in &self.includes {
            command.arg(include);
        }
        command.arg("-o").arg(&module).arg(&source);

        let output = command.output().expect("failed to spawn cc");
        assert!(
            output.status.success(),
            "failed to compile the multi-phase abi3 probe:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        module
    }
}

fn output_parts(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs the `pyx` binary inside `dir` and returns `(code, stdout, stderr)`.
fn run_pyx(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn pyx");
    output_parts(output)
}

fn run_executable(path: &Path, dir: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .current_dir(dir)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

fn runtime_lib_present() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libpyx_runtime.a").is_file())
}

/// Fails when `binary` dynamically links `libpython`; skipped when `ldd` is
/// unavailable.
#[cfg(target_os = "linux")]
fn assert_no_libpython(binary: &Path) {
    let output = match Command::new("ldd").arg(binary).output() {
        Ok(output) if output.status.success() => output,
        _ => {
            eprintln!(
                "skipping libpython check: `ldd` is unavailable for '{}'",
                binary.display()
            );
            return;
        }
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.contains("libpython"),
        "'{}' links libpython:\n{listing}",
        binary.display()
    );
}

#[cfg(not(target_os = "linux"))]
fn assert_no_libpython(_binary: &Path) {}

#[test]
fn multiphase_api_extension_runs_jit_and_aot() {
    let Some(toolchain) = Toolchain::detect() else {
        return;
    };
    let dir = TestDir::new("multiphase");
    toolchain.build_probe(&dir);
    std::fs::write(dir.path().join("main.py"), MAIN_SOURCE).expect("write main.py");

    // JIT: `pyx run` loads the extension and runs its exec slot.
    let (code, stdout, stderr) = run_pyx(dir.path(), &["run", "main.py"]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "unexpected JIT output:\n{stderr}");
    assert_no_libpython(Path::new(env!("CARGO_BIN_EXE_pyx")));

    // AOT: the linked binary exports its dynamic symbols so the extension
    // resolves the CPython C API against the executable itself.
    if !runtime_lib_present() {
        eprintln!(
            "skipping AOT multi-phase extension test: libpyx_runtime.a not present in the test profile"
        );
        return;
    }
    let program = dir.path().join("program");
    let program_arg = program.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run_pyx(
        dir.path(),
        &["build", "main.py", "-o", program_arg.as_str(), "--no-cache"],
    );
    assert_eq!(code, 0, "pyx build failed:\n{stderr}");
    let (code, stdout, stderr) = run_executable(&program, dir.path());
    assert_eq!(code, 0, "AOT binary failed:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "unexpected AOT output:\n{stderr}");
    assert_no_libpython(&program);
}
