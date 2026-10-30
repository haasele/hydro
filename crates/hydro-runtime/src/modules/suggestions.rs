//! `_suggestions`: the close-name lookup behind CPython's "did you mean"
//! hints.
//!
//! `_generate_suggestions(candidates, item, /)` returns the closest candidate
//! that fits CPython's one-third edit-distance budget, or `None`.
//! Distances are Levenshtein over UTF-8 bytes (not code points) with an
//! asymmetric cost table: any edit costs [`MOVE_COST`], while a pure ASCII
//! case flip costs [`CASE_COST`]. Candidates must be an exact `list` of
//! strings; the check order matches the Argument Clinic wrapper (the `item`
//! string conversion runs before the list validation).
//!
//! The algorithm mirrors `Python/suggestions.c` exactly, including its
//! quirks: an equal candidate is skipped (never returned), the search gives
//! up once the list holds [`MAX_CANDIDATE_ITEMS`] entries, strings longer
//! than [`MAX_STRING_SIZE`] bytes after common-affix trimming never match,
//! and a candidate that merely fits the shrinking budget can still win over
//! a nearer candidate that comes later.

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

/// `_Py_CalculateSuggestions` refuses to scan a namespace this large.
const MAX_CANDIDATE_ITEMS: usize = 750;
/// Strings with a trimmed length beyond this are treated as too far apart.
const MAX_STRING_SIZE: usize = 40;
/// Cost of an insertion, deletion or non-case substitution.
const MOVE_COST: usize = 2;
/// Cost of changing only the case of an ASCII letter.
const CASE_COST: usize = 1;

/// Byte cost of substituting `a` with `b`.
///
/// Two bytes that agree on their low five bits are either equal, an ASCII
/// case flip, or a different character that happens to share those bits; the
/// first two get [`CASE_COST`], everything else [`MOVE_COST`].
fn substitution_cost(a: u8, b: u8) -> usize {
    if a & 31 != b & 31 {
        return MOVE_COST;
    }
    if a == b {
        return 0;
    }
    let lowered = |c: u8| {
        if c.is_ascii_uppercase() {
            c + (b'a' - b'A')
        } else {
            c
        }
    };
    if lowered(a) == lowered(b) {
        CASE_COST
    } else {
        MOVE_COST
    }
}

/// Levenshtein distance between two UTF-8 byte slices, bailing out with
/// `max_cost + 1` as soon as a match is impossible.
///
/// `buffer` must hold at least [`MAX_STRING_SIZE`] entries; only the first
/// `min(a.len(), b.len())` entries are written.
fn levenshtein_distance(a: &[u8], b: &[u8], max_cost: usize, buffer: &mut [usize]) -> usize {
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut a_end = a.len();
    let mut b_end = b.len();
    while a_end > start && b_end > start && a[a_end - 1] == b[b_end - 1] {
        a_end -= 1;
        b_end -= 1;
    }
    let a = &a[start..a_end];
    let b = &b[start..b_end];
    let a_size = a.len();
    let b_size = b.len();
    if a_size == 0 || b_size == 0 {
        return (a_size + b_size) * MOVE_COST;
    }
    if a_size > MAX_STRING_SIZE || b_size > MAX_STRING_SIZE {
        return max_cost + 1;
    }
    // The row buffer is indexed by the shorter string. Selecting (rather
    // than swapping) the slices keeps their independently inferred
    // lifetimes unified by the `if` expression's result type.
    let (a, b, a_size, b_size) = if a_size <= b_size {
        (a, b, a_size, b_size)
    } else {
        (b, a, b_size, a_size)
    };
    if (b_size - a_size) * MOVE_COST > max_cost {
        return max_cost + 1;
    }

    let mut cost = MOVE_COST;
    for slot in &mut buffer[..a_size] {
        *slot = cost;
        cost += MOVE_COST;
    }

    let mut result = 0;
    for (b_index, &code) in b.iter().enumerate() {
        // cost(b[:b_index], a[:0])
        let mut distance = b_index * MOVE_COST;
        result = distance;
        let mut minimum = usize::MAX;
        for (index, &a_byte) in a.iter().enumerate() {
            // cost(b[:b_index], a[:index])
            let substitute = distance + substitution_cost(code, a_byte);
            // cost(b[:b_index], a[:index+1]) from the previous row
            distance = buffer[index];
            // cost(b[:b_index+1], a[:index])
            let insert_delete = result.min(distance) + MOVE_COST;
            result = insert_delete.min(substitute);
            buffer[index] = result;
            if result < minimum {
                minimum = result;
            }
        }
        if minimum > max_cost {
            return max_cost + 1;
        }
    }
    result
}

