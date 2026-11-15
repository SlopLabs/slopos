# Linux's interfaces in place of private calls

## The goal

The kernel answers a base through interfaces Linux defines wherever Linux
defines one, and keeps a call of its own only for what SlopOS adds. A call
moved to a Linux interface has its shape set outside SlopOS, so it cannot
drift between a kernel and a base, it is one reason fewer for a new
`BASE_INTERFACE`, and a program written for Linux that uses it runs
unported.

Of the 54 private calls (`abi/src/syscall/numbers.rs`, from
`SYSCALL_PRIVATE_BASE`), 26 move to an interface Linux defines, 8 move to
userland, 2 are deleted and 18 stay. The eight phases below take 34 of
them. The two that start and report the userland tests leave with
`tests-without-a-host.md`.

## The order

| Phase | Interface | Calls removed | Private calls left | Needs first |
|-------|-----------|---------------|--------------------|-------------|
| 1 | None new; a write to `/dev/kmsg` | 7 | 47 | |
| 2 | `/etc/resolv.conf`, a resolver in the C library | 2 | 45 | |
| 3 | `/proc` | 5 | 40 | |
| 4 | `/sys/block`, efivarfs | 3 | 37 | Phase 3's tree |
| 5 | evdev, uinput, uevents on `AF_NETLINK` | 3 | 34 | |
| 6 | DRM/KMS | 6 | 28 | Phase 5's `ioctl` and `mmap` hooks and descriptor hand-over |
| 7 | io_uring | 3 | 25 | Phase 5's `mmap` hook |
| 8 | rtnetlink, `sock_diag` | 5 | 20 | Phase 5's `AF_NETLINK`, phase 3's `/proc` |

The order is what a phase buys against what it costs. Phases 1 and 2
take more out of the kernel than they put in. Phase 3 removes the
most calls for one piece of machinery, and `/proc` is what a ported `ps`
or `top` reads first. Phase 4 is a second mount of phase 3's tree. Phase 5
closes an entry in `KNOWN_ISSUES.md`. Phase 6 is the largest kernel piece:
Asterinas shipped its input devices in 0.17.0 and a DRM that runs Xorg in
0.19.0, ten months and four pull requests later. Phase 7 changes three
crates for a ring no program outside the base uses today. Phase 8 costs
the most, a socket family's request path and a daemon, and the base gains
from it a kernel with no DHCP client in it.

## Rules every phase keeps

- **A Linux name carries Linux's shape or is absent.** What
  `abi/src/syscall/numbers.rs` says of a number holds for a file, an ioctl
  request, a socket family and a message: Linux's format and meaning, a
  value SlopOS cannot fill reading as Linux says an unknown one reads, and
  no field of SlopOS's own added. A fact Linux has no place for stays in a
  private call.
- **A phase is one interface number.** The kernel gains the Linux form and
  loses the calls in one change, the base's callers move in that change,
  and the pair goes in with `bootctl base try --kernel`. Before v1 a
  kernel serves the base of its own tree alone, so no base program uses a
  call and its replacement at once. The private range keeps no gap
  (`xtask lint syscall-abi`): the calls behind a removed one move down.
- **What the kernel serves of Linux's is in the surface.**
  `xtask base-interface` sees the private calls, the Linux-numbered calls
  in `SYSCALL_TABLE` and what a `root` line names in `slopos-abi`. It sees
  no file, ioctl request, socket family or netlink message. Each phase
  states what it serves as constants in `slopos-abi` (paths, request
  numbers, message types) under a `root` line, as `SOL_SLOPOS` and the
  `statfs` magics are rooted, and the kernel serves from those constants.
  Adding or dropping one later then takes a number.
- **The mask stays the only authority.** A mode bit says who may open a
  node and grants nothing a capability grants. A file whose call took a
  capability checks it against the calling task at each `read`, `write`,
  `ioctl` and `unlink`, as `raw_block_entitled` does for a block node
  (`fs/src/devfs/block.rs`), so a descriptor passed on carries nothing.
  Where the descriptor is the authority (an input device, the card's
  master), opening takes the capability and init does the opening.
- **A format lives in an I/O-free crate.** The text of a `/proc` file, a
  netlink message and an evdev key table are rendered and parsed in a
  `*-core` crate the kernel and the base's readers share, host-tested
  against bytes captured from a Linux machine.
- **`libc.so` only grows.** Eleven POSIX exports sit on private calls:
  `getlogin`, `getlogin_r`, `syslog`, `vsyslog`, `getaddrinfo`,
  `gethostbyname`, `getifaddrs`, `if_nametoindex`, `if_indextoname`,
  `posix_spawn` and `posix_spawnp`. Each keeps its name and gets a new
  body in the phase that moves its call. No SlopOS-named export exists.

## The 54 calls

Handlers are in `core/src/syscall/`; *Kernel* names the file there and what
the handler reaches. *Callers* are outside the kernel, tests in italics.

