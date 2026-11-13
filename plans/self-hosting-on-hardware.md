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

Phases 1 and 2 are done. Phase 3 changes what `just install` means.

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
