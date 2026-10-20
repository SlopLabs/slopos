# Users and Login: More Than One Person

## Goal

SlopOS is built for one person today: the desktop is uid 1000 and opens with
no login. When this plan is done the system is multi-user by design, and a
default install still opens straight into one person's desktop:

- **Accounts.** People have accounts with names, homes, groups and optional
  passwords. An admin role decides who may change the system.
- **Login.** A login screen on the desktop and a login prompt on the text
  console. Several people can be logged in at once and switch between their
  sessions; the screen, input and clipboard belong to the session in front.
- **Approval names a person.** Each privd action says who may ask and what
  they must prove: nothing, a click by the person asking, their password, or
  an administrator's. A standard user's request for an admin action is
  answered by an admin typing their own password.
- **`priv <command>`** runs any command with administrative power, as
  `run0` and Windows' `sudo` do: started by privd on a terminal of its own,
  inheriting nothing from the caller, holding a ceiling of capabilities that
  never reaches the base, the boot chain or the user database.
- **Sessions without a screen** answer for their own person on their
  terminal; administrator credentials are typed only where nothing in the
  requester's session can read them.
- **One person is the same system with one account.** A fresh install has one
  person, uid 1000, an admin with no password, logged in automatically. Every
  dialog, path and command behaves as it does today until a second person is
  added; adding one is the step that asks the first for a password and hands
  `/usr/local` to the system.

## Depends on

- The access model (`AGENTS.md`, *Authority discipline*): capabilities are
  the only authority, a uid is an identity. This plan keeps that: an account
  and the admin role decide **who may approve**, never what a process holds.
- `plans/system-extensions.md`, landed: `root` is uid 0 running base code,
  extensions' users join the database, and privd holds `SetId`.

## Where it stands

**The kernel is already uid-generic.** Credentials with real, effective and
saved ids and 32 supplementary groups (`slopos-ostd/src/process/cred.rs`),
DAC on every VFS entry, the sticky bit, the signal rule on real and saved
uids (`core/src/syscall/signal.rs:99-153`), `SO_PEERCRED`, spawn endowments
with uid, gid, groups and login tag (`core/src/exec/mod.rs:311-368`), a pty's
slave belonging to its opener (`fs/src/fileio/fdops.rs:179-182`), and the
compositor's socket at a path keyed by uid (`windowing/src/connection.rs:47`).
Quota accounts already form a tree per process (`slopos-ostd/src/process/
account.rs`).

**Userland assumes one person.**
- `SESSION_UID`, `SESSION_GID` and `SESSION_HOME` (`abi/src/fs.rs:239-241`)
  are "the person"; `ROOT_LAYOUT` and `SESSION_TREES` give that uid
  `/home/user`, `/src` and `/usr/local`, recorded once in
  `/var/lib/slopos/owners` and mirrored by `scripts/lib/root_layout.sh` and
  `tools/initramfs`.
- The people are the accounts record's (Phase 1, below): `/bin/users`
  changes them through privd, and slibc reads the database rendered from it.
  There is no `getlogin`.
- init runs one desktop session tagged `DESKTOP`
  (`userland/src/init_control.rs:14-21`, `userland/src/apps/init/session.rs`),
  starts it as `SESSION_UID`, named as the database names that uid, with no
  supplementary groups (`userland/src/apps/init/service.rs`), and lets the
  session restart itself and uid 0 end it
  (`userland/src/apps/init/control.rs:154-158`).
- init makes one consent socket pair per boot and hands the same end to every
  compositor it starts; privd keeps one question in flight on it
  (`userland/src/apps/init/mod.rs:162`, `:597-600`).
- privd judges people by the accounts record (Phase 2, below): `user` and
  `admin` with levels, a click only from the person whose desktop is in
  front — the first person's, since init runs one desktop as them — and an
  administrator's password for a standard user. "Logged in" is a process
  carrying the desktop's login tag; there is no session table.
- remoted runs as uid 1000 (`userland/src/apps/init/service.rs:61-69`).
- The session's own tools write `/usr/local` directly (`tree-install`,
  `scripts/selfhost.sh install`).

**One seat, one holder.** The seat arbiter gives the display and the input
sink to one holder; an equal rank cannot take it from a live holder, "so two
compositors cannot trade the screen back and forth", and release is the
holder's death (`slopos-ostd/src/seat/mod.rs:183-195`). The virtual console
outranks the compositor.

