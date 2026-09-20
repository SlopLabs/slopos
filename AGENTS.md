# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
[`just`](https://github.com/casey/just) is the command runner; the `justfile` drives cargo + `rust-lld` via `scripts/`. Run `just --list` for all recipes. No git submodules — `scripts/ensure_limine.sh` fetches pinned Limine v12.3.1 into `third_party/limine` on first ISO build.

- `just setup` — install pinned nightly from `rust-toolchain.toml`; materialize the owned `slopos` sysroot (`scripts/make_slopos_sysroot.sh`, see below); verifies Go >= 1.22 on PATH (for `tools/run_tests/`)
- `just build` — emits `builddir/kernel-dev.elf`; `just iso` regenerates `builddir/slop.iso`
- `just boot` (interactive) / `just boot-fast` (skips roulette) / `just boot-log` (non-interactive, 15 s timeout)
- `just test` — the CI/agent entry point (see Testing Guidelines)

**The userland target builds on its own toolchain.** The kernel is built by
`cargo +<pinned nightly>` against `targets/x86_64-slos.json`; the userland
target `targets/x86_64-unknown-slopos.json` is built by `cargo +slopos`
against an *owned* sysroot at `third_party/rust-slopos`. That sysroot is a
hardlink clone of the pinned rustup toolchain whose `lib/rustlib/src` is a
real copy carrying two pinned forks — `rust-lang/rust`'s `library/` and
`rust-lang/libc` — applied from the patches under `toolchain/`, because
`-Zbuild-std` reads std from the invoking sysroot's source tree and nothing
in this workspace can reach it. `scripts/make_slopos_sysroot.sh` builds and
registers it (idempotent: a stamp over `toolchain/` makes a warm run ~20 ms),
`scripts/ensure_toolchain.sh` calls it, and `scripts/check_toolchain_pin.sh`
fails the build when the materialized sysroot has drifted from the pin.
Nothing on this path writes inside the rustup toolchain directory; the only
thing it adds to `$RUSTUP_HOME` is the `slopos` symlink that registration is.

The fork is cut against a *pristine* `rust-src`, and the retired std patcher
this replaces mutated that component in place, so a machine that ever ran it
needs a one-time reset. `make_slopos_sysroot.sh` refuses to run while the
residue is there and prints the fix; both halves are required, because
`rustup component remove` deletes only what its own manifest lists, so the
files the patcher *added* survive a reinstall (measured — a remove/add left
six of them behind):

```sh
ch="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml)"
rustup component remove rust-src --toolchain "$ch" && rustup component add rust-src --toolchain "$ch"
find "$(rustc +"$ch" --print sysroot)/lib/rustlib/src" -name '*slopos*' -delete
```

**The C++ runtime is cross-built and test-only.** `x86_64-unknown-slopos` has
a C++ standard library: LLVM's `libc++` and `libc++abi`, cross-built by the
host clang whose major `toolchain/cxx/PIN` names, linked whole-archived into a
single `third_party/slopos-cxx/lib/libc++.so`. `scripts/make_slopos_cxx.sh`
builds it (idempotent: a stamp over the pin, the build script and
`slibc/include` makes a warm run milliseconds), `scripts/check_cxx_pin.sh`
gates it, and `scripts/build_userland.sh --test` is the only caller — the
shipped appliance root runs no C++ program, so the runtime is on the tests
image only, beside `cxx_probe` and `libcxxtest.so`, whose `cxx_test` proves a
C++ exception crosses a `dlopen` boundary, and `cxx_static_probe`, which links
`libc++.a` and `libc.a` instead and so is the only thing that exercises either
archive or the frame finder's `AT_PHDR` road. One artifact rather than two because
libc++abi's caught-exception stack and the `type_info` a `catch` matches on are
process-wide: two instances of it in one process is a throw that cannot be
caught across the boundary between them.

That build is the one place SlopOS needs host tools beyond rust and QEMU:
`clang`, `clang++`, `ld.lld` and `llvm-ar` at the pinned major, plus `cmake`
and `ninja`. `CLANG`/`CLANGXX`/`LD_LLD`/`LLVM_AR` override the names, which is
how CI points them at a distribution's `-18` suffixes. The sources are the
pinned `llvm-project` release tarball, fetched into `third_party/` on the first
build; an offline checkout pre-populates that file or points `LLVM_URL` at a
local copy, exactly as `LIMINE_URL` works for Limine. `just distclean` removes
the extracted 91 MB source tree; nothing removes `third_party/slopos-cxx`,
because the build is minutes and its inputs are pinned.

**The unwinder is not test-only.** `libc.so` and `libc.a` supply the Level-1
Itanium unwinder (`vendor/unwinding`, seventeen `_Unwind_*` entry points) on
every image, behind slibc's `unwinder` cargo feature which only those two
wrapper crates enable, and both are built `-C force-unwind-tables` because the
first frame of every unwind is one of their own. The cost is that the shipped
`libc.so` carries the DWARF reader — 412 384 bytes against 241 376 before —
which `--gc-sections` cannot drop while startup registers the frame finder
unconditionally. The 60 static Rust binaries are unaffected: they take slibc as
an rlib with the feature off, and `userland/userland.ld` discards `.eh_frame`
outright.

Boot targets rebuild a secondary `builddir/slop-notests.iso` with `tests=off`; override via `BOOT_CMDLINE=... just boot`, add `VIDEO=1` for a graphical window.

**The disk is the root.** `root=auto` mounts a writable `disk0` at `/`, so what a boot writes there persists; the initramfs is the fallback for no disk and for a disk that mounted read-only (the shipped verified `ext2.img`, so `just boot` still runs `/sbin/init` from RAM with the attested disk at `/mnt`). `root=` also accepts `initramfs`, `virtio`, and a device name — `/dev/vda`, `/dev/vda1`, `vdb2` — where the partition comes from the GPT or MBR table on that device; a named device or partition that is absent degrades to the initramfs exactly as no disk does. `just boot-persist` is the developer's persistent machine: it boots `fs/assets/ext2-persist.img`, built `VERITY=rw` (a v2 trailer, so the image is writable *and* attested everywhere the guest has not written) and refreshed in place across builds (`PRESERVE_FS_IMAGE=1`, binaries only) so what the guest wrote survives. `VERITY=on` builds the shipped v1 trailer, which write-protects the device and is what `just boot`'s `verity=require` asserts; `VERITY=off` builds no trailer. The shipped and *tests* images are regenerated on every build on purpose — a persistent `/` would make every filesystem test a mutation of the image the next run boots from.

**The root is not the only filesystem.** `mount(2)` with `fstype=ext2` takes a
`source` naming a block device — `mount("/dev/vdb1", "/home", "ext2", …)` —
claims that device's exclusive writer (or mounts read-only with `MS_RDONLY`,
which needs no claim), and binds it to one of four pooled `Ext2Mount`
instances, each with **its own lock**. A path walk crossing a mount therefore
holds one mount's lock while taking the next one's, which is why the four
instances carry four separate `lock_class!` sites rather than one shared class;
slot 0's is still named `CACHED_EXT2`, so the class boot registers is the class
it always was. `umount` of the last mount of an instance flushes it, marks the
image clean, drops the device and returns the slot, so the write claim is
released and the same disk can be mounted again — a leaked claim answers
`AlreadyClaimed` forever, and that is the failure the remount test exists to
catch.

