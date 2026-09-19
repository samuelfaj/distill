// Modified for Distill by Samuel Fajreldines, 2026.
//! Shim; see `distill_telemetry::instrumentation` for the implementation.
//!
//! Two pieces stay here:
//! - The [`instrumentation_timer!`] macro is `#[macro_export]`-ed from this crate.
//!   Call sites spell it `crate::instrumentation_timer!` or `distill_shell::instrumentation_timer!`.
//!   Keeping the macro here means no downstream caller needs editing.
//! - [`finalize_and_exit`] logs a terminal exit event and shuts down the shared OTel pipeline before the process exits.
//!   The telemetry crate exposes the shutdown helper; this thin wrapper combines it with `process::exit`.

pub use distill_telemetry::instrumentation::{
    ChromeTraceOptions, InstrumentationFinalizer, InstrumentationMode, InstrumentationTimer,
    TARGET, current_mode, finalize, finalizer, generate_chrome_trace, install_panic_hook, layer,
    timer,
};

/// Logs an exit event, flushes instrumentation guards, shuts down the OpenTelemetry pipeline, and exits with `code`.
///
/// Stays in shell so callers can keep calling `distill_shell::instrumentation::finalize_and_exit`.
pub fn finalize_and_exit(code: i32) -> ! {
    let signal_name = match code {
        130 => "SIGINT",
        143 => "SIGTERM",
        _ => "other",
    };
    tracing::info!(
        event_type = "process_exit",
        signal = signal_name,
        exit_code = code,
        "Exiting process"
    );
    let _ = finalize();
    if let Some(path) = distill_telemetry::span_profile::finalize() {
        eprintln!("span profile written to {}", path.display());
    }
    distill_telemetry::otel_layer::shutdown_otel();
    // Flush the --debug log stream; exiting via process::exit bypasses main's flush
    distill_telemetry::debug_log::flush();
    std::process::exit(code);
}

/// Time a block under the instrumentation target. The macro stays in shell so `$crate` continues to resolve to `distill_shell` for the 12+ existing call sites.
/// Those sites spell it `crate::instrumentation_timer!(...)` or `distill_shell::instrumentation_timer!(...)`. The macro body delegates to types and functions in `distill_telemetry::instrumentation`.
#[macro_export]
macro_rules! instrumentation_timer {
    ($name:literal) => {{
        let mode = $crate::instrumentation::current_mode();
        match mode {
            $crate::instrumentation::InstrumentationMode::Chrome => {
                let span = tracing::info_span!(target: $crate::instrumentation::TARGET, $name);
                $crate::instrumentation::InstrumentationTimer::new_with_span(
                    $name,
                    mode,
                    Some(span.entered()),
                )
            }
            _ => $crate::instrumentation::InstrumentationTimer::new($name),
        }
    }};
}
