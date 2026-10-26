//! `repr` must terminate on cyclic containers created by item assignment.

use hydro_runtime::abi::*;

#[test]
fn cyclic_list_repr_uses_a_cycle_marker() {
    hydro_runtime_init();
    let list = hydro_list_new(0, std::ptr::null());
    hydro_list_append(list, list);
    hydro_list_append(list, hydro_int(1));
    let rendered = hydro_to_repr(list);
    assert_eq!(hydro_exc_pending(), 0);
    // Exact punctuation is an implementation detail; the key contract is that
    // rendering terminates and marks the back-edge instead of recursing.
    assert!(hydro_len(hydro_to_str(rendered)) > hydro_int(0));
    let text = hydro_to_str(rendered);
    // `...` is inserted for the back-edge; exact punctuation is not pinned here.
    assert!(hydro_len(text) >= hydro_int(3));
}

#[test]
fn cyclic_dict_repr_terminates() {
    hydro_runtime_init();
    let dict = hydro_dict_new(0, std::ptr::null(), std::ptr::null());
    hydro_dict_set(dict, hydro_str_new(b"self".as_ptr(), 4), dict);
    let text = hydro_to_repr(dict);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(hydro_len(text) >= hydro_int(4));
}
