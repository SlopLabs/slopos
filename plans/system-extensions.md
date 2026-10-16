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

**Only the base holds authority.**
- A program gets capabilities two ways: init starts it from its service table
  (`userland/src/apps/init/service.rs`, every entry a base path), or privd runs
  it as an action's mechanism (`assets/privd/actions`, searched on
  `MECHANISM_PATH`, `/bin:/sbin`, `priv-core/src/lib.rs:56`). Every mechanism
  runs as uid 0 (`priv-core/src/policy.rs:328-336`).
- So nothing with authority runs loose code: init's services run from base
  paths, privd's mechanisms from `MECHANISM_PATH`, and the loader keeps
  `/usr/local/lib` from uid 0 and every `AT_SECURE` process.

**uid 0 is still a principal.** privd's `root` is any requester whose uid is 0
(`priv-core/src/policy.rs:150`), and every action allows it without a
question, `base.install` and `rescue` included. `/run/init` takes
`end-session` and `reexec` from uid 0 judged by `SO_PEERCRED`
(`userland/src/apps/init/control.rs:155-158`). This is sound only while every
uid-0 process is base code.

**Identity is base membership.**
- Exec records `Program::Base(inode)` when the image, its `#!` chain and its
  interpreter are files of `BASE_FS`, and `Program::Other` otherwise
  (`core/src/exec/mod.rs:792-797`, `:897-902`). A later executable mapping of
  anything else makes the process `Other` (`mm/src/process_vm.rs`).
- A socket records its peer's identity at `listen`, `connect` or
  `socketpair`, a value nothing later changes
  (`slopos-ostd/src/process/identity.rs`). privd reads `SO_PEERCRED` and
  `SO_PEERPROGRAM` once, at accept, and judges every request on that
  connection by them (`userland/src/apps/privd/mod.rs:183`, `:303-318`).
