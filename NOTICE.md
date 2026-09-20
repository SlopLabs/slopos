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

## Pinned Rust standard library and `libc` forks

SlopOS's userland target `x86_64-unknown-slopos` is built against two pinned
forks, both of upstream Rust projects licensed `MIT OR Apache-2.0`:

- a fork of [`rust-lang/rust`](https://github.com/rust-lang/rust)'s
  `library/` tree, at the commit `rust-toolchain.toml` pins, adding the
  `slopos` target to `library/std`: its platform allowlist, the per-target
  `os/slopos/` and `sys/random/slopos.rs` modules, and the `cfg` sites a
  unix-family target has to appear in;
- a fork of [`rust-lang/libc`](https://github.com/rust-lang/libc), adding
  `src/unix/slopos/` — the `libc` bindings for SlopOS's C library.

Neither fork is vendored into this repository. What is tracked is the diff:
`toolchain/PIN` records the channel, the pinned `libc` crate version and its
checksum, and a checksum per patch, and `toolchain/rust/` and
`toolchain/libc/` carry the patches themselves.
`scripts/make_slopos_sysroot.sh` materialises both into an owned sysroot at
`third_party/rust-slopos/` at build time, and
`scripts/check_toolchain_pin.sh` fails the build if what is materialised has
drifted from what is pinned. Upstream's code reaches a SlopOS build only
through that step. Both patches are licensed `MIT OR Apache-2.0` rather than
GPL-3.0-or-later, because they are written to be contributed upstream under the
tier-3 target policy.

Authorship within the patches is split. New work, © 2025–2026 The SlopOS
Authors, is the `libc` fork's `src/unix/slopos/` module, the `target_os =
"slopos"` arms the std patch adds to lists that were already there, and
`std/src/sys/random/slopos.rs`. The three files the std patch creates under
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
| [`bitflags`](https://github.com/bitflags/bitflags) | 2.11.0 | Copyright (c) 2014 The Rust Project Developers |
| [`libc`](https://github.com/rust-lang/libc) | 0.2.189, forked | Copyright (c) 2014 The Rust Project Developers |
| [`libm`](https://github.com/rust-lang/libm) | 0.2.16 | **MIT only** — see the note below |
| [`limine`](https://github.com/limine-bootloader/limine-rs) | 0.6.3 | Copyright © 2026 Julian Scheffers |
| [`paste`](https://github.com/dtolnay/paste) | 1.0.15 | David Tolnay (upstream ships no copyright line) |
| [`gimli`](https://github.com/gimli-rs/gimli) | 0.33.0 | Copyright (c) 2015 The Rust Project Developers |
| [`unwinding`](https://github.com/nbdd0121/unwinding/) | 0.2.9 | Gary Guo (upstream ships no copyright line) |
| Rust `core`, `alloc`, `std` | pinned nightly, `std` forked | Copyright © The Rust Project Contributors |

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

Nothing from llvm-project is vendored into this repository. `toolchain/cxx/PIN`
records the release, its download URL and its checksum;
`scripts/make_slopos_cxx.sh` fetches and builds it into
`third_party/slopos-cxx/`, and `scripts/check_cxx_pin.sh` fails the build if
what is on disk has drifted from what is pinned. The runtime reaches the
**tests** image only: `libc++.so` as a file, and `libc++.a` as the C++ half of
the statically linked `cxx_static_probe`. Both projects' license texts ship
beside it, in `/usr/share/licenses/libc++/`, the way the OFL fonts carry
theirs.

The runtime is built with localization, wide characters, the filesystem
library, the random device and the time-zone database turned off, so the parts
of libc++ that need a locale layer SlopOS has not got are not compiled at
all.

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
