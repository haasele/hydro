//! `csv.reader`/`csv.writer` and their dict variants.
//!
//! The reader is a snapshot: [`reader_new`] reads the remaining text of the
//! file object and parses it once (RFC 4180 with doubled quotes, embedded
//! newlines inside quoted fields, `\r\n`, `\n` and lone `\r` record
//! terminators). A blank line is an empty record (`[]`), an unbalanced quote
//! raises `ValueError`. Yielding a row builds a fresh list (or dict) because
//! the reader itself stores only native strings.
//!
//! `DictReader` uses the first record as the header. Missing trailing columns
//! become empty strings; extra columns are ignored (the runtime has no
//! `restkey`). `DictWriter` writes its `fieldnames` in order, missing values
//! become empty strings and a key outside `fieldnames` raises `ValueError`
//! (`dict contains fields not in fieldnames`). Rows are terminated with
//! `\r\n`, CPython's default.
//!
//! Writers hold the tagged file object and trace it as a GC edge, so the file
//! stays open as long as the writer is reachable; file failures surface as
//! `ValueError` (closed file) or `OSError` (mode mismatch) through
//! [`value::raise_file_error`].

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

fn type_error(message: impl Into<String>) -> i64 {
    let message = message.into();
    exc::raise_with_message(TYPE_ERROR, &message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn attribute_error(obj: i64, method: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "'{}' object has no attribute '{}'",
            value::type_name(obj),
            method
        ),
    )
}

// -------------------------------------------------------------------------
// Reader
// -------------------------------------------------------------------------

/// `csv.reader(file)`: parses the rest of the file into a reader object.
pub(crate) fn reader_new(file: i64) -> i64 {
    build_reader(file, false, "csv.reader")
}

/// `csv.DictReader(file)`: the first record becomes the header.
pub(crate) fn dict_reader_new(file: i64) -> i64 {
    build_reader(file, true, "csv.DictReader")
}

fn build_reader(file: i64, dict_mode: bool, function: &str) -> i64 {
    let Some(state) = value::as_file(file) else {
        return type_error(format!(
            "{function}() argument 1 must be a file object, not {}",
            value::type_name(file)
        ));
    };
    let content = match state.read(None) {
        Ok(content) => content,
        Err(error) => return value::raise_file_error(error),
    };
    let mut rows = match parse_rows(&content) {
        Ok(rows) => rows,
        Err(message) => return value_error(&message),
    };
    let header = if dict_mode {
        if rows.is_empty() {
            None
        } else {
            Some(rows.remove(0))
        }
    } else {
        None
    };
    value::csv_reader_new(rows, header)
}

/// Parses one CSV document. See the module docs for the accepted dialect.
pub(crate) fn parse_rows(text: &str) -> Result<Vec<Vec<String>>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    // True once the current field has any content or an opening quote, which
    // distinguishes a blank line (empty record) from a record with one empty
    // field (`,x`, `""`).
    let mut started = false;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if quoted {
            if ch == '"' {
                if index + 1 < chars.len() && chars[index + 1] == '"' {
                    field.push('"');
                    index += 2;
                } else {
                    quoted = false;
                    index += 1;
                }
            } else {
                field.push(ch);
                index += 1;
            }
            continue;
        }
        match ch {
            '"' => {
                if started {
                    // CPython's non-strict default keeps a quote inside an
                    // unquoted field literally.
                    field.push('"');
                } else {
                    quoted = true;
                    started = true;
                }
                index += 1;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                started = false;
                index += 1;
            }
            '\r' | '\n' => {
                if started || !row.is_empty() {
                    row.push(std::mem::take(&mut field));
                }
                rows.push(std::mem::take(&mut row));
                started = false;
                if ch == '\r' && index + 1 < chars.len() && chars[index + 1] == '\n' {
                    index += 2;
                } else {
                    index += 1;
                }
            }
            _ => {
                field.push(ch);
                started = true;
                index += 1;
            }
        }
    }
    if quoted {
        return Err("unexpected end of data".to_string());
    }
    if started || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