**One clipboard.** The kernel keeps one clipboard buffer
(`drivers/src/input_event.rs:694-721`) behind `ClipboardGlobal`, which every
program holds (`abi/src/caps.rs:26`).

**Mounts.** `mount(2)` refuses `/`, the pinned base directories, occupied
targets and anything at, above or beneath `/etc` and `/var/lib/slopos`
(`slopos_abi::fs::KEPT_DIRS`).

**No login exists.** The installer names the first person; a parent's zombie
cap notes "SlopOS has no uid" (`sched/src/task/task_family.rs:28-31`).

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| [systemd-logind](https://www.freedesktop.org/software/systemd/man/latest/systemd-logind.service.html), [sd-login](https://www.freedesktop.org/software/systemd/man/latest/sd-login.html) | Users, sessions and seats are separate; a session has a class (greeter, user, background); devices follow the session active on the seat | Sessions with classes; the seat, input and clipboard follow the session in front |
| [polkit](https://www.freedesktop.org/software/polkit/docs/latest/polkit.8.html) | Per action: `yes`, `auth_self`, `auth_admin` and kept variants, by whether the session is active; admin identities are a group; a kept answer survives changed arguments; a standard user's request shows an admin's credential prompt; `pkttyagent` answers without a screen | Levels per principal; a kept answer bound to the exact command and directory; admins are a group |
| [run0](https://www.freedesktop.org/software/systemd/man/latest/run0.html) | Elevation without setuid: the service manager starts the command on its own pty, inheriting nothing; `--empower` is weak because the caller's other processes control the result | `priv <command>` in that model, with no `--empower` |
| [Windows UAC](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works) | Admins confirm, standard users type an admin's credentials, both on a secure desktop apps cannot reach | The compositor's dialog is that desktop; admin credentials are never typed into a requester's terminal |
| [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.txt), [OWASP](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) | Argon2id is the password hash; parameters by memory and passes | Argon2id, written here and tested against the RFC's vectors |
| [shadow(5)](https://man7.org/linux/man-pages/man5/shadow.5.html), [OpenBSD login.conf](https://man.openbsd.org/login.conf.5) | Hashes kept apart from the readable user list; authentication runs in a separate program, not loaded into the caller | `/etc/shadow`; a base helper checks passwords, never privd itself |
| [macOS users](https://support.apple.com/guide/mac-help/set-up-users-guests-and-groups-mtusr001/mac), [fast user switching](https://support.apple.com/guide/mac-help/switch-quickly-between-users-mchlp2439/mac), [GDM autologin](https://help.gnome.org/admin/system-admin-guide/stable/login-automatic.html.en) | Admin and standard accounts; several sessions logged in, one in front; automatic login of one account as a default | The same, with automatic login the default for a one-person install |
| [Android multi-user](https://source.android.com/docs/devices/admin/multi-user), [ChromeOS owner](https://support.google.com/chromebook/answer/1059256) | One owner manages the device; other users keep private data and cannot change device-wide settings | The installing person is the first admin |
| [Homebrew](https://docs.brew.sh/FAQ), [XDG base directories](https://specifications.freedesktop.org/basedir-spec/latest/) | A group-writable shared prefix offers no security against anyone with write access; per-user software lives under `~/.local` | System-wide software goes in through privd; per-person software lives in `~/.local` |

## Design

### Accounts

- **The user database** is `/etc/passwd`, `/etc/group` and `/etc/shadow`,
  uid 0's and sealed, changed only through the accounts code, which runs in
  base mechanisms holding `Seal`. `/etc` and `/var/lib/slopos` are pinned
  against mounts. Group membership decides who may approve, so the database
  is protected as a grant is: neither a `DacOverride` nor a `Mount` holder
  can make itself an admin.
- **uids**: 0 root; 1 to 99 base system users (`greeter` among them); 100 to
  999 extension users; 1000 and up people; 65534 `nobody`.
- **The `admin` group** names the administrators. The installing person is
  its first member; the last admin cannot be removed or demoted.
- **Passwords** are optional, hashed with Argon2id (RFC 9106's second
  recommended parameters: 3 passes, 64 MiB, 4 lanes) in `/etc/shadow`. A
  person with no password confirms their own requests by clicking where
  another would type it.
- **A second person.** Adding one asks the adding admin to set a password if
  they have none, turns automatic login off unless the admin keeps it, and
  hands `/usr/local` to uid 0 (*Software trees*). Until then nothing about the
  one-person system changes.
- **slibc reads the files**: `getpwnam`, `getpwuid`, `getgrnam`,
  `getgrouplist` and `initgroups` with real supplementary groups. With no
  `/etc/passwd`, root and `user` stay compiled in, so a RAM root and old roots
  work. `getlogin` reads the caller's session from `/run/sessions`, which init
  writes, by the login tag the kernel reports for the caller.
- **`/bin/users`** lists accounts as anyone, and through privd adds, removes,
  renames, sets a password, adds to or removes from `admin`, and sets
  automatic login. Changing one's own password asks for the old one.
- **Homes** are `/home/<name>`, 0700, made with the account. Removing an
  account keeps its home unless asked, and never reuses its uid.

### Principals and approval

privd's grammar gains principals and levels, and `session` is retired:

| Principal | Who | Levels |
|---|---|---|
| `user` | any person, in a session | none, `alone`, `confirm`, `password` |
| `admin` | a member of `admin`, in a session | none, `confirm`, `password` |
| `greeter` | uid `greeter` running `/bin/compositor` | none |
| `root` | uid 0 running a base program | none |
| `paired` | `/bin/remoted` on a paired base | none |
| `program <path>` | that base or extension program | none |

- **No level**: the request runs unasked. For `admin`, a standard user's
  request is answered by an admin's password instead.
- **`alone`**: unasked while the person asking is the only one logged in;
  otherwise as `admin`. This is polkit's rule for powering off a machine
  others are using.
- **`confirm`**: the person who asked clicks Allow.
- **`password`**: the person who asked types their password.
- **Answering for another person always takes a password.** A standard
  user's request at an `admin` line lists the admins, and one of them types
  their own password; an admin with no password cannot answer for anyone but
  themselves.
- **Only a session in front is asked.** A request from a session not on the
  seat is refused at a `confirm` or `password` level, unless its terminal
  agent answers, below.
- **`keep <seconds>`** keeps an answer for the same requesting session and
  parent process, action, exact command line and working directory, and
  nothing wider.
- **The dialog names the person** asking, their session and terminal, the
  program, the command and its directory, and for an admin answer the admin
  answering.
- **Admission** is pooled per person instead of one pool of 32 for everyone,
  under a ceiling for them all.
- **The existing policy translates line by line.** On a one-person install the
  person is the only admin and has no password, so every row behaves as
  today:

| Today | Becomes | Actions |
|---|---|---|
| `session consent` | `admin confirm` | `base.install`, `base.rollback`, `boot.install`, `boot.clone`, `boot.set-default`, `install`, `net.configure`, `net.toggle`, `rescue` |
| `session` | `admin` | `boot.oneshot`, `boot.commit`, `cpufreq.set`, `kconsole`, `prof` |
| `session` | `user alone`, `admin` | `power.poweroff`, `power.reboot`, `boot.reboot`, `console.keymap` |
| `session` | `user` | `boot.status`, `boot.spare` |
| `program /bin/compositor` | removed | `net.toggle`: every person's compositor is that program; `admin` covers the owner's switch |

### Authentication

- **`/sbin/authcheck`**, a base program, is the only one of the base's
  services that reads `/etc/shadow`: privd spawns it per check, hands it the
  account and the secret on a pipe, and reads one verdict. privd never parses
  a hash. Checks run one at a time, so Argon2id's memory is paid once.
- **Secrets travel on descriptors**, never in a command line, which privd
  shows and keeps.
- Failures are counted per account and slow the next attempt; privd keeps the
  count for as long as it runs.
- **The dialog collects secrets.** The compositor draws it over every surface,
  as the consent question is drawn today, and no client receives what is
  typed there. That holds because no compositor request injects input or
  reads another surface's pixels, the framebuffer and input belong to the
  seat's holder, and a session's processes cannot signal a compositor holding
  the seat capabilities they lack. Each is an invariant this plan states and
  its tests hold.
- **The terminal agent.** A session with no screen, a console or a later
  remote login, registers its controlling terminal with privd. A `password`
  request from that session is answered there, for the person who asked and
  only them: an admin's password is never typed into a terminal the
  requester's processes can read. A `confirm` request there takes the
  person's password, since a click cannot be told from a script; a person
  with no password can answer nothing there.

### Sessions and login

- **init keeps a session table.** Each session has a person, a class
  (`greeter`, `user`, `console`, `background`), its own login tag allocated
  when it starts, an entry in `/run/sessions`, and `/run/user/<uid>`, which
  lives while any session of that person does. `restart-session` and
  `end-session` name a session; a person may end and restart their own, uid
  0 base code any.
- **A consent link per graphical session.** init makes a consent pair for each
  greeter and desktop it starts and tells privd which session is in front, so
  privd asks the dialog of the session that asked, and only when it is in
  front.
- **Automatic login** of one account is a setting in the accounts record. A
  fresh install sets it for its first person. With it set, init starts that
  person's desktop at boot, as today; without it, the greeter.
- **The greeter** is the compositor in greeter mode, run as the base system
  user `greeter` with the seat: it lists the people, takes a password, and
  asks privd's `org.slopos.login` (`allow greeter`), handing the password on a
  pipe. The mechanism checks it and asks init, through `/run/init`, to start
  that person's desktop.
- **Switching.** Several desktops can be logged in at once, one in front.
  init switches the seat with a new kernel operation that revokes a live
  holder and grants another (`SeatSwitch`, init's alone, a private call); the
  arbiter keeps refusing a compositor that tries to take the seat itself.
- **The clipboard is a session's.** The kernel keys its clipboard by login
  tag, so nothing copied in one session reaches another; processes in no
  person's session, privd's and extensions' services, have none.
- **Locking** asks the person's password, or, with none, a click.
- **The text console** offers a login prompt on the virtual console: a
  `console` session with a shell, for a machine without a desktop or one
  whose desktop is broken. A later remote login reuses it: `org.slopos.login`
  takes a pty and starts a `console` session on it.
- **remoted** runs as the person the accounts record names for it, the first
  admin unless set, under a login tag of its own; the pairing in the base
  names the broker, not the person.
- **Quotas.** Each person's sessions sit under an account of their own in the
  existing quota tree, so one person cannot take every task or page from
  another; the zombie cap becomes per person.

### `priv <command>`

- `priv <command> [args]` asks privd's `org.slopos.priv` (`allow admin
  confirm`, `keep 300`) and runs the command on a terminal of its own, as the
  rescue shell does: `Input::Terminal`, a fresh pty relayed byte for byte,
  nothing inherited but the command line, the working directory and a minimal
  environment.
- **As the person, holding the ceiling.** The mechanism spawns the command as
  the requesting admin's uid, in their session's login tag, holding `Mount`,
  `NetAdmin`, `ConsoleConfig`, `SysInspect`, `Clock` and `DacOverride`;
  privd holds `SetId` to spawn it so. Never `Power`, `RawBlock`,
  `BaseUpdate`, `ExtensionUpdate`, `Seal`, `BootEntry` or `SetId`. Because it
  runs as the person, its own requests to privd and to `/run/init` are the
  person's, judged by `admin` lines like any other, never as `root`; the
  boot partition, the base, extensions and accounts still ask.
- The command is resolved on `/bin:/sbin:/opt/bin`, and `/usr/local/bin` once
  uid 0 owns it: never on a directory the person's processes can write.
- `priv -s` opens a shell with that power, which passes it on to what it
  runs, as the rescue shell passes its own; it is an action of its own,
  `org.slopos.priv.shell`, whose answer is never kept.
- It takes no `sudo` options beyond those, and reads no `sudoers`: a command
  run through it is asked for like any other action, by its level.
- The shell's banner and prompt show that the power is held.

### Software trees

- **While one person exists**, `/usr/local` stays theirs and their tools
  write it as today.
- **From the second person on** `/usr/local` is uid 0's and system-wide: the
  installer and the image builders install the toolchain and the apps tree
  there as today, and a session installs through
  `org.slopos.software.install` (`tree-install`, `allow admin confirm`,
  `allow paired`). Software one person installs there runs for everyone, so a
  person's own processes cannot change it unasked; `scripts/selfhost.sh
  install` asks for the action.
- **Per-person software** lives in `~/.local`: `~/.local/bin` on the session's
  `PATH` before the system's, `~/.local/share/slopos/apps` read by the dock as
  now, and `~/.local/lib` searched by the loader for that person's processes
  that are neither `AT_SECURE` nor uid 0, as `/usr/local/lib` is.
- **`/src`** stays the first person's workspace.

### The ownership map

`ROOT_LAYOUT` keeps the system's directories and drops the person's:
`/home/<name>` is made with each account, `/src` is the first person's, and
`/usr/local` the first person's or uid 0's as above. The owners record names
each person's trees. A root holding today's one-person record is migrated in
place at its first boot under this plan: `user` becomes the first admin with
no password and automatic login, and nothing else moves.

### Kernel changes

- `SeatSwitch`, a capability init holds, and the private call that revokes a
  live seat holder and grants a named task.
- The clipboard keyed by login tag.
- The zombie cap per person rather than per parent.
- The private call table grows past its 48 slots; base interface bumps for
  each change.

## Phases

Five phases. Each ends with something that runs, and lands as commits of its
own.

### Phase 1: Accounts — done

`slopos-accounts-core` holds the accounts record, the database rendered from
it and Argon2id hashes (`tls-core` gained BLAKE2b and Argon2id, held to RFC
7693's and RFC 9106's vectors). The record lives sealed in
`/var/lib/slopos/accounts`; `/etc` and `/var/lib/slopos` are kept against
mounts and unmounts; `admin` is gid 10; people are uids 1000 to 59999. `/bin/users` lists
people and, through `org.slopos.accounts.manage` and
`org.slopos.accounts.password`, adds, removes, renames, sets passwords,
promotes, demotes and sets automatic login; `/sbin/authcheck` checks a
password. slibc gained `getgrouplist`. init's `users sync` gives an existing
root the one person's record at its first boot; the installer asks for the
first person and a password. `accounts_test` grades it, `libc_abi_test` the
groups, and `installer_test` the installed root's person. Since the desktop's
person stays an administrator, only the host tests reach the last-administrator
rule, and since nothing starts a person's session yet, `accounts_test` calls
`initgroups` itself.

### Phase 2: Principals and approval — done

`slopos-priv-core`'s grammar names `user` (none, `alone`, `confirm`,
`password`), `admin` (none, `confirm`, `password`), `root`, `paired` and
`program`, and `keep <seconds>`; `session` and `consent` are gone and the
base policy is translated as the table says, but for `net.toggle` (below).
`Action::admits` judges a requester against the people privd reads from the
accounts record as each request comes. The consent channel is version 2:
`READY` names it, a question says what answers it and whose request it is,
`SHOWN` says it is on screen, and an answer carries the password typed,
which privd hands `/sbin/authcheck` one check at a time, each person's
failures doubling the wait before their next. The compositor's dialog names
the person, takes a masked password and chooses among the administrators
listed, and privd judges the request again once it is answered. Admission
is 32 connections per uid and 128 in all. Extensions' actions take the
same principals, levels and `keep`. `test_key` (`TestHarness`, a test boot
only; base interface 8) lets a test type into the dialog. `privd_test`
grades a click and Escape for the first person, a standard user answered by
an administrator's password and refused by a wrong one and while no
administrator has one, and `user alone` running alone and refusing a
power-off beside another person; `extension_action_test` a kept answer
reused for its own command and directory and not for another; the host
tests the rest.

### Phase 3: `priv <command>` — done

`/bin/priv` asks privd's `org.slopos.priv`, kept 300 s, or the unkept
`org.slopos.priv.shell` for `priv -s`, both `allow root` and
`allow admin confirm` and the first actions with `uid requester`: privd starts
it again as the person, in their groups and session, holding
`REQUESTER_CEILING` on a terminal of its own, and that copy runs the command
holding the same with an environment of its own (`slopos_userland::held`,
which the rescue shell now shares). The command is found only in
`DEFAULT_PATH`'s directories that only uid 0 can change, every link and `#!`
interpreter on the way held to the same; a shell `priv -s` runs hands the
power on and names it before its prompt. Every action's kept answer now also
keys on the requester's live parent, and privd holds a mechanism's directory
to the one the request sent. `priv_test` grades what a command runs with as
root, as the first person after a kept click and as a standard user after an
administrator's password; the person's programs, links, scripts and shells
refused or asked for every time; `priv -s` at a terminal; a relayed signal;
and, from `priv -s`, a uid-0 file in `/etc` edited while the raw disk, the
base, `/etc/passwd` and a sealed file refuse and `bootctl base install` is
still asked.

### Phase 4: Sessions and login

- init's session table, tags, `/run/sessions`, `getlogin`; consent links per
  session, a request from a session not in front refused at any level that
  asks, and privd's "logged in" and "in front" read from the table;
  automatic login; the greeter user, the `greeter` principal and
  `org.slopos.login`; `SeatSwitch`; the clipboard by tag; locking; the
  console login and its terminal agent; remoted under its person; per-person
  quotas; the compositor's network switch, which retries, offered only to a
  person `net.toggle` runs unasked for; a secure attention key that only the
  compositor reads, since a maximised client can draw a likeness of the
  dialog.
- Graded by a second person logging in at the greeter, both desktops
  switching with the seat and clipboard following the one in front, a request
  from the session behind refused, a locked session refusing a wrong
  password, a console login, one person's fork bomb leaving the other's
  session running, and the one-person boot going straight to the desktop.

### Phase 5: Software trees

- `/usr/local` handed to uid 0 at the second person and installed through the
  action; `~/.local` on `PATH` and the loader's search; the ownership map and
  its migration; `selfhost.sh install`.
- Graded by `apps_test` installing system-wide through the action and per
  person into `~/.local`, the second person running the first's system-wide
  app and not finding the first's `~/.local` one, and a one-person root
  passing `check_fs_image.sh --map` unchanged.

## Grading

- **Gates:** `check_base_interface.sh` for the new calls and structures;
  `check_libc_exports.sh` for slibc's new readers; the authority gates; the
  ownership-map mirrors' equality test.
- **Ratchets:** new tests and locks; re-measure and name the cause.
- **By hand, on the laptop:** a second person added, logged in from the
  greeter, switched to and from; `priv` used from each; the one-person
  install untouched until then.

## Out of scope

- **Several seats** at once: one display and one keyboard per machine.
- **Encrypted homes** and keys derived from a password (systemd-homed's
  model).
- **Security keys and PINs** as factors beside a password.
- **Remote login.** OpenSSH's server is the first extension that needs these
  accounts (`plans/system-extensions.md`); it reaches them through
  `org.slopos.login` and a `console` session.
- **`su`-style switching** of one terminal to another person, and **guest
  accounts**.
- **Network accounts** (LDAP, Kerberos).

## Decided

- **Who may approve is an identity question; what a process holds stays a
  capability.** Accounts and the admin role change only the answer to "may
  this request run", never a mask.
- **One person is the multi-user design with one account**, not a separate
  mode: automatic login, an admin with no password, every dialog a click.
  Adding a second person is the step that asks for a password and hands
  `/usr/local` to the system.
- **The user database is sealed and its directories pinned.** Group
  membership grants approval, so it is protected as grants are.
- **Answering for someone else always takes a password**, so a click at an
  unattended screen never approves for another person.
- **Admin credentials are typed only into the compositor's dialog**, never
  into a terminal the requester's processes share.
- **`priv` is `run0`, not `sudo`, and named apart from it.** No setuid, no
  policy language of its own, no sudo options, nothing inherited from the
  caller; a ceiling, not everything; and the person's own uid, so nothing it
  starts is `root`. Its name says it runs through privd.
- **A kept answer covers the exact command and directory**, because polkit's
  documentation warns that a kept answer survives changed arguments.
- **System-wide software goes in through privd** once more than one person
  runs it; a shared group-writable tree would let one person's processes
  change what everyone runs.
- **The accounts record is the truth; `/etc` holds its rendering.** The
  record keeps everything about people, hashes included, as sealed
  generations only uid 0 reads, so a writer cut off anywhere leaves the old
  record or the new. `/etc/passwd`, `/etc/group` and `/etc/shadow` are
  written from it and the extensions' records under one lock, and again at
  every boot; who administers is read from the record, never from
  `/etc/group`.
- **A root without a record is the one person's**, and slibc's compiled-in
  rows are that same rendering, so a RAM root, an old root and a fresh one
  read alike. The kernel keeps the store's directory, so it cannot be moved
  aside to make a root look recordless.
- **People come first in the database**, then extensions' users; a name or
  uid one listed earlier holds is passed over. A person never takes a name an
  extension's record keeps a uid for.
- **A uid is given once.** The record's `next` only rises, as an extension
  record keeps its uids for good.
- **The first person's home is the map's `/home/user`** until the map drops
  the person's directories (Phase 5); every other person's is
  `/home/<name>`, 0700.
- **Proving oneself is the current password, or a click for a person
  without one.** Changing one's own password needs the current one through
  either action; setting a first one needs the click privd's consent gives.
  A password the accounts' mechanisms read at a terminal costs two seconds
  when wrong, and those mechanisms run one at a time; one typed in the dialog
  is counted by privd.
- **A mechanism trusts `PRIV_UID` only beside `PRIV_ACTION`**, both set by
  privd; run directly, it acts for its real uid. The accounts' actions are
  `admin confirm` and `user confirm`, the mechanism asking the current
  password itself. They name no `paired` line: remoted runs as uid 1000 too,
  and `PRIV_UID` cannot tell it from the person.
- **A mechanism leaves who may ask to privd.** Started by privd, it does not
  judge its requester again, so an administrator's password that answers for
  a standard user is the whole answer; run directly, it holds its real uid to
  administering the machine.
- **`user` matches every person and `admin` every administrator**, so the
  first of them an action lists decides for those it matches. A person no
  line matches is answered by an administrator's password only where an
  `admin` line stands, and `user alone` is that line while another person is
  logged in: an administrator runs unasked, a standard user is answered for.
- **A click answers only for the person whose screen it is.** Anyone else
  types their own password, and a person without one can answer for nothing
  away from their own screen; an administrator without a password answers
  for nobody else. Phase 4 adds that a session not in front is not asked.
- **A key pressed into the dialog keeps its repeats and its release from
  every client**, since a release names its key and would spell what was
  typed, and a question taken away under someone typing a password keeps
  taking keys until they stop.
- **An answer is judged again before its mechanism runs.** The verdict
  privd reaches once the person has answered must run the request unasked,
  or ask the same of the same person and still list the administrator who
  answered, so a removal or a demotion while the dialog was up refuses it.
- **One uid's questions wait four at most, and a refusal holds back only that
  uid's**, so a person who keeps raising questions keeps nobody else's
  waiting: one Escape clears theirs.
- **A wrong password slows its person's next check, never locks them out**:
  two seconds doubling to a minute, for the life of privd. A lockout would
  hand anyone who can raise a dialog a way to shut the administrators out.
- **A kept answer is privd's memory of one exact request**: uid, session
  (its login tag and its eldest task, so a later session tagged alike reuses
  nothing), the live task that started the requester, action and the
  activation it was read from, how it was asked,
  command line and directory, for at most 900 seconds, forgotten when privd
  reads the extensions again, and nothing for a requester carrying no login
  tag, which is no session. One an administrator's password gave holds only
  while they could answer it now. An extension's `keep` needs no grant, since
  a kept answer only repeats a command the person allowed.
- **The consent channel names its version in `READY`.** A compositor of the
  first version, which names none, is asked only for a click in that
  version's words, so a desktop older than a live base switch keeps working
  for the one person, and refuses what needs a password until its session
  restarts. A newer compositor writes the version the running base's
  interface says its privd reads, since either side of a switch may be the
  older one, and says `SHOWN` once a question is on screen, which is when the
  requester hears it is being asked.
- **`net.toggle` is `admin`**, unasked for every program an administrator
  runs where the compositor's program line was the only one before: the
  switch re-asks until the kernel agrees, which no dialog could sit behind,
  and switching the network is the least an administrator's program could do.
- **A test types into the desktop through `test_key`**, a private call that
  only `TestHarness` and a test boot reach, delivering a key after layout
  resolution past the diagnostic console and the TTY; the greeter's tests use
  it too.
- **`/sbin/authcheck` answers by its status alone**: 0 the password, 1
  anything else, 2 cannot tell. It believes `/etc/shadow` only sealed, so a
  change whose database was not written leaves the old hashes answering
  until the next boot writes it.
- **The kept directories are part of the base interface** (7), since a
  base's stores rest on them.
- **A mechanism that reads secrets runs on a terminal of its own**
  (`Input::Terminal`) whenever the requester's standard input is a terminal,
  with echo off from before the first prompt to after the last, claiming it
  as its controlling terminal so an interrupt reaches it; the installer does
  too. The requester relays nothing it shows before its own terminal is raw.
- **The first person stays an administrator until there is a login screen**,
  since the desktop runs as them alone and the person at the screen must be
  able to allow what the system does.
- **A volume holding a kept directory is not unmounted**, so the store cannot
  be taken from under a running root to make it look recordless.
- **An action may run its mechanism as its requester, under a ceiling stated
  once.** `uid requester` runs it as whoever asked, which only `root`, `user`
  and `admin` may be, holding no more than `REQUESTER_CEILING`, a constant of
  `slopos-priv-core` rather than a policy line, so no edit to an action widens
  what a person's own process may hold. It never names `Power`, `RawBlock`,
  `BaseUpdate`, `ExtensionUpdate`, `Seal`, `BootEntry`, `SetId` or
  `ProcSignal`.
- **`Power` stays out of the ceiling.** It writes the firmware's variables in
  the loader's namespace and SlopOS's, which choose what boots behind
  bootctl's checks and can fill the firmware's store; powering off,
  restarting, choosing a boot entry and the CPUs' speed each have an action
  an administrator runs unasked, which a program `priv` runs asks for as the
  person.
- **A kept answer covers one requester's parent as well as its session**, as
  polkit keys one on a live parent: one given to a person's shell covers that
  shell and what it starts, as sudo's covers a terminal, and nothing else
  running in their session. A shell, which runs whatever its input says, is
  never kept: `priv -s`, and the shell named with nothing after it, is an
  action of its own without `keep`; the shell told to read its input
  otherwise is refused, and `priv` never runs `priv`. A kept
  command still repeats with whatever input the person's processes give it,
  as sudo's timestamp lets them run anything.
- **A mechanism's directory is held to the one the request sent.** privd
  refuses to start one whose directory's path no longer leads where the
  descriptor sent did, so the person's processes cannot move an answered
  request into `/etc`. A working directory is a path, so they can still move
  one from under what already runs there.
- **What runs with power is found only where only uid 0 writes.** The program
  is in one of `DEFAULT_PATH`'s directories; it, every link on the way from
  it, what they lead to, a script's interpreter, and every directory from `/`
  down to each are uid 0's and writable by nobody else; the requester's
  `PATH` is never read. What it runs reads `$HOME` as the person's, whose
  processes can change it: no program of the base reads a file there today,
  which Phase 5 holds to when `/usr/local` becomes uid 0's.
  `/usr/local/bin` joins by that rule once uid 0 holds `/usr/local`
  (Phase 5), with no change to `priv`.
- **A standard user's `priv` runs as them**, not as the administrator who
  answered: the answer allows the request and lends no identity, so its own
  requests are still a standard user's. `DacOverride` reaches every person's
  files, so the administrator who answers hands them every person.
- **The ceiling reads every file not sealed, hashes included.** `DacOverride`
  reads `/etc/shadow`; an Argon2id hash at RFC 9106's second cost is what a
  reader gets, and whoever runs `priv` was allowed that much by an
  administrator.
- **What `priv` makes is the person's**, since it runs as them: a file it
  makes in a uid-0 directory stays theirs, and writable by everything they
  run, until a `priv chown` gives it to uid 0. A sudoedit-style flow for
  system configuration is not part of this plan.
- **What `priv` runs says nothing of `priv`'s.** Its output is the command's
  alone, so it pipes; the shell names the power in its banner and prompt, as
  the terminal has no background tint for it the way `run0` tints one.
- **What hands its mask on spawns, and passes signals on**: a signal another
  process sends, privd's relays among them, goes on to the program; one the
  terminal raised reached the program's group already.

## Constraints

- `slopos-ostd` remains the only kernel crate with `unsafe`.
- slibc stays `MIT OR Apache-2.0`; Argon2id and the database readers take
  nothing from glibc or libxcrypt.
- Concepts only from systemd, polkit, OpenBSD, Windows, macOS and Android;
  cite documented behaviour, never their files.
- Every phase updates `AGENTS.md` where it changes a stated invariant (*The
  session is uid 1000; the system is uid 0*, the ownership map, privd's
  principals), and runs the security sweep over what it touched.