/// `_Py_CalculateSuggestions`: closest candidate within the shrinking budget.
fn calculate_suggestion(candidates: &[i64], name: &str) -> Option<i64> {
    if candidates.len() >= MAX_CANDIDATE_ITEMS {
        return None;
    }
    let name_bytes = name.as_bytes();
    let mut suggestion = None;
    let mut suggestion_distance = usize::MAX;
    let mut buffer = [0usize; MAX_STRING_SIZE];
    for &candidate in candidates {
        // Every element was validated as `str` before this point.
        let candidate_text = value::as_str(candidate).expect("validated candidate string");
        if name == candidate_text {
            continue;
        }
        let candidate_bytes = candidate_text.as_bytes();
        // No more than a third of the involved bytes should change.
        let mut max_distance = (name_bytes.len() + candidate_bytes.len() + 3) * MOVE_COST / 6;
        // Skip candidates that cannot beat the current best.
        max_distance = max_distance.min(suggestion_distance - 1);
        let distance = levenshtein_distance(name_bytes, candidate_bytes, max_distance, &mut buffer);
        if distance > max_distance {
            continue;
        }
        if suggestion.is_none() || distance < suggestion_distance {
            suggestion = Some(candidate);
            suggestion_distance = distance;
        }
    }
    suggestion
}

/// `_generate_suggestions(candidates, item, /)`.
fn generate_suggestions(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "_generate_suggestions expected 2 arguments, got {}",
                args.len()
            ),
        );
    }
    let candidates = args[0];
    let item = args[1];
    // Argument Clinic converts the `item: unicode` parameter before the
    // function body runs, so a bad `item` wins over a bad `candidates`.
    let Some(name) = value::as_str(item) else {
        let type_name = if item == NONE {
            "None"
        } else {
            value::type_name(item)
        };
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("_generate_suggestions() argument 2 must be str, not {type_name}"),
        );
    };
    let Some(candidates) = value::as_list(candidates) else {
        return exc::raise_with_message(TYPE_ERROR, "candidates must be a list");
    };
    for &candidate in candidates {
        if value::as_str(candidate).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                "all elements in 'candidates' must be strings",
            );
        }
    }
    match calculate_suggestion(candidates, name) {
        Some(candidate) => candidate,
        None => NONE,
    }
}