/// Yields the next row as a fresh list (`reader`) or dict (`DictReader`).
/// `None` marks the end of the snapshot. The cursor lives on the reader, so
/// every iterator over the same reader shares it, like a file cursor.
pub(crate) fn reader_item(obj: i64) -> Option<i64> {
    use std::sync::atomic::Ordering;
    let state = value::as_csv_reader(obj)?;
    let index = state.index.fetch_add(1, Ordering::Relaxed);
    let row = state.rows.get(index).cloned()?;
    let header = state.header.clone();
    Some(match header {
        None => list_from_fields(&row),
        Some(header) => dict_from_row(&header, &row),
    })
}

/// Builds a list of strings, appending one field at a time so the partially
/// built list roots every earlier child across allocations.
fn list_from_fields(fields: &[String]) -> i64 {
    let list = value::list_new(Vec::new());
    for field in fields {
        let item = value::str_alloc(field.clone());
        container::list_append(list, item);
        if exc::pending() {
            return NONE;
        }
    }
    list
}

/// Builds a dict in header order. Missing fields are empty strings; extra
/// fields are ignored.
fn dict_from_row(header: &[String], row: &[String]) -> i64 {
    let dict = value::dict_new(Vec::new());
    for (position, name) in header.iter().enumerate() {
        let key = value::str_alloc(name.clone());
        let text = row.get(position).cloned().unwrap_or_default();
        let item = value::str_alloc(text);
        container::dict_set(dict, key, item);
        if exc::pending() {
            return NONE;
        }
    }
    dict
}

// -------------------------------------------------------------------------
// Writer
// -------------------------------------------------------------------------

/// `csv.writer(file)`.
pub(crate) fn writer_new(file: i64) -> i64 {
    if value::as_file(file).is_none() {
        return type_error(format!(
            "csv.writer() argument 1 must be a file object, not {}",
            value::type_name(file)
        ));
    }
    value::csv_writer_new(file, None)
}

/// `csv.DictWriter(file, fieldnames)`. The fieldnames are copied as owned
/// strings; their order defines both the header and the row layout.
pub(crate) fn dict_writer_new(file: i64, fieldnames: i64) -> i64 {
    if value::as_file(file).is_none() {
        return type_error(format!(
            "csv.DictWriter() argument 1 must be a file object, not {}",
            value::type_name(file)
        ));
    }
    let Some(items) = value::as_list(fieldnames).or_else(|| value::as_tuple(fieldnames)) else {
        return type_error(format!(
            "fieldnames must be a list or tuple, not {}",
            value::type_name(fieldnames)
        ));
    };
    let mut names = Vec::with_capacity(items.len());
    for &item in items {
        let Some(name) = value::as_str(item) else {
            return type_error(format!(
                "fieldnames must be strings, not {}",
                value::type_name(item)
            ));
        };
        names.push(name.to_string());
    }
    value::csv_writer_new(file, Some(names))
}

/// Dispatches `writer.writerow(...)`, `writer.writerows(...)` and
/// `DictWriter.writeheader()`. Readers expose no methods.
pub(crate) fn writer_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(writer) = value::as_csv_writer(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "writerow" => {
            if let Some(err) = check_arity(method, args.len(), 1) {
                return err;
            }
            writerow(writer, args[0])
        }
        "writerows" => {
            if let Some(err) = check_arity(method, args.len(), 1) {
                return err;
            }
            writerows(writer, args[0])
        }
        "writeheader" => {
            if let Some(err) = check_arity(method, args.len(), 0) {
                return err;
            }
            writeheader(obj, writer)
        }
        _ => attribute_error(obj, method),
    }
}

fn check_arity(method: &str, argc: usize, expected: usize) -> Option<i64> {
    if argc == expected {
        return None;
    }
    let message = match expected {
        0 => format!("{method}() takes no arguments ({argc} given)"),
        1 => format!("{method}() takes exactly one argument ({argc} given)"),
        n => format!("{method}() takes exactly {n} arguments ({argc} given)"),
    };
    Some(type_error(message))
}

fn writerow(writer: &value::CsvWriterState, row: i64) -> i64 {
    if writer.fieldnames.is_some() {
        return dict_writerow(writer, row);
    }
    let fields = match row_fields(row) {
        Ok(fields) => fields,
        Err(err) => return err,
    };
    write_fields(writer.file, &fields)
}

fn writerows(writer: &value::CsvWriterState, rows: i64) -> i64 {
    let Some(items) = value::as_list(rows).or_else(|| value::as_tuple(rows)) else {
        return type_error(format!(
            "writerows() argument must be a list or tuple, not {}",
            value::type_name(rows)
        ));
    };
    let rows: Vec<i64> = items.to_vec();
    for row in rows {
        writerow(writer, row);
        if exc::pending() {
            return NONE;
        }
    }
    NONE
}