**A rude exit is survivable.** The flusher marks the image clean *on the
medium* whenever a pass leaves nothing dirty, nothing unbarriered, no
superblock drift and an empty log — the state ext4 reaches for `fsfreeze`,
here reached automatically at idle — and `Ext2Fs::transaction` re-stamps it
dirty before the next mutation reaches the device. Closing the QEMU window
therefore costs at most the last idle window's writes, instead of leaving an
image that mounts read-only forever after and that `root=auto` then demotes to
`/mnt` while booting the initramfs. The host half is the same promise:
`build_fs_image.sh` never deletes a `PRESERVE_FS_IMAGE=1` image. One that is
damaged, left dirty, or built under a different `VERITY` stops the build
naming the command that repairs it; `just boot-persist-reset` is the only
thing that discards one; a larger `PERSIST_IMAGE_SIZE` grows the image with
`resize2fs` rather than rebuilding it; and `gen_verity.py` AND-s the old
attested bitmap into the new one, so a block the guest rewrote stays
un-attested across rebuilds. The persist default is 512M; the ceiling is now
what the machine's RAM allows rather than what one allocation allows, because
the verity hash array is chunked (4 bytes per 4 KiB block, in 256 KiB pieces,
refused past a stated share of usable memory) instead of one contiguous `KVec`.

**A write is logged before it lands.** A writable ext2 image carries a metadata
redo log in a preallocated sealed file at `/.journal` (`FS_JOURNAL_SIZE`,
default 1/64 of the image floored at 4M and capped at 64M; `0` builds none),
located at mount by path lookup and used as a
physical redo log: an operation's metadata — and its data too, when the write
is small enough to be cheaper logged than barriered — goes into the log with a
CRC-covered commit record before any of it reaches a home location. That is
what makes an operation retractable (a rollback rewinds the log; nothing was
published) and a crash recoverable: a mount that finds `s_state` unclean and
replays a committed transaction comes up **read-write**, which is the one case
in which this kernel repairs an image instead of deferring to `e2fsck`. The log
is an *image* property, not a kernel one — an image without one gets the
previous undo-scoped behaviour and still refuses an unclean mount — and the
boot log says which of the two a mount got. `/.journal` is refused to readers
and protected from write, rename, unlink and truncate by `EXT2_IMMUTABLE_FL`:
its blocks hold copies of bitmaps, inode tables and directory blocks, so a
reader of it would see the metadata of every recently changed file. The default
image is 32M rather than 16M because the log takes 4M of it.

**A writeback pass is bounded.** `sync(2)` and the flusher drive
`Ext2Fs::sync_step` in chunks of `WRITEBACK_CHUNK` device writes, releasing the
mount lock between them, so a path walk or an `exec` queued behind a pass waits
for a chunk rather than for the whole pass. A pass fixes a *dirty epoch* and the
log's head and generation when it opens, which is what keeps the ordered phases
ordered across those gaps: an operation that runs in one is entirely outside the
pass. Mutations remain serialised per mount — the plan's per-inode locking is
deliberately not what landed, because the wait, not the lock count, is what G5
was about.

## Knowledge Index (AI)
`knowledge/` hosts a local semantic index for querying the codebase. Build once with `python3 -m venv knowledge/.venv && . knowledge/.venv/bin/activate && pip install -r knowledge/requirements.txt && python knowledge/index.py`, then query via `python knowledge/query.py "<question>"` for signatures, drivers, or file locations. Rebuild after large refactors or merges. Never commit the venv or embedding database artifacts.

## Coding Style & Naming Conventions
All kernel code is Rust `#![no_std]` on nightly with `#![forbid(unsafe_op_in_unsafe_fn)]`. Keep unsafe blocks tiny and well-documented; prefer `pub(crate)` helpers and prefix cross-module APIs with their subsystem (e.g., `mm::`, `sched::`). Match the existing four-space indentation and brace-on-same-line style. Assembly sources (when needed) are Intel syntax (`*.s`) and should document register contracts.

### Comments
Write code that does not need comments. Most comments are useless: they restate the code, drift out of date, and add noise.

- Default to **no comment**. Express intent through naming, types, and structure instead.
- Comment only when the code genuinely cannot carry the meaning: a hardware/spec quirk, a non-obvious ordering or locking requirement, a deliberate deviation, or a *why* that is invisible from the diff.
- When a comment is warranted, keep it **concise** — one or two lines. No restating the signature, no narrating control flow.
- Treat the urge to comment as a smell. Needing one usually signals a hack or unobvious behaviour; prefer fixing the code so the comment becomes unnecessary. If the hack must stay, say why it exists, not what it does.
- Exempt from the above: `# Safety` sections, `///` public API docs, and register-contract notes in assembly. These are contracts, not commentary.

### Unsafe-code surface
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (the framekernel **AD-1/AD-2** discipline: one trusted crate holds all `unsafe`, every other kernel crate forbids it; CI-enforced by `scripts/check_unsafe_outside_ostd.sh`). Every other crate the kernel binary links (`abi`, `acpi`, `boot`, `core`, `drivers`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `keymap-core`, `ktesting`, `mm`, `net`, `pidfd`, `ring`, `sched`, `service-core`, `signalfd`, `video`, `vt`) carries `#![forbid(unsafe_code)]`, and `check_unsafe_outside_ostd.sh` asserts that from the binary's own dependency closure, so a new crate is covered the moment it is linked. Userland-side crates (`userland/`, `slibc/`, `slop-protocol/`, `appkit/`, `slopos-rt/`, `windowing/`) are out of scope for this discipline.

`forbid` is necessary but not sufficient: rustc drops any `unsafe_code` diagnostic whose primary span satisfies `in_external_macro`, so a macro defined in another crate expands `unsafe` into a forbid crate silently, and the call site holds no keyword for a source scan to find. `scripts/check_unsafe_expansion.sh` is what closes that — see below.

One documented exempt site exists outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct.

