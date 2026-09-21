<!-- Title is the merge commit subject: type(scope): subject — lowercase, no trailing period, <= 72
chars. The types and scopes are the gate's, in .githooks/hooks.conf.

Write terse throughout — sacrifice grammar for concision, and cut every word the diff already
says. A section with nothing to add is deleted, not filled. -->

## What

<!-- What changed for a caller. The diff says how. -->

## Why

<!-- What made it necessary: the problem, or the decision it enacts. Not what, not how. -->

## Notes

<!--
Only what the diff cannot say: a gotcha found, something deliberately not done.

Trailers go last, in the commit. Never a Claude-Session trailer or any session URL — not in a
commit, not here. Session links are leakage into permanent shared history.
-->

<!--
Enacting a decision? The ADR lands in THIS PR, with its reversal condition, and only if the
reasoning still holds now that the code exists. Drop the ADR if the surviving reason is external
to the system — that is not a decision record.

Superseding an earlier ADR? Link it. Edit the superseded file only to name what replaced it —
its reasoning stays as written.
-->
