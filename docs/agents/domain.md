# Domain Docs

How the skills consume this repo's domain docs. Single-context: one root `CONTEXT.md`, decisions in
`docs/adr/`.

## Before exploring, read these

- **`docs/CONTEXT.md`** — vocabulary.
- **`docs/adr/`** — ADRs touching the area you're about to work in.

Missing is fine: proceed silently, don't flag it; they get created lazily once terms or decisions
actually resolve.

## Use the glossary's vocabulary

Name concepts (issue titles, proposals, test names) as `docs/CONTEXT.md` defines them; don't drift
to synonyms it avoids. A missing concept is a signal — either you're inventing language, or it's a
real gap worth recording.

## Flag ADR conflicts

Surface a contradiction rather than silently overriding:

> _Contradicts ADR-0007 (event-sourced orders), but worth reopening because…_
