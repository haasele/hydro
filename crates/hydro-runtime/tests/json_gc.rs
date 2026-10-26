//! GC stress for the `json` module under a minimal collection threshold.
//!
//! This lives in its own test binary: `HYDRO_GC_THRESHOLD` is process-global and
//! is read when a thread's heap state is created, so keeping the test isolated
//! avoids changing the collection policy of other runtime tests.

use hydro_runtime::abi::*;

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn same(a: i64, b: i64) -> bool {
    module("operator", "eq", &[a, b]) == hydro_true()
}

#[test]
fn nested_json_survives_minimal_collection_threshold() {
    // Must be set before the first allocation on this thread: the heap state
    // reads the configuration when it is created (here, inside init).
    std::env::set_var("HYDRO_GC_THRESHOLD", "1");
    hydro_runtime_init();

    let source = "[[[\"a\",\"b\"],{\"k\":[1,{\"n\":[true,null,2.5]}]}],{\"z\":\"\\u00e9\"}]";
    let expected = "[[[\"a\", \"b\"], {\"k\": [1, {\"n\": [true, null, 2.5]}]}], \
                    {\"z\": \"\\u00e9\"}]";

    let parsed = module("json", "loads", &[text(source)]);
    assert_eq!(hydro_exc_pending(), 0);
    let dumped = module("json", "dumps", &[parsed]);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(same(dumped, text(expected)));

    // Build a larger array element by element: every nested parse and every
    // append happens amid constant collection pressure.
    let list = hydro_list_new(0, std::ptr::null());
    for index in 0..64 {
        let item_source = format!("{{\"i\": {index}, \"s\": \"v{index}\"}}");
        let item = module("json", "loads", &[text(&item_source)]);
        assert_eq!(hydro_exc_pending(), 0);
        hydro_list_append(list, item);
        assert_eq!(hydro_exc_pending(), 0);
    }
    assert_eq!(hydro_to_int(hydro_len(list)), 64i64 << 3);

    let dumped = module("json", "dumps", &[list]);
    assert_eq!(hydro_exc_pending(), 0);
    let roundtrip = module("json", "loads", &[dumped]);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(same(module("json", "dumps", &[roundtrip]), dumped));
}
