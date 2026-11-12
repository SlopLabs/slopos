# Linux's interfaces in place of private calls (spike)

This file is a spike. Its work is research, and its result replaces it: when
the questions below are answered, rewrite this file as the plan they lead
to, or delete it and say in the commit why no plan follows.

## The goal

The kernel answers a base through interfaces Linux defines wherever Linux
defines one, and keeps a call of its own only for what SlopOS adds. A call
moved to a Linux interface has its shape set outside SlopOS, so it cannot
drift between a kernel and a base, it is one reason fewer for a new
`BASE_INTERFACE`, and a program written for Linux that uses it runs
unported.

## What is known

The private range holds 54 calls (`abi/src/syscall/numbers.rs`, from
`SYSCALL_PRIVATE_BASE`). Grouped by name, with the Linux interface that
looks like its counterpart; no call's behaviour has been read against it
yet:

| Group | Calls | Count | Looks like |
|-------|-------|-------|------------|
| Display, input, seat | `fb_info`, `fb_flip`, `cursor_set_image`, `cursor_move`, `set_display_mode`, `screen_acquire`, `input_sink_acquire`, `input_poll_batch`, `keymap_load`, `keymap_get_name`, `font_set`, `clipboard_copy`, `clipboard_paste`, `seat_switch` | 14 | DRM/KMS on `/dev/dri`, evdev on `/dev/input`, the console's ioctls; the clipboard and keymaps in userland |
| Statistics and logging | `sys_info`, `process_list`, `cpu_info`, `percpu_stats`, `process_memory`, `block_stats`, `display_stats`, `klog_write` | 8 | `/proc`, `/sys`, `/dev/kmsg` |
| Network settings | `net_query`, `net_iface_ctl`, `net_addr_ctl`, `net_route_ctl`, `net_resolver_set`, `net_monitor`, `resolve` | 7 | rtnetlink, `/etc/resolv.conf`, a resolver in the C library |
| Firmware, power, diagnostics | `efivar_get`, `efivar_set`, `cpu_perf`, `cpu_perf_ctl`, `prof_ctl`, `kconsole` | 6 | efivarfs, cpufreq under `/sys`, `perf_event_open`, sysrq |
| Asynchronous I/O | `ring_setup`, `ring_enter`, `ring_register` | 3 | io_uring |
| Starting a process | `spawn_path`, `sigdefault` | 2 | `clone`, `execve` and `posix_spawn` in the C library |
| Tests and the wheel | `test_report`, `run_userland_tests`, `test_panic`, `test_key`, `roulette`, `roulette_result`, `roulette_draw` | 7 | none needed |
| SlopOS's own | `caps_drop`, `caps_get`, `login_tag`, `base_activate`, `base_monitor`, `ext_activate`, `ext_deactivate` | 7 | none |

The private surface is more than the calls: `SOL_SLOPOS` with
`SO_PEERPROGRAM` and `SO_PEEREXTENSION` on `getsockopt`, two `statfs`
magics, and the records the calls take, all rooted in
`scripts/gates/base-interface.txt`.

The kernel serves `/dev` with `console`, `kmsg`, `null`, `zero`, `random`,
`pts` and the disks. It has no `/proc`, no `/sys`, no netlink socket and no
io_uring, and `ioctl` answers on a terminal alone.

Asterinas, a framekernel that offers Linux's interfaces alone, went the same
way for its desktop: `/dev/fb0` and evdev first, then DRM/KMS.

## Questions

Each answer is a reading of the source or of Linux's documentation, with
where it came from.

### Call by call

1. For each of the 54: what does it do, who calls it (`grep` the base's
   programs and `slibc`), and does the Linux interface cover all of it? Name
   what would be lost or would need an extension.
2. Which calls carry SlopOS's authority model in their arguments
   (`spawn_path`'s capability mask, `screen_acquire`'s seat rank, the
   clipboard's login tag)? A Linux interface has no place for those; say
   where the check would go.
3. Which of the seven test and wheel calls can leave the kernel for
   userland, and which exist only because the tests kernel starts the
   userland tests (`self-hosting-on-hardware.md`, phase 3)?

### What each Linux interface costs

4. For `/proc`, `/sys`, rtnetlink, evdev, DRM/KMS, efivarfs and io_uring:
   the smallest subset that replaces the private calls in its group, in
   files, message types, ioctls or opcodes, and the kernel work to serve
   it. A pseudo-filesystem is a filesystem to write and keep.
5. Which of them would a ported program use at once (`ps` and `top` on
   `/proc`, `ip` on rtnetlink, a Wayland compositor on DRM and evdev), and
   which subset would those programs need beyond what the base does?
6. SlopRing and io_uring: is matching io_uring's layout and opcodes a
   goal, or does the ring stay private because it borrows only the idea?
7. evdev descriptors poll. Does moving input there remove *The compositor
   takes input once a tick* from `KNOWN_ISSUES.md` without a new private
   call?

### What it does to the system around the calls

8. An app reaches the system through `libc.so` alone and holds no
   `syscall` instruction. Which Linux interfaces would an app then use
   directly through the C library, and does `xtask lint app-deps` or
   `xtask programs` have to change?
9. A file under `/proc` or `/sys` is readable by whoever its mode lets. The
   private calls check a capability. For each group, what keeps a person
   from reading what `process_list` or `net_query` holds back today?
10. Each call that moves changes the private surface and so takes a
    `BASE_INTERFACE` number. In what order do the groups move so that each
    step is one paired switch and the base never uses both forms at once?

### What stays

11. For the seven calls with no counterpart, and the socket options: is a
    private call the right form, or would a file or an ioctl on a device of
    SlopOS's own serve better, given that either is as private as a call?

## How

- Read, do not build. A group's cost is estimated from Linux's
  documentation and from what a second kernel implementing it needed.
- Write each call's answer where it can be checked: the file and function
  that implements it, and the programs that call it.
- Rank the groups by what a move buys against what it costs, and say which
  come first.

## Done when

Every one of the 54 calls has a verdict (moves to a named Linux interface,
moves to userland, or stays, with the reason), each Linux interface has its
subset and its cost, and this file has become the plan: groups in an order,
each with what it builds and how it is checked.
