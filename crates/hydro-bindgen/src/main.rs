//! `hydro-bindgen` — generate an HNA v1 binding crate from a C header.
//!
//! Usage:
//!
//! ```text
//! hydro-bindgen generate --header <file> --module <dotted-name>
//!     --provider <str> --out <dir> [--hna-crate <rel-path>]
//!     [--library <link-name>] [--include <dir> ...] [--define NAME=VAL ...]
//!     [--check] [--report <file>]
//! hydro-bindgen check --out <dir> ...   # same options, compare only
//! ```
//!
//! The generator is deterministic and network-free. Generation happens in a
//! temporary directory and is published with a rename, so a failed run never
//! leaves a partial output directory behind.

mod check;
mod clang;
mod codegen;
mod json;
mod types;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_HNA_CRATE: &str = "../hydro-hna";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("hydro-bindgen: error: {message}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    header: PathBuf,
    module: String,
    provider: String,
    out: PathBuf,
    hna_crate: String,
    library: Option<String>,
    includes: Vec<PathBuf>,
    defines: Vec<String>,
    check: bool,
    report: Option<PathBuf>,
}

#[derive(Debug)]
enum Parsed {
    Help,
    Version,
    Run(Box<Options>),
}

fn run(args: &[String]) -> Result<u8, String> {
    let options = match parse_args(args)? {
        Parsed::Help => {
            print!("{USAGE}");
            return Ok(0);
        }
        Parsed::Version => {
            println!("hydro-bindgen {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        Parsed::Run(options) => *options,
    };

    let header_display = options
        .header
        .to_str()
        .ok_or_else(|| format!("{}: header path is not UTF-8", options.header.display()))?
        .to_owned();

    let raw = clang::ingest(&options.header, &options.includes, &options.defines)?;
    let module = types::build_module(&raw, &options.module, &options.provider, &header_display)
        .map_err(|error| error.to_string())?;

    let config = codegen::Config {
        hna_crate: options.hna_crate.clone(),
        library: options.library.clone(),
    };
    let files = codegen::render(&module, &config);

    if options.check {
        let differences = check::compare(&options.out, &files)?;
        if !differences.is_empty() {
            for difference in &differences {
                print!("{difference}");
            }
            eprintln!(
                "hydro-bindgen: check failed for {}: {} file(s) differ",
                options.out.display(),
                differences.len()
            );
            return Ok(1);
        }
        println!(
            "hydro-bindgen: check passed for {} ({} files)",
            options.out.display(),
            files.len()
        );
    } else {
        check::write_atomically(&options.out, &files)?;
        println!(
            "hydro-bindgen: generated module `{}` with {} function(s) into {}",
            module.name,
            module.functions.len(),
            options.out.display()
        );
    }

    if let Some(report) = &options.report {
        write_report(report, &files)?;
        println!("hydro-bindgen: wrote type report to {}", report.display());
    }

    Ok(0)
}

fn write_report(path: &Path, files: &[codegen::GeneratedFile]) -> Result<(), String> {
    let report = files
        .iter()
        .find(|file| file.path.as_path() == Path::new("type-report.md"))
        .ok_or_else(|| "internal error: generated output has no type-report.md".to_owned())?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "{}: cannot create report directory: {error}",
                    parent.display()
                )
            })?;
        }
    }
    std::fs::write(path, &report.contents)
        .map_err(|error| format!("{}: cannot write type report: {error}", path.display()))
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut header = None;
    let mut module = None;
    let mut provider = None;
    let mut out = None;
    let mut hna_crate = None;
    let mut library = None;
    let mut includes = Vec::new();
    let mut defines = Vec::new();
    let mut check = false;
    let mut report = None;

    let mut index = 0;
    match args.first().map(String::as_str) {
        Some("generate") => index = 1,
        Some("check") => {
            check = true;
            index = 1;
        }
        Some("help") => return Ok(Parsed::Help),
        _ => {}
    }

    while index < args.len() {
        let argument = &args[index];
        if argument == "--help" || argument == "-h" {
            return Ok(Parsed::Help);
        }
        if argument == "--version" || argument == "-V" {
            return Ok(Parsed::Version);
        }
        if argument == "--check" {
            check = true;
            index += 1;
            continue;
        }
        if !argument.starts_with("--") {
            return Err(format!(
                "unexpected argument `{argument}` (expected a --flag; see --help)"
            ));
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };
        let needs_value = matches!(
            flag,
            "--header"
                | "--module"
                | "--provider"
                | "--out"
                | "--hna-crate"
                | "--library"
                | "--include"
                | "--define"
                | "--report"
        );
        if !needs_value {
            return Err(format!("unknown flag `{flag}` (see --help)"));
        }
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| format!("`{flag}` requires a value"))?
            }
        };
        index += 1;

        match flag {
            "--header" => set_once(&mut header, "--header", value)?,
            "--module" => set_once(&mut module, "--module", value)?,
            "--provider" => set_once(&mut provider, "--provider", value)?,
            "--out" => set_once(&mut out, "--out", value)?,
            "--hna-crate" => set_once(&mut hna_crate, "--hna-crate", value)?,
            "--library" => set_once(&mut library, "--library", value)?,
            "--include" => {
                if value.is_empty() {
                    return Err("`--include` requires a non-empty directory".to_owned());
                }
                includes.push(PathBuf::from(value));
            }
            "--define" => {
                let (name, _) = value
                    .split_once('=')
                    .ok_or_else(|| format!("`--define {value}` must have the form NAME=VALUE"))?;
                if name.is_empty() {
                    return Err(format!("`--define {value}` has an empty name"));
                }
                defines.push(value);
            }
            "--report" => set_once(&mut report, "--report", value)?,
            _ => unreachable!("validated above"),
        }
    }

    let header = header.ok_or_else(|| "missing required `--header <file>`".to_owned())?;
    let module = module.ok_or_else(|| "missing required `--module <dotted-name>`".to_owned())?;
    let provider = provider.ok_or_else(|| "missing required `--provider <str>`".to_owned())?;
    let out = out.ok_or_else(|| "missing required `--out <dir>`".to_owned())?;

    // Validate here so a bad module name never reaches code generation.
    types::validate_module_name(&module)?;
    if provider.contains('\0') {
        return Err("`--provider` contains a NUL byte".to_owned());
    }
    if let Some(library) = &library {
        if library.contains('\0') {
            return Err("`--library` contains a NUL byte".to_owned());
        }
        if library.is_empty() {
            return Err("`--library` must not be empty".to_owned());
        }
    }

    Ok(Parsed::Run(Box::new(Options {
        header: PathBuf::from(header),
        module,
        provider,
        out: PathBuf::from(out),
        hna_crate: hna_crate.unwrap_or_else(|| DEFAULT_HNA_CRATE.to_owned()),
        library,
        includes,
        defines,
        check,
        report: report.map(PathBuf::from),
    })))
}

