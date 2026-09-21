# 4. The buffer is bounded, and what keeps a batch

- Status: accepted

## Chosen

The buffer holds a fixed number of batches and never grows to fit. Full, it drops the oldest, and
the sequence counter leaves a gap the cloud can see.

A batch leaves the buffer when the cloud commits it, and when the cloud rejects it permanently.
Every other outcome keeps it: no answer, a cloud that cannot commit right now, and a credential the
cloud refuses.

## Why

The device runs unattended for months on a board whose RAM and flash are fixed. A queue that grows
to fit trades a recoverable outage for a device that dies and takes the readings with it; a bounded
one turns the same outage into a known, stated loss.

Oldest-first because the newest readings are the ones still worth having, and because the loss then
has a shape: a gap says exactly how much went and when. A gap is a health signal, not a defect to
paper over — a device that renumbered to hide it would make dedup drop the wrong readings instead.

A refused credential keeps the buffer because the common cause is a rotation or a brief
misconfiguration, and neither is worth the readings that dropping would cost. The buffer's own bound
is what limits how long that can go on, so no second rule is needed.

## Cost

An outage longer than the bound loses readings, by design, and silently except for the gap.

A credential that is permanently dead pins the buffer until it fills and then discards from the
front — indistinguishable, from the device's side, from a long outage.

## Reverses

Spill to flash without a bound, trading the loss for a device that eventually fills its flash; or
treat a refused credential as permanent and drop on it. Both are changes to the buffer and the
upload loop, and neither touches the wire.
