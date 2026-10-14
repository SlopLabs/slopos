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
(`SysInspect`), mode bits (`DacOverride`), id changes (`SetId`), the `High`
tier and a negative nice (`SchedHigh`), and the signal relation, which lets a
sender name only a target whose capabilities it holds
(`core/src/syscall/signal.rs`). Init holds `CAP_MASK_ALL`; a utest holds what
its `utest!` declares, every capability as uid 0 unless it says otherwise
(`core/src/exec/utest.rs`).

**Authority is delegated at spawn.** `SpawnAttrs` (`abi/src/spawn.rs`)
carries the child's capabilities, uid, gid and groups. A spawned program holds
the universal capabilities (`ConsoleIo`, `ClipboardGlobal`, `Fate`) its
spawner holds and what the spawner delegates, any subset of its own mask
(`Endowment::check` in `core/src/exec/mod.rs`). Init's bootstrap table
(`userland/src/apps/init_process.rs`) starts privd as uid 0 with the
delegable set, and the compositor, the terminal, remoted and roulette as uid
1000.

**privd runs the mechanisms.** `/sbin/privd` (`userland/src/apps/privd/`)
runs a named action's base program for whoever the sealed policy
(`assets/privd/actions`, grammar and matcher in `priv-core`) allows, asking the
person at the machine through the compositor where it says so. `halt`,
`bootctl`, `cpufreq`, `ip`, `keymap`, `kconsole`, `prof` and the installer ask
it through `slopos_userland::privd::elevate` when started without their
action's capabilities; init's `bootctl collect` and a root test act directly.

**No path earns authority.** A child holds what `Endowment::check` admits
and nothing for the program it loads, and `mount(2)` refuses only what is
pinned, the base and the install medium
(`core/src/syscall/fs/mount_handlers.rs`). Capability bit 1 is
`caps::RETIRED`.

**exec drops every gated capability.** `drop_authority_for_exec` keeps
`caps & universal` in every thread of the process (`core/src/exec/mod.rs`),
and `caps_drop` sheds capabilities from a whole process for good
(`sched/src/task/task_lifecycle.rs`). The Verus proof
(`verification/proofs/authority.rs`) models both and the spawn bound.

**The session is uid 1000.**
- Each process carries a `Cred` (`slopos-ostd/src/process/cred.rs`). The id
  syscalls follow POSIX's rules and take `SetId` for any other id
  (`core/src/syscall/cred_handlers.rs`); slibc calls them and compiles in the
  `root` (home `/root`) and `user` rows (`slibc/src/conf.rs`).
- The VFS checks owner, group and mode against the `Subject` every entry takes
  (`fs/src/vfs/perm.rs`), and `stat` reports the stored owner. A pty's slave
  is its opener's, an `AF_UNIX` name is a socket inode under the same checks,
  and a sticky world-writable directory follows a link only for its owner or
  the directory's.
- Init, privd and the mechanisms privd runs are uid 0; everything else the
  session runs is uid 1000. Every
  root is held to the ownership map (`slopos_abi::fs::ROOT_LAYOUT`), so the
  session writes `/home`, `/src`, `/usr/local` and `/tmp`, and not `/etc` or
  `/var`.

**IPC carries identity.**
- An `AF_UNIX` socket records who connected (`slopos_ostd::process::identity`)
  at `listen`, `connect` and `socketpair`; `SO_PEERCRED`, `SO_PEERPIDFD` and
  `SO_PEERPROGRAM` read it (`core/src/syscall/net_handlers.rs`).
- A process's program is `Base(inode)` or `Other`, set by spawn, exec and fork
  and lowered by code from outside the base.
- `pidfd_open` names any process. `SCM_CREDENTIALS` is still `EINVAL`.
- `SCM_RIGHTS` carries leaf descriptors (pipes, files, memfds, pidfds, ttys) but
  not sockets or rings (`collect_scm_rights` in the same file).

**What remains of the old model's cost:**
- The remote pairing lives in the sealed base, because a mechanism holding
  `DacOverride`, the installer's, can write `/etc`.
- No sshd runs: OpenSSH's needs credentials and a privilege separation SlopOS
  lacks, so the remote control dials out to a broker instead.

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
| `/`, `/etc`, `/var`, `/var/lib`, `/var/lib/slopos`, `/var/log`, `/home`, `/media`, `/usr` | 0:0, 0755 | system state; `/etc/keymap` is written by `/bin/keymap` through privd |
| `/root` | 0:0, 0700 | |
| `/var/tmp`, and `/tmp`, a ramfs | 0:0, 1777 | |
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
(phase 4). Exec keeps `caps & universal`, Linux's behaviour for a process
that is not root and has no file or ambient capabilities.

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

