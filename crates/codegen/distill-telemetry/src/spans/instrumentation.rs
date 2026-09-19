// Modified for Distill by Samuel Fajreldines, 2026.
use std::io::{self, BufRead};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow};
use chrono::{DateTime, FixedOffset};
use serde_json::Value;
use tracing::Subscriber;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard, TraceStyle};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use distill_config::distill_home;

const ENV_ENABLED: &str = "GROK_INSTRUMENTATION";
const ENV_LOG_PATH: &str = "GROK_INSTRUMENTATION_LOG";
const DEFAULT_LOG_DIR: &str = "logs";
const DEFAULT_LOG_FILE: &str = "instrumentation.log";
const DEFAULT_TRACE_FILE: &str = "instrumentation.trace.json";

pub const TARGET: &str = "distill_instrumentation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentationMode {
    Disabled,
    Log,
    Chrome,
    Server,
}

static INSTRUMENTATION_MODE: OnceLock<InstrumentationMode> = OnceLock::new();
static LOG_GUARD: OnceLock<Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>> =
    OnceLock::new();
static CHROME_GUARD: OnceLock<Mutex<Option<FlushGuard>>> = OnceLock::new();

fn mode() -> InstrumentationMode {
    *INSTRUMENTATION_MODE.get_or_init(|| {
        let env_mode = match std::env::var(ENV_ENABLED) {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "enabled" | "log" | "json" | "jsonl" => {
                    Some(InstrumentationMode::Log)
                }
                "chrome" | "trace" | "trace.json" => Some(InstrumentationMode::Chrome),
                "server" => Some(InstrumentationMode::Server),
                "" | "0" | "false" | "off" | "disabled" | "none" => {
                    Some(InstrumentationMode::Disabled)
                }
                _ => Some(InstrumentationMode::Log),
            },
            Err(_) => None,
        };

        if let Some(mode) = env_mode {
            return mode;
        }

        // Send instrumentation to the configured OpenTelemetry endpoint by default
        InstrumentationMode::Server
    })
}

pub fn current_mode() -> InstrumentationMode {
    mode()
}

fn default_log_path() -> PathBuf {
    distill_home().join(DEFAULT_LOG_DIR).join(DEFAULT_LOG_FILE)
}

