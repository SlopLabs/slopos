# Principals, Brokered Authority and Consent

## Goal

Make authority something a holder hands on, never something a path earns at
exec. When this plan is done:

- Every process has a Unix identity: uid 0 for the system and uid 1000 for the
  one person at the machine. The filesystem enforces owner, group and mode
  bits, so `/etc` is no longer writable by every program.
- The per-task capability mask is the only thing that carries authority.
  `task.flags` describes where and how a task runs, and nothing it may do.
- A spawner may hand a child any subset of its own capabilities and nothing
  more. There is no grant table, no `Launch`, and no raise at exec.
- `/sbin/privd`, which init starts holding the delegable capabilities, runs a
  named action's mechanism program as a fresh child. Before it does, it checks
  who is asking: the uid, a process handle and the program identity, all
  attested by the kernel. Where the action requires it, it also waits for the
  person at the machine to allow the action in a dialog only the compositor can
  draw.
- Typing `halt`, `bootctl install`, `ip …` or `installer` in a terminal keeps
  working. A mechanism started without its action's capabilities asks privd to
  run it.

`plans/installer-gui.md` builds on all of this and starts once this plan is
done.

## Where it stands

**One flag word carries privilege, and it is full.** `abi/src/task.rs:200-355`
defines sixteen bits. Five of them describe how a task runs (`USER_MODE`,
`KERNEL_MODE`, `NO_PREEMPT`, `NEW_PGRP`, `FOREGROUND`) and the rest are
privileges. `TASK_FLAG_INSTALL` is "the last bit of the flag word"
(`abi/src/task.rs:286-295`). A u64 capability mask is derived from the word by
`caps_from_task_flags` (`slopos-ostd/src/authority/mod.rs:469-519`), which
calls itself "a bridge, not the model". The process-owned `Cred` it names as
its replacement exists only in comments (`core/src/syscall/dispatch.rs:47-55`).

**Privilege is raised at exactly one site, by path.**
`spawn_program_with_cwd` ORs in `grant_for(image)` when the spawner holds
`Launch`, and otherwise spawns the image ungranted
(`core/src/exec/mod.rs:322-358`). `PROGRAM_GRANTS` has 25 rows: 13 shipped rows
that raise, 6 delegated e2fsprogs rows and 6 test rows
(`core/src/exec/grants.rs:36-244`). A path key is sound only because the base
directories are sealed and pinned, mount refuses grant paths and `/lib`
(`grants.rs:262-290`), and the `Seal` capability exists to keep it that way
(`authority/mod.rs:97-101`). That makes every grant row a setuid program by
another name. It runs `AT_SECURE`, and it inherits argv, environment, cwd and
descriptors from whoever spawned it. That is why the installer starts
e2fsprogs with an emptied environment (`plans/bare-metal.md:544-548`).

**exec narrows the mask and leaves the flag word.**
`narrow_authority_for_exec` intersects `caps` with the image's grant
(`core/src/exec/mod.rs:584-617`). `task.flags` survives exec, and these checks
read it:

- NET_ADMIN (`core/src/syscall/context.rs:152-174`; not a capability at all);
- PROC_ADMIN (`context.rs:159-165`);
- the raw block-device right (`fs/src/devfs/block.rs:653-683`);
- SYSTEM, for priority, the ext2 reserve and panic fatality;
- signal dominance (`core/src/syscall/signal.rs:89-108`).

So the exec narrowing and the Verus proof (`verification/proofs/authority.rs:9-50`)
cover only half the authority. The proof also models a voluntary `Drop` step
that no syscall implements, and it speaks of "the five" grants the table names.
`plans/README.md` says 177 classified slots where `core/src/syscall/handlers.rs:341`
has 174.

**There is one identity and no DAC.**
- `getuid` and its three siblings return 0 (`core/src/syscall/process_handlers.rs:996-1001`).
- There are no set\*id, group, chown, capget or prctl syscalls. slibc emulates
  root-only answers (`slibc/src/process/ids.rs:1-84`) and compiles in one
  passwd row (`slibc/src/conf.rs:42-48`).
- Mode bits are stored, but only the execute bit is enforced
  (`fs/src/fileio/fdops.rs:712-734`, `fs/src/vfs/ops.rs:115-128`).
