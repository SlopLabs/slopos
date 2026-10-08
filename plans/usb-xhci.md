# USB

## Goal

Drive USB: an xHCI host controller driver, enumeration through hubs, and the
class drivers a development machine needs — keyboards and pointers, mass
storage and Ethernet adapters — for devices that come and go while the
system runs. QEMU's controllers come first, then the laptop's two
(`plans/bare-metal.md`). What it buys:

- An external keyboard and mouse beside the built-in ones.
- Sticks as disks: an ext4 stick mounted, written and pulled; and a live
  system that reads the install payload from the stick it booted from
  instead of holding it in RAM.
- A USB Ethernet adapter as a NIC, for a machine without an RJ45 port.

## Where it stands

Phases 1, 2 and 3 have landed. The kernel takes every xHCI controller it can
drive from the firmware and runs it. It enumerates every device on its root
ports and behind its hubs, offers each function to the drivers `usb_driver!`
registers, and removes a device cleanly when it leaves. It resets each
controller at poweroff. `usb-hid` binds keyboards, mice and tablets, which
type into the TTY and the desktop and move the one cursor beside the i8042's
and the touchpad's. No other class driver exists yet, so a stick or an
adapter is listed and left unbound; the firmware reads a stick and the
kernel does not. `just test-installer`
attaches the ISO as QEMU's `usb-storage` device on `qemu-xhci`
(`INSTALL_STICK` in `scripts/qemu_run.sh`) for the firmware alone. The install
medium reaches the kernel only as the Limine module `install`, payload
included. Limine loads it into RAM, and `fs/src/basefs.rs` serves it at
`/media/install` for the whole boot.

The pieces USB plugs into exist. These are their gaps:

- **Binding.** `driver_core::bus` drives three buses, PCI, platform and
  USB, through one `Bus` trait. A USB function is unbound when its device
  leaves, through `ClaimTable::release`; PCI and platform devices never
  are.
- **Interrupts and threads.** PCI device interrupts are MSI-X or MSI, never
  INTx. The i8042 and the touchpad's GPIO cascade are IOAPIC lines. All of
  them target the BSP. Deferred work runs on a `spawn_kernel_io!` thread,
  and each such thread takes a slot in a fixed registry of
  `MAX_KERNEL_IO_STOPS` (eight) that netpoll, net-timer, the ext2 flusher,
  the efivar thread, the touchpad and the two USB threads already draw on.
- **Device memory.** Every DMA ring in the tree is a single zeroed 4 KiB
  `OwnedPageFrame` with volatile accessors. A multi-page `DmaCoherent` run
  has no volatile accessors. The frame allocator's one placement
  constraint, below 16 MiB (`FrameAllocOptions::with_dma`), has only a
  test as a user, and nothing asks for pages below 4 GiB. No IOMMU
  translates: boot registers the identity mapper.
- **Disks.** Every disk is an `EngineDisk` over the request engine's
  `QueueOps`. It is registered with `block::register_disk` and named by
  `DiskName::virtio` or `DiskName::nvme`. `disk0`, the disk `root=auto`
  mounts, is the first disk registered, and NVMe and virtio register theirs
  inside probe. No disk is ever unregistered, and a block claim is released
  by the disk's name. No disk reports write protection, so `BLKROGET`
  answers 0 everywhere. A request waits a fixed 250 ms for a free slot,
  three times, before it fails as `Busy`.
- **Network.** A NIC is a `NetDevice` handed to `nic::publish`.
  `nic::retire` tears one down, and a test grades it, but it exists only
  under `test-hooks`. Both NIC drivers are singletons.
- **Tests.** `just test` attaches no USB controller. `just test-usb`
  enumerates sticks, a hub and HID devices, plugs and pulls them through
  QMP and injects keys and motion. CI's QEMU is the runner distribution's,
  so a USB test uses only devices and QMP commands that version carries.

The laptop has two controllers. The PCH xHCI `8086:51ed` sits at `00:14.0`
and the Thunderbolt 4 (TCSS) xHCI `8086:a71e` at `00:0d.0`. On Intel's
Type-C platforms the USB 2 lines of a Type-C port are PCH ports, and its
SuperSpeed lanes belong to the TCSS controller. The laptop's keyboard is
i8042 and its touchpad I²C-HID, so USB serves only what is plugged in and
whatever internal devices, such as Bluetooth, sit on the PCH controller.

## Phases

There are six phases. Each ends in something that runs and lands as
commits of its own. Each also ends with the security sweep `AGENTS.md`
requires over what it added, because everything a device sends is
untrusted input, and each rewrites the `AGENTS.md` and
`plans/bare-metal.md` statements its change makes false.

| Phase | Needs | Ends with |
|---|---|---|
| 1. The host controller (done) | — | both QEMU xHCI models running, every root port's attach and detach logged, the controller reset at poweroff |
| 2. Enumeration and the device model (done) | 1 | every device QEMU attaches, a hub's included, enumerated, bound or listed, and removed cleanly |
| 3. Keyboards and pointers (done) | 2 | a USB keyboard and tablet drive the shell and the desktop beside PS/2 |
| 4. Mass storage | 2 | an ext4 stick mounted, written, pulled and plugged back; the installer still installs with its stick visible |
| 5. The install medium on a stick | 4 | **milestone:** the installer takes the toolchain from the stick, not from RAM |
| 6. USB networking | 2 | a CDC Ethernet adapter takes a DHCP lease and leaves cleanly |

Phases 3, 4 and 6 can run side by side. A phase's laptop half is graded by
the user's run, or by an agent through the remote control
(`scripts/remote.py`) within the bounds `AGENTS.md` sets for it.

### Phase 1: The host controller (done)

Built:

- **`usb-core`'s xHCI half**: the register, extended-capability, TRB and
  context codecs (both context sizes), ring bookkeeping with a table of
  outstanding commands, and the handoff, halt, reset, configure and run
  sequences over a `RegisterBus` with named waits. Host tests run them
  against a simulated controller in QEMU's shape and in one with what
  QEMU's lacks (64-byte contexts, 300 scratchpads, USB Legacy Support,
  switched port power, §4.19.2's port events), a BIOS that never lets go, a
  controller without 64-bit addressing, a Host System Error with a command
  outstanding, and mutated registers.
- **The `xhci` PCI driver in `drivers/src/usb`**: a probe that takes each
  controller from the firmware, resets and runs it as **Decided** below
  states, a No Op command as proof of the rings, the `usb` thread, a
  shutdown hook per controller, the dead-controller path, the `usb` knob,
  and kernel-log lines per controller and per root port change.