Three C-ABI entry points keep `#[unsafe(no_mangle)]` in `boot/src/ffi_boundary.rs` — `kernel_main`, `common_exception_handler`, `isr_iret_frame_corrupt`. Their callers are assembly, so the symbols have to resolve at link time; routing them through a registered hook would leave an uninstalled window across early boot in which a fault triple-faults instead of panicking. `check_unsafe_expansion.sh` allowlists them by name.

These gates enforce the discipline. The source-scanning gates run via `just check-framekernel` and CI (and from a build when `KERNEL_BUILD_GATES=1`); the two ELF-inspecting gates (`check_stack_sizes.sh`, `check_kernel_softfloat.sh`) run on every kernel build, for every variant. A third class builds a probe and inspects what came out (`check_codegen_backend.sh`, `check_linker_script.sh`); those run from `just check-framekernel-gates` and CI only, never from a build, because the codegen one compiles `core` once per flag set. The source scans are kept off the default build path so the interactive boot loop stays fast.

Every gate carries a `--self-test`, run from `check-framekernel-gates` before the real scan. A check that has never been observed to reject has not been observed to work: the source scanners assert exact hit counts against planted violations *and* silence against the forms they deliberately accept, while the three ELF gates synthesise objects with `llc` (an oversized frame, a `movups`, an unblessed `link_section`) and assert the gate rejects them. A gate whose self-test fails is a build failure.

The build produces one ELF per variant — `builddir/kernel-dev.elf`, `kernel-release.elf`, `kernel-tests.elf` — because their codegen differs and a single shared path meant whichever build ran last silently answered for all three, to the gates, to gdb, and to the ISO builder. The two ELF gates take a required `--variant` and read their measured allowlist from `scripts/gates/{stack,vector}/<variant>.txt`. Those files also carry the input-sanity floors, so weakening a check is a diff on a tracked file rather than an edit to a script. Every allowlist entry must match something in the ELF being checked: an entry that matches nothing is a dead exemption and fails the gate, which is both the ratchet (a frame that shrinks hands its exemption back) and what makes a mis-stated `--variant` fail closed rather than pass on a list that happens to fit.

