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

A Linux machine is needed today for three things this plan takes on:

| Step | What needs Linux |
|------|------------------|
| Running a recipe | No `just` runs on SlopOS, and the justfile names rustup toolchains (`cargo +slopos`, `cargo +<channel>`). The guest builds through `scripts/selfhost.sh`, a second orchestrator over the same build scripts |
| Formatting, linting, gating | The cross-built toolchain carries `cargo` and `src` alone. `xtask elf` and `xtask programs` run from justfile recipes the guest skips. The base-interface and libc-exports gates need `python3`, and the second `llvm-readobj` |
| Testing | The suite boots under QEMU, and the tests kernel starts the userland tests too, so a test of a program costs a kernel boot. A slot's kernel takes extra boot options only when it was compiled with `SLOPOS_BUILTIN_CMDLINE`; the rest are in `limine.conf` on the ESP, which nothing writes after install |

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

## Phase 1: one build interface

**For:** the commands in `AGENTS.md` and the docs to mean the same thing on
both systems, and one place that says how a variant is built.

`scripts/selfhost.sh` restates what the justfile holds: the variant's
features (`kernel_features_tests`), the base's packing environment
(`base_recipe_env`, `COREUTILS_LINKS`), and the order of the builds. It runs
none of the gates the recipes run after a build. The tests grade the script
(`slopos_userland::selfhost`), so the justfile's path into a slot is never
the one tested.

Build:

1. `just` as a program of the toolchain, built by cargo for
   `x86_64-unknown-slopos` during `just toolchain` and installed with it.
   The recipes under `toolchain/recipes/` build C projects; this needs a
   way to build a pinned crate. It is a new third-party input:
   `NOTICE.md`, a vendored lock, a rung in `toolchain_test`.
2. One `cargo` and one `xtask` variable in the justfile that resolve by
   `os()`. On Linux they keep `+slopos`, `+<channel>` and
   `scripts/ensure_toolchain.sh`. On SlopOS they are the installed `cargo`,
   and the toolchain check is the stamp `build_userland.sh` reads.
3. Three kinds of recipe, each saying where it runs:
   - build and gate (`build`, `fmt`, `clippy`, the gates): both systems;
   - install (`install [variant]`, `install-kernel [variant]`): SlopOS
     only. On Linux they refuse in one line and name `just remote-install`;
   - emulate (`boot`, `test`, `iso`, the image builders): Linux and macOS
     only, refusing on SlopOS the same way.
4. `scripts/selfhost.sh` deleted. `slopos_userland::selfhost` runs `just`,
   and `install_test`, `installer_test` and `selfhost_test` follow, with
   the line `just boot` prints and the installer's words. What the script
   does for the apps moves into the recipe as it is;
   `installed-software.md` replaces it with an image.

The port is a recipe and, where a crate does not know the target, a line
in `toolchain/crates/`, which already patches `nix` and `rustix`. What
`just` calls that slibc or the kernel does not answer yet is a fix there,
found by running it, and never a patch to `just`.

Done when `just test-install-guest` and `just test-installer` pass with the
script gone, and `just boot` on a SlopOS host refuses and says why.

The rule in skill `slopos-dev-machine` that host-only steps stay in the
justfile becomes "host-only steps carry the guard"; the change appends that
to the skill's `pending.md`.

## Phase 2: the gates where the build runs

**For:** a build made on SlopOS to be held to what CI holds a host build to,
and the pre-commit steps to be possible there.

Build:

1. `rustfmt` and `clippy` in the cross-built toolchain: the `tools` list in
   `scripts/bootstrap_slopos_toolchain.sh`, and a rung each in
   `toolchain_test`. `just fmt` and `just clippy` then run on SlopOS.
2. `xtask` built natively. Its dependencies are `syn`, `object`,
   `iced-x86`, `rustc-demangle` and `serde_json`, none of which reaches the
   system beyond std. `_kernel` and `_build-userland` then run
   `xtask elf` and `xtask programs` on both systems, and the split between
   the build scripts and "what only the host does" goes.
3. The two contract gates as `xtask` modules, with their fixtures as tests:
   `scripts/check_base_interface.sh` (a Python parser over rustdoc's JSON)
   and `scripts/check_libc_exports.sh` (`llvm-readobj` and a Python
   canonicaliser). They guard the two contracts an install depends on, so
   they run before `just install` writes anything.
4. `check-framekernel-gates` split by what each line reads: the gates over
   the tree and the built binaries run on both systems; the self-tests of
   the host's image tools (`fs_tree.py`, `gen_verity.py`,
   `check_fs_image.sh`) stay with the emulate recipes.