- **PCI**: `set_power_d0` lives in `drivers/src/pci.rs` and writes back the
  BARs a reset out of D3hot lost, and BAR sizing runs with decode off on
  every function but a host bridge.
- **`just test-usb`**, in CI after `test-rude-exit`. It measured 44 of the 64
  dynamic MMIO ranges free, and 23 with the suite's disks attached too.

**On the laptop (done).** A `usb=report` boot found both controllers on PCI,
each xHCI 1.20 with 64 slots, 32-byte contexts, 64-bit addressing, 34
scratchpads and USB Legacy Support: the TCSS `00:0d.0` with USB 2.0 port 1
and USB 3.2 port 2, the PCH `00:14.0` with USB 2.0 ports 1-12 and USB 3.1
ports 13-16. A `usb=on` boot took both from the firmware and ran them on
MSI, logging the PCH's internal devices on ports 2-8, 2-9 and 2-10. A reboot
from it reached the next kernel in 14.9 s, as reboots from `usb=report`
boots did (14.7-15.6 s).

### Phase 2: Enumeration and the device model (done)

Built:

- **`usb-core`'s device half**: the standard requests, the descriptor
  walker (functions from interfaces and their associations, alternate
  settings, endpoints, SuperSpeed companions, class descriptors, strings),
  the hub descriptors, port status and class requests of both generations,
  and `usb_core::bus`, the tree: a state machine over a `Host` trait that
  steps root and hub ports, enumeration, configuration, endpoint recovery
  and removal on completions and deadlines. Host tests drive it through the
  simulated controller and simulated devices: high-speed hubs with
  transaction translators and full-speed hubs behind them, USB 3 hubs,
  stalls, babble, misstated lengths, ports that flap, disconnects
  mid-enumeration, the power rule, a controller that dies mid-transfer, and
  mutation loops over every parser.
- **Enumeration in `drivers/src/usb`**, stepped by the `usb` thread as
  **Decided** below states. A device's slot, contexts and rings are pages of
  its own, and hubs name their transaction translator, route string and
  depth in their children's contexts. A multi-TT hub is run with one
  transaction translator.
- **`UsbBus`**, the third bus: a `UsbFunction` snapshot per function, drivers
  in `.usb_driver_registry` through `usb_driver!`, and a
  `BoundDevice<UsbBus>` that vends control requests, page-sized pipes, the
  configuration's descriptors and a removal. Probes and removals run on the
  `usb-bind` thread, and removal runs the six steps **Decided** states.
- **Settle and diagnostics**: `usb::wait_settled`; the `usb settle` boot
  step, which under `tests=on` waits a minute and fails the run with `USB:
  unsettled`; a kernel-log line per device and per failure; and the
  informational kconsole command `u`.
- **`just test-usb`**: on both models a SuperSpeed and a high-speed stick, a
  full-speed keyboard, and QEMU's hub with a stick and a tablet behind it.
  `test-hooks` drivers bind every stick, send TEST UNIT READY over its bulk
  pipes and recover a stalled bulk-in, and bind the keyboard and tablet to
  abandon a read of the idle interrupt endpoint and see the endpoint come
  back. Two rounds of `device_del` and `device_add` leave no slot, page or
  claim behind, and the host grades the `u` listing. The run left 44 of the
  64 dynamic MMIO ranges free.

Left for later phases: `usb.settle_ms` arrives with its first waiter in
phase 4, enumeration reads no string descriptors, and a halted full- or
low-speed endpoint behind a high-speed hub sends its transaction translator
no `CLEAR_TT_BUFFER`, which phase 4's Bulk-Only recovery adds.

**On the laptop (not yet run),** every device on both controllers is listed,
including a low-speed device, devices behind a high-speed hub reached
through its transaction translator, and devices behind a USB 3 hub. Internal
devices such as Bluetooth are listed unbound. A SuperSpeed stick in a
Type-C port is measured: it may reach the TCSS controller, reach the PCH
controller at USB 2 speed, or reach neither.

### Phase 3: Keyboards and pointers (done)

Built:

- **`hid-core`**, HID independent of transport and host-tested with mutation
  loops: report descriptors parsed into storage the caller passes (push and
  pop, report IDs, delimiters, extended usages, variable and array fields,
  signed and relative fields, outputs and features), the boot keyboard and
  mouse reports, what a keyboard report holds and the steps between two,
  what a pointer report says, and the LED output report. The touchpad parses
  through it, and `usb-core` gained the HID class descriptor and requests.
- **The keyboard layer**, `drivers/src/keyboard.rs`, holding the machine's
  keyboard state as **Decided** below states. `ps2::keyboard` decodes set 1
  into it and keeps the i8042's LED exchange; the layout syscalls read it.
- **One cursor** in `input_event`, as **Decided** below states. The PS/2
  mouse and the touchpad move it; the touchpad scales by the bounds the
  video layer publishes rather than the boot framebuffer's.
- **`usb-hid`**: boot keyboards, report-protocol keyboards, mice and
  tablets, each interface holding a keyboard slot, a pointer slot or both.
  Report endpoints and posted control requests in `drivers/src/usb/xhci`
  carry its reports, repeats and LEDs as **Decided** states.
- **Tests**: `utest!(…, explicit)` and `stest!(…, kind = Userland)`; and in
  `just test-usb` a usb-mouse behind the hub, every posted report
  abandoned and recovered, keys and motion injected through
  `input-send-event`, and `usb_shell_test`, a shell typed at.

**On the laptop (not yet run),** an external keyboard and mouse work beside
the internal keyboard and touchpad.

### Phase 4: Mass storage

- **The storage part of `usb-core`:** Bulk-Only Transport's Command Block
  Wrapper (CBW) and Command Status Wrapper (CSW) with their validity and
  meaning rules, SCSI command builders, fixed-format sense data, READ
  CAPACITY (10) and (16), and the WP bit of the mode parameter header.
