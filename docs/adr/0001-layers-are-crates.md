# 1. Layers are crates, and the binary wires them

- Status: accepted

## Chosen

Each device layer is a library crate, and one binary depends on all of them and wires them together.
No library crate depends on another, except on the contract — the only shared vocabulary, so the
only crate the others may name. The binary is the single artifact built, flashed and versioned.

## Why

A layer boundary that is only a module convention erodes, quietly, one import at a time. A crate
boundary is checked by the compiler: dependency direction stops being a rule in a document and
becomes a fact of the build, and a layer reaching sideways fails to compile rather than fails
review.

One binary because a device is flashed, not deployed. Two artifacts would mean two versions on one
board and a question about which is running; one artifact makes the version the device reports the
version of everything it does.

The wiring lives in the binary because the alternative is a library crate that knows which other
implementation it gets, which is the boundary gone. The runtime is written against a seam; what
satisfies that seam is chosen once, where the program starts.

## Cost

Crate ceremony on a program that would fit in a file today: a manifest and a lint block per crate,
and a rebuild graph wider than the code in it.

A type shared by two layers but belonging to neither the contract nor either of them has nowhere to
live, and forces a decision that a module layout would have let slide.

## Reverses

Collapse the crates into modules of the binary. The code moves unchanged — the boundaries are
exactly where the module walls go — and the dependency direction becomes a convention again, held by
review instead of by the compiler.
