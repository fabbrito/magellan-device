# 7. The device is asynchronous, on one board

- Status: accepted

## Chosen

The device runs on one async runtime, tokio, and targets one board: a 32-bit ARM Linux machine.
Reading a source and uploading a batch are both async, and the source seam is an async trait.
Polling and uploading are separate tasks, so a stalled network cannot delay a poll.

No second board is designed for. When one arrives it gets this decision revisited, not accommodated
in advance.

## Why

The transport this device exists to carry is already written, already async, and already proven
against the hardware. Re-deriving it as blocking, to keep the options of a board nobody owns, spends
real work on a hypothetical one.

A synchronous seam over an async transport is worse than either alone: it means blocking inside an
async context, which needs deliberate care at every call site to be merely wrong rather than a
deadlock. One model throughout removes the question.

Designing for two boards was the alternative, and it was drawn out far enough to price: a platform
crate per board, a feature per board, a transport rewritten to the narrower board's rules, and a
test matrix over combinations only one of which any hardware could run. Every part of it was
scaffolding for a board that does not exist.

## Cost

An executor on a device that may later be small. The binary carries a scheduler that blocking reads
on a single thread would not have needed.

A second board inherits a choice it had no say in. If it cannot host this runtime, the transport is
re-derived then — the work moves rather than disappears, and it moves to the moment there is
hardware to check it against.

An async trait that must stay object-safe needs a boxing crate until the language does it directly.

Nothing here is checked by the compiler: one board is a decision, not a constraint the build
enforces, so a stray assumption about a second one fails review or nothing.

## Reverses

A board that cannot host the runtime. The seam becomes synchronous, the transport splits along its
existing module walls, and scheduling collapses from tasks to threads — a thread per concern is the
same shape with a heavier unit.
