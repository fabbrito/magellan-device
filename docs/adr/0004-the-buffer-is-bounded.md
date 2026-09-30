# 4. The buffer is bounded, and what keeps a batch

- Status: accepted (amended)

## Chosen

The buffer holds a fixed number of batches and never grows to fit. Full, it drops the oldest, and
the sequence counter leaves a gap the cloud can see.

A batch leaves the buffer when the cloud commits it, and when the cloud rejects it permanently.
Every other outcome keeps it: no answer, a cloud that cannot commit right now, and a credential the
cloud refuses.

The buffer is written through to flash: one file a batch, stored as it is queued and removed as it
leaves, beside the manifests they name. Files are named by queue position across boots, so order
needs no clock. A boot drains what the last one left first. The bound is sized to flash.

## Why

The device runs unattended for months on a board whose RAM and flash are fixed. A queue that grows
to fit trades a recoverable outage for a device that dies and takes the readings with it; a bounded
one turns the same outage into a known, stated loss.

Oldest-first because the newest readings are the ones still worth having, and because the loss then
has a shape: a gap says exactly how much went and when. A gap is a health signal, not a defect to
paper over — a device that renumbered to hide it would blind the one account of the loss there is.

A refused credential keeps the buffer because the common cause is a rotation or a brief
misconfiguration, and neither is worth the readings that dropping would cost. The buffer's own bound
is what limits how long that can go on, so no second rule is needed.

In RAM alone, a power cut lost the buffer. Write-through rather than spill-on-full, since a power
cut gives no warning; one file a batch rather than a log, since a sweep every few minutes is no
write rate a card notices.

## Cost

An outage longer than the bound loses readings, by design, and silently except for the gap.

A credential that is permanently dead pins the buffer until it fills and then discards from the
front — indistinguishable, from the device's side, from a long outage.

A flash write a sweep. A removal a power cut forgets resends its batch, which the cloud absorbs.

## Reverses

Spill to flash without a bound, trading the loss for a device that eventually fills its flash; or
treat a refused credential as permanent and drop on it. Both are changes to the buffer and the
upload loop, and neither touches the wire. Flash that cannot take a write a sweep puts the buffer
back in RAM.
