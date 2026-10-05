# Limine handoff diagnostics

A `BOOTX64.EFI` of the Limine SlopOS ships (12.9.1 with `toolchain/limine`'s
patches) that shows on screen where it stops. Between its framebuffer clear
and the jump to the kernel, stock Limine draws nothing. A fault there goes to an
IDT nobody services and the loop never ends, or, once the handoff GDT is
loaded, the CPU triple-faults. On a machine with no serial port you get a
black screen. This build draws into framebuffer 0 instead:

- a trail with one line per step, the newest marked `>`;
- a red **EXCEPTION** box for any CPU exception (vectors 0-31);
- a purple **CORRUPT** box when a page that Limine handed out changed behind
  its back;
- green boxes along the top right, drawn by the 32-bit trampoline once paging
  is back on: from the left, on entering 64-bit mode, after its `lgdt`, after
  the jump to the higher half, and the last, in the corner, when Limine
  executes `iretq` into the kernel.

The patches are `0001-slopos-handoff-diagnostics.patch` and the optional
`0002-slopos-autoboot-as-editor.patch`. Both are BSD-2-Clause, as Limine is,
and apply on top of the shipped loader's patches; the source release is the
one `toolchain/limine/PIN` pins.

It found the bug `toolchain/limine/0001` fixes: on a laptop whose framebuffer
is the highest memory-map entry, step 10 showed the framebuffer in Limine's
map and absent from its page tables, and the first write through it faulted.

## Build

```sh
just limine-diag                        # builddir/limine-diag/BOOTX64.EFI + limine.elf
just limine-diag --autoboot-as-editor   # adds 0002, see below
```

You need what the shipped loader needs (`scripts/ensure_limine.sh`, which this
runs first) and `patch`. The first build fetches the source tarball into
`third_party/`. To build offline, put the tarball there yourself or set
`LIMINE_SRC_URL`. The build pads the binary to the shipped `BOOTX64.EFI`'s file
and image size, because firmware places a loader by those sizes and every
allocation Limine makes moves with it (under OVMF, one page of image moves
every allocation by one page). The header's last line confirms the match.

**What 0002 does:** pressing `e` and then `F10` without changing anything is
the only way the laptop boots. With 0002, Enter, the timeout and a one-shot
boot first do what that editor path does and they do not. They copy the entry
into the editor's 4 KiB buffer and boot from that copy, allocate and free the
editor's cell map, and call `mouse_flush()`. Under QEMU, every allocation then
has the same address and size as on a real `e` + `F10` boot.

## Install on the laptop (from CachyOS)

The ESP is shared with CachyOS, so replace only SlopOS's own loader.

```sh
ESP=$(findmnt -no TARGET -t vfat | head -n1)      # usually /boot/efi or /boot
ls "$ESP/EFI/SlopOS/"                             # BOOTX64.EFI  limine.conf
sudo cp "$ESP/EFI/SlopOS/BOOTX64.EFI" "$ESP/EFI/SlopOS/BOOTX64.EFI.shipped"
sudo cp BOOTX64.EFI "$ESP/EFI/SlopOS/BOOTX64.EFI" # the file from builddir/limine-diag/
sync
```

Boot SlopOS the way that goes black: the 5 s timeout, Enter, or
`bootctl oneshot`. Wait about 10 s, until nothing changes, and photograph the
whole screen. Then hold the power button. For comparison, boot once more and
press `e` and then `F10`. The diagnostic binary still shows Limine's menu and
its CachyOS entry.

To restore the shipped loader:

```sh
sudo cp "$ESP/EFI/SlopOS/BOOTX64.EFI.shipped" "$ESP/EFI/SlopOS/BOOTX64.EFI" && sync
```

## Reading the photo

Counts (`N=`, `CPUS=`, `PT=`, `INITS=`, step numbers) are decimal. Everything
else is hex.

| Header field | Meaning |
|---|---|
| `EDITOR=1` | The entry came through the editor (or through 0002, which also shows `MIMIC=1`). |
| `POINTER INITS/REL/ABS` | How many times `mouse_init` ran, and how many relative and absolute pointer protocols the last run found. |
| `GETSTATE`, `FLUSH` | Pointer `GetState` calls and `mouse_flush()` calls before the boot. An edited boot shows `FLUSH=1`. |
| `SLIDE`, `ABOVE4G` | Limine's load address, and whether it is above 4 GiB. |
| `KBASE`, `STACK` | The kernel's physical base and the top of the handoff stack. |
| `FW-CR3` | The firmware's page tables, which stay live until paging is turned off. |
| `IMAGE … STOCK` | `SAME SIZE` means Limine's memory layout is the shipped loader's. |

Trail lines, in order:

