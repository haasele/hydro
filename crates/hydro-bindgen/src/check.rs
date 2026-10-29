//! Atomic publication of generated files and byte-for-byte `--check`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codegen::GeneratedFile;

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = UNIQUE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", std::process::id(), nanos, counter)
}

/// Remove a directory unless it was published.
struct TempDir {
    path: PathBuf,
    armed: bool,
}

impl TempDir {
    fn new(path: PathBuf) -> Result<TempDir, String> {
        fs::create_dir_all(&path).map_err(|error| {
            format!(
                "{}: cannot create temporary directory: {error}",
                path.display()
            )
        })?;
        Ok(TempDir { path, armed: true })
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn write_files(root: &Path, files: &[GeneratedFile]) -> Result<(), String> {
    for file in files {
        let target = root.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("{}: cannot create directory: {error}", parent.display())
            })?;
        }
        let mut handle = fs::File::create(&target)
            .map_err(|error| format!("{}: cannot create file: {error}", target.display()))?;
        handle
            .write_all(file.contents.as_bytes())
            .map_err(|error| format!("{}: cannot write file: {error}", target.display()))?;
        handle
            .flush()
            .map_err(|error| format!("{}: cannot flush file: {error}", target.display()))?;
    }
    Ok(())
}

/// Write `files` into `out`, atomically replacing an existing directory.
///
/// Everything is written under a sibling temporary directory first. If any
/// step fails the temporary directory is removed and the previous `out` is
/// left untouched; there is never a partially generated `out`.
pub fn write_atomically(out: &Path, files: &[GeneratedFile]) -> Result<(), String> {
    let parent = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent).map_err(|error| {
        format!(
            "{}: cannot create output parent directory: {error}",
            parent.display()
        )
    })?;
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{}: output directory has no file name", out.display()))?;

    let staging_path = parent.join(format!(".{name}.hydro-bindgen-tmp-{}", unique_suffix()));
    let mut staging = TempDir::new(staging_path)?;
    write_files(&staging.path, files)?;

    if out.exists() {
        let backup_path = parent.join(format!(".{name}.hydro-bindgen-old-{}", unique_suffix()));
        fs::rename(out, &backup_path).map_err(|error| {
            format!(
                "{}: cannot move existing output aside: {error}",
                out.display()
            )
        })?;
        match fs::rename(&staging.path, out) {
            Ok(()) => {
                staging.disarm();
                if let Err(error) = fs::remove_dir_all(&backup_path) {
                    eprintln!(
                        "hydro-bindgen: warning: could not remove {}: {error}",
                        backup_path.display()
                    );
                }
            }
            Err(error) => {
                let _ = fs::rename(&backup_path, out);
                return Err(format!(
                    "{}: cannot publish generated directory: {error}",
                    out.display()
                ));
            }
        }
    } else {
        fs::rename(&staging.path, out).map_err(|error| {
            format!(
                "{}: cannot publish generated directory: {error}",
                out.display()
            )
        })?;
        staging.disarm();
    }
    Ok(())
}

