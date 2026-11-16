# Not yet in the docs

- A `command` word may carry `+` (`c++`, `clang++`); an `app` or unix socket
  name may not.
- A declared command may be a symlink in `bin/`: it is delivered when it
  resolves, through the image's own links and at most 7 of them, to an
  executable regular file of the image (`Image::resolved`). A service's, an
  action's or a `program` principal's program is still a regular file at the
  path named.
- `Declaration::programs` no longer lists the commands.
- `docs/architecture/extensions` (reads, locks): the verifying device holds
  no lock across a read. A sealed image file whose every block is at its home
  (`FileSystem::file_runs`, mapped once at activation) is read straight off
  its filesystem's device by those runs; any other file through its
  filesystem, one call per run. A read takes its run in one request and
  checks each block (`verity::check`), the leaf over 128 looked up once. The
  levels above the leaves, up to 256 blocks, are read and checked at open
  and kept; 64 leaves are kept under `EXT_VERITY_KEPT`, a spinlock never held
  across I/O. A seal taken off later fails reads with `EIO`, never other
  bytes. Locks nest activation, then the filesystem holding the image file;
  only the through-the-filesystem reads take the second.
- An activation's `read_pages` (the file map's fills) plans the read under
  the volume's lock and reads off it, keeping nothing in the volume's block
  cache: the file map is the one cache of what a mapping reads. `read` still
  goes through the volume's cache. Lookups, but for `.` and `..`, and `stat`
  are answered from the activation's index without the volume's lock; the
  index keeps each entry's `stat` from its walk.
- `verity-core` is built at `opt-level = 3` in dev builds too
  (`[profile.dev.package.slopos-verity-core]`): every block an image serves
  is hashed, and the unoptimized hash cost forty times the read.
- `docs/architecture/extensions` (install): the image is read once.
  `Image::copy` reads it in 4 MiB chunks, two ahead, while another thread
  hashes each data block once into the tree (`verity::Streamed`, only the
  levels above the leaves held, each leaf handed on as it fills) and hands
  the chunks to a `Sink`: `extctl`'s writes them on a third thread, its
  buffers handed back. The copy is refused unless its root is the one
  named. Each file is hashed from the same chunks as they pass, by its
  extents, and held to the manifest once the root holds; a file whose
  bytes lie out of order is read again.
  `admit::install` decides what an image says (name, grant, signer,
  ceilings, clashes) and is all a requester checks before privd asks;
  `admit::contents`, the manifest and the symbol check, is the mechanism's,
  after the copy. A refused copy is taken away by the store's collect.
- Once sealed, a copy is read back only as far as its tree
  (`image::verify_tree`: the levels above the leaves whole, the leaves a
  chunk at a time against them), as is a copy the store holds already: every
  reader holds each data block to the tree. `verity::verify_all` and
  `image::verify_whole` are gone.
- `extension-core`'s reader (`Verified`) keeps the levels above the leaves,
  as many as fit 256 blocks from the top as the kernel's device does, and
  64 leaves. A run of 1 MiB or more is read in one call, its blocks hashed
  on every core (`verity::check_digests`); a shorter read takes a 1 MiB
  window, two kept, each block checked the first time it is handed out.
  `Image::check_contents` reads files in inode order, the order the writer
  laid them out in. A copy keeps an ELF file's bytes for the symbol check
  only until its last piece passes.
- `verity-core` hashes through a `sha256::Compress` a source chooses
  (`image::Source::compress`, on `Source::cores` cores). `sha256::ni` is
  the SHA instructions, built only where SSE2 is (userland: the kernel is
  soft-float); `extctl` takes it when `ni::present()` says the CPU has
  them (`extctl/disk.rs`, the one `unsafe`).