### Phase 1: nothing to build

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `clipboard_copy`, `clipboard_paste` | `ui_handlers.rs` → `clipboard_copy`, `clipboard_paste` in `drivers/src/input_event.rs`: 4096 bytes per login tag in `SESSION_LOGINS` | *`sessions_test`* alone | Userland, where it already is: the compositor keeps the selection as a memfd (`compositor/protocol.rs`, `windowing/src/clipboard.rs`, `appkit/src/clipboard.rs`) and admits a copy by key serial and focus. `ClipboardGlobal` goes |
| `set_display_mode` | `ui_handlers.rs` → `video_set_display_mode` | None | Deleted. `SETCRTC` is its Linux form if a caller appears |
| `roulette_draw` | No handler; the slot answers `ENOSYS` | None | Deleted |
| `keymap_get_name` | `keymap_handlers.rs` → `keyboard::layout_name` | `userland/src/keymap.rs` `current_name` (init's `follow`, `keymap`), *`keymap_test`, `settings_test`* | Userland. Linux keeps no layout name. Init loaded the layout (`follow_keymap`) and answers its name |
| `sigdefault` | `process_handlers.rs` → `task_default_signals_in_mask` | The shell's `traps::enter_subshell` | Userland: `rt_sigaction(SIG_DFL)` per signal with the signals blocked around the loop, as slibc's `spawn_via_fork` does |
| `klog_write` | `core_handlers.rs` → `console_write_serialized`: COM1 under the log's lock, past the ring `/dev/kmsg` reads | `tty::write` (compositor, terminal, shell, roulette), slibc's `syslog`, *`usb_disk_test`, `usb_shell_test`* | A write to `/dev/kmsg`, one record per write, in the `<prio>text` form `syslog` already builds |

### Phase 2: the resolver

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `resolve` | `net_handlers.rs` → `dns_resolve` in `net/src/dns.rs`: one A record, a 16-entry cache, 16 lookups at once | `net::resolve_host` (`curl`, `ping`, `nc`, `remoted`, the shell), slibc's `getaddrinfo` and `gethostbyname`, *`dns_resolve_test`, `dns_concurrent_test`* | Userland: a stub resolver over UDP sockets. slibc has one in `res_query` (`slibc/src/net/resolv.rs`), its codec in `slibc-core/src/dns.rs`. Lost: the machine-wide cache |
| `net_resolver_set` | `net_config_handlers.rs` → `RESOLVER.set_static` in `net/src/resolver.rs`; `NetAdmin` | `ip dns`, `network_settings.rs` (netctl, Settings), *`dns_concurrent_test`* | Userland: `/etc/resolv.conf`, three `nameserver` lines and `options timeout: attempts:`. Lost: a timeout in milliseconds |

### Phase 3: `/proc`

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `process_list` | `core_handlers.rs` → `task_try_for_each_enumerable`: one `UserTaskEntry` per thread, kernel threads too, no caller refused | `sysmon`, `ps` (`coreutils/sysquery.rs`), `endtask`, init's `Tasks::read` (`init_control.rs`), *four utests* | `/proc/<pid>/{stat,status,comm}` and `task/<tid>/`. `login` reads from `sessionid`, `block_reason` by name from `wchan`. Nothing lost |
| `process_memory` | `core_handlers.rs` → `process_vm_resident_pages` | `sysmon` | `/proc/<pid>/statm`; `VmRSS` and `VmHWM` in `status` |
| `cpu_info` | `core_handlers.rs`, from cpuid | `sysmon`, Settings, `cpufreq`, *`libc_abi_test`* | `/proc/cpuinfo`, the feature mask as names on `flags` |
| `percpu_stats` | `core_handlers.rs` → `with_cpu_scheduler` | `sysmon`, `cpufreq` | The `cpuN` lines of `/proc/stat`. Lost: per-CPU switch and ready counts, which no caller reads |
| `login_tag` | `core_handlers.rs` → `process.login()` | `sessions::mine`, slibc's `getlogin` | `/proc/self/sessionid` |

### Phase 4: `/sys/block` and efivarfs

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `block_stats` | `core_handlers.rs` → `block::disk_stats` | `sysmon` | `/sys/block/<disk>/{stat,size,ro,removable,queue/logical_block_size}`. `transport` is read off the name: `nvme*`, `vd*` and `sd*` are the three there are |
| `efivar_get`, `efivar_set` | `core_handlers.rs` → `core/src/efivar.rs` (`admit`, the firmware thread); `Power`, and `BootEntry` for four names under the global GUID | `userland/src/boot_disk.rs` `read_var`, `write_var` (`bootctl`, the installer), *`install_test`, `installer_test`, `spawn_privilege_test`* | efivarfs at `/sys/firmware/efi/efivars`. Gained: the attributes on a read, and a listing |

### Phase 5: evdev

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `input_sink_acquire` | `ui_handlers.rs` → `seat::acquire`, `input_take_sink`; `InputSeat`, and a rank in the argument | The compositor when init did not start it | Opening `/dev/input/event<N>` and `EVIOCGRAB`. The rank goes: nothing takes input at the console's rank |
| `input_poll_batch` | `ui_handlers.rs` → `input_drain_batch` in `drivers/src/input_event.rs`: one merged queue of 64 `InputEvent`s, the oldest dropped unsaid | The compositor's `drain_events`, *`seat_test`* | `read` and `poll` on the device. The codepoint, the modifier snapshot and the screen position leave the event: the compositor resolves the layout (`keymap-core`) and keeps the cursor (`chrome-core/src/pointer.rs`) |
| `test_key` | `test_handlers.rs` → `input_route_key`; `TestHarness` and `tests=on` | *`support/asking.rs`, `sessions_test`* | uinput: a keyboard the tests make, typing keycodes |

### Phase 6: DRM/KMS

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `fb_info` | `ui_handlers.rs` → `framebuffer::get_display_info` | The compositor, `roulette` | `MODE_GETRESOURCES`, `MODE_GETCONNECTOR`, `MODE_GETCRTC` |
| `fb_flip` | `ui_handlers.rs` → `fb_flip_from_shm` in `video/src/framebuffer.rs`: copies a memfd's damage rects to the kernel's front buffer and flushes | `compositor/output.rs`, `roulette` | A dumb buffer (`MODE_CREATE_DUMB`, `MODE_MAP_DUMB`, `MODE_ADDFB`), `MODE_SETCRTC` once, `MODE_DIRTYFB` per frame. The answer *suppressed* becomes `EACCES` while the kernel's log holds the screen |
| `cursor_set_image`, `cursor_move` | `ui_handlers.rs` → `video_cursor_set_image`, `video_cursor_move` (virtio-gpu, xe) | The compositor | `MODE_CURSOR2`, the image a dumb buffer's handle |
| `screen_acquire` | `ui_handlers.rs` → `seat::acquire`; `DisplaySeat`, and a rank in the argument | The compositor when init did not start it, `roulette` at the console's rank, *`seat_test`, `protocol_test`* | `SET_MASTER` on an open `/dev/dri/card0`. The rank goes: init decides whose file is master |
| `seat_switch` | `ui_handlers.rs` → `hand_over_seats`, `input_switch_compositor`; `SeatSwitch`, and a task id in the argument | Init's `step_front` (`init/session.rs`) | Init's `DROP_MASTER` and `SET_MASTER` on the card files it gave the sessions, and `EVIOCREVOKE` on their input devices |

### Phase 7: io_uring

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `ring_setup`, `ring_enter`, `ring_register` | `ring_handlers.rs` → `ring/src/enter.rs`, `ring/src/register.rs` | `slopos-rt` alone (`Ring`, `slopfut`): the compositor, the terminal, the shell's line editor, `nc`, `ping`, *six utests*. `libc.so` exports none | `io_uring_setup`, `io_uring_enter`, `io_uring_register` at 425 to 427, in the subset SlopRing's behaviour already is |

### Phase 8: rtnetlink

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `net_addr_ctl` | `net_config_handlers.rs` → `iface::add_addr`, `del_addr`; `NetAdmin` | `ip addr`, `network_settings.rs` | `RTM_NEWADDR`, `RTM_DELADDR`, field for field |
| `net_route_ctl` | `net_config_handlers.rs` → `route::insert`, `remove`; `NetAdmin` | `ip route`, `network_settings.rs` | `RTM_NEWROUTE`, `RTM_DELROUTE` |
| `net_iface_ctl` | `net_config_handlers.rs` → `iface_ctl`, `dhcp`, `connectivity`; `NetAdmin` | `ip`, `network_settings.rs`, *`ip_e2e_test`, `spawn_privilege_test`* | Up, down and MTU are `RTM_SETLINK`; the neighbour and address flushes are dumps and `RTM_DEL*`. The four DHCP operations, the master switch and the connectivity recheck have no kernel side on Linux and move to a daemon |
| `net_query` | `net_query_handlers.rs`; an `owner_pid` only for the caller's sockets without `NetAdmin` | `ip`, `ss`, `nmap`, `sysmon`, the compositor, init, netctl, Settings, slibc (`getifaddrs`, `if_nametoindex`, the `SIOCGIF*` requests it answers itself) | Links, addresses, routes and neighbours are `RTM_GET*` dumps. Sockets are `NETLINK_SOCK_DIAG`, their owner found through `/proc/<pid>/fd` as `ss -p` finds it. The lease, the switch and connectivity are the daemon's |
| `net_monitor` | `net_config_handlers.rs` → `net/src/netmon.rs` | Init, the compositor, Settings, `ip monitor`, *`ip_e2e_test`* | The rtnetlink multicast groups. Overflow is `ENOBUFS` and a new dump, where today it is a record in the stream; init, the compositor and Settings read everything again on each wake already |

### Leaving with another plan

| Call | Kernel | Callers | Verdict |
|------|--------|---------|---------|
| `test_report` | `test_handlers.rs`: a ring of 192 entries per task that the kernel's runner drains | slibc's `test_harness` | Userland: KTAP on a descriptor the runner gives (`tests-without-a-host.md`, question 7) |
| `run_userland_tests` | `test_handlers.rs` → `tests_run_userland`; `TestHarness` and `tests=on` | Init's `--run-tests` | Userland: a runner with a list of its own (question 6 there) |

Both exist only because the tests kernel starts the userland tests.

### What stays

| Call | Why Linux has no form for it |
|------|------------------------------|
| `spawn_path` | It hands the child a subset of the spawner's mask, a login tag, the person's quota account and an empty descriptor table before the child can run. `execve` drops every gated capability by design (`drop_authority_for_exec`), so a child that forks and executes holds none. slibc's `posix_spawn` asks for no capability and keeps `spawn_via_fork` for what the call cannot express |
| `caps_drop`, `caps_get` | One 64-bit mask per process, 22 bits, dropped for good with no privilege. `capset` and `capget` carry three sets per thread with Linux's bit meanings; SlopOS's mask at 125 and 126 is another operation at a Linux number, and a `CapEff:` line in `status` misleads the same way |
| `base_activate` | The source is an archive in memory, every base mount moves in one step, the interface is checked and a generation comes back. `mount(2)` takes one target |
| `base_monitor` | Linux's nearest, `poll` on `/proc/mounts`, wakes for every mount and keeps no mark per reader |
| `ext_activate`, `ext_deactivate` | A descriptor and a digest, a pinned mount replaced in one step, and the label `SO_PEEREXTENSION` answers |
| `sys_info`, shrunk | The boot's flags as the kernel decided them, the base-interface range it serves, the W/L balance and the last OOM victim. Its counters leave in phase 3 |
| `display_stats`, shrunk | Frames, bytes copied and the time copying and presenting took, which Linux keeps in a driver's debugfs. Its geometry leaves in phase 6 |
| `font_set` | 8-bit coverage for 1190 glyphs and a replacement cell. `KDFONTOP` carries one bit per pixel and 512 glyphs |
| `keymap_load` | One validated table swapped whole, indexed by HID usage. `KDSKBENT` sets one entry a call, indexed by Linux keycode. After phase 5 it serves the console's line alone |
| `cpu_perf` | APERF, MPERF and TSC sampled at one instant and the raw HWP registers, per CPU. Linux's raw form, `/dev/cpu/<N>/msr`, reads one register a call; its cpufreq files hold what `cpufreq` computes from these |
| `cpu_perf_ctl` | Placement on a hybrid CPU and limits in raw HWP levels have no file, and `cpufreq set` waits on the generation the call answers |
| `prof_ctl` | A tick classifier that keeps its tables in the kernel and reports into the log. `perf_event_open` is a descriptor per event with a ring of samples |
| `kconsole` | Its letters are the physical console's, and nine of the seventeen mean something else to `/proc/sysrq-trigger`: `s` syncs, `c` crashes, `f` kills for memory, `i` kills every process |
| `roulette`, `roulette_result` | The kernel draws, keeps the ledger and reboots for a caller that holds no `Power` |
| `test_panic` | It panics in a syscall for the recovery smoke, under a boot flag |

The rest of the private surface stays as it is. `SO_PEERPROGRAM` and
`SO_PEEREXTENSION` are on Linux's `getsockopt` at a level of SlopOS's own;
`SO_PEERSEC` has their shape, but its context is fixed when the peer
connects and a program identity lapses when the peer changes program. The
two `statfs` magics are `f_type` values, which Linux gives each filesystem
one of.

Each of these stays a call. A file or an ioctl on a node of SlopOS's own
is as private, and costs a node, a `FileOps` and a path that has to
resolve, and it takes the capability check out of the dispatch table,
where `CAP_COUNTS` and the ELF authority gate count it, into a handler.
None of the eighteen is handed from one process to another, which is what
a descriptor is for. The seats are, and phases 5 and 6 make them
descriptors.

## Phase 1: calls with nothing to build

Builds:

- `/dev/kmsg` takes a write (`fs/src/devfs/mod.rs` discards one today): one
  record per write, the `<prio>` prefix parsed, into the log's ring and the
  console, with a rate limit per writer as Linux's `printk.devkmsg` has.
  `ConsoleIo` is checked at each write, so a process that dropped it stays
  silent. `tty::write` and slibc's `syslog` open the node once and keep
  the descriptor; the terminal and the compositor open it before they can
  need it in a panic path.
- Init answers the layout it loaded, on `init_control` or in a file under
  `/run` beside the session table.
- The shell's subshell resets its signals with `default_signal` in a loop.
- `sessions_test`'s clipboard case moves to the compositor's clipboard.

Removes from the kernel: `CLIPBOARD` and its two service slots, the
`ClipboardGlobal` bit, `video_set_display_mode` with what it alone
reaches, `keyboard::layout_name`.

Lost: a program's line now enters the ring the kernel's own lines share,
with the log's prefix. The log gates that match `usb_disk_test`'s and
`usb_shell_test`'s markers match the prefixed line.

Checked by: `just base-interface` after `--record`; `sessions_test`,
`keymap_test`, `settings_test`, `shell_script_test`; `just test-usb`;
`just check-framekernel-gates` for the capability that went.

## Phase 2: the resolver in the C library

Builds:

- slibc's `getaddrinfo` and `gethostbyname` on `res_query`, with answer
  parsing and a CNAME chase in `slibc-core/src/dns.rs`, `/etc/hosts`
  first, and servers, timeout and attempts from `/etc/resolv.conf`.
- `net::resolve_host` in `userland/src/net.rs` on the same codec, for the
  base's programs.
- `netctl apply`, which init runs on an interface event
  (`init/network.rs`), writes `/etc/resolv.conf` by rename: the servers
  the network settings pin, else the lease's. `UserDhcpStatus` gains the
  lease's servers for that. Which wins becomes netctl's rule, where
  `RESOLVER`'s pin is the kernel's today.

Removes from the kernel: `net/src/dns.rs`, `net/src/resolver.rs`,
`NET_Q_RESOLVER`, `NET_EV_RESOLVER`. `connectivity::probe_wan` pings the
lease's first server, the one address of a resolver the kernel still
holds.

Lost: the shared cache, so each lookup asks a server, as on Linux without
a caching daemon; the classifier's note that a lookup succeeded; the
kernel's parse of a reply from the network, which is the gain.

Checked by: `slibc-core` host tests for the answer parser; the two DNS
utests against SLIRP's server; `curl_e2e_test`, `ip_e2e_test`,
`libc_abi_test`.

Beyond the base: AAAA records and a search list, when IPv6 exists.

## Phase 3: `/proc`

Serves, mode 0444 in directories 0555, since the calls refuse no caller
today (`docs/architecture/processes`):

| File | Holds | From |
|------|-------|------|
| `/proc/meminfo` | `MemTotal`, `MemFree`, `CommitLimit`, `Committed_AS` | `sys_info` |
| `/proc/stat` | `cpu`, `cpu<N>` (idle, and busy time in the system column until the kernel splits it), `ctxt`, `procs_running` | `sys_info`, `percpu_stats` |
| `/proc/vmstat` | `oom_kill` | `sys_info` |
| `/proc/cpuinfo` | A stanza per CPU: `vendor_id`, `cpu family`, `model`, `model name`, `stepping`, `flags` | `cpu_info` |
| `/proc/self` | A link to the reader's directory | |
| `/proc/<pid>/stat`, `status`, `comm` | Linux's 52 fields; `Name`, `State`, `Tgid`, `Pid`, `PPid`, `Uid`, `Threads`, `VmRSS`, `VmHWM`, `Cpus_allowed`, `voluntary_ctxt_switches`; the 32-byte name whole, as Linux shows a worker thread's | `process_list`, `process_memory` |
| `/proc/<pid>/statm` | Resident pages | `process_memory` |
| `/proc/<pid>/sessionid` | The login tag; 4294967295 for none, as Linux's unset one reads | `process_list`, `login_tag` |
| `/proc/<pid>/wchan` | The `BlockReason`'s name, `0` when running | `process_list` |
| `/proc/<pid>/task/<tid>/` | `stat`, `status`, `comm`, `wchan` per thread | `process_list` |

Kernel work:

- One synthetic filesystem beside devfs (1390 lines; ramfs is 1400): a
  `FileSystem` whose inodes are a static tree of nodes, each rendering its
  text on a read, with the directories of live tasks made on lookup as
  devfs makes `/dev/pts/<n>` (`walk_pts`). `FileSystem::read` takes an
  inode and an offset and keeps nothing per open, so a node renders whole
  on each read and a file is read whole in one.
- A `procfs` arm in `mount(2)`'s closed set
  (`core/src/syscall/fs/mount_handlers.rs`) and a mount at boot beside
  `/dev` (`fs/src/vfs/init.rs`).
- The directory of a process is its leader's id; `process_memory` keys by
  the kernel's process id and takes that mapping.

Shrinks `sys_info` to the boot flags, the base-interface range, the W/L
balance and the last OOM victim.

Costs: `sysmon` reads a directory and two files per task each second
where it makes one call.

Checked by: a `proc-core` crate's host tests against lines captured from
Linux; stests in place of `process_list`'s and `process_memory`'s in
`core/src/syscall/tests.rs`; `sessions_test` (init ends a session by its
tag), `privd_test` (`endtask`), `installer_test`, `script_exec_test`,
`extension_service_test`, and `libc_abi_test` for `getlogin`.

Beyond the base: procps and busybox read `/proc/<pid>/cmdline`,
`/proc/uptime` and `/proc/loadavg` before anything; a compiler or a
runtime reads `/proc/self/exe` and `/proc/self/maps`; `df` reads
`/proc/mounts`. `cmdline` needs the argument block's bounds kept at
`execve` and a read of another process's memory.

## Phase 4: `/sys/block` and efivarfs

Serves:

- `/sys/block/<disk>/stat` (Linux's 17 columns, zero where the block
  layer keeps none: merges, ticks per direction, in flight, discards),
  `size` in 512-byte sectors, `ro`, `removable`,
  `queue/logical_block_size`.
- `/sys/firmware/efi/efivars/<Name>-<GUID>`: four bytes of attributes and
  the value on a read, one `SetVariable` per write, a delete on `unlink`.
  The directory is absent on a boot with no UEFI runtime services, which
  is how a Linux program asks.

Kernel work:

- `/sys` as a second mount of phase 3's tree.
- efivarfs as a `FileSystem` over `core/src/efivar.rs`. `admit` runs at
  lookup, read, write and unlink against the caller's mask (`Power`, and
  `BootEntry` for `BootOrder`, `BootNext`, `BootCurrent` and `Boot####`),
  so a name the caller may not reach is not there. `check_boot_write`
  (`boot-core/src/variables.rs`) runs at the write, which holds the whole
  value. A listing needs `GetNextVariableName` in `slopos-ostd/src/uefi.rs`,
  filtered by `admit`.

