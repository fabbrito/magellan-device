//! Layer 7 — the platform seam. The runtime never names an OS; a platform supplies the clock,
//! the boot id and the store, the OS-specific bits nothing else can. One platform is built — Linux
//! on 32-bit ARM — and the seam stays anyway, because the runtime is written against a shape rather
//! than an OS.

mod boot;
mod clock;
#[cfg(feature = "fake")]
pub mod fake;
mod store;

pub use crate::boot::boot_id;
pub use crate::clock::{Clock, SystemClock};
pub use crate::store::{Dir, Store};