1. `FB`
2. `RNG SEED`, `POST-EBS TERMINAL`, `BLI ON BOOT`: each gets `OK` when its
   firmware call returns.
3. `GETMEMORYMAP #n … EXITBOOTSERVICES: <status>`: one line per attempt. A
   small yellow square means GetMemoryMap returned and ExitBootServices was
   called. Nothing is drawn between the two calls, so the map key stays valid.
4. `CHECK EBS`
5. `EFI MEMMAP REBUILT`
6. `MAP … TABLES`
7. `BUILD PAGEMAP SNAP= NOW= TOP= PT=`: the memory map's entry count before and
   after `build_pagemap` allocates its copy, each entry past `SNAP` as
   `TAIL <base>+<length> T<type>` (unpatched 12.9.1 left these out of the page
   tables), then `MAP-FB` for each framebuffer entry and `WALK FB:` /
   `WALK FB END:`, the page-table entries the framebuffer's first and last
   page resolve through, level by level.
8. `CHECK PAGEMAP`
9. `INIT SMP`, then one `START AP LAPIC ID n:` line per AP, ending `BOOTED`
   or `TIMED OUT`
10. `SMP DONE`
11. `CHECK SMP`
12. `MEMMAP RESPONSE`
13. `IOMMU`: one `VT-D <base> STS=<GSTS before>` line per unit. The line gains
    `TE:` / `IR:` / `QI:` before each wait and `OFF` after it.
14. `MASK PIC + IOAPIC`
15. `CONFIGURE BSP LAPIC`
16. `CHECK SPINUP`
17. `COMMON_SPINUP GDT+IDT OK, FLUSH IRQS:OK`
18. `TO 32-BIT, PAGING OFF`

A green `CHECK … OK n OBJ m PG` line means every page of n tracked objects
(m pages in all) still matches. The tracked objects are every allocation from
`boot()` on, named after the responses that point at them, plus the GDT copy,
the low spinup trampoline, the flush IDT, Limine's text and every page-table
page. The kernel image is also compared word for word with its ELF file.

**EXCEPTION** box: vector, error code, RIP and `LIMINE+offset`, CR2, RSP, CS,
RFLAGS, CR0, CR3, CR4, and the last step. `CS 28` means the fault came after
`common_spinup` loaded Limine's GDT. Decode the offset:

```sh
addr2line -f -i -e builddir/limine-diag/limine.elf 0x<offset>
```

Use the `limine.elf` from the same build. `OUTSIDE LIMINE` means the fault was
in firmware code.

**CORRUPT** box: the object, the check that found it, the page and its offset
in the object, the object's allocation site (`LIMINE+offset`, decode it the
same way), and the first changed word (old and new values). For a 64-byte
chunk, it shows the chunk's current contents. Up to six more changed objects
are listed.

## What the outcome means