- **`scripts/check_unsafe_outside_ostd.sh`** — fails if any `.rs` file under a kernel crate (other than `slopos-ostd/`, `slopos-ostd-derive/`, or the exempt file above) contains an `unsafe` keyword that is not a comment, not the `#[unsafe(...)]` attribute form, and not `#[cfg(...)]`-gated. Mirrors `check_alloc_dep.sh`'s cfg-aware lookback. Also asserts that every crate in the kernel binary's dependency closure carries `#![forbid(unsafe_code)]` at its crate root, so a newly linked crate cannot start life without the lint.
- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency **and** fails if any kernel `.rs` file (other than `kernel/src/main.rs` and the `slopos-ostd/` tree) contains a bare `extern crate alloc;` / `use alloc::` / `use ::alloc::` statement (with `#[cfg(...)]`-aware lookback so cfg-gated usages that compile out of the kernel build are accepted).
- **`scripts/check_stack_sizes.sh`** — fails if any function in the kernel ELF has a stack frame larger than `STACK_SIZE_THRESHOLD` (default **2048 bytes / 2 KiB**, matching Linux mainline's `CONFIG_FRAME_WARN` default on x86_64/arm64 but stricter in enforcement — SlopOS fails the build, Linux merely warns). This is the load-bearing enforcement of SlopOS invariant **S-5** (bounded kernel stack use — named to avoid collision with the framekernel paper's own Inv. 5, *sensitive memory cannot be tampered with by user programs*). Driven by `-Zemit-stack-sizes`; inspects the final ELF's `.stack_sizes` section, so it catches NRVO failures, inlining, and trait-object dispatch that a source-level heuristic would miss. Above the threshold sits a second limit no allowlist can raise: the 4 KiB guard page. The target sets `"stack-probes": {"kind": "none"}`, so a frame larger than that steps clean over the guard in one instruction — a measured cap records how big a frame is, not whether that size is survivable. `min-records` is what stops a dropped `-Zemit-stack-sizes` from reading as a kernel with no large frames: `llvm-readobj` prints an empty `StackSizes [ ]` and exits **0** for an ELF carrying no section at all.
- **`scripts/check_kernel_softfloat.sh`** — fails if the kernel ELF touches XCR0-managed register state outside the sanctioned save/restore. The kernel must be built `+soft-float` so it never touches that register file: a syscall/exception entering from userland does **not** save the caller's FPU/vector state (only a full context switch does, via `xsave`/`xrstor`), so a single kernel instruction that disturbs it in a fault/IRQ path clobbers the interrupted user task's live registers. The scope is all four classes XCR0 enumerates, not just the vector one — x87 and MMX share one physical register file with XMM under XCR0 bit 0, and `xrstor64` overwrites the whole area at once — because XMM is only the instance rustc is likely to emit, and hand-written `asm!` is not subject to target features at all. The soft-float guarantee lives in `targets/x86_64-slos.json` (`features: …,-sse,…,+soft-float` + `rustc-abi: softfloat`) — **not** in `.cargo/config.toml`, because a `RUSTFLAGS` env var fully overrides `target.*.rustflags`. The `slopos-ostd` xsave-conformance helpers in the `kernel/tests` build are one allowlist entry with a measured instruction budget rather than a whole-binary exemption, so a vector instruction anywhere *else* in the tests kernel still fails.
- **`scripts/check_unsafe_expansion.sh`** — expands every kernel crate with `-Zunpretty=expanded`, over each crate's feature configurations, and holds the result to a constant rather than a recorded count: zero executable `unsafe`, `unsafe impl` only of an allowlisted trait, `#[unsafe(link_section)]` only of a `link.ld` section, `#[unsafe(no_mangle)]` only of an asm-called symbol. This is the only mechanism that sees macro-injected `unsafe`; `forbid` and the source scan are both blind to it. A golden fixture fails the gate if a toolchain bump moves the compiler's own emitted shapes. ~16 s warm.
- **`scripts/check_process_designator.sh`** — fails if a process-keyed table entry point (`mm/src/process_vm.rs`, `fs/src/fileio/`) takes a bare `u32` process id, or if a lock-free scan for a matching id grows back. Ids recycle, so a `u32` parameter is a confused-deputy surface: a stale one designates whichever process holds that number *now*, and the kernel services the call against a stranger's address space or open files. The replacements — `slopos_ostd::process::ProcessId` and `slopos_fs::fileio::FdTable` — carry a generation and can only be built from a live process, so a stale one fails the check instead of resolving. Scope is deliberately narrow: a `u32` pid is still correct at the ABI boundary (`getpid` returns one, the PCR carries one across a syscall); what must not happen is a *table lookup* keyed on one.
- **`scripts/check_registry_sections.sh`** — holds the kernel ELF to `link.ld`'s section set and each linker registry's span to a whole number of entries. Catches a *dependency's* `link_section`, which no first-party scan can see, and the wrong-entry-size case that would make `registry_slice`'s `offset_from` unsound.
- **`scripts/check_authority_reachability.sh`** — walks the linked ELF's call graph from every syscall handler to the terminal power primitives, and fails unless each handler that can reach one is either classified `Power` itself or carries a stated reason in `scripts/gates/authority/<variant>.txt`. The `rustc`-level classification gate in `core/src/syscall/handlers.rs` covers *the table*, not *reachability*: `roulette_result` was classified, the gate was green, and its loss arm called `kernel_reboot` two syscalls from an unprivileged caller. The ELF is the input rather than the source because inlining, generic instantiation and trait-object dispatch all change who really calls whom. Indirect calls (`call *%rax`) are the seam it cannot see, which is why the kernel-initiated `PowerOps` callers are a tracked list rather than something it discovers. Runs against the dev kernel from `check-framekernel-gates`, and separately in CI against the **tests** and **release** ELFs — the tests kernel is the only variant whose allowlist carries `run_userland_tests`, which powers the machine off to end the run. Gated in CI rather than on the build path because the walk disassembles the whole ELF (~8 s).
- **`scripts/check_safe_contract_surface.sh`** — ratchet on safe `pub fn`s in `slopos-ostd/` that carry a `# Safety` section. Those are self-declared caller obligations the compiler does not check, so a fault lands in the trusted core while the cause is an ordinary safe call in a service crate. The baseline is **0**: every such contract is currently expressed instead, as a capability witness (`&IrqDisabled`, `&BspToken`, `Osxsave`), a validated newtype (`Xcr0Mask`), a linear handle (`ptr_buf::OneShotBuf`), an owning reference (`KArc`), a sealed trait (`ApTrampolineAbi`), a runtime-checked borrow (`sync::PerCpuSlot`), or a slice in place of a pointer and a length. Reach for those before raising the baseline. Not a count of safe fns containing `unsafe` — that is the design working, not a defect.
- **`scripts/check_toolchain_pin.sh`** — holds the userland target's standard library to the fork it is pinned to. `x86_64-unknown-slopos` builds on an owned sysroot (`third_party/rust-slopos`, materialised by `scripts/make_slopos_sysroot.sh` out of the patches under `toolchain/`), and every way that can drift is silent: a fork cut against a different `rust-toolchain.toml` channel, a patch edited without its `toolchain/PIN` checksum, a sysroot left over from a previous overlay, or a `slopos` toolchain registered against some other directory. A stale sysroot compiles — it just compiles the previous std. It is the replacement for the retired std-patching script's `cfg_select!` arm-order check, whose failure mode (an arm placed after the `_` wildcard, dead code that still compiles — it shipped once as a `ud2` in `std::process::exit`) cannot occur now that the target is unix-family and rides std's own `sys/pal/unix`. It also asserts that every file the patches *create* is present in a materialized sysroot: the stamp describes the overlay, not the result, so a tree a broken run left half-patched — patched std, unpatched libc — otherwise carries a correct stamp and passes (observed). The sysroot and link checks are conditional on those existing, so CI that never materialises a sysroot still passes on the pin alone.
- **`scripts/check_cxx_pin.sh`** — holds the cross-built C++ runtime to `toolchain/cxx/PIN` and to what `libc.so` exports. Two silent failures: a `third_party/slopos-cxx` built from a different llvm-project or a different clang still links, it just links a different C++ ABI; and `libc++.so` is linked without `-z defs`, because an undefined symbol in a shared object is legal and the loader resolves it at load time — so a libc gap that would have been a link error is instead a `dlopen` that fails on a machine, at the point the runtime is first needed. The gate holds every symbol `libc++.so` leaves undefined (68 today) to being one `libc.so` defines, and holds the built tree's stamp to what `make_slopos_cxx.sh --print-stamp` says it should be — asked of the build script rather than recomputed, so there is no second copy of that digest to drift. All three are conditional on the runtime having been built, so a checkout that never cross-built it still passes on the pin's own consistency.
- **`scripts/check_codegen_backend.sh`** — holds a rustc codegen backend to seven of the capabilities `targets/x86_64-slos.json` depends on: an ELF object format, soft-float, `.stack_sizes`, safestack instrumentation through `__safestack_pointer_address`, `#[unsafe(link_section)]`, `#[unsafe(naked)]`, and `sym` operands in `asm!`. Two of those are flags a backend can *accept and ignore* — `-Zemit-stack-sizes` and `-Zsanitizer=safestack` — so a backend swap can leave the build green with S-5 and the dual-stack split enforced by nothing. Tracked verdicts live in `scripts/gates/codegen/<backend>.txt` and a mismatch fails **in either direction**: a `lacks` the probe finds present is the signal that the self-hosting question in `plans/self-hosting.md` needs re-deciding. `disable-redzone` and the `unwind` panic strategy are stated as residual rather than probed — the gate's header says why. Cold it costs ~60 s and ~460 MB for `llvm` / ~310 MB for `cranelift` under `builddir/gates/codegen-probe/` (which `just clean` removes); warm it is ~1 s. `llvm` is graded on every `just check-framekernel-gates`; `cranelift` reports `skipped` when the rustup component is absent, and CI installs it in a job of its own so the answer is re-taken rather than assumed.
- **`scripts/check_linker_script.sh`** — holds a linker to the eighteen linker-script constructs `link.ld` uses, from `. = KERNEL_VIRT_BASE` through `PHDRS`, `(NOLOAD)`, all three spellings of `ALIGN`, `KEEP` under `--gc-sections` and the four page-table reservations past `_bss_end`. Each probe's script carries the construct under test and nothing else a probe grades — a script that scaffolds itself with an `ALIGN` reports the linker's `ALIGN` support under whatever name that probe carries — with one deliberate exception, `composed-layout`, which links a `link.ld`-shaped script because a linker can take every construct alone and compose them differently. That exception is what the gate is built around: wild 0.10.0 refuses `link.ld` on its location-counter assignment, and given the shape it does accept it keeps the script's section order and still starts the image 0x13e8 past the base it was given. A second, self-maintaining half compares the constructs probed against the keywords `link.ld` actually uses, so a construct added to the script with no probe fails the gate and a probe whose construct left the script fails as a dead entry. `scripts/gates/linker/<linker>.txt`; `lld` is graded on every `just check-framekernel-gates`, `wild` reports `skipped` when it is not installed and is pinned in the CI job that installs it.
- **`scripts/tcb_ratio.sh`** (via `just tcb-ratio`) — a hard gate at `--max 1.0` from both `just check-framekernel-gates` and `KERNEL_BUILD_GATES=1` builds. Prints lines of `unsafe` in `slopos-ostd/` divided by total kernel Rust LoC. Read it as a trend, not as a TCB fraction comparable to other projects': the denominator is raw LoC including the 41 kLoC vendored DWARF reader, and published comparators measure post-LTO linked code size.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

