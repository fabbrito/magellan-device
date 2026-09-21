# 6. A layer may be more than one crate

- Status: accepted
- Supersedes part of 1

## Chosen

A layer is one seam crate plus zero or more implementation crates. Within a layer, an implementation
crate may depend on its seam crate and on its siblings. Across layers, a crate depends only
downward, and only on the lower layer's seam — never on one of its implementations.

This replaces the rule that the contract is the only crate another crate may name. That rule stands
between layers, where it always meant something; inside a layer it only meant that a layer had
exactly one crate.

## Why

The layer that forces it is the drivers. One crate per kind of source is what keeps them from
growing into each other: two sources sharing one crate can share an internal module, and the wall
between them is a convention again — the erosion the crate layout exists to prevent. A source is
also the unit that changes, so a crate per source means adding one cannot break another by accident.

The seam stays in its own crate because that is what the layer above compiles against. An
implementation that vanishes, or three that appear, changes nothing for the layer above.

Keeping the original rule was the alternative, and it read well until a layer needed two
implementations. It would have forced either one crate holding every driver, with the walls back to
convention, or a driver naming the layer above to reach a shared type, which is the dependency
direction inverted.

## Cost

The crate count stops tracking the layer count, so the layout no longer reads off the list of layers
and a newcomer needs telling which crates are one layer.

A seam crate holding a single trait looks empty enough to invite folding it into its one
implementation, which is exactly the move this forbids.

Ceremony per implementation: a manifest and a lint block for what may be a few hundred lines.

## Reverses

Collapse a layer's crates into modules of its seam crate. The code moves unchanged — the module
walls are where the crate walls were — and the boundaries inside that layer become convention again,
held by review.
