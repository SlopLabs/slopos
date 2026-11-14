# Developing SlopOS on an installed machine

## The goal

On a computer SlopOS was installed on, a person pulls, builds, checks,
installs a kernel and a base, tries them, keeps them and pushes the change
with the commands the Linux host uses. The QEMU development machine is a
convenience for the same loop, and stops being where the loop is defined.

How the toolchain and the apps reach a machine, building the compilers
there and what the private calls become are other plans, named at the end.

## The model

The system is a kernel and a base, each replaced its own way:

| Layer | Replaced by | Tried before kept | Way back |
|-------|-------------|-------------------|----------|
| Kernel, one per slot | `bootctl install` into the spare slot | Yes: `oneshot`, then `commit` | Any reset |
| Base, shared by both slots | `bootctl base install`, live | No | `bootctl base rollback` |

`BASE_INTERFACE` ties a kernel to a base, and `libc.so`'s exports tie a
base to what runs outside it.

A Linux machine is needed today for one thing this plan takes on: testing.
The suite boots under QEMU, and the tests kernel starts the userland tests
too, so a test of a program costs a kernel boot. A slot's kernel takes extra
boot options only when it was compiled with `SLOPOS_BUILTIN_CMDLINE`; the
rest are in `limine.conf` on the ESP, which nothing writes after install.

Rules every phase keeps:

- Only a kernel change needs a reboot. A base, an app, an extension, the
  toolchain or a setting takes effect under the running kernel. A reset
  may be the way back from a trial, never the way forward.
- Nothing writes the ESP after install, and `bootctl` never writes the
  default slot's kernel.
- A base is replaced whole. `libc.so` only grows.
- A new check is a module of `tools/xtask`.
- The system's tools decide and the recipes sequence. What may become the
  kernel or the base is checked by `bootctl` or the kernel, never by a
  comparison in the justfile. A tree is nobody's to refuse.

## Phase 1: one build interface (done)

The justfile is the one build interface; `scripts/selfhost.sh` is gone.
`just` 1.59.0 is a toolchain program (`toolchain/recipes/just/`, built by
the `cargo` recipe template against a lock that resolves `libc`, `nix`,
`rustix`, `getrandom` and `errno` to the fork and the `toolchain/crates/`
ports), held by the `just_runs_a_justfile` rung. `cargo`, `host_cargo` and
`ensure_toolchain` resolve by `os()`. A recipe that
emulates or needs the host's tools depends on `_host`; `install [variant]`
and `install-kernel [variant]` depend on `_slopos`; each refuses elsewhere in
one line. `slopos_userland::selfhost` runs `just build`, `just install` and
`just install-kernel`. Porting just grew slibc by `mkfifoat` and the libc
fork by Linux's `sysconf`/`pathconf` names and five group and password
declarations.

## Phase 2: the gates where the build runs (done)

The cross-built toolchain carries rustdoc, rustfmt and clippy (compiler
patch 0004 links a cross-built `rustc_private` tool against SlopOS's
libLLVM; std's `current_exe` reads `AT_EXECFN`), held by the
`rustfmt_formats_a_crate` and `clippy_lints_a_crate` rungs. `xtask` builds
natively, taking `cargo +<toolchain>` only where
rustup is, so the justfile's `gate` is gone. The two contract gates are
`xtask base-interface` and `xtask libc-exports`, their scripts and Python
helpers gone: the first also digests the Linux calls `SYSCALL_TABLE`
answers (interface 13), the second reads `.dynsym` with `object` and dumps
layouts with the clang `cxx_host_tools.sh` finds on either system.
`install` runs them before it writes. `check-gates` (both systems) holds
the tree and the dev kernel; `check-framekernel-gates` adds the host's
toolchain and image-tool gates. `just precommit` (`fmt`, `test-host`,
`clippy`, `build dev`, `check-gates`, `libc-exports`) is the lane
`AGENTS.md` names for SlopOS, and `test-selfhost`'s guest runs it over its
own build before the host confirms the kernels.

## Phase 3: a base tried, boot options with the kernel (done)

`bootctl base try <img> [<sha256>]` runs a base from memory under
`BaseUpdate` alone and writes nothing, so any reset ends the trial. The first
try notes the base it replaced in `/run/bootctl/base-trial`, which `base
commit` keeps as the one that last ran and `base rollback` runs again.
Given `--kernel`, a base the running kernel does not serve goes to the disk
beside the running one and the kernel into the spare slot, armed once; with
no kernel serving it nothing is written. `just install`, `remote-install`
and `remote.py install` hand both over; `install-kernel` stays for a kernel
alone. A base write copies the default kernel over a slot it would strand.
A kernel carries its words in a 512-byte `.slopos.cmdline` section, filled
by `SLOPOS_BUILTIN_CMDLINE` and stamped by `bootctl install` and `clone` from
uid 0's `/etc/kernel/cmdline` (`bootctl cmdline set`) and `--cmdline`; the
installer writes that file, stamps both slots and leaves `limine.conf`
`root=` alone. `bootctl status` prints each slot's interfaces and words, and
`test-install` holds the trial, its end, the commit and the pair.

## Order

Phases 1, 2 and 3 are done.

## What turns on at v1

Before v1 none of this is promised. At v1, in one change:

- `BASE_INTERFACE_MIN` gets a value of its own. A private call keeps its
  shape, a new shape is a new call, and a retired call answers `ENOSYS` in
  its slot.
- The kernel goes first and the base follows live. The pair path stays for
  a machine that skipped versions.
- `slibc` states the oldest kernel it runs on, and for every call the
  kernel gained after that it falls back when the kernel answers `ENOSYS`,
  as glibc does. An older kernel under a newer base then loses a feature,
  where before v1 it is refused.
- Each interface `docs/abi/interfaces` lists as *Unversioned* gets a
  version or a rule: tree manifests, the accounts record and the extension
  store, the base manifest, `limine.conf`, the disk layout, the kernel
  command line. A base rolled back has to read what the newer one wrote on
  the root, and nothing checks that today.
- A kernel and a base are signed, a slot's stamped words inside what is
  signed.

## Not in this plan

- Building the compilers and the install medium on SlopOS. `self-build.md`
  holds the research for that plan; until it lands, a new toolchain and a
  recovery medium come from a Linux machine.
- Moving private calls to the interfaces Linux defines.
  `linux-interfaces.md` holds the research for that plan.
- How the toolchain and the apps reach a machine and are replaced there.
  `installed-software.md` is that plan: every installed program an image,
  with `/usr/local`, `tree-install` and the hand-over of `/usr/local` to
  uid 0 gone. Until it lands the toolchain arrives from the install medium
  or the host's image builder, and a commit that moves a toolchain pin
  needs it again.
- Signing Limine or a kernel for Secure Boot.
- Drivers, and the kernel defects `KNOWN_ISSUES.md` lists.
- QEMU on SlopOS.
- Compatibility across versions before v1. A kernel and a base come from one
  tree, a private call changes shape freely, and no phase adds a shim, a
  frozen format or a migration to keep an older install updatable. A change
  the installed system cannot take costs a reinstall from a medium.
