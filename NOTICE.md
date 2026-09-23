# SlopOS — Copyright and Third-Party Notices

Copyright © 2025–2026 The SlopOS Authors

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.

The full license text is in [`LICENSE`](LICENSE).

## Authorship and provenance

SlopOS is developed with heavy use of AI coding assistants under continuous
human direction, architecture, review, integration, debugging, and
modification. Copyright is claimed in the human-authored expression and in the
selection, coordination, and arrangement of the work as a whole.

No code was copied from any other kernel or operating system. Where in-tree
comments name other systems — Linux, Asterinas, Redox, CortenMM, Fuchsia,
FreeBSD, illumos, seL4, Rust for Linux, musl, dlmalloc, smoltcp and others —
they identify **conceptual influence, published specifications, or ABI
compatibility targets**, not copied source. Interface constants (syscall
numbers, `errno` values, ioctl codes, struct layouts, hardware register
offsets) are reproduced where compatibility requires it; those are interface
facts rather than authorship.

The TLS stack in [`tls-core/`](tls-core/) is written from the specifications it
implements — RFC 8446, 8439, 7748, 8017, 5280 and 6125, FIPS 180-4, 186-5 and
197, NIST SP 800-38D and SEC 1. Its AES S-box is the Boolean circuit Boyar and
Peralta published in "A new combinational logic minimization technique with
applications to cryptology" (2010). Its test vectors were generated with
pyca/cryptography, and its interop tests run it against OpenSSL's `s_server`
and `s_client`; no code from either is incorporated.

## Pinned Rust standard library, `libc` and compiler forks

SlopOS's userland target `x86_64-unknown-slopos` is built against three pinned
forks, all of upstream Rust projects licensed `MIT OR Apache-2.0`:

