//! Tracing layer that folds the span tree by wall time when `GROK_SPAN_PROFILE_OUT` names an output; inert otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::instrumentation::NoOpLayer;

/// A fixed file is last-writer-wins across processes sharing the variable.
/// Use a directory for multi-process runs (each writes `<label>-<pid>-.folded`).
pub const OUT_ENV: &str = "GROK_SPAN_PROFILE_OUT";

const MAX_PATHS: usize = 8192;

static PROFILE: OnceLock<SpanProfile> = OnceLock::new();

/// One process's folded span wall times and their destination.
struct SpanProfile {
    output: PathBuf,
    label: &'static str,
    started_at: u64,
    folded: Mutex<HashMap<String, u64>>,
    dropped_paths: AtomicU64,
}

impl SpanProfile {
    fn record(&self, path: String, self_time_us: u64) {
        if self_time_us == 0 {
            return;
        }
        let mut folded = self.folded.lock().unwrap_or_else(PoisonError::into_inner);
        if folded.len() >= MAX_PATHS && !folded.contains_key(&path) {
            self.dropped_paths.fetch_add(1, Ordering::Relaxed);
            return;
        }
        *folded.entry(path).or_insert(0) += self_time_us;
    }

    fn write(&self) -> Option<PathBuf> {
        let path = artifact_path(&self.output, self.label, self.started_at);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "failed to create the span profile directory");
            return None;
        }
        let folded =
            std::mem::take(&mut *self.folded.lock().unwrap_or_else(PoisonError::into_inner));
        if folded.is_empty() {
            return None;
        }

        use std::fmt::Write as _;
        let mut lines: Vec<(String, u64)> = folded.into_iter().collect();
        lines.sort();
        let mut body = String::new();
        for (path, weight_us) in &lines {
            let _ = writeln!(body, "{path} {weight_us}");
        }

        if let Err(error) = std::fs::write(&path, body) {
            // Restore so a later finalize on another exit path can retry.
            let mut folded = self.folded.lock().unwrap_or_else(PoisonError::into_inner);
            for (path, weight_us) in lines {
                *folded.entry(path).or_insert(0) += weight_us;
            }
            tracing::warn!(%error, "failed to write the span profile");
            return None;
        }
        let dropped = self.dropped_paths.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(dropped, "span profile dropped paths past the cap");
        }
        tracing::info!(path = %path.display(), "wrote span profile");
        Some(path)
    }
}

struct SpanTiming {
    opened: Instant,
    child_wall_us: u64,
    /// The span's `name` field value; the folded path uses it instead of the span name, so one callsite can produce a frame per region.
    detail: Option<String>,
}

struct NameVisitor(Option<String>);

impl tracing::field::Visit for NameVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "name" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

struct SpanProfileLayer {
    profile: &'static SpanProfile,
}

impl<S> Layer<S> for SpanProfileLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            let mut visitor = NameVisitor(None);
            attrs.record(&mut visitor);
            span.extensions_mut().insert(SpanTiming {
                opened: Instant::now(),
                child_wall_us: 0,
                detail: visitor.0,
            });
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(timing) = span.extensions_mut().remove::<SpanTiming>() else {
            return;
        };
        let wall_us = timing.opened.elapsed().as_micros() as u64;

        if let Some(parent) = span.parent()
            && let Some(parent_timing) = parent.extensions_mut().get_mut::<SpanTiming>()
        {
            parent_timing.child_wall_us = parent_timing.child_wall_us.saturating_add(wall_us);
        }

        // Concurrent children overlap, so a parent's self time clamps at zero.
        let self_time_us = wall_us.saturating_sub(timing.child_wall_us);

        let mut path = String::new();
        for ancestor in span.scope().from_root() {
            if !path.is_empty() {
                path.push(';');
            }
            if ancestor.id() == id {
                path.push_str(timing.detail.as_deref().unwrap_or(ancestor.name()));
            } else {
                let extensions = ancestor.extensions();
                let detail = extensions
                    .get::<SpanTiming>()
                    .and_then(|t| t.detail.as_deref());
                path.push_str(detail.unwrap_or(ancestor.name()));
            }
        }
        self.profile.record(path, self_time_us);
    }
}

/// The span-profile layer for this process when [`OUT_ENV`] names an output; a no-op layer otherwise.
/// `label` prefixes the artifact file name when the output is a directory.
pub fn layer<S>(label: &'static str) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    let Some(output) = std::env::var_os(OUT_ENV).filter(|value| !value.is_empty()) else {
        return Box::new(NoOpLayer::new());
    };
    let profile = SpanProfile {
        output: PathBuf::from(output),
        label,
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
        folded: Mutex::new(HashMap::new()),
        dropped_paths: AtomicU64::new(0),
    };
    if PROFILE.set(profile).is_err() {
        return Box::new(NoOpLayer::new());
    }
    #[expect(clippy::expect_used, reason = "the set on the line above succeeded")]
    let profile = PROFILE.get().expect("just set");
    Box::new(SpanProfileLayer { profile })
}

