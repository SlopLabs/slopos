# System Extensions: System Software Outside the Base

## Goal

A general-purpose system lets software extend it. When this plan is done,
software that is not part of the base can do what system software does on
Linux, FreeBSD or macOS, without giving up the sealed base or the capability
model:

- **A daemon starts at boot** without a session, runs as a user of its own,
  holds the capabilities it was granted, listens on sockets init keeps for it,
  logs to a file, and is restarted when it crashes.
- **An app gets a privileged helper.** An extension ships privd actions for
  its own mechanisms, as a polkit action file does, and an app in
  `/usr/local` asks for them as for the base's.
- **The kernel knows what runs.** A program of an extension is attested the
  way a base program is, so privd and other programs judge it by what it is,
  never by a path someone could write.
- **The owner decides, once.** A first install asks the person and shows what
  it grants. A signing key the owner trusts installs and updates its
  extensions with no question, inside a ceiling the owner set for that key.
  Scripts, CI and the paired host install without anyone at the machine.
- **Extensions keep the base's guarantees.** Each is a sealed image, replaced
  whole and live, with the previous kept for rollback. A broken one is
  switched off from the boot menu or the rescue shell.

## Depends on

- The live userland, landed: the sealed base, live activation through
  `BaseUpdate`, init as a service manager, privd's policy and consent, the
  rescue shell.
- The access model (`AGENTS.md`, *Authority discipline*). This plan keeps
  every rule it states and widens one premise: a program the kernel attests is
  a file of a base generation **or of an extension activation**. It tightens
  another: uid 0 alone is never authority, at privd or at `/run/init`.
- `plans/users-and-login.md`, for the user database this plan's system users
  join and for privd's `user` and `admin` principals, which replace `session`.
  Until it lands, `user` reads as today's `session` and `admin confirm` as
  `session consent`; the shared parts land with whichever plan comes first.

## Where it stands

**Only the base and the owner's extensions hold authority.**
- A program gets capabilities three ways: init starts it from its service
  table (`userland/src/apps/init/service.rs`, every entry a base path), init
  starts it as an enabled extension's service (Phase 3), or privd runs it as
  an action's mechanism: the base's (`assets/privd/actions`), run as uid 0, or
  an enabled extension's, run as its first user (Phase 4).
- So nothing with authority runs loose code: every program it runs is a file
  the kernel attests, the base's or an activation's, and the loader keeps
  `/usr/local/lib` from uid 0 and every `AT_SECURE` process.

**uid 0 is not a principal.** privd's `root` is uid 0 running a base
program, read again before every verdict, and every action allows it without
a question, `base.install` and `rescue` included. `/run/init` takes
`end-session` and `reexec` from uid 0 running a base program as of the
request (Phase 1).

**Identity is what the kernel serves, while the program runs** (Phase 1).
- Exec records `Program::Base(inode)` for a base program,
  `Program::Extension(inode)` for an activation's whose other links are the
  base's or its own, and `Program::Other` otherwise
  (`core/src/exec/script.rs`, `Program::admits`). An executable mapping of
  code the program may not run makes the process `Other`.
- A socket records its peer's identity at `listen`, `connect` or
  `socketpair`; its program stands while that process runs that program
  instance (`Identity::now`), and `SO_PEEREXTENSION` names the activation.
  privd reads the program again before every verdict and before a mechanism
  starts (`userland/src/apps/privd/mod.rs`).
- privd's `program <path>` principal names a static base program under
  `/bin`, `/sbin` or `/usr/bin`, a dynamically linked one refused because the
  loader would take libraries from outside the base under the base's
  identity; in an extension's action it may name one of that extension's own
  programs, judged by its activation's label (Phase 4).
- remoted reads its pairing only from the base, because "a configuration
  anywhere a program could write would hand any program the authority of
  whatever broker it named" (`userland/src/apps/remoted/mod.rs:16-20`). The
  same holds for anything that grants authority.

**Serving sealed archives is solved, for the base and in memory for
extensions.** `fs/src/basefs/` serves a newc archive from memory, every inode
sealed: `BASE_FS` with up to sixteen generations, `MEDIUM_FS`, and a pool of
32 activation instances (`basefs/extension.rs`) shown at `/opt/<name>`
(Phase 1). The mount table holds 64 mounts.

**Verity is an integrity check, not an authenticity one.** `fs/src/verity.rs`
verifies a disk against a CRC-32 per block from a trailer (`:1-33`); nothing
verifies a SHA-256 tree, and no kernel crate computes SHA-256. There is no
file-backed block device (`fs/src/blockdev.rs` holds `MemoryBlockDevice`).

**Extensions extend privd, `PATH` and the dock** (Phase 4): privd runs the
running extensions' actions, `/opt/bin` links their commands last on
`DEFAULT_PATH`, the dock shows their app entries, and `/etc/<name>` holds the
machine's configuration of each.

**The private call table is full.** `SYSCALL_PRIVATE_TABLE_SIZE` is 48 and
`ext_activate` and `ext_deactivate` took the last two slots; the next private
call grows the table.

**The kernel's command line stays in the kernel.** It is parsed in
`boot/src/early_init.rs`; userland learns of it only through `sys_info`'s boot
flags, `BOOT_FLAG_EXTENSIONS_OFF` among them.

**Outside the base and the extensions, software has the session's power
only.**
- Apps install into `/usr/local` or `~/.local` as the session, with no
  capability (`tree-core`, `/bin/tree-install`).
- The loader searches `/usr/local/lib` only for a process that is neither
  `AT_SECURE` nor uid 0, and refuses `$ORIGIN` under `AT_SECURE`
  (`slibc/src/ld_so/mod.rs`).
- The dock reads app entries from each shown extension's `share`, then
  `/usr/share`, `/usr/local/share` and `~/.local/share`
  (`userland/src/app_entry.rs`).

**Extensions have users; people are one.** Records keep uids from 100 to
999, and `/etc/passwd` and `/etc/group` list root, `user` and each installed
extension's users, sealed (Phase 3); slibc answers root and `user` while
neither file is sealed.

**Ports are not privileged.** `Port::is_privileged` (`net/src/types.rs:124`)
gates nothing; any process may bind any port.

**`/run` is uid 0's** (`fs/src/vfs/init.rs`): only init makes directories
there. `/opt` and `/opt/bin` are in `ROOT_LAYOUT`, uid 0's.

