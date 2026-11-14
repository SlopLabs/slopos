# Pending for the docs

- `docs/self-hosting/updating`, `docs/architecture/base`,
  `docs/architecture/privd`: a base is tried before it is kept. `bootctl base
  try <img> [<sha256>]` makes the checks `base install` makes and activates
  the base from memory, writing nothing; it holds `BaseUpdate` alone
  (`org.slopos.base.try`), and any reset boots the partition's base. The
  first try of a trial notes the replaced base's manifest digest in
  `/run/bootctl/base-trial` (uid 0's, in memory). `base commit <img>
  [<sha256>]` (`org.slopos.base.commit`, `RawBlock Power`) writes the running
  base as `base install` does, keeping the base the trial replaced as the one
  that last ran; `base install` during a trial follows the same rule.
  `base rollback` during a trial runs that base again from the partition and
  writes nothing. `bootctl status` prints `base tried:` for a running base
  neither file holds. `base install` still writes and runs at once.
- Given `--kernel <kernel.elf> [<sha256>]` (`org.slopos.base.pair`), a base
  the running kernel serves is tried as above and the kernel left alone; one
  it does not serve goes to the partition beside the running one, and the
  kernel, which must serve it, into the spare slot, armed to boot once;
  otherwise nothing is written. `just install` builds both and hands them
  over; `remote.py install` tries, commits once the agent answers with the
  base's tag, then puts in a changed kernel. The pair replaces the
  base-first-then-kernel order the updating page describes.
- A base write copies the default slot's kernel over any other offered
  slot's kernel that would serve neither base, but the slot a pair fills.
  `bootctl status` prints each slot's served interfaces and words.
- Boot options travel with the kernel: `.slopos.cmdline` is 512 bytes of
  words (graphic ASCII, single spaces) then NULs, asserted by `link.ld`; the
  kernel appends them to the loader's line and drops an invalid section
  whole. `SLOPOS_BUILTIN_CMDLINE` fills it at build, held to the same rule.
  `bootctl install` and `clone` stamp the machine's words,
  `/etc/kernel/cmdline` (`layout::MACHINE_CMDLINE`, counted only while uid 0
  alone can write it and its directory), then `install --cmdline <words>`; a
  kernel going to another disk's slot (`--boot`) keeps its own words unless
  `--cmdline` names some. `bootctl cmdline [set <words>]` reads and writes the
  file (`org.slopos.boot.cmdline`). The installer writes the file and stamps
  both slots; `limine.conf` carries `root=` alone. `docs/abi/boot-options`
  says where words come from.