Lost: `ENOBUFS`; a reader sizes its buffer from `stat`.

Checked by: `core/src/tests/efivar_tests.rs` and `authority_tests.rs`
through the files; `install_test`, `installer_test` and
`spawn_privilege_test`'s firmware cases; `just test-install`; `sysmon`'s
disk page against `block_stats`'s stest.

Beyond the base: `lsblk` wants `/sys/block/<disk>/dev` and
`/sys/dev/block`; `efibootmgr` wants every variable listed, not those
`admit` lets through.

Not built: cpufreq's files. `cpu_perf` stays, and no base program would
read them.

## Phase 5: input on evdev

Serves:

- `/dev/input/event<N>`, one node per keyboard and pointer the drivers
  claim (`keyboard::claim`, eight of each). `read` answers 24-byte
  `input_event`s: `EV_KEY` with Linux's key codes and 2 for a repeat,
  `EV_REL` for a mouse and for the touchpad as the kernel converts it
  today, `EV_ABS` with the device's range for a tablet, `REL_WHEEL_HI_RES`,
  `SYN_REPORT` after each report, `SYN_DROPPED` when the queue overflowed.
- `EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGBIT`, `EVIOCGABS`,
  `EVIOCGKEY`, `EVIOCGRAB`, `EVIOCREVOKE`.
