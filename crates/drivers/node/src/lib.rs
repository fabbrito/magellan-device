//! A node: a board on the LAN, read over Modbus TCP as one of this device's sources.
//!
//! The node is hardware this project does not control, as the inverter is: its firmware publishes
//! a register map and this driver mirrors it. Where the two disagree, the node's document wins and
//! this crate is the bug.
//!
//! Input registers (FC4), from 0, big-endian, a u32 high word first:
//!
//! | Register | Value                         | Metric              |
//! | -------- | ----------------------------- | ------------------- |
//! | 0–1      | uptime, s, u32                | `uptime` counter    |
//! | 2        | Wi-Fi signal, dBm, i16        | `rssi` gauge        |
//! | 3        | a random number, u16          | `random` gauge      |
//! | 4–6      | firmware major, minor, patch  | `firmware` state    |
//!
//! A node advertises `_modbus._tcp` over mDNS with a TXT `id`; that is how it is found.

mod discover;
mod settings;
mod source;
mod wire;

pub use crate::settings::{SettingsError, from_settings};
pub use crate::source::Node;