- **`usb-storage`, a `UsbBus` driver.** It binds class `0x08`, subclass
  `0x06`, protocol `0x50`, and a USB Attached SCSI (UAS) device's Bulk-Only
  alternate setting.
  - It sends `GET MAX LUN`, and takes a STALL as a single LUN.
  - For each LUN it sends INQUIRY, then TEST UNIT READY until the LUN is
    ready or `usb.settle_ms` has passed since the LUN was probed, then READ
    CAPACITY, and MODE SENSE for write protection. A LUN that is still not
    ready is declined with a log line.
  - It is a `QueueOps` transport, with one engine per device; `nsid` is the
    LUN. The engine has two slots: one command on the wire and one queued
    behind it, so the engine suite's concurrent submissions hold. The queue
    is the transport's own, behind its own lock. `submit` appends to it and
    returns the tag. The bulk pipes carry one CBW, data stage and CSW at a
    time, and the drain starts the queued command when a CSW completes.
  - `pop` never touches the event ring. It returns only what the drain has
    already put in the transport's completion queue.
  - `Engine::init` takes the slot wait, which is a fixed 250 ms today.
    `usb-storage` sets it to cover the command ahead plus its recovery, so
    a request queued behind a slow, healthy command is not answered `Busy`,
    which ext4 would count as a device error.
  - Reads and writes use READ/WRITE (10), and (16) past 2³² blocks.
    SYNCHRONIZE CACHE is the flush.
  - A CSW that is not valid, a Phase Error, or a stall of bulk-OUT during
    the CBW hands the device to the USB thread for Reset Recovery: a
    Bulk-Only Mass Storage Reset, then `CLEAR_FEATURE(ENDPOINT_HALT)` on
    bulk-IN and on bulk-OUT, and `CLEAR_TT_BUFFER` to the transaction
    translator a full-speed stick behind a high-speed hub is reached through
    (USB 2.0 §11.24.2.3). If that fails, recovery escalates to a port
    reset and re-enumeration. During recovery the transport keeps the tag,
    queues new commands rather than answering `Busy`, and re-issues the
    CBW once. A recovery that fails completes the tag with a non-retryable
    error.
  - A stalled data stage is cleared and the CSW is then read. A CSW that
    reports Command Failed is followed by REQUEST SENSE, and the sense key
    decides: UNIT ATTENTION and NOT READY are retried, while MEDIUM ERROR,
    DATA PROTECT and ILLEGAL REQUEST fail the request.
  - The host side of a halt is cleared too. A pipe the controller reports
    Halted takes Reset Endpoint first. After each
    `CLEAR_FEATURE(ENDPOINT_HALT)`, which resets the device's data toggle
    or sequence number, a Configure Endpoint that drops and adds the pipe
    resets the controller's. Set TR Dequeue Pointer then skips the failed
    command. Without that reset, the first packet after recovery carries a
    stale toggle and the device discards it.
  - Every command has a USB-side deadline, counted from its CBW. The USB
    thread ends a command past it: Stop Endpoint on both bulk pipes, the
    recovery above, and Set TR Dequeue Pointer past the command. The
    engine's timeout covers both slots' commands, each with one recovery
    and one re-issue, so the engine quarantines a Bulk-Only request only
    when a kill outlasts `UNINTERRUPTIBLE_MAX_MS`. The transport then
    completes the quarantined tag when its CSW or recovery ends, which
    returns its pages and lifts the abandoned-write fence.
- **Block layer changes.**
  - `DiskName::scsi` names USB disks `sda`, `sdb` and so on, reusing the
    lowest free letter. Partitions are `sda1`.
  - `block::unregister_disk` removes a disk.
  - Block claims carry the disk's generation.
  - `EngineDisk` reports write protection, and the node view that partition
    nodes wrap, `DiskReader`, forwards it. `BLKROGET`, the installer and
    ext4's read-only verdict all see it.
  - `root=auto` and `root=disk` resolve to the first disk that is not a USB
    disk.
  - `fs init` and `cmdline mounts` wait for USB to settle when the device
    `root=` or a `mount=` names is absent. The crash store never waits.
- **The shutdown hook** gains the storage drain in front of the halt
  (Decided).
- **The installer never offers its own medium.** The ISO is built with a
  GPT disk GUID chosen for that build, through xorriso's `--gpt_disk_guid`
  (checked with `--protective-msdos-label`, which `scripts/build_iso.sh`
  passes), and the `install` module records it. `/bin/installer` never
  offers a disk whose GPT disk GUID is the medium's, nor a write-protected
  disk, and `installer_test` picks its target with the same filter. One
  `test-installer` run attaches the stick writable, so that the GUID rule
  is the one that excludes it.
- **The default test lane.** `qemu-xhci` joins `just test` at a fixed PCI
  address after the suite's NVMe and virtio devices, with a scratch stick.
  The phase measures how many of the dynamic MMIO ranges `just test` leaves
  free: each controller takes up to three, BAR0, the MSI-X table and the
  PBA. If fewer than eight are left, `register_io_mem_range` stops appending
  a range an existing entry already contains.
  The stick is a fourth arm of `on_scratch!`. `msix_tests` picks its device
  by identity rather than by enumeration order.

**Done when** all of these hold:

- the engine suite, `concurrent_requests` included, passes on `sda` beside
  `vdb` and the NVMe scratch disks;
- in `just test-usb`, an ext4 stick is mounted, written and fsynced, then
  pulled while writes are in flight. The mount turns read-only: every
  mutation answers `EROFS`, every call that reaches the device fails at
  once rather than waiting out the engine's timeout, and `umount` releases
  the mount. Plugged back, the stick is `sda` again and still holds what
  was fsynced;
- a read-only drive reports write protection and mounts read-only;
- `nvme0n1` stays `disk0` with a stick attached;
- `just test-installer` installs with its stick visible as `sda`.

The host tests must also drive the transport through stalls, Phase Errors,
CSWs that are not valid, a device that never answers, and a slow command
with another queued behind it. The simulated controller tracks data
toggles and fails a transfer whose toggle does not match, because QEMU's
`usb-storage` does not model them.

**On the laptop,** a stick is read and written on a Type-A port and on a
Type-C port.

### Phase 5: The install medium on a stick (milestone)

- **The payload partition.** `PAYLOAD=1` puts the toolchain, its manifest
  under `var/lib/slopos/trees` and the clone in an ext4 volume.
  - The volume is built in the profile, populated, then given ext4's
    `read-only` feature with `tune2fs -O read-only`, since `mke2fs` refuses
    the feature at creation. Linux mounts such a volume read-only and
    refuses to remount it read-write, and the kernel takes it as a
    read-only verdict (`requires_readonly`), so no other system leaves its
    journal needing recovery.
  - It is appended to the ISO as a GPT partition of a new SlopOS payload
    type, which `boot-core::layout` defines beside `CRASH_TYPE`. xorriso's
    `-append_partition` takes the type GUID, and `-appended_part_as_gpt`
    puts the partition in the GPT, where `block::locate_partition` looks.
  - The `install` module, still the one Limine loads, keeps the rest:
    Limine, the licences, the notices, the recipe sources and the medium's
    GPT disk GUID.