- `/dev/uinput` on a `tests=on` boot, with `TestHarness` at each request:
  `UI_SET_EVBIT`, `UI_SET_KEYBIT`, `UI_DEV_SETUP`, `UI_DEV_CREATE`,
  `UI_DEV_DESTROY` and a write of events. Its keys reach readers of its
  node alone, never the diagnostic console's trigger or the TTY.
- `socket(AF_NETLINK, SOCK_DGRAM, NETLINK_KOBJECT_UEVENT)`, `bind` to
  group 1, `recv` and `poll`: an `add` or `remove` message when an input
  node comes or goes. No request is parsed.

Kernel work:

- `FileOps` gains `ioctl` and `mmap` (`abi/src/file_ops.rs`);
  `syscall_ioctl`'s fixed chain (`core/src/syscall/fs/poll_ioctl_handlers.rs`)
  ends in the file's own.
- A devfs node that opens with ops of its own, as `/dev/ptmx` does by a
  special case in `fs/src/fileio/fdops.rs`; the open takes `InputSeat`.
- A queue and a waiter per device in `drivers/src/input_event.rs`, woken
  from `route`; a HID usage to Linux key code table in a `*-core` crate.
- A third family in `core/src/syscall/net_handlers.rs`'s `SocketFd`, with
  `net/src/netmon.rs`'s ring as the pattern for a group's readers.
