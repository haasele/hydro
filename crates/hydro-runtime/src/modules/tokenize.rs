//! `_tokenize`: the CPython C tokenizer surface, deliberately unimplemented.
//!
//! CPython 3.14's accelerator exposes one name, `TokenizerIter`, which turns
//! a source string into `tokenize.TokenInfo` records. Hydro's parser is not
//! the CPython tokenizer and there is no `TokenInfo` type in this runtime, so
//! constructing the iterator raises `NotImplementedError`; no token is ever
//! fabricated. Pure-Python `tokenize` code that calls
//! `_generate_tokens_from_c_tokenizer` therefore fails loudly instead of
//! producing tokens that do not describe the compiled program.
//!
//! The module itself is importable so probe code (`import _tokenize`) and the
//! `from _tokenize import TokenizerIter` import in `Lib/tokenize.py` resolve;
//! every other name stays absent and raises `AttributeError`.

use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR};

/// Dispatches a native `_tokenize` function.
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        "TokenizerIter" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "_tokenize.TokenizerIter() is not implemented: Hydro does not run the CPython \
             C tokenizer and has no tokenize.TokenInfo values",
        ),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_tokenize' has no attribute '{function}'"),
        ),
    }
}