/// Dispatches `module.function(...)` calls for `_suggestions`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "_generate_suggestions" => generate_suggestions(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_suggestions' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, list_new, str_alloc};

    fn text(s: &str) -> i64 {
        str_alloc(s.to_string())
    }

    fn list(items: &[i64]) -> i64 {
        list_new(items.to_vec())
    }

    fn suggest(candidates: &[&str], item: &str) -> i64 {
        let values: Vec<i64> = candidates.iter().map(|s| text(s)).collect();
        call("_generate_suggestions", &[list(&values), text(item)])
    }

    fn matched(result: i64) -> Option<String> {
        value::as_str(result).map(|s| s.to_string())
    }

    fn message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn returns_closest_candidate_within_budget() {
        exc::init();
        assert_eq!(
            matched(suggest(&["abc", "abd"], "ab")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            matched(suggest(&["hello", "world"], "hellp")).as_deref(),
            Some("hello")
        );
        // Equal candidates are skipped rather than returned: `"abc"` is equal
        // to the item, so the only remaining candidate `"ab"` (distance 2)
        // wins even though it is farther away.
        assert_eq!(
            matched(suggest(&["abc", "ab"], "abc")).as_deref(),
            Some("ab")
        );
        // A candidate that fits the one-third budget wins immediately; later
        // candidates cannot replace it because the budget shrinks by one.
        assert_eq!(
            matched(suggest(&["ab", "acb"], "ab")).as_deref(),
            Some("acb")
        );
        // A strictly nearer candidate can still replace an earlier one: the
        // case flip in `"abC"` costs 1, the trailing insert in `"abcd"` 2.
        assert_eq!(
            matched(suggest(&["abcd", "abC"], "abc")).as_deref(),
            Some("abC")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn case_flips_are_cheap() {
        exc::init();
        // Three ASCII case flips cost 3, exactly the one-third budget for
        // three-byte strings, and the first accepted candidate wins.
        assert_eq!(
            matched(suggest(&["ABC", "abc"], "abc")).as_deref(),
            Some("ABC")
        );
        // The lookup is byte-based: `é` is two UTF-8 bytes, so `"héllo"`
        // still comes within budget of `"hello"`, while the equal candidate
        // `"hello"` is skipped.
        assert_eq!(
            matched(suggest(&["héllo", "hello"], "hello")).as_deref(),
            Some("héllo")
        );
        assert_eq!(
            matched(suggest(&["héllo", "hello"], "héllo")).as_deref(),
            Some("hello")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn distant_candidates_are_ignored() {
        exc::init();
        assert_eq!(suggest(&["a", "b"], "zzz"), NONE);
        assert_eq!(suggest(&["abcd"], "ab"), NONE);
        assert_eq!(suggest(&[], "x"), NONE);
        // `"abc"` alone equals the item and is skipped.
        assert_eq!(suggest(&["abc"], "abc"), NONE);
        // Empty strings are at least two edits away from any non-empty name.
        assert_eq!(suggest(&["", "a"], ""), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn long_strings_and_large_lists_bail_out() {
        exc::init();
        // 40-byte strings are still compared (distance 2 fits the budget),
        // but 41 bytes exceed MAX_STRING_SIZE after affix trimming.
        let forty = "ab".repeat(20);
        let forty_flipped = "ba".repeat(20);
        assert_eq!(
            matched(suggest(&[forty_flipped.as_str()], &forty)).as_deref(),
            Some(forty_flipped.as_str())
        );
        let forty_one = "ab".repeat(20) + "a";
        let forty_one_flipped = "ba".repeat(20) + "b";
        assert_eq!(suggest(&[forty_one_flipped.as_str()], &forty_one), NONE);

        // A list with MAX_CANDIDATE_ITEMS entries is never scanned.
        let mut candidates = vec![text("ab")];
        candidates.extend((1..750).map(|_| text("zzz")));
        let item = text("a");
        assert_eq!(
            call("_generate_suggestions", &[list(&candidates), item]),
            NONE
        );
        // One fewer entry is scanned, and `"ab"` is within budget of `"a"`.
        candidates.pop();
        assert_eq!(
            matched(call("_generate_suggestions", &[list(&candidates), item])).as_deref(),
            Some("ab")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn argument_errors_match_cpython() {
        exc::init();
        assert_eq!(call("_generate_suggestions", &[text("x")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "_generate_suggestions expected 2 arguments, got 1"
        );
        exc::clear();

        // `item` is converted before the candidate list is validated.
        let integer = encode_int(7).expect("representable");
        assert_eq!(call("_generate_suggestions", &[text("abc"), integer]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "_generate_suggestions() argument 2 must be str, not int"
        );
        exc::clear();

        assert_eq!(
            call("_generate_suggestions", &[text("abc"), text("abc")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "candidates must be a list");
        exc::clear();

        assert_eq!(
            call(
                "_generate_suggestions",
                &[list(&[text("ok"), integer]), text("ok")]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(message(), "all elements in 'candidates' must be strings");
        exc::clear();

        assert_eq!(call("unknown", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        assert_eq!(
            message(),
            "module '_suggestions' has no attribute 'unknown'"
        );
        exc::clear();
        assert!(!exc::pending());
    }
}
