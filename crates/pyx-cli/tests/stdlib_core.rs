//! End-to-end acceptance tests for the P2-13 core standard-library slice:
//! `math`, `time`, `random`, `json`, `re`, `os`/`os.path`/`pathlib` and
//! `io`/`csv` must *behave*, not merely import.
//!
//! Each test runs a self-contained program through `pyx run`, pins the exact
//! stdout, and then drives a second, failing program to prove the module
//! surfaces its native error instead of returning a silent default. No test
//! depends on a CPython source tree; float results are checked with an
//! epsilon, and the seeded `random` values follow PyX's documented SplitMix64
//! generator rather than CPython's Mersenne Twister.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An isolated directory for a single test, removed on drop.
///
/// The generated program lives next to (never inside) the `work` directory the
/// program itself receives as `BASE`, so filesystem tests can assert on that
/// directory's exact contents.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-stdlib-core-{}-{test_name}-{unique}",
            std::process::id()
        ));
        // Remove stale state from an interrupted previous run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("work")).expect("create temp dir");
        TestDir { path }
    }

    /// Scratch directory handed to the program as `BASE`.
    fn work(&self) -> PathBuf {
        self.path.join("work")
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

/// Writes `source`, substituting `__BASE__` with the test's work directory,
/// runs it and pins the exit status and stdout.
fn assert_ok(dir: &TestDir, name: &str, source: &str, expected_stdout: &str) {
    let source = source.replace("__BASE__", &python_literal(&dir.work()));
    let program = dir.write(name, &source);
    let (code, stdout, stderr) = run(&program);
    assert_eq!(code, 0, "{name}: pyx run must exit 0; stderr:\n{stderr}");
    assert_eq!(
        stdout, expected_stdout,
        "{name}: unexpected stdout; stderr:\n{stderr}"
    );
}

/// Runs a deliberately failing program and requires the native error text.
fn assert_failure(dir: &TestDir, name: &str, source: &str, needle: &str) {
    let source = source.replace("__BASE__", &python_literal(&dir.work()));
    let program = dir.write(name, &source);
    let (code, _stdout, stderr) = run(&program);
    assert_ne!(
        code, 0,
        "{name}: the program must exit non-zero; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(needle),
        "{name}: stderr must contain {needle:?}; got:\n{stderr}"
    );
}

// -------------------------------------------------------------------------
// math
// -------------------------------------------------------------------------

const MATH_PROGRAM: &str = r##"import math


def close(left, right):
    return abs(left - right) < 1e-9


print(close(math.sqrt(9), 3.0), close(math.sqrt(2), 1.4142135623730951))
print(close(math.pow(2, 10), 1024.0), close(math.pow(2, -1), 0.5))
print(math.floor(2.7), math.floor(-2.1), math.ceil(2.1), math.ceil(-2.7), math.floor(7))
print(math.gcd(12, 18), math.gcd(0, 5), math.gcd(12, 18, 24), math.gcd())
print(close(math.pi, 3.141592653589793), close(math.e, 2.718281828459045))
print(close(math.tau, 6.283185307179586), close(math.tau, 2.0 * math.pi))
print(math.inf > 1e308, math.nan != math.nan)
print(close(math.pow(9, 0.5), math.sqrt(9)))
"##;

const MATH_STDOUT: &str = r##"True True
True True
2 -3 3 -2 7
6 5 6 0
True True
True True
True True
True
"##;

/// A negative `sqrt` must raise `ValueError`; a non-number argument must raise
/// the native `TypeError`.
const MATH_DOMAIN_PROGRAM: &str = r##"import math

math.sqrt(-1)
"##;

const MATH_TYPE_PROGRAM: &str = r##"import math

math.sqrt("9")
"##;

#[test]
fn math_functions_and_constants_behave() {
    let dir = TestDir::new("math");
    assert_ok(&dir, "math_core.py", MATH_PROGRAM, MATH_STDOUT);
    assert_failure(
        &dir,
        "math_domain.py",
        MATH_DOMAIN_PROGRAM,
        "ValueError: math domain error",
    );
    assert_failure(
        &dir,
        "math_type.py",
        MATH_TYPE_PROGRAM,
        "TypeError: must be real number, not str",
    );
}

// -------------------------------------------------------------------------
// time
// -------------------------------------------------------------------------

const TIME_PROGRAM: &str = r##"import time

start = time.perf_counter()
time.sleep(0.05)
elapsed = time.perf_counter() - start
print(elapsed >= 0.04)
print(time.perf_counter() >= start)

before = time.monotonic()
time.sleep(0.01)
after = time.monotonic()
print(after > before)
print(after - before >= 0.005)

print(time.time() > 1000000000.0)
print(time.time_ns() > 0)
print(time.perf_counter_ns() >= 0)
print(time.monotonic_ns() >= 0)
print(time.sleep(0) == None)
"##;

const TIME_STDOUT: &str = r##"True
True
True
True
True
True
True
True
True
"##;

const TIME_FAILING_PROGRAM: &str = r##"import time

time.sleep(-0.01)
"##;

#[test]
fn time_clocks_advance_and_sleep_blocks() {
    let dir = TestDir::new("time");
    assert_ok(&dir, "time_core.py", TIME_PROGRAM, TIME_STDOUT);
    assert_failure(
        &dir,
        "time_negative_sleep.py",
        TIME_FAILING_PROGRAM,
        "ValueError: sleep length must be non-negative",
    );
}

// -------------------------------------------------------------------------
// random
// -------------------------------------------------------------------------

/// The expected integers are PyX's SplitMix64 stream for seed 2026; the same
/// seed in two independent generators must reproduce them exactly.
const RANDOM_PROGRAM: &str = r##"import random

first = random.Random(2026)
a = first.randint(1, 100)
b = first.randint(1, 100)
c = first.randrange(0, 50, 5)
d = first.randrange(10)
pick = first.choice(["red", "green", "blue"])
items = [1, 2, 3, 4, 5]
first.shuffle(items)
print(a, b, c, d, pick)
print(items)

second = random.Random(2026)
print(second.randint(1, 100) == a, second.randint(1, 100) == b)
print(second.randrange(0, 50, 5) == c, second.randrange(10) == d)
print(random.Random(2027).randint(1, 100) != a)

random.seed(7)
print(random.randint(0, 9), random.randint(0, 9), random.randint(0, 9), random.randint(0, 9), random.randint(0, 9))

bounded = True
i = 0
while i < 25:
    draw = random.randint(3, 7)
    if draw < 3 or draw > 7:
        bounded = False
    i += 1
print(bounded)

in_range = True
i = 0
while i < 25:
    draw = random.randrange(10, 20, 3)
    if draw != 10 and draw != 13 and draw != 16 and draw != 19:
        in_range = False
    i += 1
print(in_range)

print(random.choice([9]) == 9)
"##;

const RANDOM_STDOUT: &str = r##"62 90 35 2 red
[1, 3, 4, 5, 2]
True True
True True
True
5 5 8 5 7
True
True
True
"##;

const RANDOM_EMPTY_RANGE_PROGRAM: &str = r##"import random

rng = random.Random(5)
rng.randint(5, 1)
"##;

const RANDOM_BAD_SEED_PROGRAM: &str = r##"import random

random.seed("bad")
"##;

#[test]
fn random_seeded_streams_and_bounds() {
    let dir = TestDir::new("random");
    assert_ok(&dir, "random_core.py", RANDOM_PROGRAM, RANDOM_STDOUT);
    assert_failure(
        &dir,
        "random_empty_range.py",
        RANDOM_EMPTY_RANGE_PROGRAM,
        "ValueError: empty range for randrange() (5, 1, 1)",
    );
    assert_failure(
        &dir,
        "random_bad_seed.py",
        RANDOM_BAD_SEED_PROGRAM,
        "TypeError: The only supported seed types are: int, float, None",
    );
}

// -------------------------------------------------------------------------
// json
// -------------------------------------------------------------------------

const JSON_PROGRAM: &str = r##"import json

data = {"b": 2, "a": [1, "x", True, None], "c": {"nested": 1}}
print(json.dumps(data))
print(json.dumps(data, sort_keys=True))
print(json.dumps(data["c"], indent=2))
print(json.dumps({"text": "héllo"}, ensure_ascii=True))
print(json.dumps({"text": "héllo"}, ensure_ascii=False))

text = json.dumps(data, sort_keys=True)
back = json.loads(text)
print(back == data, back["a"][2], back["c"]["nested"])
print(json.loads('{"n": 1.5, "s": "ok"}'))
print(json.loads("[1, 2, 3]"))
print(json.loads("null") == None)

path = __BASE__ + "/data.json"
handle = open(path, "w", encoding="utf-8")
json.dump(data, handle, indent=2, sort_keys=True)
handle.close()

handle = open(path, "r", encoding="utf-8")
loaded = json.load(handle)
handle.close()
print(loaded == data)

handle = open(path, "r", encoding="utf-8")
print(handle.read() == json.dumps(data, indent=2, sort_keys=True))
handle.close()
"##;

const JSON_STDOUT: &str = r##"{"b": 2, "a": [1, "x", true, null], "c": {"nested": 1}}
{"a": [1, "x", true, null], "b": 2, "c": {"nested": 1}}
{
  "nested": 1
}
{"text": "h\u00e9llo"}
{"text": "héllo"}
True True 1
{'n': 1.5, 's': 'ok'}
[1, 2, 3]
True
True
True
"##;

const JSON_FILE_CONTENT: &str = "{\n  \"a\": [\n    1,\n    \"x\",\n    true,\n    null\n  ],\n  \"b\": 2,\n  \"c\": {\n    \"nested\": 1\n  }\n}";

const JSON_INVALID_PROGRAM: &str = r##"import json

json.loads("")
"##;

#[test]
fn json_roundtrips_files_and_options() {
    let dir = TestDir::new("json");
    assert_ok(&dir, "json_core.py", JSON_PROGRAM, JSON_STDOUT);
    assert_eq!(
        std::fs::read_to_string(dir.work().join("data.json")).expect("json.dump must write"),
        JSON_FILE_CONTENT,
        "json.dump must honour indent=2 and sort_keys=True"
    );
    assert_failure(
        &dir,
        "json_invalid.py",
        JSON_INVALID_PROGRAM,
        "ValueError: Expecting value",
    );
}

// -------------------------------------------------------------------------
// re
// -------------------------------------------------------------------------

const RE_PROGRAM: &str = r##"import re

text = "2026-10-25 alpha 42 beta 7"
print(re.search(r"\d+", text).group())
print(re.match(r"\w+", text).group())
print(re.match(r"[a-z]+", text) == None)
print(re.fullmatch(r"[a-z]+", "abc") != None, re.fullmatch(r"[a-z]+", "abc1") == None)
print(re.findall(r"\d+", text))
print(re.findall(r"([a-z])(\d)", "a1 b2"))
print(re.sub(r"\d+", "#", text))
print(re.sub(r"\d+", "#", text, 2))
print(re.split(r"\s+", "a  b   c"))
print(re.split(r"\d+", "a1b22c", 1))

found = re.search(r"(\w+)@(\w+)", "mail: user@host!")
print(found.group(0), found.group(1), found.group(2), found.groups())
print(found.span(), found.span(1), found.start(2), found.end())

compiled = re.compile(r"[a-z]+")
print(compiled.findall("ab 12 cd"))
print(compiled.search("12cd").group(), compiled.fullmatch("abc") != None)
print(compiled.pattern, compiled.flags)
"##;

const RE_STDOUT: &str = r##"2026
2026
True
True True
['2026', '10', '25', '42', '7']
[('a', '1'), ('b', '2')]
#-#-# alpha # beta #
#-#-25 alpha 42 beta 7
['a', 'b', 'c']
['a', 'b22c']
user@host user host ('user', 'host')
(6, 15) (6, 10) 11 15
['ab', 'cd']
cd True
[a-z]+ 0
"##;

const RE_INVALID_PROGRAM: &str = r##"import re

re.compile("(")
"##;

#[test]
fn re_search_groups_sub_split_and_compile() {
    let dir = TestDir::new("re");
    assert_ok(&dir, "re_core.py", RE_PROGRAM, RE_STDOUT);
    assert_failure(
        &dir,
        "re_invalid.py",
        RE_INVALID_PROGRAM,
        "ValueError: invalid regular expression",
    );
}

// -------------------------------------------------------------------------
// os / os.path / pathlib
// -------------------------------------------------------------------------

const OS_PROGRAM: &str = r##"import os
import pathlib

BASE = __BASE__
print(os.path.join("a", "b", "c"))
print(os.path.join(BASE, "sub", "f.txt") == BASE + "/sub/f.txt")

note = os.path.join(BASE, "note.txt")
renamed = os.path.join(BASE, "renamed.txt")
print(os.path.exists(note), os.path.isfile(note), os.path.isdir(BASE))

handle = open(note, "w")
handle.write("payload")
handle.close()
print(os.path.exists(note), os.path.isfile(note), os.path.isdir(note))

os.rename(note, renamed)
print(os.path.exists(note), os.path.exists(renamed))
os.remove(renamed)
print(os.path.exists(renamed))

base = pathlib.Path(BASE)
print(base.exists(), base.is_dir(), base.is_file())

p = base / "note.txt"
print(p.name, p.stem, p.suffix)
print(p.parent.as_posix() == base.as_posix())

written = p.write_text("héllo\n")
print(written)
print(p.read_text() == "héllo\n")
print(len(p.read_bytes()))

sub = base / "sub"
sub.mkdir()
print(sub.exists(), sub.is_dir())
sub.joinpath("inner.txt").write_text("inner")
found = base.glob("*.txt")
print(len(found), found[0].name)

names = []
for entry in base.iterdir():
    names.append(entry.name)
print(names)

os.remove(p)
os.remove(sub.joinpath("inner.txt"))
print(p.exists())
"##;

const OS_STDOUT: &str = r##"a/b/c
True
False False True
True True False
False True
False
True True False
note.txt note .txt
True
6
True
7
True True
1 note.txt
['note.txt', 'sub']
False
"##;

const OS_REMOVE_FAILING_PROGRAM: &str = r##"import os

os.remove(__BASE__ + "/missing.txt")
"##;

#[test]
fn os_path_and_pathlib_filesystem_operations() {
    let dir = TestDir::new("os-pathlib");
    assert_ok(&dir, "os_pathlib.py", OS_PROGRAM, OS_STDOUT);
    assert_failure(
        &dir,
        "os_remove_missing.py",
        OS_REMOVE_FAILING_PROGRAM,
        "OSError: remove:",
    );
}

// -------------------------------------------------------------------------
// io / csv
// -------------------------------------------------------------------------

const IO_CSV_PROGRAM: &str = r##"import csv
import io

BASE = __BASE__
text_path = BASE + "/text.txt"
handle = io.open(text_path, "w", encoding="utf-8")
print(handle.write("first line\nsecond line\n"))
handle.close()

handle = io.open(text_path, "r", encoding="utf-8")
content = handle.read()
handle.close()
print(content == "first line\nsecond line\n")
print(len(content.splitlines()))

handle = open(text_path, "r")
print(handle.readline().strip())
handle.close()

csv_path = BASE + "/rows.csv"
handle = open(csv_path, "w", newline="")
writer = csv.writer(handle)
writer.writerow(["name", "qty"])
writer.writerows([["a", "1"], ["b,c", "2"]])
handle.close()

handle = open(csv_path, newline="")
rows = list(csv.reader(handle))
handle.close()
print(rows)
print(rows[1], rows[2])

handle = open(csv_path, newline="")
for row in csv.DictReader(handle):
    print(row["name"], row["qty"])
handle.close()

dict_path = BASE + "/rows_dict.csv"
handle = open(dict_path, "w", newline="")
dict_writer = csv.DictWriter(handle, fieldnames=["id", "value"])
dict_writer.writeheader()
dict_writer.writerow({"id": 1, "value": "x"})
handle.close()
handle = open(dict_path, "r")
print(handle.read() == "id,value\r\n1,x\r\n")
handle.close()
"##;

const IO_CSV_STDOUT: &str = r##"23
True
2
first line
[['name', 'qty'], ['a', '1'], ['b,c', '2']]
['a', '1'] ['b,c', '2']
a 1
b,c 2
True
"##;

const IO_CSV_FAILING_PROGRAM: &str = r##"import csv

csv.reader("not a file")
"##;

#[test]
fn io_and_csv_roundtrip_text_files() {
    let dir = TestDir::new("io-csv");
    assert_ok(&dir, "io_csv.py", IO_CSV_PROGRAM, IO_CSV_STDOUT);
    assert_eq!(
        std::fs::read_to_string(dir.work().join("rows.csv")).expect("csv.writer must write"),
        "name,qty\r\na,1\r\n\"b,c\",2\r\n",
        "csv.writer must quote fields containing separators and use CRLF"
    );
    assert_eq!(
        std::fs::read_to_string(dir.work().join("rows_dict.csv"))
            .expect("csv.DictWriter must write"),
        "id,value\r\n1,x\r\n",
        "csv.DictWriter must write the header and fieldnames order"
    );
    assert_failure(
        &dir,
        "csv_invalid_target.py",
        IO_CSV_FAILING_PROGRAM,
        "TypeError: csv.reader() argument 1 must be a file object, not str",
    );
}
