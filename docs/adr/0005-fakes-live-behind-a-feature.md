# 5. Fakes live beside the seam, behind a feature

- Status: accepted

## Chosen

Each fake lives in the crate that owns the seam it satisfies, behind a cargo feature named for it. A
test that needs one enables the feature on that crate as a dev-dependency. There is no crate whose
purpose is to hold fakes, and no fake reachable from a release build.

## Why

A fake is a second implementation of a seam, so it belongs where the seam is defined: the trait and
the thing that satisfies it move together, and a seam that changes cannot leave its fake behind in
another crate.

A test-only module does not cross a crate boundary, so the binary's end-to-end tests could not reach
one — the fake cloud would be written twice, and the two would drift exactly where they must agree.
A feature crosses that boundary and still disappears from a build that does not ask for it.

A crate for fakes was the other candidate. It would put every fake in one place, at the price of a
crate that is not a layer, which is the one thing the crate layout means.

## Cost

Feature plumbing: a flag per crate, and a dev-dependency that names it at every call site. A feature
left off silently removes the fake rather than failing to find it, so the error arrives as a missing
symbol rather than as a sentence.

A crate carries its own test scaffolding in its own source, so reading the crate means reading past
it.

## Reverses

Move the fakes into a crate of their own and drop the features. Call sites change; nothing about the
seams does.
