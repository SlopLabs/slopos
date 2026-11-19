# One form for installed software

## The goal

Software reaches a SlopOS machine in one form. What someone distributes is
a package: an image, verified on every read, shown at `/opt/<name>`, with a
version and a previous image to go back to. What a person builds themselves is a
file in their home, and no directory a person can write is shared.

| Place | Holds | Changed by |
|-------|-------|------------|
| The base | The system | `bootctl`, whole |
| `/opt/<name>` | Every package: the apps outside the base, the toolchain | `pkgctl`, one image each |
| `~/.local` | A person's own builds | That person, with `cp` |

The image came first, for system services. The work made it the only form.

## What stands today

Software outside the base is a package: one ext4 image with a verity tree,
installed by `pkgctl` and shown at `/opt/<name>`. The toolchain and the
apps are two, which every root is offered. Only what a package's signer
signed replaces it until a removal gives up the name. A person's own builds
are files in their `~/.local`.

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
- **An image is its name and its signer** (phase 7), as the systems that
  grew large settled it (below).
- **The word is package** (phase 9). An image is a package's file.

## What later security rests on

An image is where privacy and confinement can attach later, because four
things already hold, and every phase keeps them:

1. **The bytes are the publisher's.** Every block is held to the image's
   root as it is read.
2. **A process knows, and can prove, which image it is of.** The kernel
   labels a program started from an activation, and a peer reads the label
   (`SO_PEERPACKAGE`).
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
| The root of its hash tree | These exact bytes | The store's file name, `pkg_activate`, the label a peer reads (name and root) |
| Its name | One image per machine | `/opt/<name>`, the record, its uid, `/var/lib/<name>`, `/etc/<name>` |
| The fingerprint of the key that signed it, kept in the record | Its publisher | Every install, and `install-signed` unasked |

Until phase 7 the name alone carried what an image owns, and an
administrator's approval put any image over one of its name. The label a
peer reads still names no publisher.

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
image nobody signed belongs to the machine. Phase 7 built that. A short
id derived from the key, a successor key vouched for by the old one, and
name prefixes bound to keys are left for later (below); none of them
changes what phase 7 stores.

## Phase 1: the toolchain is built as an image — done

`just toolchain` ends in `slopos-toolchain.pkg`, held to every check an
install makes.

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

`toolchain_test` on the dev tests kernel, the image against the same files
as a tree on the root, means of two and three runs: `rustc --version` (cold) 910 and 1197 ms,
`rustc hello.rs` 3010 and 3566, `cargo build` 3419 and 3571, cmake's
configure 2554 and 2704, `cargo clippy` 2158 and 2303, `bash` 2005 and 1944.
`cargo fmt`, 985 and 849, is the one rung past 10%, inside the tree's own
spread of 697 to 970. `fsperf` holds `FSIMG`: one file faulted in cold from
an image and from the root it is a file of.

## Phase 3: an image installs at the speed of its disk — done

`pkgctl install` had read the 950 MB toolchain image over and over, 180 000
`pread`s of a millisecond each and a `pwrite` per block: 270 s. Measured on
the host over a 950 MB image of the Rust nightly's 5018 files, the passes
took 2 145 reads to open it, 24 475 to hold its files to the manifest,
20 992 to read its ELF files again for the symbol check, 14 500 to copy it
and 19 112 to read the copy back: 81 224 reads of 5.8 GB.

The image is now read once (`Image::copy`). One thread reads 4 MiB chunks,
another hashes each block once into the tree, held to the named root, and
each file into its manifest digest, and a third writes runs of non-zero
blocks. Only the levels above the leaves are held. The requester checks only
what the image says; the manifest and symbol checks are the mechanism's,
from the digests the copy took, or a read of the files when the store holds
the image already. The kernel holds every block it reads to the tree, so
the sealed copy is read back only as far as its tree
(`image::verify_tree`). Hashing takes the SHA instructions where the CPU has
them (`sha256::ni`, userland only) and a long run every core.

