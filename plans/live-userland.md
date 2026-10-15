# Live Userland: One Sealed Base, Replaced Without a Reboot

## Goal

Only a kernel change needs a reboot. When this plan is done:

- **The base is the core package.** One sealed archive carries what the
  security model rests on and what a repair needs. It lives on the boot
  partition as `/boot/base/current.img`, with the base that ran before it kept
  as `previous.img`. Every kernel slot boots it; the slots hold kernels only.
- **A base is replaced live.** `bootctl base install <image>` writes the new
  base and switches the running system to it without a reboot. New programs
  start from the new base at once; a service whose program changed restarts.
  `bootctl base rollback` goes back to the previous base the same way.
- **Everything else is plain Unix.** The editor, the file manager and the image
  viewer leave the base for `/usr/local`, and anything a person builds goes
  into their home or `/usr/local`. None of it needs a reboot or a permission.
- **`libc.so` is the stable interface.** Calls with Linux numbers stay stable
  by Linux's rule. A program outside the base reaches SlopOS-only calls only
  through `libc.so`, whose exports only ever grow. The compositor protocol is
  versioned, and the compositor keeps serving older apps.
- **init is a service manager.** It restarts a crashed service, restarts a
  service whose program a base switch changed, re-executes itself, and can end
  and restart the desktop session. A changed compositor or terminal asks the
  person to restart the session.
- **The live medium repairs an installed system.** Booted from the install
  medium, the person can ask privd for a rescue shell. After one consent, it
  holds every capability privd can give, so it can mount the installed root,
  check it, and install or roll back its base.

## Depends on

The access model, which has landed (`AGENTS.md`, *Authority discipline*). This
plan keeps every rule it states and changes one premise: a `Base` program is no
longer "a file of the image that booted" but "a file of a base generation the
running kernel installed".

`plans/installer-gui.md` changes the installer too. Its step "writing both
slots" becomes "writing both kernel slots and the base". Whichever of the two
plans lands second re-baselines that step.

## Where it stands

**The base is the boot partition's and fixed for the boot (Phase 3).**
- Slots hold kernels only; every entry loads `/boot/base/current.img` and
  `previous.img` as `initramfs` and `initramfs-previous`
  (`boot-core/src/layout.rs`, `limine.rs`).
- In the loader handoff the kernel boots the first base whose interface it
  serves and hands the other's pages to memory init as usable
  (`boot/src/base.rs`, `mm/src/memory_init.rs`). Nothing can reach the
  module after; there is no API to give the allocator memory after boot.
- `BASE_FS` is a static whose index is a `OnceLock`, installed once at boot
  (`fs/src/basefs.rs`). File data are slices of the chosen module.
  `MEDIUM_FS` is a second static instance for the install medium.
- `BASE_DIRS` are mounted read-only and pinned (`fs/src/vfs/init.rs:536-555`).
  The syscall layer refuses to unmount them or mount over them
  (`core/src/syscall/fs/mount_handlers.rs`).
- Sizes, measured: the shipped base is 21 MB and the release kernel 12 MB. The
  boot partition is 1 GiB (`layout::BOOT_BYTES`).

**Program identity assumes one base per boot.**
- Exec records `Program::Base(inode)` when the image and its interpreter come
  from `BASE_FS`. The check is pointer identity against the static
  (`core/src/exec/mod.rs:761-766`, `fs/src/basefs.rs:287-289`).
- The identity is an inode number in an `AtomicU64`
  (`slopos-ostd/src/process/identity.rs:8-27`). `SO_PEERPROGRAM` turns it into
  a path through whatever `BASE_FS` is now
  (`core/src/syscall/net_handlers.rs:711-730`).
- privd reads its policy and whether the base is paired once, at start
  (`userland/src/apps/privd/mod.rs:87-93`).

**init is a service manager (Phase 4).**
- A single-threaded poll loop (`userland/src/apps/init/`) starts privd,
  remoted and the desktop session again when they exit, at most five times in
  any minute each. A test boot runs the tests in a copy of init and no session
  until one is asked for or the tests fail.
- The desktop's processes carry the login tag `DESKTOP`, remoted's `REMOTE`
  and privd's `PRIVD` (`userland/src/init_control.rs`); a session ends by its
  tag, and a privd started again takes no exclusive group while its
  predecessor's mechanisms run.
- `/run/init` takes `restart-session` (session, uid 0), `end-session` and
  `reexec` (uid 0). Init re-executes keeping its pid, children, capabilities,
  service table and descriptors (`init/reexec.rs`); nothing triggers it yet but
  a request.
- init keeps both ends of the consent pair; each new privd or compositor
  resynchronises with `RESET`/`RESET_SEEN` (`priv-core/src/consent.rs`).

**Programs outside the base (Phase 2).**
- The editor, the file manager and the image viewer are `slopos-apps`
  (`apps/`): dynamic against `libc.so`, with no `syscall` instruction of their
  own, installed as the tree `apps` at `/usr/local`.
- The dock pins the app entries under `/usr/share`, `/usr/local/share` and
  `~/.local/share` (`userland/src/app_entry.rs`). The shell's registry names
  no app.
- The loader searches `/usr/local/lib` last, for a process neither
  `AT_SECURE` nor uid 0's (`slibc/src/ld_so/mod.rs`).
- The compositor serves protocol versions `PROTOCOL_VERSION_MIN` through
  `PROTOCOL_VERSION`, both 3 (Phase 1).
- The base programs are static: slibc is linked as an rlib, and raw
  syscalls are made through `slopos_userland::syscall`.
- `libc.so` reaches four SlopOS-only calls itself: `spawn_path`, `resolve`,
  `net_query` and `klog_write` (`slibc/src/pal/slopos.rs`). It exports one
  SlopOS-named entry to them, `slopos_spawn_path`
  (`slibc/src/ffi/syscalls.rs`).
- These base programs make SlopOS-only calls directly:
  - sysmon: `sys_info`, `process_list`, `percpu_stats`, `cpu_perf`, `cpu_info`
    and `net_query` (`userland/src/apps/sysmon/state.rs`);
  - ss and nmap: `net_query`;
  - ping and nc: `resolve` and the rings;
  - oops_smoke: `test_panic`.

**Kernel and base each carry their interface number.** The kernel states the
range it serves in `sys_info` and its ELF note, which `boot-core::kernel`
reads from the file; a base carries `base-interface` and `base-manifest`,
which `boot-core::base` reads (Phases 1 and 3). The boot chooses by them,
and `bootctl install` refuses a kernel that serves neither base.

