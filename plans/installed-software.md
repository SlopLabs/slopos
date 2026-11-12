# One form for installed software

## The goal

Software reaches a SlopOS machine in one form. What someone distributes is
an image, verified on every read, shown at `/opt/<name>`, with a version
and a previous image to go back to. What a person builds themselves is a
file in their home. `/usr/local`, `tree-install` and everything that guards
a shared directory a person can write go away.

| Place | Holds | Changed by |
|-------|-------|------------|
| The base | The system | `bootctl`, whole |
| `/opt/<name>` | Every installed program: extensions, the apps outside the base, the toolchain | `extctl`, one image each |
| `~/.local` | A person's own builds | That person, with `cp` |

An extension is already this image. The work is to make it the only form.

## What stands today

Software outside the base has two forms. An extension is one ext4 image
with a verity tree, installed by `extctl`. A tree is files copied into
`/usr/local` or `~/.local` by `tree-install`, with a manifest of what was
copied; two exist on every root, `usr_local` (the toolchain) and `apps`.

`/usr/local` is a directory the first person owns that is on everyone's
search path, and the tree guards it in five places: the hand-over to uid 0
when a second person is added (`root_layout::share`), the boot mark `priv`
reads before it takes a command from there (`root_layout::settled`), the
loader leaving `/usr/local/lib` out for a process that is `AT_SECURE` or
uid 0's (`slibc/src/ld_so/mod.rs`), privd naming no app from there, and
the staged copy behind `org.slopos.software.install`.

## Measured

At `de13e59f`, 2026-11-12, with the toolchain `just toolchain` built on
2026-10-03, whose std stamp is the one this tree pins. QEMU with KVM, 4
CPUs, 8 GiB, the release kernel, the toolchain packed as an image named
`devtools` and installed by `extctl install` through the paired agent.

| Step | Result |
|------|--------|
| Packing `builddir/slopos-toolchain/install` as it is built | Refused, four ways (below) |
| Packing it after those four are met | 936 MB, 11 292 entries, 30 s on the host. All 64 programs pass `xtask programs` and the image's symbol check |
| `extctl install` | 55 s; 3 min 38 s on the debug kernel. No capability, no user, one activation |
| `extctl enable` of the installed image, which is what every boot's `activate-all` does | 7.9 s |
| `rustc`, `cargo`, `clang`, `git`, `cmake`, `ninja`, `bash`, `ssh`, `ld.lld`, `mke2fs` started from `/opt/devtools` | All start, each `AT_SECURE` |
| `rustc hello.rs`, `cc hello.c` | Build and run, once the linker is given `/lib` for `libc.so` and clang the moved headers |
| `cargo build` of a workspace with a build script and a procedural macro | 8 s; the program runs |
| The tree at `HEAD`: `build_kernel.sh` (dev, `KERNEL_SAFESTACK=0`), then `build_userland.sh` | 70 s and 225 s, both exit 0, with `-Zbuild-std` reading std's sources from the image |
| `build_kernel.sh` with SafeStack | Stops: it writes into the read-only sysroot |
| `build_initramfs.sh` | Stops: no one directory holds both `sbin/mke2fs` and `share/licenses/e2fsprogs` |
| `git --exec-path` | `/usr/local/libexec/git-core`, which does not exist |

Not run: the tests variant (the C++ runtime under CMake and Ninja), a C++
compile, git or ssh against a server, and a boot with a cold cache.

## What an image refuses of today's toolchain

1. **Top-level directories.** An image holds `bin`, `libexec`, `lib` and
   `share` (`extension-core/src/image.rs`, `TOP`). The install has `etc`
   (ssh's and e2fsprogs' configuration), `include` (the C and C++ headers,
   1 454 files) and `sbin` (e2fsprogs, `sshd`).
2. **Links that climb.** A link's target has no `..` component. Nine do:
   `bin/ld.lld` and `bin/llvm-ar` into `lib/rustlib`, and git's five names
   and `scalar` in `bin/` into `libexec/git-core`.
3. **Commands.** A declared command is a regular file in `bin/` named in
   lowercase letters, digits and `-_.`. `cc`, `clang`, `ld.lld` and `git`
   are links, and `c++` and `clang++` are not such names.
4. **The link-time C library.** `lib/libc.so` is judged as a library of the
   image and refused: it binds `main`, which nothing it needs exports.
5. **A sysroot nobody writes.** `scripts/build_kernel.sh` makes
   `lib/rustlib/x86_64-slos/lib` and writes the empty SafeStack archive
   there on every run.