/// Regenerate into a temporary directory and compare every generated file
/// with the checked-in copy in `out`.
///
/// Returns one unified-ish diff per differing or missing file. Extra files in
/// `out` are ignored: `--check` answers "is the generated surface current?",
/// not "is the directory only generated files?".
pub fn compare(out: &Path, files: &[GeneratedFile]) -> Result<Vec<String>, String> {
    let temp_path = std::env::temp_dir().join(format!("hydro-bindgen-check-{}", unique_suffix()));
    let temp = TempDir::new(temp_path)?;
    write_files(&temp.path, files)?;

    let mut differences = Vec::new();
    for file in files {
        let expected = fs::read(temp.path.join(&file.path)).map_err(|error| {
            format!(
                "{}: cannot read staged generated file: {error}",
                file.path.display()
            )
        })?;
        let committed_path = out.join(&file.path);
        match fs::read(&committed_path) {
            Ok(committed) => {
                if committed != expected {
                    differences.push(unified_diff(
                        &file.path.to_string_lossy(),
                        &expected,
                        &committed,
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                differences.push(unified_diff(
                    &format!("{} (missing)", file.path.display()),
                    &expected,
                    &[],
                ));
            }
            Err(error) => {
                return Err(format!(
                    "{}: cannot read checked-in file: {error}",
                    committed_path.display()
                ));
            }
        }
    }
    Ok(differences)
}

/// Unified-style diff with a single hunk. Byte differences in non-UTF-8 files
/// are reported as a summary rather than a line diff.
///
/// The freshly generated file is the baseline (`--- generated/`) and the
/// checked-in file is the compared side (`+++ checked-in/`): an added `+` line
/// is content present on disk but missing from regeneration.
fn unified_diff(relative: &str, generated: &[u8], checked_in: &[u8]) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- generated/{relative}\n"));
    out.push_str(&format!("+++ checked-in/{relative}\n"));
    let checked_in = match std::str::from_utf8(checked_in) {
        Ok(text) => text,
        Err(_) => {
            out.push_str("@@ binary file differs @@\n");
            return out;
        }
    };
    let generated = match std::str::from_utf8(generated) {
        Ok(text) => text,
        Err(_) => {
            out.push_str("@@ binary file differs @@\n");
            return out;
        }
    };
    // `--- generated/` is the baseline, `+++ checked-in/` is the compared
    // side, so `-` lines exist in regeneration and `+` lines exist on disk.
    let old: Vec<&str> = generated.lines().collect();
    let new: Vec<&str> = checked_in.lines().collect();
    out.push_str(&format!("@@ -1,{} +1,{} @@\n", old.len(), new.len()));

    if old.len().saturating_mul(new.len()) > 4_000_000 {
        for line in &old {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
        for line in &new {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }

    // Longest common subsequence over lines.
    let rows = old.len() + 1;
    let cols = new.len() + 1;
    let mut table = vec![0u32; rows * cols];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i * cols + j] = if old[i] == new[j] {
                table[(i + 1) * cols + (j + 1)] + 1
            } else {
                table[(i + 1) * cols + j].max(table[i * cols + (j + 1)])
            };
        }
    }
    let mut i = 0;
    let mut j = 0;
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            out.push(' ');
            out.push_str(old[i]);
            out.push('\n');
            i += 1;
            j += 1;
        } else if table[(i + 1) * cols + j] >= table[i * cols + (j + 1)] {
            out.push('-');
            out.push_str(old[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push('+');
            out.push_str(new[j]);
            out.push('\n');
            j += 1;
        }
    }
    while i < old.len() {
        out.push('-');
        out.push_str(old[i]);
        out.push('\n');
        i += 1;
    }
    while j < new.len() {
        out.push('+');
        out.push_str(new[j]);
        out.push('\n');
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("hydro-bindgen-test-{label}-{}", unique_suffix()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn diff_reports_added_and_removed_lines() {
        let diff = unified_diff("module.spec", b"a\nb\n", b"a\nc\n");
        assert!(diff.contains("--- generated/module.spec"));
        assert!(diff.contains("-b"));
        assert!(diff.contains("+c"));
        assert!(diff.contains(" a"));
    }

    #[test]
    fn writes_and_replaces_atomically() {
        let root = temp_root("atomic");
        let out = root.join("out");
        let files = vec![GeneratedFile {
            path: PathBuf::from("Cargo.toml"),
            contents: "first\n".to_owned(),
        }];
        write_atomically(&out, &files).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("Cargo.toml")).unwrap(),
            "first\n"
        );

        let files = vec![
            GeneratedFile {
                path: PathBuf::from("Cargo.toml"),
                contents: "second\n".to_owned(),
            },
            GeneratedFile {
                path: PathBuf::from("src/lib.rs"),
                contents: "// lib\n".to_owned(),
            },
        ];
        write_atomically(&out, &files).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("Cargo.toml")).unwrap(),
            "second\n"
        );
        assert_eq!(
            fs::read_to_string(out.join("src/lib.rs")).unwrap(),
            "// lib\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compare_detects_every_generated_file() {
        let root = temp_root("compare");
        let out = root.join("out");
        let files = vec![
            GeneratedFile {
                path: PathBuf::from("hydro-manifest.toml"),
                contents: "manifest_version = 1\n".to_owned(),
            },
            GeneratedFile {
                path: PathBuf::from("type-report.md"),
                contents: "report\n".to_owned(),
            },
        ];
        write_atomically(&out, &files).unwrap();
        assert!(compare(&out, &files).unwrap().is_empty());

        fs::write(out.join("type-report.md"), "changed\n").unwrap();
        let differences = compare(&out, &files).unwrap();
        assert_eq!(differences.len(), 1);
        assert!(differences[0].contains("--- generated/type-report.md"));
        assert!(differences[0].contains("+++ checked-in/type-report.md"));
        // `--- generated/` is the fresh output and `+++ checked-in/` is the
        // file on disk, so the added line is the stale on-disk content.
        assert!(differences[0].contains("\n-report\n"));
        assert!(differences[0].contains("\n+changed\n"));

        fs::remove_file(out.join("hydro-manifest.toml")).unwrap();
        let differences = compare(&out, &files).unwrap();
        assert_eq!(differences.len(), 2);
        let _ = fs::remove_dir_all(root);
    }
}