- a fork of [`rust-lang/rust`](https://github.com/rust-lang/rust)'s
  `library/` tree, at the commit `rust-toolchain.toml` pins, adding the
  `slopos` target to `library/std`: its platform allowlist, the per-target
  `os/slopos/` and `sys/random/slopos.rs` modules, and the `cfg` sites a
  unix-family target has to appear in;
- a fork of [`rust-lang/libc`](https://github.com/rust-lang/libc), adding
  `src/unix/slopos/` — the `libc` bindings for SlopOS's C library;
- a fork of [`rust-lang/rust`](https://github.com/rust-lang/rust)'s compiler
  tree, at the same commit, making `x86_64-unknown-slopos` a built-in target:
  `compiler/rustc_target/src/spec/{base/slopos.rs,targets/x86_64_unknown_slopos.rs}`,
  the `Os`/`Env` variants and the match arms that follow from them, bootstrap's
  stage0 target list, the tier-3 documentation page, and the test fixtures
  those change.

None of the three is vendored into this repository. What is tracked is the diff:
`toolchain/PIN` records the channel, the pinned `libc` crate version and its
checksum, and a checksum per patch; `toolchain/compiler/PIN` records the rustc
source tarball's checksum and its own patch's; and `toolchain/{rust,libc,compiler}/`
carry the patches themselves. `scripts/make_slopos_sysroot.sh` materialises the
first two into an owned sysroot at `third_party/rust-slopos/` and
`scripts/make_rustc_src.sh` materialises the third into
`third_party/slopos-rustc-src/`, both at build time, and
`scripts/check_toolchain_pin.sh` fails the build if what is materialised has
drifted from what is pinned. Upstream's code reaches a SlopOS build only
through those steps. All three patches are licensed `MIT OR Apache-2.0` rather
than GPL-3.0-or-later, because they are written to be contributed upstream
under the tier-3 target policy.

Authorship within the patches is split. New work, © 2025–2026 The SlopOS
Authors, is the `libc` fork's `src/unix/slopos/` module, the `target_os =
"slopos"` arms the std patch adds to lists that were already there,
`std/src/sys/random/slopos.rs`, and the compiler fork's two spec modules and
its documentation page — the modules following the shape of upstream's
`spec/base/redox.rs` and `spec/targets/x86_64_unknown_redox.rs`, the page
following `src/doc/rustc/src/platform-support/TEMPLATE.md`. The three files
the std patch creates under
`std/src/os/slopos/` are instead derived from upstream's own
`std/src/os/redox/` ones: `fs.rs` is `os/redox/fs.rs` with `redox` renamed to
`slopos` and nothing else changed, and `raw.rs` is `os/redox/raw.rs` with
SlopOS's own type widths and `stat` padding. Those are substantial portions of
the upstream work, so The Rust Project Contributors' copyright in them is
retained as MIT's notice-retention clause requires — recorded here and in the
`MIT OR Apache-2.0` licence the patch elects, upstream's own library files
carrying no per-file copyright header to carry over. The unmodified upstream
sources remain © The Rust Project Contributors and © The `rust-lang/libc`
Developers respectively.

## Components linked into SlopOS binaries

Each component below is dual-licensed `MIT OR Apache-2.0` unless noted. **SlopOS
elects the MIT option** for all of them. The MIT permission notice is reproduced
once at the end of this section and applies to every entry.

| Component | Version | Copyright |
|---|---|---|
| [`bitflags`](https://github.com/bitflags/bitflags) | 2.13.2 | Copyright (c) 2014 The Rust Project Developers |
| [`libc`](https://github.com/rust-lang/libc) | 0.2.189, forked | Copyright (c) 2014 The Rust Project Developers |
| [`libm`](https://github.com/rust-lang/libm) | 0.2.16 | **MIT only** — see the note below |
| [`limine`](https://github.com/limine-bootloader/limine-rs) | 0.6.5 | Copyright © 2026 Julian Scheffers |
| [`paste`](https://github.com/dtolnay/paste) | 1.0.15 | David Tolnay (upstream ships no copyright line) |
| [`gimli`](https://github.com/gimli-rs/gimli) | 0.33.0 | Copyright (c) 2015 The Rust Project Developers |
| [`unwinding`](https://github.com/nbdd0121/unwinding/) | 0.2.9 | Gary Guo (upstream ships no copyright line) |
| Rust `core`, `alloc`, `std` | pinned nightly, `std` forked | Copyright © The Rust Project Contributors |
| [`hashbrown`](https://github.com/rust-lang/hashbrown), linked by `std` | 0.17.1 | Copyright (c) 2016 Amanieu d'Antras |
| [`rustc-demangle`](https://github.com/rust-lang/rustc-demangle), linked by `std` | 0.1.28 | Copyright (c) 2014 Alex Crichton |

`gimli` and `unwinding` are vendored verbatim under [`vendor/`](vendor/); each
directory retains its upstream `LICENSE-MIT` and `LICENSE-APACHE`.

`libm` 0.2.16 is licensed **MIT only** — no Apache option is available — and
carries these copyrights, retained from its own `LICENSE.txt`:

```
Copyright (c) 2018 Jorge Aparicio
```

Portions of `libm` derive from musl libc (<https://www.musl-libc.org/>),
which carries `Copyright © 2005-2020 Rich Felker, et al.`, and from the
CORE-MATH project. musl's own notice records that much of the math library code
is `Copyright © 1993,2004 Sun Microsystems`, `© 2003-2011 David Schultz`,
`© 2003-2009 Steven G. Kargl`, `© 2003-2009 Bruce D. Evans`, `© 2008 Stephen
L. Moshier`, or `© 2017-2018 Arm Limited`, as labelled in the individual source
files.

The MIT License permission notice, applying to every component in this section:

```
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

## The C++ runtime

SlopOS's C++ runtime is [LLVM's `libc++` and
`libc++abi`](https://github.com/llvm/llvm-project), cross-built from the
pinned source release for `x86_64-unknown-slopos` and linked into a single
`libc++.so`. It is **Apache License 2.0 with the LLVM exception**, which is
compatible with GPL-3.0-or-later:

```
Copyright (c) 2009-2019 by the contributors listed in CREDITS.TXT (llvm-project)
Licensed under the Apache License, Version 2.0, with LLVM Exceptions.
See https://llvm.org/LICENSE.txt for license information.
SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
```

llvm-project's sources are not vendored into this repository. What is tracked
is the diff: `toolchain/cxx/PIN` records the release, its download URL, its
checksum and a checksum per patch, and `toolchain/llvm/` carries the patches
themselves. `scripts/make_slopos_cxx.sh` fetches the release and builds the
runtime into `third_party/slopos-cxx/`; `scripts/make_slopos_llvm_src.sh`
materialises the whole tree with the port applied into
`third_party/llvm-project-<version>.src/`; and `scripts/check_cxx_pin.sh`
fails the build if either has drifted from what is pinned. Upstream's code
reaches a SlopOS build only through those steps. The runtime reaches the
**tests** image only: `libc++.so` as a file, and `libc++.a` as the C++ half of
the statically linked `cxx_static_probe`. Both projects' license texts ship
beside it, in `/usr/share/licenses/libc++/`, the way the OFL fonts carry
theirs.

`toolchain/llvm/` is SlopOS's port of llvm-project: the two places LLVM
dispatches on the host OS without a default a new one can take, the
`Triple::SlopOS` enumerator and the switches that have to name it, and
clang's `SlopOSTargetInfo`. It is licensed **`Apache-2.0 WITH
LLVM-exception`** rather than GPL-3.0-or-later, because it is written to be
contributed upstream. Every line it adds is new work, © 2025–2026 The SlopOS
Authors, `SlopOSTargetInfo` following the shape of the `OSTargetInfo`
specializations it sits between in the same file; the context lines a diff
carries remain © the llvm-project contributors. The patch touches neither
`libcxx/` nor `libcxxabi/` — `scripts/check_cxx_pin.sh` fails if one ever
does — so nothing built from it reaches a shipped or a tests image. It builds
a compiler, not an artifact this project distributes.

The runtime is built with the time-zone database turned off — there is no zone
data on this system to answer from — and with localization, wide characters,
the filesystem library and the random device on, because LLVM's own sources
reach all four.

## The self-hosted toolchain

`just toolchain` cross-builds rustc, cargo, LLVM, clang and lld from the pinned
rustc source tarball, and `scripts/build_devdisk.sh` stages the result on the
dev disk (`fs/assets/ext2-devdisk.img`), a volume built for development and not
distributed. Each keeps its upstream licence: rustc and cargo `MIT OR
Apache-2.0`, LLVM, clang and lld `Apache-2.0 WITH LLVM-exception`.

Beyond the forks above, that build applies patches of the same shape, each
written to be contributed upstream and licensed as the project it patches: the
cargo fork (`toolchain/cargo/`, `MIT OR Apache-2.0`), rustc's own llvm-project
port (`toolchain/llvm-rustc/`, `Apache-2.0 WITH LLVM-exception`), and
`target_os = "slopos"` ports of crates the compiler and cargo depend on
(`toolchain/crates/`): `getrandom`, `errno` and `stacker` (`MIT OR
Apache-2.0`), `libloading` (ISC), `nix` (MIT) and `rustix` (`Apache-2.0 WITH
LLVM-exception OR Apache-2.0 OR MIT`). Every added line is © 2025–2026 The
SlopOS Authors; the crates themselves remain © their authors, and none is
vendored into this repository.

`tools/kallsyms`, which builds the kernel's symbol table on the host and in
the guest, carries a Rust v0 symbol demangler derived from LLVM's
`llvm/lib/Demangle/RustDemangle.cpp`, and its test data is LLVM's
`llvm/test/Demangle/rust.test`, copied unmodified: © the llvm-project
contributors, `Apache-2.0 WITH LLVM-exception`, notice as in the C++ runtime
section above. The tool runs at build time and is linked into no SlopOS image.

## Components distributed on the SlopOS ISO

### Limine bootloader

`limine-bios.sys`, `limine-bios-cd.bin`, `limine-uefi-cd.bin`, `BOOTX64.EFI` and
`BOOTIA32.EFI` are distributed on the SlopOS ISO. Limine is a separate and
independent work aggregated onto the same medium; its inclusion does not place
it under the GNU GPL, and the GPL does not apply to it.

```
Copyright (C) 2019-2026 Mintsuki and contributors.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### Mozilla CA certificate bundle

[`assets/certs/ca-certificates.crt`](assets/certs/ca-certificates.crt),
installed as `/etc/ssl/certs/ca-certificates.crt`, is the root certificate
list of the Mozilla CA Certificate Program, as extracted from NSS's
`certdata.txt` by curl's `mk-ca-bundle.pl` and published at
<https://curl.se/docs/caextract.html> (the extraction named in the file's own
header, checked against the digest curl.se publishes by
`scripts/update_ca_bundle.sh`, which is the only thing that replaces it). It is
data aggregated alongside SlopOS, not part of it, and is distributed unmodified
under the Mozilla Public License 2.0, whose full text travels with it in
[`assets/certs/MPL-2.0.txt`](assets/certs/MPL-2.0.txt) and on the installed
images at `/usr/share/licenses/ca-certificates/MPL-2.0.txt`.

### Fonts

Both families are shipped unmodified as `.ttf` files in `/usr/share/fonts/`,
bundled alongside SlopOS rather than merged into it. Neither declares a Reserved
Font Name. Each family's full SIL Open Font License 1.1 text travels with it, in
[`assets/fonts/`](assets/fonts/) and on the installed images, and covers every
weight of that family shipped beside it.

- **Inter** (Regular, SemiBold) — `Copyright (c) 2016 The Inter Project Authors
  (https://github.com/rsms/inter)` — SIL OFL 1.1, full text in
  [`assets/fonts/Inter-OFL.txt`](assets/fonts/Inter-OFL.txt)
- **JetBrains Mono** (Regular, Bold) — `Copyright 2020 The JetBrains Mono
  Project Authors (https://github.com/JetBrains/JetBrainsMono)` — SIL OFL 1.1,
  full text in
  [`assets/fonts/JetBrainsMono-OFL.txt`](assets/fonts/JetBrainsMono-OFL.txt)

## Host tooling — not distributed with SlopOS

QEMU, OVMF / EDK II, Verus and Z3, Miri, rr, `just`, and the Go toolchain
(including `golang.org/x/term` and `golang.org/x/sys`) are used to build, run,
verify and test SlopOS. None of their code is incorporated into SlopOS or
distributed with it, and their licenses impose no terms on SlopOS output.

## Trademarks

Linux® is a registered trademark of Linus Torvalds. seL4® is a trademark of
LF Projects, LLC. FreeBSD is a registered trademark of The FreeBSD Foundation.
Fuchsia is a trademark of Google LLC. All other names are the property of their
respective owners.

Use of these names is nominative — they identify the projects they belong to and
nothing else. SlopOS is an independent, from-scratch operating system. It is not
affiliated with, endorsed by, sponsored by, or derived from any project named
here or elsewhere in this repository.
