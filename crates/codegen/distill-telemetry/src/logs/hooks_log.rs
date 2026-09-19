// Modified for Distill by Samuel Fajreldines, 2026.
//! A dedicated tracing target for hooks and plugins subsystems with an optional
//! file logger that writes to `~/.grok/logs/hooks.log`.
//!
//! ## When to use
//!
//! Use regular `tracing::info!` / `tracing::debug!` / `tracing::warn!` with targets `distill_hooks` or `distill_agent::plugins`.
//! Log at discovery, dispatch, execution, and error points.
//!
//! ## Enabling
//!
//! ```bash
//! GROK_HOOKS_LOG=1 grok              # enable, write to ~/.grok/logs/hooks.log
//! GROK_HOOKS_LOG=/tmp/h.log grok     # write to custom path
//! GROK_HOOKS_LOG=0 grok              # explicitly disable
//! tail -f ~/.grok/logs/hooks.log     # watch in another terminal
//! ```

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use tracing::Subscriber;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

use distill_config::distill_home;

const ENV_HOOKS_LOG: &str = "GROK_HOOKS_LOG";

static LOG_GUARD: std::sync::OnceLock<Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>> =
    std::sync::OnceLock::new();

#[derive(Clone)]
struct UptimeTimer {
    epoch: Instant,
}

impl UptimeTimer {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl FormatTime for UptimeTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let elapsed = self.epoch.elapsed();
        write!(w, "+{}.{:03}s", elapsed.as_secs(), elapsed.subsec_millis())
    }
}

/// Writes to `~/.grok/logs/hooks.log` (or custom path via `GROK_HOOKS_LOG`).
/// Filters to hooks (`distill_hooks`) and plugins (`distill_agent::plugins`) targets.
/// Set `GROK_HOOKS_LOG=0` to disable, `GROK_HOOKS_LOG=/path` to redirect.
pub fn layer<S>() -> Option<impl Layer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let path = resolve_log_path()?;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("[hooks-log] Failed to open {:?}: {}", path, e);
            return None;
        }
    };

    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let guard_slot = LOG_GUARD.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = guard_slot.lock() {
        *slot = Some(guard);
    }

    let filter = tracing_subscriber::filter::EnvFilter::new(
        "distill_hooks=debug,distill_agent::plugins=debug",
    );
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_thread_ids(true)
        .with_timer(UptimeTimer::new())
        .with_writer(BoxMakeWriter::new(non_blocking))
        .with_filter(filter);

    tracing::info!(
        "[hooks-log] Hooks/plugins logging enabled: {}",
        path.display()
    );
    Some(fmt_layer)
}

fn resolve_log_path() -> Option<PathBuf> {
    let default_path = || distill_home().join("logs").join("hooks.log");
    let raw = match std::env::var(ENV_HOOKS_LOG) {
        Ok(val) => val,
        Err(_) => return None, // opt-in only
    };
    let raw = raw.trim();
    match raw {
        "" | "0" | "false" | "off" | "no" => None,
        "1" | "true" | "on" | "yes" => Some(default_path()),
        other => Some(PathBuf::from(other)),
    }
}