fn log_path_from_env() -> Option<PathBuf> {
    std::env::var(ENV_LOG_PATH)
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn default_output_path(mode: InstrumentationMode) -> PathBuf {
    let root = distill_home().join(DEFAULT_LOG_DIR);
    match mode {
        InstrumentationMode::Chrome => root.join(DEFAULT_TRACE_FILE),
        // Server uses OTLP export, not file output
        InstrumentationMode::Log | InstrumentationMode::Disabled | InstrumentationMode::Server => {
            root.join(DEFAULT_LOG_FILE)
        }
    }
}

/// A wrapper layer that filters events by target name in `enabled()` directly rather than via `.with_filter()`.
/// A `Filtered<L, F, S>` layer needs `FilterId` registration with the subscriber; boxing as `Box<dyn Layer<S>>` loses that.
/// The result is a panic: "a Filtered layer was used, but it had no FilterId".
pub struct TargetFilterLayer<L, S> {
    inner: L,
    target: &'static str,
    _subscriber: PhantomData<fn(S)>,
}

impl<L, S> TargetFilterLayer<L, S> {
    pub fn new(inner: L, target: &'static str) -> Self {
        Self {
            inner,
            target,
            _subscriber: PhantomData,
        }
    }
}

impl<L, S> Layer<S> for TargetFilterLayer<L, S>
where
    L: Layer<S>,
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn enabled(&self, metadata: &tracing::Metadata<'_>, ctx: Context<'_, S>) -> bool {
        metadata.target() == self.target && self.inner.enabled(metadata, ctx)
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if attrs.metadata().target() == self.target {
            self.inner.on_new_span(attrs, id, ctx);
        }
    }

    fn on_record(
        &self,
        span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        self.inner.on_record(span, values, ctx);
    }

    fn on_follows_from(
        &self,
        span: &tracing::span::Id,
        follows: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        self.inner.on_follows_from(span, follows, ctx);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() == self.target {
            self.inner.on_event(event, ctx);
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }
}

/// Returned when instrumentation is disabled, so the subscriber stack pays no overhead.
pub struct NoOpLayer<S>(PhantomData<fn(S)>);

impl<S> Default for NoOpLayer<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S> NoOpLayer<S> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S: Subscriber> Layer<S> for NoOpLayer<S> {
    // All methods use default implementations which do nothing
}

fn resolve_output_path(mode: InstrumentationMode) -> Option<PathBuf> {
    if mode == InstrumentationMode::Disabled {
        return None;
    }

    log_path_from_env().or_else(|| Some(default_output_path(mode)))
}

fn resolve_log_path() -> Option<PathBuf> {
    if mode() != InstrumentationMode::Log {
        return None;
    }
    resolve_output_path(InstrumentationMode::Log)
}

fn build_writer(path: Option<PathBuf>) -> BoxMakeWriter {
    let Some(path) = path else {
        return BoxMakeWriter::new(std::io::sink);
    };

    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "Failed to create instrumentation log directory {:?}: {}",
            parent, err
        );
        return BoxMakeWriter::new(std::io::sink);
    }

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!(
                "Failed to open instrumentation log file {:?}: {}",
                path, err
            );
            return BoxMakeWriter::new(std::io::sink);
        }
    };

    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let guard_slot = LOG_GUARD.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = guard_slot.lock() {
        *slot = Some(guard);
    }
    BoxMakeWriter::new(non_blocking)
}

fn build_log_layer<S>(mode: InstrumentationMode) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    if mode == InstrumentationMode::Disabled {
        return Box::new(NoOpLayer::new());
    }

    let writer = build_writer(resolve_log_path());

    // Use TargetFilterLayer instead of .with_filter() to avoid the FilterId registration issue when the layer is boxed as Box<dyn Layer<S>>
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false) // `spans` array already carries the full ancestor list
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_target(true)
        .with_writer(writer);

    Box::new(TargetFilterLayer::new(fmt_layer, TARGET))
}

fn build_chrome_layer<S>() -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    let Some(path) = resolve_output_path(InstrumentationMode::Chrome) else {
        return build_log_layer(InstrumentationMode::Disabled);
    };

    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "Failed to create chrome trace directory {:?}: {}",
            parent, err
        );
        return build_log_layer(InstrumentationMode::Disabled);
    }

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("Failed to open chrome trace file {:?}: {}", path, err);
            return build_log_layer(InstrumentationMode::Disabled);
        }
    };

    let (layer, guard) = ChromeLayerBuilder::<S>::new()
        .writer(file)
        .include_args(true)
        .trace_style(TraceStyle::Async)
        .build();

    let guard_slot = CHROME_GUARD.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = guard_slot.lock() {
        *slot = Some(guard);
    }

    // Use TargetFilterLayer instead of .with_filter() to avoid the FilterId registration issue when the layer is boxed as Box<dyn Layer<S>>
    Box::new(TargetFilterLayer::new(layer, TARGET))
}

pub fn layer<S>() -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    let mode = mode();
    match mode {
        InstrumentationMode::Chrome => build_chrome_layer(),
        InstrumentationMode::Log => build_log_layer(mode),
        // Server uses the OTEL layer in tracing.rs, not this instrumentation layer
        InstrumentationMode::Disabled | InstrumentationMode::Server => {
            build_log_layer(InstrumentationMode::Disabled)
        }
    }
}

