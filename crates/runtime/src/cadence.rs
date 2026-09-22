//! Every interval the runtime keeps, in one place: the poll reads some and the drain the rest,
//! and neither owns what the other reads.

use std::time::Duration;

/// How long the device waits between things.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// One sweep of every source per slot.
    pub sweep: Duration,
    /// First wait after the cloud declines a batch; doubles up to `backoff_max`. Spread before
    /// it is taken, so the rung is a ceiling and never a schedule.
    pub backoff_min: Duration,
    /// Longest the device waits before trying the cloud again.
    pub backoff_max: Duration,
    /// Least time between two sends while the buffer drains: a backlog is already minutes old,
    /// and a cloud that has just come back is owed no burst.
    pub drain_pace: Duration,
    /// Longest a closed window is slept on before looking again. The board has no clock of its
    /// own until the network steps it, so a sleep computed until sunrise at boot can land hours
    /// out; looking again settles it.
    pub recheck: Duration,
}
