# Not yet in the docs

- `vendor/gimli` is its crates.io release unmodified. `vendor/unwinding` is
  its release with three edits, and an upgrade re-applies them before the pin
  row in `scripts/check_vendor_pin.sh` is refreshed: gimli's `read` feature
  in place of `read-core`; `StoreInVec` in `src/unwinder/frame.rs`, which
  keeps the unwind tables on the heap rather than in stack arrays; and no
  `stmxcsr`/`fnstcw`/`ldmxcsr`/`fldcw` in `src/unwinder/arch/x86_64.rs`,
  instructions the soft-float rule forbids the kernel. `vendor/unwinding`
  omits the crate's `Cargo.lock`, `vendor/gimli` keeps its own, and both
  carry cargo's `.cargo-ok`. Page: `verification/tcb-discipline` (the annex
  row).
- `xtask lint unsafe-expansion`'s `SECTION_ALLOWLIST` holds `.slopos.cmdline`,
  the one entry that is neither a registry nor Limine's: OSTD's
  `cmdline_section!` places the kernel's built-in words there, `link.ld`
  asserts the section is 512 bytes, and kernel code reads it only through
  `black_box`, since `bootctl` rewrites it after link. Page:
  `verification/tcb-discipline`.