/// Install a global panic hook that emits a structured tracing event before invoking the default hook.
/// Call this once, early in `main`, after the tracing subscriber has been installed.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        // `location` is the panic's source `file:line:col` (no user content); the redact layer scrubs the paths
        // It gives the panic counter a place to point without exporting the message or stack
        let err_span = tracing::info_span!(
            "internal_error",
            error_type = "panic",
            location = tracing::field::Empty,
        );
        if let Some(loc) = location.as_deref() {
            err_span.record("location", loc);
        }
        err_span.in_scope(|| {});
        // The external OTEL stream gets the error class only, never the message or location
        // `emit` is a synchronous queue push and a no-op unless the stream is active
        // The internal pipelines keep the richer span and event above
        crate::external::emit(&crate::events::InternalError {
            error_type: "panic".to_owned(),
        });
        tracing::error!(
            error_type = "panic",
            panic.message = %message,
            panic.location = ?location,
            "Process panicked"
        );
        default_hook(info);
    }));
}

fn resolve_input_path(input: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = input {
        return Ok(path);
    }
    if let Some(path) = log_path_from_env() {
        return Ok(path);
    }
    Ok(default_log_path())
}

#[derive(Debug, Clone, Default)]
pub struct ChromeTraceOptions {
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
}

pub fn generate_chrome_trace(options: ChromeTraceOptions) -> Result<PathBuf> {
    let input = resolve_input_path(options.input)?;
    let output = options
        .output
        .unwrap_or_else(|| input.with_extension("trace.json"));

    let file = std::fs::File::open(&input)
        .map_err(|err| anyhow!("failed to open instrumentation log {:?}: {}", input, err))?;
    let reader = io::BufReader::new(file);

    let mut events: Vec<Value> = Vec::new();
    let mut seen = 0usize;

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let target = value.get("target").and_then(Value::as_str);
        if target != Some(TARGET) {
            continue;
        }

        let fields = match value.get("fields").and_then(Value::as_object) {
            Some(fields) => fields,
            None => continue,
        };

        let event = fields.get("event").and_then(Value::as_str);
        if event != Some("timing") {
            continue;
        }

        let name = match fields.get("name").and_then(Value::as_str) {
            Some(name) => name,
            None => continue,
        };

        // Support both elapsed_us (new) and elapsed_ms (legacy) formats
        let dur_us = if let Some(us) = fields.get("elapsed_us").and_then(|v| v.as_u64()) {
            us
        } else if let Some(ms) = fields.get("elapsed_ms").and_then(|v| v.as_u64()) {
            ms.saturating_mul(1_000)
        } else {
            continue;
        };
        if dur_us == 0 {
            continue;
        }

        let timestamp = value.get("timestamp").and_then(Value::as_str);
        let end_us = match timestamp.and_then(parse_timestamp_us) {
            Some(ts) => ts,
            None => continue,
        };
        let start_us = end_us.saturating_sub(dur_us as i64);

        let mut args = serde_json::Map::new();
        args.insert("elapsed_us".to_string(), Value::Number(dur_us.into()));
        if let Some(extra) = fields.get("fields") {
            args.insert("fields".to_string(), extra.clone());
        }

        let thread_name = value
            .get("thread_name")
            .or_else(|| value.get("threadName"))
            .and_then(Value::as_str);
        if let Some(name) = thread_name {
            args.insert("thread_name".to_string(), Value::String(name.to_string()));
        }

        let thread_id = value
            .get("thread_id")
            .or_else(|| value.get("threadId"))
            .and_then(parse_thread_id)
            .unwrap_or(0);

        let trace_event = serde_json::json!({
            "name": name,
            "cat": "instrumentation",
            "ph": "X",
            "ts": start_us,
            "dur": dur_us,
            "pid": 1,
            "tid": thread_id,
            "args": Value::Object(args),
        });

        events.push(trace_event);
        seen += 1;
    }

    if seen == 0 {
        return Err(anyhow!("no timing events found in {:?}", input));
    }

    let trace = serde_json::json!({
        "displayTimeUnit": "ms",
        "traceEvents": events,
    });

    let mut output_file = std::fs::File::create(&output)
        .map_err(|err| anyhow!("failed to create chrome trace {:?}: {}", output, err))?;
    serde_json::to_writer_pretty(&mut output_file, &trace)
        .map_err(|err| anyhow!("failed to write chrome trace: {}", err))?;

    Ok(output)
}

