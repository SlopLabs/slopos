# toolchain

Pinned forks as patches, and recipes for third-party programs. Nothing here is built in place: scripts materialise patched trees under `third_party/` and stamp them. Read the `slopos-toolchain` skill first.

## Patches and pins

- Every patch is pinned by checksum in the `PIN` beside its tree (`PIN`, `compiler/PIN`, `cxx/PIN`, `crates/PIN`). Edit a patch and its `PIN` line in one commit, or the materialised tree is silently the previous one.
- After an edit, re-materialise (`just setup` for the sysroot, `just rustc-src` for the compiler tree) and run `scripts/check_toolchain_pin.sh`.
- The built-in `x86_64-unknown-slopos` spec in `compiler/` and `targets/x86_64-unknown-slopos.json` describe one machine. Change both together; `scripts/check_rustc_target.sh` holds them equal.
- `llvm/*.patch` stays out of `libcxx/`, `libcxxabi/` and `runtimes/`: the C++ runtime's stamp leaves these patches out on that promise.
- `libc/` is `MIT OR Apache-2.0` and renders slibc's headers. Nothing copyleft goes in.

## Recipes

- `recipes/<name>/recipe` pins an `https` release tarball by SHA-256, names its licence and template, and holds no file beside `recipe` but its declared `config` and `patch`es.
- A patch only teaches the target: named `NNNN-slopos-<what>.patch`, it only adds lines (a removed run is replaced in place by lines that start with it), patches each file in one section, and names SlopOS in every hunk or in the path of a file it creates.
- A build that needs any other edit to upstream is a slibc or kernel finding. Fix it there.
- A recipe under a licence this tree may not hold the text of (GPL-2.0-only, CDDL) carries no patch: a diff quotes upstream's lines as context.
- An `arg` picks among upstream's options and carries no flag, program, path or search root.
- A new recipe takes a `NOTICE.md` entry. Nothing GPL-2.0-only may link OpenSSL.

Checks: `scripts/check_recipes.sh`, `scripts/check_toolchain_pin.sh`, `scripts/check_cxx_pin.sh`, `just check-bootstrap-config`; all of them in `just ci-toolchain`. `docs/verification/toolchain-gates` says what each holds.
