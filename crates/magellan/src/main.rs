//! Magellan — the device half. Layers 5–8: runtime, source drivers, platform, hardware.
//!
//! The cloud stores whatever the manifest declares; this device declares its sources once, then
//! polls, buffers and uploads. North star: `docs/DESIGN.md`.

use std::process::ExitCode;

use clap::Parser;

/// What built this binary, as `build.rs` spells it.
pub const VERSION: &str = env!("MAGELLAN_VERSION");

/// Command-line entry point. Subcommands land with the runtime.
#[derive(Debug, Parser)]
#[command(
    name = "magellan",
    version = VERSION,
    about = "Magellan device — collect, buffer, upload readings"
)]
struct Cli;

fn main() -> ExitCode {
    let _ = Cli::parse();
    ExitCode::SUCCESS
}