pub fn finalize() -> Result<()> {
    let mode = mode();
    if mode == InstrumentationMode::Disabled {
        return Ok(());
    }

    drop_guard(LOG_GUARD.get());
    drop_guard(CHROME_GUARD.get());

    Ok(())
}

fn drop_guard<T>(guard: Option<&Mutex<Option<T>>>) {
    if let Some(lock) = guard
        && let Ok(mut slot) = lock.lock()
    {
        let _ = slot.take();
    }
}

pub struct InstrumentationFinalizer;

impl Drop for InstrumentationFinalizer {
    fn drop(&mut self) {
        let _ = finalize();
    }
}

pub fn finalizer() -> InstrumentationFinalizer {
    InstrumentationFinalizer
}

fn parse_timestamp_us(timestamp: &str) -> Option<i64> {
    let parsed: DateTime<FixedOffset> = DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(parsed.timestamp_micros())
}

fn parse_thread_id(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

pub struct InstrumentationTimer {
    name: &'static str,
    start: Instant,
    fields: Vec<(String, Value)>,
    subphase: Option<crate::startup::Subphase>,
    subphase_span: Option<tracing::Span>,
    parent: Option<timer_parents::Guard>,
    timer_span: tracing::Span,
    mode: InstrumentationMode,
    _span_guard: Option<tracing::span::EnteredSpan>,
}

impl InstrumentationTimer {
    pub fn new(name: &'static str) -> Self {
        Self::new_with_span(name, mode(), None)
    }

    pub fn new_with_span(
        name: &'static str,
        mode: InstrumentationMode,
        span_guard: Option<tracing::span::EnteredSpan>,
    ) -> Self {
        let (timer_span, span_guard, parent) = if span_guard.is_some()
            || matches!(
                mode,
                InstrumentationMode::Disabled | InstrumentationMode::Chrome
            ) {
            (tracing::Span::none(), span_guard, None)
        } else {
            let (timer_span, parent) = timer_parents::open(name);
            (timer_span, None, parent)
        };
        Self {
            name,
            start: Instant::now(),
            fields: Vec::new(),
            subphase: None,
            subphase_span: None,
            parent,
            timer_span,
            mode,
            _span_guard: span_guard,
        }
    }

    pub fn with_field(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        // Startup keeps fields in every mode: the `unified.jsonl` mirror needs them.
        if (self.mode != InstrumentationMode::Disabled && self.mode != InstrumentationMode::Chrome)
            || crate::startup::is_active()
        {
            self.fields.push((key.into(), value.into()));
        }
        self
    }

    pub fn with_server(&mut self, server: &str) -> &mut Self {
        self.timer_span.record("server_name", server);
        self
    }

    /// Route this timer's elapsed into a typed startup sub-phase field on drop (while startup is active).
    /// The mapping from producer to field is checked at compile time.
    pub fn with_subphase(&mut self, sp: crate::startup::Subphase) -> &mut Self {
        self.subphase = Some(sp);
        // Gated like the drop-time field mirror: create the span only while startup recording is active
        if self.subphase_span.is_none() && crate::startup::is_active() {
            self.subphase_span = Some(crate::startup::subphase_span(sp, &self.timer_span));
        }
        self
    }

    pub fn subphase_span(&self) -> Option<tracing::Span> {
        self.subphase_span.clone()
    }
}

impl Drop for InstrumentationTimer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        drop(self.parent.take());
        drop(self.subphase_span.take());
        drop(self._span_guard.take());
        drop(std::mem::replace(
            &mut self.timer_span,
            tracing::Span::none(),
        ));
        crate::startup::record_sub_timing(self.name, elapsed);
        // Mirror into `unified.jsonl` while startup is active, so a slow-launch report needs no env vars or repro
        // The first usable session latches this off
        if crate::startup::is_active() {
            if let Some(sp) = self.subphase {
                crate::startup::record_subphase(sp, elapsed);
            }
            let mut ctx = serde_json::Map::new();
            ctx.insert("name".to_string(), Value::String(self.name.to_string()));
            ctx.insert(
                "elapsed_ms".to_string(),
                Value::Number((elapsed.as_millis() as u64).into()),
            );
            for (key, value) in &self.fields {
                ctx.entry(key.clone()).or_insert_with(|| value.clone());
            }
            crate::unified_log::info(
                crate::startup::STARTUP_TIMING_MSG,
                None,
                Some(Value::Object(ctx)),
            );
        }
        if self.mode == InstrumentationMode::Disabled || self.mode == InstrumentationMode::Chrome {
            return;
        }
        let elapsed_us = elapsed.as_micros() as u64;
        if self.fields.is_empty() {
            tracing::info!(
                target: TARGET,
                event = "timing",
                name = self.name,
                elapsed_us = elapsed_us,
            );
            return;
        }

        let mut map = serde_json::Map::new();
        for (key, value) in std::mem::take(&mut self.fields) {
            map.insert(key, value);
        }

        let fields = Value::Object(map);
        tracing::info!(
            target: TARGET,
            event = "timing",
            name = self.name,
            elapsed_us = elapsed_us,
            fields = ?fields
        );
    }
}

