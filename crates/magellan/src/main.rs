//! Magellan — the device half. Layers 5–8: runtime, source drivers, platform, hardware.
//!
//! The cloud stores whatever the manifest declares; this device declares its sources once, then
//! polls, buffers and uploads. North star: `docs/DESIGN.md`.
//!
//! This is the only place that names both a driver and the runtime. The runtime is written
//! against seams; which driver satisfies one is chosen here, where the program starts (ADR 1).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use driver::Source;
use runtime::{Config, Http, SourceConfig, Wiring, manifest_of};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// What built this binary, as `build.rs` spells it.
pub const VERSION: &str = env!("MAGELLAN_VERSION");
// Every heartbeat carries it, and one past the contract is never sent: too long refuses the build
// rather than silencing the device.
const _: () = assert!(VERSION.len() <= contract::limits::FIRMWARE_VERSION_LENGTH_MAX);

/// `EX_CONFIG` from sysexits. A configuration fault is not an outage: asking again will never fix
/// it, so a unit carrying `RestartPreventExitStatus=78` stops instead of restart-looping.
const EX_CONFIG: u8 = 78;

/// Marks a failure the configuration caused, so the exit status can tell it from an outage.
#[derive(Debug)]
struct ConfigFault;

impl std::fmt::Display for ConfigFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the configuration is not usable")
    }
}

impl std::error::Error for ConfigFault {}

#[tokio::main]
async fn main() -> ExitCode {
    // The token comes from the environment: a dev tree keeps it in `.env.local`, a unit in its
    // EnvironmentFile. Missing is the normal case — production has no file. dotenvy never
    // overwrites what is set, so a unit's values are never displaced. Before the subscriber, so a
    // `RUST_LOG` in the file still reaches it.
    dotenvy::from_filename(".env.local").ok();
    init_tracing();
    journal_panics();
    let outcome = match Cli::parse().command {
        Command::Check { config } => check(&config),
        Command::Run { config } => run(&config).await,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            // The chain, not just the head: "is not set" alone does not say which variable.
            error!("{why:#}");
            if why.downcast_ref::<ConfigFault>().is_some() {
                ExitCode::from(EX_CONFIG)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "magellan",
    version = VERSION,
    about = "Magellan device — collect, buffer, upload readings"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read the configuration and the drivers, and say what would be declared. Touches nothing.
    ///
    /// Temporary: a scaffold to prove the config until the capture path stands alone. It prints a
    /// report rather than journalling, and goes away when it has served that.
    Check {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Poll, buffer and upload until stopped.
    Run {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
}

/// The drivers this binary carries, constructed from what the configuration says. Touches
/// nothing: a driver finds its source when a sweep needs it.
fn build_source(source: &SourceConfig) -> Result<Box<dyn Source>> {
    let whose = || format!("source {:?}", source.id);
    match source.driver.as_str() {
        "sofar" => Ok(Box::new(
            sofar::from_settings(&source.id, &source.settings).with_context(whose)?,
        )),
        "node" => Ok(Box::new(
            node::from_settings(&source.id, &source.settings).with_context(whose)?,
        )),
        other => bail!(
            "source {:?} names no driver this binary carries: {other:?}",
            source.id
        ),
    }
}

/// Read the configuration and say what this device would declare, without touching anything.
fn check(path: &Path) -> Result<()> {
    let config = Config::load(path).context(ConfigFault)?;
    let sources = config
        .sources
        .iter()
        .map(|source| build_source(source).context(ConfigFault))
        .collect::<Result<Vec<_>>>()?;
    let manifest = manifest_of(&config.zone, &sources);
    let encoded = manifest
        .encode()
        .map_err(|refusal| anyhow::anyhow!("the manifest breaks the contract: {refusal}"))
        .context(ConfigFault)?;
    let metrics: usize = manifest.sources.iter().map(|s| s.metrics.len()).sum();
    println!("device {}", config.device_id);
    println!("endpoint {}", config.endpoint);
    println!("zone {}", manifest.tz);
    println!("manifest {}", encoded.hash());
    println!(
        "{} source(s), {metrics} metric(s), buffer {} batches",
        manifest.sources.len(),
        config.buffer
    );
    for source in &manifest.sources {
        println!("  {} — {} metric(s)", source.id, source.metrics.len());
    }
    Ok(())
}

async fn run(path: &Path) -> Result<()> {
    info!("magellan {VERSION}");
    let config = Config::load(path).context(ConfigFault)?;
    let sources = config
        .sources
        .iter()
        .map(|source| build_source(source).context(ConfigFault))
        .collect::<Result<Vec<_>>>()?;
    let cloud = Arc::new(Http::new(
        config.endpoint.clone(),
        config.device_id.clone(),
        config.token.clone(),
        config.request_timeout,
    )?);
    let wiring = Wiring {
        sources,
        cloud,
        clock: Arc::new(platform::SystemClock::new()),
        store: Arc::new(
            platform::Dir::open(config.buffer_dir.clone())
                .with_context(|| format!("opening {}", config.buffer_dir.display()))
                .context(ConfigFault)?,
        ),
        boot_id: platform::boot_id().context("drawing a boot id")?,
        firmware: VERSION.to_owned(),
    };

    let stop = CancellationToken::new();
    let signal = tokio::spawn(stop_on_signal(stop.clone()));
    let ran = runtime::run(config, wiring, stop).await;
    // A run ended by a refusal has no signal left to wait for.
    signal.abort();
    // Every way a run ends early is one asking again cannot fix.
    ran.context(ConfigFault)
}

/// Until the service manager asks the device to stop: say so, then cancel everything.
async fn stop_on_signal(stop: CancellationToken) {
    wait_for_a_signal().await;
    info!("stopping");
    stop.cancel();
}

/// Until the service manager asks the device to stop.
async fn wait_for_a_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(why) => {
            warn!(%why, "cannot watch for SIGTERM; only ctrl-c will stop this");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// A panic through the journal at `error`, rather than as unlevelled stderr. The release profile
/// aborts right after, so this line is the device's last and the one someone reads.
fn journal_panics() {
    std::panic::set_hook(Box::new(|panic| {
        let at = panic
            .location()
            .map_or_else(String::new, ToString::to_string);
        let why = panic.payload_as_str().unwrap_or("no message");
        error!(at, "panic: {why}");
    }));
}

/// The journal, at the levels ADR 9 sets: `error` ends the run, `warn` lost something, `info` is a
/// state change or the pulse, `debug` is inside one unit of work.
///
/// Under a unit — systemd sets `JOURNAL_STREAM` — entries go to the journal in its own protocol, a
/// level arriving as a priority. Otherwise they go to stderr, which is where diagnostics belong;
/// stdout is left to a subcommand's output, and ANSI is on only when a person is watching.
/// `from_default_env` would default to ERROR, and nothing in the tree logs that loud, so an
/// unset or unreadable `RUST_LOG` falls back to `info` rather than to silence.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    let journal = std::env::var_os("JOURNAL_STREAM").and_then(|_| tracing_journald::layer().ok());
    match journal {
        Some(layer) => registry.with(layer).init(),
        None => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(std::io::stderr().is_terminal())
                    .with_writer(std::io::stderr),
            )
            .init(),
    }
}
