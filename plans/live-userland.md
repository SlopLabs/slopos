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

**The base is per slot and fixed for the boot.**
- Each slot holds a kernel and its base: `/boot/<slot>/kernel.elf` and
  `base.img` (`boot-core/src/layout.rs:93-96`). Each Limine entry loads its
  slot's `base.img` as module `initramfs` (`boot-core/src/limine.rs:153-191`).
  Booting the other slot therefore also swaps the userland.
- The kernel takes the module named `initramfs`, else the first
  (`boot/src/limine_protocol.rs:440-469`). Module memory stays mapped for the
  kernel's lifetime.
- `BASE_FS` is a static whose index is a `OnceLock`, installed once at boot
  (`fs/src/basefs.rs:274-318`). File data are slices of the module.
  `MEDIUM_FS` is a second static instance for the install medium (`:283`).
- `BASE_DIRS` are mounted read-only and pinned (`fs/src/vfs/init.rs:536-555`).
  The syscall layer refuses to unmount them or mount over them
  (`core/src/syscall/fs/mount_handlers.rs`).
- Sizes, measured: the shipped base is 21 MB and the release kernel 12 MB. The
  boot partition is 1 GiB (`boot-core/src/layout.rs:42`).

**Program identity assumes one base per boot.**
- Exec records `Program::Base(inode)` when the image and its interpreter come
  from `BASE_FS`. The check is pointer identity against the static
  (`core/src/exec/mod.rs:761-766`, `fs/src/basefs.rs:287-289`).
- The identity is an inode number in an `AtomicU64`
  (`slopos-ostd/src/process/identity.rs:8-27`). `SO_PEERPROGRAM` turns it into
  a path through whatever `BASE_FS` is now
  (`core/src/syscall/net_handlers.rs:711-730`).
- privd reads its policy and whether the base is paired once, at start
  (`userland/src/apps/privd/mod.rs:82-88`).

**init starts everything once.**
- The service table (`userland/src/apps/init_process.rs:46-79`) is spawned
  once. Nothing restarts a service.
- init hands its end of the consent pair to the compositor and drops it
  (`:245-249`). After the compositor exits, init only reaps (`:280-285`).

**What a program outside the base would meet.**
- The dock pins five compiled paths (`userland/src/apps/compositor/dock.rs:137-173`).
  The shell's registry is compiled in (`userland/src/program_registry.rs`).
- The loader searches only `/lib` and `/usr/lib` (`slibc/src/ld_so/mod.rs:56-57`).
- The compositor protocol demands an exact version match
  (`slop-protocol/src/types.rs:3`, `slop-protocol/src/client.rs:57-58`).
- The shipped programs are static: slibc is linked as an rlib, and raw
  syscalls are made through `slopos_userland::syscall`.
- `libc.so` reaches four SlopOS-only calls itself: `spawn_path`, `resolve`,
  `net_query` and `klog_write` (`slibc/src/pal/slopos.rs:1121-1416`). It
  exports one SlopOS-named entry to them, `slopos_spawn_path`
  (`slibc/src/ffi/syscalls.rs:1070`).
- These programs make SlopOS-only calls directly:
  - sysmon: `sys_info`, `process_list`, `percpu_stats`, `cpu_perf`, `cpu_info`
    and `net_query` (`userland/src/apps/sysmon/state.rs:196-233`);
  - ss and nmap: `net_query`;
  - ping and nc: `resolve` and the rings;
  - oops_smoke: `test_panic`.
  
  The editor, the file manager and the image viewer make none.

**Kernel and base are never checked against each other.** Both carry a build
tag (`core/src/syscall/core_handlers.rs:121-143`,
`tools/initramfs/src/main.rs:354-358`). Nothing compares them, and the private
call shapes in `abi/` change freely.

