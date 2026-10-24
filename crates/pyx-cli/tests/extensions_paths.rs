//! End-to-end tests for extension-module discovery through environment paths.
//!
//! An abi3 probe built outside the source directory must still be importable
//! via `PYX_EXTENSION_PATH`, `PYTHONPATH`, `PYX_SITE_PACKAGES` and a virtual
//! environment's `site-packages`. Hosts without a C toolchain or CPython
//! development headers skip cleanly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Environment variables the CLI reads while resolving extension modules.
const SEARCH_ENV: [&str; 5] = [
    "PYX_EXTENSION_PATH",
    "PYTHONPATH",
    "PYX_SITE_PACKAGES",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
];

const EXPECTED_STDOUT: &str = "5\nabi3\n";

const MAIN_SOURCE: &str = "\
import pyx_capi_probe as p
print(p.add(2, 3))
print(p.tag)
";

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-extensions-paths-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    /// Creates a subdirectory and returns it.
    fn subdir(&self, relative: &str) -> PathBuf {
        let path = self.path.join(relative);
        std::fs::create_dir_all(&path).expect("create subdirectory");
        path
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
    fn detect() -> Option<Toolchain> {
        match Command::new("cc").arg("--version").output() {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("skipping extension path test: `cc` is not available");
                return None;
            }
        }

        let output = match Command::new("python3-config").arg("--includes").output() {
            Ok(output) if output.status.success() => output,
            _ => {
                eprintln!("skipping extension path test: `python3-config` is not available");
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
            eprintln!("skipping extension path test: `python3-config --includes` is empty");
            return None;
        }

        let headers_found = includes.iter().any(|include| {
            Path::new(include.trim_start_matches("-I"))
                .join("Python.h")
                .is_file()
        });
        if !headers_found {
            eprintln!("skipping extension path test: Python.h is not installed");
            return None;
        }

        Some(Toolchain { includes })
    }

    /// Builds the limited-API probe into `dir`, away from the importing
    /// source file. No `libpython` is linked.
    fn build_probe(&self, dir: &Path) -> PathBuf {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/abi3_probe.c");
        let module = dir.join("pyx_capi_probe.abi3.so");
        let mut command = Command::new("cc");
        command.arg("-shared").arg("-fPIC");
        for include in &self.includes {
            command.arg(include);
        }
        command.arg("-o").arg(&module).arg(&fixture);

        let output = command.output().expect("failed to spawn cc");
        assert!(
            output.status.success(),
            "failed to compile the abi3 probe:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        module
    }
}

/// Runs `pyx run main.py` inside `dir` with a controlled environment: every
/// extension search variable is cleared first, then `envs` is applied.
fn run_pyx(dir: &Path, envs: &[(&str, &OsStr)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command.args(["run", "main.py"]).current_dir(dir);
    for key in SEARCH_ENV {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn write_main(dir: &Path, source: &str) {
    std::fs::write(dir.join("main.py"), source).expect("write main.py");
}

fn assert_runs(dir: &Path, envs: &[(&str, &OsStr)], case: &str) {
    let (code, stdout, stderr) = run_pyx(dir, envs);
    assert_eq!(code, 0, "{case} failed:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "{case} produced unexpected output");
}

#[test]
fn extension_search_paths_are_honored() {
    let Some(toolchain) = Toolchain::detect() else {
        return;
    };
    let dir = TestDir::new("env");
    let lib = dir.subdir("lib");
    toolchain.build_probe(&lib);
    let lib_path = lib.as_os_str();

    // (a) PYX_EXTENSION_PATH, with the extension outside the project dir.
    let project = dir.subdir("project");
    write_main(&project, MAIN_SOURCE);
    assert_runs(
        &project,
        &[("PYX_EXTENSION_PATH", lib_path)],
        "PYX_EXTENSION_PATH import",
    );

    // (b) PYTHONPATH.
    let project = dir.subdir("project-pythonpath");
    write_main(&project, MAIN_SOURCE);
    assert_runs(&project, &[("PYTHONPATH", lib_path)], "PYTHONPATH import");

    // (c) An explicit site-packages override.
    let project = dir.subdir("project-site-packages");
    write_main(&project, MAIN_SOURCE);
    assert_runs(
        &project,
        &[("PYX_SITE_PACKAGES", lib_path)],
        "PYX_SITE_PACKAGES import",
    );

    // (d) A virtual environment's site-packages, discovered from VIRTUAL_ENV.
    let venv = dir.subdir("venv");
    let venv_site = venv.join("lib").join("python3.14").join("site-packages");
    std::fs::create_dir_all(&venv_site).expect("create venv site-packages");
    std::fs::copy(
        lib.join("pyx_capi_probe.abi3.so"),
        venv_site.join("pyx_capi_probe.abi3.so"),
    )
    .expect("copy extension into venv");
    let project = dir.subdir("project-venv");
    write_main(&project, MAIN_SOURCE);
    assert_runs(
        &project,
        &[("VIRTUAL_ENV", venv.as_os_str())],
        "VIRTUAL_ENV import",
    );

    // (e) Without any configured path the module is still diagnosed as
    // missing, so the search additions never mask a real error.
    let project = dir.subdir("project-missing");
    write_main(&project, "import missing_extension_module\n");
    let (code, _stdout, stderr) = run_pyx(&project, &[]);
    assert_ne!(code, 0, "missing extension import unexpectedly succeeded");
    assert!(
        stderr.contains("module 'missing_extension_module' not found"),
        "missing extension was not diagnosed:\n{stderr}"
    );
}
