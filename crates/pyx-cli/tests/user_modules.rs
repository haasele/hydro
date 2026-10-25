//! End-to-end tests for imports of sibling user modules (`foo.py`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-user-modules-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("write module source");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_executable(path: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .output()
        .expect("failed to spawn built executable");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn runtime_lib_present() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libpyx_runtime.a").is_file())
}

fn project(test_name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::new(test_name);
    dir.write("helper.py", "def add(a, b):\n    return a + b\n");
    let main = dir.write(
        "main.py",
        "import helper\nfrom helper import add as plus\nprint(helper.add(1, 2), plus(3, 4))\n",
    );
    (dir, main)
}

#[test]
fn jit_runs_sibling_module_imports() {
    let (_dir, main) = project("jit");
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, "3 7\n");
}

#[test]
fn aot_binary_contains_sibling_module_functions() {
    if !runtime_lib_present() {
        eprintln!("skipping: libpyx_runtime.a not present in the test profile");
        return;
    }
    let (dir, main) = project("aot");
    let main = main.to_string_lossy().into_owned();
    let binary = dir.path().join("program");
    let binary_arg = binary.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run(&["build", main.as_str(), "-o", binary_arg.as_str()]);
    assert_eq!(code, 0, "pyx build failed:\n{stderr}");
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "AOT binary failed:\n{stderr}");
    assert_eq!(stdout, "3 7\n");
}

#[test]
fn nested_module_imports_run() {
    let dir = TestDir::new("nested");
    dir.write("base.py", "def inc(value):\n    return value + 1\n");
    dir.write(
        "helper.py",
        "from base import inc\n\ndef add(a, b):\n    return inc(a) + b\n",
    );
    let main = dir.write("main.py", "import helper\nprint(helper.add(1, 2))\n");
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, "4\n");
}

#[test]
fn check_reports_missing_module() {
    let dir = TestDir::new("missing");
    let main = dir.write("main.py", "import nowhere\nprint(1)\n");
    let main = main.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run(&["check", main.as_str()]);
    assert_eq!(code, 1, "missing module must fail:\n{stderr}");
    assert!(
        stderr.contains("module 'nowhere' not found"),
        "missing diagnostic:\n{stderr}"
    );
    assert!(stderr.contains("main.py"), "missing file name:\n{stderr}");
}

#[test]
fn check_reports_circular_import() {
    let dir = TestDir::new("cycle");
    dir.write("a.py", "from b import g\n\ndef f():\n    return 1\n");
    dir.write("b.py", "from a import f\n\ndef g():\n    return 2\n");
    let main = dir.write("main.py", "import a\nprint(a.f())\n");
    let main = main.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run(&["check", main.as_str()]);
    assert_eq!(code, 1, "cycle must fail:\n{stderr}");
    assert!(
        stderr.contains("circular import"),
        "missing cycle diagnostic:\n{stderr}"
    );
}

#[test]
fn function_local_import_not_called_runs() {
    let dir = TestDir::new("deferred-import-uncalled");
    dir.write("helper.py", "def add(a, b):\n    return a + b\n");
    let main = dir.write(
        "main.py",
        "def run():\n    from helper import add\n    return add(1, 2)\n\nprint('ok')\n",
    );
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, "ok\n");
}

#[test]
fn function_local_import_call_raises_at_runtime() {
    let dir = TestDir::new("deferred-import");
    dir.write("helper.py", "def add(a, b):\n    return a + b\n");
    let main = dir.write(
        "main.py",
        "def run():\n    from helper import add\n    return add(1, 2)\n\nprint(run())\n",
    );
    let main = main.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run(&["run", main.as_str()]);
    assert_ne!(code, 0, "the deferred call must fail:\n{stderr}");
    assert!(
        stderr.contains("RuntimeError: dynamic import of 'helper.add' is not supported"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn top_level_import_still_initializes_next_to_a_nested_import() {
    let dir = TestDir::new("deferred-top-level");
    dir.write("helper.py", "VALUE = 5\n");
    let main = dir.write(
        "main.py",
        "import helper\n\ndef unused():\n    from nowhere import thing\n    return thing()\n\nprint(helper.VALUE)\n",
    );
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, "5\n");
}

#[test]
fn function_local_package_import_call_raises_at_runtime() {
    let dir = TestDir::new("deferred-package");
    std::fs::create_dir_all(dir.path().join("pkg")).expect("create package dir");
    dir.write("pkg/__init__.py", "");
    dir.write("pkg/sub.py", "def value():\n    return 7\n");
    let main = dir.write(
        "main.py",
        "def run():\n    from pkg import sub\n    return sub.value()\n\nprint(run())\n",
    );
    let main = main.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = run(&["run", main.as_str()]);
    assert_ne!(code, 0, "the deferred call must fail:\n{stderr}");
    assert!(
        stderr.contains("RuntimeError: dynamic import of 'pkg.sub.value' is not supported"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn submodule_imports_initializing_package_member_at_runtime() {
    let dir = TestDir::new("initializing-parent");
    std::fs::create_dir_all(dir.path().join("pkg")).expect("create package dir");
    dir.write(
        "pkg/__init__.py",
        "from .child import run\n\ndef greeting():\n    return 'hi'\n",
    );
    dir.write(
        "pkg/child.py",
        "from . import greeting\n\ndef run():\n    return greeting()\n",
    );
    let main = dir.write("main.py", "from pkg.child import run\nprint(run())\n");
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 0, "pyx run failed:\n{stderr}");
    assert_eq!(stdout, "hi\n");
}

#[test]
fn module_initializer_exception_stops_before_entry_body() {
    let dir = TestDir::new("initializer-error");
    dir.write("bad.py", "raise ValueError(\"boom\")\n");
    let main = dir.write("main.py", "import bad\nprint(\"after\")\n");
    let main = main.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", main.as_str()]);
    assert_eq!(code, 1, "initializer error must fail:\n{stderr}");
    assert_eq!(stdout, "", "the entry body must not run after the failure");
    assert!(
        stderr.contains("ValueError: boom"),
        "the original initializer error must be reported, stderr:\n{stderr}"
    );
}