5. A lane a SlopOS machine can run before a commit, named in `AGENTS.md`'s
   pre-commit steps beside `just ci`.
6. The Linux-numbered calls the kernel answers, in the surface the
   base-interface gate digests. The gate reads `slopos-abi` alone, and
   which calls are answered is the dispatch table in
   `core/src/syscall/handlers.rs`. So a base that starts using a call the
   kernel has just gained installs live, under the same number, on a
   kernel without it. In the digest, a newly answered call takes the next
   number and the pair path of phase 3.

The same holds for `xtask` and for `cargo test` (which needs std's `test`
crate for the target, and decides whether `just test-host` joins the
lane): what they call and SlopOS lacks is a slibc or kernel fix. Still to
settle: how the layout half of the libc-exports gate gets its input
without Python.

Until step 3 lands, a change to a private call cannot be recorded on
SlopOS: `--record` needs `python3`.

Done when `just test-selfhost` has the guest grade its own build with that
lane, and the host's second grading of the exported kernels only confirms
it.

## Phase 3: only a kernel change needs a reboot

**For:** a base to be tried and dropped without a boot, boot options to
follow the kernel that reads them, and the userland tests to run on the
kernel that is already up.

### A base is tried before it is kept

`bootctl base install` writes the boot partition and then activates the
image from memory (`base_activate`). The two steps do not depend on each
other, so the trial is the second without the first.

Build:

1. `bootctl base try <image> [<sha256>]`: the checks `base install` makes,
   then the activation, and no write. It needs `BaseUpdate` and not
   `RawBlock`. Any reset boots the base on the disk.
2. `bootctl base commit`: writes the pair as `base install` does, only for
   the image whose manifest the running generation has.
3. `bootctl base rollback` with a trial running activates the disk's
   current base again and writes nothing.
4. The rule for `previous.img` gains a case. Today current moves there only
   when it is the running base; at a commit the running base is the tried
   one, so `bootctl` decides from the base that ran before the trial.
5. `org.slopos.base.try` and `org.slopos.base.commit` in
   `assets/privd/actions`; `just install` tries and prints the commit
   command, as `install-kernel` does for a slot; `just remote-install` and
   `scripts/remote.py install` follow.

A base the running kernel does not serve cannot run live. It goes to the
disk as it does today and rides the kernel's trial: the old default kernel
does not serve it either and boots previous.

This closes *A base install takes effect for every slot with no trial boot*
in `KNOWN_ISSUES.md`, and removes the dependency of a rollback on the
compositor and privd the new base replaced.

### A change to both goes in as a pair

Before v1 a kernel serves its own interface alone, so every change to the
private call surface needs a new kernel and a new base together.
`bootctl base install` handles that today by staging without saying so: it
writes the base, does not run it, and the last line it prints is the only
sign.

`bootctl` decides and the recipe sequences:

1. One `bootctl` command takes the base a build made and, beside it, the
   kernel. When the running kernel serves the base, this is the live trial
   above and the kernel is left alone. When it does not, `bootctl` writes
   the base beside the running one, puts the kernel into the spare slot,
   arms one boot and says a reboot is needed. Given no kernel that serves
   the base, it refuses and writes nothing.
2. `just install` builds both and hands both over. It compares nothing
   itself. `just remote-install` and `scripts/remote.py install` call the
   same command, and `test-install`'s case of a base no kernel serves
   follows.
3. Every slot's kernel is held to a base, and a base write that would
   strand the spare slot's copies the default kernel into it first (*A
   base write can leave the spare slot's kernel serving neither base* in
   `KNOWN_ISSUES.md`). The pair makes that case routine.
4. `bootctl status` prints the interfaces each slot's kernel serves.

`just install-kernel` stays for a kernel change that moves no interface.

### Boot options travel with the kernel that reads them

Where the options live elsewhere:

