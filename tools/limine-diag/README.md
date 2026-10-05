# Limine handoff diagnostics

A Limine 12.9.1 `BOOTX64.EFI` that shows on screen where it stops. Between its
framebuffer clear and the jump to the kernel, stock Limine draws nothing. A
fault there goes to an IDT nobody services and the loop never ends, or, once
the handoff GDT is loaded, the CPU triple-faults. On a machine with no serial
port you get a black screen. This build draws into framebuffer 0 instead:

- a trail with one line per step, the newest marked `>`;
- a red **EXCEPTION** box for any CPU exception (vectors 0-31);
- a purple **CORRUPT** box when a page that Limine handed out changed behind
  its back;
- a green box in the top-right corner when Limine executes `iretq` into the
  kernel.

The patches are `0001-slopos-handoff-diagnostics.patch` and the optional
`0002-slopos-autoboot-as-editor.patch`. Both are BSD-2-Clause, as Limine is.
The source release is pinned in `PIN`.

## Build

```sh
just limine-diag                        # builddir/limine-diag/BOOTX64.EFI + limine.elf
just limine-diag --autoboot-as-editor   # adds 0002, see below
```

You need `clang`, `ld.lld`, `llvm-objcopy`, `llvm-objdump`, `llvm-readelf`,
`nasm`, `make` and `patch`. The first build fetches the source tarball into
`third_party/`. To build offline, put the tarball there yourself or set
`LIMINE_SRC_URL`. The build pads the binary to the stock release's file and
image size, because firmware places a loader by those sizes and every
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
sudo cp "$ESP/EFI/SlopOS/BOOTX64.EFI" "$ESP/EFI/SlopOS/BOOTX64.EFI.stock"
sudo cp BOOTX64.EFI "$ESP/EFI/SlopOS/BOOTX64.EFI" # the file from builddir/limine-diag/
sync
```

Boot SlopOS the way that goes black: the 5 s timeout, Enter, or
`bootctl oneshot`. Wait about 10 s, until nothing changes, and photograph the
whole screen. Then hold the power button. For comparison, boot once more and
press `e` and then `F10`. The diagnostic binary still shows Limine's menu and
its CachyOS entry.

To restore the stock loader:

```sh
sudo cp "$ESP/EFI/SlopOS/BOOTX64.EFI.stock" "$ESP/EFI/SlopOS/BOOTX64.EFI" && sync
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
| `IMAGE … STOCK` | `SAME SIZE` means Limine's memory layout is stock's. |

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
7. `BUILD PAGEMAP TOP= PT=`
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
| `TO 32-BIT, PAGING OFF` is the last line and there is no green box | Stopped in `limine_spinup_32`. It runs with a limit-0 IDT and paging off, so a fault there triple-faults. | |
| Green box present, but the kernel never draws or answers | Limine handed off. The hang is in the kernel's `_start` or before `kernel_main_impl`. | |
| No diagnostic text at all | Stopped in or before `fb_init`, or framebuffer 0 is not 32 bpp. | |
| It boots fine | The diagnostics changed what fails: timing (drawing and checksums add up to about a second), or firmware-pool placement (the tables take 22 pages from the firmware at `boot()`). With `SAME SIZE`, Limine's own allocations are where stock puts them, so they are not what changed. | |

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
