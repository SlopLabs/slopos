# Pending for the docs

- `docs/architecture/authority`, `docs/abi/glossary`, `docs/abi/privd-actions`:
  `ClipboardGlobal` is gone and the bits above it moved down one (`Fate` is
  bit 7, `SeatSwitch` bit 20; 21 in all); `CAP_UNIVERSAL` is `ConsoleIo |
  Fate`. `ConsoleIo` gates no syscall: devfs checks it at every write to
  `/dev/kmsg` (`EPERM` without it), so a descriptor handed on carries
  nothing. A write is one record: Linux's `<N>` prefix sets the level
  (0-3 error, 4 warn, 5-6 info, 7 debug, none warn), over 1024 bytes is
  `EINVAL`, and past 10 records in 5 s a process's records are dropped while
  the write still answers its length, as `printk.devkmsg=ratelimit` does.
  A record stays one log line: a control character but tab, a line
  separator and a backslash go in as `\xNN` per byte, as Linux shows them.
- `docs/architecture/privd`, `docs/abi/privd-actions`,
  `docs/abi/interfaces`: an action may name up to 8 terms, `term <word>
  "<Label>"`: the argument each shape's `?` takes right after `<word>`,
  found by position (`Action::terms_of`), so no argument can pass for a
  key; every shape must have it. The details list each term under its
  label above *Runs*, `-` read as *None* and a list's commas followed by a
  space (`consent::term_text`). A label passes `words::term`: at most 24
  characters, a capital first, no closing punctuation, none of the
  details' own labels. `consent::VERSION` is 2: a question carries
  `term`/`value` pairs. The shown limit is 4096 escaped bytes each for the
  command line, the directory and a term, not 320: the dialog scrolls its
  content, so the person still sees all of it or is not asked.
  `org.slopos.extension.install` takes only the canonical line,
  `install ? ? name ? grant ? unasked ? users ? commands ?`, and names
  *Installs*, *Capabilities*, *Runs unasked*, *Adds users* and *Adds
  commands*.
- `docs/architecture/privd` (the requester): output for a terminal goes to
  a terminal of the mechanism's own, sized as the requester's and resized
  with it, whose master the requester relays, as run0 does: a mechanism
  draws progress and colour as it would at the terminal. Unless it takes
  keys (`Input::Terminal`) that terminal has `OPOST` off, since the
  requester's stays cooked and turns newlines into CR LF once. With no
  terminal free, output alone falls back to pipes. A socket still goes
  through a pipe. On a terminal, *waiting for you to allow this
  on the screen* is taken back (`\r`, `ESC [K`) once privd answers or the
  mechanism writes; elsewhere it stays a line.