- **Mounting it.** The install-medium boot step finds the payload as the one
  partition of the payload type on the disk whose GPT disk GUID the module
  records, through `block::locate_partition`, as the crash store finds its
  partition. It waits for USB to settle if the disk is absent, then mounts
  the partition read-only and pinned at `/media/payload`. If the payload is
  still absent, or two disks carry the medium's GUID, the live system logs
  it and installs without the toolchain, as `INSTALLER_PAYLOAD=0` does.
  The mount holds one of the four ext4 mount slots for the boot. A stick
  pulled during the session leaves the pinned mount failing every read that
  reaches the device until reboot, and an install in progress fails,
  naming the payload.
- **The installer.** `Medium::find` keeps its basefs check and the kernel,
  base and loader files at `/media/install`. It checks the toolchain with
  its manifest, and the clone, at `/media/payload`. `root_min` sizes the
  root from `/media/payload`'s `statfs`, and `installer_test`'s clone
  probe reads `/media/payload/src`.
- **The ISO's table.** A host test holds the built ISO's GPT to the kernel's
  and `boot-core`'s parsers, both of which keep the ESP and the payload
  entries.
- **A medium the kernel cannot read.** `qemu_run.sh` gains `INSTALL_CDROM`,
  which attaches the ISO as an `ide-cd` with `bootindex=0` beside
  `BOOT_DISK_IMG`. Today the CD drive is attached only when neither
  `INSTALL_STICK` nor `BOOT_DISK_IMG` is set.