/// Materialises one plain writer row. Lists/tuples are the common case; a
/// string is iterated by scalar value (CPython treats it as an iterable of
/// one-character fields).
fn row_fields(row: i64) -> Result<Vec<String>, i64> {
    if let Some(items) = value::as_list(row).or_else(|| value::as_tuple(row)) {
        return Ok(items.iter().map(|&item| field_text(item)).collect());
    }
    if let Some(text) = value::as_str(row) {
        return Ok(text.chars().map(|ch| ch.to_string()).collect());
    }
    Err(type_error(format!(
        "writerow() argument must be a list or tuple, not {}",
        value::type_name(row)
    )))
}

/// Converts one field value to text; `None` becomes the empty string, every
/// other value uses `str()`.
fn field_text(v: i64) -> String {
    if v == NONE {
        String::new()
    } else {
        value::to_string(v)
    }
}

fn dict_writerow(writer: &value::CsvWriterState, row: i64) -> i64 {
    let Some(pairs) = value::as_dict(row) else {
        return type_error(format!(
            "writerow() argument must be a dict, not {}",
            value::type_name(row)
        ));
    };
    // SAFETY of the unwrap: `writerow` only routes here when fieldnames is
    // `Some`.
    let fieldnames = writer.fieldnames.as_deref().unwrap_or(&[]);
    for &(key, _) in pairs {
        let known = value::as_str(key)
            .map(|key| fieldnames.iter().any(|name| name.as_str() == key))
            .unwrap_or(false);
        if !known {
            return value_error("dict contains fields not in fieldnames");
        }
    }
    let mut fields = Vec::with_capacity(fieldnames.len());
    for name in fieldnames {
        let selected = pairs
            .iter()
            .find(|&&(key, _)| value::as_str(key) == Some(name.as_str()))
            .map(|&(_, item)| item)
            .unwrap_or(NONE);
        fields.push(field_text(selected));
    }
    write_fields(writer.file, &fields)
}

/// Writes the header record. Only a `DictWriter` has `writeheader`.
fn writeheader(obj: i64, writer: &value::CsvWriterState) -> i64 {
    let Some(fieldnames) = writer.fieldnames.as_deref() else {
        return attribute_error(obj, "writeheader");
    };
    write_fields(writer.file, fieldnames)
}

/// Serialises one record with `\r\n` and appends it to the target file.
fn write_fields(file: i64, fields: &[String]) -> i64 {
    let mut text = String::new();
    for (position, field) in fields.iter().enumerate() {
        if position > 0 {
            text.push(',');
        }
        if field
            .chars()
            .any(|ch| matches!(ch, ',' | '"' | '\r' | '\n'))
        {
            text.push('"');
            for ch in field.chars() {
                if ch == '"' {
                    text.push('"');
                }
                text.push(ch);
            }
            text.push('"');
        } else {
            text.push_str(field);
        }
    }
    text.push_str("\r\n");
    let Some(state) = value::as_file(file) else {
        return type_error("csv writer target is not a file object");
    };
    match state.write(&text) {
        Ok(_) => NONE,
        Err(error) => value::raise_file_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_quotes_commas_and_newlines() {
        assert_eq!(
            parse_rows("a,b\r\n\"c,d\",\"e\"\"f\"\n"),
            Ok(vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c,d".to_string(), "e\"f".to_string()],
            ])
        );
        assert_eq!(parse_rows(""), Ok(vec![]));
        assert_eq!(parse_rows("\n"), Ok(vec![vec![]]));
        assert_eq!(
            parse_rows("a\n\nb"),
            Ok(vec![vec!["a".to_string()], vec![], vec!["b".to_string()],])
        );
        assert_eq!(
            parse_rows("\"a\nb\",c"),
            Ok(vec![vec!["a\nb".to_string(), "c".to_string()]])
        );
        assert_eq!(
            parse_rows("a,"),
            Ok(vec![vec!["a".to_string(), String::new()]])
        );
        assert_eq!(parse_rows("\"\""), Ok(vec![vec![String::new()]]));
        assert!(parse_rows("\"a").is_err());
    }
}