| What the photo shows | Cause | Next |
|---|---|---|
| Last line `GETMEMORYMAP … EXITBOOTSERVICES:` with the yellow square and no status | Hang inside `ExitBootServices`: a firmware EBS handler. The pointer-protocol hypothesis predicts this. | Compare `POINTER`/`GETSTATE`/`FLUSH` with an `e` + `F10` boot. Try `--autoboot-as-editor`, then split its two halves (next table). |
| `RNG SEED`, `POST-EBS TERMINAL` or `BLI ON BOOT` without `OK` | Hang in that firmware call (RNG protocol or `SetVariable`). | |
| `CORRUPT <object>` | Something outside Limine wrote memory Limine owns: DMA after EBS, SMM, or a firmware handler. The check name says when: `EBS` during ExitBootServices, `SMP` during AP bring-up, `SPINUP` after it. | Boot twice. If the same physical page is hit, a fixed page is being clobbered, and the editor's one-page shift decides which object it hits. Check the edited boot: the same page may land in a harmless object. |
| `VT-D … TE:` (or `IR:`, `QI:`) with no `OFF` | `iommu_disable_all` polls that unit's GSTS bit forever. | `STS=` on that line is the GSTS before the write. |
| `START AP LAPIC ID n:` with no result | Hang while starting that AP. | |
| Red EXCEPTION box | Fault at the decoded RIP. | `addr2line` on `limine.elf`. |
| `TO 32-BIT, PAGING OFF` is the last line and there is no box top right | Stopped in `spinup_go32` or `limine_spinup_32` before 64-bit mode: paging off, EFER, CR4 and CR3, the TSS, then the PAT, CR3, PAE, EFER, paging on and the far return. A limit-0 IDT is loaded, so a fault there triple-faults, and the framebuffer above 4 GiB cannot be drawn into. | Bisect with the reset probes (below). |
| One to three boxes, none in the corner | Stopped after the box furthest right: one box, in or after the `lgdt`; two, in the jump to the higher half (the HHDM alias of the trampoline's code or stack); three, unmapping the lower half. | |
| All four boxes, but the kernel never draws or answers | Limine handed off. The hang is in the kernel's `_start` or before `kernel_main_impl`. | |
| No diagnostic text at all | Stopped in or before `fb_init`, or framebuffer 0 is not 32 bpp. | |
| It boots fine | The diagnostics changed what fails: timing (drawing and checksums add up to about a second), or firmware-pool placement (the tables take 22 pages from the firmware at `boot()`). With `SAME SIZE`, Limine's own allocations are where the shipped loader puts them, so they are not what changed. | |

If the `--autoboot-as-editor` build boots where the plain one hangs, add
one of these to find which half of the editor path it needs:

| Build | What Enter, the timeout and a one-shot boot do first | If it boots |
|---|---|---|
| `--autoboot-as-editor` | The editor's allocations and `mouse_flush()` | One of the two is enough. |
| `… --define SLOPOS_DIAG_MIMIC_NO_FLUSH` | The editor's allocations only (the header says `ALLOCATIONS ONLY`) | The memory layout decides it. |
| `… --define SLOPOS_DIAG_MIMIC_NO_ALLOC` | `mouse_flush()` only (the header says `MOUSE_FLUSH ONLY`) | The pointer-protocol state decides it. |

## Self-tests

These use `--define` (repeatable) with `just limine-diag`:

- `SLOPOS_DIAG_TEST_FAULT=<kind>` takes a #GP when that step starts. The
  kinds are 1 `FB`, 2 `RNG`, 3 `TERM`, 4 `BLI`, 5 the GetMemoryMap and
  ExitBootServices line, 6 `REBUILD`, 7 `TABLES`, 8 `PAGEMAP`, 9 `SMP`, 10 `AP`,
  11 `SMP DONE`, 12 `MEMMAP`, 13 `IOMMU`, 14 `PIC`, 15 `LAPIC`, 16 `CHECK`,
  17 `SPINUP` and 18 `GO32`. Use 4 to test the firmware IDT before EBS, 8 to
  test it after EBS, 17 to test Limine's IDT after the GDT switch (`CS 28`) and
  18 to test it after `flush_irqs`.
- `SLOPOS_DIAG_TEST_CORRUPT=<n>` flips one word. 1 hits the requests array at
  the EBS check. 2 hits the kernel image at the EBS check. 3 hits a page-table
  page at the SMP check. 4 hits the kernel image before anything is sealed,
  which only the comparison against the ELF file catches.
- `SLOPOS_DIAG_E9_TRACE` prints every allocation to QEMU's debugcon (port
  0xe9). That is how 0002 was compared with `e` + `F10`.
- `SLOPOS_DIAG_RESET_AT=<n>` resets the machine at probe n of the spinup:
  the reset control register's hard reset (0xcf9 ← 2, then 6), its full reset
  (14), the keyboard controller's reset line (0x64 ← 0xfe), then a triple
  fault, each after a pause. 1 is after the IRQ flush, which every boot that
  draws `TO 32-BIT` reaches, so it shows the reset works on that machine. In
  `spinup_go32`: 2 on entry, 3 after paging is off, 4 after EFER, 5 after CR4
  and CR3, 6 after `ltr`. In `limine_spinup_32`: 7 on entry, 8 after the PAT,
  9 after CR3 and PAE, 10 after EFER, 11 after paging is on; then, in 64-bit
  mode, 12 before the first box, 13 after it (the first write through the new
  HHDM), 14 after the `lgdt`, 15 after the jump to the higher half, 16 after
  unmapping the lower half. A machine that reboots on its own reached that
  probe; one that stops black did not. Under QEMU, every probe sends OVMF
  round its boot loop without ever reaching the kernel.
- `SLOPOS_DIAG_PROBE_STEP` does that bisection in one sitting: each boot takes
  the probe after the last one's, from `SLOPOS_DIAG_PROBE_FIRST` (default 1)
  to 16, counted in the NV UEFI variable `SlopDiagProbe` (vendor GUID
  `5c1f2a8e-3b7d-4e61-9a0c-510b05d1a607`), and the header shows it as
  `PROBE=n`. Left to boot the default entry by itself, the machine resets
  through probe after probe until it hangs at one it never reaches: it
  stopped between probe n-1 and n. Hanging at the first probe means the reset
  does not work there, and says nothing. One that keeps cycling stops after
  the last probe. Delete the variable from Linux afterwards: `sudo chattr -i`
  and then `sudo rm` on
  `/sys/firmware/efi/efivars/SlopDiagProbe-5c1f2a8e-3b7d-4e61-9a0c-510b05d1a607`.
