# 12. A release is a tag on main, drafted by CI

- Status: accepted

## Chosen

A release is a version tag on a commit of main. CI builds it, runs each binary, attests the
binaries' provenance and drafts the release; the maintainer runs the draft on a board and publishes
it. The version bump reaches main as a PR like any other change.

## Why

Built on the maintainer's machine, a release was whatever that machine held: its toolchain, its
tree, its uncommitted state, and no record anyone else could check. Built by CI from a tag, the
binary is the one check ran, and the attestation says which commit and which workflow made it.

The draft keeps the board in the loop. CI proves the binary runs on its target under emulation; only
a board proves it runs on the hardware, and that stays the maintainer's.

main takes no push but a merge, without bypass, so the bump cannot be committed to main from a
machine: it rides a PR, and the tag follows the merge.

## Cost

A release is two steps apart in time — a PR to merge, then a tag — where it was one script. A tag
pushed is public, draft or not; a bad build gets the next patch, never a retag.

The release depends on GitHub's runners and the actions it pins; a release cannot be cut offline.

## Reverses

A signing key the maintainer must hold, a board CI can reach, or a second distribution channel that
does not read GitHub releases.
