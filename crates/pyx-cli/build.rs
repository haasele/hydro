//! Build script for the `pyx` CLI.
//!
//! `pyx run` loads CPython stable-ABI (abi3) extension modules with `dlopen`.
//! Such a module resolves the CPython C API against the symbols of the
//! process that loaded it, so the `pyx` executable has to export its own
//! (dynamically linked) definitions: `--export-dynamic` adds them to the
//! dynamic symbol table. No `libpython` is linked; the extension only uses the
//! limited API that the host interpreter provides.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic");
    }
}
