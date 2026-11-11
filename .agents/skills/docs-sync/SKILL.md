---
name: docs-sync
description: >
  Bring the slopos-docs site up to a slopos commit and empty the skills'
  pending notes. Use for "sync the docs", when `xtask lint agents-doc` reports
  a full `pending.md`, or before a release.
---

The docs repository (`$SLOPOS_DOCS`, else `../slopos-docs`) is the long form of
everything `AGENTS.md` and the skills only point at. Its `SYNCED` file holds
the slopos commit its pages were last checked against.

## Steps

1. Read `STYLE.md` in the docs repository. It is binding.
2. Here, `git log --stat $(cat ../slopos-docs/SYNCED)..HEAD` says what
   changed, and the same range with `-p -- .agents/skills AGENTS.md
   '*/AGENTS.md'` says which rules moved.
3. For each `.agents/skills/*/pending.md`: state every note in a page its
   skill names, where `STYLE.md` puts it (the explanation's body, its For
   contributors section, a reference table, In the source). Check each against
   the source at HEAD; a note is a lead, not the text. Then delete the file.
4. For each subsystem the log shows changed: read the pages its skill names
   against the source and fix what HEAD made wrong.
5. A new idea gets a row in `abi/glossary.mdx`. A new page gets its section's
   `meta.json` entry and a skill that names it as `docs/<section>/<page>`.
6. Status pages state the date and the slopos commit they were checked
   against.
7. In the docs repository: `bun run types:check && bun run lint && bun run
   build`.
8. Write HEAD's full hash to `SYNCED`. Commit the docs (`docs: sync every
   page to slopos <short hash>`), then commit the removed `pending.md` files
   here.

## Rules

- Facts come from the source, never from another page or from memory.
- No measured timings, sizes or counts outside Status pages.
- A bare `{`, `}` or `<` in MDX prose breaks the build; keep them in code
  spans.
- A skill's rules change only when an invariant does. The sync moves notes
  into pages; it does not grow `SKILL.md`.
- The pin is the commit whose source you read. Sync from a clean tree.
