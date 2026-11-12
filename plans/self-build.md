# Building everything on SlopOS (spike)

This file is a spike. Its work is research, and its result replaces it: when
the questions below are answered, rewrite this file as the plan they lead
to, or delete it and say in the commit why no plan follows.

## The goal

A SlopOS machine builds what a Linux machine builds for it today: its own
compilers and toolchain programs, and the medium that installs or repairs a
machine. With that, recovery and a moved toolchain pin need no second
computer, and nothing a release is made of has to come from CI.

## What is known

| Piece | Built today by | Needs on the building machine |
|-------|----------------|-------------------------------|
| rustc, cargo, std | `scripts/bootstrap_slopos_toolchain.sh`, Rust's `x.py` | `python3`, `go`, `git`, `curl`, `tar`, `patch`, a host Rust to start from |
| LLVM, clang, the C++ runtime | the same bootstrap, `scripts/make_slopos_cxx.sh` | `cmake`, `ninja`, `make`, `python3` |
| The recipes (git, OpenSSH, curl, bash, CMake, Ninja, e2fsprogs and their libraries) | `scripts/build_recipes.sh` | `cmake`, `ninja`, `make`, `meson`, `perl` |
| The install medium and its payload | `scripts/build_install_medium.sh` | `python3` (`fs_tree.py`), `debugfs`, `mke2fs`, `cpio` |
| The ISO | `scripts/build_iso.sh` | `xorriso`, `cpio` |
| A boot disk | `scripts/build_bootdisk.sh`, `tools/bootdisk` | `sfdisk`, `mkfs.fat`, `mtools` |
| Limine | `scripts/ensure_limine.sh` | a download of prebuilt binaries |
| The log gates' runner | `scripts/ensure_go.sh`, `tools/run_tests` | `go` |

A SlopOS machine with the toolchain has `rustc`, `cargo`, `clang`, `ld.lld`,
`cmake`, `ninja`, `git`, `curl`, `bash` and e2fsprogs. It has no `python3`,
`go`, `perl`, `make`, `meson`, `xorriso` or `mtools`.

The docs put a compiler build at tens of gigabytes of disk and hours of CPU.
One process may pin an eighth of memory for the files it maps, and a link
past that dies (`KNOWN_ISSUES.md`); the debug tests kernel's link already
needs a 6 GiB machine.

## Questions

Each answer is a measurement or a reading of the source, with where it came
from.

### The compilers

1. Does `x.py` need Python, or can the bootstrap binary it wraps be built
   and driven by the installed `cargo` alone?
2. What does LLVM's CMake build run besides `cmake`, `ninja` and a C++
   compiler? Name each Python or Perl step and whether a build option turns
   it off.
3. Stage 0: the bootstrap starts from a prebuilt host Rust. Is the installed
   toolchain usable as that compiler, so that SlopOS builds the next
   toolchain from the one it has?
4. What does one full build cost on the development machine: wall time,
   peak disk, peak memory, and the largest link against the pin share?
5. Which system calls and libc functions does the build reach that SlopOS
   answers `ENOSYS` or lacks? Run it until it stops, fix nothing, list them.

### The recipes

6. Which recipes build with what SlopOS has, and which need `make`,
   `meson`, `perl` or an autoconf `configure`? For each tool missing: port
   it as a recipe, or change the recipe's build to what is there.

### The medium

7. Can `tools/bootdisk`, `fat-core` and `boot-core`, which already write
   every structure of a SlopOS disk, replace `sfdisk`, `mkfs.fat` and
   `mtools` in the builders?
8. Can the base's `mke2fs` and `debugfs` and a Rust writer of trees replace
   `fs_tree.py` in `build_install_medium.sh`?
9. What does a bootable medium need that an ISO gives: is a raw disk image
   written to a stick enough, so that `xorriso` is needed for a CD alone?
10. Limine arrives as downloaded binaries. Is building it from source on
    SlopOS in scope, or is a pinned download held to a digest enough?

### What it replaces

11. With the answers above, which of Python, Go, Perl, make and meson has
    to run on SlopOS at all, and which uses go away by moving a script to
    Rust, as `AGENTS.md` already asks of a new check?
12. Which checks does a self-built toolchain have to pass to be trusted as
    much as the cross-built one: the ladder of `toolchain_test`, a second
    build of itself that comes out the same?

## How

- Work on the development machine (`just boot`) or the self-hosting test
  root, never on a person's installed disk.
- Change nothing in the tree to make a step pass. A step that fails is a
  finding: write down the error and the cause.
- Keep the measurements beside the claims, with the commit they were taken
  at.

## Done when

Every question has an answer or a stated reason it could not be answered,
and this file has become the plan: phases in an order, each with what it
builds, what it needs first and how it is checked, as
`self-hosting-on-hardware.md` is written.