**Trees.** `tree-core` installs a tree by manifest, replacing what the old
manifest names and keeping what the user added. It keys the manifest by
destination, `usr_local` for `/usr/local` (`tree-core/src/lib.rs:33-35`), so
one tree owns a destination. `scripts/fs_tree.py` applies it on the host, and
the installer applies it for the medium's toolchain
(`userland/src/apps/installer/root.rs:473-486`).

**Building in the guest.** `scripts/selfhost.sh install` builds the kernel,
the userland and the base, installs both into the spare slot and arms one boot
of it (`scripts/selfhost.sh:76-82`).

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| FreeBSD base and `/usr/local` ([handbook](https://docs.freebsd.org/en/books/handbook/cutting-edge/)) | The base is one unit maintained with the kernel; third-party software lives in `/usr/local`; a userland-only update restarts services, not the machine | The split, and the base keeping the tools that speak the kernel's own interfaces (ping, netstat, top) |
| macOS: sealed system volume and libSystem | System files are sealed; the only stable interface is the system library, not raw syscalls | `libc.so` as the contract for programs outside the base |
| Fuchsia base packages ([packages](https://fuchsia.dev/fuchsia-src/concepts/packages/package)) | The base is updated whole; other packages are updated independently | One sealed unit for the core |
| Android APEX ([format](https://source.android.com/docs/core/ota/apex)) and soft restart ([docs](https://source.android.com/docs/core/runtime/soft-restart)) | A module is swapped live and only its dependents restart | Restart only the services whose program changed |
| NixOS `switch-to-configuration` ([manual](https://nixos.org/manual/nixos/stable/)) | Activation restarts only the units that changed; old generations stay for rollback | Digest-driven restarts; `previous.img` |
| systemd `soft-reboot` ([man](https://www.freedesktop.org/software/systemd/man/latest/systemd-soft-reboot.service.html)) | Userspace switches root without reinitialising the kernel | Not taken: it ends every process, which a mount switch does not need |
| Haiku package activation ([infrastructure](https://www.haiku-os.org/docs/develop/packages/Infrastructure.html)) | Packages activate live; the last consistent state is recorded | "previous always holds the base that last ran" |
| Linux audit session id, systemd login scopes | A login session is a tag every child inherits, not the POSIX session | The login tag that bounds a desktop session |
| Linux live USB, macOS Recovery | Booting recovery media is full power for whoever holds the machine | The rescue action, offered only on the medium |

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
- `/usr/share/slopos/base-manifest`: one `<sha256> <path>` line per file.

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
  - `base rollback` writes `current` with the previous image, then `previous`
    with the image it rolled back from.
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
| `org.slopos.base.install` | `base install *` | `RawBlock BaseUpdate`, `cwd`, exclusive `boot-slots` | `root`; `session consent`; `paired` |
| `org.slopos.base.rollback` | `base rollback` | `RawBlock BaseUpdate`, exclusive `boot-slots` | `root`; `session`; `paired` |

`org.slopos.boot.install` takes `install <slot> <kernel>`. A rollback needs
no dialog, because the person approved that base when it was installed.

### `libc.so` as the stable interface

- A program outside the base links `libc.so` dynamically. It reaches
  SlopOS-only calls only through `libc.so`'s exports, never by a raw syscall.
  Calls with Linux numbers stay stable by the rule
  `abi/src/syscall/numbers.rs:3-9` already states.
- `libc.so` only grows. `scripts/check_libc_exports.sh` holds its dynamic
  symbol table to `scripts/gates/libc/exports.txt`:
  - a removal fails;
  - an addition is a diff to that file;
  - the size and field offsets of every header structure an export takes are
    held to `scripts/gates/libc/layouts.txt`.
  
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
    `name`, `exec`, `icon`;
  - icons as PNG files in `share/slopos/icons/`.
  
  The base carries the entries for its own apps (terminal, sysmon) under
  `/usr/share/slopos/apps/`.
- **The dock** reads entries at session start from `/usr/share/slopos/apps`,
  `/usr/local/share/slopos/apps` and `~/.local/share/slopos/apps`, in that
  order, a later entry replacing an earlier one with the same id. The compiled
  list goes.
- **The shell's registry** drops the apps. `PATH` finds them.
- **The loader** appends `/usr/local/lib` to its default search, only for a
  process that is not `AT_SECURE` and whose effective uid is not 0.
- **Install.** The apps form a tree named `apps`. `tree-core` keys a manifest
  by tree name rather than destination; `usr_local` stays the toolchain's, so
  two disjoint trees share `/usr/local` and the existing refusal keeps them
  disjoint. The tree reaches a root four ways:
  - the host image builders install it as a host tree, like the toolchain;
  - the install medium carries it at `apps/` beside its manifest, and the
    installer installs it onto the new root;
  - on a live boot, init installs it into the RAM root's `/usr/local`;
  - in the guest, `scripts/selfhost.sh install` installs it as the session,
    through `/bin/tree-install`. That is a base program over `tree-core` that
    holds no capability.

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
    service table and descriptors in a memfd.
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
  0, judged by `SO_PEERCRED`. The compositor's system menu and the base
  program `/bin/session restart` send it.

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

### Phase 1: The contracts

`BASE_INTERFACE` and its range, the ELF note and `sys_info`'s report;
`check_base_interface.sh`; `check_libc_exports.sh` with its tracked lists; the
protocol's version range. No layout changes.

Graded by each gate's self-test and by a test client speaking
`PROTOCOL_VERSION_MIN` against the current compositor.

### Phase 2: Apps leave the base

The dynamic build of the three apps and `check_loose_programs.sh`. App entries
and the dock reading them, the registry without the apps, and the loader's
`/usr/local/lib`. `tree-core` keyed by tree name and `/bin/tree-install`. The
apps tree on host images, on the medium, on a live boot's RAM root, and from
`selfhost.sh`. Tests that named `/bin/editor`, `/bin/file_manager` or
`/bin/image_viewer` follow them.

Graded by:
- `editor_test` and the app tests running the apps from `/usr/local/bin`;
- a utest holding the dock to the entries;
- `dl_secure_probe` extended to `/usr/local/lib`: searched as uid 1000, not as
  uid 0 or under `AT_SECURE`;
- `tree-core` host tests for two trees sharing a destination.

### Phase 3: The boot partition

The layout, both modules in every entry, the kernel choosing a base by
interface number and freeing the other. `bootctl install <slot> <kernel>` and
`bootctl base install|rollback`, offline, applied at the next boot. The
installer, `build_bootdisk.sh` and the ISO on the new layout.

Graded by:
- `boot-core` host tests of the rendered entries;
- kernel tests choosing current, previous or neither among synthetic archives;
- `test-install` booting both kernel slots on one base, and installing then
  rolling back a base across boots;
- `test-installer` on the new layout, its reinstall keeping the root.

### Phase 4: init as a service manager

The service table with crash restarts, the login tag, `/run/init`, ending and
restarting the session, the consent pair kept by init with `Reset`, init's
re-exec, and `/bin/session`.

Graded by:
- kernel tests of the login tag: inherited, and set only with `SetId`;
- a root test that kills privd and finds a new one serving;
- a root test that restarts the session and finds remoted's tagged process
  alive;
- `privd_test` asking for consent across a compositor restart.

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
- **A base install asks the session for consent; a rollback does not.** root
  and the paired agent need no consent, as for `boot.install`.
- **Only what changed restarts.** A changed compositor or terminal asks for a
  session restart and never forces one.
- **Repair is the medium's:** one consent, a rescue shell with every delegable
  capability, and tools that work on a mounted disk. There is no chroot.
- **Previous always holds the base that last ran.** Two uninstalled bases in a
  row never push the running one off the disk.
- **No in-place migration.** A reinstall keeping the root moves an installed
  disk to the layout, because `bootctl` never writes the ESP.

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
