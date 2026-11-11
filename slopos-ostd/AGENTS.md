# slopos-ostd

The one kernel crate that may contain `unsafe`. Every other kernel crate is `#![forbid(unsafe_code)]` and trusts what this one exports, so a contract stated loosely here is a fault that surfaces in a safe caller.

- Keep each `unsafe` block tiny, with a `// SAFETY:` line saying why it holds here.
- A safe `fn` never carries a `# Safety` section (`clippy::unnecessary_safety_doc`, baseline 0). Express the obligation in the signature: a capability witness (`&IrqDisabled`, `&BspToken`), a validated newtype, a linear handle, an owning reference, a sealed trait, or a slice in place of a pointer and a length.
- An exception to a lint is a site-local `#[expect(lint, reason = "...")]`, never a blanket allow. A kernel-only one is `#[cfg_attr(target_os = "none", expect(...))]`, because the host and userland builds check this crate too.
- A macro defined here expands into forbid crates, where rustc reports nothing. Whatever it emits must pass `xtask lint unsafe-expansion`: no executable `unsafe`, `unsafe impl` only of an allowlisted trait, `link_section` only of a `link.ld` section.
- This crate is linked into userland binaries. Register nothing in a kernel linker registry from here (`kcommand!`, drivers, tests): a userland link brackets no kernel section.
- Allocation goes through `mm::heap` (`KBox`, `KVec`, `KArc`, ...). Build a large struct in place with `KBox::try_init` or `PinBox::try_init`, so it never sits on a stack frame.
- Raw task pointers stay inside the placement and link primitives, the PCR slots and the pre-heap stubs. PCR offset 40 is read from assembly on every instrumented prologue and is frozen.
- No `mem::forget` and no `ManuallyDrop::new`: a kernel object's destructor gives back what it owns.
- Before changing `task_is_dispatch_pinned`, the frame refcounts or `vm_space`, read the header of `verification/STATUS.md`: the proofs keep verifying whether or not their model still describes the tree.

Read the `slopos-gates` skill for the gates and `slopos-memory-sched` for task ownership and allocation.

Checks: `just check-miri`, `just verify`, `cargo run -p slopos-xtask -- lint unsafe-expansion tcb-ratio`.
