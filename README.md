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

Reads a Sofar inverter over Modbus TCP, composes its manifest from the register profile, buffers
batches to flash and drains them against the contract's status classes, and sends an hourly
heartbeat apart from them. Runs against a board and the cloud: `magellan-cloud` speaks the batch
identity this device sends (ADR 8), and its OpenAPI document is the contract `crates/contract`
mirrors.

## Setup

Needs [mise](https://mise.jdx.dev) and rustup; every other tool is pinned in mise (`mise.toml`,
`.config/mise/`), Rust in `rust-toolchain.toml`.

```sh
make deps       # the pinned tools; again after a bump
make hooks      # once per clone: lefthook's git hooks
cargo build     # debug binary
```

`make help` lists the rest, so they are not copied here to rot.

## Running it

The installation lives in `config.toml`, copied from `config.example.toml`: the device, its site,
every source and what finds it. Only the device token comes from the environment; `.env.example`
names it. `.gitignore` covers both the real config and the real `.env.local` — a real config names a
home, and is never committed.

```sh
magellan check    # read the config and the profiles, say what would be declared, touch nothing
magellan run      # poll, buffer and upload until stopped
```

`check` is the one to reach for first: it exercises the configuration, every driver's profile and
the composed manifest against the contract, without a network or a board. It is **temporary**: a
scaffold to prove the config until the capture path stands on its own, and it prints to stdout and
stderr rather than the journal for that reason.

Both exit **78** (`EX_CONFIG`) when the configuration is at fault, and 1 for anything else. A unit
should carry `RestartPreventExitStatus=78`: an outage is worth retrying, a typo never is.

## Releasing

A release is a tag on main, built in CI and drafted until a board has run it (ADR 12):

```sh
make release VERSION=x.y.z   # bump on release/vx.y.z, open its PR; check gates it
                             # merge it, then on main:
make tag                     # tag origin/main as its version, push the tag alone
```

The tag drafts the release (`.github/workflows/release.yml`). Run the draft's binary on a board,
then `gh release edit vx.y.z --draft=false`. Verify a published binary: signed by the release
workflow, on its tag, not by any workflow here.

```sh
gh attestation verify <binary> -R fabbrito/magellan-device \
  --signer-workflow fabbrito/magellan-device/.github/workflows/release.yml \
  --source-ref refs/tags/vx.y.z
```

Repository settings are the maintainer's, by hand — no workflow holds them. The rulesets are
`.github/rulesets/`: `main` takes no push but a merged PR with `check` green, and no bypass; a `v*`
tag is never moved or deleted, and only an admin creates one. In order — `selected` comes before its
list:

```sh
r=repos/fabbrito/magellan-device
for f in .github/rulesets/*.json; do gh api -X POST $r/rulesets --input "$f"; done
gh api -X PUT $r/actions/permissions -F enabled=true -f allowed_actions=selected \
  -F sha_pinning_required=true
gh api -X PUT $r/actions/permissions/selected-actions --input - <<<'{
  "github_owned_allowed": true, "verified_allowed": false,
  "patterns_allowed": ["jdx/mise-action@*", "Swatinem/rust-cache@*"]}'
gh api -X PUT $r/actions/permissions/fork-pr-contributor-approval \
  -f approval_policy=all_external_contributors
gh api -X PUT $r/immutable-releases
gh api -X PUT $r/private-vulnerability-reporting
gh api -X PUT $r/vulnerability-alerts
```

`GITHUB_TOKEN` stays read-only by default, and Actions may not approve PRs. A ruleset change is
another run of its file through `gh api -X PUT $r/rulesets/<id>`.

## Layout

```
crates/contract/   one native Rust reading of the contract schemas
crates/runtime/    Layer 5 — config, clock, scheduling, buffer, upload, health
crates/driver/     Layer 6 — the source-driver seam
crates/drivers/    Layer 6 — one crate per kind of source; sofar reads an inverter;
                   node reads a board on the LAN; modbus is the reads they share,
                   read-only by type
crates/platform/   Layer 7 — the OS seam: Linux on 32-bit ARM
crates/magellan/   the binary — wires the crates, owns the subcommands
docs/              design, vocabulary, style, decisions
e2e/               each release binary run as a process, and its configs
scripts/           release plumbing: version, notes, the release PR, the tag
.github/           check on every PR, release on a tag; templates
.config/           the gate's pieces, copied from repokit: tool pins, lanes,
                   the commit-message policy (commitlint.config.mjs)
lefthook.yml       the commit gate - which lanes the hooks run
mise.toml          this repo's own tools, pinned
Makefile           the targets; check and fmt are .config/make's lanes
```

Layer 8 (hardware) is the board and its buses; it is not a crate.

A driver crate keeps what it decodes against beside it: `reference/` holds the external document,
and its README carries the provenance and what the captures prove.

The crates exist; what goes in them lands rung by rung. `docs/DESIGN.md` is the layers and the
boundaries.
