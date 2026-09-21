# Magellan Device

Device half of Magellan — reads small sources, buffers the readings across outages, and uploads them
to the cloud, which stores whatever the device's manifest declares. The cloud half is
`magellan-cloud`; the two meet only at the contract.

- North star: `docs/DESIGN.md`
- Vocabulary: `docs/CONTEXT.md`
- Style: `docs/STYLE.md`
- Settled decisions: `docs/adr/`
- Conventions: `AGENTS.md`

MIT licensed — see `LICENSE`.

## Status

Scaffold. The workspace is wired, the contract types are written from the cloud's Zod authoring
source, the docs are written; no runtime behaviour yet. The OpenAPI contract is not in hand.

## Setup

```sh
make hooks      # once per clone: enable .githooks
cargo build     # debug binary
```

`make help` lists the rest, so they are not copied here to rot.

## Layout

```
crates/contract/   one native Rust reading of the contract schemas
crates/runtime/    Layer 5 — config, clock, scheduling, buffer, upload, health
crates/driver/     Layer 6 — the source-driver seam
crates/drivers/    Layer 6 — one crate per kind of source; sofar reads an inverter
crates/platform/   Layer 7 — the OS seam: Linux on 32-bit ARM
crates/magellan/   the binary — wires the crates, owns the subcommands
docs/              design, vocabulary, style, decisions
scripts/           release plumbing: notes, tag, publish
.githooks/         the commit gate — a vendored engine, all policy in hooks.conf
Makefile           the targets; the gate's lanes live in hooks.conf, not here
```

Layer 8 (hardware) is the board and its buses; it is not a crate.

A driver crate keeps what it decodes against beside it: `reference/` holds the external document,
and its README carries the provenance and what the captures prove.

The crates exist; what goes in them lands rung by rung. `docs/DESIGN.md` is the layers and the
boundaries.