On the release kernel, `pkgctl install` takes 7.5 s by its own clock where
a plain copy of the file in the same boot, 4 MiB reads and writes and an
fsync, takes 5.8 s: 1.3 times the raw time.
Measured once by hand; no gate holds it.

## Phase 4: an image reaches a root without a person — done

Before `activate-all`, init runs `pkgctl install-offered` holding
`PackageUpdate` and `Seal`: each image in `/var/lib/slopos/offered`, uid
0's alone, that asks for nothing but commands and apps goes in above its
record's floor, never over a key's, a removed one or a version rolled back
from (`admit::Authority::Offered`). Host roots offer `slopos-toolchain` and
`slopos-essentials` there, and the installer copies the medium's images into
the new root's. A live boot installs the archive's `images/` into its RAM
root, whose seal `Seal` now clears as on ext4: smaller than teaching userland
a second way to show an image. `priv` takes a command through `/opt/bin`, an
image's files being uid 0's. The cheap boot came with phase 3's kept needs.

## Phase 5: the apps SlopOS ships are one image — done

`build_userland.sh` stages `builddir/apps` with a release and a declaration
(four apps, five commands), its version moving only with its files, and
`_build-userland` packs it as `slopos-essentials`. Its entries name
`/opt/slopos-essentials/bin/…`, the editor reads its manual from the image,
and `just install` installs it with `pkgctl install`. The apps start
`AT_SECURE`.

## Phase 6: the shared prefix goes — done

`tree-install`, `tree-core`, `org.slopos.software.install` and the prefix
the first person held went, with what guarded it: the hand-over, `priv`'s
boot mark, the loader's, the dock's and the shell's extra directory.
`DEFAULT_PATH` is `/bin:/sbin:/opt/bin`, and a command's completions come
from its image. The second `users add` closes the first home; a home made
anew is open while the accounts record holds one person. `~/.cargo/bin`
stays off every path.

## Phase 7: an image is its name and its signer — done

`admit::install` holds every install, a person's too, to the key that signed
what the name holds, or to nobody, and names any other as another
publisher's image of that name. A person's `pkgctl install` takes the
signature beside the image, trusted or not, and the question names its
signer or nobody. `pkgctl remove`, an action holding `DacOverride`, gives the
name up: the record keeps only that it was removed, its retired uids and its
configuration serial, and `/etc`, `/var/lib`, `/var/log` and
`/run/<name>` go, a first install waiting while any is left. Another
publisher's image then goes in as a first install, as `package_test` shows.

## Phase 8: as many images as a machine holds — done

The pool of 32 and the mount table's fixed slots went. One filesystem
serves every activation at a mount of its own, each made from the kernel
heap and found from the number its inodes carry; the mount table is a
vector sorted by path. What the resident activations hold that no reclaim
gives back is bounded by an eighth of usable memory (`package::budget`).
An image granted no user takes no uid, and a removal that leaves nothing
its uids own gives them back.

`test_a_hundred_images_are_shown_and_one_updated` shows a hundred and
updates one. On the dev tests kernel each took 1.6 to 2.7 ms and 12 reads
of its file to show, and holds 13 KiB against the budget of 270 KiB of
pages, the rest its volume's cache; the `fsperf` gate caps the heap and the
reads (`FSACT`). Images stay shown at boot: a hundred cost the kernel under
0.3 s, and showing one at first use would have a walk wait on userland.

## Phase 9: the word extension goes — done

Installed software is a *package*: FHS's word for what sits at
`/opt/<name>`, and Android's, Fuchsia's and MSIX's for a verified unit of
software. *Image* stays the word for a package's file. The tool is
`pkgctl`, the calls `pkg_activate` and `pkg_deactivate`, the suffix `.pkg`,
and every other name follows the word, under `BASE_INTERFACE` 16. A store
under the old path is not read.

## Later

- **A front end.** Nothing fetches an image. A sidecar already names an
  image, its version and its root, and `pkgctl install-signed` already
  checks the signature, the key's ceilings and the floor, so an index is a
  signed list of sidecars with an address and a serial that only rises. The
  program that reads it needs the network and `pkgctl`, holds no
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
