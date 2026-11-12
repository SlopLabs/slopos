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
