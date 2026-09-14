# SlopOS Vulnerability Audit and CVSS Scoring

**No findings are open.** Last swept 2026-09-14 (the utilities becoming
executables: a multicall binary whose archive extractor, patch applier, regex
engine, inflater and digest all read attacker-supplied input, plus the `/bin`
symlink install and the `execve` thread-pointer reset). Three guaranteed
defects and six below the confidence bar; all fixed inside the same unreleased
change, so none is an entry.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no
> backwards-compatibility or audit-trail obligations, so this file tracks **open
> findings only**. A resolved finding is **removed**, never kept as a `fixed`
> record, and a defect fixed inside the change that introduced it never becomes
> one. IDs stay stable while a finding is open and are never reused, so gaps are
> expected. The git history is the audit trail: `git log -p -- CVSS.md` recovers
> any entry that was here, and a fix's own test is the durable record of it.

The highest ID issued so far is **SLOPOS-2026-0055**. The next finding is
`SLOPOS-2026-0056`.

## Open findings

None.

## Cadence

1. Sweep after each major milestone and before any release or PR handoff.
2. Re-scan what recent commits touched — at minimum syscall paths, memory
   management, filesystems and drivers.

## Triage workflow (strict order)

1. **List every finding first**, unscored.
2. Score confidence 0-100: evidence quality (0-40, direct code with exact
   `path:line`), exploitability clarity (0-30, a realistic attacker path),
   reproducibility (0-30).
3. Only **confidence >= 80** is a guaranteed issue.
4. Only a guaranteed issue gets a CVSS vector, computed with
   `python3 scripts/cvss_calc.py "CVSS:3.1/..."` so agents agree.
5. A finding below 80 with an attacker-reachable trigger is still recorded, with
   no vector and a note saying what evidence would raise it. One with no
   attacker-reachable trigger belongs in `plans/`, not here.

## Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- **Verify the claim before fixing it.** An entry can be wrong; read the code.
- A fix lands with a test that **fails without it** — confirmed by reverting the
  fix, not by assertion.

## Entry format

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps; if none is
  safely possible, say why and give the nearest deterministic validation
- Remediation: the shape of the fix, not "add a check"
```

## Scoring notes

CVSS 3.1 Base Score; `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`,
`9.0-10.0 Critical`.

SlopOS has no credential model, so "unprivileged local attacker" means any
process that can execute code: `AV:L/PR:L`.

A panic in a `#![forbid(unsafe_code)]` crate is an availability impact, never a
memory-safety one. Overflow checks are on in the dev and tests kernels and off
in release, so the same arithmetic defect is a panic in one build and a silent
wrong value in another — say which when scoring.
