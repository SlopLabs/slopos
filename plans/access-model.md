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

**One mask carries authority.** `task.flags` (`abi/src/task.rs:200-251`) says
where and how a task runs. Every privilege is a bit of the per-task capability
mask (`slopos-ostd/src/authority/mod.rs`, bits in `abi/src/caps.rs`), and every
check reads it: the network-control slots (`NetAdmin`), block nodes, the crash
records and the `DiskBlocks` ceiling (`RawBlock`), `kconsole(2)` and `prof_ctl`
(`SysInspect`), mode bits (`DacOverride`), id changes (`SetId`), and the signal
relation, which lets a sender name only a target whose capabilities it holds
(`core/src/syscall/signal.rs:132-143`). Init and every utest binary hold
`CAP_MASK_ALL` (`core/src/exec/mod.rs:162-177`, `core/src/exec/utest.rs:107`).
A spawned program holds the universal capabilities (`ConsoleIo`,
`ClipboardGlobal`, `Fate`) its spawner holds, and what its path earns. A nice
value below zero is init's, by task id.

**Privilege is raised at exactly one site, by path.**
`spawn_program_with_cwd` ORs in `grant_for(image)` when the spawner holds
`Launch`, and otherwise spawns the image ungranted
(`core/src/exec/mod.rs:357-377`). `PROGRAM_GRANTS` has 25 rows: 13 shipped rows
that raise, 6 delegated e2fsprogs rows and 6 test rows
(`core/src/exec/grants.rs:39-237`). A path key is sound only because the base
directories are sealed and pinned, mount refuses grant paths and `/lib`
(`grants.rs:274-288`), and the `Seal` capability exists to keep it that way
(`authority/mod.rs:99-103`). That makes every grant row a setuid program by
another name. It runs `AT_SECURE`, and it inherits argv, environment, cwd and
descriptors from whoever spawned it. That is why the installer starts
e2fsprogs with an emptied environment (`userland/src/apps/installer/disk.rs:473`).

**exec narrows all of it.** `narrow_authority_for_exec` keeps
`caps & (universal | grant | delegated)` in every thread of the process
(`core/src/exec/mod.rs:627-654`), and `caps_drop` sheds capabilities from a
whole process for good
(`sched/src/task/task_lifecycle.rs:2647-2667`). The Verus proof
(`verification/proofs/authority.rs`) models both.

**Every process has an identity, and it is uid 0.**
- Each process carries a `Cred` (`slopos-ostd/src/process/cred.rs`). The id
  syscalls follow POSIX's rules and take `SetId` for any other id
  (`core/src/syscall/cred_handlers.rs`); slibc calls them and compiles in the
  `root` and `user` rows (`slibc/src/conf.rs`).
- The VFS checks owner, group and mode against the `Subject` every entry takes
  (`fs/src/vfs/perm.rs`), and `stat` reports the stored owner.
- Every process still runs as uid 0 and every file is 0:0, so every process
  can write `/etc`, `/var`, `/home`, `/src` and `/usr/local` (`AGENTS.md`, the
  remote paragraph). `CVSS.md` scores any code execution `PR:L` for that
  reason.

**IPC carries no identity.**
- There is no `SO_PEERCRED`. `SCM_CREDENTIALS` is `EINVAL` (`core/src/syscall/tests.rs:9205-9214`).
- `socketpair` is `ENOSYS` (`slibc/src/net/mod.rs:199-207`).
- `pidfd_open` names only the caller's children.
- `SCM_RIGHTS` carries leaf descriptors (pipes, files, memfds, pidfds, ttys) but
  not sockets or rings (`core/src/syscall/net_handlers.rs:675-695`).

**The model's cost shows up as workarounds:**
- The compositor spawns `/bin/ip` because it must not hold `NetAdmin`, which
  is why it needs `Launch` (`userland/src/apps/compositor/popover.rs:650-675`,
  `grants.rs:40-50`).
- The remote pairing must live in the sealed base because any program can
  write `/etc`.
- The shell must *spawn* registry programs because fork+exec keeps no grant
  (`userland/src/apps/shell/exec.rs:1467-1469`). A granted program in a
  pipeline, a subshell, a bash script, or a `posix_spawn` that falls back to
  fork+exec runs ungranted.
- utest binaries hold every capability (`core/src/exec/utest.rs:107`), so they
  test an unprivileged gate only through an ungranted child or `caps_drop`.
- No sshd runs: OpenSSH's needs credentials and a privilege separation SlopOS
  lacks, so the remote control dials out to a broker instead.
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
  it, and spawn and exec keep it with the effective ids saved, as POSIX has
  it.
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
    init's claim to them in `current_task_is_system`.
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
  `ProcSignal`, which nothing in the shipped system is given, waives both, and
  `SIGCONT` to a task of the sender's own session needs neither, as Linux's
  `kill` allows, so the shell's `fg` resumes a granted job
  (`core/src/syscall/signal.rs:132-143`).