- `stat` always reports owner 0 (`fs/src/vfs/traits.rs:139-142`).
- Every process can write `/etc`, `/var`, `/home`, `/src` and `/usr/local`
  (`AGENTS.md`, the remote paragraph). `CVSS.md` scores any code execution
  `PR:L` because "SlopOS has no credential model".

**IPC carries no identity.**
- There is no `SO_PEERCRED`. `SCM_CREDENTIALS` is `EINVAL` (`core/src/syscall/tests.rs:8884-8888`).
- `socketpair` is `ENOSYS` (`slibc/src/net/mod.rs:199-207`).
- `pidfd_open` names only the caller's children.
- `SCM_RIGHTS` carries leaf descriptors (pipes, files, memfds, pidfds, ttys) but
  not sockets or rings (`core/src/syscall/net_handlers.rs:675-695`).

**The model's cost shows up as workarounds:**
- The compositor spawns `/bin/ip` because it must not hold NET_ADMIN, which
  is why it needs `Launch` (`userland/src/apps/compositor/popover.rs:650-675`,
  `grants.rs:42-46`).
- The remote pairing must live in the sealed base because any program can
  write `/etc`.
- The shell must *spawn* registry programs because fork+exec keeps no grant
  (`userland/src/apps/shell/exec.rs:1467-1469`). A granted program in a
  pipeline, a subshell, a bash script, or a `posix_spawn` that falls back to
  fork+exec runs ungranted.
- utest binaries run with SYSTEM (`core/src/exec/utest.rs:107`), so they cannot
  test an unprivileged gate.
- No sshd runs, because "OpenSSH's needs credentials and a privilege separation
  SlopOS lacks" (`plans/bare-metal.md:686-689`).
- A GUI cannot hand install authority to a helper: an app the dock launches
  holds nothing, and a helper it spawns is ungranted unless the app becomes a
  sixth `Launch` holder.

## Why this shape

Every system that has had to let an unprivileged UI get a privileged job done
has converged on the same shape:

1. An unprivileged frontend talks over IPC to a clean privileged service that
   the service manager starts.
2. A policy decides on a *named action*.
3. A trusted UI asks for consent.