### Allocation discipline
**`slopos_ostd::mm::heap` is the only kernel allocation surface.** Every kernel crate routes heap allocation through `slopos_ostd`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The `kernel/src/main.rs` global-allocator carve-out above is the lone exception.

The in-place-init primitive (`slopos_ostd::Init<T, E>`, `Zeroable`, `init_from_closure`, `init_zeroed`, `Field<T, U, OFF>` + `#[derive(SlotFields)]`) is **in-house** — defined in `slopos-ostd/src/mm/init.rs` with no external dependency on `pinned-init` or Rust-for-Linux's `pin-init`. Large structs must be constructed via `KBox::try_init(T::init_…())` / `PinBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack. `check_stack_sizes.sh` enforces the upper bound from the other direction. `init_struct_with`'s closure must return `Initialised<T>`, which only `SlotPtr::finish` mints, so a caller cannot claim success without going through the slot; `finish` additionally checks field coverage under `debug_assertions`.

### Licensing discipline

SlopOS is `GPL-3.0-or-later`. Two rules keep it that way.

**No verbatim code from a GPL-2.0-only source, ever.** GPL-2.0-only (Linux, the
seL4 kernel, `rust/kernel/**`) and CDDL (illumos) are incompatible with the GPL
version SlopOS ships under, and CDDL is incompatible with every GPL version.
Concepts, algorithms and interface facts are free to take — ABI numbers, `errno`
values, ioctl codes, struct layouts and hardware register offsets carry no
copyright, which is why the ABI-compatibility work is sound. Upstream *prose* is
not: never paste an upstream comment block, design essay or documentation
paragraph. When citing an influence in a comment, name the **specification or
the documented behaviour**, not the implementation file — "values follow the
Linux x86-64 ABI" is an interoperability statement, "derived from
`kernel/sched/fair.c`" is not. Keep the existing influence comments; they are
contemporaneous evidence that what was borrowed was the concept.

