# Context

Domain vocabulary for the device half. Terms here are the ones the code, tests, issues and commits
use — don't drift to synonyms. The seam's words are the cloud's (`magellan-cloud/docs/CONTEXT.md`);
this glossary adds what a device is and does, and never a word the cloud would not recognize in a
manifest or a batch.

Magellan reads small sources, buffers the readings across outages, and uploads them to the cloud,
which stores whatever the device's manifest declares. The device is where the variety lives.

## The seam

Shared with the cloud, defined there; repeated here only to keep the device's reading of them close.
Where this and the cloud disagree, the contract document wins.

| Term           | Meaning                                                                                               |
| -------------- | ----------------------------------------------------------------------------------------------------- |
| **Device**     | A physical agent that uploads readings: an ESP32, a Raspberry Pi                                      |
| **Source**     | A named thing a device polls, identified per device                                                   |
| **Metric**     | A named, typed quantity of a source: a `key`, a `kind`; if measured, an `exponent` and maybe a `unit` |
| **Value**      | An integer a reading carries; the metric's `exponent` scales it: `value × 10^exponent`                |
| **Reading**    | One source poll: a timestamp plus that source's metric values                                         |
| **Manifest**   | A device's description of its zone, sources and metrics, versioned by hash                            |
| **Zone**       | The device's IANA time zone, the manifest's `tz`. Calendar days are cut in it; timestamps stay UTC    |
| **Batch**      | One upload: a boot id and `seq`, a manifest hash, ordered readings                                    |
| **Sequence**   | A counter, monotonic within one boot, canonical decimal                                               |
| **Boot id**    | Hex drawn once per boot and held in RAM; with **Sequence**, identifies a batch — never the dedup key  |
| **Reset**      | A counter's value decreasing, declared or not; `resets` names the cadence (`daily`)                   |
| **Heartbeat**  | The device's account of itself, apart from any batch — uptime, buffer depth, last heard, firmware     |
| **Last heard** | When a source was last read; the cloud's is the same, one hop up                                      |
| **Measured**   | When the device read the values — the reading's timestamp                                             |
| **Received**   | When the cloud committed the batch. Routinely later than **Measured**                                 |

**Caution — a batch carries many readings.** A batch is the unit of delivery and retry; a reading is
the unit of storage, query and dedup — on its own source and timestamp. Saying "batch" when you mean
one poll makes a replayed batch look like a lost one.

**Caution — a metric is a quantity a source reports**, not an observability metric. CPU time and
request latency are the journal, not data.

**Caution — a source is what the device talks to, not what does the sensing.** One source usually
aggregates many transducers: an inverter reports two PV strings, a meter three phases. The
transducer is named in the metric key — `pv1_voltage`, `pv2_voltage` — and is no level of its own.
Nothing between **Source** and **Metric** crosses the seam.

## The device

| Term            | Meaning                                                                             |
| --------------- | ----------------------------------------------------------------------------------- |
| **Buffer**      | The bounded queue of stamped batches awaiting a drain; where a reading is refused   |
| **Store**       | Where the buffer outlives a power cut: named blobs, checked by whoever reads them   |
| **Gap**         | A `seq` the device dropped, or never assigned, visible in the numbers it does send  |
| **Cadence**     | How often the runtime does a thing: a source poll, a heartbeat, a drain             |
| **Drain**       | Uploading pending batches oldest-first until the buffer is empty or the cloud stops |
| **Backoff**     | Waiting longer after each failed attempt: an interval doubling to a ceiling         |
| **Jitter**      | Spreading a wait inside its interval, so a fleet does not retry in step             |
| **Pace**        | The least time between two sends while a backlog drains                             |
| **Window**      | When a source is worth polling, if it has one; a sun window follows daylight        |
| **Limit**       | A bound the contract sets and the device holds a copy of                            |
| **Refusal**     | The device rejecting its own manifest, or a reading bound for a batch, on a limit   |
| **Implausible** | A value a source reported that its profile's bounds say cannot be true              |

**Caution — the buffer is bounded on purpose.** When it fills, the oldest batch is dropped and `seq`
leaves a visible gap. A gap is a health signal, never something the device hides.

**Caution — the fleet is the unit a backoff is judged against, not the device.** One outage ends for
every device at once, so a backoff each follows identically makes the recovery a spike. Jitter and
pace are what a device owes the cloud it shares.

**Caution — `seq` alone does not identify a batch.** It restarts at every boot, so only the pair
with **Boot id** is unique. Reading gaps from `seq` by itself mistakes a reboot for a loss.

**Caution — a refusal is the device's, a rejection is the cloud's.** A refusal never reaches the
wire; a rejection is a `4xx` that already cost a round trip. The device refuses so the cloud has
nothing left to reject.

**Caution — a refused reading leaves no gap.** A refusal drops the reading, not the batch; its
sweep's other readings still ship under the next `seq`. The journal is the only account of it.

**Caution — implausible is neither.** A refusal and a rejection judge the device's own work against
the contract; implausible judges what a source said against what that source can physically report.
An implausible value is dropped at the driver and never becomes a metric value, so the contract
never sees it.

## Layers

Device-side mechanism. The cloud has no vocabulary for any of this; if one leaks into a manifest or
batch, the boundary has leaked.

| Term               | Meaning                                                                  |
| ------------------ | ------------------------------------------------------------------------ |
| **Device runtime** | Layer 5 — config, clock, scheduling, buffer, upload, health              |
| **Source driver**  | Layer 6 — reads one kind of source; the first is Sofar-over-Modbus       |
| **Platform**       | Layer 7 — the OS seam: Linux on 32-bit ARM                               |
| **Hardware**       | Layer 8 — the board, its power and its buses                             |
| **Journal**        | Diagnostics: the program's log lines. Records are machine data, not this |

**Caution — "source driver" is not the cloud's "source".** A source is what the manifest declares; a
driver is the code that reads it. One driver can serve many sources on one device.

## Vocabulary limits

Terms outside these bounds have no meaning in this repository; if one is needed, the boundary has
leaked.

- **No cloud-internal words.** `registry`, `rollup`, `bucket`, `archive`, `ingest worker` name cloud
  mechanism; the device knows the contract and nothing behind it.
- **No device commands.** The contract runs device to cloud. Control, configuration and OTA have no
  vocabulary here.
- **No time-series vocabulary.** There is no retention policy or downsample here — the buffer is a
  bounded queue on flash and nothing else.
- **No tenancy.** One deployment, one operator, one set of devices.
