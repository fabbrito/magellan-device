# Magellan device

Device half of Magellan. What the system is meant to be lives in `docs/DESIGN.md`; it is malleable
until a rung builds it, after which the built shape wins and the document catches up. Vocabulary is
`docs/CONTEXT.md`. The cloud half is `magellan-cloud`; the two meet only at the contract.

**Write terse.** Sacrifice grammar for concision — prose, comments, commits, this file.

## Defer

Defer building until needed; revise decisions as issues surface.

Exempt from deferral — decide before the code lands. The test is reversibility cost, not importance:
an important decision that stays cheap to reverse still defers.

- the published contract — the cloud's `OpenAPI` document, when it lands
- durable storage shapes — anything written to flash
- boundaries later code assumes — layout, naming, dependency direction

## Hardware and cloud are the maintainer's to run

Anything that talks to a real source, a board, or the cloud is run by the maintainer, never by an
agent. Agents write the code and hand over the command. `cargo build`, `cargo nextest`, `cargo
clippy`, `make lint` and anything offline stay agent work; flashing a board, polling a live source,
and any request carrying a device token never leave the maintainer's hands.

## Guardrails

- Sources are read-only. Never write a register, never send a source a command.
- Device tokens and any credential never enter git — env, or a file `.gitignore` already covers.
  Tokens are stored hashed cloud-side.
- PII (IPs, MACs, home-network details, location) never enters git — config or env instead.
- The repository is meant to be published; a disclosure is what no later commit undoes.

## The commit gate

Lanes are lefthook's, in `lefthook.yml`: shared ones from `fabbrito/githooks` at a pinned `ref:`,
this repo's own beside them. Message policy is `.githooks/hooks.conf`. Bump by changing the `ref:`;
never redefine a shared job — the remote one wins. Per-repo tool flags go in the tool's own config
(`.shellcheckrc`).

Tools are pinned in `mise.toml`, Rust in `rust-toolchain.toml`. Hooks and `make` put mise's tools on
`PATH` themselves and fail without mise — never fall back to a system copy.

The gate is lanes matching paths by glob. A file no lane matches is never formatted or linted, so a
new kind of file means a lane. A missing tool fails: a skipped lane is not a green commit. Clippy
and the tests are not lanes — neither is fast enough to sit between you and a commit; they are `make
lint` and `make test`.

## Comments earn their keep

A comment carries knowledge from outside the code it sits on: why this choice, what breaks
otherwise, the gotcha the API hides. Narration of the code below it goes stale and dies. One line
where one line does.

## Promote what spans

Where knowledge lives, in order: `code > comments > docs > README`. Moving right raises altitude;
write at the lowest level that holds the knowledge.

| Where             | Holds                                                                                  |
| ----------------- | -------------------------------------------------------------------------------------- |
| `docs/DESIGN.md`  | what the system is meant to be — layers, invariants, the shape decisions check against |
| `AGENTS.md`       | how to work here — the process an agent follows, never facts about the system          |
| `docs/CONTEXT.md` | domain vocabulary                                                                      |
| `README.md`       | orientation — layout, setup, where the docs are                                        |
| `docs/agents/`    | how the engineering skills read this repo — issue tracker, domain docs consumption     |

A settled decision the project still lives under a year from now goes to `docs/adr/` — what was
chosen, what it costs, what reverses it, no paths or symbols.

Contradicting a row is allowed. Doing it quietly is not — name the line, and say which of the two
you would change.

**AGENTS.md is not a knowledge base.** The test: a rule that survives the code being rewritten is
process, and stays. A rule that stops being true when the code changes was a doc all along.

Name things in `docs/CONTEXT.md`'s vocabulary. A concept the glossary has no term for is a signal,
not a gap to fill in passing.

## The hidden contract

`.tmp/` holds plan-internal material — plans, handoffs, session scratch — and may go stale. `docs/`
and the README never mention it: they stand alone and publishable.

## Tests bite

A test proves the part under test and fails when that part breaks.

Work outward. Never inward.

| Where        | Mechanism                                  |
| ------------ | ------------------------------------------ |
| Happy path   | **assert** at runtime — crash loud on exit |
| Edges        | tests                                      |
| The specific | regression test the exact bug, once found  |

The device is tested against a fake cloud and fake sources, never against a board or production.

## Rust

One binary (`magellan`) over library crates; the binary wires, the crates do not wire each other.
The workspace denies panics, `unwrap`, indexing and `todo` (`Cargo.toml`) — a device that reboots on
its own has no one to read the backtrace. The style rulebook is `docs/STYLE.md`.

## Commits

- Never a red tree: every commit passes formatting, lint, typecheck and tests.
- `type(scope): subject`, scope a crate or a cross-cutting name. Subject-only by default; cut every
  word the diff already says.
- AI co-authored: `Co-Authored-By:` naming the model. Never a session link — history is permanent.
