//! Build script for the Hydro runtime.
//!
//! Compiles `src/capi/parse.c`, which provides the single variadic CPython
//! entry point (`PyArg_ParseTuple`) that cannot be expressed in stable Rust.
//! The file includes no CPython headers: it only needs `<stdarg.h>` and
//! `<stddef.h>` and forwards each conversion to a Rust accessor.
//!
//! `dlopen` lives in libc on modern glibc; link `libdl` explicitly on Linux so
//! older toolchains resolve it as well.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/capi/parse.c");

    cc::Build::new()
        .file("src/capi/parse.c")
        .warnings(true)
        .compile("hydro_capi_parse");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=dl");
        // Integration tests load the C fixture with `dlopen`; the extension's
        // undefined `Py*` symbols must resolve against the test binary, so its
        // symbols have to be in the dynamic symbol table.
        println!("cargo:rustc-link-arg-tests=-Wl,--export-dynamic");
    }
}
