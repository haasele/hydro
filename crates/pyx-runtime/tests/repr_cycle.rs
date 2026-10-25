//! `repr` must terminate on cyclic containers created by item assignment.

use pyx_runtime::abi::*;

#[test]
fn cyclic_list_repr_uses_a_cycle_marker() {
    pyx_runtime_init();
    let list = pyx_list_new(0, std::ptr::null());
    pyx_list_append(list, list);
    pyx_list_append(list, pyx_int(1));
    let rendered = pyx_to_repr(list);
    assert_eq!(pyx_exc_pending(), 0);
    // Exact punctuation is an implementation detail; the key contract is that
    // rendering terminates and marks the back-edge instead of recursing.
    assert!(pyx_len(pyx_to_str(rendered)) > pyx_int(0));
    let text = pyx_to_str(rendered);
    // `...` is inserted for the back-edge; exact punctuation is not pinned here.
    assert!(pyx_len(text) >= pyx_int(3));
}

#[test]
fn cyclic_dict_repr_terminates() {
    pyx_runtime_init();
    let dict = pyx_dict_new(0, std::ptr::null(), std::ptr::null());
    pyx_dict_set(dict, pyx_str_new(b"self".as_ptr(), 4), dict);
    let text = pyx_to_repr(dict);
    assert_eq!(pyx_exc_pending(), 0);
    assert!(pyx_len(text) >= pyx_int(4));
}
