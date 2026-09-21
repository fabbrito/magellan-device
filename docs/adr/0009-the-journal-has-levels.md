# 9. The journal has levels, a channel, and a redaction rule

- Status: accepted

## Chosen

Diagnostics are one stream with five levels, and the level is the policy:

- `error!` — the run is ending and a person must act.
- `warn!` — the loop continues, but something was lost or degraded: a reading, a source, a
  credential.
- `info!` — a state change or the device's pulse: start, the window, one line per sweep.
- `debug!` — inside one unit of work: a range read, a request.
- `trace!` — bytes on the wire. Reserved; no call site yet.

The channel is stderr, or the systemd journal when `JOURNAL_STREAM` is set. The default filter is
`info`; an invalid `RUST_LOG` falls back to `info` rather than failing to start. ANSI is on only
when stderr is a terminal. Credentials never enter the journal; a serial is masked at the call site
that holds it, while the device's own names — device id, endpoint, a LAN address — may.

## Why

A device that reboots on its own has no one to read a backtrace, so the journal is the only account
of what it did, and it is read by severity. Two rules fall out of that. Data loss is never below
`warn`: a dropped batch is the one event the operator must not have to opt into seeing. And `info`
is silent only when nothing is happening — night, not a working sweep — so an absent line means the
device is idle, never that it is stuck.

The level was previously implicit and wrong: the default filter was `ERROR`, and nothing in the tree
logs that loud, so a healthy run was indistinguishable from a hung one. Stating the ladder makes
that a decision rather than an accident.

The channel is a decision because the journal and the records are different things. Diagnostics go
to stderr, leaving stdout for program output; under a unit, the journald layer files each entry with
its own priority instead of relying on a captured text stream.

Masking at the call site keeps the decision where the knowledge is: `Token` already redacts itself,
and a serial is masked by whoever holds it, rather than by a logging helper that cannot know what it
was handed. A journal is copied, shipped and pasted into issues, so an identifier that maps to a
home network is not worth the convenience.

## Cost

Volume. A sweep logged at `info` is one line every cadence, and a range read at `debug` is one per
range; the ladder trades a quiet journal for one that shows the device working.

Two paths — journald and stderr — means the format differs between a dev tree and a unit, so a
reproduction has to say which it came from. The journald layer is also a dependency on systemd's
socket; where it is absent, the fallback is a text stream.

Redaction is discipline, not a type. A call site that forgets is not caught by the compiler, which
is why the serial only ever appears through an error that names it.

## Reverses

Drop the journald layer and read stderr everywhere: one format, and systemd files the captured
stream. Raise `warn` to `error` for the events the operator must act on, at the price of a filter
that no longer tells loss from a dead device.
