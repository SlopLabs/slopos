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
with a verity tree, installed by `extctl`; the toolchain is one,
`slopos-toolchain`. A tree is files copied into `/usr/local` or `~/.local`
by `tree-install`, with a manifest of what was copied: `apps` on every
root, and `usr_local`, the toolchain image's tree, on a root the install
medium's payload filled.

`/usr/local` is a directory the first person owns that is on everyone's
search path, and the tree guards it in five places: the hand-over to uid 0
when a second person is added (`root_layout::share`), the boot mark `priv`
reads before it takes a command from there (`root_layout::settled`), the
loader leaving `/usr/local/lib` out for a process that is `AT_SECURE` or
uid 0's (`slibc/src/ld_so/mod.rs`), privd naming no app from there, and
the staged copy behind `org.slopos.software.install`.

## Decided here

- **Every program of an image stays `AT_SECURE`.** It is what makes an
  image's identity mean something (below). The toolchain builds the tree
  under it. What it costs: `LD_DEBUG` is ignored, and the programs start
  not dumpable.
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
- **An image is its name and its signer** (phase 6), as the systems that
  grew large settled it (below).
- **The word extension goes** (phase 8), since nothing installed is an
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
image nobody signed belongs to the machine. Phase 6 builds that. A short
id derived from the key, a successor key vouched for by the old one, and
name prefixes bound to keys are left for later (below); none of them
changes what phase 6 stores.

## Phase 1: the toolchain is built as an image — done

`just toolchain` ends in `slopos-toolchain.ext`, held to every check an
install makes. Until phase 3 a root carries it as a file at
`/var/lib/slopos/toolchain` that a test or a person installs with
`extctl`, and the install medium's payload lays its tree at `/usr/local`.

## Phase 2: an image reads as fast as the root — done

A fault on an image's file had cost 4.6 times one on the root's, each block
read through the root's filesystem one `read` at a time under a mutex per
image. The verifying device now reads a sealed store file straight off the
root's device by the extents mapped at activation, a run per request, and
holds no lock across a read: the levels above the leaves are kept from open,
64 leaves beside them, and a run's leaf is looked up once per 128 blocks. An
activation plans a fault's read under its lock, reads off it and keeps
nothing, the file map being the one cache, and answers lookups and `stat`
from its index. A fault on an executable mapping reads the aligned window
around its page, on every filesystem. Hashing was then most of the cost,
unoptimized in the dev kernel: `verity-core` is built optimized, and two
blocks hashed interleaved gained nothing without the SHA instructions.
composefs's shape was not taken: per-file verity in ext4 and a new store
format, for what the device reaches without either.

`toolchain_test` on the dev tests kernel, the image against a `/usr/local`
tree, means of two and three runs: `rustc --version` (cold) 910 and 1197 ms,
`rustc hello.rs` 3010 and 3566, `cargo build` 3419 and 3571, cmake's
configure 2554 and 2704, `cargo clippy` 2158 and 2303, `bash` 2005 and 1944.
`cargo fmt`, 985 and 849, is the one rung past 10%, inside the tree's own
spread of 697 to 970. `fsperf` holds `FSIMG`: one file faulted in cold from
an image and from the root it is a file of.

## Phase 3: an image reaches a root without a person

**For:** the host's image builder, the installer and the install medium to
put the toolchain and the apps on a root through the store, as `extctl`
does, with nobody there to approve.

No host script writes a store, the installer has no path into one
(`extctl --root` offers `status`, `enable` and `disable`), and init
installs the medium's apps as a tree. A test installs the toolchain the
host put on a root (`selfhost::toolchain`); a person does it by hand.

Build:

1. A directory on the root that only uid 0 writes, holding images offered
   to the machine. Before `activate-all`, init runs `extctl` holding
   `ExtensionUpdate` and `Seal`, as it runs `activate-all`
   (`userland/src/apps/init/mod.rs`), to install each offered image above
   its record's floor. This path installs only an image that asks for
   nothing but commands and apps, so it can never hand out a capability.
2. `scripts/build_fs_image.sh` offering the `slopos-toolchain` and
   `slopos-essentials` images there, in place of the image file at
   `/var/lib/slopos/toolchain` and the `apps` tree. A host build that
   changed offers a higher version; one the guest installed since stays,
   by its floor.
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

## Phase 4: the apps SlopOS ships are one image

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

## Phase 5: `/usr/local` goes

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

## Phase 6: an image is its name and its signer

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

## Phase 7: as many images as a machine holds

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
  `ext_activate`.

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

## Phase 8: the word extension goes

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

Phase 3 needs phase 4's image, and phase 4 can come before it. Phase 5
removes what phases 1, 3 and 4 stopped using. Phase 6 depends on none of
them and is small; it can come early. Phase 7 is needed before software
from elsewhere arrives one image at a time, not before. Phase 8 comes last,
so that no other phase is written against names that move under it.

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
