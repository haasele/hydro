# Reproducible AOT builds

Scope: the artifacts produced by `pyx build` (see `.github/workflows/release.yml`
for the packaged set: `pyx`, `libpyx_runtime.a`, docs, tarball + `.sha256`).
`pyx run` (JIT) is not an artifact and is not covered.

## Contract

A build is reproducible **on one host** when these inputs are identical:

| Input | Where it enters |
|---|---|
| source text of the entry module | frontend → IR → codegen |
| every `.py` module reachable from the entry's directory | `check_file` import resolution; import cycles and site-packages also depend on `PYX_SITE_PACKAGES`, `VIRTUAL_ENV`, `CONDA_PREFIX`, `PYTHONPATH` |
| the source name exactly as spelled on the command line | `Program::source_name`, embedded by `build_pyx_main` through `make_bytes_slot` (stack immediates, visible as `.text` size changes) |
| the `pyx` executable | codegen (Cranelift), frontend |
| `libpyx_runtime.a` | static link input (`PYX_RUNTIME_LIB` or a candidate next to the executable / `target/{release,debug}`) |
| C toolchain (`CC`, default `cc`) and target libc/loader | link step: `cc <object> <libpyx_runtime.a> -o <output> -Wl,--export-dynamic -lpthread -ldl -lm` |

The output path does **not** participate, and neither does the build cache:
`.pyx/cache` stores the already-linked binary under a content key (source,
modules, compiler metadata, runtime archive identity) and copies it back
unchanged. A cache hit therefore cannot change artifact bytes.

Given identical inputs, the ELF is byte-identical. Verified on 2026-10-25
(Linux x86_64, `pyx 0.1.0` release, GCC 16.2.1):

```
$ ./scripts/repro_check.sh examples/hello.py
ok: uncached builds (different output paths) identical (25f7d4e3…77e66)
ok: uncached builds behavior identical (exit 0)
ok: cached builds (fresh link vs cache hit) identical (25f7d4e3…77e66)
ok: cached vs uncached build identical (25f7d4e3…77e66)
```

Two uncached builds of `examples/modules.py` (a program with an import, a
class and functions) also matched, as did `fib`, `closures`, `collections`,
`fstrings` and `classes`.

### Path spelling is part of the input

`pyx build hello.py` and `pyx build /abs/path/hello.py` compile the same text
but produce different bytes: the source name appears as immediates in
`pyx_main`, so the absolute form changed `.text` from 4,289,597 to 4,289,709
bytes and the sha256 from `2e63febe…` to `9c151704…` (2026-10-25). Treat the
path spelling, not just the file contents, as an input; `scripts/repro_check.sh`
resolves the source to one absolute path and reuses it for every build.

## Verifying

```bash
cargo build --release -p pyx-cli -p pyx-runtime   # once; the check never runs cargo
./scripts/repro_check.sh                          # examples/hello.py
./scripts/repro_check.sh examples/modules.py
./scripts/repro_check.sh --pyx target/debug/pyx examples/fib.py
```

The script builds the source four times into separate output directories: two
uncached builds, then a fresh cached link and a cache-hit build. It compares
sha256 sums byte-for-byte, runs both binaries and compares stdout, stderr and
exit status, and prints the first differing offsets with both sizes on failure.
Exit codes: `0` identical, `1` nondeterminism or build/run failure, `2` no
usable `pyx`/`libpyx_runtime.a`. `--keep` preserves the temporary artifacts.
The release workflow runs it between the release build and packaging, so a
tagged release is only produced from a host where this check passes.

## Not claimed: cross-host reproducibility

No claim is made that a rebuild on another machine, distribution, linker or
Rust release produces the same bytes. The link step is the host `cc` against
the host libc, so `.comment` (and thus the content-derived build-id) changes
with the toolchain, and the repository does not pin a sysroot. The published
`.sha256` files attest the uploaded artifact, not a rebuild elsewhere. Precise
work remaining:

- **TODO(repro-01, cross-host):** add a container image that pins rustc
  (`rust-toolchain.toml` + image digest), `Cargo.lock`, `cc`/`ld` and glibc;
  build the tarball twice from the digest and compare `sha256sum`; record the
  digest and `cc --version` in the release notes. Until then
  `docs/REPRODUCIBLE.md` describes same-host determinism only.
- **TODO(repro-02, path normalization):** add a `--source-name` /
  repo-relative normalization for `Program::source_name` so artifacts do not
  depend on how the caller spelled the path. This touches the frontend
  `Program` field and the build command; the contract above deliberately
  documents the current behaviour instead of pretending otherwise.
- **TODO(repro-03, toolchain provenance):** record the resolved `CC` version
  and the runtime archive's sha256 in the release manifest. Today the cache key
  folds in `pyx`/`libpyx_runtime.a` size and mtime, which is sufficient for a
  local cache but is not provenance.

## Cache and re-baselining notes

- The cache lives in `./.pyx/cache` relative to the working directory and is
  only an optimization; `--no-cache` forces a direct link.
- Changing source, imported modules, the `pyx` executable or the runtime
  archive invalidates the key; there is no manual cache-clearing step.
- `scripts/repro_check.sh` builds from a temporary working directory, so it
  never writes into the checkout's `.pyx/`.