**Fonts load at runtime; never `include_bytes!` one into a shipped binary.**
`assets/fonts/*.ttf` are SIL OFL 1.1 and ship as separate files in
`/usr/share/fonts/`, which is aggregation and imposes nothing on the kernel.
Baking a font into `kernel.elf` or a userland binary would put OFL §5 ("must be
distributed entirely under this license") in direct conflict with GPLv3 §5(c)
("license the entire work, as a whole, under this License"). The
`include_bytes!` sites in `font/src/` are `#[cfg(test)]`-gated and must stay
that way. Each font's license text ships beside it, in `assets/fonts/` and on
the installed images.

New third-party code linked into a shipped binary needs an entry in
`NOTICE.md`; `MIT OR Apache-2.0` crates elect MIT there.

### Task-ownership discipline

**`KArc<Task>` is the only owning handle for a task, and `TaskRef` is the only
way to hold one outside `slopos-ostd`.** A raw task pointer says nothing about
whether the task is still alive, whether anyone else is mutating it, or who is
responsible for tearing it down; the owning handle says all three.
CI-enforced by `scripts/check_task_ownership.sh` (run from
`just check-framekernel`, hard-failing), whose header documents each check.

The invariants the gate protects:

- **I1** Raw task pointers exist only inside the ostd placement/link
  primitives, the PCR slots, and the pre-heap `.bss` stubs — the surfaces the
  gate lists as sanctioned. Everything else binds `&Task`, a guard, or a
  `TaskRef`.
- **I2** Linked implies owned: a task on any queue, inbox or wait map has its
  owning reference held *by that container*, moved in and out only through
  `slopos_ostd::task::placement`.
- **I3** The final drop never runs on the dying task's own stack, never with
  IRQs disabled, and never under a lock. `task_put` is the sole release; its
  destructor frees to the buddy allocator, whose reuse path performs
  synchronous cross-CPU TLB drains.
- **I4** Wake and enqueue allocate nothing; a `KArc` clone is one atomic.
- **I5** `current` is a borrow (`CurrentTask`), never an owned handle. PCR
  offset 40 stays raw and ABI-frozen — `__safestack_pointer_address` reads it
  from asm on every instrumented prologue. `IdleTask` is the same shape for the
  idle slot. The CPU separately holds *one* owning reference to the running
  task in `PCR.current_task_ref`, which is what lets a task be dispatched
  directly from its predecessor: a reference living in the dispatching frame
  would be owned by a stack the successor outlives. Idle owns none, being
  pinned by `task_is_dispatch_pinned`'s idle disjunct.
- **I6** Lookup is weak-upgrade only. Fabricating a strong reference from a
  raw pointer is not a thing that can be written.
- **I7** `KArc` is fallible everywhere and saturates on refcount overflow.
- **I8** **A task only ever exits from its own context.** Kill is a flag:
  `task_kill_and_wake` marks the target and wakes it, every blocking primitive
  returns `Err(WaitAbort::Killed)`, and the task unwinds by *returning*, so
  destructors run on its own stack at a point it chose. An owning task handle
  may therefore live in a stack frame that blocks. The residual is a kernel
  loop that reaches no blocking primitive at all: nothing can stop one, and
  `task_terminate`'s remote branch survives only as the bounded shutdown
  fallback and the IRQ-exit self-kill.

I1–I7 above are the tree's naming for this discipline. The proof in
`verification/proofs/task_ownership.rs` checks a model of it under the names
T1–T7, and `verification/STATUS.md` records which parts of the tree that model
does *not* reach — weak-memory ordering, the intrusive links and the raw-pointer
provenance are audited, not proved. (I1–I4 elsewhere in `STATUS.md` are
`mm::frame`'s refcount invariants, a different set.) Read that
file's header before changing `task_is_dispatch_pinned`, because the proof
keeps verifying whether or not the model still describes the tree.

## Testing Guidelines
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over serial in KTAP grammar. The Go host wrapper (`tools/run_tests/` → `builddir/run_tests`) parses that stream into a live progress bar + per-failure detail. `just test` builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and exits 0 green / 1 on any failure.

**Run `just test` before sending changes.** A green `just test` is necessary but **not** sufficient — it runs neither the framekernel gates nor the three boot-log ratchets, all of which CI runs and any of which can fail on a commit whose tests pass. See **Pre-commit (MANDATORY)** below for the full sequence. For manual inspection use `just boot` or `just boot-log` (serial transcript in `test_output.log`; `VIDEO=1` for a framebuffer). Note regressions or warnings in your PR description.

### `just test` recipes
- `just test` — full run; dotted progress, per-failure blocks, summary line.
- `just test 'slopos_mm::*'` — run only tests whose `<module>::<name>` matches the glob (positional — a `FILTER=` prefix is passed through literally and breaks the first glob). Comma-separated globs supported (`'mm::*,core::*'`). Filtered runs that create files must include `'*ext2_aaa*'` so the lex-first ext2 root mount runs.
- `just test-rerun-failed` — re-run only the names in `builddir/last-fail.list` (written automatically after every non-aborted run).
- `just test-verbose ['glob']` — also dump captured klog of every passing test.
- `just test-quiet ['glob']` — render only failures + summary; suppresses pass lines on the wire too.
- `just test-raw` — passthrough QEMU stdout verbatim (KTAP + klog interleaved). Last-resort debugging.
- `just test-json builddir/events.jsonl` — also write one JSON event per line to PATH (machine-consumable).
- `just test-userland-only` — skip the kernel phase; run only the userland (`utest!`) phase.
- `just check-tests-host` — run the Go wrapper's own unit tests via `go test ./tools/run_tests/...` (host-side, no QEMU).
- `just check-test-count` — count-regression CI guard; fails if total planned tests across phases drops below `TEST_COUNT_BASELINE`. The default lives in `scripts/check_test_count.sh` and is written down only there — read it from the script rather than restating it here, and bump it there when the suite grows. Measure the new value with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`; never guess it.
- `just check-fs-image` — hold the image the suite just wrote to `e2fsck -fn` and a clean superblock. Runs in CI after the test capture; an image SlopOS wrote that e2fsck rejects is a bug in SlopOS.
- `just test-persist` — two boots of one image with no rebuild between: write + `fsync` under `/var` on the disk root, power off, read back. In CI after `check-fs-image`. Needs its own boots and cannot reuse the shared capture.
- `just test-capacity` — the capacity check: build (once, then preserve) a 16 GiB ext2 volume, attach it as `virtio-disk3`, and let the suite mount it, walk it, write to it and report. Separate from `just test` because the image takes minutes to build and ~70M of host disk once populated; what CI grades per run is the cheaper `check-fs-throughput` ratchet below. `CAPACITY_IMAGE_SIZE` overrides the size; the guest measures a *mount* in device reads rather than in seconds, because reads are deterministic and wall time is not.
- `just check-fs-throughput` — filesystem cost ratchet over the `FSPERF[…]` / `FSCAP[…]` report lines, with gate data in `scripts/gates/fsperf/<variant>.txt`. Counts per MiB — transactions, journal commits, device write requests, barriers — are deterministic for one ISO and carry caps; a write rate is not, so the only rate graded is the quotient of the filesystem's write rate and the **same run's** raw block-device write rate, which is invariant under a change of accelerator (the gate's `--self-test` asserts exactly that: a uniformly three-times-slower machine must still pass). Floors (`min-bytes`, `min-volume-gib`, `min-dirents`) exist because a measurement that stopped happening looks exactly like one that got free. `--log` / `--emit-allowlist` / `--self-test` as in the other ratchets.
- `just check-quota-headroom` — resource-quota ratchet; asserts every account's peak stays under its measured cap in `scripts/gates/quota/<variant>.txt`, that nothing was denied, and that the charge path has not got slower. What the `used`/`peak` packing buys is that a *reported* peak is a value that was genuinely held — the caps themselves are measured maxima carrying the observed spread as margin, exact only on the rows the gate file records as deterministic (`process`, and the fd/object rows). The **cost** check is one cap and two floors, never a cycle count: a cycle count on that path measures the accelerator, not the kernel, and the absolute caps this gate used to carry failed on the *unmodified* tree on any machine without `/dev/kvm`. The cap is `max-depth-cost-ratio` — depth 7 against depth 1, the only quantity here invariant under a change of accelerator. The floors are `min-charge-over-reference` (one charge+refund round trip against a same-run bare CAS, a floor and not a ceiling because that ratio *does* move with the accelerator) and `min-reference-cycles` (an absolute physical bound on the reference itself, since the first floor is a ratio over it). Stated plainly: a slowdown that scales the whole charge path uniformly passes every one of them, and catching it would need the absolute ceiling that failed without KVM. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate, with one difference: this gate's `--log` is a single run, so its file records spreads in prose rather than merging several logs mechanically. `--emit-allowlist` emits a depth cap a quarter above the observation, and its own output is round-tripped through the check path by the self-test — the property that makes "re-measure with `--emit-allowlist`" a remedy that actually works.
- `just check-lockdep-headroom` — lock-order ratchet; boots the test ISO and fails unless every phase the kernel reports (`boot`, `post-kernel-tests`, `post-userland-tests`) says `ACTIVE`, reports no violation, and stays inside the gate file's `max-fill-pct`. Gate data lives in `scripts/gates/lockdep/<variant>.txt` in the same measured-and-tracked style as the stack/vector gates, and an entry matching nothing fails as a dead entry. The three pools are not graded alike. Class counts are deterministic — a class registers on the first acquire of a declaration site, and three runs of one pinned ISO measured boot at 71 every time — so they carry **exact caps**. Boot's edge and chain counts carry caps rather than bands for the same reason, though the recorded values still hold the old convention's slack until they are re-measured onto the observed 43/110. The two test phases' edge and chain counts measure which orderings a run *happened to observe* and move between runs of identical code, so they carry **bands** (`band <phase> <pool> <lo> <hi>`) instead: leaving one prints `DRIFT` on stderr and the run still passes. Be clear about what that gives up — a banded pool has no upper failure of its own, so growth up to `max-fill-pct` (~3.5x observed) reaches you only as that DRIFT line; an *inverted* order is caught by the cycle detector and still fails. `min-classes` / `min-edges` / `min-chains` are the floors that stop a validator which quietly stopped recording from reading as maximally healthy. `--emit-allowlist` writes a fresh baseline (and accepts several `--log`s to merge), a single `--log FILE` parses a capture instead of booting, and `--self-test` (run from `check-framekernel-gates`) drives its crafted logs through the parser — proving both that the gate rejects and that it stays silent on the forms it deliberately accepts.

- `just check-sched-spread` — SMP placement-eligibility gate. Reads the `SCHEDCPU[<phase>]:` lines and holds the run to one structural invariant: **a CPU that is online is eligible for task placement.** Not a measured ceiling — the only tracked number is `eligible-lag`, how many online CPUs may legitimately not be eligible yet, which is `1` at `boot` and at `post-kernel-tests` (the BSP executes boot steps on the bootstrap stub and cannot dispatch until `enter_scheduler(0)` runs at the end of boot init) and `0` afterwards. A lag above that is a bug, not a number to raise. It exists because the failure is invisible: every placement helper filters candidates through `is_schedulable_cpu`, so a CPU whose runqueue is online but not *enabled* disappears from every fork, exec and wakeup while still dispatching whatever work stealing hands it — the machine boots, the suite passes, and one core does all the work. It shipped that way: `boot_step_scheduler_init` ran in the `services` phase and `init_all_percpu_schedulers` reset every runqueue it found, including the three APs that had already entered their scheduler loops during `drivers`. `allow-ineligible` records *which* CPUs that lag may name, because a count alone accepts an AP dropping out as readily as the BSP. `require-dispatch` is the companion check, so a CPU that is eligible but never actually dispatches also fails — and it is a flag, never a volume: how *many* switches a CPU makes is a property of the machine, so any floor above zero fails on a runner that is merely faster or slower. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate.

All four boot-based ratchets (`check_test_count.sh`, `check_lockdep_headroom.sh`, `check_quota_headroom.sh`, `check_sched_spread.sh`) accept `--log`, and CI feeds them one `builddir/run_tests --raw --no-color` capture rather than booting QEMU once per question. Do the same locally — see the pre-commit sequence below — rather than paying four boots.

`check_sched_spread.sh` is the one gate in this list that is not a ratchet: its number describes the boot sequence, not a resource high-water mark, so a failure means a CPU stopped participating in scheduling and the fix is upstream of the gate.

A ratchet failure is a **measurement to re-take, not a number to raise**. Bump a cap only with a fresh `--emit-allowlist` in the same commit, and say in the commit message which lock, test or account added the delta. Never edit a gate file by hand to make a run pass.

### Cmdline knobs
The kernel parses these from the Limine cmdline (threaded through `scripts/build_iso.sh`'s third positional arg, controlled by the `test_cmdline` justfile constant or the `TEST_CMDLINE=…` env override). For manual `just boot-log` invocations, set `BOOT_CMDLINE='tests=on tests.run=mm::*'` to run a subset.

| Key | Values | Effect |
|---|---|---|
| `tests` | `on` / `off` | master enable |
| `tests.shutdown` | `on` / `off` | write to `isa-debug-exit` after the run |
| `tests.verbosity` | `quiet` / `summary` / `verbose` | per-test emit policy |
| `tests.warn_ms` | integer | mark slower tests as `OVER_TIME` |
| `tests.run` | comma-separated globs | only run matching tests |
| `tests.skip` | comma-separated globs | skip matching tests |
| `root` | `auto` / `initramfs` / `virtio` / `/dev/vdX[N]` / `vdX[N]` | which filesystem `/` is. `auto` prefers a writable `disk0` and falls back to the initramfs; a device name selects a probe-order device and, with a number, a GPT/MBR partition of it; an absent device or partition degrades to the initramfs with a klog line |
| `lockdep` | `off` / `warn` / `panic` | lock-order validator policy; default `panic` |
| `verity` | `require` | an attached disk must mount with a verity trailer or the `fs init` boot step fails; no disk at all still passes. `just boot` sets it — the shipped image is verified, so an unverified interactive boot is a broken artifact |
| `sched.ap_pause_ms` | integer | wall-clock budget for the AP pause; `0` disables the deadline and falls back to the iteration bound. Default measured — see `AP_PAUSE_BUDGET_NS_DEFAULT` |
| `kconsole` | `off` / `on` / `<hex mask>` | diagnostic-console permission mask; default `on` (informational only) |
| `kconsole.serial` | `on` / `off` | serial BREAK trigger; default `on` |
| `kconsole.arm_ms` | integer | how long the keyboard chord stays armed; default 3000 |
| `kconsole.max_lines` | integer | per-command line budget; default 512 |
| `kconsole.probe_ms` | integer | per-CPU answer budget for the all-CPU probe; default 250 |

`lockdep=warn` reports each distinct finding once (deduped per class pair) and
keeps booting, so one boot enumerates every ordering finding in the tree instead
of stopping at the first. `lockdep=off` keeps the held-lock stack — the poison
walk, the TLB ack-wait diagnostic and the watchdog all read it — but runs no
ordering checks, which is how the validator's own per-acquire cost is measured
without a separate build.

### Diagnostic console

`slopos_ostd::kconsole` is the kernel's magic-key facility: a key pressed on the
**physical console** makes the kernel describe itself. Press SysRq
(Alt+PrintScreen) to arm and one command key to run, or send a serial BREAK and
then the command key. Press the trigger then `h` for the list.

Commands live in the `.kconsole_registry` linker registry, so the crate that
owns a subsystem's data owns the command that prints it — `kcommand!` in `mm/`,
`sched/`, `core/`, `boot/`. OSTD defines the registry and never names an entry;
registration must happen in a crate only the kernel links, because OSTD is
linked into userland binaries too and their linker script brackets no kernel
section.

Three properties are load-bearing:

- **Only the physical console triggers it.** The keyboard hook sits in the IRQ
  handler ahead of layout resolution and consumes its keys, so they reach
  neither the TTY nor the focused GUI application; the serial trigger is a BREAK
  condition, which no byte pattern can forge. There is no call edge from any
  userland write path to `kconsole::request`, and that is the point — the
  facility this replaced was reachable by any process holding a PTY master.
- **One execution tier.** Every command runs at the bottom-half point with
  interrupts and preemption enabled. No command runs in NMI context and none may
  assume it can: a *returning* NMI handler must be fault-free, and the
  frame-pointer walk a backtrace needs is only fault-*recoverable*. The all-CPU
  probe therefore asks each CPU to describe itself from its own NMI handler
  rather than walking a peer.
- **Triggers only queue.** `request` is one `fetch_or` and one `gs`-relative
  byte store — what `bh::raise` permits from a hard IRQ and from under a
  cli-spinlock. The pending set is global rather than per-CPU because
  `bh::raise` marks only the calling CPU, and every CPU's timer tick pokes its
  own bottom half while anything is queued.

`KCMD_DESTRUCTIVE` commands are registered but refused unless the mask names
their bit, which the default does not: boot `kconsole=0x3` to enable them.

### Output format and JSONL events
The public KTAP docs describe the wire grammar and the JSONL event schema. The wire format is stable; the JSONL schema is a strict superset suitable for downstream JUnit XML conversion or test-history regression detection.

## Commit & Pull Request Guidelines
Subjects are `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`), ≤72 chars. Add a body for rationale, boot implications, or follow-ups. For PRs include: motivation, testing artifacts (command + result), issue references, and serial excerpts or screenshots when boot flow or visible output changes. Flag breaking changes and downstream-script coordination.

### Commit messages (MANDATORY)
**Always use the `caveman-commit` skill to write the commit message. Never hand-write one.**

- Load it with `/skill:caveman-commit`, or read `~/.agents/skills/caveman-commit/SKILL.md` directly if skill commands are unavailable.
- The skill only *generates* the message; staging and running `git commit` remain your job.
- This applies to every commit, including one-line and trivial ones.

### Branching (MANDATORY)
Commit directly to `develop` (the working branch). Do **not** create a new branch unless explicitly asked to — this overrides any default "branch before committing on the default branch" behavior. Likewise, do not open PRs or push to a remote unless explicitly asked.

### Pre-commit (MANDATORY)
Before every `git commit`, **always** run `cargo fmt --all` and stage any reformatted files. If formatting fails, fix the issue before committing. Never commit unformatted Rust code.

**`just test` alone is not the bar.** CI runs gates that `just test` does not, and a commit that only satisfies fmt + tests routinely fails on them — most often the lockdep ratchet. Reproduce the whole `ci` job locally:

```sh
cargo fmt --all                       # then stage the reformatted files
just fmt                              # CI: Check formatting
just test-host                        # CI: Host-side unit tests
just build                            # CI: Build kernel
just check-framekernel-gates          # CI: Framekernel gates (self-tests + vendor/toolchain pins + all source/ELF scans)

# CI: Run tests — one raw capture, which the ratchets then parse.
just _build-run-tests
set -o pipefail
builddir/run_tests --raw --no-color 2>&1 | tee builddir/ci-test.log

# The userland exists now, so the half of the C++ gate that needs a `libc.so`
# to compare against runs. It is skipped in the gates step above.
scripts/check_cxx_pin.sh

scripts/check_authority_reachability.sh --variant tests builddir/kernel-tests.elf
scripts/check_test_count.sh        --log builddir/ci-test.log
scripts/check_lockdep_headroom.sh  --log builddir/ci-test.log
scripts/check_quota_headroom.sh    --log builddir/ci-test.log   # not yet a CI step; run it anyway
scripts/check_sched_spread.sh      --log builddir/ci-test.log
scripts/check_fs_throughput.sh     --log builddir/ci-test.log   # not yet a CI step; run it anyway
```

The capture is reused deliberately: booting QEMU once per ratchet is a boot per
question, and a second boot could disagree with the one that was graded.

A changed lock order, a new lock, a new test, a new quota account, or a write
path that issues more device requests per MiB will move a ratchet. That is the
gate working — re-measure with the gate's `--emit-allowlist` and explain the
delta in the commit message. Never hand-edit `scripts/gates/**` to silence a
run.

Two CI jobs are *not* in the sequence above because they are slow and run as separate jobs: `just check-miri` (KernMiri) and `just verify` (Verus). Run them when touching `slopos-ostd/` or `verification/`; `just check-framekernel` is the recipe that runs the gates plus both.

Commit order: `cargo fmt --all` → the sequence above → stage → `caveman-commit` for the message → `git commit`.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads the pinned Limine binary release into `third_party/limine`; offline environments should pre-populate that directory (it only needs `limine-bios.sys` + `BOOTX64.EFI`) or set `LIMINE_URL`/`LIMINE_VERSION` to avoid network stalls. Rust crates are auto-discovered via the workspace, so most build changes belong in `justfile`, `scripts/`, `Cargo.toml`, and `targets/*.json`; ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered. VirtIO devices require MSI-X (preferred) or MSI as a minimum — legacy polling has been removed; probe panics if neither interrupt mechanism is available.**

`scripts/make_slopos_sysroot.sh` needs the pinned `libc` crate; it takes it from `$CARGO_HOME/registry/cache` when it is already there and only falls back to `static.crates.io`, so an offline environment should pre-populate that cache (or point `LIBC_URL` at a local copy). It also needs the `rust-src` component, which `scripts/ensure_toolchain.sh` installs.

## Safety & Execution Boundaries
Keep all work inside this repository. Do not copy kernel binaries to system paths, do not install or chainload on real hardware, and never run outside QEMU/OVMF. The scripts already sandbox execution; if you need fresh firmware or boot assets, use the provided automation instead of manual installs. Treat Limine, OVMF, and the kernel as development artifacts only and avoid touching `/boot`, `/efi`, or other host-level locations.


## Security Triage & CVSS Ledger (MANDATORY)

All agents must run a recurring vulnerability review loop for newly written and recently changed code.

### Required cadence
1. Run a security sweep after each major milestone and before any release/PR handoff.
2. Re-scan subsystems touched by recent commits (at minimum: syscall paths, memory management, filesystems, drivers).

### Triage workflow (strict order)
1. **List all findings first** in a raw triage section (do not score as CVSS yet).
2. For each finding, assign a **confidence score (0-100)** using this model:
   - Evidence quality (0-40): direct code proof, exact path/line references
   - Exploitability clarity (0-30): realistic attacker path and impact
   - Reproducibility (0-30): deterministic repro or strong step-by-step plausibility
3. Only findings with **confidence >= 80** are considered **guaranteed issues**.
4. Only guaranteed issues get a CVSS vector/score entry.
5. Use `scripts/cvss_calc.py` to compute CVSS v3.1 vectors/scores consistently across agents.

### CVSS file lifecycle requirements
1. Maintain `CVSS.md` as the single living ledger of **open findings only** (pre-alpha policy).
2. Every entry must include:
   - Stable internal ID (e.g., `SLOPOS-YYYY-NNNN`)
   - Status: `open` or `needs-retest`
   - Confidence score and reasoning
   - CVSS vector + score (only if confidence >= 80)
   - Exact evidence paths/lines
3. When an issue is fixed (pre-alpha policy):
   - **Remove it** from `CVSS.md`. SlopOS is pre-alpha with no audit-trail obligation, so resolved findings are deleted rather than retained as historical `fixed` records.
   - Internal IDs stay stable for findings that remain open; gaps in the numbering are expected.
4. When new guaranteed issues are found, append them with incremented IDs.

### Repro/examples (required when possible)
1. Add a minimal repro recipe for each guaranteed issue when technically feasible.
2. Repro can be a syscall sequence, malformed input artifact, or concise PoC steps.
3. If no safe repro is possible, document why and provide nearest deterministic validation method.

### Non-negotiable rule
- Never present speculative issues as CVSS-scored vulnerabilities.
- Confidence-gated, evidence-backed issues only.

---
