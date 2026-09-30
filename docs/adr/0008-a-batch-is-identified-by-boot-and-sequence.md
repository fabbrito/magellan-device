# 8. A batch is identified by its boot and its sequence

- Status: accepted (amended)

## Chosen

Every batch carries the id of the boot that produced it, alongside a counter monotonic within that
boot. The two together identify a batch, and a gap in them is how a loss becomes visible; the cloud
deduplicates on each reading's own key, never on the pair. The counter restarts from zero at every
boot.

The boot id was already defined, held in RAM and drawn fresh each time the device starts. It moves
out of the optional heartbeat and onto the batch itself, because something a batch is identified by
cannot be optional.

A batch the buffer keeps across a reboot (ADR 4) keeps its pair; the counter itself is never stored.

## Why

A counter alone had to be unique for the life of the device, which meant surviving a reboot, which
meant writing it down: one small file between a power cut and a sequence that repeats, and a gap
that can no longer be told from a replay.

The pair removes the need entirely. The boot id already exists, already needs no storage, and
already carries exactly the information the counter was missing: which run of the device this is.
Uniqueness comes from entropy drawn once per boot rather than from a write that must survive losing
power at the wrong moment.

Persisting the counter was the alternative. It keeps the wire unchanged, at the price of a durable
shape whose loss is silent, wear on the flash it lives in, and a failure that is invisible until the
archive is missing a day.

The device's clock was a third candidate, seeding the counter from wall time. It fails in exactly
the case that matters: a board with no battery-backed clock reads 1970 until the network steps it,
which is the same moment the first readings are taken.

## Cost

Uniqueness rests on entropy. Two boots that draw the same id alias into one in gap detection, and a
loss in one can hide behind the other's sequence. The readings themselves still land: dedup never
looks at the pair.

A counter that restarts is harder to read at a glance: a gap means loss, but so does a restart, and
telling them apart means looking at the boot id.

## Reverses

Make the counter durable and drop it back to identifying a batch alone. The wire loses a field, and
the device gains the one piece of storage this avoided.