**Trees.** `tree-core` installs a tree by manifest, replacing what the old
manifest names and keeping what the user added. A manifest is keyed by the
tree's name, `usr_local` for the toolchain and `apps` for the apps, both at
`/usr/local`, and names where its tree went. `scripts/fs_tree.py` applies it
on the host, and `slopos_userland::tree` in the guest, for the installer,
init and `/bin/tree-install`.

**Building in the guest.** `scripts/selfhost.sh install` builds the kernel,
the userland, the base and the apps, runs `bootctl base install` (offline:
the next boot takes it), installs the kernel into the spare slot and arms
one boot of it. It installs the apps through the `tree-install` it built
when the running `libc.so` is the one they link, and otherwise prints the
command for the new boot. `just remote-install` does the same base and
kernel install and carries no apps: a machine moved by it from a base that
held the apps has none until a tree install (Phase 5).

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| FreeBSD base and `/usr/local` ([handbook](https://docs.freebsd.org/en/books/handbook/cutting-edge/)) | The base is one unit maintained with the kernel; third-party software lives in `/usr/local`; a userland-only update restarts services, not the machine | The split, and the base keeping the tools that speak the kernel's own interfaces (ping, netstat, top) |
| macOS: sealed system volume and libSystem | System files are sealed; the only stable interface is the system library, not raw syscalls | `libc.so` as the contract for programs outside the base |
| Fuchsia base packages ([packages](https://fuchsia.dev/fuchsia-src/concepts/packages/package)) | The base is updated whole; other packages are updated independently | One sealed unit for the core |
| Android APEX ([format](https://source.android.com/docs/core/ota/apex)) and soft restart ([docs](https://source.android.com/docs/core/runtime/soft-restart)) | A module is swapped live and only its dependents restart | Restart only the services whose program changed |
| FreeBSD `installkernel` ([kern.post.mk](https://github.com/freebsd/freebsd-src/blob/main/sys/conf/kern.post.mk)) | `/boot/kernel` moves to `kernel.old` only when the running kernel is the one in `/boot/kernel`; otherwise it is replaced, keeping the `kernel.old` that booted | "previous always holds the base that last ran", decided by the running base's manifest |
| Fuchsia base packages and A/B/R slots ([assembly RFC](https://fuchsia.dev/docs/contribute/governance/rfcs/0072_standalone_image_assembly_tool)) | Each slot boots a kernel image; the base packages live in a shared store the slot's boot arguments name | Slots hold kernels; the base is the boot partition's, beside them |
| Linux initrd ([initramfs.c](https://github.com/torvalds/linux/blob/master/init/initramfs.c)) | The initrd's whole pages are freed once it is unpacked; Linux never chooses between archives | The unused base's pages go back; here before the allocator is seeded, so nothing reads them after |
| Limine protocol ([PROTOCOL.md](https://github.com/limine-bootloader/limine-protocol/blob/trunk/PROTOCOL.md)) | A module is 4 KiB aligned; module memmap entries are illustrative only; a `module_path` it cannot open fails the entry | The page range freed is the module's whole pages, computed from the module, not the memmap; every disk holds both bases |
| NixOS `switch-to-configuration` ([manual](https://nixos.org/manual/nixos/stable/)) | Activation restarts only the units that changed; old generations stay for rollback | Digest-driven restarts; `previous.img` |
| systemd `soft-reboot` ([man](https://www.freedesktop.org/software/systemd/man/latest/systemd-soft-reboot.service.html)) | Userspace switches root without reinitialising the kernel | Not taken: it ends every process, which a mount switch does not need |
| Haiku package activation ([infrastructure](https://www.haiku-os.org/docs/develop/packages/Infrastructure.html)) | Packages activate live; the last consistent state is recorded | "previous always holds the base that last ran" |
| Linux audit session id, systemd login scopes | A login session is a tag every child inherits, not the POSIX session | The login tag that bounds a desktop session |
| systemd `daemon-reexec` ([systemctl(1)](https://man7.org/linux/man-pages/man1/systemctl.1.html)) | PID 1 serializes its state into a memfd, clears close-on-exec on the descriptors the state names and executes itself again | init's re-exec: the state in a pipe, the descriptors it names kept |
| systemd start limits and kill ([systemd.unit(5)](https://man7.org/linux/man-pages/man5/systemd.unit.5.html), [systemd.kill(5)](https://man7.org/linux/man-pages/man5/systemd.kill.5.html)) | A unit started too often in an interval stays failed until reset; a stop sends `SIGTERM`, then `SIGCONT`, then `SIGKILL` after a timeout | Five starts a minute; the session's end |
| Redox init ([boot process](https://doc.redox-os.org/book/boot-process.html)) | Starts services in order and only reaps after | Not taken: a crashed service is started again |
| Linux live USB, macOS Recovery | Booting recovery media is full power for whoever holds the machine | The rescue action, offered only on the medium |
| Linux `check-uapi.sh` ([docs](https://docs.kernel.org/dev-tools/checkuapi.html)) | A gate compares the UAPI headers' ABI across commits with libabigail and fails an incompatible change | A gate over the private call surface; the tree records the surface's digest itself, so a shallow CI checkout can grade it without a base commit |
| Linux vDSO `.note.Linux` ([glibc's use](https://www.spinics.net/lists/stable/msg443097.html)) | The kernel states its version in an ELF note that glibc reads without a syscall | The kernel states the base interfaces it serves in an ELF note, read from the file before booting it |
| Redox relibc and libredox ([libraries](https://doc.redox-os.org/book/libraries-apis.html), [FAQ](https://doc.redox-os.org/book/developer-faq.html)) | The stable ABI is in userspace, not at the syscall layer; relibc breaks when a function is removed or renamed, its arguments change, or a struct's size or alignment changes | `check_libc_exports.sh` holds removals and struct layouts; the syscall layer stays free behind the interface number |
| Asterinas ([book](https://asterinas.github.io/book/kernel/index.html)) | A kernel that offers only Linux's ABI has no private calls to number | Not taken: SlopOS keeps a private range, so it numbers it |
| Wayland versioning ([protocol](https://wayland.freedesktop.org/docs/html/ch04.html)) | A server keeps serving every version up to the one it advertises, and speaks the one a client binds | The compositor serves a version range and speaks the client's `Hello` |
| Redox launcher ([orbutils](https://gitlab.redox-os.org/redox-os/orbutils)) and libredox ([libraries](https://doc.redox-os.org/book/libraries-apis.html)) | Native app files are `key=value` lines (`name`, `binary`, `icon`) beside XDG `.desktop` entries, sorted by name; Rust programs reach relibc's C ABI rather than raw syscalls | `key=value` app entries in name order; the toolkit's `slopos-cabi` |
| XDG desktop entries ([file naming](https://specifications.freedesktop.org/desktop-entry/latest/file-naming.html)) | Of two entries with one id, the directory first in precedence wins, the user's data home ahead of `/usr/local/share` ahead of `/usr/share` | The dock's three shares and their precedence |
| glibc `ld.so` and `ld.so.conf` | `LD_LIBRARY_PATH` and `$ORIGIN` are ignored under `AT_SECURE`; `/usr/local/lib` is a search entry a distribution adds | `/usr/local/lib` searched last, and never for uid 0 or `AT_SECURE` |

## Design

### What the base carries

One rule decides membership. A program belongs in the base when:
- the security model rests on it: it holds or hands out authority, draws the
  consent dialog, or receives privd's connection; or
- a repair needs it; or
- it makes a SlopOS-only call directly.

Under that rule the base carries:
- `init`, `privd`, `compositor`, `shell`, `terminal`, `coreutils`, `remoted`,
  `roulette` and `curl`;
- the mechanisms: `halt`, `bootctl`, `installer`, `ip`, `keymap`, `cpufreq`,
  `kconsole`, `prof` and the six e2fsprogs programs;
- the tools on SlopOS-only calls: `sysmon`, `ss`, `nmap`, `ping`, `nc` and
  `oops_smoke`;
- `tree-install` and `session`, below;
- `libc.so` and the loader;
- the privd policy, the remote pairing, fonts, the CA bundle and licence texts,
  as today.

`editor`, `file_manager` and `image_viewer` leave it. `scripts/lib/base.sh`
states the list, with the apps in a list of their own.

The archive gains two files, which `tools/initramfs` writes:
- `/usr/share/slopos/base-interface`: the interface number it needs;
- `/usr/share/slopos/base-manifest`: one `<sha256> <mode> <path>` line per
  record but itself, in archive order.

### The boot partition

```
/boot/a/kernel.elf        kernel slots, A/B as today, kernels only
/boot/b/kernel.elf
/boot/base/current.img    the base every entry loads, module "initramfs"
/boot/base/previous.img   the base that last ran, module "initramfs-previous"
```

- `boot-core::layout` gains `BASE_DIR`, `BASE_CURRENT` and `BASE_PREVIOUS`
  and drops `BASE_FILE` from the slots. `boot-core::limine` renders both
  modules into every slot entry.
- An install writes both files with the same image, since Limine fails an
  entry whose module is missing.
- Every write is one `fat-core` copy-on-write file store, so an interruption
  leaves a pair of whole images, never a half one.
- **Previous always holds the base that last ran.**
  - `base install` copies `current` over `previous` only when `current` is the
    running base; otherwise it replaces `current` alone.
  - `base rollback` swaps the two, writing first over the image the default
    slot's kernel does not need.
- **No in-place migration.** An installed disk moves to this layout by a
  reinstall that keeps its root. `bootctl` never writes the ESP, and
  `limine.conf` lives there.

### Kernel and base: the interface number

- `slopos_abi::BASE_INTERFACE` is a number bumped by any change to a
  SlopOS-only call's number, arguments or structures.
- The kernel serves a range, from `BASE_INTERFACE_MIN` to `BASE_INTERFACE`.
  It carries that range in an ELF note, `.note.slopos.base-interface`, and
  reports it in `sys_info`.
- `scripts/check_base_interface.sh` holds a digest of the private call surface
  (the private part of `abi/src/syscall/numbers.rs` and the structures those
  calls take) to the number recorded in `scripts/gates/base-interface.txt`. A
  change without a bump fails. It carries a `--self-test`.

**At boot** the kernel reads `base-interface` from `initramfs` and from
`initramfs-previous`, and serves the first one in its range. It logs
`BOOT: base current|previous (<n> bytes), interface <i>` and returns the other
module's frames to the frame allocator. If neither is in range, the boot step
fails naming both numbers.

**At install:**
- `bootctl install <slot> <kernel>` refuses a kernel that serves neither the
  current nor the previous base.
- `bootctl base install` writes a base the running kernel cannot serve but
  does not activate it; the next boot of a kernel that serves it takes it.

### Base generations in the kernel

- `BaseFs` becomes a generation: an index over bytes the generation owns, and
  a generation number never repeated within a boot. The boot generation's
  bytes are the chosen module.
- A live generation's bytes are copied into memory it owns. The heap
  discipline applies, so no 21 MB contiguous allocation is assumed.
- `is_base` asks whether a filesystem is *a* base generation.
- `Program::Base` carries the generation beside the inode. `SO_PEERPROGRAM`
  resolves the path in the process's own generation.
- A generation stays resident while a mount, an open file, a mapping or a
  socket's captured identity refers to it. An identity whose generation has
  gone reads as no program, so privd refuses it.
- `base_activate(fd)` is a new private call gated on a new capability,
  `BaseUpdate` (bit 20):
  - it reads an archive from `fd`, checks its interface number against the
    kernel's range, and builds the new generation;
  - it switches every `BASE_DIRS` mount to it in one step under the mount
    table's lock; descriptors already open keep the old generation;
  - the mounts stay pinned and sealed.
- `base_monitor() -> fd` turns readable whenever the generation changes, as
  `net_monitor`'s does.
- Both calls fit the private table: 44 of 48 slots are used
  (`abi/src/syscall/numbers.rs:33-39`).
- `BaseUpdate` joins `slopos_priv_core::DELEGABLE` and init's grant to privd.

### Replacing a base: `bootctl`

`bootctl base install <image>`:
1. reads the archive and refuses one that is not a base;
2. writes `previous` and `current` by the rule above, through `RawBlock`;
3. activates the image through `BaseUpdate` when the running kernel serves it,
   and says so either way.

`bootctl base rollback` does the same with `previous`. `bootctl status`
reports both bases' build tags and interface numbers, and which one runs.

The policy gains:

| Action | Command | Capabilities | Allow |
|---|---|---|---|
| `org.slopos.base.install` | `base install *` | `RawBlock Power BaseUpdate`, `cwd`, exclusive `boot-slots` | `root`; `session consent`; `paired` |
| `org.slopos.base.rollback` | `base rollback` | `RawBlock Power BaseUpdate`, exclusive `boot-slots` | `root`; `session`; `paired` |

`org.slopos.boot.install` takes `install <slot> <kernel>`. A rollback needs
no dialog, because the person approved that base when it was installed.
`Power` finds the boot partition, through `LoaderDevicePartUUID`; Phase 3
ships both actions without `BaseUpdate`, which Phase 5 adds.
`org.slopos.boot.set-default` gains `RawBlock`, to read the slot's kernel.

### `libc.so` as the stable interface

- A program outside the base links `libc.so` dynamically. It reaches
  SlopOS-only calls only through `libc.so`'s exports, never by a raw syscall.
  Calls with Linux numbers stay stable by the rule
  `abi/src/syscall/numbers.rs:3-9` already states.
- `libc.so` only grows. `scripts/check_libc_exports.sh` holds its dynamic
  symbol table to `scripts/gates/libc/exports.txt`:
  - a removal fails;
  - an addition is a diff to that file;
  - the size, alignment and field offsets of every struct and union the
    headers define are held to `scripts/gates/libc/layouts.txt` — every one,
    since exports also take structures through `void *`.
  
  It carries a `--self-test`.
- A statically linked program outside the base is unsupported, as on macOS.
  Base programs stay static: they change with the kernel's interface number.

### The compositor protocol

- The compositor serves every protocol version from `PROTOCOL_VERSION_MIN` to
  `PROTOCOL_VERSION`, and speaks the version a client's `Hello` names.
- A client accepts any compositor at its version or newer.
- A change adds a version. Raising the minimum is a stated decision with a
  reason, never a side effect.

### Apps outside the base

- **Build.** `editor`, `file_manager` and `image_viewer` are built as a
  person's `cargo build` would build them: against the target's std, linked
  dynamically against `libc.so` with `--dynamic-linker=/lib/ld-slopos.so.1`.
  `scripts/check_loose_programs.sh` fails one that is static, lacks
  `libc.so` in `DT_NEEDED`, or holds a `syscall` instruction. It carries a
  `--self-test`.
- **Location.**
  - programs in `/usr/local/bin/`;
  - app entries in `share/slopos/apps/<id>.app`, as `key=value` lines: `id`,
    `name`, `exec`, `icon`, the icon a PNG file named in the same share's
    `slopos/icons/`.

  The base carries the entries for its own apps (terminal, sysmon) under
  `/usr/share/slopos/apps/`.
- **The dock** reads entries at session start from `/usr/share/slopos/apps`,
  `/usr/local/share/slopos/apps` and `~/.local/share/slopos/apps`, in that
  order, a later entry replacing an earlier one with the same id.
- **The shell's registry** drops the apps. `PATH` finds them.
- **The loader** appends `/usr/local/lib` to its default search, only for a
  process that is not `AT_SECURE` and whose effective uid is not 0.
- **Install.** The apps form a tree named `apps`, beside the toolchain's
  `usr_local` at `/usr/local`. It reaches a root four ways:
  - the host image builders install it as a host tree, like the toolchain;
  - the install medium carries it at `apps/`, and the installer installs it
    onto the new root;
  - on a live boot, init installs it into the RAM root's `/usr/local`;
  - in the guest, `scripts/selfhost.sh install` installs it as the session,
    through the `tree-install NAME TREE DEST` it built, a base program over
    `tree-core` that holds no capability.

### Nothing with authority runs loose code

- init's services run from absolute base paths.
- privd's mechanisms search `MECHANISM_PATH`, `/bin:/sbin`
  (`priv-core/src/lib.rs:56`). The rescue shell keeps it.
- The loader rule above keeps `/usr/local/lib` from uid 0 and from every
  `AT_SECURE` process.
- A test may break this rule; the shipped system may not.

### init as a service manager

- **Services.** Each service names:
  - its program;
  - its capabilities, uid and tier;
  - whether it is restarted when it crashes (privd, remoted, compositor), at
    most five times a minute;
  - whether it is session-bound (compositor, terminal).
- **A base switch.** init watches `base_monitor`. On a switch it compares the
  old and new `base-manifest` digests of each service's program:
  - **privd** drains: it stops accepting, lets running mechanisms finish, and
    exits. init starts the new privd, which takes over `/run/privd`. `elevate`
    retries a refused connect for two seconds.
  - **remoted** restarts. Its broker redials.
  - **A session-bound service** stays. The compositor shows a notice offering
    a session restart, which the person may postpone.
  - **`/sbin/init`** re-executes itself and keeps its process. It passes its
    service table and descriptors in a pipe.
- **The consent pair outlives both of its users.** init keeps both ends for
  the boot and hands one to each privd and the other to each compositor
  instance. A new instance sends `Reset` first, and the other side drops what
  was pending. `SCM_RIGHTS` cannot carry a socket, so the pair cannot be
  re-handed any other way.
- **The login tag.** A process carries a login tag that fork, spawn and exec
  inherit and that only a `SetId` holder sets, at spawn. init gives the
  desktop one tag and remoted another. `process_list` reports it.
- **Ending a session.**
  - init signals `SIGTERM` to every process carrying the desktop's tag, then
    `SIGKILL` after five seconds;
  - it starts the compositor and the terminal again, re-handing the consent
    end;
  - remoted and what it runs carry their own tag and survive.
- **`/run/init`** accepts `restart-session` from the session uid and from uid
  0, and `end-session` and `reexec` from uid 0, judged by `SO_PEERCRED`. The
  compositor's system menu and the base program `/bin/session restart` send
  the first.

### Rescue on the install medium

- The kernel sets `BOOT_FLAG_MEDIUM` in `sys_info` when it installs the medium
  (`boot/src/boot_services.rs:571`).
- The policy grammar gains a `medium` line: an action that carries it exists
  only when that flag is set.
- `org.slopos.rescue` runs `/bin/shell`:
  - with every delegable capability, as uid 0;
  - `allow root`, `allow session consent`, never `paired`;
  - `medium`, with a `describe` that says it controls every disk.
- `elevate` relays its input, as it does the installer's.
- **The rescue shell passes its power on.** Started with
  `PRIV_ACTION=org.slopos.rescue`, the shell:
  - runs every command through spawn, delegating its whole mask, since exec
    drops gated capabilities;
  - removes `PRIV_ACTION` from the environment those commands get, so a
    mechanism holding its action's capabilities acts itself.
- coreutils gains `mount` and `umount`.
- `bootctl --boot <disk>` (`userland/src/apps/bootctl.rs:572-575`) already
  names another disk. With it, `base install` and `base rollback` repair an
  installed system's boot partition from the medium.

### Building and installing in the guest

- `scripts/selfhost.sh install` builds the system and the apps tree, runs
  `bootctl base install` and installs the apps tree. When the base needs a
  kernel the running one cannot serve, it says so and leaves the base for the
  next boot.
- `scripts/selfhost.sh install-kernel` builds the kernel and installs it into
  the spare slot with one boot armed, as `install` does today.
- `just remote-install` installs the base live and the kernel only when it
  changed. It waits for remoted's base tag, and for the kernel tag only across
  a reboot.

## Phases

Six phases. Each ends with something that runs, and each lands as commits of
its own.

### Phase 1: The contracts — done

- `slopos_abi::BASE_INTERFACE` and `BASE_INTERFACE_MIN`, both 1
  (`abi/src/base_interface.rs`). The kernel reports the range in `sys_info`
  (`base_interface_min`/`_max`, printed by the shell's `info`) and carries it
  in `.note.slopos.base-interface`, which `link.ld` keeps and asserts.
- `scripts/check_base_interface.sh` with `scripts/lib/base_interface.py` and
  the roots and ledger in `scripts/gates/base-interface.txt`; interface 1 is
  recorded. The boot flags and the seat ranks are now stated once, in abi.
- `scripts/check_libc_exports.sh` with `scripts/lib/libc_layouts.py`,
  `scripts/gates/libc/exports.txt` (932 exports) and `layouts.txt` (84
  records), checked against clang 18 and 23.
- `PROTOCOL_VERSION_MIN` beside `PROTOCOL_VERSION`, both 3, negotiated in
  `slop-protocol`'s server; every compositor `Error` now ends its
  connection. `protocol_test` (`utest_protocol`) starts the real compositor:
  a client at the minimum draws a window, and one outside the range, one
  speaking before `Hello` and one reusing a surface id are refused.

### Phase 2: Apps leave the base — done

- `editor`, `file_manager` and `image_viewer` are `slopos-apps` (`apps/`),
  built by `build_userland.sh` with the target's std and unwinding, linked
  against `libc.so` by the clang driver's line, and staged with their entries
  and icons as `builddir/apps`. `check_loose_programs.sh` holds them after
  every host userland build; its `--self-test` runs in the gates.
- `appkit`, `windowing` and `slop-protocol` call `slopos-cabi`, safe wrappers
  over `libc.so`'s exports; appkit waits in `poll`, not the ring. `libc.so`
  gained `memfd_create`.
- App entries and 64 px PNG icons, the base's in `assets/`, the apps' in
  `apps/share/`. The dock pins them in name order, reading entries and icons
  bounded and non-blocking. The shell's registry dropped the apps.
- The loader searches `/usr/local/lib` last for a process neither
  `AT_SECURE` nor uid 0's.
- `tree-core` keys manifests by name (`TOOLCHAIN`, `APPS`) and records each
  tree's destination in an `at` line, refusing a reinstall elsewhere.
  `slopos_userland::tree` scans and installs a tree for the installer, init
  and `/bin/tree-install`; init hands the session the apps' manifest, and an
  install reads only one uid 0 or the installing uid wrote. `FS_HOST_TREES`
  takes `<host>:<guest>[:<name>]`.
- The apps tree is on the tests, verified, persistent and self-hosting
  images, on the medium at `apps/`, on a live boot's RAM root, and installed
  by `selfhost.sh install`.
- The installer mounts the new root inside a directory only uid 0 may
  search.
- Graded by `apps_test` (each app, run from `/usr/local/bin`, connects to a
  stand-in compositor and names its window), `dock_test` (with links, FIFOs,
  oversized files and a hundred entries in the person's share),
  `tree_install_test` (the session's own tree over a manifest it rewrites in
  place), `dl_test`'s root and session cases, `tree-core`'s and
  `fs_tree.py`'s tests of two trees at one destination, and
  `test-installer`'s markers for the apps.

### Phase 3: The boot partition — done

- `boot-core`: `layout::BASE_*` (slots hold kernels; no slot is `base`),
  every entry rendering both modules; `newc`, the one reader and writer of
  the archive format, under the kernel's `fs::cpio`, the packer, `bootctl`
  and the tests; `base` (interface, manifest lines, `choose`, `check`,
  `describe`); `kernel::served_interfaces`, the note found by section name.
- `tools/initramfs` writes `base-interface` and `base-manifest` and holds its
  output to `base::check`.
- The kernel chooses in the loader handoff (`boot/src/base.rs`), logs
  `BOOT: base current|previous (<n> bytes), interface <i>`, fails `initramfs
  root` naming both readings when it serves neither, and hands the unused
  base's pages to memory init (`init_memory_system_pre_typestate`'s
  `unused_base`). The ISO stages its one base at `/boot/base/current.img`, as
  the medium serves it (`MEDIUM_BASE`).
- `bootctl install <slot> <kernel>` and `clone` move a kernel alone and refuse
  one that serves neither base; `bootctl base install <image>` and `base
  rollback` (`org.slopos.base.*`) write the bases offline by the previous
  rule, guarding the default and running kernels; `set-default` takes only a
  slot that serves a base; `status` names each base's interface, build tag
  and whether it runs.
- The installer writes both kernel slots and the base (removing a kept
  boot partition's per-slot `base.img`); `build_bootdisk.sh` refuses a base
  that does not check out or a kernel that does not serve it;
  `selfhost.sh install` and `remote.py install` install the base, then the
  kernel; `bootdisk base|kernel` answer the host's recipes.
- Graded by `boot-core`'s host tests (rendering, `newc`, `base`, `check`, the
  note), the kernel's `base_tests` (current, previous or neither over
  synthetic archives, and a two-base boot's freed pages in usable memory),
  `test-install` (both kernel slots on one base; a base installed, booted,
  rolled back; an unserved one passed over for previous; an unserved kernel,
  and a base or default leaving the default unserved, refused),
  `test-installer` (the layout on every disk, the built base running, the
  reinstall keeping the root and removing a per-slot base) and `test-remote`
  (a new base and kernel installed as the paired agent).

### Phase 4: init as a service manager — done

- **Kernel.** A process carries a login tag (`Process::login`) that fork,
  spawn and exec pass on and that a `SetId` holder sets at spawn through
  `SpawnLoginAttrs`; `process_list` reports it. Init's exec of the base's
  `/sbin/init` as its sole thread keeps its capabilities
  (`exec::exec_keeps_authority`), the authority proof's `Reexec` step, whose
  guard a kernel test grades. Base interface 2, minimum still 1. A
  `recvmsg` that finds no bytes no longer takes, and drops, descriptors sent
  with bytes that land under it.
- **init** (`userland/src/apps/init/`): the service table with rate-limited
  restarts, the session's start, end (`SIGTERM`+`SIGCONT`, `SIGKILL` at 5 s,
  given up at 10 s) and restart, `/run/init`, re-exec through a pipe, and the
  test runner as `/sbin/init --run-tests`. `/bin/session restart|end`, and a
  system menu under the bar's icon with "Restart session".
- **Consent across restarts**: `consent::Link`'s `RESET`/`RESET_SEEN`,
  `READY` as a one-question credit, `Refusal::Interrupted`, init resetting
  for a side that exited and discarding what is written to one not running,
  and the compositor reading the channel through a oneshot poll. A privd
  started again refuses exclusive actions while its predecessor's mechanisms
  run. The compositor waits up to five seconds for seats a predecessor's exit
  still holds; `elevate` waits two seconds for a privd being started again.
- Graded by the kernel's `test_a_login_tag_is_set_only_with_set_id` (spawn
  and fork) and `only_init_reexecuting_itself_keeps_its_authority`,
  `spawn_privilege_test` (a tag through spawn, fork and exec; refused without
  `SetId`), `supervision_test` (re-exec keeps privd; a
  killed privd is replaced by one that serves; strangers and the session are
  refused what is uid 0's, and a session uid 0 ended; a session restart ends
  the desktop's tagged process, spares remoted's, starts a new session),
  `privd_test` (asked; a question on screen interrupted when the session ends,
  with nobody left to ask; asked again once it restarts) and `priv-core`'s
  host tests of the handshake and credits.

### Phase 5: Live activation

Base generations, identity per generation, `base_activate` and
`base_monitor`, `BaseUpdate`, `bootctl` activating live, init's
digest-driven restarts and the compositor's notice. The `selfhost.sh` and
`remote-install` changes.

Graded by:
- kernel tests:
  - a process from generation N keeps its identity and its path after N+1
    activates;
  - generation N stays resident while mapped and is freed after;
  - the six mounts switch together;
  - an archive outside the kernel's range is refused;
- a utest that activates a fixture base differing in one program and in
  privd, and finds:
  - a new exec running the new bytes;
  - an old process attested in its own generation;
  - privd restarted and serving;
- `test-install` with a base installed live, then a reboot that keeps it;
- `test-remote` installing a base without a reboot.

### Phase 6: Rescue

`BOOT_FLAG_MEDIUM`, the `medium` policy line, `org.slopos.rescue`, the
shell's rescue mode, and `mount` and `umount`.

Graded by:
- `privd_test`: the action absent without the flag;
- `test-installer`: from the medium, the rescue shell mounts an installed
  root, runs `e2fsck`, and rolls its base back with `bootctl --boot`.

## Grading

- **Gates:**
  - the three new gates and their self-tests;
  - the authority histogram for the new calls and `BaseUpdate`;
  - `check_authority_reachability.sh`;
  - `check_unsafe_outside_ostd.sh`;
  - the Verus proof, where `BaseUpdate` and the login tag touch what it
    models.
- **Ratchets:** new locks (generations, the mount switch, init's control
  socket) and new tests move the lockdep and test-count ratchets. Re-measure
  with `--emit-allowlist` and name the cause in the commit.
- **By hand, on the laptop:**
  - a base installed live while a session runs;
  - the compositor's notice and a session restart;
  - a rollback;
  - a rescue from the USB medium.

## Out of scope

- **An emergency console** on Ctrl+Alt+F2: a shell on the kernel's text
  console when the desktop hangs. init's service table is where it will
  start, so this plan leaves room for it.
- **A rescue entry in the boot menu**, without a USB stick.
- **chroot and per-process mount views.** slibc's `chroot` answers `ENOSYS`
  (`slibc/src/ffi/syscalls.rs:851-856`). Repair works on a mounted disk.
- **Hot reload of the compositor** without a session restart.
- **A root-owned area for loose system services** that run with more than the
  session's power.
- **Per-app permissions or sandboxing.**
- **A package manager** for loose software.
- **Faster kernel updates** that skip the firmware.
- **A consent question on the text console.**
- **SlopOS-only calls exported through `libc.so` beyond what it uses today.**
  Each one is added when a program outside the base needs it.

## Decided

- **Only a kernel change needs a reboot.** The rest of userland is replaced
  while the system runs. Breaking it is allowed, and the medium repairs it.
- **One sealed base, replaced whole.** Not several packages: one unit is the
  thing the security model can name.
- **The code keeps the name "base".** It is the core package. Renaming
  `BASE_FS`, `BASE_DIRS` and `Program::Base` would be churn that changes
  nothing.
- **Base membership follows the rule, not taste:** trusted, needed to repair,
  or speaking a SlopOS-only call directly. The last clause keeps sysmon, ss,
  nmap, ping, nc and oops_smoke in the base, as FreeBSD's base keeps its
  ping, netstat and top.
- **Installing follows what an install touches.** Writing into a home or
  `/usr/local` needs no permission. Replacing the base asks the person.
- **The base lives on the boot partition, shared by the kernel slots.** A
  damaged root still boots to a shell. Booting the other kernel never changes
  the userland.
- **The kernel boots the newest base it serves,** current, then previous.
- **An interface number, not a shared build,** ties kernel and base, so either
  can change without the other.
- **macOS-style stability:** `libc.so` is the contract, calls with Linux
  numbers are stable anyway, and static programs outside the base are
  unsupported.
- **The compositor protocol is versioned now,** because three GUI apps leave
  the base in this plan.
- **The toolkit is outside-the-base code.** `appkit`, `windowing` and
  `slop-protocol` reach the system only through `libc.so`'s exports
  (`slopos-cabi`), so a base program and an app run the same code, and
  nothing an app links makes a system call in place. The ring is for base
  programs alone.
- **Apps keep the target's panic strategy.** They unwind, through the
  unwinder `libc.so` carries; base programs stay `panic = abort`.
- **A tree is named, and its manifest says where it went.** Trees share a
  destination, kept disjoint by `tree-core`'s refusal. Keyed by name, a
  manifest would otherwise apply its removals wherever an install named;
  the `at` line refuses that.
- **A manifest belongs to whoever installs its tree again.** The apps' is the
  session's, which keeps them with no capability: `tree-install` rewrites it
  in place in uid 0's directory, which it may not add to, and init makes it
  for the session on a root without one, owned before it is in place. The
  toolchain's is uid 0's. `tree-install` refuses a manifest another uid
  keeps, so a uid 0 run never takes over a tree the session keeps.
- **An installer acts only on a record it trusts.** A manifest neither uid 0
  nor the installing uid wrote is not read, and one that does not parse, as
  an in-place rewrite cut short leaves, is not believed: either stands for
  the new tree unfinished, whose paths the install takes back, so nothing the
  session writes in a manifest steers what uid 0 removes. A skip also needs
  the recorded destination and every entry of the kind and size recorded; it
  checks no content, so a reinstall keeping the root does not restore what
  the session changed in place inside its own trees, and leaves an app the
  new tree dropped, entry and all, for the session to remove. A per-tree
  directory the session owns would have made the rewrite a rename, at the
  cost of a second shape of manifest directory.
- **On a host-built root the host's record counts.** It stays outside the
  image, so the guest's copy takes its tree's owner. A guest install over a
  host tree stands until the host's tree changes; the host then replaces what
  its own record names and refuses, with a warning each build, a path the
  guest's install added that its tree now adds too.
- **The apps are no slot's.** They follow the `libc.so` they link, which the
  base carries: `selfhost.sh install` installs them at once only over the
  same `libc.so`, and a rollback keeps them. Exports only grow, so apps
  built against an older `libc.so` run on a newer one; apps installed for a
  base that was then rolled back may need an export the older one lacks,
  and are installed again from a build of the base that runs.
- **The apps' manifest is computed from the tree where it is installed**: its
  identity is a digest of every entry, so no manifest ships beside the apps to
  disagree with them. The toolchain's comes with the payload, written by the
  host's `fs_tree.py`, whose identity is its own; the two are never
  compared.
- **App entries rank like the XDG data directories** (`~/.local/share` over
  `/usr/local/share` over `/usr/share`, by id) and the dock orders them by
  name, as Redox's launcher does. An entry passes over a key it does not
  know, so a newer entry still works on an older dock.
- **What the session writes, a holder of more reads bounded and
  non-blocking.** The compositor holds the seats and draws the consent
  question, and reads entries and icons the session can replace with a FIFO
  or a huge file.
- **A base install asks the session for consent; a rollback does not.** root
  and the paired agent need no consent, as for `boot.install`.
- **Only what changed restarts.** A changed compositor or terminal asks for a
  session restart and never forces one.
- **Repair is the medium's:** one consent, a rescue shell with every delegable
  capability, and tools that work on a mounted disk. There is no chroot.
- **Previous always holds the base that last ran.** Two uninstalled bases in a
  row never push the running one off the disk. The running base is the one
  whose manifest `/usr/share/slopos/base-manifest` serves, on the disk the
  system booted from: no flag records which module a boot took, so a live
  switch (Phase 5) changes what runs without a second record to keep true.
  The manifest names every record, its mode and digest, so equal manifests
  are one tree. Installing the image current already holds writes nothing.
- **No base write leaves the default or the running kernel serving neither
  base.** A base install checks the kernel notes of the default slot, the
  running one and the one armed to boot once first, since a try that fails falls back on the default, and
  `set-default` takes only a slot whose kernel serves a base on the
  partition. Writing both bases, an install or a rollback writes first over
  the image the default does not need, and ignores the termination signals
  privd relays once the writes begin. A power cut
  between the two writes still leaves one image twice. Both bases together
  take at most half the machine's memory, since every boot loads both.
- **An image a privileged install reads may name its digest.** `bootctl
  install` and `base install` take a SHA-256 after the path and install only
  those bytes: `remote.py` pushes as the session, which may replace the file
  before the paired agent's install reads it.
- **The kernel chooses before memory is laid out.** It reads only each base's
  interface, in the loader handoff, and the unused base's pages are usable
  memory before the frame allocator is seeded: no API gives the allocator
  memory after boot, frame descriptors and metadata slots cover those pages
  as any others, and nothing can read the module after. The page range comes
  from the module, every whole 4 KiB page it covers, so nothing else can lie
  in one; its memmap entries are not authoritative.
- **The base the kernel would index is decided before it reaches the disk.**
  The other base is freed before the chosen one is indexed, mounted and
  unpacked, so a base any of those would refuse must never be chosen:
  `boot-core::base::check` (canonical names, each once and none beneath a
  file; the base directories, those on the way to them and the kernel's own
  mount points directories, nothing in devfs; symlinks the kernel takes; an
  interface; the manifest, read in one pass in archive order) is what the
  packer writes, what `bootctl base install` takes and what the host's disk
  builder takes. It allocates nothing and compares records pairwise, so it
  caps a base at 2048 records.
- **One reading of the archive.** `boot-core::newc` reads and writes `newc`
  for the kernel's base and RAM root, the packer, `bootctl`, the host tools
  and the tests, and a path names the last record at it, as the base's index
  serves it.
- **A kernel's note is read by section name**, through the section headers;
  `bootctl status` reads no kernel, since the FAT reader reads whole files
  and a tests kernel is 130 MB.
- **A base goes in before the kernel that needs it.** `bootctl install`
  refuses a kernel serving neither base, so `selfhost.sh install` and
  `remote.py install` install the base first: a kernel that serves only a
  newer interface is accepted against it.
- **No slot is named `base`**: the bases' directory sits among the slots'.
- **No in-place migration.** A reinstall keeping the root moves an installed
  disk to the layout, because `bootctl` never writes the ESP; it removes the
  per-slot `base.img` a kept boot partition held.
- **The interface surface is the compiler's view of `slopos-abi`.**
  `check_base_interface.sh` digests rustdoc's JSON, not source text: each
  private call's number and the backticked signature its doc states, register
  packing included, what those signatures name and the docs link, everything
  reachable through field and alias types, and `root` globs for what no
  signature can name. The methods of a rooted type are in it, since they
  carry wire packing, so refactoring one takes a bump. Linux's own shapes a
  private call passes through (`sockaddr`, `msghdr`, `iovec`) are held by
  Linux compatibility instead. What a private call takes from outside
  `slopos-abi` — the font's glyph ranges, the kconsole keys, the EFI GUIDs,
  limits written into a handler — is not held and moves into abi before it
  changes; an encoding another crate keeps for an abi value is held to it by
  a const assertion where both meet, as `sched::cpufreq` does. Every call a
  later phase adds states its signature in its doc.
- **An issued interface is never rewritten.** The ledger only appends, and a
  changed surface takes the next number. `BASE_INTERFACE_MIN` rises only when
  a change takes away or reshapes something an older base calls. A bump the
  surface did not need still costs: a base built after it refuses the older
  kernels.
- **The kernel's note is a section, not a segment.**
  `.note.slopos.base-interface` (owner `SlopOS`, type
  `NT_SLOPOS_BASE_INTERFACE` = 1, descriptor `min, max` as little-endian
  `u32`s) sits in the read-only `PT_LOAD` with no `PT_NOTE`: Limine ignores
  notes, and a new `PHDRS` keyword would need a linker-gate probe.
- **`libc.so` holds its exports and every header record.** A data export
  carries its size. Field types are not held, since clang's spelling of them
  moves between releases; nor are function prototypes, or an alignment set on
  a typedef rather than its record. `--record` only adds.
- **The protocol handshake is fixed.** Both `Hello`s keep their encoding for
  good. The compositor's `Hello` is all it sends before the client's; the
  first request must be a `Hello` naming a served version, and anything else
  is answered `Error { object_id: 0, code }` (`VersionMismatch` or
  `MalformedMessage`) before the connection closes. The server records each
  client's version for the first request or event that differs by version.
- **A compositor `Error` ends the connection.** It is always on object 0, its
  code a `ProtocolError` value fixed on the wire, and the close follows: a
  reused or zero surface id is `MalformedMessage` and a full surface table
  `BufferFull`, as Wayland makes a protocol error and running out of memory
  fatal. A client never has to tell a fatal error from a recoverable one.
- **A login tag names a role, not an instance.** The desktop is always
  `DESKTOP` and remoted `REMOTE`, so every restart ends every process the
  desktop ever started, a straggler from an earlier session included. Only a
  `SetId` holder sets one, at spawn; nothing changes a process's tag after,
  as Linux's audit session id is set once at login.
- **The tag joined the private surface compatibly.** `UserTaskEntry.login`
  sits in what was padding and `SpawnLoginAttrs` is read past `SpawnAttrs` only
  under `SPAWN_SET_LOGIN`, so a kernel serving interface 2 still serves 1.
- **Init alone keeps its capabilities across an exec,** and only re-executing
  the base's `/sbin/init` as its sole thread. The exec rule is otherwise
  unchanged; this is the authority proof's one exception to S2.
- **A compositor that exits takes its session with it.** Its clients lost
  their surfaces anyway, so init ends what carries the desktop's tag and starts
  the session again, which the five-a-minute limit bounds. Any
  `restart-session` clears that limit; nothing else does.
- **A start limit delays, never gives up.** Past five starts in a minute the
  next waits until the minute allows it, so a service that crashes on a
  transient fault comes back without anyone asking.
- **A restarted privd refuses exclusive actions while its predecessor's
  mechanisms run.** They carry the tag `PRIVD`, and it notes the ones alive
  when it starts: it cannot know which groups they hold, and two writers of
  one boot partition is the failure. Waiting for them to end instead would
  let one stuck mechanism keep privd, reboot included, down for the boot.
- **uid 0's `end-session` holds.** The session cannot restart a session uid
  0 ended; only uid 0's `restart-session` or a failed test run starts it
  again, and no number of the session's requests waiting crowds uid 0's out.
- **Init's state crosses an exec in a pipe,** not a mapped file: under 4 KiB,
  written before the exec, it cannot fail to be read. Its first line names
  the descriptors kept, in a form every version reads, so an image that
  cannot read the rest, as a newer init's may not be, closes them, adopts
  the privd and remoted among its children, starts the session again and runs
  without consent until the next boot.
- **A session end is a scan, not an atomic kill.** Init signals what carries
  the tag, again each pass, so a process forking faster than that can outlast
  the ten seconds; the requester is told how many did. A kernel kill by tag,
  as a cgroup's `cgroup.kill` is, would close it.
- **Init answers a session request once it is done.** `end-session` returns
  when the session's processes are gone, `restart-session` likewise (its
  requester is usually among them), and `reexec` from the new image, which
  gets the request's descriptor in its state. Re-exec is refused while a
  session starts or ends, whose timers and ready pipe the state does not carry.
- **The consent pair is resynchronised, never re-made.** A new instance's
  `RESET` names a fresh epoch and it acts only after `RESET_SEEN` names it
  back, so nothing written for a predecessor is read as its own — above all no
  answer to a question with the same id. A `RESET` interrupts every pending
  question rather than re-asking it. privd sends a question only on a `READY`
  sent since the last, which bounds what one side ever has unread to one
  question and one cancel: every send fits the socket's buffer whole, so the
  stream is never left torn by a side that exits mid-frame. Init discards
  what is written to a side that is not running, so nothing piles up for its
  next instance; that may discard a peer's own `RESET`, so a side whose
  `RESET` is unanswered sends it again with each `RESET_SEEN`, and a `RESET`
  heard again from the same epoch is only acknowledged, or each copy would
  grant privd another `READY`.
- **Init speaks for a side that exited.** It writes a `RESET` toward the
  survivor, so privd refuses at once what a dead compositor was showing and a
  compositor drops what a dead privd asked. It is the only consent frame init
  writes.
- **A reader that must not miss a frame polls oneshot.** The ring's multishot
  `OP_POLL_ADD` posts only when readiness changes between two of the reader's
  own harvests, so bytes landing after a drain and before its next
  `ring_enter` wake nothing. The compositor's consent task re-arms a oneshot
  after each read, as the runtime's wakeup pipe does; its protocol client
  tasks and the net monitor still stream a multishot and can stall the same
  way until the kernel re-fires a row whose file was woken since its last
  completion.
- **The test runner is a copy of init.** `run_userland_tests` runs in
  `/sbin/init --run-tests`, holding `TestHarness` alone, so init supervises
  while the suite runs and a test can kill privd or restart the session.
- **`elevate` waits for a restarting privd** two seconds, from Phase 4 on,
  since a crash restart leaves the same window a base switch will.

## Constraints

- `slopos-ostd` remains the only kernel crate with `unsafe`.
- A Linux syscall number carries Linux semantics or `ENOSYS`. The new calls go
  in the private range, and any change to that range bumps `BASE_INTERFACE`.
- slibc stays `MIT OR Apache-2.0`. The export gate is a tracked list, not code
  taken from anywhere.
- Concepts only from systemd, NixOS, Android and FreeBSD. Cite the documented
  behaviour, never their files.
- Every phase updates `AGENTS.md` where it changes a stated invariant, and
  runs the security sweep over the syscall paths it touched. `base_activate`,
  the generation lifetime and the rescue action come first.
