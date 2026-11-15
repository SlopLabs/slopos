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
