# scripts/gates

Tracked gate data: measured allowlists, caps, floors, recorded lists and verdicts. A diff here weakens or tightens a check, so it is reviewed as one.

- Never edit a file here by hand to make a run pass. Regenerate it with the gate's own `--emit-allowlist` or `--record`; the `ratchet-remeasure` skill has each command.
- Re-measure in the commit that causes the delta, and say in the message which lock, test, quota account, stack frame or call path added it.
- An entry that matches nothing in what is being checked is a dead exemption and fails the gate. Remove it by re-measuring.
- A floor (`min-records`, `min-classes`, `min-bytes`, ...) exists so that a measurement that stopped happening does not read as a perfect one. Do not lower one to pass.
- One file per variant (`dev`, `release`, `tests`, `release-tests`): their codegen differs, and a list measured on one says nothing of another.
- `base-interface.txt`: an issued row is never rewritten; a changed surface appends the next number.
- `libc/exports.txt` and `libc/layouts.txt` only grow. A removal is a deliberate hand edit, called out in the commit message.
- `syscall/linux-x86_64.tbl` is vendored from Linux's allocation; it changes only by re-vendoring.
- `codegen/` and `linker/` verdicts fail in both directions; a `lacks` that turned present is a decision to re-take.
