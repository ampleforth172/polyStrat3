//! Polymarket 15m Up/Down trader — Rust port.
//! Single-threaded event-loop design; see polymm/docs/rust_implementation_plan.md.

use std::path::PathBuf;

use clap::Parser;

use poly_strat3::config::{self, Config, StrategyKind};
use poly_strat3::engine;
#[cfg(feature = "live")]
use poly_strat3::exec;

#[derive(Parser, Debug)]
#[command(name = "polyStrat3", about = "Polymarket 15m Up/Down maker/taker bot")]
struct Cli {
    /// Path to the strategy TOML config.
    #[arg(long, default_value = "config/taker.toml")]
    config: PathBuf,

    /// Path to the credentials TOML (live mode only).
    #[arg(long, default_value = "config/credentials.toml")]
    credentials: PathBuf,

    /// Override [general].symbol (BTC or ETH).
    #[arg(long)]
    symbol: Option<String>,

    /// Override [general].strategy (taker | maker).
    #[arg(long)]
    strategy: Option<String>,

    /// Force dry-run regardless of config.
    #[arg(long)]
    dry_run: bool,

    /// Override [general].log_level (trace|debug|info|warn|error, or an
    /// EnvFilter directive like "info,poly_strat3::alpha=debug").
    #[arg(long)]
    log_level: Option<String>,

    /// Pin the engine thread to this CPU core (overrides [general].pin_core).
    #[arg(long)]
    pin_core: Option<usize>,

    /// Replay a recorded event journal through the engine (forces dry-run;
    /// fills are re-simulated). The same decision code that trades runs
    /// against the recorded stream — a deterministic backtest.
    #[arg(long)]
    replay: Option<PathBuf>,
}

/// Pin the current (engine) thread to one core. Hard pin via
/// sched_setaffinity on Linux; on macOS this is only a scheduler hint and
/// is ignored on Apple Silicon. Must run before the tokio runtime is built
/// so the whole single-threaded engine inherits the pin.
fn pin_to_core(idx: usize) {
    let Some(cores) = core_affinity::get_core_ids() else {
        tracing::warn!("core pinning unsupported on this platform — continuing unpinned");
        return;
    };
    let Some(core) = cores.iter().find(|c| c.id == idx) else {
        tracing::warn!(
            "pin_core={idx} not available (cores: 0-{}) — continuing unpinned",
            cores.iter().map(|c| c.id).max().unwrap_or(0)
        );
        return;
    };
    if core_affinity::set_for_current(*core) {
        #[cfg(target_os = "linux")]
        tracing::info!("engine thread pinned to core {idx}");
        #[cfg(not(target_os = "linux"))]
        tracing::info!("engine thread pinned to core {idx} (best-effort hint on this OS)");
    } else {
        tracing::warn!("failed to pin to core {idx} — continuing unpinned");
    }
}

/// Initialize logging with a NON-BLOCKING stdout writer: log lines go
/// through a channel to a dedicated writer thread, so a backpressured
/// stdout (paused terminal, slow pipe) can never stall the engine thread.
/// When the channel saturates, lines are dropped rather than blocking.
/// The returned guard must stay alive for the process lifetime — dropping
/// it flushes and stops the writer.
fn init_tracing(
    cfg_level: &str,
    cli_level: Option<&str>,
) -> tracing_appender::non_blocking::WorkerGuard {
    // Precedence: RUST_LOG > --log-level > TOML log_level > "info".
    let directive = std::env::var("RUST_LOG")
        .ok()
        .or_else(|| cli_level.map(String::from))
        .unwrap_or_else(|| cfg_level.to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(writer)
        .init();
    guard
}

fn main() -> Result<(), String> {
    // Pick the rustls provider explicitly — with the SDK in the dependency
    // tree both ring and aws-lc-rs are present and auto-detection fails.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    let mut cfg = if cli.config.exists() {
        Config::load(&cli.config)?
    } else {
        eprintln!(
            "config {} not found — using built-in defaults",
            cli.config.display()
        );
        Config::default()
    };

    // CLI overrides.
    if let Some(sym) = &cli.symbol {
        cfg.general.symbol = sym.to_uppercase();
    }
    if let Some(st) = &cli.strategy {
        cfg.general.strategy = match st.to_lowercase().as_str() {
            "taker" => StrategyKind::Taker,
            "maker" => StrategyKind::Maker,
            other => return Err(format!("unknown strategy '{other}'")),
        };
    }
    if cli.dry_run {
        cfg.general.dry_run = true;
    }
    if let Some(lvl) = &cli.log_level {
        config::parse_level(lvl)?;
        cfg.general.log_level = lvl.clone();
    }
    cfg.validate()?;

    if let Some(idx) = cli.pin_core {
        cfg.general.pin_core = Some(idx);
    }

    // Keep the guard alive until exit so buffered log lines are flushed.
    let _log_guard = init_tracing(&cfg.general.log_level, cli.log_level.as_deref());

    // Pin before the runtime exists — every future then runs on this core.
    if let Some(idx) = cfg.general.pin_core {
        pin_to_core(idx);
    }

    tracing::info!(
        "polyStrat3 starting: symbol={} strategy={:?} mode={} host={}",
        cfg.general.symbol,
        cfg.general.strategy,
        if cfg.general.dry_run { "DRY-RUN" } else { "LIVE" },
        cfg.general.clob_host,
    );

    // Single-threaded runtime + LocalSet: the whole bot runs on one thread.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async move {
        if let Some(path) = &cli.replay {
            let mut replay_cfg = cfg.clone();
            replay_cfg.general.dry_run = true;
            replay_cfg.journal.enabled = false;
            let engine = engine::Engine::new(replay_cfg)?;
            return engine.replay(path);
        }
        #[cfg_attr(not(feature = "live"), allow(unused_mut))]
        let mut engine = engine::Engine::new(cfg.clone())?;

        if !cfg.general.dry_run {
            #[cfg(feature = "live")]
            {
                let creds = config::Credentials::load(&cli.credentials)?;
                let live =
                    exec::live::LiveExec::connect(&cfg.general.clob_host, &creds).await?;
                engine.set_live_exec(live);
                tracing::info!(
                    "LIVE mode: fills confirmed via the SDK user channel; order placement \
                     is gated until the channel connects"
                );
            }
            #[cfg(not(feature = "live"))]
            {
                return Err(
                    "built without the 'live' feature — rebuild with default features for live mode"
                        .to_string(),
                );
            }
        }

        engine.run().await
    })
}
