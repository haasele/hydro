//! End-to-end tests for the second slice of the standard-library program
//! (`docs/TODO.md`, `P7`): every module in native Batch B imported and
//! exercised through `pyx run`.
//!
//! The program covers `binascii`, `base64`, `quopri`, `fnmatch`, `getopt` and
//! `mimetypes`. It depends only on the native implementations, never on a
//! CPython source tree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An isolated directory for a single test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-stdlib-batch-b-{}-{test_name}-{unique}",
            std::process::id()
        ));
        // Remove stale state from an interrupted previous run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("write program");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Renders `path` as a Python string literal for the generated program.
fn python_literal(path: &Path) -> String {
    let text = path.to_str().expect("UTF-8 temp path");
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            _ => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

fn run(path: &Path) -> (i32, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(["run", path.to_str().expect("UTF-8 program path")])
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The Batch B program. The `__Q_*__` placeholders are replaced with fresh
/// temp paths so the `quopri` file round trip never touches shared files.
const PROGRAM: &str = r#"import binascii
import base64
import quopri
import fnmatch
import getopt
import mimetypes


print(binascii.hexlify(b"pyx"))
print(binascii.b2a_hex(b"pyx"))
print(binascii.hexlify(b"abcd", b"-"))
print(binascii.unhexlify(b"707978"))
print(binascii.unhexlify(binascii.hexlify(b"round")))
print(binascii.crc32(b"pyx"))
print(binascii.adler32(b"pyx"))
encoded = binascii.b2a_base64(b"hello")
print(encoded)
print(binascii.a2b_base64(encoded))

print(base64.b64encode(b"hello"))
print(base64.b64decode(b"aGVsbG8="))
print(base64.standard_b64decode(base64.standard_b64encode(b"pyx!")))
print(base64.urlsafe_b64encode(b"\xfb\xff"))
print(base64.urlsafe_b64decode(b"-_8="))
print(base64.encodebytes(b"hello\nworld\n"))
print(base64.decodebytes(base64.encodebytes(b"hello\nworld\n")))
print(base64.b16encode(b"pyx"))
print(base64.b16decode(b"707978"))
print(base64.b32encode(b"pyx"))
print(base64.b32decode(base64.b32encode(b"pyx")))

print(quopri.encodestring(b"hello =world\nsec\tond\n"))
print(quopri.decodestring(b"hello =3Dworld\nsec\tond\n"))

handle = open(__Q_IN__, "wb")
handle.write(b"hello =world\nplain line\n")
handle.close()
src = open(__Q_IN__, "rb")
dst = open(__Q_OUT__, "wb")
quopri.encode(src, dst, False)
src.close()
dst.close()
handle = open(__Q_OUT__, "rb")
encoded = handle.read()
handle.close()
print(encoded)
src = open(__Q_OUT__, "rb")
dst = open(__Q_DEC__, "wb")
quopri.decode(src, dst)
src.close()
dst.close()
handle = open(__Q_DEC__, "rb")
print(handle.read())
handle.close()

print(fnmatch.fnmatch("a.py", "*.py"), fnmatch.fnmatch("a.py", "*.PY"))
print(fnmatch.fnmatchcase("b.txt", "[a-c].txt"), fnmatch.fnmatchcase("d.txt", "[!a-c].txt"))
print(fnmatch.fnmatchcase("ab", "a?"), fnmatch.fnmatchcase("a*b", "a[*]b"))
print(fnmatch.filter(["a.py", "b.txt", "c.py"], "*.py"))
print(fnmatch.filterfalse(["a.py", "b.txt", "c.py"], "*.py"))
translated = fnmatch.translate("*.py")
print(len(translated) > 0, translated.startswith("(?s:"))

print(getopt.getopt(["-a", "-b", "value", "--name=x", "--flag", "tail"], "ab:", ["name=", "flag"]))
print(getopt.getopt(["-a", "--", "-b", "x"], "ab:"))
print(getopt.gnu_getopt(["tail", "-a"], "a"))

print(mimetypes.guess_type("x.html"))
print(mimetypes.guess_type("x.tar.gz"))
print(mimetypes.guess_type("x.tgz"))
print(mimetypes.guess_extension("application/x-tar"))
print(mimetypes.guess_all_extensions("application/x-tar"))
mimetypes.add_type("application/x-pyx", ".pyx", strict=True)
print(mimetypes.guess_type("x.pyx"))
print(mimetypes.guess_extension("application/x-pyx"))
print(len(mimetypes.types_map) > 0, len(mimetypes.common_types) > 0, len(mimetypes.encodings_map) > 0, len(mimetypes.suffix_map) > 0)
"#;

/// Stdout of [`PROGRAM`] with fresh temp files.
const EXPECTED_STDOUT: &str = "b'707978'\n\
     b'707978'\n\
     b'61-62-63-64'\n\
     b'pyx'\n\
     b'round'\n\
     194218739\n\
     45941090\n\
     b'aGVsbG8=\\n'\n\
     b'hello'\n\
     b'aGVsbG8='\n\
     b'hello'\n\
     b'pyx!'\n\
     b'-_8='\n\
     b'\\xfb\\xff'\n\
     b'aGVsbG8Kd29ybGQK\\n'\n\
     b'hello\\nworld\\n'\n\
     b'707978'\n\
     b'pyx'\n\
     b'OB4XQ==='\n\
     b'pyx'\n\
     b'hello =3Dworld\\nsec\\tond\\n'\n\
     b'hello =world\\nsec\\tond\\n'\n\
     b'hello =3Dworld\\nplain line\\n'\n\
     b'hello =world\\nplain line\\n'\n\
     True False\n\
     True True\n\
     True True\n\
     ['a.py', 'c.py']\n\
     ['b.txt']\n\
     True True\n\
     ([('-a', ''), ('-b', 'value'), ('--name', 'x'), ('--flag', '')], ['tail'])\n\
     ([('-a', '')], ['-b', 'x'])\n\
     ([('-a', '')], ['tail'])\n\
     ('text/html', None)\n\
     ('application/x-tar', 'gzip')\n\
     ('application/x-tar', 'gzip')\n\
     .tar\n\
     ['.tar']\n\
     ('application/x-pyx', None)\n\
     .pyx\n\
     True True True True\n";

#[test]
fn batch_b_modules_run_end_to_end() {
    let dir = TestDir::new("end-to-end");
    let source = PROGRAM
        .replace("__Q_IN__", &python_literal(&dir.path.join("quopri_in.txt")))
        .replace(
            "__Q_OUT__",
            &python_literal(&dir.path.join("quopri_out.txt")),
        )
        .replace(
            "__Q_DEC__",
            &python_literal(&dir.path.join("quopri_dec.txt")),
        );
    let program = dir.write("batch_b.py", &source);
    let (code, stdout, stderr) = run(&program);
    assert_eq!(code, 0, "batch B program must exit 0:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "stderr:\n{stderr}");
}

/// Odd-length hex must surface the native error instead of a silent default.
const FAILING_PROGRAM: &str = r#"import binascii

binascii.unhexlify(b"abc")
"#;

#[test]
fn batch_b_modules_report_errors() {
    let dir = TestDir::new("error");
    let program = dir.write("bad_hex.py", FAILING_PROGRAM);
    let (code, _stdout, stderr) = run(&program);
    assert_ne!(
        code, 0,
        "odd-length unhexlify must exit non-zero; stderr:\n{stderr}"
    );
}