6. **The prefix compiled in.** `GUEST_PREFIX=/usr/local`
   (`scripts/build_recipes.sh`) is configured into git, OpenSSH, bash and
   e2fsprogs. The base's own `/sbin/mke2fs` carries
   `/usr/local/etc/mke2fs.conf` for the same reason; the installer points
   it elsewhere (`E2FSPROGS_PROFILE`).

Everything else passed: every library a program needs is in the top level
of `lib/`, which the loader searches as `/opt/<name>/lib` once the kernel
says the program is that image's; a procedural macro is opened by its path
and needs `libc.so` alone; a build script is a file of the target
directory, so it is not `AT_SECURE` and keeps cargo's library path.

## Decided here

- **Every program of an image stays `AT_SECURE`.** It is what makes an
  image's identity mean something (below). The toolchain builds the tree
  under it. What it costs: `LD_DEBUG` is ignored (`toolchain_test`'s first
  rung reads it), and the programs start not dumpable.
- **One image per piece of software.** Its publisher puts programs that
  ship together into one image, which already holds any number of programs
  and app entries; a bundle is a word for such an image, not a kind of
  one. SlopOS ships two: `slopos-toolchain`, and `slopos-essentials`, the
  five apps under `apps/`. A name is one per machine (it is the directory
  under `/opt`, the store's key and what an update is matched by), so
  SlopOS's own carry its prefix. Neither name is reserved.
- **A program is linked against the running base's `/lib/libc.so`.** The
  image carries no copy. What is built on a machine runs on that machine's
  base and, since `libc.so` only grows, on every later one.
- **Installing stays an administrator's.** The first person is one, and a
  trusted key's image goes in unasked. An install by someone who is not is
  outside this plan (below).
- **An image is its name and its signer** (phase 5), as the systems that
  grew large settled it (below).
- **The word extension goes** (phase 7), since nothing installed is an
  extension of anything once an editor and a compiler are images.

## What later security rests on

An image is where privacy and confinement can attach later, because four
things already hold, and every phase keeps them:

1. **The bytes are the publisher's.** Every block is held to the image's
   root as it is read.
2. **A process knows, and can prove, which image it is of.** The kernel
   labels a program started from an activation, and a peer reads the label
   (`SO_PEEREXTENSION`).
3. **Only that image's code runs under the label.** The loader takes no
   library from the starter's environment (`AT_SECURE`), and mapping other
   code executable withdraws the label
   (`core/src/syscall/memory_handlers.rs`). Dropping `AT_SECURE` for images
   that hold no grant would leave a label anyone can run their own code
   under.
4. **What an image may hold is a grant its owner approved**, written in
   its declaration and held to at every activation.

What a grant can say grows later (the network, a person's files, a
device); the four do not have to change for it.

## What identifies an image

Three things name an image today, and each is used for something else:

| Identifier | Says | Used for |
|------------|------|----------|
| The root of its hash tree | These exact bytes | The store's file name, `ext_activate`, the label a peer reads (name and root) |
| Its name | One image per machine | `/opt/<name>`, the record, its uid, `/var/lib/<name>`, `/etc/<name>` |
| The fingerprint of the key that signed it, kept in the record | Who may update it unasked | `install-signed` |

The name alone carries what an image owns. An administrator's approval
installs any image over one of the same name, whoever signed the first:
`admit::install` holds a person's install to no signer, the record's
signer is cleared, and the uid, the state and the settings pass to the
newcomer. The question the administrator answers names the image and its
grant, not that it replaces another publisher's. The label a peer reads
names no publisher either.

How systems that grew large settled it:

| System | Identity | A different signer under the same name |
|--------|----------|----------------------------------------|
| Android | Package name and signing certificate; a new key is vouched for by the old one | Refused. The app is removed first, with its data |
| Windows (MSIX) | Name and a 13-character hash of the publisher, as one family name | Another package, installed beside the first |
| Chrome extensions | An id that is the hash of the publisher's public key | Another extension |
| Apple | Team id and bundle id, as a requirement on the signature | Not the same code |
| Snap | An id the store assigns, bound to a name and a publisher by a signed statement | Needs the store |
| Fuchsia, OCI | The content's hash, with the repository's keys vouching for it | Not applicable: the hash is the identity |
| The Update Framework | A repository's root delegates name patterns to keys | Refused: the name is not that key's |

None uses a random unique id as the identity. Where an id exists it is
derived from the publisher's key, so nobody can claim it without the key
and no registry has to hand it out; Snap's is the exception and needs its
store.

So an image's identity is its name and the key that signed it, and an
image nobody signed belongs to the machine. Phase 5 builds that. A short
id derived from the key, a successor key vouched for by the old one, and
name prefixes bound to keys are left for later (below); none of them
changes what phase 5 stores.

## Phase 1: the toolchain is built as an image

**For:** `just toolchain` to end in one file a machine installs, in place
of a tree only an image builder can lay down.

Build, in `scripts/bootstrap_slopos_toolchain.sh` and
`scripts/build_recipes.sh`:

1. The image's layout. Headers go to `share/include`, named by the clang
   configuration files beside `--sysroot` (`-isystem
   <CFGDIR>/../share/include`), configuration to `share/etc` through each
   recipe's `sysconfdir`, and e2fsprogs and `sshd` to `libexec/`.
2. No link climbs. Either the nine become copies, or the rule admits a
   `..` that resolves inside the image, which changes `extension-core` and
   the kernel's index together.
3. How 40 names in `bin/` reach the search path. Either each is a declared
   command, which needs `+` in a command word and a link accepted as a
   command, or one declaration takes all of an image's `bin/`.
4. No `libc.so` and no `libc.a` in the image, and `-L/lib` in the clang
   configuration. `crt0.o` and the headers stay.
5. The SafeStack archive shipped in the image, and `build_kernel.sh`
   writing it only where it is missing.
6. `GUEST_PREFIX=/opt/slopos-toolchain`.
7. One identity over every pin (`toolchain/PIN`, `compiler/PIN`,
   `cxx/PIN`, `crates/PIN`, the recipes, `rustc_build_settings.sh`), kept
   in the image and shown as its release's `display`. Only std carries a
   stamp today, covering `toolchain/PIN`, `toolchain/rust/` and
   `toolchain/libc/`. `build_userland.sh` and `just setup` hold the tree
   to it and name the one the tree needs. The release's `version`, which
   only rises, is the time of the build.
8. `tools/extension` run as the last step, so a toolchain the checks refuse
   never leaves the host.
9. `build_initramfs.sh` taking the base's recipe programs and their
   licence texts from two directories, since the image has no one prefix
   that holds both.
10. `toolchain_test` holding the installed image by its root, in place of
    the manifest at `/var/lib/slopos/trees/usr_local`.

Find out first: a C++ compile with libc++'s headers under `share/include`;
the tests variant in the guest; a SafeStack kernel; `git clone` over HTTPS
and `ssh` with the new prefix; a CMake project, since `cmake --version`
shows only that it starts.

Done when `just test-toolchain` and `just test-selfhost` pass on a root
whose toolchain is the image.

## Phase 2: an image reaches a root without a person

**For:** the host's image builder, the installer and the install medium to
put the toolchain and the apps on a root through the store, as `extctl`
does, with nobody there to approve.

No host script writes a store, the installer has no path into one
(`extctl --root` offers `status`, `enable` and `disable`), and init
installs the medium's apps as a tree.

Build:

1. A directory on the root that only uid 0 writes, holding images offered
   to the machine. Before `activate-all`, init runs `extctl` holding
   `ExtensionUpdate` and `Seal`, as it runs `activate-all`
   (`userland/src/apps/init/mod.rs`), to install each offered image above
   its record's floor. This path installs only an image that asks for
   nothing but commands and apps, so it can never hand out a capability.
2. `scripts/build_fs_image.sh` offering the `slopos-toolchain` and
   `slopos-essentials` images there, in place of the two host trees. A
   host build that changed offers a higher version; one the guest
   installed since stays, by its floor.
3. The installer copying the medium's images into the new root's offered
   directory (`installer/root.rs`, `fill`), and
   `scripts/build_install_medium.sh` carrying image files in place of the
   `apps` tree and the payload's `usr/local`.
4. The live boot. A store needs a seal taken off to add a generation, and
   a root in memory keeps a seal for good (`fs/src/ramfs/mod.rs`); the
   accounts store refuses such a root outright. Either ramfs lets a `Seal`
   holder clear one as ext4 does, or the live boot shows the apps' image
   straight from the medium, which the kernel accepts (`ext_activate`
   takes any readable file) and userland does not yet, since the dock
   reads the store's names.
5. A cheap boot. Every `activate-all` reads each ELF of each image whole
   to hold it to `libc.so`, 7.9 s for the toolchain. The record keeps the
   digest of the `libc.so` an image was last held to, and the check runs
   again only when the base's differs.

Find out first: which of the two live-boot answers is smaller, and whether
`priv` accepts a command reached through `/opt/bin` (`trusted_within`,
`userland/src/apps/priv.rs`).

Done when `just test-installer` installs a disk whose toolchain and apps
are images, and `just boot-live` shows the apps.

## Phase 3: the apps SlopOS ships are one image

**For:** the five apps under `apps/` to be installed, replaced and rolled
back as the image `slopos-essentials`.

They are built the way an image needs: `bin/` and `share/` alone, dynamic,
`libc.so` their one library, no `syscall` instruction.

Build:

1. `scripts/build_userland.sh` staging `builddir/apps` with a release and a
   declaration (four apps, five commands), its entries naming
   `/opt/slopos-essentials/bin/…`, and packing it.
2. The editor finding its manual in its own image; it looks in
   `~/.local/share`, `/usr/local/share` and `/usr/share`
   (`apps/src/editor/files.rs`).
3. `just install` installing the image with `extctl install`. Changed
   bytes need a higher version, so the build stamps one; an administrator
   is asked each time, as for a base.
4. The tests that name `/usr/local/bin/<app>`: `apps_test`, `dock_test`,
   `privd_test`, `extension_action_test`.

What changes for a person: the apps start `AT_SECURE`, and privd names one
that asks as the image `slopos-essentials` where it names none today.

Done when `apps_test` and `dock_test` pass with no `apps` tree on the root.

## Phase 4: `/usr/local` goes

**For:** no shared directory a person can write to be left on anyone's
search path.

Goes, with nothing in its place:

- `tree-install`, `tree-core`, `userland/src/tree.rs` (its four file
  helpers move to the installer, which imports them), the privd action
  `org.slopos.software.install` and its staging directories.
- In `root_layout.rs`: `LOCAL`, `settled` and its mark, the hand-over mark,
  `take_local` and the checks under it. In init: `hand_over`, `settle`,
  `install_live_apps`. In `users add`: the hand-over and the restart
  advice. In `priv`: the clause on `settled`.
- `/usr/local/bin` from `DEFAULT_PATH` and `search_path`, `/usr/local` from
  `ROOT_LAYOUT` and `PERSON_TREES` (`abi/src/fs.rs`) and their mirrors
  (`scripts/lib/root_layout.sh`, `tools/initramfs`), `LOCAL_DIR` from the
  loader, `/usr/local/share` from the dock's shares.
- The tests of all of it: `tree_install_test`, `apps_test --trees`, the
  hand-over cases of `dac_test`, the `/usr/local/lib` cases of `dl_test`,
  `tree-core`'s host tests, the action's cases in `priv-core`.
- In `KNOWN_ISSUES.md`, *Installing for everyone through privd is bounded*;
  in `CVSS.md`, the findings that rest on the hand-over and the staged
  copy, each re-read before it is deleted.

Needs something in its place:

- **One person or several.** "The owners record holds `/usr/local`" also
  means "the machine has one person", which decides whether the first home
  is open to others (`root_layout.rs`, `scripts/build_fs_image.sh`).
  Closing the first home at the second `users add` stays, on a fact of its
  own.
- **Shell completions.** The shell reads
  `/usr/local/share/shell/completions` and the base's. An image's
  `share/shell/completions` takes the first one's place.
- **`scripts/fs_tree.py`** stays for the host trees that are not
  `/usr/local`: the ladder fixtures, a planted pairing.

What a person loses is `tree-install` into `~/.local`, which remembered
what it copied. `cp` is what remains, and `extctl build` packs a directory
for everyone with no tool and no capability. `~/.cargo/bin` is on no
search path today; decide whether it joins.

Neither `DEFAULT_PATH` nor `ROOT_LAYOUT` is in the base-interface gate's
roots, so none of this takes an interface number.

Done when nothing in the tree, its tests or `plans/` names `/usr/local`,
and a note for each of the 25 docs pages that do is in the skills'
`pending.md`.

## Phase 5: an image is its name and its signer

**For:** nothing to take over what an image owns by using its name.

Build:

1. `admit::install` holding every install to the record's signer, a
   person's too. An image under another key, or a signed one over one
   nobody signed, or the reverse, is refused as an update and named as
   what it is: another publisher's image of that name.
2. A removal that gives up everything the name owned: the record with its
   floor, the uid, `/var/lib/<name>`, `/etc/<name>` and `/var/log/<name>`.
   `extctl remove` today keeps the record, the floor and the uid. Only
   after it does another signer's image go in, as a first install.
3. The question privd shows naming who signed the image, by the key's
   fingerprint, or saying that nobody did.
4. The cases in `extension_test`: a key's image over a person's, a
   person's over a key's, a second key's over a first's, each refused
   until the removal, and each then starting with no state.

What stays as it is: an image nobody signed replaces another nobody
signed, by an administrator's approval, which is how `just install`
replaces the apps the install medium offered.

Done when those cases pass and *An administrator's approval replaces an
image with another publisher's* is gone from `KNOWN_ISSUES.md`.

## Phase 6: as many images as a machine holds

**For:** one image per piece of software to be possible, which a pool of
32 is not.

The numbers were sized for a few system add-ons:

- `MAX_ACTIVATIONS = 32` (`abi/src/extension.rs`) sizes a static pool
  (`fs/src/extension/mod.rs`, `POOL`), searched from the front on every
  exec and every executable mapping (`is_activation`). A replaced image
  holds its slot until nothing uses it, so with 32 in use none can be
  updated.
- `MAX_MOUNTS = 32 + MAX_ACTIVATIONS` sizes the mount table and two arrays
  on the kernel stack (`fs/src/vfs/mount.rs`, `fs/src/vfs/ops.rs`), which
  the 2 KiB frame rule bounds.
- Every image name ever installed takes one of 900 uids for good, whether
  or not it declares a user (`extension-core/src/accounts.rs`).
- Each activation walks its whole volume into a kernel index at
  `ext_activate`, and holds a block cache of its own.

Build:

1. The pool and the mount table grown from the kernel heap as images are
   shown, with no array of either on a stack, and an activation found from
   the number its inodes carry, not by a search.
2. An image that declares no user taking no uid, and a uid given back when
   an image's record is removed for good.
3. A bound that is a budget of memory, as base generations have one, in
   place of a count.
4. A measurement of what one more image costs at boot and at rest, with a
   hundred small ones shown, and a cap on it in the suite.

Find out first: whether an image is better shown at its first use than at
boot. Showing all at boot is what a hundred images pay for; showing on
demand moves the cost to the first program started.

Done when a test shows more images than 32, updates one of them, and the
measured cost of each is in the plan's place in the docs.

## Phase 7: the word extension goes

**For:** the names to say what the thing is.

It is in the crates (`extension-core`, `fs/src/extension`,
`tools/extension`), the tool (`extctl`), the calls (`ext_activate`,
`ext_deactivate`, `SO_PEEREXTENSION`), the capability (`ExtensionUpdate`),
the store (`/var/lib/slopos/extensions`), the privd actions
(`org.slopos.extension.*`), the image itself (`/.extension/`, the `.ext`
suffix, the signature's namespace `extension@slopos`), the boot option
(`extensions=off`), a skill and the docs.

Decide the word first: *image* is what the format is, and already names
the base, a disk and an ISO; *package* is what a person installs.

The calls and the capability are in the private call surface, so the
rename takes a `BASE_INTERFACE` number, and a store under the old path is
not read. Before v1 neither is kept for.

Done when nothing in the tree, the docs or the skills says extension for
an installed image.

## Order

Phase 1 changes the cross-built toolchain, so it shares the rebuild that
`self-hosting-on-hardware.md`'s phase 2 needs. Phase 2 needs phase
1's image and phase 3's. Phase 3 needs no toolchain and can come first.
Phase 4 removes what the first three stopped using. Phase 5 depends on
none of them and is small; it can come first. Phase 6 is needed before
software from elsewhere arrives one image at a time, not before. Phase 7
comes last, so that no other phase is written against names that move
under it.

## Later

- **A front end.** Nothing fetches an image. A sidecar already names an
  image, its version and its root, and `extctl install-signed` already
  checks the signature, the key's ceilings and the floor, so an index is a
  signed list of sidecars with an address and a serial that only rises. The
  program that reads it needs the network and `extctl`, holds no
  capability, and can itself be an image.
- **More of an image's identity.** A short id derived from the signing
  key, as Chrome and Windows derive theirs, shown beside the name wherever
  a person approves and carried where a peer learns who it talks to. A
  successor key vouched for by the old one, as Android's lineage does, so
  a publisher can change keys and keep the name. A prefix of names bound
  to a key in the front end's index, as The Update Framework's
  delegations bind them, since nothing says who may use a name before it
  is installed.
- **An install by someone who is not an administrator.** A key's ceiling
  bounds capabilities only, so no policy line says "an image that asks for
  nothing". It needs an action of its own whose mechanism refuses users,
  services, actions and directories, and limits a person can reach today
  without any: the store is charged to nobody, and a command name belongs
  to the first image that declares it.
- **Confinement.** A program of an image that holds no grant runs with
  everything the person who started it may reach. Narrowing that by the
  image's grant is where privacy starts, and it changes what every
  existing image may do, so it is cheaper the earlier it is decided.
- **Recipes that build images.** A recipe names a tarball, a template and
  arguments, all into one shared prefix. An image per recipe needs a
  prefix per recipe and each image carrying the libraries it links, and it
  waits on `self-build.md`: `make`, `meson` and `perl` do not run on
  SlopOS.
