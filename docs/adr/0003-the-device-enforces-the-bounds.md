# 3. The device enforces the contract's bounds

- Status: accepted

## Chosen

The device carries its own copy of the contract's bounds — counts, ranges, lengths, patterns — and
refuses a manifest or a batch that violates one before it reaches the upload queue. A refusal is an
operating error: journalled and dropped, never a crash. The cloud's rejection stays the authority;
it is no longer how the device finds out.

## Why

A bound written in a comment enforces nothing, and the device's native types are wider than the
contract in both directions that matter — a signed 64-bit value and an unsigned millisecond
timestamp each reach past what the cloud accepts. "The type is the bound" is false here, so the
check has to be written or it does not exist.

Learning a violation from a rejection means learning it on a board, unattended, after readings are
already queued against a shape that can never commit. The buffer then holds work whose only future
is to be dropped, and the gap that follows looks like an outage.

Refusing early is also where the check is cheap: one batch, in RAM, before it costs flash and a
round trip.

## Cost

A second copy of numbers that belong to the other half of the system. It drifts silently, and
nothing catches the drift until the published document can be read mechanically.

Refusing is not free either: a bound that is wrong on the device refuses readings the cloud would
have taken, and that failure is invisible from the cloud.

## Reverses

Read the bounds from the published document instead of mirroring them, or delete the check and let
the rejection teach. The refusal is internal — nothing on the wire changes either way.
