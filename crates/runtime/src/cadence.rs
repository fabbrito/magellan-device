//! Every interval the runtime keeps, in one place: the poll reads some and the drain the rest,
//! and neither owns what the other reads.

use std::time::Duration;

/// How long the device waits between things.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// One sweep of every source per slot.
    pub sweep: Duration,
    /// First backoff interval after the cloud declines; doubles up to `backoff_ceiling`. A wait
    /// is spread inside its interval, so this bounds a wait and never schedules one.
    pub backoff_first: Duration,
    /// Longest the device waits before trying the cloud again.
    pub backoff_ceiling: Duration,
    /// Least time between two sends while the buffer drains: a backlog is already minutes old,
    /// and a cloud that has just come back is owed no burst.
    pub drain_pace: Duration,
    /// Longest a closed window is slept on before looking again. The board has no clock of its
    /// own until the network steps it, so a sleep computed until sunrise at boot can land hours
    /// out; looking again settles it.
    pub recheck: Duration,
}
