//! Magellan — the device half. Layers 5–8: runtime, source drivers, platform, hardware.
//!
//! The cloud stores whatever the manifest declares; this device declares its sources once, then
//! polls, buffers and uploads. North star: `docs/DESIGN.md`.
//!
//! This is the only place that names both a driver and the runtime. The runtime is written
//! against seams; which driver satisfies one is chosen here, where the program starts (ADR 1).

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
use tracing::{info, warn};

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

/// The drivers this binary carries, constructed from what the configuration says.
///
/// `address` is where the source is reached. `check` has no address to give and never reads a
/// register, so it passes none — a driver built this way can declare itself and nothing else.
fn build_source(source: &SourceConfig, address: Option<String>) -> Result<Box<dyn Source>> {
    match source.driver.as_str() {
        "sofar" => {
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
    info!(source = source.id, "found by discovery");
    Ok(format!("{found}:{port}"))
}

/// Read the configuration and say what this device would declare, without touching anything.
fn check(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let sources = config
        .sources
        .iter()
        .map(|source| build_source(source, None))
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
    let config = Config::load(path)?;
    let mut sources = Vec::with_capacity(config.sources.len());
    for source in &config.sources {
        let address = address_of(source).await?;
        sources.push(build_source(source, Some(address))?);
    }

    let manifest = manifest_of(&sources);
    manifest
        .validate()
        .map_err(|refusal| anyhow::anyhow!("the manifest breaks the contract: {refusal}"))?;

    let cloud: Arc<dyn runtime::Cloud> = Arc::new(runtime::Http::new(
        config.endpoint.clone(),
        config.device_id.clone(),
        config.token.clone(),
        READ_LIMIT,
    )?);
    let hash = cloud
        .declare(&manifest)
        .await
        .map_err(|why| anyhow::anyhow!("declaring the manifest: {why:?}"))?;
    info!(hash, "manifest accepted");

    let buffer = Arc::new(Mutex::new(Queue::new(config.buffer)));
    let cadence = Cadence {
        sweep: config.sweep_period,
        backoff_min: Duration::from_secs(5),
        backoff_max: Duration::from_mins(5),
        recheck: Duration::from_mins(15),
    };
    let polling = Polling {
        sources,
        buffer: Arc::clone(&buffer),
        batches: Batches::new(hash, platform::boot_id().context("drawing a boot id")?),
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
    let drain = tokio::spawn(drain_forever(buffer, cloud, cadence, stop.clone()));
    let poll = tokio::spawn(polling.run(stop.clone()));
    wait_for_a_signal().await;
    info!("stopping");
    stop.cancel();
    let _ = tokio::join!(drain, poll);
    Ok(())
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

#[tokio::main]
async fn main() -> ExitCode {
    // Secrets come from the environment: a dev tree keeps them in `.env.local`, a unit in its
    // EnvironmentFile. Missing is the normal case — production has no file. dotenvy never
    // overwrites what is set, so a unit's values are never displaced.
    dotenvy::from_filename(".env.local").ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let outcome = match Cli::parse().command {
        Command::Check { config } => check(&config),
        Command::Run { config } => run(&config).await,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            // The chain, not just the head: "is not set" alone does not say which variable.
            eprintln!("magellan: {why:#}");
            ExitCode::FAILURE
        }
    }
}
