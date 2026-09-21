# Reference material

External documents the driver decodes against. Kept unedited but for structure; provenance and trust
live here.

| Document                                   | Subject                                     |
| ------------------------------------------ | ------------------------------------------- |
| [`sofar-g3-modbus.md`](sofar-g3-modbus.md) | Sofar G3 register map and fault definitions |

## `sofar-g3-modbus.md`

From a `Sofar_G3_Modbus.xlsx` spreadsheet: 1827 register rows, 327 fault flags, protocol basics, the
spreadsheet's revision table. `profiles/sofar-g3.toml` was generated from it once and is the source
of truth now; this is where its facts came from.

**Provenance is informal.** It circulates in the G3 community and ships in several third-party
logger readers. No manufacturer attestation; this conversion is not an authorised reproduction. Its
revision table (§4), 2020-05-23 to 2021-11-03 over 118 entries, fits a maintained protocol document
but proves no origin. Best available map, not an authority.

**Read-only, whatever the document offers.** Large parts of it describe writable registers — remote
control, parameter setting, safety limits. A source is read, never written, so those sections are
here as map, never as instruction.

Trust comes from decoding captures off this installation. The golden vectors in `tests/fixtures/`,
decoded by `tests/golden.rs`, re-check on every run that:

- System-date registers match the capture timestamp.
- Generation-minutes does not outrun elapsed daylight at the capture hour, and total service time is
  at least total generation time — both break if a U32's words swap.
- V × I = P on both PV strings, and on the grid output: every exponent in that path holds.
- Grid frequency reads 60.02 Hz, and energy today never exceeds energy total.
- `Modbus_Protocol_Version` at `0x0044` reads 1.23, inside the document's stated 1.06-99.99.
- `AddressMask` — a U64 heading a partition, bit _i_ marking address base+_i_ valid — predicts the
  captures, including which registers read zero and which are absent entirely.
- No bound in the profile rejects a reply that really arrived.

The map was first trusted against a wider capture set, taken days apart and in the logger's own
framing as well as Modbus TCP. Those captures stayed behind with that framing; what ships here is
the Modbus TCP set, and it holds the same relations.

Where the community map disagrees on a scale factor, our capture backs this document.

### Caveats

- Covers the whole G3 family; battery, BMS and multi-string partitions this 7.5KTLM-G3 lacks read
  empty or reserved.
- The inverter reports protocol 1.23, newer than the document, so later registers may be missing:
  the power partition marks `0x069C`-`0x069F` valid where the document says reserved. Later public
  revisions drop `AddressMask` altogether.
- `0x041B` reads 120 °C: an unpopulated sensor, not a heatsink. Its mask bit is clear, as expected.
- The document states scale factors as decimal multipliers. The profile carries them as powers of
  ten, so a register the document gives `0.1` has `exponent = -1` and the raw integer is what
  travels.
- Register facts are the spreadsheet's, structure is ours. Fix structure, never facts.
