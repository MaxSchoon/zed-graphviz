# graphviz.wasm

The Graphviz `dot` command compiled to WebAssembly (`wasm32-wasip1`, a WASI
preview 1 command module). The `graphviz-preview` language server embeds it
and runs it with wasmtime when no `dot` executable is installed.

## Build

```sh
./build.sh
```

This writes `graphviz.wasm` and `graphviz.wasm.sha256` into this directory.
Neither file is checked in; the release workflow builds them.

Requirements: `bash`, `curl`, `tar` (with xz support), `patch`, `make` and
`cmake`. Everything else is
downloaded and checked against the SHA-256 sums pinned at the top of
`build.sh`:

| Input | Version |
|---|---|
| Graphviz | 16.1.0 |
| Expat | 2.9.0 |
| wasi-sdk (clang 23.1.0, wasi-libc `2e6fb9d8ee0c`) | 34.0 |

Set `WASI_SDK_PATH` to reuse an existing wasi-sdk 34.0 install,
`GRAPHVIZ_WASM_WORK` to move the download and build trees (default
`.work/`), and `JOBS` to change the number of parallel make jobs (default 2).
A clean build takes about a minute.

The build is reproducible: it starts from pristine sources every time and
maps build paths out of the output, so two builds with the same inputs
produce the same file regardless of the directory they run in. For
reference, a build on macOS (arm64) produced:

```
d1ad8ec50a4c76a486edc41294028b97a98f75a918beb6b45965f0ba1783d210  graphviz.wasm
```

To upgrade an input, change its version and SHA-256 in `build.sh`, check that
the patches still apply, and update `THIRD_PARTY_NOTICES.md`.

## Patches

`patches/*.patch` are applied in order to the Graphviz sources:

- `0001-core-include-unistd-for-read.patch`: adds a missing `#include` that
  only matters on platforms without `mmap`.
- `0002-sfdp-no-spurious-triangulation-error.patch`: without GTS, every
  `sfdp` layout reported "Graphviz not built with triangulation library"
  and exited with status 1, although sfdp had not asked for a
  triangulation.

Both are candidates for upstream.

## Interface

The module behaves like `dot`:

- arguments: `dot -Tsvg -K<engine> [-G<name>=<value>] [-N...] [-E...]`
  (the first argument is the program name and is ignored);
- input: DOT source on standard input;
- output: SVG on standard output; warnings and errors on standard error;
- exit status 0 on success and non-zero on error (syntax errors, bad HTML-like
  labels, unknown engines or formats). Graphviz may still write partial
  output before failing, so check the status first.

Layout engines: `dot`, `neato`, `fdp`, `sfdp`, `circo`, `twopi`, `osage`,
`patchwork` (plus `nop`, `nop1`, `nop2`). Output formats include `svg`,
`svg_inline`, `dot`, `xdot`, `json`, `plain` and `ps`; there are no bitmap
formats. HTML-like labels and record shapes work.

The module needs no preopened directories or environment. Without
filesystem access, `image=` and `shapefile=` attributes cannot be resolved
and produce a warning.

Differences from a typical desktop build:

- No text-layout library is compiled in, so text sizes come from Graphviz's
  built-in per-character width tables (Times, Helvetica/Arial, Courier,
  DejaVu Sans, Verdana, Georgia, Trebuchet MS, Consolas, Calibri, Open Sans
  and Nunito, each with bold and italic variants). Any other font name gets
  Times metrics and a one-time warning on standard error. Text widths differ
  from a pango-based `dot` by up to about 10%, and line height is 1.2 times
  the font size.
- No GTS, so `overlap=prism` (sfdp's default overlap removal in builds with
  GTS) is not available; sfdp lays out without it, and the other `overlap`
  modes still work.
- A failed internal assertion aborts the module, which surfaces as a
  WebAssembly trap rather than an exit status.

## Licence

`graphviz.wasm` and the files in `patches/` are covered by the Eclipse Public
License 2.0, not by this repository's MIT licence. See
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for all components, their
licences and what a release that ships the module must include.