- **Enumeration** is universal, as Linux's `/proc` is. `process_list` reports
  every task to every caller, so `SysInspect` no longer widens it. An id another
  uid owns can be read and not signalled.

### One authority carrier

Built by phase 1. Later phases add three capabilities under the same rules:
`SetId` and `DacOverride` (phase 2), and `SchedHigh` for the `High` tier
(phase 4). Exec's narrowing keeps its form, `caps & (universal | grant |
delegated)`, until the grant table goes; with no grants left, the same
expression drops every gated capability at exec, which is Linux's behaviour
without ambient capabilities.

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
which is what the runner gives today, so the suite keeps its behaviour. A test
of an unprivileged gate declares `caps = none, uid = 1000`, and so does a test
that acts as the person (git in `/src`, whose ownership check refuses a uid-0
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
| `/bin/compositor` `DisplaySeat InputSeat Launch`, High | bootstrap child above; the network toggle asks for `org.slopos.net.toggle` (`allow program /bin/compositor`) |
| `/bin/shell`, `/bin/terminal` `Launch` | nothing |
| `/bin/remoted` `Launch` | bootstrap child above |
| `/bin/roulette` `DisplaySeat` | bootstrap child above |
| `/bin/keymap` `ConsoleConfig` | `org.slopos.console.keymap`: `ConsoleConfig`, uid 0, session and paired |
| `/bin/ip` `NetAdmin` | `org.slopos.net.configure`: `NetAdmin`, session with consent |
| `/bin/sysmon` `SysInspect` | gone in phase 2: enumeration is universal, kill follows the signal rule |
| `/bin/kconsole` `SysInspect` | `org.slopos.kconsole`: `SysInspect`, session and paired |
| `/bin/prof` `SysInspect` | `org.slopos.prof`: `SysInspect`, session and paired |
| `/bin/halt` `Power` | `org.slopos.power.poweroff`, `.reboot`: `Power`, session and paired |
| `/bin/bootctl` `RawBlock Power` | `org.slopos.boot.install`, `.set-default` (session with consent); `.oneshot`, `.commit`, `.spare`, `.status` (session); all paired; `collect` is init's |
| `/bin/cpufreq` `Power` | `org.slopos.cpufreq.set`: `Power`, session and paired; reading needs nothing |
| `/bin/installer` `RawBlock Mount Power BootEntry Seal` | `org.slopos.install`: `RawBlock Mount Power BootEntry Seal DacOverride`, uid 0, session with consent, never paired |
| six e2fsprogs rows, delegated `RawBlock` | the installer spawns them with `RawBlock` delegated |
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
- `org.slopos.kconsole` and `org.slopos.prof`;
- `org.slopos.console.keymap`.

The installer and network reconfiguration are not on the list, so a paired host
holds the boot slots and the power button but not the disk layout or the
network. Everything else remoted runs, runs as the session user.

## Phases

Six phases. Each ends with something that runs, and each lands as commits of
its own.

### Phase 1: One authority carrier — done

Every privilege moved off `task.flags` onto the per-task mask, whose bits are
ABI (`abi/src/caps.rs`). `NetAdmin` gates the four network-control slots;
`RawBlock` gates block nodes, `BLKRRPART` and the crash records and lifts the
`DiskBlocks` ceiling, leaving `Mount` only `mount`/`umount2`; `SysInspect` left
the universal set and gates `kconsole(2)`, `prof_ctl` and an unrestricted
`process_list`, and `/bin/prof` became its own program to hold it. Signals, the
OOM killer's shield and `process_list` compare masks, and `vhangup`, which
would hang up a whole session past that relation, takes `ConsoleConfig`, as
Linux's takes `CAP_SYS_TTY_CONFIG`. Init and utests hold `CAP_MASK_ALL`; grant
rows name capabilities; exec narrows every thread of the process. `caps_drop`
gave the proof's `Drop` a syscall, and the proof, `STATUS.md` and
`plans/README.md` match the tree. Graded by
`test_exec_of_an_ungranted_image_drops_every_grant` (every
grant row through exec's narrowing), `spawn_privilege_test`'s
`exec_narrows_to_the_images_grant` (a real `execve` that loses the network,
raw block and signal gates), `test_caps_drop_is_total_and_process_wide`,
`caps_drop_is_process_wide` and `test_dispatch_gates_follow_the_capability_mask`.

### Phase 2: Credentials and DAC — done

Each process carries a `Cred`, process-wide, that fork and spawn copy and exec
keeps with its effective ids saved. The id syscalls and the chown family sit at
the Linux numbers; any other id, and `setgroups`, takes `SetId`. Every VFS
entry takes the `Subject` it acts for and checks owner, group and mode as Linux
does: search on the walk, read and write on open, write and search on a parent
with the sticky bit, write on a directory moved between parents, ownership for
`chmod`, `chown`, times and inode flags, set-ID stripping and setgid
directories. `DacOverride` passes every mode check, and ring opcodes check as
their submitter. `stat` reports the stored owner, a new inode is born with its
owner and final mode, and the ext4 reserve is the superblock's uid and group.
Signals add POSIX's uid rule, waived by `ProcSignal` and a same-session
`SIGCONT`, and `process_list` lists every task, so `/bin/sysmon` lost its
`SysInspect` row. slibc calls the syscalls and gained the `user` rows. uid 0
without `DacOverride` meets a mode that denies the owner, which exposed
userland creating files 0000; such a file on a preserved root is its owner's to
`chmod`. Graded by `dac_test`, the `cred` kernel tests, `cred.rs`'s host tests
and `persist_test`'s reserve against a uid-1000 filler.

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
root. `utest!` declarations replace the test rows. Three gaps close before
the session leaves uid 0, each harmless while every process is uid 0:
- a pty's slave takes the opener's uid, mode 0620, as Linux's devpts gives
  it, since devfs reports every character device 0:0 0666;
- an `AF_UNIX` name is bound and connected through the filesystem's search
  and write checks, which today's global name table never consults;
- a sticky, world-writable directory follows a symlink only for the link's
  owner or the directory's, as Linux's `protected_symlinks` does, so a
  root-run program cannot be steered through a link the session planted in
  `/tmp`.

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
- **`task.flags` never carries authority.** It says where and how a task runs.
  The retired privilege bits are reserved, so a spawn naming one is `EINVAL`,
  and authority reaches a child only as a capability mask.
- **Capability bits are ABI.** `abi/src/caps.rs` states them once, and
  `slopos_ostd::authority::Capability` is held to exactly those bits, because a
  mask crosses the syscall boundary (`caps_drop` now, `SpawnAttrs` in phase 4).
- **A drop is process-wide, total and irreversible.** `caps_drop` takes the
  bits from every task sharing the caller's process and records them on the
  process, so a thread whose clone races the drop loses them as it is
  registered; it has no error. A fork or spawn racing the drop is ordered
  before it.
  Threads share an address space, so a per-thread drop protects nothing: Linux
  keeps per-thread sets for history and its newer interfaces (seccomp's and
  Landlock's `TSYNC`, libcap's psx) all go process-wide, and Redox holds
  credentials per process.
- **A sender holds every capability its target holds,** beside POSIX's uid
  rule. That goes beyond Linux, whose `kill` compares no capability sets
  (ptrace and `cap_safe_nice` do), so a session process cannot stop a
  mechanism. `SIGCONT` within the sender's session waives both, so the shell's
  `fg` resumes a job holding more than the shell.
- **Kernel threads hold no capability.** A gate a kernel thread must pass —
  raw block devices, the ext4 reserve — names the kernel thread itself.
- **What only the system does is init's, by task id,** where no capability
  names it: a nice value below zero, until phase 4's `SchedHigh`.
- **A program that holds authority is its own binary.** A grant, and later
  privd's `Base(path)`, keys on the resolved path, so a multicall symlink
  would hand every tool its authority; `prof` left coreutils for this.
- **Reading machine-wide state needs nothing.** `sys_info`, `cpu_info`,
  `percpu_stats`, `cpu_perf`, `net_query`, `net_monitor` and `process_list`
  are `NoneSelf`, as `sysinfo` and Linux's `/proc` are; `SysInspect` is for
  the diagnostic console and steering the profiler.
- **Credentials are process-wide,** as POSIX has them and Redox holds them. A
  `SeqLock` on the process holds them and `change_cred` is the only writer,
  a read-modify-write under its lock, so no thread sees a torn or stale
  identity and no setxid broadcast is needed. The filesystem ids are the
  effective ids; there is no `setfsuid`.
- **Changing ids changes no capability.** Leaving uid 0 drops nothing and
  reaching it grants nothing, as with Linux's `SECBIT_NO_SETUID_FIXUP` and
  `SECBIT_NOROOT`.
- **The VFS is told who asks.** Every entry takes a `Subject` — the effective
  ids and whether `DacOverride` is held — and nothing below the syscall layer
  reads the current task for a mode check; the ext4 reserve, which every
  allocation reaches, reads the running task's ids instead. The kernel's own
  callers and kernel threads pass `Subject::KERNEL`; a ring opcode passes its
  submitter's. privd's mechanisms and a peer's attested identity need no VFS
  change.
- **`DacOverride` is Linux's `CAP_DAC_OVERRIDE`, `CAP_FOWNER` and `CAP_CHOWN`
  in one bit.** It passes read and write, execute where some class may execute
  or on a directory, every ownership test, and any `chown`. One broker grants
  it, so three bits would only be three ways to name the same holder.
- **Set-ID bits are stored and never honoured.** They are stripped as Linux
  strips them — by `chown`, by a `chmod` outside the file's group, and from a
  group-executable file created outside it — so a file copied to a system that
  honours them carries none it should not.
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
  power, cpufreq, kconsole, the profiler and the keymap, and leaves out the
  installer and the network.
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
