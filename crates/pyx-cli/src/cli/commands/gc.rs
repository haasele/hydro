//! `pyx gc`: compile and run a program, then report collector statistics.
//!
//! The program is compiled and executed exactly like `pyx run`; afterwards the
//! runtime's cumulative GC counters are printed to stdout. With `--json` the
//! report is one JSON object, emitted after any program output, so the last
//! stdout line is always machine-readable.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use pyx_runtime::GcStats;

use super::{exit_code, read_source, unwrap_or_report};
use crate::cli::args::{parse_single_source, Verbosity};
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_gc(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let (json, rest) = split_json(args);
    let (file, mut options) = parse_single_source(&rest, "gc", verbosity)?;
    if json {
        // `--json` also selects the machine-readable diagnostics rendering,
        // matching the other compiler commands.
        options.json = true;
    }
    run_and_report(&file, &options, json)
}

/// Removes `--json` from the argument list so the shared single-source parser
/// does not consume it as a diagnostic flag. A `--` still ends flag parsing.
fn split_json(args: &[OsString]) -> (bool, Vec<OsString>) {
    let mut json = false;
    let mut positional_only = false;
    let mut rest = Vec::with_capacity(args.len());
    for arg in args {
        let text = arg.to_string_lossy();
        if !positional_only {
            if text == "--" {
                positional_only = true;
            } else if text == "--json" {
                json = true;
                continue;
            }
        }
        rest.push(arg.clone());
    }
    (json, rest)
}

fn run_and_report(file: &Path, options: &diag::Options, json: bool) -> CliResult {
    let source = read_source(file)?;
    let program = unwrap_or_report(
        pyx_frontend::check_file(file),
        &source,
        file,
        "compile",
        options,
    )?;
    // Same canonical source path as `pyx run`: spawned children recompile it.
    let canonical = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    pyx_runtime::set_jit_source(canonical);
    let code = pyx_codegen::run_jit(&program)
        .map_err(|error| CliError::new(2, format!("pyx: internal codegen error: {error}")))?;

    let stats = pyx_runtime::gc_stats();
    if json {
        println!("{}", render_json(&stats));
    } else {
        print_human(&stats);
    }
    Ok(ExitCode::from(exit_code(code)))
}

/// One stable JSON object with snake_case keys, in a fixed order.
fn render_json(stats: &GcStats) -> String {
    format!(
        "{{\"collections\":{},\"live_objects\":{},\"threshold\":{},\
         \"pause_total_us\":{},\"pause_max_us\":{},\"mark_total_us\":{},\
         \"sweep_total_us\":{},\"freed_total\":{},\"stress\":{},\
         \"precise\":{},\"precise_frames\":{}}}",
        stats.collections,
        stats.live_objects,
        stats.threshold,
        stats.pause_total_us,
        stats.pause_max_us,
        stats.mark_total_us,
        stats.sweep_total_us,
        stats.freed_total,
        stats.stress,
        stats.precise,
        stats.precise_frames
    )
}

fn print_human(stats: &GcStats) {
    println!("gc statistics:");
    println!("  collections    {}", stats.collections);
    println!("  live objects   {}", stats.live_objects);
    println!("  threshold      {}", stats.threshold);
    println!("  pause total    {} us", stats.pause_total_us);
    println!("  pause max      {} us", stats.pause_max_us);
    println!("  mark total     {} us", stats.mark_total_us);
    println!("  sweep total    {} us", stats.sweep_total_us);
    println!("  freed total    {}", stats.freed_total);
    println!(
        "  precise mode   {}",
        if stats.precise { "strict" } else { "mixed" }
    );
    println!("  precise frames {}", stats.precise_frames);
}
