// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::task::AbortOnDropHandle;
use distill_pager::agent_runtime::AgentRuntime;
use distill_shell::agent::config::Config;

use crate::shutdown_and_flush_telemetry;

const STDIO_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct AgentSignals {
    defer_exit: Arc<AtomicBool>,
    received: oneshot::Receiver<i32>,
    _listener: AbortOnDropHandle<()>,
}

pub(crate) fn spawn_signal_flush() -> AgentSignals {
    let defer_exit = Arc::new(AtomicBool::new(false));
    let defer_exit_for_listener = Arc::clone(&defer_exit);
    let (sender, received) = oneshot::channel();
    // Signal observation must remain independent of synchronous agent startup
    let listener = AbortOnDropHandle::new(tokio::spawn(async move {
        let code = next_signal_code().await;
        if !defer_exit_for_listener.load(Ordering::Acquire) || sender.send(code).is_err() {
            shutdown_and_flush_telemetry(code);
        }
        // A graceful teardown gets one timer; a second signal ends it now
        let code = tokio::select! {
            () = tokio::time::sleep(STDIO_SHUTDOWN_TIMEOUT) => code,
            again = next_signal_code() => again,
        };
        shutdown_and_flush_telemetry(code);
    }));
    AgentSignals {
        defer_exit,
        received,
        _listener: listener,
    }
}

async fn next_signal_code() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate())
            .inspect_err(|error| tracing::warn!(%error, "failed to listen for SIGTERM"))
            .ok();
        let mut hup = signal(SignalKind::hangup())
            .inspect_err(|error| tracing::warn!(%error, "failed to listen for SIGHUP"))
            .ok();

        distill_pager::app::signal_handler::next_signal_code(&mut term, &mut hup).await
    }

    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
        130
    }
}

pub(crate) async fn run_stdio(
    runtime: &AgentRuntime,
    agent_config: &Config,
    mut signals: AgentSignals,
) -> anyhow::Result<()> {
    signals.defer_exit.store(true, Ordering::Release);

    let outcome = tokio::select! {
        biased;
        code = &mut signals.received => code.map(Some).map_err(anyhow::Error::from),
        result = runtime.run_stdio(agent_config) => result.map(|()| None),
    };
    if let Some(exit_code) = outcome? {
        shutdown_and_flush_telemetry(exit_code);
    }
    Ok(())
}