fn set_once(slot: &mut Option<String>, flag: &str, value: String) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("`{flag}` requires a non-empty value"));
    }
    if slot.replace(value).is_some() {
        return Err(format!("`{flag}` was given more than once"));
    }
    Ok(())
}

const USAGE: &str = "\
hydro-bindgen — generate an HNA v1 binding crate from a C header

USAGE:
    hydro-bindgen generate --header <file> --module <dotted-name> \\
        --provider <str> --out <dir> [OPTIONS]
    hydro-bindgen check --out <dir> [OPTIONS]     # compare, exit 1 on drift
    hydro-bindgen --check --out <dir> [OPTIONS]

REQUIRED:
    --header <file>        C header to ingest (clang required)
    --module <dotted-name> exact Python import name, e.g. example._fast
    --provider <str>       distribution version, e.g. 'example-native 0.4.0'
    --out <dir>            output directory for the generated crate

OPTIONS:
    --hna-crate <rel-path> hydro-hna dependency path (default: ../hydro-hna)
    --library <link-name>  emit #[link(name = \"...\")] on the C declarations
    --include <dir>        add a clang include directory (repeatable)
    --define NAME=VALUE    add a clang preprocessor definition (repeatable)
    --check                regenerate into a temp dir and compare every file;
                           exits 1 and prints a diff if anything differs
    --report <file>        also write type-report.md to this path
    -h, --help             print this help
    -V, --version          print the version

Generation is deterministic and network-free. Unsupported declarations stop
generation with the symbol, source line and reason; there is no --force.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_generate_options() {
        let parsed = parse_args(&args(&[
            "generate",
            "--header",
            "libm.h",
            "--module",
            "libm",
            "--provider",
            "libm 1.0",
            "--out",
            "out",
            "--include",
            "a",
            "--include",
            "b",
            "--define",
            "X=1",
            "--library",
            "m",
            "--report",
            "report.md",
        ]))
        .unwrap();
        let Parsed::Run(options) = parsed else {
            panic!("expected run");
        };
        assert_eq!(options.header, PathBuf::from("libm.h"));
        assert_eq!(options.module, "libm");
        assert_eq!(options.provider, "libm 1.0");
        assert_eq!(options.out, PathBuf::from("out"));
        assert_eq!(options.hna_crate, DEFAULT_HNA_CRATE);
        assert_eq!(options.library.as_deref(), Some("m"));
        assert_eq!(options.includes.len(), 2);
        assert_eq!(options.defines, vec!["X=1"]);
        assert!(!options.check);
        assert_eq!(options.report, Some(PathBuf::from("report.md")));
    }

    #[test]
    fn accepts_check_subcommand_and_flag() {
        for values in [
            vec![
                "check",
                "--header",
                "h.h",
                "--module",
                "m",
                "--provider",
                "p",
                "--out",
                "o",
            ],
            vec![
                "--check",
                "--header",
                "h.h",
                "--module",
                "m",
                "--provider",
                "p",
                "--out",
                "o",
            ],
        ] {
            let Parsed::Run(options) = parse_args(&args(&values)).unwrap() else {
                panic!("expected run");
            };
            assert!(options.check);
        }
    }

    #[test]
    fn supports_inline_values() {
        let Parsed::Run(options) = parse_args(&args(&[
            "generate",
            "--header=h.h",
            "--module=m",
            "--provider=p",
            "--out=o",
        ]))
        .unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.module, "m");
    }

    #[test]
    fn rejects_missing_and_duplicate_arguments() {
        assert!(parse_args(&args(&["generate"])).is_err());
        let error = parse_args(&args(&[
            "generate",
            "--header",
            "h.h",
            "--header",
            "g.h",
            "--module",
            "m",
            "--provider",
            "p",
            "--out",
            "o",
        ]))
        .unwrap_err();
        assert!(error.contains("more than once"));
        assert!(parse_args(&args(&[
            "generate",
            "--header",
            "h.h",
            "--module",
            "bad name",
            "--provider",
            "p",
            "--out",
            "o",
        ]))
        .is_err());
        assert!(parse_args(&args(&[
            "generate",
            "--header",
            "h.h",
            "--module",
            "m",
            "--provider",
            "p",
            "--out",
            "o",
            "--define",
            "NOEQUALS",
        ]))
        .is_err());
        assert!(parse_args(&args(&["generate", "--unknown"])).is_err());
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(
            parse_args(&args(&["--help"])).unwrap(),
            Parsed::Help
        ));
        assert!(matches!(
            parse_args(&args(&["generate", "--version"])).unwrap(),
            Parsed::Version
        ));
    }
}