- privd's `program <path>` principal names only a static program under
  `/bin`, `/sbin` or `/usr/bin` (`priv-core/src/policy.rs:42`, `:475`); a
  dynamically linked one is refused because the loader would take libraries
  from outside the base under the base's identity (`userland/src/apps/privd/
  mod.rs:260-279`).
- remoted reads its pairing only from the base, because "a configuration
  anywhere a program could write would hand any program the authority of
  whatever broker it named" (`userland/src/apps/remoted/mod.rs:16-20`). The
  same holds for anything that grants authority.

**Serving sealed archives is solved, for one archive.** `fs/src/basefs/`
serves a newc archive from memory, with up to sixteen generations
(`MAX_GENERATIONS`, `:42`), every inode sealed, and two static instances,
`BASE_FS` and `MEDIUM_FS` (`:110`, `:114`). Every entry point that attests or
holds is keyed to `BASE_FS`: `is_base` (`:118-120`), `Hold::of` (`:124-128`),
activation (`activate.rs`), the reaper (`reap.rs`) and `program_path`
(`core/src/syscall/net_handlers.rs:725-746`). The mount table holds 32 mounts
(`fs/src/vfs/mount.rs:8`).

**Verity is an integrity check, not an authenticity one.** `fs/src/verity.rs`
verifies a disk against a CRC-32 per block from a trailer (`:1-33`); nothing
verifies a SHA-256 tree, and no kernel crate computes SHA-256. There is no
file-backed block device (`fs/src/blockdev.rs` holds `MemoryBlockDevice`).

**The private call table is nearly full.** `SYSCALL_PRIVATE_TABLE_SIZE` is 48
(`abi/src/syscall/numbers.rs:36`) and offsets 0 through 45 are taken.

**The kernel's command line stays in the kernel.** It is parsed in
`boot/src/early_init.rs:505-611`; userland learns of it only through
`sys_info`'s boot flags.

**Outside the base, software has the session's power only.**
- Apps install into `/usr/local` or `~/.local` as the session, with no
  capability (`tree-core`, `/bin/tree-install`).
- The loader searches `/usr/local/lib` only for a process that is neither
  `AT_SECURE` nor uid 0, and refuses `$ORIGIN` under `AT_SECURE`
  (`slibc/src/ld_so/mod.rs:62`, `:628-633`, `:737-750`).
- The dock reads app entries from `/usr/share`, `/usr/local/share` and
  `~/.local/share` (`userland/src/app_entry.rs:18-27`).

**There are two users.** slibc compiles in root and `user` "until there is
more than one person" (`slibc/src/conf.rs:48-63`); there is no `/etc/passwd`.

**Ports are not privileged.** `Port::is_privileged` (`net/src/types.rs:124`)
gates nothing; any process may bind any port.

**`/run` is uid 0's** (`fs/src/vfs/init.rs:512`): only init makes directories
there.

**Signatures.** `tls-core` has SHA-256, SHA-512, X25519, ECDSA and RSA, and no
Ed25519. OpenSSH's `ssh-keygen` is in every toolchain install.

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

None of them gets the owner's position right for this system. ChromeOS,
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
- **An unasked action**, one that allows `user` with no level, hands its
  capabilities to every person's processes. It counts against a key's
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
| `/var/lib/slopos/extensions/` | the store, every directory down from `/var` sealed, the store pinned against mounts | uid 0 |
| `…/keys` | trusted keys, `allowed_signers`-shaped: key, ceiling, session ceiling, comment | uid 0, sealed |
| `…/<name>/current.ext`, `previous.ext`, their sidecars and signatures | the images | uid 0, sealed |
| `…/<name>/record` | the grant, the floor, the refused versions, the uids, enabled | uid 0, sealed |
| `/opt/<name>` | the active image, read-only and pinned | the kernel |
| `/opt/bin` | links to declared commands | uid 0 |
| `/etc/<name>` | the machine's configuration | uid 0, readable by the extension's users |
| `/var/lib/<name>`, `/var/log/<name>` | state and logs | the extension's first user |
| `/run/<name>` | runtime directory and sockets | the extension's first user, made by init |

- `/opt` and `/opt/bin` join `ROOT_LAYOUT` and its mirrors, uid 0, 0755.
- `DEFAULT_PATH` becomes `/bin:/sbin:/usr/local/bin:/opt/bin`: the base first,
  then the system-wide software tree, then what the owner granted; a person's
  `~/.local/bin` comes before them in their session. A granted command cannot
  shadow the toolchain; two extensions declaring one command are refused at
  install.
- The seal keeps a `DacOverride` holder from rewriting a record without also
  holding `Seal`; sealing the store's directories keeps it from renaming the
  store aside; pinning keeps a `Mount` holder from covering it.
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
  both, and `SO_PEERCRED`, before every verdict.
- **`BOOT_FLAG_EXTENSIONS_OFF`** in `sys_info`, set by `extensions=off` on the
  command line.
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
beside `DELEGABLE` to spawn them so, and delegates neither.

**Actions.**

| Action | Command | Capabilities | Allow |
|---|---|---|---|
| `org.slopos.extension.install` | `install *` | `ExtensionUpdate Seal DacOverride` | `root`; `admin confirm`; `paired` |
| `org.slopos.extension.install-signed` | `install-signed *` | `ExtensionUpdate Seal DacOverride` | `root`; `user`; `paired` |
| `org.slopos.extension.trust` | `key add *`, `key remove *` | `Seal` | `root`; `admin confirm`; `paired` |
| `org.slopos.extension.manage` | `enable *`, `disable *`, `remove *`, `rollback *` | `ExtensionUpdate Seal DacOverride` | `root`; `admin confirm`; `paired` |
| `org.slopos.extension.config` | `config *` | `Seal DacOverride` | `root`; `admin confirm`; `paired` |

- **A first install** is `extctl install <image> <sha256> name <name> grant
  <caps> unasked <actions> users <names> commands <names>`. The person reads
  exactly what is granted; the mechanism refuses an image whose `release` or
  `declare` says anything else. It fits privd's 320-byte question for any
  extension a person could judge in one; a longer one is refused there.
- **A signed install** names no grant: the mechanism refuses unless the
  sidecar is signed by a trusted key whose ceilings cover the declaration, and
  the version is allowed.
- **A key** is trusted by `extctl key add <ssh-ed25519 key> ceiling <caps>
  user <caps>`; the question shows its fingerprint and both ceilings. Where
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
  `ExtensionUpdate`, before it starts privd and unless
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
- `session` services start with each person's desktop session, under its
  tag, as that person and holding their grant, and end with it.
- **A live change** reaches init as `/run/init`'s `extensions` command, from
  the mechanism, which is base code running as uid 0. init reloads the
  records, restarts the services whose program or `reads` changed
  (`boot_core::base::changed` over the two manifests), stops what was
  disabled or removed, and restarts privd when an extension's actions
  changed.
- After a re-execution init carries only the base's services through its pipe
  and re-derives the extensions' from the records.

### Actions, commands and apps

- privd reads the base's policy and the enabled extensions' actions, each
  held to its record. It spawns an extension mechanism as the extension's
  first user, from `/opt/<name>`, with `MECHANISM_PATH` led by
  `/opt/<name>/bin`.
- A `program <path>` principal may name an extension program. Both of today's
  refusals change: `PROGRAM_DIRS` admits `/opt/<name>/`, and a dynamically
  linked principal is admitted when it is an extension's, because the kernel
  turns it `Other` the moment it maps code from outside the base and its
  activation.
- The loader searches `/lib` and the program's own `/opt/<name>/lib` for an
  `AT_SECURE` extension program, the second derived from its path, which only
  its activation's mount can hold.
- `/opt/bin` holds a link for each enabled extension's commands, and the dock
  reads `share/slopos/apps` of every enabled extension after
  `/usr/local/share`.

### Configuration

- An extension's defaults ship in its image. `/etc/<name>` holds what the
  machine changed.
- `extctl config edit <name> <file>` is sudoedit's model: the session edits a
  copy, and `org.slopos.extension.config` installs it, then restarts the
  services whose `reads` name it.
- `extctl config apply <name> <file> <sig>` installs one with no question when
  signed, under `extension-config@slopos`, by the key the record names, over
  the extension's name, the file's path, a serial above the record's last and
  the content.
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

### Phase 1: Activation and identity

- `ExtensionUpdate`; `ext_activate` and `ext_deactivate`; `basefs`
  generalised to a pool of instances; `/opt/<name>` mounts and the mount
  table; `Program::Extension`; identity that lasts while the program does;
  `SO_PEEREXTENSION`; `BOOT_FLAG_EXTENSIONS_OFF`; the memory budget; base
  interface 5.
- `root` and `/run/init` require a base program.
- `/opt` and `/opt/bin` in `ROOT_LAYOUT` and its mirrors.
- Graded by kernel tests: an activation served and attested; a replacement
  switching the mount in one step; a library mapped from elsewhere turning
  the process and its sockets `Other`; an exec after a connect turning the
  socket `Other`; the budget refusing; an old activation reaped once unheld;
  and a utest in which a uid-0 program outside the base is refused as `root`.

### Phase 2: Format, signatures and the install

- `extension-core`: the format, the name grammar, the declaration grammar,
  the record, the keys file, the ceilings, the symbol check.
- Ed25519 and SSHSIG verification in `tls-core`, beside its other primitives,
  tested against `ssh-keygen -Y sign` output.
- `/bin/extctl` and the actions in the table; the base ceiling file; the
  sealed, pinned store.
- `tools/extension`, `just extension DIR`, and `remote.py install
  --extension`.
- Graded by `extension_test`: a first install asks and grants what it read;
  one declaring more than its grant line is refused; a signed one installs
  unasked under a trusted key; one past either ceiling is refused; an update
  by the same key goes in unasked and an older version is refused; a rollback
  refuses the version it left; a removed key, or a lowered ceiling, stops its
  extension; one needing an export `libc.so` lacks is refused; a reserved name
  is refused.

### Phase 3: Services and users

- Extension users in the user database, which this phase creates with
  slibc's readers if `plans/users-and-login.md` has not; uid allocation.
- init's boot step, extension services, `after`, readiness, logs, `listen`
  sockets, restarts, session services, live changes, `extensions=off`, the
  re-derivation after re-exec.
- The loader's search for `AT_SECURE` extension programs.
- Graded by a test extension whose daemon runs as its own user, holds exactly
  its grant, takes its socket from init, logs, restarts after a crash with its
  port held, survives a session restart, and is restarted by an update that
  changes it and not by one that does not.

### Phase 4: Actions, commands, apps and configuration

- privd's extension actions, spawned as the extension's user, for which init
  gives privd `SetId` beside `DELEGABLE`; extension principals; `/opt/bin`,
  `DEFAULT_PATH`, the dock; `extctl config` edit and apply.
- Graded by an app in `/usr/local` asking its extension's action with
  consent and an unasked one without; a principal naming an extension program
  refused once that program maps foreign code; privd restarted by an action
  change; a command found on `PATH` after the session's; a configuration
  edited, applied and its service restarted; a signed configuration with an
  old serial refused; and `extctl --root` from the rescue shell.

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
  never runs as uid 0, its mechanisms included.
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
