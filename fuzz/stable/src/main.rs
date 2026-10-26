//! Stable-toolchain fallback runner for the Hydro fuzz targets.
//!
//! `libfuzzer-sys` needs nightly (`cargo-fuzz`). Where that is unavailable,
//! this runner executes the *same* harness functions on deterministic
//! generated inputs, so a bounded fuzz smoke run still happens on stable:
//!
//! ```text
//! cargo build --release --manifest-path fuzz/stable/Cargo.toml
//! fuzz/stable/target/release/hydro-fuzz-stable frontend_pipeline --iterations 50
//! fuzz/stable/target/release/hydro-fuzz-stable runtime_gc --iterations 2000
//! ```
//!
//! A panic (or an abnormal exit) fails with a non-zero status and prints the
//! offending input as hex, so it can be replayed with `--input`:
//!
//! ```text
//! hydro-fuzz-stable runtime_gc --input crash.bin
//! ```
//!
//! The long campaign is the real fuzzer's job; see `fuzz/Cargo.toml`.

#[path = "../../src/lib.rs"]
mod harness;

use std::path::PathBuf;
use std::time::Instant;

use harness::{SplitMix64, generate_frontend_input, generate_runtime_input};

const USAGE: &str = "\
usage: hydro-fuzz-stable <target> [options]

targets:
  frontend_pipeline   parse + lower + type-check + native codegen
  runtime_gc          allocation, containers, cycles, fibers, collection

options:
  --iterations N   generated inputs to run (default 100)
  --seed N         PRNG seed for generated inputs (default 0x5EED)
  --input PATH     run one file as the fuzz input (repeatable)
  --corpus DIR     run every file in DIR (repeatable)
  --no-corpus      skip the checked-in seed corpus
  -h, --help       print this help";

fn main() {
    let mut target: Option<String> = None;
    let mut iterations = 100usize;
    let mut seed = 0x5EEDu64;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut corpus_dirs: Vec<PathBuf> = Vec::new();
    let mut use_default_corpus = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--iterations" => {
                iterations = number(&mut args, "--iterations");
            }
            "--seed" => {
                seed = number(&mut args, "--seed");
            }
            "--input" => {
                files.push(PathBuf::from(
                    args.next().unwrap_or_else(|| usage("--input needs a path")),
                ));
            }
            "--corpus" => {
                corpus_dirs.push(PathBuf::from(
                    args.next().unwrap_or_else(|| usage("--corpus needs a directory")),
                ));
            }
            "--no-corpus" => use_default_corpus = false,
            other if other.starts_with('-') => usage(&format!("unknown option '{other}'")),
            other => {
                if target.replace(other.to_string()).is_some() {
                    usage("more than one target given");
                }
            }
        }
    }

    let Some(target) = target else {
        usage("no target given");
    };
    let run: fn(&[u8]) = match target.as_str() {
        "frontend_pipeline" => harness::frontend_pipeline,
        "runtime_gc" => harness::runtime_gc,
        other => usage(&format!("unknown target '{other}'")),
    };

    let start = Instant::now();
    let mut executed = 0usize;

    // Checked-in seeds first: they must always keep working.
    if use_default_corpus {
        let default_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("corpus")
            .join(&target);
        corpus_dirs.push(default_dir);
    }
    for dir in &corpus_dirs {
        for path in sorted_files(dir) {
            let data = std::fs::read(&path)
                .unwrap_or_else(|error| usage(&format!("cannot read {}: {error}", path.display())));
            run_one(run, &data);
            executed += 1;
        }
    }
    for path in &files {
        let data = std::fs::read(path)
            .unwrap_or_else(|error| usage(&format!("cannot read {}: {error}", path.display())));
        run_one(run, &data);
        executed += 1;
    }

    let mut rng = SplitMix64::new(seed);
    for index in 0..iterations {
        let data = match target.as_str() {
            "frontend_pipeline" => generate_frontend_input(&mut rng),
            _ => generate_runtime_input(&mut rng),
        };
        if std::env::var_os("FUZZ_TRACE").is_some() {
            eprintln!("input {index}: {}", hex(&data));
        }
        run_one(run, &data);
        executed += 1;
    }

    println!(
        "hydro-fuzz-stable: {target}: {executed} inputs in {:.2?}",
        start.elapsed()
    );
}

/// Runs one candidate; a panic fails the process with a replayable input.
fn run_one(run: fn(&[u8]), data: &[u8]) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(data)));
    if outcome.is_err() {
        eprintln!(
            "hydro-fuzz: {} byte input crashed the target\nhex: {}",
            data.len(),
            hex(data)
        );
        std::process::exit(101);
    }
}

/// Every regular file directly inside `dir`, in stable order (missing
/// directories are empty: the seed corpus is optional).
fn sorted_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

fn number<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, option: &str) -> T {
    let value = args.next().unwrap_or_else(|| usage(&format!("{option} needs a value")));
    match value.parse() {
        Ok(parsed) => parsed,
        Err(_) => usage(&format!("{option} needs a number, got '{value}'")),
    }
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn usage(message: &str) -> ! {
    eprintln!("hydro-fuzz-stable: {message}\n\n{USAGE}");
    std::process::exit(2);
}