This bound is the intersection `verification/proofs/authority.rs` once kept as
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
caller). The six test rows the grant table carried are declarations.

**A root test drives the person's loop through its own connection.** A uid-0
test that runs the person's loop at uid 1000 (`selfhost_test`, `install_test`,
`installer_test`) opens a privd connection first and hands it to its uid-1000
children, as remoted does. Their requests are then attested as the test's,
which is `root`, so they need no consent and no agent.

### Peer identity

- **`socketpair(AF_UNIX, SOCK_STREAM)`** at the Linux number.
- **`SO_PEERCRED`** returns the peer's pid, effective uid and effective gid as
  of `connect`, `listen` or `socketpair`, per
  [unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html): the server is
  told the connector, the client the listener.
- **`SO_PEERPIDFD`** returns a close-on-exec pidfd of the peer, captured at the
  same moment, which polls readable when the peer's leader thread exits — at
  once for a peer already gone.
  `pidfd_open` names any process's leader.
- **Program identity** is recorded on the process by spawn and exec.
  - `Base(inode)` when the image is served by the boot slot's pinned basefs
    at a `BASE_DIRS` path, and so is every script of its `#!` chain and its
    ELF interpreter. Each interpreter has to be named by an absolute path and
    each script by the absolute path it was found at, since its interpreter
    opens that spelling again, through a working directory another process
    may share.
  - `Other` otherwise, and from the moment the process maps code that the base
    did not supply.
  - `SO_PEERPROGRAM` at the private level `SOL_SLOPOS` returns the peer's
    program identity as the inode's base path. Nothing a client sends can set
    it.
- **Inherited connections speak as their original owner.** A descriptor
  inherited through spawn keeps the identity of whoever connected it. That is
  how remoted's children ask privd as remoted.

### privd and the action policy

**`/sbin/privd`** listens on `/run/privd`, mode 0666: anyone may ask, and the
policy answers. Init starts it as uid 0 holding the delegable set,
`slopos_priv_core::DELEGABLE`, and waits for it to listen before the tests or
any service run.

**Policy file.** The policy lives in the sealed base at
`/usr/share/slopos/privd/actions` (`assets/privd/actions` in the tree), so no
running program can edit it. Its grammar and matcher are `priv-core`, a
host-tested crate:

```
action org.slopos.boot.install
  program   /bin/bootctl
  args      install *
  caps      RawBlock Power
  uid       0
  cwd
  exclusive boot-slots
  allow     root
  allow     session consent
  allow     paired
  describe  "Install a kernel and base into a boot slot"
```

- An action names one base program (under `/bin`, `/sbin` or `/usr/bin`), the
  argv shapes it accepts (`args`, repeatable: fixed leading words, then `*`
  for any further ones), the capabilities it runs with (only from the
  delegable set), its uid, a `describe` line for the dialog, and who may ask.
- `allow` principals, tried in order, the first that matches deciding:
  - `root`: any uid-0 requester, never asked about. A uid-0 process is
    already a system process, because nothing but init and privd makes one.
    Every shipped action lists it.
  - `session`: uid 1000, any program but a paired connection.
  - `program <base path>`: a requester with that attested identity, whatever
    its uid. privd refuses its whole policy if one names a program with an
    ELF interpreter.
  - `paired`: a connection whose attested program is `Base(/bin/remoted)`, on a
    base that carries the pairing. A paired connection matches nothing else,
    so an action that does not list `paired` is refused to it outright.
- **Consent.** Each `allow` but `root` and `paired` says `consent` or nothing.
- **`cwd`.** The request may name its working directory.
- **`exclusive [group]`.** One mechanism of the group, by default the action's
  name, runs at a time: the slot writers share `boot-slots`.

**Requests.** A connection serves its requests one after another. A request
is one frame, framed as `remote-core` frames its messages, carrying an id the
requester picks, the action, argv past its first word and whether it names a
directory, with stdin, stdout, stderr and that directory as `SCM_RIGHTS`.
privd answers `Asking` when the question is on screen, then `Started{pid}` and
`Exited{code | signal}`, or `Denied{reason}`, marked when the person refused
or did not answer; every reply names the request's id, so a requester on an
inherited connection passes over what an earlier request left, and id 0 is a
denial of the connection itself. The requester may relay `SIGHUP`, `SIGINT`,
`SIGQUIT` and `SIGTERM`: to the mechanism once it runs, and before that as a
withdrawal; a second signal stops the requester waiting.