- The kernel's own readers keep every key: the diagnostic console's
  chord, the lock LEDs, the boot log's scroll. The console's TTY gets none
  while a device is grabbed, the rule `keys_reach_console` holds today.

Userland work:

- Init is the seat's manager, as logind is. It holds `InputSeat`, opens
  each device for the session in front, passes the descriptors to that
  session's compositor, revokes them when another session comes in front,
  and follows uevents for a device that arrives. The compositor and the
  greeter hold `InputSeat` no more.
- The compositor grabs each device, waits on them with
  `slopfut::poll_add_multishot` as it waits on its listen socket, and
  owns what the kernel did for it: the layout and dead keys, the modifier
  state across keyboards, the cursor's position, bounds and scaling of an
  absolute device. Tests type keycodes through uinput.

Removes from the kernel: the merged sink, the cursor position and button
merging in `input_event.rs`, `seat_switch`'s input half.

Closes *The compositor takes input once a tick* in `KNOWN_ISSUES.md` with
no new private call: `OP_POLL_ADD` waits on any descriptor whose `FileOps`
registers a waiter.

Checked by: host tests of the key table against `input-event-codes.h`;
`seat_test`, `sessions_test` (a switched-away compositor reads nothing),
`priv_test`, `privd_test`, `extension_action_test` and `apps_test`
through uinput; `just test-usb` for a keyboard that arrives; an event
queued between two ticks reaching a client before the next.

