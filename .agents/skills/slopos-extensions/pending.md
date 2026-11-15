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