pub fn timer(name: &'static str) -> InstrumentationTimer {
    InstrumentationTimer::new(name)
}

mod timer_parents {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::thread::ThreadId;

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum Key {
        Task(tokio::task::Id),
        Thread(ThreadId),
    }

    pub(super) struct Guard {
        key: Key,
        span: tracing::Span,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let mut map = map_lock();
            let Some(stack) = map.get_mut(&self.key) else {
                return;
            };
            if let Some(index) = stack.iter().rposition(|open| same_span(open, &self.span)) {
                stack.remove(index);
            }
            if stack.is_empty() {
                map.remove(&self.key);
            }
        }
    }

    fn key() -> Key {
        match tokio::task::try_id() {
            Some(id) => Key::Task(id),
            None => Key::Thread(std::thread::current().id()),
        }
    }

    fn map_lock() -> std::sync::MutexGuard<'static, HashMap<Key, Vec<tracing::Span>>> {
        static STACKS: std::sync::OnceLock<Mutex<HashMap<Key, Vec<tracing::Span>>>> =
            std::sync::OnceLock::new();
        STACKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn top() -> Option<tracing::Span> {
        let map = map_lock();
        map.get(&key()).and_then(|stack| stack.last().cloned())
    }

    fn push(span: &tracing::Span) -> Option<Guard> {
        if span.is_disabled() {
            return None;
        }
        let key = key();
        map_lock().entry(key).or_default().push(span.clone());
        Some(Guard {
            key,
            span: span.clone(),
        })
    }

    pub(super) fn open(name: &'static str) -> (tracing::Span, Option<Guard>) {
        let span = if let Some(parent) = top().or_else(crate::startup::current_phase_span) {
            tracing::info_span!(parent: &parent, "timer", name = name, server_name = tracing::field::Empty)
        } else {
            let current = tracing::Span::current();
            if current.is_disabled() {
                tracing::info_span!("timer", name = name, server_name = tracing::field::Empty)
            } else {
                tracing::info_span!(parent: &current, "timer", name = name, server_name = tracing::field::Empty)
            }
        };
        let guard = push(&span);
        (span, guard)
    }

    fn same_span(a: &tracing::Span, b: &tracing::Span) -> bool {
        match (a.id(), b.id()) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
}