Beyond the base: libinput wants `EVIOCGPROP`, `EVIOCGPHYS`,
`EVIOCSCLOCKID`, `ABS_MT_*` from a touchpad, and udev's view of a device
(`/sys/class/input`, or libudev-zero over the uevents above); SDL's evdev
backend runs on the base's subset.

## Phase 6: the screen on DRM/KMS

Serves `/dev/dri/card0`: one CRTC, one encoder, one connector with one
mode, its timings made from the scanout's size as Linux's simpledrm makes
them.

- Any opener: `VERSION`, `GET_CAP`, `MODE_GETRESOURCES`,
  `MODE_GETCONNECTOR`, `MODE_GETENCODER`, `MODE_GETCRTC`,
  `MODE_CREATE_DUMB`, `MODE_MAP_DUMB` and `mmap` at the offset it answers,
  `MODE_DESTROY_DUMB`, `MODE_ADDFB`, `MODE_RMFB`.
- The master file: `MODE_SETCRTC`, `MODE_DIRTYFB`, `MODE_CURSOR2`;
  `ENXIO` from the last on the firmware's framebuffer, where the
  compositor draws the cursor itself.
- `SET_MASTER` and `DROP_MASTER`, which take `DisplaySeat`.

Kernel work:

- Dumb buffers as the page sets a memfd has (`mm/src/process_vm.rs`),
  charged to their maker, mapped through phase 5's `mmap` hook.
