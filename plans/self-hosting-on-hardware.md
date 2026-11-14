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
| Kernel, one per slot, with its stamped words | `bootctl install` into the spare slot | Yes: `oneshot`, then `bootctl commit` | Any reset |
| Base, shared by both slots | `bootctl base try`, live | Yes: `bootctl base commit` | Any reset, `bootctl base rollback` |
| Both, across an interface change | `bootctl base try --kernel` | Yes: the base rides the kernel's trial | Any reset |

`BASE_INTERFACE` ties a kernel to a base, and `libc.so`'s exports tie a
base to what runs outside it.

Rules every change keeps:

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

## What is left

- A Linux machine is still needed to test: the suite boots under QEMU, and
  the tests kernel starts the userland tests. `tests-without-a-host.md` is
  the spike for that.
- Three things in userland still wait for a boot, each an entry in
  `KNOWN_ISSUES.md`: an activation refused for room names no holder, an
  init that cannot read its predecessor's state loses consent until
  reboot, and only `cpufreq.epp=` and `sched.hybrid=` change while the
  kernel runs.

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
  tree, a private call changes shape freely, and no change adds a shim, a
  frozen format or a migration to keep an older install updatable. A change
  the installed system cannot take costs a reinstall from a medium.
