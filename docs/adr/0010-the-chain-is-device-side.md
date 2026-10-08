# 10. The chain is device-side

- Status: accepted

## Chosen

A deployment is a chain: the cloud, the device that uploads, the things it reads, and the
transducers behind those. Only the first hop crosses the seam. Whatever a device reads — an inverter
it does not control, a node on the local network, a transducer wired to its own pins — is declared
as one of its **sources**, and the cloud sees one device declaring sources exactly as it does today.

The manifest gains no level, `Device` keeps its meaning, and the chain is a shape the device half
arranges behind the seam.

## Why

The chain is real and will get longer: a board on the local network may reach the cloud through a
device rather than directly, and the same device may read transducers itself. Letting that reach the
contract would put topology into a document whose whole point is that the cloud stays dull — a new
level the cloud must store, index and version, to record something no query asks about.

Collapsing at the seam keeps re-parenting free. Moving a transducer from a node to the device that
reads it is a manifest change on one device, not a migration on both halves.

It also keeps the credential rule whole. One revocable token per device, holding its own and no
other's, survives a chain only because a node is not a device to the cloud; the alternative is a
device forwarding someone else's credential.

The alternative was a manifest that nests devices. It reads well for the solar chain, where the
inverter is a real device by any ordinary use of the word, and it is what the contract would need if
provenance ever had to be queryable. Recorded here because that is the condition, not because the
shape is wrong.

## Cost

"Device" in conversation and "Device" in the contract drift apart: the inverter is plainly a device
to anyone standing in front of it, and is a source to the manifest. The glossary carries that, and a
newcomer has to be told once.

Provenance inside a chain is convention, not structure — a source id or a key prefix — so nothing
stops two hops from colliding on a name.

## Reverses

A query that has to answer "which box measured this", or a deployment where the nodes are owned by
someone other than the operator of the device that reads them. Either makes the topology data rather
than arrangement, and the contract grows the level this defers.
