# Style

The rulebook for writing code here, adapted from
[TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md) — that is
where the rules and their reasons come from.

Goals, in order: **safety, then performance, then developer experience.** Style is how the three are
brought together, not decoration. Readability is table stakes, never the point.

Simplicity is the last draft, not the first: it takes sketches, passes and a throwaway to land, and
that cost belongs in the design, not production.

The language is Rust. Names on the wire belong to the contract — read, not chosen.

## Bound everything

A loop, a queue, a buffer, a body, a manifest: everything has a limit, so state it. A bound turns a
spike into a refusal, not a stall. A loop that cannot terminate is asserted rather than trusted.

Memory is bounded too: allocate against a stated cap, never grow unbounded at runtime. State a bound
where it is enforced, once, so the bound a reader trusts is the bound in force.

Use explicit-width integers, never the architecture's word size: the boards are 32-bit, so `usize`
is not enough for a timestamp and a wider integer is a deliberate choice.

## Assert programmer errors

An operating error is expected and handled. A programmer error is not: the only correct handling is
to crash. A crash on a device is a reboot, and the buffer retries the batch — loud, and never a
corrupt reading.

- Assert arguments, return values, preconditions, postconditions and invariants **on infallible
  paths**. A function must not operate blindly on data it has not checked; where it can fail, it
  returns a refusal rather than asserting. Tiger Style's quota assumes a language whose failure mode
  is a panic, and this workspace denies the panic family on purpose.
- Assert the positive space you expect **and** the negative space you do not; that boundary is where
  bugs live.
- Pair assertions: assert the same property in two places, before a write and again after reading it
  back.
- Split a compound assertion into separate assertions, for a sharper failure.
- Assert an implication on a single line.
- A blatantly true assertion is stronger documentation than a comment where the condition is
  critical and surprising.
- Assert the relationships between constants, so a design invariant breaks the build rather than
  production.

The lints deny the panic family, so a panic is not a control-flow tool. An assertion on an
infallible path is the crash that remains, and a fallible function returns.

Validation is not assertion. Malformed input is an operating error and is refused; a value that was
promised and cannot be found is a programmer error and crashes.

## Shape

- **No recursion.** Every execution that should be bounded is bounded.
- **70 lines per function, hard.** Art is born of constraints, and few things are worth a scroll.
- **Push `if`s up, `for`s down.** One function owns the control flow; the functions it calls are
  branch-free and pure.
- **Smallest scope, latest declaration.** Compute a value where it is used and keep few variables in
  scope: the gap between a check and its use is where bugs live.
- **Say invariants positively.** State the condition under which the invariant holds, never the one
  under which it fails.
- **Split compound conditions** into nested branches, so every case is visible and none is implied.
- **Handle every error.** Most catastrophic failures are error paths that were never exercised.
- **Pass large arguments by reference** when they are not meant to be copied.
- **A function runs to completion.** An assertion holds for the whole body, so nothing suspends
  between a check and its use.
- **Explicit options at the call site**, never an inherited default: a default that changes is a bug
  arriving without a diff.
- **Fewer return dimensions.** Prefer `()` to `bool`, `bool` to a number, a number to a nullable.
- **Index, count and size are distinct.** Converting between them is stated, never implied.
- **Few abstractions, and excellent ones.** Every abstraction can leak, and none is free.
- **No aliases, no duplicate state.** A second copy is a second thing to keep in sync.
- **Batch, don't react.** Bounded work per poll, with I/O, bytes and CPU amortized across the batch.

## Naming

- Get the nouns and verbs right. A name is where understanding of the domain shows, or fails to.
- **No abbreviations.** `source`, not `src`; a long flag over a short one.
- **Units and qualifiers last, most significant word first**: `latency_ms_max`, `latency_ms_min`.
  Related names then group and line up.
- `snake_case` for functions and modules, `CamelCase` for types; names on the wire belong to the
  contract.
- Related names the same length, so calculations line up and symmetry is visible.
- Prefix a helper with the name of the function that calls it, so the call history shows.
- Don't overload a word, and don't let a name mean two things by context.
- **An error type is `<Thing>Error`**, never a bare `Error`: the name holds wherever it lands.
- Name a thing as it will be referred to — a noun survives being a heading or a column; a participle
  does not.
- Order matters on the first read: `main` first, then what matters most; in a type, fields, then
  constructors, then methods.

## Comments

A comment carries knowledge from outside the code it sits on: why this choice, what breaks
otherwise, the gotcha an API hides. Narration of the code below it goes stale and dies. One line
where one line does.

## Cost

Cost is a hard budget, not a target. The four resources are network, flash, RAM and CPU: sketch the
work against them before writing it, and optimize the slowest first, compensated by how often it is
used — a flash write taken often costs what a network round trip costs once. A shape that cannot fit
the budget is wrong, not a budget to raise.

No microbenchmarks. A bound is justified by the memory one batch holds and by the buffer that must
survive an outage, never by a synthetic timing taken somewhere other than the board.

## Dependencies

Few, and never idly. A dependency is supply chain, install time, and another toolchain to keep
strict. Foundational code carries the fewest, and nothing foundational is worth a convenience.

Prefer a tool already in the repository; adding one costs more than it looks.

## Formatting

The formatter and the linter decide, at the commit gate: 100 columns as a hard limit and never a
horizontal scrollbar, rustfmt's 4-space indents, no warnings.

Imports group std, then external, then local. The option that would enforce it is nightly and does
nothing on stable, so the grouping is written by hand and caught in review — a rule this document
holds, not one the gate does.

## Enforcement

The lints hold what a machine can check, and the commit gate runs them with warnings denied. What a
lint cannot judge — a name, an order, a bound's number, the shape of the code — this document holds,
and review catches.