| System | Where | Changed by |
|--------|-------|------------|
| Unified kernel image (UAPI Boot Loader Specification, Type #2) | A `.cmdline` section of the kernel image file | `kernel-install` and `ukify` assemble the image at install from `/etc/kernel/cmdline` |
| ChromeOS | Inside each slot's signed kernel blob | Repacking the blob (`vbutil_kernel --repack --config`) |
| Boot Loader Specification, Type #1 | An `options` line in a file per entry on the boot partition | Editing the file; the loader reads it |
| systemd's `SystemdOptions` firmware variable | A UEFI variable | Removed in systemd 258 |

The current form is the first: the words are part of the kernel image, and
the machine's own file is their source at install time. Limine reads no
per-entry files, only one `limine.conf`, the one beside itself first.

SlopOS has half of that. `limine.conf` holds `root=PARTUUID=…` and the
words the installer was given (`panic=reboot` unless told otherwise),
fixed at install. A kernel compiled with `SLOPOS_BUILTIN_CMDLINE` carries
words the kernel appends after the loader's
(`boot/src/limine_protocol.rs`), where the last occurrence of an option
wins. The words are an `option_env!` constant, so changing one means
compiling a kernel, and a machine that needs `tp.poll` or `xe.pipe=B`
loses it at the next `install-kernel` unless every build sets the
variable.

Build:

1. The built-in words in a reserved, fixed-size section of the ELF,
   asserted by `link.ld` as the base-interface note is, in place of the
   constant. A build with `SLOPOS_BUILTIN_CMDLINE` fills the same section,
   so there is one mechanism.
2. The machine's words in one file on the root, as `/etc/kernel/cmdline`
   is. `bootctl install` and `bootctl clone` stamp them into the section
   as they write a slot, through `boot-core::kernel`, which already finds
   the note by section name. `--cmdline <words>` adds words for that
   install alone.
3. The installer writes the words it is given to that file and stamps
   both slots; `limine.conf` keeps `root=`.
4. `bootctl status` prints each slot's words.

The words then follow the A/B rule the kernel does: tried once with the
slot, kept at its commit, never left behind for the default.

The file is as privileged as a kernel install, since its words can turn
off `quota=` and `lockdep=`: only uid 0 writes it, and a change goes
through privd under the consent `bootctl install` takes.

Decide: where the stamp sits if a kernel is ever signed, since `ukify`
signs after it assembles.

Not chosen: `limine.conf` on the boot partition, where `bootctl` could
rewrite it. One file serves both slots, so a bad one has no slot to fall
back to.

### The suite on hardware

The kernel's tests (`stest!`) are a kernel change: `just test` on SlopOS
puts the tests kernel into the spare slot with the tests' words and boots
it once. Two things are not known:

- **Which tests write a disk.** The suite boots `root=auto`, and its
  filesystem tests mutate the root (the justfile's note on
  `PRESERVE_FS_IMAGE=0`). On a person's machine it runs on a RAM root and
  holds every test to disks it made.
- **Where the result goes.** The log gates read a serial capture. A
  machine with no host needs the capture kept across the reset, as the
  crash partition keeps a panic's record.

The userland tests (`utest!`) test programs, and by the rule above run on
the kernel that is up. Today the tests kernel starts them
(`core/src/utests.rs`, the `run_userland_tests` call) and their programs
are in a tests base. Build: the tests base tried live with
`bootctl base try`, a runner in userland that starts each test and
reports as the kernel's harness does, and the base put back afterwards.
Find out first which tests need what only the tests kernel gives:
`test_key`, and whatever else answers to `TestHarness`.

### What in userland still waits for a boot

Each of these breaks the rule today:

- **An activation refused for room.** `base_activate` answers `EBUSY` at
  16 generations or a quarter of memory held, and the base waits for the
  next boot. `bootctl` names the programs holding the old generations, so
  restarting them is the answer.
- **An init that cannot read its predecessor's state** starts the
  sessions again without consent dialogs until the next boot.
- **Boot options that are settings.** An option the kernel can change
  while it runs is a setting with a boot-time default. Of
  `docs/abi/boot-options`: `boot.debug=`, `cpufreq.epp=`, `sched.hybrid=`,
  `watchdog=`, `lockdep=`, `quota=`, `mem.commit=`, `kconsole.*` and
  `roulette=skip` are candidates. `root=`, `mount=`, `verity=`, `usb=`,
  `xe.*`, `tp.*`, `tests=` and the `panic.*` smokes have to be known
  before userland runs and stay boot options. Find out which candidates
  have a runtime switch already.

Restarting the desktop after a base changed the compositor is a session
restart and stays.

## Order

Phase 1 comes first: every later phase adds recipes, and adding them to
`selfhost.sh` would grow what phase 1 deletes. Phase 2 needs phase 1's
`just`, and its port of the base-interface gate decides when kernel work
that touches a private call can be committed from SlopOS. Phase 3 changes
what `just install` means. Phases 1 and 2 each change the cross-built
toolchain, so one rebuild carries both.

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
