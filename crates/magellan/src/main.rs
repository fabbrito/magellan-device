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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use driver::Source;
use runtime::window::Sun;
use runtime::{Batches, Cadence, Config, Polling, Queue, SourceConfig, drain_forever, manifest_of};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// What built this binary, as `build.rs` spells it.
pub const VERSION: &str = env!("MAGELLAN_VERSION");

/// Longest a dial may take before the address counts as dark.
const CONNECT_LIMIT: Duration = Duration::from_secs(10);
/// Longest one range read may take. Must clear the slowest refusal, not the typical one.
const READ_LIMIT: Duration = Duration::from_secs(20);
/// Gap between reads inside a sweep.
const READ_GAP: Duration = Duration::from_secs(15);
/// How long a logger gets to answer the discovery hello. Only a dark one takes it all.
const DISCOVERY_LIMIT: Duration = Duration::from_secs(3);
/// Longest one request to the cloud may take. A source and the cloud are different networks, so
/// this moves for its own reasons and is not the read limit under another name.
const UPLOAD_LIMIT: Duration = Duration::from_secs(20);

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
    // Secrets come from the environment: a dev tree keeps them in `.env.local`, a unit in its
    // EnvironmentFile. Missing is the normal case — production has no file. dotenvy never
    // overwrites what is set, so a unit's values are never displaced. Before the subscriber, so a
    // `RUST_LOG` in the file still reaches it.
    dotenvy::from_filename(".env.local").ok();
    init_tracing();
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

/// Refuse a `[[source]]` setting the named driver will never read.
///
/// The settings table is open by construction — each driver names its own keys, and the runtime
/// holds them without reading them — so a misspelling has nothing to fail against until here.
fn only_settings_the_driver_reads(source: &SourceConfig, known: &[&str]) -> Result<()> {
    for key in source.settings.keys() {
        if !known.contains(&key.as_str()) {
            bail!(
                "source {:?}: {:?} is not a setting the {:?} driver reads ({})",
                source.id,
                key,
                source.driver,
                known.join(", ")
            );
        }
    }
    Ok(())
}

/// The drivers this binary carries, constructed from what the configuration says.
///
/// `address` is where the source is reached. `check` has no address to give and never reads a
/// register, so it passes none — a driver built this way can declare itself and nothing else.
fn build_source(source: &SourceConfig, address: Option<String>) -> Result<Box<dyn Source>> {
    match source.driver.as_str() {
        "sofar" => {
            only_settings_the_driver_reads(source, sofar::SETTINGS)?;
            let profile = source
                .settings
                .get("profile")
                .and_then(toml::Value::as_str)
                .with_context(|| format!("source {:?} has no profile", source.id))?;
            let slave = source
                .settings
                .get("slave")
                .and_then(toml::Value::as_integer)
                .unwrap_or(1);
            let timing = sofar::Timing {
                connect: CONNECT_LIMIT,
                read: READ_LIMIT,
                gap: READ_GAP,
            };
            Ok(Box::new(sofar::Inverter::new(
                source.id.clone(),
                sofar::builtin(profile).with_context(|| format!("profile {profile:?}"))?,
                address.unwrap_or_else(|| "unresolved:0".to_owned()),
                u8::try_from(slave).context("slave id is not a Modbus unit id")?,
                timing,
            )))
        }
        other => bail!(
            "source {:?} names no driver this binary carries: {other:?}",
            source.id
        ),
    }
}

/// Where a source is, from the configuration or by asking the network for it.
async fn address_of(source: &SourceConfig) -> Result<String> {
    let port = source
        .settings
        .get("port")
        .and_then(toml::Value::as_integer)
        .unwrap_or(8899);
    if let Some(host) = &source.host {
        return Ok(format!("{host}:{port}"));
    }
    // No address configured: the logger is found by broadcasting for its serial.
    let found = sofar::discover::find(
        source.serial,
        &sofar::discover::broadcast_targets(),
        DISCOVERY_LIMIT,
    )
    .await
    .context("broadcasting for the logger")?
    .with_context(|| format!("no logger answered for source {:?}", source.id))?;
    info!(source = source.id, %found, "found by discovery");
    Ok(format!("{found}:{port}"))
}

/// Read the configuration and say what this device would declare, without touching anything.
fn check(path: &Path) -> Result<()> {
    let config = Config::load(path).context(ConfigFault)?;
    let sources = config
        .sources
        .iter()
        .map(|source| build_source(source, None).context(ConfigFault))
        .collect::<Result<Vec<_>>>()?;
    let manifest = manifest_of(&sources);
    manifest
        .validate()
        .map_err(|refusal| anyhow::anyhow!("the manifest breaks the contract: {refusal}"))?;
    let metrics: usize = manifest.sources.iter().map(|s| s.metrics.len()).sum();
    println!("device {}", config.device_id);
    println!("endpoint {}", config.endpoint);
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
    let mut sources = Vec::with_capacity(config.sources.len());
    for source in &config.sources {
        let address = address_of(source).await?;
        sources.push(build_source(source, Some(address)).context(ConfigFault)?);
    }

    let manifest = manifest_of(&sources);
    manifest
        .validate()
        .map_err(|refusal| anyhow::anyhow!("the manifest breaks the contract: {refusal}"))?;

    let cloud: Arc<dyn runtime::Cloud> = Arc::new(runtime::Http::new(
        config.endpoint.clone(),
        config.device_id.clone(),
        config.token.clone(),
        UPLOAD_LIMIT,
    )?);
    // The name a batch carries, computed rather than asked for: the cloud may be down at boot, and
    // the device knows its own manifest (both sides hash the bytes they handle).
    let hash = runtime::manifest_hash(&manifest)
        .map_err(|why| anyhow::anyhow!("hashing the manifest: {why:?}"))?;

    let buffer = Arc::new(Mutex::new(Queue::new(config.buffer)));
    let cadence = Cadence {
        sweep: config.sweep_period,
        backoff_min: Duration::from_secs(5),
        backoff_max: Duration::from_mins(5),
        drain_pace: Duration::from_secs(1),
        recheck: Duration::from_mins(15),
    };
    let polling = Polling {
        sources,
        buffer: Arc::clone(&buffer),
        batches: Batches::new(
            hash.clone(),
            platform::boot_id().context("drawing a boot id")?,
        ),
        clock: Arc::new(platform::SystemClock::new()),
        daylight: Sun {
            site: config.site,
            before_sunrise: config.margins.before_sunrise,
            after_sunset: config.margins.after_sunset,
        },
        cadence,
        firmware: VERSION.to_owned(),
    };

    let stop = CancellationToken::new();
    let signal = tokio::spawn(stop_on_signal(stop.clone()));
    // Poll before declaring: a cloud that is down at boot is the same as one that goes down
    // later, and the readings must not wait on it. Only a refusal asking again cannot fix ends
    // the run; an outage leaves the batches in the buffer and the declare retrying.
    let poll = tokio::spawn(polling.run(stop.clone()));
    let declared = tokio::select! {
        declared = runtime::declare_forever(cloud.as_ref(), &manifest, cadence) => declared,
        () = stop.cancelled() => {
            let _ = tokio::join!(signal, poll);
            return Ok(());
        }
    };
    declared.map_err(|why| anyhow::anyhow!("declaring the manifest: {why:?}"))?;
    info!(hash, "manifest accepted");

    let drain = tokio::spawn(drain_forever(buffer, cloud, cadence, stop.clone()));
    stop.cancelled().await;
    let _ = tokio::join!(signal, poll, drain);
    Ok(())
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
