# 11. The heartbeat is apart from data

- Status: accepted

## Chosen

The heartbeat is its own request on its own cadence, day and night, never inside a batch, never
buffered or retried. It names its boot, and when the device last heard each source.

When a source is worth polling is the source's: an optional window on the source, not the device.

## Why

Inside a batch, liveness hung on the domain: an inverter dark at night meant a device silent for
twelve hours, a dead device indistinguishable from a quiet night. A heartbeat is state, not record;
one sent late reports a device that no longer exists.

Last heard carries the chain's health one hop past what the cloud can see.

## Cost

A request an hour per device and a route in the contract. A heartbeat lost to an outage is gone.

## Reverses

A cloud that needs replayable health, or a link billed per request.
