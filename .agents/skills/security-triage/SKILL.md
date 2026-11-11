---
name: security-triage
description: >
  The recurring vulnerability review and the `CVSS.md` ledger. Use after a
  major milestone, before a release or PR handoff, when asked for a security
  sweep, and when a fix closes a finding in `CVSS.md`.
---

## Read first

- `docs/development/security-cvss`: the triage loop, worked through
- `docs/status/known-limitations`: defects already known

## When

After each major milestone and before any release or handoff. Re-scan what
recent commits touched; at least syscall paths, memory management,
filesystems and drivers.

## Triage, in this order

1. List every finding raw, unscored.
2. Give each a confidence from 0 to 100: evidence quality (0 to 40: direct
   code proof, exact paths and lines), exploitability clarity (0 to 30: a
   realistic attacker path and impact), reproducibility (0 to 30: a
   deterministic repro or a step-by-step plausible one).
3. Confidence of 80 or more is a guaranteed issue. Only those get a CVSS
   vector and score, computed with `scripts/cvss_calc.py` (CVSS v3.1).
4. Below 80 with a trigger an attacker reaches: record it with no vector and
   say what evidence would raise it. One no attacker reaches goes in
   `plans/`.

## The ledger

`CVSS.md` lists open findings only. Each entry has:

- a stable id, `SLOPOS-YYYY-NNNN`;
- status `open` or `needs-retest`;
- the confidence and its reasoning;
- the vector and score, only at confidence 80 or more;
- exact evidence paths and lines;
- a minimal repro (a syscall sequence, a malformed input, concise steps), or
  why none is safe and the nearest deterministic validation.

A fixed finding is removed, not marked: the project is pre-alpha and keeps no
audit trail. Ids of open findings never change, so gaps are expected. New
findings append with the next id.

## Rules a change must keep

- Never present a speculative issue as CVSS-scored.
- Evidence first: no entry without a path and a line.
- A fix that closes a finding deletes its entry in the same commit.

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