**Running a mechanism.** privd matches the request against the policy, checks
the peer (`SO_PEERCRED`, `SO_PEERPIDFD`, program identity, all captured at
connect), and asks for consent when the matching `allow` says so. It then
spawns the mechanism as a fresh child, with:
- the action's capabilities and uid, and argv `[basename, args…]`;
- an environment holding only `PATH=/bin:/sbin`, `LANG=C.UTF-8` and
  `PRIV_ACTION=<name>`, and the relayed signals at their defaults;
- cwd `/`, or, for an action that says `cwd`, the path of the directory the
  request must send;
- the caller's descriptors as stdio.

**Failing closed.** Every failure is a denial: argv outside every shape, a
missing descriptor or a malformed frame (which also ends the connection), an
identity lookup that fails, a policy miss or an unreadable policy, a consent
agent that is absent, busy or does not answer, and a requester whose process
has exited — checked before the question and again after it — and an
exclusive group already running, checked before the question too. A requester
that leaves or withdraws while the person is asked takes the question off the
screen. A client that disconnects does not stop a running mechanism. One uid
running one program holds at most eight connections; every requester but root
and the paired agent shares 32, so nothing the session runs crowds those two
out; and a requester that leaves a reply unread for five seconds loses its
connection rather than holding a privd thread.

