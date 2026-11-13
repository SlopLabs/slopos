# Pending for the docs

- `docs/architecture/base`, `docs/abi/interfaces`, `docs/self-hosting/updating`:
  before v1 `BASE_INTERFACE_MIN` is `BASE_INTERFACE`, so a kernel serves the
  base interface of its own tree alone and `info` prints `Base interfaces:
  <n>-<n>`. A change to the private call surface still takes the next number
  and a ledger row, but weighs no older base: a call may change shape or be
  removed, and a retired call keeps no slot. A kernel and a base either side
  of such a change go in as a pair, the base first, which waits on the disk,
  then the kernel into the spare slot. The range a kernel serves, its note
  and `bootctl`'s refusals stay as they are, for v1 to widen the range again.
- `docs/verification/toolchain-gates`, `docs/abi/interfaces`: the two contract
  gates are xtask modules, `scripts/check_base_interface.sh`,
  `scripts/check_libc_exports.sh` and their Python helpers are gone, and both
  run on SlopOS. `just base-interface [--record | --print]` (`xtask
  base-interface`) reads slopos-abi's rustdoc JSON as before, and also the
  Linux-numbered calls the kernel answers: `static SYSCALL_TABLE` in
  `core/src/syscall/handlers.rs`, one `linux <n> <SYSCALL_NAME>` line each,
  with findings `unresolved`, `range` (a number at or above
  `SYSCALL_PRIVATE_BASE`), `meta` and a floor of 100. A call the kernel
  starts or stops answering takes the next number; that took interface 13.
  `just libc-exports [--record]` (`xtask libc-exports <libc.so>`) reads
  `.dynsym` with `object` rather than `llvm-readobj`, and canonicalises the
  dump of the clang `scripts/cxx_host_tools.sh` resolves (`$CLANG`) in Rust;
  neither half is ever skipped. Their fixtures are `cargo test -p
  slopos-xtask`. `just install` runs both before writing anything,
  `install-kernel` the first. Pages naming the old scripts too: `abi/syscalls`,
  `architecture/{base,software,syscalls,userland}`,
  `verification/tcb-discipline`, and `development/adding-a-syscall`, whose
  `--record` steps become `just base-interface --record` / `just libc-exports
  --record`, and which gains the step that registering a Linux call in
  `SYSCALL_TABLE` takes the next `BASE_INTERFACE`.
