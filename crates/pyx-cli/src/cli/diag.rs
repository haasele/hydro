//! Rendering of frontend diagnostics for the CLI.
//!
//! The frontend owns the diagnostic contract ([`pyx_frontend::Diagnostic`]);
//! this module turns it into cargo-style snippets, machine-readable JSON and
//! optional ANSI color. Everything is `std`-only so the CLI keeps its small
//! dependency footprint.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use pyx_frontend::Diagnostic;

/// When to emit ANSI escape sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// Color only when stderr is a terminal and `NO_COLOR` is unset.
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Whether this mode enables color on the current stderr.
    pub fn use_color(self) -> bool {
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal(),
        }
    }
}

/// Diagnostic presentation options parsed from the command line.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Emit a JSON array on stderr instead of human-readable snippets.
    pub json: bool,
    pub color: ColorMode,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            json: false,
            color: ColorMode::Auto,
        }
    }
}

impl Options {
    fn use_color(&self) -> bool {
        self.color.use_color()
    }
}

/// Print `diagnostics` and return the process exit code.
///
/// Human output goes to stderr; JSON mode prints exactly one array (no summary)
/// so the stream stays machine-readable. An empty diagnostic list is an
/// internal frontend contract violation and yields exit code 2.
pub fn report(
    diagnostics: &[Diagnostic],
    source: Option<&str>,
    file: &Path,
    verb: &str,
    options: &Options,
) -> u8 {
    if diagnostics.is_empty() {
        eprintln!("pyx: internal frontend error: no diagnostics reported");
        return 2;
    }

    if options.json {
        eprintln!("{}", render_json(diagnostics));
        return 1;
    }

    let color = options.use_color();
    let entry_file = file.to_string_lossy();
    let mut out = String::new();
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        // Diagnostics may point at imported modules; load their source so the
        // caret snippet matches the reported file.
        let fallback;
        let diagnostic_source = if diagnostic.file == entry_file.as_ref() {
            source
        } else {
            fallback = fs::read_to_string(&diagnostic.file).ok();
            fallback.as_deref()
        };
        out.push_str(&render_human(diagnostic, diagnostic_source, color));
    }
    eprint!("{out}");

    eprintln!(
        "error: could not {verb} '{}' due to {} previous error(s)",
        file.display(),
        diagnostics.len()
    );
    1
}

/// Render one diagnostic as a cargo-style snippet.
fn render_human(diagnostic: &Diagnostic, source: Option<&str>, color: bool) -> String {
    let mut out = String::new();
    let line_number = diagnostic.line.max(1);
    let width = digits(line_number);

    let header = format!("error[{}]:", diagnostic.code);
    let _ = writeln!(
        out,
        "{} {}",
        paint(color, "1;31", &header),
        diagnostic.message
    );

    let _ = writeln!(
        out,
        "{}{} {}:{}:{}",
        " ".repeat(width + 1),
        paint(color, "34", "-->"),
        diagnostic.file,
        line_number,
        diagnostic.col
    );

    if let Some(line) = source.and_then(|source| source_line(source, line_number)) {
        let gutter = " ".repeat(width + 2);
        // Tabs become single spaces: diagnostics use byte columns, and a tab is
        // one byte, so the caret stays aligned with the displayed line.
        let display = line.replace('\t', " ");
        let _ = writeln!(out, "{gutter}|");
        let _ = writeln!(out, " {line_number:>width$} | {display}", width = width);

        let (start, length) = caret_span(line, diagnostic);
        let carets = paint(color, "31", &"^".repeat(length));
        let _ = writeln!(out, "{gutter}| {}{carets}", " ".repeat(start));

        if diagnostic.help.is_some() {
            let _ = writeln!(out, "{gutter}|");
        }
    }

    if let Some(help) = &diagnostic.help {
        let _ = writeln!(out, "{}= help: {help}", " ".repeat(width + 2));
    }

    out
}

/// Render diagnostics as a compact JSON array with a stable key order.
pub(crate) fn render_json(diagnostics: &[Diagnostic]) -> String {
    let mut out = String::from("[");
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let help = match &diagnostic.help {
            Some(help) => json_string(help),
            None => String::from("null"),
        };
        let _ = write!(
            out,
            "{{\"file\":{},\"line\":{},\"col\":{},\"end_col\":{},\"code\":{},\
             \"message\":{},\"help\":{help}}}",
            json_string(&diagnostic.file),
            diagnostic.line,
            diagnostic.col,
            diagnostic.end_col,
            json_string(diagnostic.code),
            json_string(&diagnostic.message)
        );
    }
    out.push(']');
    out
}

/// Encode `value` as a JSON string literal (including the quotes).
pub fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Number of decimal digits of `line` (at least one).
fn digits(line: u32) -> usize {
    line.to_string().len().max(1)
}

/// Zero-based character offset and caret length for a diagnostic's span.
fn caret_span(line: &str, diagnostic: &Diagnostic) -> (usize, usize) {
    let byte_offset = diagnostic.col.saturating_sub(1) as usize;
    let start = line
        .char_indices()
        .take_while(|(offset, _)| *offset < byte_offset)
        .count();
    let remaining = line.chars().count().saturating_sub(start);
    let length = (diagnostic.caret_len() as usize).min(remaining).max(1);
    (start, length)
}

/// Return the requested 1-based line without its line terminator.
fn source_line(source: &str, line: u32) -> Option<&str> {
    let index = line.checked_sub(1)? as usize;
    source.lines().nth(index)
}

fn paint(enabled: bool, style: &str, text: &str) -> String {
    if enabled {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