Examples:
- Linux: [polkit](https://polkit.pages.freedesktop.org/polkit/polkit.8.html)
  with its mechanisms, and
  [run0](https://www.freedesktop.org/software/systemd/man/latest/run0.html).
  run0 inherits no caller context, because "the service manager forks a
  fresh, isolated service".
- macOS: a privileged helper over XPC, with the SecurityAgent drawing the
  prompt.
- Windows: [Administrator protection](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/administrator-protection/),
  which moved elevation to a separate principal.
- Android: [PackageInstaller](https://developer.android.com/reference/android/content/pm/PackageInstaller),
  whose requester gets `STATUS_PENDING_USER_ACTION` and a system-owned
  confirmation.
- Fuchsia: the paver, the one component allowlisted to hold the block devices.
  The installer and updater use it through
  [`fuchsia.paver`](https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/sdk/fidl/fuchsia.paver/paver.fidl).

Systems that run unmodified POSIX software keep uid/gid as the compatibility
and data-ownership layer and put system-wide power elsewhere. Fuchsia keeps uids
inside Starnix
([RFC-0082](https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/docs/contribute/governance/rfcs/0082_starnix.md)).
Asterinas implements Linux credentials at the ABI and keeps its type-level
rights inside the kernel. Redox keeps uids and elevates through a
[`sudo` scheme](https://gitlab.redox-os.org/redox-os/userutils/-/blob/73aea8b3f858578ad3909e0661e9cf310e3b1d6d/src/bin/sudo.rs)
daemon that checks a kernel-supplied caller uid.

The failures on record each set a rule here:
- **Identity is captured when the peer connects, bound to a handle that cannot
  be reused.** polkit's (pid, start-time) subject was raced
  ([CVE-2013-4288](https://bugzilla.redhat.com/show_bug.cgi?id=CVE-2013-4288)).
- **Every error path fails closed.** polkit treated a caller that vanished
  mid-check as uid 0
  ([CVE-2021-3560](https://github.blog/security/vulnerability-research/privilege-escalation-polkit-root-on-linux-with-bug/)).
- **No privileged program inherits its caller's execution context.** See
  [PwnKit](https://www.qualys.com/2022/01/25/cve-2021-4034/pwnkit.txt) and the
  [`no_new_privs` rationale](https://raw.githubusercontent.com/torvalds/linux/master/Documentation/userspace-api/no_new_privs.rst).
- **No heritable authority reaches an interpreter.**
  [Shrootless](https://www.microsoft.com/en-us/security/blog/2021/10/28/microsoft-finds-new-macos-vulnerability-shrootless-that-could-bypass-system-integrity-protection/)
  shows what happens otherwise.
- **No catch-all bit.** `CAP_SYS_ADMIN` became "the new root"
  ([capabilities(7)](https://man7.org/linux/man-pages/man7/capabilities.7.html)).
- **The compositor is the only credible trusted path.** GNOME Shell implements
  the polkit agent
  ([`Shell.PolkitAuthenticationAgent`](https://gnome.pages.gitlab.gnome.org/gnome-shell/shell/class.PolkitAuthenticationAgent.html)),
  and Sculpt reserves keys and labels in its GUI server
  ([Sculpt 26.04](https://genode.org/documentation/articles/sculpt-26-04)).

## Design

### Principals and DAC

- **Two principals.**
  - uid/gid 0, `root`, home `/root`: init, privd and the mechanisms privd runs.
  - uid/gid 1000, `user`, home `/home/user`: the compositor, the terminal and
    shell, every program the dock starts, remoted and what it runs.
  - slibc's compiled-in passwd and group rows gain `user`. A parsed
    `/etc/passwd` waits for multi-user.
- **Credentials.** Each process carries real, effective and saved uid and gid,
  and up to 32 supplementary groups, in a `Cred` the process owns. fork copies
  it, and exec keeps it.
- **Id syscalls** are added at the Linux numbers: `setuid`, `setgid`, `setreuid`,
  `setregid`, `setresuid`, `setresgid`, `getresuid`, `getresgid`, `setgroups`
  and `getgroups`.
  - They follow POSIX's unprivileged rules: a process may move among its own
    real, effective and saved ids.
  - Moving to any other id, or calling `setgroups`, takes the new `SetId`
    capability.
  - slibc's emulation goes away.
- **No setuid or setgid exec, ever.** The mode bits are stored and inert.
  slibc answers `PR_GET_NO_NEW_PRIVS` with 1 and accepts `PR_SET_NO_NEW_PRIVS`,
  because that is already the system's state.
- **Kernel DAC** in the VFS:
  - search on every directory a walk crosses;
  - read and write on open;
  - write and search on the parent for create, unlink and rename, with the
    sticky bit honoured;
  - ownership for chmod and utimes;
  - `access`/`faccessat2` with real `AT_EACCESS`;
  - the execute bit, as today.

  The check runs with the effective ids. Ring opcodes that resolve paths check
  with the submitting task's `Cred`.
- **DAC override is a capability, not uid 0.** The new `DacOverride` bypasses
  mode bits and permits `chown` to any owner. Without it, `chown` follows Linux's
  unprivileged rule. This keeps every system-wide power off the uid axis, as
  Linux securebits `NOROOT` and Plan 9's host owner do.
- **Ownership on disk.**
  - `stat` reports the stored owner: ext4's `i_uid`/`i_gid`, the creator's ids
    on ramfs, and the archive's 0:0 on basefs.
  - A new file takes the effective uid, and the effective gid unless its
    directory is setgid.
  - The ext4 reserved blocks are uid 0's, as ext4 defines them. That replaces
    the SYSTEM test in `current_task_is_privileged`.
- **Ownership map.** The image builders, the installer, and a one-time
  migration on a preserved root hold the root to this map once the session runs
  as uid 1000 (phase 4):

| Path | Owner | Why |
|---|---|---|
| the base directories | 0:0, sealed | as archived |
| `/`, `/etc`, `/var`, `/var/lib/slopos`, `/var/log` | 0:0, 0755 | system state; `/etc/keymap` is written by `/bin/keymap` through privd |
| `/root` | 0:0, 0700 | |
| `/tmp` | 0:0, 1777 | |
| `/home/user`, `/src` and the clone under it | 1000:1000 | the person's work |
| `/usr/local` and every host tree in it | 1000:1000 | the session installs there without a broker, as Homebrew owns `/opt/homebrew`; `DEFAULT_PATH` already puts it after the base, so nothing there shadows a system tool |

  - `scripts/fs_tree.py` writes host trees with the owner of their destination.
  - `tools/initramfs` packs 0:0.
  - On a preserved root, init applies the map once to `/home`, `/src` and
    `/usr/local`, recording `/var/lib/slopos/owners`.
- **Signals.** A task may signal another when POSIX's uid rule admits it
  (sender real or effective uid equals the target's real or saved uid) *and*
  the target's gated capabilities are a subset of the sender's. Holding
  `ProcSignal`, which nothing in the shipped system is given, waives both. The
  dominance relation today stands in "for the user ids POSIX asks about"
  (`signal.rs:91-92`); it becomes those ids, plus the capability subset that
  keeps a session process from killing a mechanism or the compositor.
- **Enumeration** is universal, as Linux's `/proc` is. `process_list` reports
  every task. An id another uid owns can be read and not signalled, and
  `PROC_ADMIN` retires.

### One authority carrier

- **Capabilities.**
  - New: `NetAdmin`, `RawBlock`, `SetId`, `DacOverride`, `SchedHigh`.
  - `RawBlock` takes block-node I/O, `BLKRRPART`, crash-record access and the
    lifted `DiskBlocks` ceiling.
  - `Mount` keeps only `mount`/`umount2`.
  - `SysInspect` stops being universal; it gates `kconsole(2)` and `prof_ctl`.
  - The universal set shrinks to `ConsoleIo`, `ClipboardGlobal` and `Fate`,
    each with its stated deletion condition.
  - The warn-once set widens from u16 (`authority/mod.rs:378`).
- **What leaves the flag word.** Every privileged bit leaves `task.flags`:
  `SYSTEM`, `COMPOSITOR`, `DISPLAY_EXCLUSIVE`, `NET_ADMIN`, `CONSOLE_ADMIN`,
  `PROC_ADMIN`, `POWER`, `LAUNCH`, `MOUNT` and `INSTALL`. `NO_PREEMPT` stays,
  kernel-only.
- **Init by task id.** Init is told apart by its task id, as it already is
  (`core/src/exec/mod.rs:143-148`). Init and the utest runner hold
  `CAP_MASK_ALL` instead of a SYSTEM bit.
- **Classification.** `define_syscall!` loses `requires(net_admin)`. The four
  network-control slots become `cap(NetAdmin)`, and the histogram moves with
  them.
- **exec.** Narrowing keeps its form, `caps' = caps & grant(image)`, until the
  grant table goes. Once nothing else carries authority, it covers everything.
  With no grants left, the same expression drops every gated capability at
  exec, which is Linux's behaviour without ambient capabilities.
- **`caps_drop(mask)`.** A private, total, irreversible syscall lets a
  mechanism shed what it no longer needs, for example the installer after it
  registers its firmware entry. It gives the proof's `Drop` step a real
  counterpart.

### Delegation at spawn

`SpawnAttrs` (`abi/src/spawn.rs:67`) gains the child's requested capabilities,
uid, gid and groups:

- **Capabilities.** The requested set must be a subset of the spawner's
  capabilities, or the spawn fails with `EPERM`.
- **Ids.** Ids other than the spawner's own take `SetId`.
- **Priority.** The `High` tier takes `SchedHigh`.
- **`AT_SECURE`.** The child is `AT_SECURE` when it holds a gated capability or
  its ids differ from the spawner's real ids.
- **fork+exec.** The fork+exec road cannot delegate, because exec drops. The
  spawn primitive is how authority moves.

This bound is the intersection that `verification/proofs/authority.rs` keeps as
a broken witness: "`parent.caps & grant` would mean `/bin/roulette` could never
draw". That objection held only while conferral came from launchers that did
not hold the authority they conferred. Here authority flows from holders. Init
holds everything and hands the compositor the seats, roulette the display seat,
privd the delegable set, and `bootctl collect` its `RawBlock` and `Power`.

**Init's bootstrap table** replaces the registry flags. It lives in
`init_process.rs`, alongside the readiness gate it already owns:

| Child | uid | Capabilities and descriptors |
|---|---|---|
| `/sbin/privd` | 0 | the delegable set: `Power`, `Mount`, `RawBlock`, `NetAdmin`, `ConsoleConfig`, `SysInspect`, `Clock`, `Seal`, `BootEntry`, `DacOverride`; its end of the consent channel. Not `SetId`: every action runs as uid 0, which privd already is |
| `/bin/bootctl collect` | 0 | `RawBlock`, `Power` |
| `/bin/remoted` | 1000 | none; a privd connection it hands its children |
| `/bin/roulette` | 1000 | `DisplaySeat` |
| `/bin/compositor` | 1000 | `DisplaySeat`, `InputSeat`, `High` tier; its end of the consent channel |
| `/bin/terminal` | 1000 | none |

**utests declare what they run with.** `utest!(…, caps = …, uid = …)` sets
what the runner gives the test. The default stays every capability as uid 0,
which is what SYSTEM gives today, so the suite keeps its behaviour. A test of an
unprivileged gate declares `caps = none, uid = 1000`, and so does a test that
acts as the person (git in `/src`, whose ownership check refuses a uid-0
caller). The six test rows in `grants.rs` become declarations.

**A root test drives the person's loop through its own connection.** A uid-0
test that runs the person's loop at uid 1000 (`selfhost_test`, `install_test`,
`installer_test`) opens a privd connection first and hands it to its uid-1000
children, as remoted does. Their requests are then attested as the test's,
which is `root`, so they need no consent and no agent.

### Peer identity

- **`socketpair(AF_UNIX, SOCK_STREAM)`** at the Linux number.
- **`SO_PEERCRED`** returns the peer's pid, uid and gid as of `connect`,
  `listen` or `socketpair`, per
  [unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html).
- **`SO_PEERPIDFD`** returns a pidfd of the peer, captured at the same moment.
  pidfds stop being limited to the caller's children; a peer pidfd polls
  readable on exit.
- **Program identity** is recorded on the task by spawn and exec.
  - `Base(path)` when the image inode is served by the boot slot's pinned
    basefs at a `BASE_DIRS` path.
  - `Other` otherwise.
  - A script is `Base` only when both the script and its interpreter are.
  - A private `getsockopt` level returns the peer's program identity. Nothing a
    client sends can set it.
- **Inherited connections speak as their original owner.** A descriptor
  inherited through spawn keeps the identity of whoever connected it. That is
  how remoted's children ask privd as remoted.

### privd and the action policy

**`/sbin/privd`** listens on `/run/privd`, mode 0666: anyone may ask, and the
policy answers.

**Policy file.** The policy lives in the sealed base at
`/usr/share/slopos/privd/actions`, so no running program can edit it. Its
grammar and matcher are `priv-core`, a host-tested crate:

```
action org.slopos.boot.install
  program  /bin/bootctl
  args     install *
  caps     RawBlock Power Mount
  uid      0
  allow    session consent
  allow    paired
  describe "Install a kernel and base into the spare boot slot"
```

- An action names one base program, the argv shapes it accepts (fixed leading
  words, then free), the capabilities and uid it runs with, and who may ask.
- `allow` principals:
  - `root`: any uid-0 requester, always without consent. A uid-0 process is
    already a system process, because nothing but init and privd makes one.
  - `session`: uid 1000, any program.
  - `program <base path>`: a requester with that attested identity.
  - `paired`: a connection whose attested program is `Base(/bin/remoted)`, on a
    base that carries the pairing.
- **Consent.** Each `allow` says `consent` or nothing.
- **`exclusive`.** An action may declare `exclusive`, so only one instance
  runs.

**Requests.** A request carries:
- the action;
- argv;
- the caller's stdin, stdout and stderr over `SCM_RIGHTS`;
- a working directory as a dirfd, when the action allows one.

The wire is framed the way `remote-core` frames its messages.

**Running a mechanism.** privd matches the request against the policy, checks
the peer (`SO_PEERCRED`, `SO_PEERPIDFD`, program identity), and asks for
consent when the matching `allow` says so. It then spawns the mechanism as a
fresh child, with:
- the action's capabilities and uid;
- an environment holding only a fixed `PATH`, `LANG` and
  `PRIV_ACTION=<name>`;
- cwd `/`, or the passed dirfd;
- the caller's descriptors as stdio.

It answers `Started{pid}` and then `Exited{status}`, or `Denied{reason}`.

**Failing closed.** Every failure is a denial: a vanished peer, an identity
lookup that fails, a policy miss, a consent agent that is absent or times out.
A client that disconnects does not stop a running mechanism.

**Mechanisms elevate themselves.** `slopos_userland::privd::elevate` does it:
a mechanism started without `PRIV_ACTION` names the action its parsed argv
belongs to, sends its own argv and stdio, and exits with the mechanism's
status. With `PRIV_ACTION` set, it holds its argv to that action before
acting. This is pkexec's lesson: the privileged side validates.

**Today's rows map onto actions:**

| Today | Becomes |
|---|---|
| `/bin/compositor` `COMPOSITOR\|LAUNCH`, High | bootstrap child above; the network toggle asks for `org.slopos.net.toggle` (`allow program /bin/compositor`) |
| `/bin/shell`, `/bin/terminal` `LAUNCH` | nothing |
| `/bin/remoted` `LAUNCH` | bootstrap child above |
| `/bin/roulette` `DISPLAY_EXCLUSIVE` | bootstrap child above |
| `/bin/keymap` `CONSOLE_ADMIN` | `org.slopos.console.keymap`: `ConsoleConfig`, uid 0, session and paired |
| `/bin/ip` `NET_ADMIN` | `org.slopos.net.configure`: `NetAdmin`, session with consent |
| `/bin/sysmon` `PROC_ADMIN` | nothing: enumeration is universal, kill follows the signal rule |
| `/bin/kconsole` `PROC_ADMIN` | `org.slopos.kconsole`: `SysInspect`, session and paired |
| `/bin/halt` `POWER` | `org.slopos.power.poweroff`, `.reboot`: `Power`, session and paired |
| `/bin/bootctl` `MOUNT\|POWER` | `org.slopos.boot.install`, `.set-default` (session with consent); `.oneshot`, `.commit`, `.spare`, `.status` (session); all paired; `collect` is init's |
| `/bin/cpufreq` `POWER` | `org.slopos.cpufreq.set`: `Power`, session and paired; reading needs nothing |
| `/bin/installer` `MOUNT\|POWER\|INSTALL` | `org.slopos.install`: `RawBlock Mount Power BootEntry Seal DacOverride`, uid 0, session with consent, never paired |
| six e2fsprogs rows, `delegated: MOUNT` | the installer spawns them with `RawBlock` delegated |
| six test rows | `utest!` declarations |

### Consent

- **The channel.** Init creates a socketpair and gives one end to privd and the
  other to the compositor. Holding that pair is what makes an answer the
  compositor's and a question privd's. No other process can open it.
- **The request.** It carries the action's `describe` text from the sealed
  policy, the requester's attested program identity, its uid and its pid. It
  carries no text the requester chose, apart from the argv privd matched.
- **The dialog.**
  - The compositor dims every surface, draws the dialog above all of them, and
    routes input only to the dialog until it is answered.
  - Focus starts on Deny. Allow takes a click, or a Tab or arrow followed by
    Enter, the same no-stray-Enter rule as appkit's `Dialog`.
  - No client can draw over it, move it or feed it input. The compositor owns
    both seats, so the answer comes from the keyboard or pointer at the machine.
- **Expiry.** A request expires after 120 seconds as a denial. Requests are
  answered one at a time.
- **No agent.** With no compositor (a `tests=on` boot, a serial-only boot), an
  action that needs consent is denied. `root` and `paired` principals do not
  need consent.

### The remote control

remoted runs as uid 1000 with no capabilities. At start it opens a privd
connection and hands it to every program it runs (`PRIVD_FD`), so their
requests are attested as remoted's.

`paired` admits the actions `just remote` and `remote-install` drive:
- `org.slopos.boot.*`;
- `org.slopos.power.*`;
- `org.slopos.cpufreq.set`;
- `org.slopos.kconsole`;
- `org.slopos.console.keymap`.

The installer and network reconfiguration are not on the list, so a paired host
holds the boot slots and the power button but not the disk layout or the
network. Everything else remoted runs, runs as the session user.

## Phases

Six phases. Each ends with something that runs, and each lands as commits of
its own.

### Phase 1: One authority carrier

Move every privilege off `task.flags`:
- `NetAdmin` and `RawBlock` become capabilities, and the network slots take
  `cap(NetAdmin)`.
- `SysInspect` stops being universal and gates `kconsole(2)` and `prof_ctl`;
  the PROC_ADMIN flag check folds into it.
- Signal dominance moves to capability masks.
- Init and the utest runner hold `CAP_MASK_ALL`, and SYSTEM's other uses become
  init identity.
- `grants.rs` rows name capabilities instead of flag bits.

The exec narrowing then covers all authority. Fix the proof's drift (five
grants, the modelled-but-absent `Drop`) and `plans/README.md`'s slot count.

Ends with: every authority check reads `caps`. A test execs an unprivileged
image from each granted program and finds NET_ADMIN, raw block access and the
signal relation gone.

### Phase 2: Credentials and DAC

Add `Cred`, the id syscalls, kernel DAC, `DacOverride` and `SetId`, owner
reporting in `stat`, and the passwd and group rows. Every file stays 0:0 and
the session still runs as uid 0 in this phase, so the owner bits admit
everything they admitted before, and only a process that has changed its ids
meets the new checks.

Ends with:
- `just test` green;
- a test that switches to uid 1000 and cannot write `/etc`, read a 0600 root
  file, or unlink another owner's file from a sticky `/tmp`;
- the toolchain ladder green.

### Phase 3: Peer identity

Add `socketpair`, `SO_PEERCRED`, `SO_PEERPIDFD`, program identity on the task,
and its `getsockopt`.

Ends with tests that:
- a connected pair reports each other's pid, uid and identity;
- an identity survives a descriptor inherited through spawn;
- a pidfd from `SO_PEERPIDFD` polls readable when the peer exits;
- a program outside the base reports `Other`.

### Phase 4: Delegation at spawn

`SpawnAttrs` carries capabilities and ids under the subset bound, beside the
`Launch` raise, which still runs for the rows that remain. Init's bootstrap
table starts the compositor, remoted, roulette and the terminal by delegation,
as uid 1000. Until phase 6 the compositor, the terminal and remoted are also
delegated `Launch`, so the grant rows that remain keep working from the
session. A row whose program writes root-owned state (`keymap`'s `/etc/keymap`,
the installer's new root) also names `DacOverride` until privd runs it as
uid 0. The ownership map lands with the uid change: the image builders,
`fs_tree.py`, the installer, and init's one-time migration on a preserved
root. `utest!` declarations replace the test rows.

Ends with:
- a test that a spawn asking beyond its spawner's mask is `EPERM`, and one that
  a uid change without `SetId` is `EPERM`;
- the unprivileged-gate tests running at uid 1000;
- `check-fs-image` holding the tests image to the map;
- `just boot` reaching the desktop with the session at uid 1000 and a
  preserved root migrated, and the toolchain ladder green as uid 1000.

### Phase 5: privd and consent

Build `priv-core` (policy grammar and matcher, request codec, consent request),
`/sbin/privd`, `slopos_userland::privd::elevate`, the consent channel and the
compositor's dialog, then move every mechanism onto its action:
- `halt`, `bootctl`, `cpufreq`, `ip`, `keymap`, `kconsole` and `installer`
  elevate themselves;
- the compositor's network toggle asks privd;
- remoted hands out its connection;
- `installer` spawns e2fsprogs with `RawBlock` delegated.

The tests base carries no extra policy. Tests run as uid 0, and `root` needs no
consent. A test that drives the person's loop hands its children its own
connection.

Ends with:
- `just test`, `test-installer`, `test-install`, `test-remote` and
  `test-selfhost` green with every mechanism reached through privd;
- a uid-1000 test whose consent-requiring request is denied with no agent;
- a test that a request whose argv falls outside the action is denied;
- a manual boot where `bootctl install` typed in the terminal raises the
  compositor's dialog, Deny refuses it and Allow runs it.

### Phase 6: Delete the raise

Remove the following, and nothing grants at exec any more:
- `Launch`;
- `PROGRAM_GRANTS` and `grant_for`/`delegated_for`;
- `covers_grant_path`, keeping only the pinned-mount refusal;
- the registry's flags and priorities;
- the shell's spawn-for-grant path.

After that:
- exec drops every gated capability;
- `Seal`'s deletion condition is restated: the seal holds the base's integrity,
  and that is what privd's `Base(path)` identity rests on;
- the Verus proof is rewritten: S2 exec drops, S3 delegation is bounded by the
  spawner, S4 drop is total for the real `caps_drop`, each with a broken
  witness, and the old "intersection starves" witness is deleted with the
  reason;
- `AGENTS.md`'s authority paragraphs, `CVSS.md`'s `PR` reasoning, and every
  ledger entry about grants are re-examined.

Ends with:
- `grants.rs` gone;
- `check_authority_reachability.sh` and the proof green;
- `rg Launch` matching nothing in the kernel;
- the full pre-commit sequence green.

## Grading

- **Kernel tests:**
  - DAC on open, create, unlink, rename (sticky), chmod and chown;
  - the POSIX id rules and `SetId`;
  - the signal rule;
  - `socketpair`, `SO_PEERCRED`, `SO_PEERPIDFD` and program identity;
  - the delegation bound and the exec drop;
  - `caps_drop` totality.

  Each rejection has a positive control beside it.
- **Host tests:** `priv-core` under `just test-host`, with mutation loops over
  the policy grammar and the request codec as `remote-core` has.
- **Userland tests:**
  - privd's allow and deny paths per principal;
  - argv outside an action;
  - a vanished requester (denied);
  - absent consent (denied);
  - an inherited remoted connection.
- **Integration:** the integration suites above run unchanged in what they
  grade. They reach every mechanism through privd.
- **Gates:** the histogram, `check_authority_reachability.sh`, the Verus proof
  and `check_unsafe_outside_ostd.sh`. Re-measure ratchets with
  `--emit-allowlist` where new locks (`Cred`, privd's channel) move them.
- **By hand:** the consent dialog is graded on `just boot`. Spoofing it would
  take a client drawing over the compositor's top layer, which the protocol has
  no request for.

## Out of scope

- A login screen, passwords, a parsed `/etc/passwd`, and more than one human
  principal.
- setuid/setgid exec, and `capget`/`capset` as a Linux compatibility view.
- Per-process mount namespaces and a Capsicum- or Landlock-style self-restriction
  call. These are the next steps once descriptors carry authority.
- Descriptor forms of `Power`, `Clock` and `BootEntry` (their deletion
  conditions). privd makes them unnecessary for now.
- An sshd on SlopOS. This plan makes it possible; it is work of its own.
- A consent agent on the serial console.
- `SCM_CREDENTIALS`.

## Decided

- **Hybrid.** Unix identity for compatibility and data ownership, capabilities
  for system authority, and a broker for elevation. A pure Unix model would
  bring back setuid and a catch-all bit. A pure object-capability model would
  leave `getuid()` at 0 forever and need namespaces for every POSIX program.
- **Two principals**, `root` (0) and `user` (1000). Multi-user can come later
  without changing the design.
- **uid 0 owns files and holds no power.** DAC override, id changes, mount, raw
  block access, power, the network and boot entries are capabilities.
- **No setuid, ever.** Every process is `no_new_privs`, and elevation is a
  fresh child of privd.
- **Authority flows from holders.** A spawn may delegate a subset of the
  spawner's mask. exec keeps no gated capability once the table is gone.
- **One broker.** privd, with an action policy in the sealed base, runs
  unchanged mechanism programs. Per-domain daemons would multiply processes and
  protocols for no gain in a single-seat system.
- **Program identity is the sealed base path, not a content hash.** The base
  cannot change under a running boot, so a hash adds nothing, and a hash would
  churn with every build the guest makes of itself.
- **Consent is presence.** The compositor draws it over an init-made channel.
  It defaults to Deny, expires in 120 s, and fails closed. A password waits for
  multi-user.
- **A pairing is consent for a fixed allowlist.** That covers the boot slots,
  power, cpufreq, kconsole and the keymap, and leaves out the installer and the
  network.
- **uid-0 requesters need no consent.** Only init and privd create uid-0
  processes, so a uid-0 requester is already the system, and the tests run as
  uid 0.
- **Enumeration is universal, as on Linux.** Acting on a process follows the
  signal rule.
- **`/usr/local` is the session's,** because that is where the guest installs
  for itself. System integrity rests on the sealed base.
- **Strictly before the GUI installer.** `plans/installer-gui.md` starts after
  phase 6.

## Constraints

- `slopos-ostd` remains the only kernel crate with `unsafe`. Every new kernel
  crate is `forbid(unsafe_code)`.
- A Linux syscall number carries Linux semantics or `ENOSYS`
  (`abi/src/syscall/numbers.rs:1-18`). SlopOS-only calls and sockopt levels use
  the private ranges.
- Concepts only from polkit, systemd and Linux. polkit and systemd are LGPL and
  Linux is GPL-2.0-only. Cite the specification or documented behaviour, never
  their files.
- slibc stays `MIT OR Apache-2.0`, and its id calls and passwd rows stay its
  own code.
- A ratchet that moves is re-measured with `--emit-allowlist`, and the commit
  says which lock or test moved it.
- Every phase updates `AGENTS.md` where it changes a stated invariant, and runs
  the security sweep the ledger policy asks for over the syscall paths it
  touched.
