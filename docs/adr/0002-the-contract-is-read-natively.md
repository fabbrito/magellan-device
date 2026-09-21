# 2. The contract is read natively, not generated

- Status: accepted

## Chosen

The device writes and maintains its own reading of the contract's schemas, in its own language,
rather than generating types from the published document. The document stays the authority: where
the two disagree, the device is wrong. Until the document is in hand, the reading is written from
the cloud's authoring source and checked against the document when it lands.

## Why

Generated types make both halves one implementation of the shape. A shape that is wrong is then
wrong identically on both sides, satisfies both, and the disagreement that would have caught it
never happens. A native reading keeps each half independent of the other's toolchain, which is the
reason there are two repositories meeting at one seam rather than one repository with two
deployments.

The document also cannot carry everything it must. Uniqueness by property has no schema keyword, so
unique source ids and unique metric keys survive into a generated type as prose at best. A reading
written by hand can enforce them.

Independence is safe because of the hash. Each side hashes the exact bytes it sends and receives,
and the accepted hash comes back, so a device whose reading differs from what it actually puts on
the wire learns it at the first exchange rather than through readings that can never commit.

## Cost

Transcription. Every change to the shape is hand-carried, a mistake is silent until the wire
disagrees, and nothing in the build notices a field the other side added.

Two definitions of one shape also means two places to look when they disagree, and the answer is
always that the document wins — which is cheap to say and slow to check.

## Reverses

Generate from the published document once it lands, keeping the native reading as a check against
it, or dropping it. Either way the change is confined to one crate: nothing outside it names the
wire shape.