- `fb_flip_from_shm`'s copy and flush behind `MODE_DIRTYFB`; what the
  first flip does (`COMPOSITOR_OWNS_FB`, the end of the boot log's toggle)
  behind `MODE_SETCRTC`.
- Master as a state of the open file on the seat arbiter
  (`slopos-ostd/src/seat/`), dropped when the file closes, which gives the
  kernel's console the screen back as `release_compositor_fb` does. The
  on-screen log and the panic screen take the screen as today, and
  `MODE_DIRTYFB` answers `EACCES` while they hold it.

Userland work:

- Init opens the card once per session and for `roulette`, hands each its
  file at start, and makes the file of whoever is in front the master.
  `step_front` becomes `DROP_MASTER`, `SET_MASTER` and phase 5's revoke.
- `compositor/output.rs` and `roulette` draw into a mapped dumb buffer.
- `sysmon` reads the geometry from the card; `display_stats` keeps the
  adapter and the four counters.

Authority after phases 5 and 6: `DisplaySeat` and `InputSeat` are init's
alone, the compositor holds a descriptor for each seat and no capability
for either, and `SeatSwitch` goes, as its deletion condition in
`slopos-ostd/src/authority/mod.rs` says. The two ranks go with the calls
that took them.

Checked by: `seat_test` and `protocol_test` on the card; `sessions_test`
(two desktops, one in front); the install and desktop lanes on virtio-gpu
and on the firmware framebuffer; the hardware cursor on xe through
`scripts/remote.py`; `just check-framekernel-gates` for the three
capabilities.

Beyond the base: Xorg's modesetting driver ran on Asterinas with this
set; a Wayland compositor on wlroots or Weston wants `MODE_PAGE_FLIP` and
its event read from the card, `SET_CLIENT_CAP`, planes and properties
(`MODE_GETPLANERESOURCES`, `MODE_OBJ_GETPROPERTIES`, `MODE_GETPROPERTY`),
and a libseat backend.

Not built: `/dev/fb0`. A client that maps the front buffer loses the
damage flush virtio-gpu and xe need, and a dumb buffer with `MODE_DIRTYFB`
is what `fb_flip` does.

## Phase 7: SlopRing as io_uring

SlopRing holds io_uring's first 40 bytes of a submission, its whole
completion and its provided-buffer ring, and differs in the rest. A call
that takes Linux's idea moves to Linux's number once it takes its shape
(`docs/abi/interfaces`); the ring takes the shape.

Serves 425 to 427 for a ring set up with `IORING_SETUP_SINGLE_ISSUER |
IORING_SETUP_DEFER_TASKRUN`, the mode in which Linux posts completions
inside `io_uring_enter(IORING_ENTER_GETEVENTS)` alone, which is when
`ring/src/enter.rs` posts them. A ring asked for without both is `EINVAL`.
The features it reports leave out `IORING_FEAT_NODROP`: a full completion
queue drops and counts, as SlopRing does.

Changes, in `abi/src/ring.rs`, `ring/` and `slopos-rt`:

- `io_uring_params` with `sq_off` and `cq_off`; the rings and the
  submission array mapped by `mmap` on the ring's descriptor at Linux's
  three offsets (phase 5's hook); the index array.
- Linux's opcode numbers, flag bits, bytes 40 to 63 of a submission, and
  register numbers. `RECVFROM` becomes `RECVMSG` with a name. Multishot
  moves to where each Linux operation keeps it.
- Linux's meaning where SlopRing's differs: `READ` and `WRITE` honour the
  offset, `TIMEOUT` takes a `timespec`, `OPENAT` a directory and a mode,
  `ACCEPT` and `CONNECT` Linux's address fields, a datagram is not cut at
  4096 bytes.
- `IORING_ENTER_GETEVENTS` as the only wait.

Checked by: `ring_test`, `slopfut_test`, `multishot_test`,
`percore_reactor_test`, `pidfd_e2e_test`; host tests of the layouts
against `io_uring.h`'s offsets; the compositor's frame loop and `nc`
unchanged above `slopos-rt`.

Beyond the base: most liburing programs set up a ring in the default
mode, which needs completions posted outside `io_uring_enter` and a ring
descriptor that polls; then `IORING_REGISTER_PROBE`, registered files,
linked submissions, `READV`, `WRITEV`, `FSYNC` and `STATX`. A program
outside the base reaches the calls through `syscall`, which `libc.so`
exports.

After this phase io_uring is the ring's one interface. No opcode, flag or
register number of SlopOS's own is added to it, and the subset widens
toward what a Linux program sees, the default mode first; a request the
kernel does not serve fails as Linux says an unsupported one fails. The
ring's descriptor follows Linux as programs need it to: it polls, a fork
inherits it and a socket passes it, a second task's submission refused as
`IORING_SETUP_SINGLE_ISSUER` refuses one.

The engine under the interface stays SlopOS's. `ring/` stays synchronous
(`xtask lint no-kernel-async`), no opcode reaches a gated capability
(`opcode_capability`'s assert), the kernel trusts its own copy of each
index, and an operation that makes a descriptor or takes data runs only
while the completion queue has room. Linux's worker threads and
`IORING_SETUP_SQPOLL` are how Linux serves the interface and a flag a
kernel may refuse; neither is taken for its own sake.

## Phase 8: network settings on rtnetlink

Serves, on phase 5's family:

- `NETLINK_ROUTE`: `RTM_GETLINK`, `RTM_SETLINK`, `RTM_GETADDR`,
  `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_GETROUTE`, `RTM_NEWROUTE`,
  `RTM_DELROUTE`, `RTM_GETNEIGH`, `RTM_DELNEIGH`, for `AF_INET`; dumps
  that end in `NLMSG_DONE`, `NLMSG_ERROR` as the answer to a change, and
  the groups `RTNLGRP_LINK`, `RTNLGRP_IPV4_IFADDR`, `RTNLGRP_IPV4_ROUTE`
  and `RTNLGRP_NEIGH`. A request that changes something takes `NetAdmin`.
- `NETLINK_SOCK_DIAG`: `inet_diag` dumps for TCP and UDP. They carry a
  uid and an inode, never a process.
- `/proc/<pid>/fd/`, readable by the process's uid or with `DacOverride`,
  a socket's link naming its inode. That is where a socket's owner is
  read, and it widens today's rule from the caller's process to the
  caller's uid.
- `SO_BROADCAST` and `SO_BINDTODEVICE`, a datagram sent from an interface
  with no address and one received before it has any: what a DHCP client
  on a UDP socket needs (`net/src/dhcp/transport.rs` has them by being in
  the kernel).

Kernel work: `sendmsg` and `recvmsg` on a socket that is no `AF_UNIX`
one, `MSG_PEEK` and `MSG_TRUNC` (a netlink reader sizes its buffer with
them), and the message codec in a `*-core` crate.

Userland work:

- A network daemon in the base, started by init with `NetAdmin`: the DHCP
  client (`net/src/dhcp/` moves out, its codec to a `*-core` crate), the
  connectivity classifier (`net/src/connectivity.rs`), the master switch,
  and phase 2's writing of `/etc/resolv.conf`. It serves the lease,
  connectivity and the switch on a socket under `/run` that `netctl`,
  Settings, the compositor and `ip` read and wait on.
- `ip`, `ss`, `nmap`, `sysmon` and `network_settings.rs` on netlink;
  slibc's `getifaddrs`, `if_nametoindex`, `if_indextoname` and the
  `SIOCGIF*` requests it answers on `RTM_GETLINK` and `RTM_GETADDR`.

Removes from the kernel: the DHCP client, the connectivity classifier,
`netmon`, the query renderers.

The group moves whole. rtnetlink beside `net_query` would give init, the
compositor and Settings two things to wake on for one question.

Checked by: host tests of the codec against messages captured from
Linux's `ip`; `ip_e2e_test`, `curl_e2e_test`, `settings_test`,
`spawn_privilege_test`'s `NetAdmin` cases; a lease taken on `just boot`
before the remote agent pairs.

Beyond the base: iproute2's `ip` wants `IFLA_STATS64`, `IFA_CACHEINFO`,
strict checking and an empty dump for a family the kernel has none of,
so that `AF_INET6` ends rather than hangs; dhcpcd wants `AF_PACKET`.

## Apps and the C library

An app reaches the system through `libc.so` alone and holds no `syscall`
instruction, and that stays. After this plan an app opens a `/proc` file
with `fopen`, asks `getaddrinfo` and `getifaddrs`, and can open a netlink
socket with `socket`; none of these is a new export. It holds no seat
capability and no device descriptor, so the screen, input and the
clipboard reach it through the compositor as today (`windowing`, `appkit`
over `slopos-cabi`). `xtask lint app-deps` reads a crate's dependencies
and `xtask programs` reads the ELF for its interpreter, `libc.so` and a
`syscall` instruction; neither changes. `xtask libc-exports` changes only
if a phase adds an export.

## Not in this plan

- The two calls of the userland tests: `tests-without-a-host.md`.
- What each phase lists as *beyond the base*. It is added for the first
  ported program that needs it, under the surface rule above.
- Linux calls the kernel does not answer and that replace no private
  call: `epoll`, `inotify`, `eventfd`, `timerfd`. A ported event loop
  needs them before most of the above.
- The descriptor forms the authority module names for `Power`, `Clock`,
  `ConsoleConfig`, `Fate` and `Mount`. They change who holds authority,
  where this plan changes the shape of a call.
- IPv6.

## Where Linux's side is read

- `proc(5)`; `Documentation/ABI/stable/sysfs-block` and
  `Documentation/block/stat.rst`; `Documentation/filesystems/efivarfs.rst`;
  `Documentation/admin-guide/sysrq.rst`.
- `include/uapi/linux/input.h`, `input-event-codes.h`, `uinput.h`;
  `Documentation/input/event-codes.rst`.
- `include/uapi/drm/drm.h`, `drm_mode.h`; `drivers/gpu/drm/tiny/simpledrm.c`
  for a mode made from a firmware framebuffer.
- `io_uring_setup(2)`, `io_uring_enter(2)`, `io_uring_setup_flags(7)`.
- `netlink(7)`, `rtnetlink(7)`, `sock_diag(7)`.
- Asterinas: the 0.17.0 notes (input devices at `/dev/input/eventX`,
  December 2025) and the 0.19.0 notes (DRM core, `/dev/dri`, dumb
  buffers, KMS objects; Xorg on the modesetting driver; pull requests
  3787, 3925, 3888 and 3941; October 2026) at `asterinas.github.io`.
