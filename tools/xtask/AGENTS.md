# tools/xtask

The tree's checks. A new check is a module here, never a shell script; a new `scripts/check_*.sh` is refused in review. Read the `slopos-gates` skill first.

- A source gate is a module of `src/lint/`, registered in `lint::GATES`. It matches `syn` syntax trees, not lines, so a brace in a char literal or a `//` in a string reads as the compiler reads it.
- An ELF gate is a module of `src/elf/`. It reads the image with `object` and decodes code with `iced-x86`, never a disassembler's text.
- The module's `//!` doc states the invariant the gate holds and why it matters. That doc is the gate's specification.
- Every gate has `#[test]`s that plant violations and the forms it deliberately accepts: exact hit counts on one, silence on the other. A check never seen to reject has not been seen to work. Source fixtures use `fixture::Tree`; ELF fixtures are assembled in memory with `object`'s writer.
- A pass prints one summary line saying what was measured, so a gate that scanned nothing cannot read like one that scanned the tree. Finding nothing to scan is an error, not a pass.
- An allowlist is a constant in the module or a tracked file under `scripts/gates/`. An entry that matches nothing fails the gate.
- A gate that cannot run returns `Err` (exit 2); a gate that found something returns a failing `Report` (exit 1).
- The graders of a boot's serial log are Go, in `tools/run_tests/gates/`, with Go tests.

Checks: `cargo test -p slopos-xtask`, `cargo run -p slopos-xtask -- lint <gate>`, `just test-host`.