**Done when** `just test-installer` installs from a stick whose payload the
kernel reads over USB, while the module Limine loads carries no toolchain.
A reinstall must keep the root. The same medium booted through
`INSTALL_CDROM`, which the kernel cannot read, must install without the
toolchain: the host requires the installer's `the medium carries no
toolchain` line in that run's log, and no `INSTALLER-BUILT` in the disk
boot that follows.

**On the laptop,** a stick made with `PAYLOAD=1 just iso` installs SlopOS
beside CachyOS, toolchain included, from a live system that never holds
the toolchain in RAM.

### Phase 6: USB networking

- **The CDC part of `usb-core`:** the header, union and Ethernet functional
  descriptors, the MAC address string, NCM's NCM Transfer Block (NTB16)
  header and datagram pointer tables, and `GET_NTB_PARAMETERS`.
- **`usb-net`, a `UsbBus` driver** for CDC-ECM and CDC-NCM functions, one
  instance per device.
  - It selects the data alternate setting and posts bulk-IN transfers
    before it calls `nic::publish`, with no lock held.
  - `tx` copies the frame into a bulk-OUT buffer, framed as an ECM frame
    (ended by a zero-length packet when needed) or as an NTB, and returns
    without waiting.
  - Completions wake netpoll.
  - Carrier comes from `NETWORK_CONNECTION` notifications and is kept in an
    atomic. The parser checks the request type, the notification code and
    `wValue`, but not that `wIndex` names the control interface, because
    QEMU's `usb-net` sends its data interface's number. A notification that
    repeats the current state wakes nothing, since QEMU answers every poll
    with one.
  - A function whose `wMaxSegmentSize` is under 1514 is declined, because
    the stack assumes a 1500-byte MTU.
- **`nic::retire` in production.** Removal runs it.

**Done when** QEMU's `usb-net` device in `just test-usb`, in its ECM
configuration (it lists RNDIS first), takes a DHCP lease from a SLIRP
network of its own (`net=10.0.3.0/24`) and fetches over TCP through
10.0.3.2, which only the USB interface's route reaches, beside virtio-net.
`device_del` must retire its interface, along with its routes, neighbours
and DHCP client, while the other NIC keeps its name.

**On the laptop,** a USB-C Ethernet adapter in its class configuration
takes a lease, and git fetches over it.

## Grading

- **Host:** `usb-core` and `hid-core` under `just test-host`. The simulated
  controller and devices run:
  - both context sizes, with scratchpad buffers;
  - the firmware handoff, and a controller that dies;
  - hubs with transaction translators, and USB 3 hubs;
  - stalls, babble, short packets and disconnects;
  - the storage recovery, with data toggles tracked.

  Every parser of device input also runs deterministic mutation loops over
  its inputs, and must neither panic nor read past them.
- **`just test`:** one `qemu-xhci` with a scratch stick, from phase 4.
- **`just test-usb`:** a boot of its own with a QMP socket, which
  `qemu_run.sh` opens when `QEMU_QMP` names one and `scripts/test_usb.py`
  drives. Its guest half is kernel tests registered `FLAG_EXPLICIT |
  FLAG_UNCAPTURED`, which the recipe names in `tests.run`, and, from phase
  3, explicit userland tests. It carries:
  - both QEMU models, the NEC one on MSI, each with two USB 3 and four USB 2
    root ports;
  - a stick on each connector, each a `-blockdev` node, since QEMU deletes a
    `-drive` backend together with the device that used it;
  - a hub;
  - `usb-kbd`, `usb-mouse` and `usb-tablet`, with keys and motion injected
    through QMP;
  - ext4 sticks that are plugged and pulled;
  - QEMU's `usb-net` device.

  QEMU sends an event that names no display to the unbound input device
  activated most recently, and one that names a display to a device bound
  to it. The test binds `usb-kbd` and `usb-tablet` to a stdvga given the id
  `video0`, which must come before them on the command line, so a key or a
  button that names no display reaches the i8042; `-parallel none` keeps a
  text console from aborting QEMU's lookup. Motion that names none reaches
  the usb-mouse our probe's `SET_IDLE` activated, which cannot be bound,
  and the PS/2 mouse once it is unplugged. QEMU's HID devices never repeat
  a key and its PS/2 keyboard has no typematic, so the host presses a key
  twice to stand in for one; it merges a button's press and release sent
  in one command, so each goes alone. The i8042's LEDs are graded from
  QEMU's `ps2_set_ledstate` trace, a USB keyboard's from the output report
  it acknowledged. The host drives the test at markers the guest prints, as
  `just test-remote` does, and holds each stick image to `e2fsck -fn`
  afterwards. A kernel test registered with `kind = Userland` runs after
  the userland ones, which is how the controllers are reset only after the
  shell has been typed at. The test runs in CI's `ci` job after
  `test-rude-exit`, because only that job has the tests build, and its warm
  cost is held under a minute.
- **`just test-installer`:** phases 4 and 5.
- **The laptop** grades what QEMU cannot:
  - Intel's controllers and the firmware handoff;
  - scratchpad buffers;
  - a high-speed hub with its transaction translators, and USB 3 hubs;
  - low-speed devices;
  - the Type-C split.

  QEMU's controller reports 32-byte contexts, no scratchpads and no USB
  Legacy Support capability. It accepts any EP0 packet size, its only hub
  is full-speed, and its `usb-storage` keeps no data toggles. A mistake in
  any of these passes CI, so the simulated controller and the laptop are
  the grade for each; the laptop's controllers use 32-byte contexts too, so
  64-byte contexts are the simulated controller's alone.
- **Ratchets:** any lock class, test or account the driver adds is
  re-measured in the commit that adds it, as `AGENTS.md` requires.

## Out of scope

- **EHCI, OHCI and UHCI** (Decided: xHCI only).
- **Controllers that come and go.** This covers PCI hotplug, docks, USB4
  and Thunderbolt tunnelling, the Thunderbolt NHI, and Type-C power
  delivery and alternate modes. Where a Type-C port's lanes land is
  measured in phase 2; driving the Type-C mux through the Power Management
  Controller's IPC is not planned.
- **Isochronous transfers:** audio and cameras.
- **Other classes.** Bluetooth, USB serial, printers, smart cards and MTP
  get no driver, nor do vendor NIC modes or RNDIS (Decided).
- **UAS.** At SuperSpeed it needs bulk streams, and at high speed its own
  command queueing. A UAS device's Bulk-Only alternate setting serves
  instead.
- **Power management:** suspend, runtime power management, U1/U2 link power
  management and remote wakeup.
- **Userland USB access** (Decided).
- **Media change in card readers.** The disk is the medium present at
  enumeration.
- **Kernel-mounted FAT and exFAT.** A FAT stick gets its nodes and its
  `by-label` link, but nothing mounts it.
- **USB on the panic path** (Decided).
- **A graded boot from a USB root.** `root=PARTUUID=` reaches one once USB
  has settled, but no test boots one, and such a system keeps no crash
  record.
- **Consumer-page keys** (media keys), which have no keycode in the ABI.

## Decided

- **xHCI only.** Every USB 2 and USB 3 device works behind it, the laptop
  has no other kind of controller, and QEMU recommends it.
- **The formats are data; the driver is `drivers/src/usb`.** xHCI, the
  descriptors, hubs, Bulk-Only and SCSI, and CDC live in `usb-core`. HID
  lives in `hid-core`, because I²C-HID and USB HID share the report
  format. Both crates are alloc-free and host-tested, like `nvme-core` and
  `rtl8168-core`. The USB core holds the rings, the USB threads and the
  bindings.
- **MSI-X or MSI, one interrupter.** Interrupter 0 has one event-ring
  segment of one page. QEMU's model accepts nothing else: it dies on a
  segment table of any size but one. A controller that offers neither MSI-X
  nor MSI is declined from config space, before probe powers or decodes it,
  as `rtl8168` declines one.
- **A declined controller stays the firmware's.** Probe declines before the
  handoff, so a controller it cannot drive keeps its owner, its bus
  mastering and any keyboard emulation the firmware gives it; at most its
  power state has moved to D0. One that would find no dynamic MMIO range
  left for BAR0, or for an MSI-X table when it has no MSI to fall back to,
  is declined before probe touches it at all.
- **The firmware hands the controller over.** The driver sets OS Owned and
  waits for BIOS Owned to clear, for at most a second, which is the
  specification's bound. Past that it clears BIOS Owned itself. Either
  way, it then turns off every SMI enable and acknowledges the SMI events,
  as Linux and Haiku do, because some firmware reports a clean handoff and
  leaves its SMIs armed. Bus mastering stays as the firmware set it until
  the handoff ends, because the firmware's SMI handler may need DMA to stop
  its own schedules. Then the driver halts the controller, waits for it to
  stop, turns bus mastering off, sets HCRST, waits 1 ms before touching
  any register (some Intel controllers hang the machine otherwise), and
  waits for HCRST and Controller Not Ready (CNR) to clear. Bus mastering
  comes back on just before the controller is given its memory, since
  writing ERSTBA makes it read the segment table by DMA, which QEMU's model
  does at once and fails as a Host Controller Error with mastering off. The
  handoff ends any PS/2 emulation the firmware's SMM gave a USB keyboard.
  The laptop's built-in keyboard is a real i8042 and is unaffected.
- **Rings and contexts are single pages.** Each is a zeroed
  `OwnedPageFrame`, so no ring or buffer can cross a 64 KiB or page
  boundary, and the CPU reads device-written TRBs through volatile
  accessors. This takes on the identity-mapper assumption that every ring
  in the tree already carries. The scratchpad buffer array is one page too,
  which is why a controller asking for more than 512 buffers is declined.
  Transfer data uses the block engine's 4 KiB pages, one TRB each. A
  controller without 64-bit addressing is declined until the frame
  allocator has a below-4 GiB constraint that page allocation can ask for.
  QEMU's and the laptop's controllers have 64-bit addressing. 64-bit
  registers are written as one qword, as the specification
  asks of a 64-bit controller.
- **The drain owns the event ring, and nothing drains it under an engine
  lock.**
  - The event ring is the only completion path for every device on a
    controller. The interrupt handler drains it under the controller's
    event lock, boundedly and allocating nothing, into per-endpoint
    completion queues.
  - Once it has released the event lock, it wakes whatever consumes each
    queue: the block engine's waiters, through `Engine::handle_irq` as
    NVMe's and virtio-blk's handlers wake them; netpoll; or the USB threads.
    HID reports are decoded after the wakes.
  - A driver that reads reports keeps one transfer posted on its interrupt
    endpoint (`Reports`). The drain marks the endpoint done and, with the
    event lock released and the device table's held so the device cannot
    be freed under it, copies the report out, posts the next transfer and
    hands the report to the driver's `ReportSink`, which may not block,
    allocate or log. An endpoint the tree recovers is posted again from its
    recovery, whatever the halt left posted, until it has halted four times,
    each within a second of the last, with no report between; then it stays
    quiet, and the log says so. A posted request still out after five
    seconds is abandoned, as a waited one is. A reopened endpoint keeps the
    buffer it had, and a `Posted` request's data stage lives as long as the
    device, so a transfer abandoned under either never reaches a freed
    page.
  - A transport's `pop` reads only its own completion queue. A lost
    interrupt is recovered by the USB thread, which runs the same drain on
    every pass, at least once a second.
  - The lock order is: `BlockEngine.state` or the event lock, never both;
    then a transport's queue lock; then an endpoint's ring. No engine lock
    is held while the event lock is taken, and HID decoding and every wake
    happen with neither held.
- **Two threads serve every controller.**
  - The USB thread steps commands, port changes, control transfers,
    recovery, deadlines and enumeration as state machines, on each
    completion or deadline. It never waits on a completion, so a device
    that answers late delays only itself.
  - The bind thread runs what may block: driver probes and removals, and
    the table read inside `block::register_disk`. A transfer a probe issues
    completes from the drain and wakes it, so recovery, deadlines,
    enumeration and key repeat go on while a probe or a table read waits.
  - The USB thread also keeps each USB keyboard's repeat, at a deadline it
    parks to, and the LEDs (**Lock LEDs** below). A control request it may
    not wait for goes out as a `Posted` request, one at a time, collected on
    a later pass.
  - USB takes two slots of the fixed stop registry, however many
    controllers a machine has.
  - Probe and the shutdown hooks wait for the controller by spinning, and
    drain the ring themselves when the interrupt does not. Probe runs on
    the BSP's boot context, which has no current task, so a wait there
    answers `WaitAbort::NoRuntime`. The shutdown hooks run after the kernel
    I/O threads have stopped.
- **A command that never completes kills the controller.** Commands run in
  ring order, so one that never completes holds up every command behind
  it. A working controller completes each in milliseconds, so after five
  seconds the controller is taken for dead rather than sent a Command Abort:
  halted, taken off the bus, and every device on it removed without a
  command.
- **A halt is cleared on both sides.** An endpoint a transfer halted takes
  Reset Endpoint, which resets the controller's data toggle. A bulk or
  interrupt endpoint is halted on the device too, and takes
  `CLEAR_FEATURE(ENDPOINT_HALT)`, which resets the device's toggle, once EP0
  can carry it. Only then does Set TR Dequeue Pointer move its ring past
  what the halt left, and the ring run again. An endpoint that was only
  stopped keeps both toggles. EP0 is recovered before anything else. Each
  endpoint counts its own failed steps since it last ran. Three leave it
  halted without holding up EP0 or any other endpoint. Three on EP0 remove
  the device, and its port tries it again.
- **An abandoned transfer stops its endpoint.** A transfer whose waiter
  gave up may still be on the controller's ring, so its endpoint takes no
  transfer until the USB thread has stopped it and moved its dequeue pointer
  past what was abandoned, and nothing reuses a buffer under the controller.
- **A controller proves its rings before it is used.** Once it runs, probe
  sends a No Op command and gives its interrupt 200 ms to deliver the
  completion, then polls for 500 ms more. One that answers only when polled
  is kept, logged and served by the thread's drain; one that does not
  answer is reset, taken off the bus and left unused, so a broken command
  ring, event ring or doorbell surfaces at probe rather than as a stalled
  enumeration.
- **The `usb` knob.** `on` is the default. `off` binds no controller and
  touches none. `report` logs each controller's capabilities, protocols
  and ports and leaves it to the firmware. The last `usb=` token naming a
  mode decides, as for every knob the built-in command line can override;
  one naming none is logged and ignored.
- **Names.** Controllers are numbered from 1 in probe order and a root
  port is `<controller>-<port>`, a hub port appending `.<port>`, as the
  kernel log and the kconsole listing name them. Each device gets one
  kernel-log line once enumerated, such as `USB: 1-3.2 046d:c52b 3
  functions, bound usb-hid`. The kconsole command `u` lists controllers,
  ports, devices, bound drivers and each endpoint's queue, so `just remote
  run -- kconsole u` reads a machine nobody is sitting at.
- **Enumeration is the USB core's, one device at a time.** Each controller
  has at most one device in the default state. A port is debounced, reset
  where its protocol needs it, given a slot and an address, and EP0 is
  sized from the port's speed or, at full speed, from the device
  descriptor's first eight bytes and an Evaluate Context. The device's
  descriptors are then read and a configuration set. Every wait is bounded
  by USB 2.0's chapter 7 and 9 timings. A port whose device fails three
  times is disabled until it is unplugged.
- **Hubs belong to the USB core.** A hub's slot must be marked as a hub
  before any child of it is addressed, and its children are the USB core's
  enumeration work. A hub is therefore never offered to `UsbBus`. It is
  configured before it is asked anything as a hub, since a hub's answer to
  a class request is undefined until then, and a second Configure Endpoint
  then marks its slot a hub. A bus-powered hub's port offers one unit load,
  and a configuration that asks for more is not set. A hub that fails eight
  requests between two port statuses, or any of whose endpoints stays
  halted, is removed, and its port tries it again as it would any device
  that failed.
- **`UsbBus` is a third linker-registered bus.** It shares the binding
  protocol, the claim table and the `Devres` bag with PCI and platform, so
  a class driver added later touches no central list. The cost is a
  `RegistryId` in `slopos-ostd`, a `link.ld` section, and rows in the
  registry and expansion gates; touching `slopos-ostd` brings `just
  check-miri` and `just verify` with it.
- **Configuration choice.** The USB core sets the first configuration in
  which a registered driver matches a function, or else the first
  configuration. QEMU's `usb-net` lists RNDIS first, and an RTL8153 does
  not always list its vendor configuration first.
- **No RNDIS.** It is insecure by design against a hostile device. Linux's
  USB maintainer proposed in 2022 to disable every RNDIS driver, host and
  gadget, for that reason. ECM and NCM cover the class-mode adapters.
- **Unbind exists for USB devices only.** It is built on the seam the
  framework left for it: the `Binding` drops before the `Devres`, and a
  `ClaimTable` release that only the USB bus calls. PCI and platform
  devices are still never unbound. A removed device's transfers complete at
  once with a non-retryable status, so an engine never waits out a timeout
  on a device that is gone.
- **Removal is six steps, in order.**
  1. The device is marked gone, so every submission answers at once and no
     doorbell of its slot rings again.
  2. Its endpoints are stopped.
  3. Every outstanding transfer completes with a disconnect status that
     maps to a non-retryable error.
  4. Its claims leave the claim table and each `Binding`'s removal runs on
     the bind thread, with no lock held.
  5. Disable Slot runs and is waited for; one that never completes kills
     the controller.
  6. The slot's DCBAA entry is cleared and the claim dropped, the `Binding`
     first and the `Devres` last, which frees the rings and contexts.

  A controller that dies removes every device on it the same way, issuing
  no command. A driver whose objects outlive its binding keeps what they
  touch outside the `Devres`: a stick's engine lives until the disk's last
  mount and node are gone, so the transport's completion queue and gone
  flag are its own, and once the device is gone it touches no ring or
  context page.
- **USB enumerates after PCI, and it settles.** `disk0`, `eth0` and which
  disk a PARTUUID or UUID two disks share resolves to are all decided by
  registration order, and a stick written from a SlopOS disk image carries
  that disk's PARTUUIDs. Enumeration therefore starts once every PCI driver
  has probed. Identity order is the order of devfs nodes, which a table
  re-read renews, so after the installer re-reads the internal disk a
  stick's duplicate would precede it. Nothing the boot resolves depends on
  that order once the internal disk has been re-read. `fs init` for
  `root=`, `cmdline mounts` and the install-medium step wait for USB to
  settle only when the device they name is absent; under `tests=on` the
  kernel tests wait too. No other boot step waits for USB.
  - The bus is *settled* once every root and hub port has been powered for
    its power-good time plus the 100 ms a device may take to signal attach
    and has since been quiet for a debounce interval, and every device that
    connected has reached an end: a hub configured with its own ports
    settled, each function bound, declined or matched by no driver, a
    failing port given up and, from phase 4, every disk a bound driver
    registers with its table read.
  - A boot step waits up to `usb.settle_ms` (default 5000), counted from
    the start of its own wait. Boot steps run on the BSP with no current
    task, so the wait polls while the USB threads run on the APs. With one
    CPU those threads cannot run before the BSP enters the scheduler, so
    the wait is skipped and the kernel log says so.
  - Under `tests=on`, the `usb settle` step ahead of the boot lockdep
    report waits up to a minute, so the report's exact class caps and the
    kernel tests see the same bus on every run. A bus still unsettled then,
    or a one-CPU run with a controller, prints `USB: unsettled` and fails
    the run.
- **`root=auto` and `root=disk` never take a USB disk.** Each means the
  machine's own disk, and a stick left in a port must not change what
  boots. A root on USB is named with `root=PARTUUID=` and waited for.
- **USB disks are `sd<letters>`, as Linux names SCSI and USB disks.** A
  letter is reused once its disk is gone, as on Linux. That is why block
  claims, which are released by name, carry the disk's generation.
- **A disk that is gone answers at once.** Its mounts turn read-only
  through ext4's `errors=remount-ro`. They answer `EROFS` to every
  mutation and fail every call that reaches the device, and they stay until
  they are unmounted. Nothing unmounts them by force, as on Linux.
- **Bulk-Only is cautious.**
  - Requests are cut at 120 KiB, Linux's default for USB mass storage,
    which it keeps to work with as many devices as possible.
  - A device that refuses MODE SENSE is taken as writable.
  - A device that answers SYNCHRONIZE CACHE with ILLEGAL REQUEST has no
    cache, and is not asked again.
  - Recovery runs on the USB thread, because `QueueOps` runs with
    interrupts off and may not block. It escalates to a port reset, which
    QEMU's `usb-storage` needs after a direction mismatch.
  - The quirk table starts empty. A device that fails even 120 KiB, or
    needs any other exception, joins it once one is seen, as Linux's
    32 KiB entries do and as a Realtek version joins `rtl8168-core`'s
    table.
- **One keyboard state for the machine.** Every keyboard feeds
  `drivers/src/keyboard.rs` `(source, usage, pressed)` steps: the i8042 is
  a fixed source, a USB keyboard claims one of `MAX_KEYBOARDS`. Locks and
  the layout are shared by every keyboard, as on Linux's console.
  Modifiers are merged by counting, as Linux's are: a modifier is held
  while any keyboard holds it, so a release on one keyboard, or the
  releases sent when one is removed, never lift a modifier another keyboard
  holds. A press of a key its source already holds is a repeat: delivered
  with `KEY_FLAG_IS_REPEAT`, and toggling no lock. Each source keeps its
  own repeat: the i8042 keeps its typematic, and a USB keyboard gets
  `KeyRepeat`'s `REPEAT_DELAY_MS` and `REPEAT_INTERVAL_MS` for the key it
  pressed last. Input events stay anonymous on the ABI; the legacy byte of
  a key event is its set-1 make code whichever keyboard pressed it. The
  kernel keeps per-source state only where sources must merge: held
  modifiers, held buttons, and held keys to release on removal.
- **Lock LEDs.** Every keyboard is told when the locks change. The i8042's
  exchange is started from wherever the locks changed and carried on by its
  own interrupt, each ACK sending the next byte, as Linux's libps2 does, so
  nothing masks its lines or polls port 0x60 once it is running. A byte the
  keyboard answers with RESEND, or leaves unanswered for 250 ms, is sent
  again, twice at most; an unanswered one is noticed at the keyboard's next
  byte, the next lock change or the USB thread's next pass, so a machine
  without xHCI has no clock behind it. A USB keyboard is sent the locks when
  it binds and at every change, one output report at a time, through
  `SET_REPORT` on EP0, from a table of the eight lock states built at bind.
  A lock state a keyboard did not take, the i8042's included, is not sent
  to it again until the locks change; a USB request cancelled by another
  request's halt was not refused, and goes again.
- **One cursor.** `input_event` owns the pointer's position and the screen
  bounds the video layer publishes; the first bounds centre it. A pointing
  device claims a `PointerSource`, the PS/2 mouse and the touchpad fixed
  ones, and reports `hid-core`'s `Motion`: a relative axis moves the
  cursor, an absolute one maps its logical range onto the bounds. Each
  source's buttons are its own, and a button goes down when the first
  source presses it and up when the last releases it. A report changes only
  the buttons it carries, so a device whose buttons and wheel come in
  different reports keeps its buttons held across the wheel's. An absolute
  axis places the cursor only when its value changed, as an input core drops
  a repeated absolute value: a tablet repeats its position in every report,
  and would otherwise snap the cursor back from a mouse on each click.
- **HID protocols.** A boot keyboard is told to use the boot protocol, as
  HID 1.11 asks a host to tell rather than assume; one that refuses is read
  through its report descriptor if that names keys. Anything else is read in
  the report protocol through `hid-core`, a boot-subclass device told so,
  and a boot mouse whose descriptor cannot be read, or names no pointer,
  falls back to the boot protocol. Every interface whose protocol is chosen
  is sent `SET_IDLE(0)`, whose refusal is ignored. A report that rolls over changes nothing held, and a keyboard's
  report replaces only the keys of its own report ID. Keys are read only
  from Keyboard and Keypad application collections and motion only from
  Mouse and Pointer ones, so a game pad moves no cursor; an interface with
  neither, such as a media-key or vendor collection, is declined. A short
  report is zero-padded to the length posted and an empty one dropped, as
  Linux's HID core does.
  Input and output elements past `MAX_ELEMENTS`, which bounds what decoding
  costs where the ring is drained, are left out rather than refusing the
  descriptor; feature fields, decoded only at probe, count toward nothing,
  and a feature report past `MAX_REPORT_BITS` leaves its later fields out.
- **A USB keyboard is the physical console.** Someone typing on it is at
  the machine, as on Linux. Its reports feed the layer's one `SysrqFsm`
  wherever the event ring is drained, and a command key reaches the same
  `kconsole::request` the i8042 hook calls, and the repeats of a command key
  still held are eaten with it. Destructive commands still need the
  `kconsole=` mask.
- **The payload moves to a partition the kernel mounts.**
  - The module stays small, so Limine no longer loads gigabytes into RAM.
  - A single file in the ISO no longer carries the toolchain, so the ISO
    9660 limit of 4 GiB per file stops mattering.
  - The payload is found by the medium's GPT disk GUID, which the build
    fixes and the module records, not by a filesystem identity that a
    volume on an internal disk could also carry.
  - It is pinned, as `/media/install` is, so no process can substitute
    one.
  - It is the one exception to `root=initramfs` mounting no disk that a
    `mount=` did not name, and `AGENTS.md` says so when phase 5 lands.
- **The installer never offers its own medium.** The medium's GPT disk GUID
  identifies the stick on every boot path, UEFI, BIOS or optical, with no
  help from the loader. A write-protected disk is excluded as unwritable.
  On a medium that carries a payload, the payload's block read claim makes
  the installer's table re-read on that disk fail with `EBUSY` before
  anything is written, which backstops both.
- **USB NICs are class drivers.** `nic::publish` is the only way in, and
  `nic::retire` the only way out. An interface takes the lowest free
  `ethN` when it is published. USB publishes after PCI, so the built-in NIC
  keeps `eth0`, and a USB NIC that leaves gives its name back. Vendor modes
  are not planned; a class configuration reaches most adapters.
- **No USB API in userland.** A device shows up only as the disk, the
  interface or the input its class gives it. The kernel log and a kconsole
  command list the bus.
- **One shutdown hook per controller, no hook per device.** The filesystem
  flush runs first, with the interrupt path and the USB threads alive. The
  hook then works polled, with the USB threads stopped:
  1. it stops every storage engine on the controller;
  2. it drains the event ring until each engine is idle, within a bound as
     NVMe's drain is;
  3. it sends SYNCHRONIZE CACHE to each LUN that has a write cache and
     whose pipes step 2 left idle, as a polled Bulk-Only exchange below the
     engine, with a CSW deadline and no recovery. A LUN left mid-command or
     mid-recovery is skipped and logged. A stick written raw through
     `/dev/sdX` is thus flushed whenever it can be;
  4. it halts and resets the controller, and turns bus mastering off.

  Every controller is reset at poweroff and reboot, not only the PCH's.
  Linux resets `8086:51ed` because the next boot's firmware stalls for
  about 20 seconds on ports left in U3.
- **The panic path touches no USB.** Its crash record stays NVMe's and its
  Enter prompt stays the i8042's. `panic=reboot` is the answer on a machine
  with only a USB keyboard.
- **A device is untrusted input.**
  - Every descriptor, report, CSW, NTB and string is parsed by host-tested
    code that bounds its reads.
  - No length a device reports is trusted past the buffer posted for it.
  - Enumeration retries are bounded per port.
  - A device beyond a table's capacity is declined with a log line: slots,
    disks, NICs.
  - Mounting a stick takes `Mount`, as any mount does. The kernel itself
    mounts only what `root=` or a `mount=` names and, on a live system, the
    read-only payload partition of the disk that carries the medium's GPT
    disk GUID.
- **The TCSS controller stays in D0.** It is never suspended. If the
  firmware leaves it powered off, it stays unsupported, because powering it
  needs ACPI power resources the AML interpreter cannot run.
- **Facts come from the specifications.** Those are xHCI 1.2, USB 2.0 and
  3.2, HID 1.11 and its usage tables, Bulk-Only 1.0, CDC 1.2 with ECM and
  NCM, and, because T10's drafts are not public, Seagate's SCSI command
  reference. Behaviour of Linux and QEMU is taken only as fact. The
  permissive implementations, Redox's `xhcid`, `usbhidd` and `usbscsid`,
  Haiku, FreeBSD, SerenityOS and OpenBSD, may be named as influences. Code
  and prose come from none of them, as for the Realtek driver.

## Constraints

- Everything in `plans/self-hosting.md`'s Constraints applies.
- A primitive USB needs that the existing MMIO and DMA surfaces lack is a
  safe API in `slopos-ostd`, with its contract expressed in types rather
  than documented: the safe-contract baseline is zero.
- Descriptors, contexts and report buffers are never staged on the stack.
- The kernel is soft-float, so a TRB is written as integer stores, with
  fences ordering the cycle bit last.
- Nothing slow runs under a spinlock: no bring-up, port reset, enumeration
  or control transfer. `QueueOps` and `NetDevice::tx` run with interrupts
  off, so they never block, allocate or log. A hub's lock is never held
  while a child's is taken.
- The USB threads wait through `KernelIoToken::park_timeout`, never a bare
  sleep, so they honour stop and freeze.
- No code or prose comes from Linux (GPL-2.0-only) or QEMU (GPL-2.0 as a
  whole, its USB models under per-file licences). Spec prose stays out too:
  Intel's xHCI specification grants no licence, and USB-IF documents are
  licensed for internal use. Neither the PDFs nor long excerpts enter the
  tree; comments cite sections.
