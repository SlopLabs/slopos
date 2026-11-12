# SlopOS Plans

This directory holds only live proposals and open work, written statelessly
against the current tree: a plan describes what to build and why, never what
was already built or how the plan evolved. When part of a plan lands, remove
that part and re-baseline the rest; when nothing remains, delete the plan.
Git history preserves the record. Stable public documentation lives in the
sibling docs repo.

Even live plans can carry stale paths after refactors — verify paths before
editing.

## Current Files

| Document | Scope |
|----------|-------|
| `KNOWN_ISSUES.md` | Working notes on open issues; verify before using as source of truth |
| `ci-latency.md` | Where CI's wall clock goes, the lane rule that bounds it, and the cold-build work still open |
| `self-hosting-on-hardware.md` | Developing the kernel and the base on an installed machine: one build interface (done), the gates in the guest, a base tried before it is kept, a kernel and a base installed as a pair, boot options that follow the kernel, userland that never waits for a boot |
| `self-build.md` | A spike: the research that decides how SlopOS builds its own compilers, toolchain programs and install medium, to be replaced by the plan it leads to |
| `linux-interfaces.md` | A spike: the research that decides which of the 54 private calls move to an interface Linux defines, which move to userland and which stay, to be replaced by the plan it leads to |
| `installed-software.md` | Every installed program as an image under `/opt`: the toolchain built as one, images offered to a root with nobody to approve, the apps SlopOS ships as one, `/usr/local` and `tree-install` removed, an image identified by its name and its signer, no fixed number of images, and the word extension retired |

What has landed is described in the docs repo, the skills under
`.agents/skills/` and the code, not here.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