/// Write the folded span wall times, returning the artifact path; `None` when this process never enabled the layer or nothing closed.
/// Safe to call from any exit path; later calls after a successful write return `None`.
pub fn finalize() -> Option<PathBuf> {
    PROFILE.get()?.write()
}

fn artifact_path(output: &Path, label: &str, started_at: u64) -> PathBuf {
    if output.extension().is_some() && !output.is_dir() {
        return output.to_path_buf();
    }
    output.join(format!(
        "{label}-{pid}-{started_at}.folded",
        pid = std::process::id()
    ))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// Run `f` under a fresh profile layer and return its folded output.
    pub(crate) fn folded_with_layer(f: impl FnOnce()) -> String {
        let profile: &'static SpanProfile = Box::leak(Box::new(SpanProfile {
            output: std::path::PathBuf::from("/tmp"),
            label: "test",
            started_at: 0,
            folded: Mutex::new(HashMap::new()),
            dropped_paths: AtomicU64::new(0),
        }));
        let subscriber = tracing_subscriber::registry().with(SpanProfileLayer { profile });
        let _guard = tracing::subscriber::set_default(subscriber);
        f();
        let folded = profile
            .folded
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut lines: Vec<String> = folded.iter().map(|(k, v)| format!("{k} {v}")).collect();
        lines.sort();
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> SpanProfile {
        SpanProfile {
            output: PathBuf::from("/tmp"),
            label: "test",
            started_at: 0,
            folded: Mutex::new(HashMap::new()),
            dropped_paths: AtomicU64::new(0),
        }
    }

    #[test]
    fn record_accumulates_per_path_and_holds_the_cap() {
        let profile = test_profile();
        profile.record("a;b".to_owned(), 5);
        profile.record("a;b".to_owned(), 7);
        profile.record("zero-weight".to_owned(), 0);
        for i in 0..MAX_PATHS - 1 {
            profile.record(format!("p{i}"), 1);
        }
        profile.record("past-the-cap".to_owned(), 1);

        let folded = profile.folded.lock().unwrap();
        assert_eq!(folded.get("a;b"), Some(&12));
        assert!(!folded.contains_key("zero-weight"));
        assert!(!folded.contains_key("past-the-cap"));
        assert_eq!(profile.dropped_paths.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(unix)]
    fn failed_write_keeps_the_profile_for_retry() {
        use std::os::unix::ffi::OsStrExt;
        // A NUL byte makes the write fail even when tests run as root.
        let blocked = std::env::temp_dir().join(std::ffi::OsStr::from_bytes(b"out\0.folded"));
        let profile = SpanProfile {
            output: blocked,
            ..test_profile()
        };
        profile.record("a;b".to_owned(), 5);
        assert!(profile.write().is_none());
        assert_eq!(profile.folded.lock().unwrap().get("a;b"), Some(&5));
    }

    #[test]
    fn nested_timer_folds_under_parent_without_enter() {
        use crate::instrumentation::{InstrumentationMode, InstrumentationTimer};

        let folded = super::test_support::folded_with_layer(|| {
            let _parent =
                InstrumentationTimer::new_with_span("parent.work", InstrumentationMode::Log, None);
            let _child =
                InstrumentationTimer::new_with_span("child.work", InstrumentationMode::Log, None);
            std::thread::sleep(std::time::Duration::from_millis(2));
        });
        let paths: Vec<&str> = folded
            .lines()
            .filter_map(|l| l.rsplit_once(' ').map(|(p, _)| p))
            .collect();
        assert!(
            paths.contains(&"parent.work;child.work"),
            "child timer must nest via the explicit parent stack, got:\n{folded}"
        );
        assert!(
            !paths.contains(&"child.work"),
            "child timer folded as root:\n{folded}"
        );
    }

    #[test]
    fn artifact_path_treats_extensionless_output_as_directory() {
        let dir = artifact_path(Path::new("/tmp/prof"), "tui", 7);
        assert!(dir.to_string_lossy().contains("/tmp/prof/tui-"));
        assert!(dir.to_string_lossy().ends_with("-7.folded"));
        let file = artifact_path(Path::new("/tmp/prof/run.folded"), "tui", 7);
        assert_eq!(file, PathBuf::from("/tmp/prof/run.folded"));
    }
}