**Signatures.** `tls-core` has SHA-256, SHA-512, X25519, ECDSA and RSA, and
verifies Ed25519 strictly and SSHSIG (Phase 2). OpenSSH's `ssh-keygen` is in
every toolchain install.

**Extensions go in through privd** (Phase 2). `/bin/extctl` runs the four
`org.slopos.extension.*` actions as uid 0 with `ExtensionUpdate` and `Seal`,
over a store the kernel keeps where it is.

**init runs extensions' services** (Phase 3): `extctl activate-all` at boot,
each service as its user under a login tag of its own, on sockets init
keeps, logged by a copy of init running as the extension's user
(`userland/src/apps/init/extensions.rs`).

## What the prior art settles

| Source | What it shows | Taken here |
|---|---|---|
| [systemd-sysext](https://www.freedesktop.org/software/systemd/man/latest/systemd-sysext.html), [Flatcar](https://www.flatcar.org/docs/latest/sys-ext/) | Add-ons are separate read-only images beside an immutable `/usr`; an `extension-release` file gates compatibility; Flatcar's `SYSEXT_LEVEL` decouples images from OS versions | Extensions are sealed images beside the base; compatibility is the `libc.so` contract, not the base version |
| [portablectl](https://www.freedesktop.org/software/systemd/man/latest/portablectl.html) | Attaching an image picks a privilege profile; `trusted` means full privileges | The grant is chosen at install, outside the image |
| [Android APEX](https://source.android.com/docs/core/ota/apex), [privileged allowlist](https://source.android.com/docs/core/permissions/perms-allowlist) | An update must be signed by the key of the installed copy; privileged permissions need an allowlist on the verified partition | Same-key updates need no question; a ceiling in the base bounds every grant |
| [Fuchsia capabilities](https://fuchsia.dev/fuchsia-src/concepts/components/v2/capabilities), [Genode Sculpt](https://genode.org/documentation/articles/sculpt-24-04) | A component declares what it uses; its parent's configuration routes it; nothing is ambient. Sculpt verifies software by the provider key the owner chose | The extension declares, the owner grants, and the owner chooses the keys |
| [macOS SMAppService](https://developer.apple.com/documentation/servicemanagement/smappservice), [system extension policy](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.system-extension-policy.yaml) | A daemon is registered from a signed bundle and approved once by the person; a management profile pre-approves by team identifier and extension type | Approve a signer and a ceiling once, then automate; the owner holds the keys, not a vendor |
| [shim MOK](https://github.com/rhboot/shim/raw/main/MokVars.txt) | An owner key is enrolled once, with the person at the console | Trusting a key asks the person; using it does not |
| [FreeBSD veriexec](https://reviews.freebsd.org/D8575), [IPE](https://docs.kernel.org/admin-guide/LSM/ipe.html), [fs-verity](https://docs.kernel.org/filesystems/fsverity.html) | Trusted userspace verifies a signed manifest and the kernel holds the digests; IPE judges execution by verity roots; fs-verity verifies each page against a Merkle tree | A base program checks signatures; the kernel attests the bytes it serves; a SHA-256 tree is the on-disk form |
| [composefs](https://github.com/composefs/composefs) | Per-file verity cannot cover metadata; an image can | One signature covers an extension's files, modes and names together |
| [ssh-keygen -Y](https://man.archlinux.org/man/ssh-keygen.1.en) | SSH keys sign arbitrary data under a namespace, as git's SSH commit signing does | Extensions are signed with the owner's SSH key |
| [polkit](https://polkit.pages.freedesktop.org/polkit/polkit.8.html), [pkexec](https://polkit.pages.freedesktop.org/polkit/pkexec.1.html), [D-Bus activation](https://dbus.freedesktop.org/doc/dbus-specification.html) | A package ships actions with per-session defaults; a system service runs as its `User=`; the caller is a uid, pid and session, never a program | Extensions ship actions whose mechanisms run as their own users; the kernel attests the calling program too |
| [sudoedit](https://www.sudo.ws/docs/man/sudo.man/) | The person edits a copy as themselves; changed files go back; the editor's words stay apart from the files | `extctl config edit`, the file going in by path and digest |
| [snapd](https://github.com/canonical/snapd/blob/master/data/env/snapd.sh.in), [FHS /opt](https://refspecs.linuxfoundation.org/FHS_3.0/fhs/ch03s13.html) | Package commands are appended to `PATH` and menu entries to `XDG_DATA_DIRS`; `/opt/bin` links packages' front ends | `/opt/bin` last on `PATH`; an extension's app entry replaces none |
| [TUF](https://theupdateframework.github.io/specification/latest/), [AVB](https://android.googlesource.com/platform/external/avb/+/refs/heads/main/README.md) | Signed metadata carries a version the client never lets fall | A signed configuration's serial, kept in the record |
| [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html) | `ConfigurationDirectory=` stays root's, readable by the service | `/etc/<name>` is uid 0's and sealed |
| [Redox userutils](https://gitlab.redox-os.org/redox-os/userutils), [pkgar](https://gitlab.redox-os.org/redox-os/pkgar) | `sudo` is one group check with no per-command policy or caller program; packages are Ed25519-signed with no version, so an old one can come back | A policy per action, judged by the attested program, and a version floor per extension |

Asterinas runs Linux's userland unmodified, so what it offers here is
Linux's. None of them gets the owner's position right for this system. ChromeOS,
Fuchsia and locked Android admit system add-ons only under the vendor's key,
so the owner's only way in is all or nothing. Linux, NixOS and plain sysext
trust whatever root drops in, with no grant step and identity by path.

## Design

### An extension

- **A name**: lowercase letters, digits and `-`, at most 32 bytes, no dots.
  `slopos`, `base`, `bin`, `etc`, `lib`, `root`, `user` and the names of the
  base's directories are reserved.
- **An image**: a newc archive packed and checked as a base is, with a
  manifest of `<sha256> <mode> <path>` for every record (`boot-core::base`'s
  format) and, under `/.extension/`, `release` (`name`, `version` an unsigned
  integer, `display` a free version string, `summary`) and `declare`.
- **A sidecar** `<name>.meta`: the name, the version and the image's SHA-256.
  It is what is signed, so the signed object keeps its shape when Phase 5
  changes the payload's.
- **A signature**, optional: `<name>.meta.sig`, `ssh-keygen -Y sign -n
  extension@slopos -O hashalg=sha512` over the sidecar, Ed25519 keys only.
- **Contents** are relative to its mount point: `bin/`, `libexec/`, `lib/`,
  `share/`. Its programs are loose programs: linked dynamically against
  `libc.so`, no `syscall` instruction (`check_loose_programs.sh`). An
  extension carries the libraries it needs beyond `libc.so`; one extension
  does not load another's.
- **Built** by `extctl build <dir>` in the guest and `tools/extension` on the
  host, over one crate, `extension-core`, as the base's packer shares
  `boot-core`.

### What an extension declares

`declare` is `key value` lines in the policy file's style. Capabilities are
named as `slopos_abi::caps` names them.

- `user <suffix>`: a system user, named `<name>` or `<name>-<suffix>`.
- `service <name>`: a program, its arguments, the user it runs as, the
  capabilities it asks for, `after <service>`, `reads` lines naming files of
  the image and of `/etc/<name>` it reads beside its program, `listen` lines
  (a TCP or UDP port, or a socket under `/run/<name>`), `restart`, and
  `session` for one that runs in each person's desktop session.
- `action <name>`: a privd action named `ext.<name>.<action>`, whose program is
  the extension's, in the policy grammar: argument shapes, capabilities,
  `allow` lines. An extension action may allow `admin` and `user` at any
  level, `paired`, and `program` naming its own programs or a base program;
  never `root`.
- `command <name>`: a program put on everyone's `PATH`.
- `app <id>`: an app entry and its icon under `share/slopos/`.
- `state`, `config`, `runtime`: whether it has `/var/lib/<name>`,
  `/etc/<name>` and `/run/<name>`.

### What is granted

- **For a first install**, the grant is what the person approved: the line
  `extctl install` puts in the consent question, below. Each service and
  action holds what it declares, intersected with the grant.
- **For a signed install**, the grant is the declaration, held inside the
  key's ceiling.
- **An unasked action**, one that allows `user` with no level or `alone`,
  `admin` with no level, or a `program`, hands its capabilities to every
  person's processes; one that allows `paired` hands them to the broker. It counts against a key's
  separate `user` ceiling, empty by default, and it is named in a first
  install's grant line.
- **The base's ceiling**, `/usr/share/slopos/privd/extensions`, sealed in the
  base, names what any extension can ever be granted: `Power`, `Mount`,
  `RawBlock`, `NetAdmin`, `ConsoleConfig`, `SysInspect`, `Clock`,
  `DacOverride` and `SchedHigh`. Never `BaseUpdate`, `ExtensionUpdate`, `Seal`,
  `BootEntry`, `SetId`, `ProcSignal`, `TestHarness`, `DisplaySeat` or
  `InputSeat`. `RawBlock`, `Mount` and `DacOverride` can rewrite or cover the
  root, so they are full control: the question says so, and a key's ceiling
  leaves them out unless the owner names them.
- An action's capabilities are also inside `DELEGABLE`, which privd's grammar
  already holds them to; `SchedHigh` is a service's only.
- **Every activation re-checks** grant ⊆ key ceiling ⊆ base ceiling, so
  lowering a key's ceiling, removing the key, or booting a base with a smaller
  ceiling stops what no longer fits.

### Where it lives

| Path | What | Owner |
|---|---|---|
| `/var/lib/slopos/extensions/` | the store; it and each directory on the way to it kept by the kernel, never renamed, removed or covered | uid 0, sealed |
| `…/keys.<n>` | trusted keys, `allowed_signers`-shaped: key, ceiling, session ceiling, comment | uid 0, sealed |
| `…/<name>/<sha256>.ext`, `.meta`, `.meta.sig` | the images, their sidecars and signatures | uid 0, sealed |
| `…/<name>/record.<n>` | the grant, the floor, the refused versions, enabled and why not | uid 0, sealed |
| `/var/lib/slopos/extensions.lock` | the writers' lock | uid 0, 0600 |
| `/opt/<name>` | the active image, read-only and pinned | the kernel |
| `/opt/bin` | links to declared commands | uid 0 |
| `/etc/<name>` | the machine's configuration | uid 0, sealed, readable by everyone |
| `/var/lib/<name>`, `/var/log/<name>` | state and logs | the extension's first user |
| `/run/<name>` | runtime directory and sockets | the extension's first user, made by init |

- `/opt` and `/opt/bin` join `ROOT_LAYOUT` and its mirrors, uid 0, 0755.
- `DEFAULT_PATH` is `/bin:/sbin:/usr/local/bin:/opt/bin`: the base first,
  then the system-wide software tree, then what the owner granted; a person's
  `~/.local/bin` comes before the last two in their session. A granted command cannot
  shadow the toolchain; two extensions declaring one command are refused at
  install.
- The seal keeps a `DacOverride` holder from rewriting a record without also
  holding `Seal`; the kernel's hold keeps anyone from renaming the store aside
  or covering it.
- The store is world-readable, so `extctl status` lists every extension, its
  record, every key and its ceilings as whoever runs it.

### The kernel: activation and identity

- **`ExtensionUpdate`**, a new capability: activating, replacing and
  deactivating an extension. init holds it, as every capability; it joins
  `DELEGABLE` for the extension mechanisms; no extension is ever granted it.
- **`ext_activate(name, archive, len, digest)`** copies an archive from the
  caller's memory into kernel memory as `base_activate` does, indexes it, and
  mounts it read-only and pinned at `/opt/<name>`, replacing a previous
  activation's mount in one step. It records `name` and `digest` as the
  activation's label: the caller holds `ExtensionUpdate` and has verified the
  bytes, and the kernel serves the copy it made, so no one swaps them after.
  `ext_deactivate(name)` unmounts it. These take the last two free private
  slots.
- **`basefs` generalised**: `is_base`, `Hold`, activation, the reaper and
  `program_path` take any instance, and an instance per activation is drawn
  from a pool. An activation number, never reused within a boot, is the high
  half of every inode it serves; an old activation is freed once nothing
  holds it. The mount table grows by the activations it must hold.
- **`Program::Extension(inode)`**: exec records it when the image, every link
  of its `#!` chain and its interpreter are files of the base or of that one
  activation. An executable mapping of anything else makes the process
  `Other`.
- **Identity lasts while the program does.** A socket's recorded identity,
  base or extension, holds only while the process it was taken from still
  runs that program: an exec, a foreign mapping or its exit turn it `Other`.
  The one exec that keeps it is init's re-execution of the base's
  `/sbin/init`, which keeps its capabilities too (`exec_keeps_authority`), so
  the consent pair init made stays init's. The recorded value never becomes a
  later program's, so a request written before an exec is never read as the
  next program's.
- **Peers.** `SO_PEERPROGRAM` answers `/opt/<name>/<path>` in the activation's
  tree, and `SO_PEEREXTENSION` at `SOL_SLOPOS` answers its label. privd reads
  both before every verdict; `SO_PEERCRED` stands as connected.
- **`BOOT_FLAG_EXTENSIONS_OFF`** in `sys_info`, set by `extensions=off` on the
  command line, under which `ext_activate` answers `EPERM`.
- **Memory.** Activations share a budget, an eighth of usable memory by
  default (`ext.memory=<percent>`), at most 32 at once. Phase 5 lifts it.
- **Base interface 5**: the two calls, the socket option, the boot flag, the
  capability's bit and the identity rule.

### Trust and the install

Three things are kept apart, as veriexec and macOS keep them: the
**measurement**, the digest of the bytes the kernel serves; the **authority**,
the key or the person that vouched for them; and the **grant**.

**`root` means base code.** privd's `root` principal and `/run/init`'s uid-0
commands require uid 0 **and** a base program. An extension never runs as
uid 0, and its mechanisms run as its own first user: privd holds `SetId`
beside `DELEGABLE` to spawn them so, and `ProcSignal` to relay their
requesters' signals, and delegates neither.

**Actions.**

| Action | Command | Capabilities | Allow |
|---|---|---|---|
| `org.slopos.extension.install` | `install *` | `ExtensionUpdate Seal` | `root`; `session consent`; `paired` |
| `org.slopos.extension.install-signed` | `install-signed *` | `ExtensionUpdate Seal` | `root`; `session`; `paired` |
| `org.slopos.extension.trust` | `key add *`, `key remove *` | `ExtensionUpdate Seal` | `root`; `session consent`; `paired` |
| `org.slopos.extension.manage` | `enable *`, `disable *`, `remove *`, `rollback *` | `ExtensionUpdate Seal` | `root`; `session consent`; `paired` |
| `org.slopos.extension.config` | `config install *` | `Seal` | `root`; `session consent`; `paired` |
| `org.slopos.extension.config-signed` | `config apply *` | `Seal` | `root`; `session`; `paired` |

`session` becomes `admin` and `user` once `plans/users-and-login.md` brings
more than one person.

- **A first install** is `extctl install <image> <sha256> name <name> grant
  <caps> unasked <actions> users <names> commands <names>`. The person reads
  exactly what is granted; the mechanism refuses an image whose `release` or
  `declare` says anything else. It fits privd's 320-byte question for any
  extension a person could judge in one; a longer one is refused there.
- **A signed install** names no grant: the mechanism refuses unless the
  sidecar is signed by a trusted key whose ceilings cover the declaration, and
  the version is allowed.
- **A key** is trusted by `extctl key add <key.pub> ceiling <caps> user
  <caps>`, which the requester writes out as `key add <fingerprint>
  ssh-ed25519 <base64> ceiling <caps> user <caps> [comment]`; the question
  shows its fingerprint and both ceilings. Where
  the private half lives is the owner's choice: on another machine, where a
  compromised session cannot sign, or in a session, for `sudo NOPASSWD`-like
  convenience that anything running in that session then shares.

**Versions.**
- An update signed by the key the record names, inside its ceilings and above
  the floor, installs with no question. Any other update is a first install.
- The floor is the highest version installed. A lower one is refused; the
  same one is a no-op when its digest matches and refused when it does not.
- `extctl rollback <name>` makes `previous` current, lowers the floor to it,
  and adds the version it left to the record's refused list, until a newer one
  installs. Removing an extension keeps its record, floor and uids.

**At every activation** a base program checks the image against its record:
the digest, or the signature and the key's presence; the floor and the refused
list; the declaration against the grant; the grant against both ceilings; and
the image's dynamic symbols against the running `libc.so`'s exports and its
own libraries, as `check_bootstrap_config.sh` does. A failure disables the
extension and says why. `libc.so` only grows, so the symbol check fails only
after a base rollback to an older one.

### Users

- An extension's users join the user database `plans/users-and-login.md`
  defines: `/etc/passwd` and `/etc/group`, sealed, written by the extension
  mechanisms through the same accounts code. init and privd take an
  extension's uids from its record, never from a name lookup.
- An extension's users get uids and gids from 100 to 999, allocated at first
  grant and kept after removal, so its state keeps its owner. They are never
  members of `admin`.

### Services

- At boot init runs `/bin/extctl activate-all` itself, holding
  `ExtensionUpdate` and `Seal`, before it starts privd and unless
  `BOOT_FLAG_EXTENSIONS_OFF` is set; privd then loads the enabled extensions'
  actions as it starts, and init starts their services in `after` order
  before the session.
- A service holds exactly its grant, runs as its user in a session of its own
  with an extension login tag, starts in `/var/lib/<name>`, and gets init's
  readiness notifier. Its standard output and error go to
  `/var/log/<name>/<service>.log`, kept at two files of a bounded size.
  Restarts follow init's budget, five a minute.
- **Sockets.** init binds each `listen` socket before the service's first
  start, hands it over by descriptor, and keeps it across restarts, as
  launchd and systemd do: no other process can take the port while the
  service is down, and a service needs no capability to listen.
- `session` services start with each person's desktop session, as that
  person and holding their grant, each under a tag of its own that the
  session's end ends with the desktop's.
- **A live change** reaches init as `/run/init`'s `extensions` command, from
  the mechanism, which is base code running as uid 0. init reloads the
  records, restarts the services whose program or `reads` changed
  (`boot_core::base::changed` over the two manifests, and what they read of
  `/etc/<name>`), stops what was disabled or removed, and has privd read the
  extensions' actions again when they changed.
- After a re-execution init carries only the base's services through its pipe
  and re-derives the extensions' from the records, taking what they cannot
  say from pipes of its own.

### Actions, commands and apps

- privd reads the base's policy and the running extensions' actions, each
  held to its record. It spawns an extension mechanism as the extension's
  first user, from `/opt/<name>`, with `/opt/<name>/bin` after
  `MECHANISM_PATH`, as a service's.
- An extension's action may name one of its own programs as a `program`
  principal, dynamically linked as all of them are, because the kernel turns
  it `Other` the moment it maps code from outside the base and its
  activation; privd judges it by its activation's label. The base's policy
  names base programs alone.
- The loader searches `/lib` and the program's own `/opt/<name>/lib` for an
  `AT_SECURE` extension program, the activation the kernel attests.
- `/opt/bin` holds a link for each shown extension's commands, and the dock
  reads `share/slopos/apps` of every shown extension before `/usr/share`, so
  its entries replace none.

### Configuration

- An extension's defaults ship in its image. `/etc/<name>`, a sealed
  directory of sealed files everyone reads, holds what the machine changed.
- `extctl config edit <name> <file>` is sudoedit's model: the session edits a
  copy, and `org.slopos.extension.config` installs it, then init restarts the
  services whose `reads` name it.
- `extctl config apply <from>` installs one with no question when its sidecar
  `<from>.meta`, naming the extension, the file, a serial above the record's
  last and the content's SHA-256, is signed under `extension-config@slopos` by
  the key the record names.
- Configuration never widens a grant: capabilities come from the record, so a
  configuration only steers what was already granted.

### On-disk images

Memory suits daemons and helpers, not large system software. Phase 5 moves
extensions to disk:

- The image becomes a read-only ext4 volume followed by a SHA-256 tree in the
  layout dm-verity uses. The sidecar's digest becomes the tree's root.
- A read-only block device backed by a file on the root serves it, verifying
  every block against the tree as it is read. This puts ext4 under a block
  device under the VFS under ext4, a new layering the lockdep ratchet will
  see and the plan names as such.
- `/opt/<name>` mounts it read-only and pinned; the kernel builds the
  activation's path index by walking the read-only volume once, for
  `SO_PEERPROGRAM`.
- The CRC-32 trailer stays for root images. Installed in-memory images are
  refused at the cutover and reinstalled, as the base's layout change was.

### Recovery

- `extensions=off`, typed into Limine's entry editor, enabled by default,
  boots with no extension.
- The rescue shell's `extctl --root /mnt/root disable <name>` turns one off on
  an installed disk.
- A reinstall keeping the root keeps the store; the new base's ceiling is
  applied at its first activation.

## Phases

Five phases. Each ends with something that runs, and lands as commits of its
own.

### Phase 1: Activation and identity — done

`ExtensionUpdate`; `ext_activate` and `ext_deactivate` over a pool of 32
`basefs` instances shown read-only and pinned at `/opt/<name>`, replaced in
one step and reaped once unheld; `Program::Extension` and `Program::admits`;
a 64-bit program instance behind `Identity::now`, so `SO_PEERPROGRAM` and the
new `SO_PEEREXTENSION` end with an exec, a foreign mapping or the exit;
`extensions=off` and `ext.memory=`; base interface 5. privd's `root` and
`/run/init`'s uid-0 commands require a base program, read again per request.
`/opt` and `/opt/bin` joined `ROOT_LAYOUT` and its mirrors. Graded by
`fs::tests::extensions`, the extension cases of `syscall::tests_peer`,
`peer_test`'s `an_extension_program_names_its_activation` and
`an_exec_after_a_connect_ends_the_program`, `privd_test`'s
`uid_0_outside_the_base_is_no_root`, and `supervision_test` holding
`/run/init` to `/sbin/init` across a re-execution.

### Phase 2: Format, signatures and the install — done

`extension-core`: the image (a newc archive sealed by `/.extension/manifest`,
records under `bin/`, `libexec/`, `lib/` and `share/` and symlinks that stay
inside it, with `release` and `declare`), the declaration grammar and the
grant it asks for, the sidecar, the record, the keys file, the base ceiling
and `NEVER`, an ELF reader and the symbol check, and the install and
activation verdicts; `boot_core::base::sealed` shares the base's sealed-archive
check. `tls-core` verifies Ed25519 strictly and SSHSIG, interop-tested both
ways against `ssh-keygen -Y`. `/bin/extctl` runs the four
`org.slopos.extension.*` actions with `ExtensionUpdate`, now in `DELEGABLE`,
and `Seal`, over a store of sealed generations; the base's ceiling is
`/usr/share/slopos/privd/extensions`; the kernel keeps the store's
directories where they are (`KEPT_DIRS`) and a seal final against the page
set. `tools/extension`, `just extension DIR` and `remote.py install
--extension` build and push one. Graded by `extension_test` (every case the
phase named, a person's disable against a key's update, a key file's comment,
and the store's seal, hold and generations), `fs::tests::mount`'s kept
directory, `fs::tests::filemap`'s seal against the page set, and host tests
under `just test-host`.

### Phase 3: Services and users — done

Records keep uids (`extension_core::accounts`) and the extension's number,
the uid of its name; `extctl` writes `/etc/passwd` and `/etc/group` from them
after every change and tells init `extensions`; slibc's readers believe only
a sealed database (`slibc/src/pwd.rs`, `slibc_core::passwd`). init runs
`activate-all` at boot and supervises every service (`init/extensions.rs`):
login tags from the number and the service's place, `after` on readiness,
`listen` sockets bound and kept, one service's each, logs through
`/sbin/init --log`, the restart policy within the start budget, session
services with the session, replacement of only what an update or a new
base's ceiling changes, and adoption by tag and pid after a re-execution,
pipes of their own carrying what the records cannot say. The kernel lets an
`SO_REUSEADDR` listener take a port only connections hold, shares a UDP
address only among sockets that all set it, and binds UDP port 0 to an
ephemeral port at once. The loader searches an attested program's
`/opt/<name>/lib`. A declaration has at most 16 services, a user for any
service or directory, a socket per service, and no name `TAKEN` by the
system's paths. Graded by `extension_service_test`, `tcp_live`'s
`test_reuse_addr_listens_beside_time_wait`, `udp_demux`'s and
`socket_framework`'s reuse and port-0 tests, and host tests of the
accounts, the record's uids, the database's text and the declaration's
limits. `getgrouplist` stays with `plans/users-and-login.md`.

### Phase 4: Actions, commands, apps and configuration — done

privd reads the running extensions' actions at its start beside the base's
policy (`slopos_userland::activation`), each held to its record and its
activation, translates their `allow` lines to today's principals
(`Action::for_privd`), and spawns their mechanisms as the extension's first
user with no supplementary group, holding `SetId` and `ProcSignal` beside
`DELEGABLE` to start them and relay their requesters' signals
(`PRIVD_CAPS`); an extension's own program is a principal
(`Principal::Extension`) while it runs that file of the activation its record
installed. Init has privd read them again (`SIGHUP`) when what they were
read from changes. `/opt/bin` links the shown extensions' commands, last on
`DEFAULT_PATH`; the session's `PATH` puts `~/.local/bin` before
`/usr/local/bin`; the dock reads extensions' entries first. `extctl config
edit|install|apply` keeps `/etc/<name>` sealed, the record keeping the last
signed serial (`slopos_extension_core::config`), and init restarts a service
whose `/etc/<name>` reads changed; `extctl --root` reaches a mounted root's
store. Graded by `extension_action_test` (an app in `/usr/local` asking an
unasked action and one that asks, root refused, an extension program's
principal lapsing once it maps foreign code, privd reading an action
change while it keeps serving, the command shadowed by
`/usr/local/bin`, the dock's entries, a configuration edited, refused to the
person with nobody to ask, applied signed, its service restarted each time,
an old serial and another key refused, and `extctl --root` holding every
capability as the rescue shell runs it), and host tests of the translation,
the principal, the sidecar and the record's serial.

### Phase 5: On-disk images

- The SHA-256 tree, the file-backed read-only block device, the mount, the
  path index, the format change; the memory budget removed.
- Graded by an extension larger than the old budget, a flipped bit in its
  image failing the read that meets it, and every test of Phases 1–4 but the
  budget's.

## Grading

- **Gates:** `check_base_interface.sh` for interface 5;
  `check_authority_reachability.sh` for the new calls; the unsafe and
  expansion gates; `check_loose_programs.sh` over the test extensions;
  `check_libc_exports.sh` for the user database's readers.
- **Ratchets:** new locks and tests move lockdep and the test count;
  re-measure and name the cause.
- **By hand, on the laptop:** an extension built and signed on the host,
  installed from the host unasked, updated, rolled back, and turned off from
  the boot menu.

## Beyond this plan

Gaps that keep SlopOS from being a general-purpose system, each a plan of its
own, all building on extensions:

- **Device access from userland**: USB devices, HID and serial nodes for
  programs (devfs serves consoles, `pts`, disks, `crash`, `kmsg`, `null`,
  `zero` and `random`), and user-space drivers.
- **Filesystems in userland** (FUSE).
- **Virtual network devices** (TUN/TAP) for VPNs.
- **Users and login**: more than one person, a login service, remote shells.
  OpenSSH's server, which every toolchain install already builds, is the
  first extension that needs it, with `chroot` for its privilege separation
  (slibc answers `ENOSYS`).
- **Repositories**: signed indexes of extensions and apps, dependencies
  between extensions and shared runtimes, unattended updates.
- **Timers** in init.
- **Session autostart** for apps.
- **Per-app permissions and sandboxing.**

## Decided

- **Extensions, not a bigger base.** Composing system software into the base
  would have reused everything, but couples it to the base's lifecycle and
  memory; a classic root-owned `/opt` with identity by path would have undone
  the identity model. An extension is a small base with a lifecycle of its
  own.
- **Identity is what the kernel serves, while it serves it.** A path is never
  authority, a recorded identity ends with the program it names, and a base
  program, not the kernel, checks signatures.
- **uid 0 is not authority.** `root` is uid 0 running base code; an extension
  never runs as uid 0, its mechanisms included. A connection code outside the
  base makes is never root, but uid-0 code outside the base can still start a
  base program as uid 0, so it is that no extension runs as uid 0 that keeps
  extensions from root.
- **The extension declares; the owner grants; the base bounds.** A first
  install's grant is the line the person read; a signed install's is its
  declaration under the key's ceilings; both sit under the base's ceiling,
  checked again at every activation.
- **Unasked actions are grants to every person**, so they are named in the
  question and bounded by a key's separate `user` ceiling.
- **Approve a signer once, then automate.** Consent is for a first install, a
  new key, a rollback, a removal and a configuration edit; never for an update
  or a configuration signed by the key already trusted.
- **The owner holds the keys.** No vendor key is built in. Keys are SSH keys,
  because every owner has one and git signs with them; the signed object is a
  small sidecar, so it outlives a change of image format.
- **A version floor, and the version left by a rollback refused**, so a signed
  image with a known hole cannot come back unasked.
- **Own users, never uid 0**, with init holding their sockets: a uid owns an
  extension's state, capabilities are its power, ports are held across
  restarts.
- **Compatibility is `libc.so`.** Extensions do not name a base interface;
  their symbols are checked against `libc.so` at every activation. One
  extension does not load another's libraries until repositories bring
  dependencies.
- **Memory first, then disk.** In-memory activation reuses `basefs` and makes
  every layer above testable at once; on-disk images follow and replace the
  in-memory form for extensions.
- **An activation is an instance.** Each is a `basefs` instance of its own
  from a fixed pool, numbered across every activation, so an inode names its
  activation for good, replacement is a mount swap rather than a generation
  switch, and the reaper frees an activation as it frees a retired base
  generation. A program's word tags an extension's inode with its top bit, so
  every generation number stays below 2^31. A replacement takes a vacant
  instance, so with the pool full an extension can be deactivated but not
  updated until one is freed; and a program of the activation replaced that
  opens a library by its `/opt/<name>` path afterwards reaches the new
  activation's and forfeits, as mixing two activations' code must. Any
  process that keeps a file of a replaced activation open keeps that copy
  resident against the shared budget and pool, as with the base; on-disk
  images lift the budget. Like a base directory across a switch, a working
  directory or `dirfd` under `/opt/<name>` follows a replacement to the next
  activation's. A script is attested by the path it was found at
  and reopened there by its interpreter, so across a replacement it can read
  the next activation's bytes under the first's identity, and after a
  deactivation whatever the root holds at that path: privd judges an
  extension's script by its label, and only an `ExtensionUpdate`
  or `DacOverride` holder can stage either.
- **A program instance, never a socket walk.** Every change of a process's
  program moves a 64-bit counter (the lesson of Linux's 32-bit `exec_id`);
  a recorded identity holds its process handle and instance and is judged
  when read. Exec, exit and a foreign mapping touch no socket. Init's
  re-execution of itself keeps its instance, decided before its old image
  goes, so no reader sees its identity lapse in between.
- **A lapse is for good, so read the label first.** A recorded program only
  ever stops holding; a reader asks `SO_PEEREXTENSION` before
  `SO_PEERPROGRAM`, so an extension's identity lapsing between the two reads
  answers no path rather than a path that looks like the base's.
- **`extensions=off` is the kernel's.** The flag refuses `ext_activate`
  itself, not only init's boot step, so recovery holds whatever userland
  does.
- **No authority before its user.** `ExtensionUpdate` exists from Phase 1 and
  init holds it, but joins `DELEGABLE` only with the Phase 2 actions that run
  under it.
- **The kernel keeps the store; the store keeps its seal.** The store,
  `/var/lib/slopos/extensions`, and each directory on the way to it are held
  by the kernel from boot (`slopos_abi::fs::KEPT_DIRS`): no rename or removal
  moves one, and no mount covers one, one above it or anything beneath it —
  `/var` itself among them — so they are held only once the root and the
  `mount=` volumes are mounted, and a `/var` of its own holds the store.
  Only the store and what it holds are sealed, so the rest of `/var` stays
  writable, and nothing unsealed in it is believed: a file there that no
  `Seal` holder sealed is no record and no key. A RAM root keeps a seal for
  good, so extensions need a disk root.
- **Images by digest, records by generation, nothing renamed.** The store
  keeps each image, sidecar and signature under its SHA-256; a record or the
  keys go in as `record.<n>` or `keys.<n>` beside the last, sealed through
  the descriptor that wrote it and read back, and the newest sealed one is
  believed. No rename and no rewrite in place means no moment in which the
  believed record is unsealed, so a writer cut off anywhere — a crash, a
  power loss — leaves the old state or the new, never a store that has
  forgotten an extension's floor. Writers lock a 0600 file beside the store
  and ignore the termination signals privd relays; readers take nothing.
- **A seal is final for readers.** The store rests on it, so the kernel
  refuses to seal a file a writable shared mapping holds and refuses a
  `write(2)` into a sealed file's page set, as it already refused one to the
  filesystem: what a sealer reads back is what every reader reads after.
- **No authority before its user, for files too.** A mechanism reads the
  files a request names as uid 0 without `DacOverride`, so only what uid 0
  may read by mode is installed; the actions take `DacOverride` with the
  phase whose state needs it, and `org.slopos.extension.trust` holds
  `ExtensionUpdate` because a key's change stops what no longer fits.
- **The person reads the grant, the mechanism holds the image to it.** A
  first install's command line is `install <image> <sha256> name <name>
  grant <caps> unasked <actions> users <users> commands <commands>`, lists
  joined by commas and `-` for none; `extctl install <image>` writes it out in
  the requester, the mechanism runs only that form, and it refuses an image
  whose declaration is not that grant, as sets. A key is trusted by `key add
  <fingerprint> ssh-ed25519 <base64> ceiling <caps> user <caps> [comment]`,
  so the question shows its fingerprint; a key file's comment that would not
  read back is dropped. The keys file is shaped like `allowed_signers` but not
  readable by `ssh-keygen`, which refuses an option it does not know.
- **Unasked is what any person reaches unasked.** An action counts toward a
  key's `user` ceiling and the grant's `unasked` list when it allows `user`
  with no level or `alone` — one person logged in, which this system always
  is — `admin` with no level, which that person is, or a `program`, which any
  person may run.
- **A person's word outlasts a key's.** A signed update of an extension the
  person disabled goes in disabled; one the person removed goes back in only
  as a first install. A stop a check made is no person's word, so a fitting
  update runs again. An update that will not activate leaves the record as
  it was and what ran before running.
- **The floor binds every install; the refused list, signed ones.** A version
  below the floor never goes in, the floor's version only with its own bytes;
  a version a rollback left goes in again only as a first install, and
  installing a version clears the refused ones at or below it. A rollback to
  a version refuses it no longer, and an activation refuses a refused
  version. A person's install leaves the record naming no key, so later
  signed updates are first installs.
- **One name, one owner.** No two installed extensions declare one command
  or one user, checked at install and at rollback, since `foo`'s `user bar`
  and `foo-bar`'s own user are both `foo-bar`.
- **Strict Ed25519.** A key must be a canonical point outside the small-order
  subgroup, `S` below the order and `R` the canonical encoding of
  `[S]B - [k]A`, so one message has one signature under one key; every
  signature `ssh-keygen` makes passes.
- **What an image holds and declares.** Its records are under `bin/`,
  `libexec/`, `lib/`, `share/` and `.extension/` alone, and a symlink's
  target is relative and never climbs through `..`, so it stays inside the
  mount. A declaration's settings are the indented lines after a `service`
  or `action`; programs are named by the path they have at `/opt/<name>`; an
  action's exclusive group is the extension's own, `ext.<name>.<group>`;
  `reads` names a path under `/opt/<name>` or `/etc/<name>`, a trailing `/`
  for a directory; a service restarts on failure unless it says otherwise.
  Every ELF file an image holds that a loader maps is dynamically linked; a
  program the declaration names lives outside `lib/`, and every ELF file
  outside `lib/` is a program, loaded through `/lib/ld-slopos.so.1` and
  needing `libc.so`, so a static PIE is refused wherever it could run from;
  each needs nothing but `libc.so` and libraries in the image's `/lib`, found
  through soname links there, and binds only what those export, read from
  its section headers. An image carries git's modes, so it is its tree's and
  not its packer's umask's. SSHSIG blobs carry an empty reserved field.
- **The person is asked only about what would go in.** The requester holds a
  request to the store as it reads it — the image, its grant, the floor, the
  ceilings, the symbols, the clashes, a rollback's fit — before privd asks,
  and the mechanism holds it again with the store locked.

- **A service's login tag is its own**, made from its extension's number
  and its place in the declaration, and a service that moves in its
  declaration is restarted. Session services carry their own too, which the
  session's end ends with the desktop's, so init can end one alone.
- **A re-execution re-derives services from the records and the tags,**
  and hands on in pipes of its own only what neither can say: whether a
  service was meant to stay down or gave up, the pid it was started as,
  its starts, a digest of what it was started as, and its descriptors. The
  base's pipe stays as every image reads it; a running service whose
  digest no longer matches the records is started again, and what carries
  a tag no service claims is ended. An image whose text lacks their header
  gets none, and the services end before init re-executes into it, so none
  runs unsupervised.
- **The user database is a view of the records.** A uid is given once, kept
  through removal and never given again; the files list only installed
  extensions' users and are believed only sealed, and nothing that grants
  takes a uid from them.
- **init holds the port, not the service's process.** A listen socket is
  bound before the first start and kept across restarts, re-executions and
  updates that keep it. TCP's is bound with `SO_REUSEADDR`, which only
  another listener refuses, so the connections a crashed service left in
  `TIME_WAIT` do not keep it from its port; UDP's without it, and a UDP
  address is shared only among sockets that all set it, so no bind takes a
  held port's datagrams. A port is one service's: a declaration names it
  once, and of two extensions that would, the lower number keeps it. Init
  gives its services at most half its descriptors, so no set of extensions
  starves the rest of init.
- **An extension's files are written as its user.** Its log is kept by a
  copy of init running as the extension's first user, never by init as uid 0
  in a directory the extension owns; a directory it gets is made for it or
  already its.
- **The loader asks the kernel which activation a program is of**, through
  `SO_PEEREXTENSION` on a socket pair, never the path it was started by.
- **No extension takes a system path's name**: `TAKEN` covers what
  `/var/log`, `/run` and `/etc` already hold.
- **An update restarts what it changes**: a service's declaration, user,
  grant or tag, or its program, `reads` or the image's `lib/` in the
  manifest; the rest runs on, holding the activation it started from.
- **privd reads the actions again in place; init tells it when.** privd
  reads the running extensions' actions at its start and again on `SIGHUP`,
  which init sends on an `extensions` that changes what they were read from —
  each extension's name, image digest and activation — and a request takes
  them as they stand when it comes. Starting privd again instead, as a base
  switch does, left it off its socket while the request that made the change
  drained, longer than a requester waits under emulation, and the new
  instance refused every exclusive group while the old one lived. An update
  of an extension with actions has privd read them again even when they read
  the same, since its programs are another activation's files.
- **An extension's `allow` lines read as the one person meets them**, until
  privd tells people apart: `user` and `admin` with no level, and `user
  alone`, run for the session's uid unasked; `confirm` and `password` ask the
  person, who has no password to type; the first line a requester matches
  decides, so a later line for the same person is never reached.
- **An extension's program is a principal of its own actions alone**,
  matched by path and by the label of the activation its record installed,
  so a script reopened across a replacement, or the same path in another
  activation, is no match. The base's policy names base programs only: what
  it means cannot depend on what is installed. uid 0 running an extension's
  program is no `root`.
- **A mechanism's `PATH` is a service's**, the base first and the
  extension's own `bin` after it, so what an extension ships shadows no base
  tool even inside its own mechanisms.
- **The question names the extension**: its `describe` is the extension's
  text, so privd shows `(extension <name>)` after it, and the command line
  shows the program's `/opt/<name>` path.
- **An action needs a user**: its mechanism runs as the extension's first
  user, never as uid 0, so an extension that declares an action declares a
  user, and one installed before this rule with an action and no user stops
  at its next activation, saying why.
- **`paired` is unasked too**: privd never asks about the paired agent, so an
  action that allows it is named in a first install's grant and held to a
  key's `user` ceiling, as one any person reaches unasked is; otherwise the
  broker's key would hold what no question named.
- **privd relays signals with `ProcSignal`**: a mechanism running as an
  extension's user is no uid privd may signal by the uid rule, so privd holds
  `ProcSignal`, which no action names, to relay its requester's signals and a
  timeout's; it signals only the mechanisms it started, whose pids are never
  reused.
- **`/etc/<name>` is laid out sealed at activation**: an image that declares
  `config` gets an empty sealed directory before it runs, and one there that
  holds anything unsealed is refused, so nothing a `DacOverride` holder
  planted before the first configuration is sealed in as one.
- **`/opt/bin` is `extctl`'s, and only its links into `/opt`**: it links each
  command of every extension shown after every change, so a disabled,
  stopped or unshown extension's command leaves `PATH` with it, and anything
  else there is left alone and keeps its name. A script command run through
  its link is no attested program, as any script found through a link is
  not.
- **An extension's app entry replaces none.** The dock reads extensions'
  shares first, so the base's, `/usr/local`'s and the person's entries win,
  as its commands come last on `PATH` and as snapd appends its menu entries
  to `XDG_DATA_DIRS`. The person's own `~/.local/bin` comes after the base and
  before `/usr/local/bin` and `/opt/bin` in their session.
- **Configuration is read by everyone and changed only through the seal.**
  `/etc/<name>` is a sealed uid-0 directory of sealed files, one file deep,
  so a `DacOverride` holder without `Seal` cannot steer another extension's
  service; it is world-readable as `/etc` is, so its actions need `Seal` and
  nothing else, and secrets belong in `/var/lib/<name>`. The person's file
  reaches the mechanism as the requester's standard input, by the SHA-256 the
  question shows, never by a path uid 0 opens, which would copy a file only
  uid 0 may read where everyone reads it; a signed one goes in by a sidecar,
  as an image's, whose serial the
  record takes before the file goes in, so a file signed before it never
  goes in after, as TUF's version and AVB's rollback index never fall. Init
  holds what a service reads of `/etc/<name>` in what it started on, so a
  change restarts exactly those services.
- **`--root` is the rescue shell's**: `extctl --root <dir>` acts only on what
  its caller holds, and no privd action takes it, so a session never reaches
  a store other than the running root's.

## Constraints

- `slopos-ostd` remains the only kernel crate with `unsafe`.
- Each new private call bumps `BASE_INTERFACE`; Linux numbers keep Linux
  semantics; the private table grows when a call needs a slot.
- slibc stays `MIT OR Apache-2.0`; the user database readers take nothing
  from glibc.
- Concepts only from systemd, FreeBSD, Android, Fuchsia, Genode and macOS;
  cite documented behaviour, never their files.
- Every phase updates `AGENTS.md` where it changes a stated invariant, and runs
  the security sweep over the paths it touched. Phase 1 states that nothing
  with authority runs code the kernel cannot attest, and rewrites *A peer is
  told who connected, as of when it connected* as *for as long as it runs what
  it ran then*.