**Mechanisms elevate themselves.** `slopos_userland::privd::elevate` does it:
a mechanism started without `PRIV_ACTION` names the action its parsed argv
belongs to; when it holds that action's capabilities and runs as its uid it
acts itself, and otherwise it sends its own argv and stdio — through the
connection it inherited (`PRIVD_FD`, used only when its peer is uid 0 running
the base's `/sbin/privd`) or a fresh one — and exits with the mechanism's
status, or 126 when the person refused and 127 when privd did, as pkexec's
are. With `PRIV_ACTION` set, it acts only when its argv belongs to that
action. This is pkexec's lesson: the privileged side validates. A terminal
never reaches a mechanism, which runs in privd's session where the terminal
answers `EIO`, as run0 isolates one: `elevate` relays the mechanism's output
to it through pipes, and its input only for the installer, which asks
questions.

**The rows mapped onto actions:**

| Was | Is |
|---|---|
| `/bin/compositor` delegated `DisplaySeat InputSeat Launch`, High | bootstrap child, the seats and `High`; the network toggle hands `ip` a connection the compositor made, `org.slopos.net.toggle` (`allow program /bin/compositor`) |
| `/bin/shell`, `/bin/terminal` `Launch` | gone; the shell forks and execs a mechanism and hands it its connection |
| `/bin/remoted` `Launch`, also delegated | bootstrap child, holding nothing |
| `/bin/roulette` `DisplaySeat`, also delegated | bootstrap child; started from a shell it holds no seat and prints its fate |
| `/bin/keymap` `ConsoleConfig DacOverride` | `org.slopos.console.keymap`: `ConsoleConfig`, session and paired |
| `/bin/ip` `NetAdmin` | `org.slopos.net.configure`: `NetAdmin`, session with consent; `ip net on` and `off` are `org.slopos.net.toggle`, the compositor without consent and the session with |
| `/bin/sysmon` `SysInspect` | gone in phase 2: enumeration is universal, kill follows the signal rule |
| `/bin/kconsole` `SysInspect` | `org.slopos.kconsole`: `SysInspect`, session and paired |
| `/bin/prof` `SysInspect` | `org.slopos.prof`: `SysInspect`, session and paired |
| `/bin/halt` `Power` | `org.slopos.power.poweroff`, `.reboot`: `Power`, session and paired |
| `/bin/bootctl` `RawBlock Power DacOverride` | `org.slopos.boot.install`, `.clone`, `.set-default` (session with consent); `.oneshot`, `.commit`, `.spare`, `.status`, `.reboot` (session); all paired; `collect` is init's, with `RawBlock` and `Power` |
| `/bin/cpufreq` `Power` | `org.slopos.cpufreq.set`: `Power`, session and paired; reading needs nothing |
| `/bin/installer` `RawBlock Mount Power BootEntry Seal DacOverride` | `org.slopos.install`: the same, session with consent, never paired, exclusive |
| six e2fsprogs rows, delegated `RawBlock` | the installer spawns them with `RawBlock` delegated |
| six test rows | `utest!` declarations |

### Consent

- **The channel.** Init creates a socketpair and gives one end to privd and the
  other to the compositor, each at fd 4. Holding that pair is what makes an
  answer the compositor's and a question privd's. No other process can open
  it, and each end refuses a pair whose `SO_PEERCRED` and `SO_PEERPROGRAM` are
  not init's, which a socket pair records for both ends. The compositor says
  `READY` once it can draw; until then privd has nobody to ask. privd's writes
  to it time out rather than wait on a compositor that stopped reading.
- **The request.** It carries the action's `describe` text from the sealed
  policy, the requester's attested program identity, its uid and its pid, and
  the command privd will run and where. It carries no text the requester
  chose, apart from the argv privd matched and its directory, which the dialog
  shows whole on lines of their own, wrapped, with every byte outside
  printable ASCII escaped; privd asks about no command line or directory
  longer than 320 bytes so escaped.
- **The dialog.**
  - The compositor dims every surface, draws the dialog above all of them, and
    routes input only to the dialog until it is answered; releases still reach
    whoever saw the press.
  - Focus starts on Deny. Allow takes a click, or Tab, Left or Right followed
    by Enter, the same no-stray-Enter rule as appkit's `Dialog`; Escape denies,
    key repeats count for nothing, and input in the 750 ms after the frame
    that first shows the dialog, or after focus moves, is ignored, since it
    was aimed at what was there before.
  - No client can draw over it, move it or feed it input. The compositor owns
    both seats, so the answer comes from the keyboard or pointer at the machine.
- **Expiry.** A question expires 120 seconds after it is shown, as a denial.
  Questions are shown one at a time, at most eight waiting.
- **A refusal sticks.** When the person refuses, every question waiting is
  refused too, and so is whatever asks in the next five seconds: a refusal is
  not answered with another question.
- **No agent.** With no compositor (a `tests=on` boot, a serial-only boot), an
  action that needs consent is denied. `root` and `paired` principals do not
  need consent.

### The remote control

remoted runs as uid 1000 with no capabilities. Each mechanism the policy names
that it runs, and the shell, gets a privd connection remoted made for it
(`PRIVD_FD`), so its requests are attested as remoted's; anything else gets
none, since a base program such as `env` or `xargs` would pass it on to
whatever it execs. A mechanism keeps the descriptor its environment names
from what it starts, and the shell hands it on only to the mechanisms its own
process runs while its environment names it, not from a subshell or pipeline
stage and not while one it handed it to lives, so
neither what it execs nor two jobs at once share it. A redirection the shell
undoes restores close-on-exec with the descriptor. A command's timeout sends
its group `SIGTERM` first, which a requester relays to the mechanism privd
runs outside the group.

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

### Phase 3: Peer identity — done

`socketpair` sits at the Linux number. An `AF_UNIX` socket records an
`Identity` — pid, effective ids and program — at `listen`, `connect` (a ring's
`OP_CONNECT` as the task whose `ring_enter` performs it, when it does) and
`socketpair`; `SO_PEERCRED`, `SO_PEERPIDFD` and `SO_PEERPROGRAM`
(`SOL_SLOPOS`) read it, and slibc's `socketpair` calls the kernel. A process's
program is `Base(inode)` when spawn or exec loaded it wholly from `BASE_FS`,
each interpreter and script named by its absolute path. Fork copies it with
the address space; an exec beside another thread, an exec failing past the
reset and an executable mapping from outside the base leave `Other`.
`pidfd_open` takes any leader, every pidfd is close-on-exec, and a peer's
pidfd outlives the peer. `Process` is now built in place. Graded by
`tests_peer` (each option and refusal, the capture points, a stale pidfd, the
mapping rules, the exec rules against a stand-in base, the spelling a script
must be named by), `peer_test` (between real processes: a uid-1000 child
exec'd from the base, a connection handed through spawn, a ring's connect and
one that waits while the ids change, a peer's pidfd on exit, the same program
copied to `/tmp`, foreign code), `test_exec_attests_only_what_the_base_serves`
against the real base, `libc_abi_test` through slibc, and
`test_basefs_names_an_inode_by_its_path`. No base program is a script, so a
base script's attestation is graded only through its spelling.

### Phase 4: Delegation at spawn — done

`SpawnAttrs` carries a capability mask, uid, gid and groups, each id taken
only under its `SPAWN_SET_*` bit; a spawn asking beyond its spawner's mask is
`EPERM`, ids other than its own take `SetId`, and the `High` tier and a
negative nice take the new `SchedHigh`. A delegated or re-identified child
starts `AT_SECURE`. The raise still runs for 12 rows and e2fsprogs' 6
delegated ones; the compositor's row and the test rows are gone, and
`keymap`, `bootctl` and the installer also name `DacOverride`. Init's
bootstrap table starts the compositor (the seats and `Launch`, at `High`), the
terminal and remoted (`Launch`) and roulette (the display seat) as uid and gid
1000 with no groups, `HOME=/home/user` and `XDG_RUNTIME_DIR=/run/user/1000`,
where the compositor's socket moved, and `bootctl collect` as uid 0 with
`RawBlock` and `Power`, its row adding `DacOverride`. `utest!` takes `caps =
…` and `uid = …`. A pty slave is its master opener's, 0620, judged on the
slave an open takes; an `AF_UNIX` name is a socket inode its bound socket holds
open, bound through the parent's write and search checks and connected through
the name's write check, and `/run` is a ramfs; a sticky world-writable
directory follows a link only for its owner or the directory's. The ownership
map is `slopos_abi::fs::ROOT_LAYOUT` and `SESSION_TREES`, mirrored in
`scripts/lib/root_layout.sh` and `tools/initramfs` and held equal by a test.
The image builders and `tools/initramfs` lay it out and record it,
`fs_tree.py` gives a tree its destination's owner, the installer lays out and
records a new root, and init makes what a root lacks at every boot and
migrates a root without the record once (`userland/src/root_layout.rs`). The
tests that run git in `/src` run as uid 1000, and a root test runs git and
`selfhost.sh` there as the session. The ladder's fixtures moved to
`/home/user/ladder`, since ssh reads a key only its user may, and the remote
check pushes and stages under the session's directories.

Graded by `test_spawn_delegates_within_the_spawner`,
`test_a_shared_directory_follows_a_link_for_its_owners`,
`test_a_negative_nice_takes_sched_high` and
`test_pty_open_slave_judges_the_owner_it_opens`; `spawn_privilege_test`
(tiers, the bound, ids, the `SpawnAttrs` fields, `AT_SECURE`) and its
`--session` run at uid 1000; `dac_test` (pty, socket names, links, a migrated
root) and its `--session` run, which holds the map; `ip_e2e_test`'s session
half; the S5 obligation of `verification/proofs/authority.rs`; and
`check_fs_image.sh --map` on the tests, installed and self-hosting roots.
Checked by hand: a boot without tests reaching the compositor, the terminal
and a shell as uid 1000 in `/home/user`, and a tests image made into an old
all-uid-0 root migrated on its first boot and held to the map. Not run: the
ladder with a toolchain, `test-remote` and the install checks.

### Phase 5: privd and consent — done

`priv-core` holds the policy grammar and matcher, the request and consent
codecs, the consent queue (`Questions`) and the dialog's focus and arming
rules, host-tested with mutation loops over each, a random walk over the
queue, and the shipped policy, which the base carries at
`/usr/share/slopos/privd/actions`. `/sbin/privd`, which init starts first and
waits for, runs an action's mechanism after checking the connection's recorded
identity, asking the person through the compositor where the policy says so;
the compositor draws the question over every surface, Deny focused. `halt`,
`bootctl`, `cpufreq set`, `ip`'s changes, `keymap`, `kconsole`, `prof` and the
installer elevate themselves through `slopos_userland::privd::elevate`; the
compositor's network toggle hands `ip` a connection of its own; remoted, the
shell and a root test hand theirs to mechanisms; the installer spawns
e2fsprogs with `RawBlock` delegated. The mechanisms' 8 grant rows and
e2fsprogs' 6 are gone, and init delegates the compositor no `Launch` and
remoted and the terminal nothing.

Graded by `privd_test` (root runs an action holding its capabilities, an exit
status relayed, argv outside an action and an unknown action refused for that
reason, a request without its descriptors or with one too many refused and
its connection ended, the working directory taken from the descriptor sent and
a non-directory or an unwanted one refused, what an earlier request left
passed over, the per-requester connection cap,
the session running what it may, refused what needs a person with nobody to
ask and running it through a root connection it was handed, a uid no
principal names refused, a departed requester refused beside its control, a
mechanism holding its argv to `PRIV_ACTION`, and output for the requester's
controlling terminal relayed); `spawn_privilege_test`'s e2fsprogs case, now
delegated explicitly; `ip_e2e_test` and `cpufreq_test`, whose mechanisms now
run through privd; `no_path_earns_a_mechanism_s_authority` and the
exec-narrowing test; and `priv-core`'s host tests. Checked by hand on the live
ISO under QEMU: the dialog for `bootctl set-default` as the session, Enter on
Deny refusing with 126, Enter straight after Tab ignored and then allowing, a
200-byte argument wrapped whole, Escape refusing, and an unanswered question
refused after 120 s. Not run: `test-install`, `test-installer`,
`test-remote` and the ladder with a toolchain; the network toggle's click and
the refusal cooldown were not reached by hand; and nothing grades a signal
relayed to a running mechanism, a withdrawal while the person is asked, or an
exclusive group refusing, since no shipped mechanism runs long enough under
test and a test boot has nobody to ask. On this TCG host
`check_fs_throughput.sh`'s `min-fs-over-raw-pct` read under its floor in one
capture before the change and one after, and passed in the final one.

### Phase 6: Delete the raise — done

`grants.rs`, `Launch`, `covers_grant_path`, the registry's flags and
priorities and the shell's spawn of registry programs are gone, and
capability bit 1 is `caps::RETIRED`. A child holds what `Endowment::check`
admits, exec keeps `caps & universal` process-wide, and `mount(2)` refuses
only what is pinned. The shell runs a registry program by fork and exec like
any other, handing its privd connection only to a mechanism its own process
runs whose environment names it; the dock spawns at the normal tier.
`Seal`'s deletion condition is restated. The Verus proof is rewritten: S1 no
step widens, S2 exec drops, S3 delegation is bounded by the spawner and no
operand names the program, S4 `caps_drop` is total and reaches every thread,
through the process's record those registered after it; each has broken
witnesses, eleven in all, the deleted grant table among them, and the
`parent.caps & grant` witness went with its reason. Graded by
`test_exec_drops_every_gated_capability`,
`test_mount_over_the_base_is_refused` (beside an ordinary mount),
`spawn_privilege_test` (a shell spawned with nothing signalled by a task
holding the universal set, a retired bit refused, an exec dropping
everything, the base's programs and directories sealed), `privd_test`'s
shell hand-off beside a subshell and a renamed or unset `PRIVD_FD` that may
not take it, and the proof, each obligation failing with its broken variant
planted. Not run: `test-install`, `test-installer`, `test-remote`,
`test-install-guest` and the ladder with a toolchain, which drive the shell's
hand-off from a paired agent and a root test.

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
  spawner's mask. exec keeps no gated capability, and nothing about the
  program a child loads adds to what it holds.
- **A retired capability's bit is never named again.** `Launch`'s bit is
  `caps::RETIRED`: `caps::ALL` leaves it out, so a spawn naming it is
  `EINVAL`, as one naming a retired flag bit is, and no later capability
  takes it.
- **`mount(2)` refuses only what is pinned:** the base's directories, the
  install medium's and what lies beneath them. With no privilege keyed on a
  path, nothing else is a target a mount must never cover.
- **The seal holds the base whole.** Every base inode is sealed and no
  capability unseals it; on a writable volume `Seal` moves the seal on the
  directories a root keeps for the base and on a system an image carries.
  privd's `Base` identity rests on the base being what booted, and privd
  starts an action's program by path, so the base's directories refuse a
  rename, a create beneath them and their removal. `Seal` dies when no root
  holds anything for the base.
- **The shell hands its connection to a mechanism alone.** It decides before
  starting it, on the path it then execs: a program the policy names, never a
  nested shell, while the environment names the connection, since a mechanism
  keeps back from what it starts only the connection its environment names. A
  command assigning `PATH` or `PRIVD_FD` is not handed it, and the forked
  child clears close-on-exec only on the connection itself, never on a
  descriptor a redirection put at its number.
- **Nothing shields remoted from the session.** It holds no capability and
  runs as uid 1000, so any session program may signal it and the OOM killer
  may take it for a session writer, and init does not restart it. A
  capability held only to fend off signals would be a catch-all bit by
  another name, and the session may power the machine off without consent
  anyway, which ends the remote control as surely.
- **The registry names programs and nothing more:** a name, a path and a
  description, for the shell's lookup, its aliases and `help`. A program the
  dock spawns runs at the normal tier, and one the shell forks at the
  shell's.
- **`task.flags` never carries authority.** It says where and how a task runs.
  The retired privilege bits are reserved, so a spawn naming one is `EINVAL`,
  and authority reaches a child only as a capability mask.
- **Capability bits are ABI.** `abi/src/caps.rs` states them once, and
  `slopos_ostd::authority::Capability` is held to exactly those bits, because a
  mask crosses the syscall boundary in `caps_drop` and `SpawnAttrs`.
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
- **Running ahead of everything is `SchedHigh`,** the `High` spawn tier and a
  nice value below zero alike, as Linux's `CAP_SYS_NICE` covers both. Init
  holds it and spawns the compositor at `High`; nothing is init's by task id.
- **A program that holds authority is its own binary.** privd's `Base(path)`
  keys on the resolved path, so a multicall symlink would hand every tool its
  authority; `prof` left coreutils for this.
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
- **A peer's identity is captured, never looked up.** `listen`, `connect` and
  `socketpair` record it, as Linux records `sk_peer_cred`; a later id change,
  exec or hand-off of the descriptor changes nothing a peer reads. Task ids are
  never reused, so the captured pid is the handle polkit's (pid, start-time)
  was not, and a peer's pidfd is handed out after the peer has gone, as Linux
  6.16's stale pidfds are.
- **The socket layer is told who connects.** An `Identity` reaches
  `unix_connect` and `unix_listen` from the syscall or the ring's submitter, as
  a `Subject` reaches the VFS; nothing below the syscall layer reads the
  current task for it. The ring asks its submitter at each try, so a connect
  that waits for room attests who connects when it does; the name's mode
  bits are checked once, against the submitter as `ring_enter` found it.
- **`Base` attests code: every executable mapping came from the base.** An
  executable mapping of anything else — anonymous memory, a memfd, a file
  elsewhere — and an `mprotect` making any page executable that was not lower
  the process to `Other` before that code can run, as macOS clears `CS_VALID`
  when an unsigned page is mapped. Without it a session process could start a
  dynamically linked base program with `LD_LIBRARY_PATH` naming its own
  `libc.so`. What the base's own code writes into its own writable text, as
  the loader's relocations do, is the base's doing, and a connection made, or
  a socket listening, before a forfeit keeps what it captured: the base
  program chose both.
- **A `program` or `paired` principal names a statically linked program.** The
  loader honours `LD_LIBRARY_PATH` outside `AT_SECURE`, and an object with no
  executable segment forfeits nothing yet can interpose symbols on base code.
  `/bin/remoted` and `/bin/compositor` are static; before a principal names a
  dynamic program, the loader must refuse objects from outside the base for
  it.
- **An exec that begins beside another thread attests nothing.** Exec here does
  not stop sibling threads, which run on in the new image; one could map or
  patch code there and exit before the exec ends. The process is `Other` from
  the reset of its address space until its next exec; one that fails before
  the reset changes nothing.
- **`Base` does not attest the arguments.** A principal must not name a program
  whose behaviour its caller steers through argv, the environment or the files
  it reads — a shell, an interpreter, a launcher — since anyone may start any
  base program. `program` and `paired` name programs that take their
  instructions from the sealed base or from their own peers.
- **`SO_PEERPROGRAM` answers a path, `SO_PEERSEC`-shaped.** The value is the
  base path with no NUL, no bytes for `Other`, and `ERANGE` with the length
  needed for a short buffer; a socket with no peer is `ENOTCONN`, so no peer
  never reads as `Other`.
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
- **A spawn asks for nothing it does not name.** Each id field of
  `SpawnAttrs` counts only under its `SPAWN_SET_*` bit, so a zeroed struct
  delegates nothing and keeps the spawner's ids rather than asking for uid 0.
- **A spawn is checked whole before anything is built.** Unknown capability
  or id bits and more than `NGROUPS_MAX` groups are `EINVAL`; a capability the
  spawner lacks, an id it could not take itself and the `High` tier without
  `SchedHigh` are `EPERM`. A child's ids follow POSIX's rules for the spawner
  moving to them, so a spawner may hand a child any of its own real, effective
  or saved ids without `SetId`, and the child's saved ids are its effective
  ones.
- **`AT_SECURE` follows what the child holds and who it runs as:** set when
  it holds a gated capability its spawner delegated, or runs with ids other
  than its spawner's real ones, as Linux sets it for a capability gain or an
  id change across exec.
- **A socket name is a file,** as on Linux: `bind` makes an inode of type
  socket through the parent's write and search checks (`EADDRINUSE` on an
  existing name), `connect` needs write on it, and `open` refuses it with
  `ENXIO`. A bound socket holds its inode open, as a descriptor holds a file:
  the inode is not freed or reused while the socket lives, so the socket table
  finds a listener by it and a name unlinked and bound again reaches the new
  listener and never the old, and the volume it is on is busy, as Linux's
  bound socket pins its path. A ring's `OP_CONNECT` takes only an absolute
  name, since a ring has no working directory.
- **A link in a shared directory is followed only for its owners.** In a
  sticky directory every class may write, a symlink is followed only when the
  follower's effective uid owns it or the directory's owner does, as Linux's
  `protected_symlinks`. It holds for every link a walk follows and is not
  waived by `DacOverride`: the follower it protects is a privileged one. The
  kernel's re-check of a path it has just opened for a caller resolves as
  that caller.
- **A pty's slave is its opener's,** uid and gid, mode 0620, as devpts gives
  it without a `tty` group; opening it takes those bits, judged on the slave
  the open takes, so a slot freed and handed to another opener in between is
  judged as the new opener's.
- **The ownership map is stated once.** `slopos_abi::fs::ROOT_LAYOUT` and
  `SESSION_TREES` are the statement, `scripts/lib/root_layout.sh` and
  `tools/initramfs` mirror it, and a test holds the three equal. Every
  builder lays a new root out by it and records the session's trees in
  `/var/lib/slopos/owners`, the initramfs included, so a RAM root is never
  migrated.
- **A root is migrated once, in place, by init.** A root without the record
  has everything below the session's trees handed to uid 1000, links never
  followed, a file with a second name and a read-only volume left alone, and
  the map's directories set as it says; the record is written, after a
  `sync`, only when nothing refused, so an interrupted migration runs again. Init also makes, at every
  boot, any directory of the map a root lacks, and the session's runtime
  directory, `/run/user/1000`, mode 0700.
- **A test runs as uid 0 holding everything unless it declares otherwise.**
  A test of an unprivileged gate or of the person's work declares `caps =
  none, uid = 1000`; one that needs a single gate declares only that.
- **A connection speaks as its maker, one request at a time.** privd reads a
  connection's identity once, at `connect`, and refuses a request once the
  process that connected has exited, checked before the person is asked and
  again after. Every reply names the request it answers, so a requester on an
  inherited connection passes over what an earlier one left. remoted makes one
  per mechanism it runs, the compositor one per toggle, and a root test one for
  the loop it drives.
- **A connection reaches only the shell and mechanisms.** Whoever holds one
  speaks as its maker, so it is handed only to `/bin/shell` and the programs
  the policy names, each of which keeps it from what it execs or starts; the
  shell's own process passes it only to a mechanism it runs, so no subshell
  or pipeline stage reads it beside another. A base program such as `env`,
  `xargs` or `find` would hand it to code from outside the base. What the
  paired agent runs through the shell is the paired agent's, a script the
  session can edit included.
- **privd never waits on a requester.** A reply the requester leaves unread
  for five seconds ends its connection, and root's and the paired agent's
  connections stand outside the pool the rest share. A mechanism writes to the
  requester's own descriptors, so one that is never read blocks; that holds
  only what the session could end anyway by restarting the machine.
- **The first matching `allow` decides, and a pairing is only ever paired.**
  The order of an action's lines is policy. A paired connection matches no
  `session` or `program` line, so an action that does not name `paired` is
  refused to the host; a `program` principal names the program whatever uid
  runs it; and `root` is listed on every action rather than implied.
- **privd holds the delegable set and nothing else,** stated once as
  `slopos_priv_core::DELEGABLE`; a policy naming any other capability does not
  parse, and one whose `program` principal names a dynamically linked program
  is refused whole.
- **The mechanism validates.** Started by privd, a mechanism acts only when
  the argv it parses belongs to the action `PRIV_ACTION` names; started by
  anyone else holding its action's capabilities and uid, it acts itself.
- **A terminal never reaches a mechanism.** The mechanism runs in privd's
  session, where a terminal answers `EIO`, and run0 isolates one the same way:
  `elevate` relays output to a terminal or socket through pipes, and input
  only for a mechanism that asks questions, the installer.
- **A requester that leaves withdraws its question, not its mechanism.** The
  dialog of one that disconnects or exits goes, and a mechanism already
  started runs to its end; a signal the requester takes before the mechanism
  starts withdraws the request, and after it `SIGHUP`, `SIGINT`, `SIGQUIT`
  and `SIGTERM` are relayed.
- **Consent is never remembered.** Unlike polkit's `auth_*_keep`, every
  request whose action asks, asks.
- **A question ignores input for 750 ms after the frame that first shows it
  and after each move of focus,** since a click or key that lands then was
  aimed at what was there before; Space and the vertical arrows do nothing,
  being keys someone typing elsewhere presses.
- **A refusal sticks for five seconds** and refuses what was waiting, so a
  requester cannot answer the person's no with the same question.
- **Consent covers the command shown, and what it asks after.** The dialog
  shows every byte of the argv and directory privd will use; an interactive
  mechanism's later answers come from its requester, so the installer's
  dialog says the requester chooses the disk.
- **Exit statuses follow pkexec's:** 126 when the person refused or did not
  answer, 127 for any other refusal.
- **The slot writers share one exclusive group,** `boot-slots`, since two of
  them writing one FAT volume at once corrupt it.

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
