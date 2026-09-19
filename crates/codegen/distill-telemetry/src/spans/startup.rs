//! Named startup phases on a per-process timer, reported once to `unified.jsonl`, product events, and OTLP metrics.
//! A closed schema with pinned metric keys: time anything else with a `tracing` span, or give it its own schema.
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
/// `unified.jsonl` message keys, exported so consumers (the probe, tests) grep for the same strings this module writes.
pub const STARTUP_PHASE_MSG: &str = "startup phase";
pub const CONNECT_FINISHED_MSG: &str = "connect finished";
pub const STARTUP_COMPLETE_MSG: &str = "startup complete";
pub const STARTUP_INTERACTIVE_MSG: &str = "startup interactive";
pub const STARTUP_TIMING_MSG: &str = "startup timing";
pub const STARTUP_SLOW_PHASE_MSG: &str = "startup phase running long";
pub const STARTUP_OVER_BUDGET_MSG: &str = "startup phase over budget";
/// A launcher stamps the wall-clock spawn time, capturing the gap before our own clock starts.
pub const SPAWN_TIMESTAMP_ENV: &str = "GROK_SPAWN_TIMESTAMP_MS";
/// Benchmarks set this to stop right after the first confirmed frame.
pub const EXIT_AFTER_FIRST_RENDER_ENV: &str = "GROK_EXIT_AFTER_FIRST_RENDER";
const SLOW_PHASE_WARN_AFTER: Duration = Duration::from_secs(10);
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum StartupPhase {
    ConfigLoad,
    ManagedPolicy,
    Bootstrap,
    ModelCatalog,
    WorkerSpawn,
    LeaderConnect,
    AcpInitialize,
    EagerAuth,
    AppInit,
    SessionCreate,
}
impl StartupPhase {
    pub fn label(self) -> &'static str {
        self.into()
    }
    /// Regression-gate ceilings, not SLOs: they only flag a phase that grew far past its usual cost.
    fn budget(self) -> Duration {
        use StartupPhase::*;
        match self {
            ConfigLoad | ModelCatalog | WorkerSpawn | AppInit => Duration::from_secs(1),
            ManagedPolicy | EagerAuth => Duration::from_secs(2),
            AcpInitialize => Duration::from_secs(6),
            Bootstrap | LeaderConnect | SessionCreate => Duration::from_secs(3),
        }
    }
}
macro_rules! span_table {
    ($visibility:vis fn $name:ident, fn $under:ident($enum_name:ident) { $($variant:ident => $label:literal),* $(,)? }) => {
        $visibility fn $name(value: $enum_name) -> tracing::Span {
            match value {
                $($enum_name::$variant => tracing::info_span!($label),)*
            }
        }
        $visibility fn $under(value: $enum_name, parent: &tracing::Span) -> tracing::Span {
            match value {
                $($enum_name::$variant => tracing::info_span!(parent: parent, $label),)*
            }
        }
    };
    ($visibility:vis fn $name:ident($enum_name:ident, parent) { $($variant:ident => $label:literal),* $(,)? }) => {
        $visibility fn $name(value: $enum_name, parent: &tracing::Span) -> tracing::Span {
            match value {
                $($enum_name::$variant => tracing::info_span!(parent: parent, $label),)*
            }
        }
    };
    ($visibility:vis fn $name:ident($enum_name:ident) { $($variant:ident => $label:literal),* $(,)? }) => {
        $visibility fn $name(value: $enum_name) -> tracing::Span {
            match value {
                $($enum_name::$variant => tracing::info_span!($label),)*
            }
        }
    };
}
pub(crate) use span_table;
span_table!(fn phase_span(StartupPhase, parent) {
    ConfigLoad => "startup.config_load",
    ManagedPolicy => "startup.managed_policy",
    Bootstrap => "startup.bootstrap",
    ModelCatalog => "startup.model_catalog",
    WorkerSpawn => "startup.worker_spawn",
    LeaderConnect => "startup.leader_connect",
    AcpInitialize => "startup.acp_initialize",
    EagerAuth => "startup.eager_auth",
    AppInit => "startup.app_init",
    SessionCreate => "startup.session_create",
});
span_table!(pub(crate) fn subphase_span(Subphase, parent) {
    SessionLoad => "startup.session_load",
    SessionReplay => "startup.session_replay",
    SessionGitScan => "startup.session_git_scan",
    SessionSpawn => "startup.session_spawn",
    InitProcess => "startup.bootstrap.init_process",
    ResolveConfig => "startup.bootstrap.resolve_config",
    RemoteSettings => "startup.bootstrap.remote_settings",
    ModelsManager => "startup.model_catalog.models_manager",
    ManagedPolicyAuthWait => "startup.managed_policy.auth_wait",
    ManagedPolicyConfigSync => "startup.managed_policy.config_sync",
});
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr, serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StartupOutcome {
    Ok,
    Timeout,
    Cancelled,
    Error,
}
impl StartupOutcome {
    pub fn label(self) -> &'static str {
        self.into()
    }
}
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    strum::AsRefStr,
    strum::IntoStaticStr,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Unknown,
    Personal,
    Team,
    Deployment,
}
impl AuthMode {
    pub fn label(self) -> &'static str {
        self.into()
    }
}
/// Who reports the timer, so an embedded run does not report it twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Client,
    Agent,
}
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr, serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Embedded,
    Leader,
}
impl AgentKind {
    pub fn label(self) -> &'static str {
        self.into()
    }
}
#[derive(Clone, Debug)]
pub struct PhaseSnapshot {
    pub completed: Vec<(StartupPhase, Duration)>,
    pub open: Option<(StartupPhase, Duration)>,
}
impl PhaseSnapshot {
    pub fn stuck_in(&self) -> &'static str {
        self.open.map_or("unknown", |(phase, _)| phase.label())
    }
    /// Not the open step: a step with no await inside closes before a deadline observer can run, so the open one is usually its successor.
    pub fn longest_step(&self) -> Option<StartupPhase> {
        self.completed
            .iter()
            .copied()
            .chain(self.open)
            .max_by_key(|(_, elapsed)| *elapsed)
            .map(|(phase, _)| phase)
    }
    fn over_budget(&self) -> Vec<(StartupPhase, Duration)> {
        self.completed
            .iter()
            .copied()
            .chain(self.open)
            .filter(|(phase, elapsed)| *elapsed > phase.budget())
            .collect()
    }
    /// Completed phases read `phase=dur`; the open one reads `phase>=dur`.
    pub fn summary(&self) -> String {
        if self.completed.is_empty() && self.open.is_none() {
            return "no phases entered".to_string();
        }
        let mut out = String::new();
        for (phase, d) in &self.completed {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let _ = write!(out, "{}={}", phase.label(), format_duration(*d));
        }
        if let Some((phase, open)) = self.open {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let _ = write!(out, "{}>={}", phase.label(), format_duration(open));
        }
        out
    }
}
struct Inner {
    completed: Vec<(StartupPhase, Duration)>,
    current: Option<(StartupPhase, Instant)>,
    current_span: Option<tracing::Span>,
    root_span: tracing::Span,
    auth_mode: AuthMode,
    owner: Owner,
}
pub struct StartupTimer {
    started: Instant,
    inner: Mutex<Inner>,
    /// This attempt's `startup.*` sub-timers, owned so they cannot bleed into another attempt.
    sub_timers: Mutex<Vec<(String, u64)>>,
    /// Drains this attempt's sub-timers at most once, whichever site reaches it first.
    sub_timers_drained: AtomicBool,
}
impl StartupTimer {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: Mutex::new(Inner {
                completed: Vec::new(),
                current: None,
                current_span: None,
                root_span: tracing::info_span!("startup", outcome = tracing::field::Empty),
                auth_mode: AuthMode::Unknown,
                owner: Owner::Agent,
            }),
            sub_timers: Mutex::new(Vec::new()),
            sub_timers_drained: AtomicBool::new(false),
        }
    }
    /// Appends a `startup.*` sub-timing to this attempt's own buffer.
    fn push_sub_timing(&self, key: &str, elapsed: Duration) {
        self.sub_timers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key.to_string(), elapsed.as_millis() as u64));
    }
    /// Drains this attempt's sub-timers exactly once and emits them with `outcome` and this attempt's auth mode.
    fn drain_sub_timers(&self, outcome: StartupOutcome) {
        if self.sub_timers_drained.swap(true, Ordering::Relaxed) {
            return;
        }
        let timings: Vec<(String, u64)> = {
            let mut buf = self.sub_timers.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *buf)
        };
        if timings.is_empty() {
            return;
        }
        crate::session_ctx::log_event(crate::events::StartupSubTimers {
            timings,
            outcome,
            auth_mode: self.auth_mode(),
        });
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Closes the open phase; re-entering the open phase is ignored, so two layers can name the same step and it is measured once.
    pub fn enter(&self, phase: StartupPhase) {
        let now = Instant::now();
        let finished_span;
        {
            let mut g = self.lock();
            if matches!(g.current, Some((open, _)) if open == phase) {
                return;
            }
            if let Some((prev, t0)) = g.current.take() {
                g.completed.push((prev, now.saturating_duration_since(t0)));
            }
            g.current = Some((phase, now));
            let span = phase_span(phase, &g.root_span);
            finished_span = g.current_span.replace(span);
        }
        drop(finished_span);
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        tracing::info!(phase = %phase.label(), elapsed_ms, "startup phase");
        crate::unified_log::info(
            STARTUP_PHASE_MSG,
            None,
            Some(serde_json::json!({ "phase": phase.label(), "elapsed_ms": elapsed_ms })),
        );
    }
    fn close_open_phase(&self) {
        let now = Instant::now();
        let finished_span;
        {
            let mut g = self.lock();
            if let Some((prev, t0)) = g.current.take() {
                g.completed.push((prev, now.saturating_duration_since(t0)));
            }
            finished_span = g.current_span.take();
        }
        drop(finished_span);
    }
    fn close_root_span(&self, outcome: &'static str) {
        let (open_phase, root);
        {
            let mut g = self.lock();
            open_phase = g.current_span.take();
            g.root_span.record("outcome", outcome);
            root = std::mem::replace(&mut g.root_span, tracing::Span::none());
        }
        drop(open_phase);
        drop(root);
    }
    /// A discarded run's spans close at the discard, not at the last `Arc` drop, so idle wait after first client is never attributed to a phase.
    fn discard_spans(&self) {
        self.close_root_span("discarded");
    }
    pub fn set_auth_mode(&self, mode: AuthMode) {
        self.lock().auth_mode = mode;
    }
    pub fn auth_mode(&self) -> AuthMode {
        self.lock().auth_mode
    }
    pub fn owner(&self) -> Owner {
        self.lock().owner
    }
    fn open_phase_age(&self) -> Option<(StartupPhase, Duration)> {
        self.lock().current.map(|(p, t0)| (p, t0.elapsed()))
    }
    /// One read, so a caller reporting several facts can't mix moments.
    pub fn phase_snapshot(&self) -> PhaseSnapshot {
        let now = Instant::now();
        let g = self.lock();
        PhaseSnapshot {
            completed: g.completed.clone(),
            open: g
                .current
                .map(|(phase, t0)| (phase, now.saturating_duration_since(t0))),
        }
    }
    pub fn summary(&self) -> String {
        self.phase_snapshot().summary()
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn phase_durations_ms(&self) -> BTreeMap<String, u64> {
        let now = Instant::now();
        let g = self.lock();
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        for (phase, d) in &g.completed {
            *map.entry(phase.label().to_string()).or_default() += d.as_millis() as u64;
        }
        if let Some((phase, t0)) = g.current {
            *map.entry(phase.label().to_string()).or_default() +=
                now.saturating_duration_since(t0).as_millis() as u64;
        }
        map
    }
    pub fn emit_telemetry(
        &self,
        connect_target: AgentKind,
        outcome: StartupOutcome,
        timeout_secs: Option<u64>,
        embedded_fallback: bool,
    ) {
        if outcome == StartupOutcome::Ok {
            self.close_open_phase();
        }
        let timings = self.phase_snapshot();
        let stuck_in = (outcome == StartupOutcome::Timeout).then(|| timings.stuck_in().to_string());
        let phases = timings.summary();
        let elapsed_ms = self.elapsed().as_millis() as u64;
        let ctx = serde_json::json!({
            "connect_target": connect_target,
            "outcome": outcome,
            "stuck_in": stuck_in,
            "phases": phases,
            "elapsed_ms": elapsed_ms,
            "auth_mode": self.auth_mode(),
        });
        crate::unified_log::info(CONNECT_FINISHED_MSG, None, Some(ctx));
        crate::session_ctx::log_event(crate::events::AgentConnect {
            connect_target,
            outcome,
            stuck_in,
            phases,
            phase_durations_ms: self.phase_durations_ms(),
            elapsed_ms,
            timeout_secs,
            embedded_fallback,
            auth_mode: self.auth_mode(),
        });
        if outcome != StartupOutcome::Ok {
            self.drain_sub_timers(outcome);
        }
    }
}
impl Default for StartupTimer {
    fn default() -> Self {
        Self::new()
    }
}
static CURRENT: Mutex<Option<Arc<StartupTimer>>> = Mutex::new(None);
static DONE: AtomicBool = AtomicBool::new(false);
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);
/// A startup sub-phase routed to its own `*_ms` field, so a producer timer's field is chosen at compile time rather than by string match.
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::IntoStaticStr, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum Subphase {
    SessionLoad,
    SessionReplay,
    SessionGitScan,
    SessionSpawn,
    InitProcess,
    ResolveConfig,
    RemoteSettings,
    ModelsManager,
    ManagedPolicyAuthWait,
    ManagedPolicyConfigSync,
}
#[derive(Clone, Copy, Default, serde::Serialize)]
struct SubphaseTimings {
    prefetch_wait_ms: Option<u64>,
    session_load_ms: Option<u64>,
    session_replay_ms: Option<u64>,
    session_git_scan_ms: Option<u64>,
    session_spawn_ms: Option<u64>,
    init_process_ms: Option<u64>,
    resolve_config_ms: Option<u64>,
    remote_settings_ms: Option<u64>,
    models_manager_ms: Option<u64>,
    managed_policy_auth_wait_ms: Option<u64>,
    managed_policy_config_sync_ms: Option<u64>,
    time_to_first_frame_ms: Option<u64>,
    startup_total_ms: Option<u64>,
}
static SUBPHASES: Mutex<SubphaseTimings> = Mutex::new(SubphaseTimings {
    prefetch_wait_ms: None,
    session_load_ms: None,
    session_replay_ms: None,
    session_git_scan_ms: None,
    session_spawn_ms: None,
    init_process_ms: None,
    resolve_config_ms: None,
    remote_settings_ms: None,
    models_manager_ms: None,
    managed_policy_auth_wait_ms: None,
    managed_policy_config_sync_ms: None,
    time_to_first_frame_ms: None,
    startup_total_ms: None,
});
static INTERACTIVE: AtomicBool = AtomicBool::new(false);
static STARTUP_AUTH_MODE: Mutex<AuthMode> = Mutex::new(AuthMode::Unknown);
fn subphases() -> std::sync::MutexGuard<'static, SubphaseTimings> {
    SUBPHASES.lock().unwrap_or_else(|e| e.into_inner())
}
/// Routes a `startup.*` sub-timing to the current attempt's own buffer; a no-op with no current attempt.
/// Not gated on DONE: the gate was what dropped sub-timers on warm runs.
pub fn record_sub_timing(name: &str, elapsed: Duration) {
    let Some(key) = name.strip_prefix("startup.") else {
        return;
    };
    if let Some(timer) = current() {
        timer.push_sub_timing(key, elapsed);
    }
}
pub fn record_prefetch_wait(elapsed: Duration) {
    subphases().prefetch_wait_ms = Some(elapsed.as_millis() as u64);
}
pub fn record_first_frame() {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let elapsed_ms = process_elapsed().as_millis() as u64;
    let mut sub = subphases();
    if sub.time_to_first_frame_ms.is_none() {
        sub.time_to_first_frame_ms = Some(elapsed_ms);
    }
}
/// Records process start to the first confirmed frame once; returns whether this call recorded it.
pub fn record_interactive_frame() -> bool {
    if INTERACTIVE.swap(true, Ordering::Relaxed) {
        return false;
    }
    let event = crate::events::StartupInteractive {
        interactive_ms: process_elapsed().as_millis() as u64,
        startup_total_ms: subphases().startup_total_ms,
        spawn_to_first_frame_ms: spawn_to_first_frame_ms(),
        auth_mode: *STARTUP_AUTH_MODE.lock().unwrap_or_else(|e| e.into_inner()),
    };
    if let Ok(record) = serde_json::to_value(&event) {
        crate::unified_log::info(STARTUP_INTERACTIVE_MSG, None, Some(record));
    }
    crate::session_ctx::log_event(event);
    true
}
/// Whether a benchmark asked to quit after the first confirmed frame.
pub fn exit_after_first_render() -> bool {
    std::env::var(EXIT_AFTER_FIRST_RENDER_ENV).is_ok_and(|v| !v.is_empty() && v != "0")
}
/// Wall-clock milliseconds from the launcher-stamped spawn time to now, if stamped.
fn spawn_to_first_frame_ms() -> Option<u64> {
    let spawn_ms: u64 = std::env::var(SPAWN_TIMESTAMP_ENV).ok()?.parse().ok()?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    now_ms.checked_sub(spawn_ms)
}
pub(crate) fn record_subphase(sp: Subphase, elapsed: Duration) {
    let ms = elapsed.as_millis() as u64;
    let mut sub = subphases();
    let slot = match sp {
        Subphase::SessionLoad => &mut sub.session_load_ms,
        Subphase::SessionReplay => &mut sub.session_replay_ms,
        Subphase::SessionGitScan => &mut sub.session_git_scan_ms,
        Subphase::SessionSpawn => &mut sub.session_spawn_ms,
        Subphase::InitProcess => &mut sub.init_process_ms,
        Subphase::ResolveConfig => &mut sub.resolve_config_ms,
        Subphase::RemoteSettings => &mut sub.remote_settings_ms,
        Subphase::ModelsManager => &mut sub.models_manager_ms,
        Subphase::ManagedPolicyAuthWait => &mut sub.managed_policy_auth_wait_ms,
        Subphase::ManagedPolicyConfigSync => &mut sub.managed_policy_config_sync_ms,
    };
    match sp {
        Subphase::InitProcess => {
            slot.get_or_insert(ms);
        }
        _ => *slot = Some(ms),
    }
}
/// Call first in `main`; the clock otherwise starts at first use and totals undercount.
pub fn mark_process_start() {
    LazyLock::force(&PROCESS_START);
}
pub fn process_elapsed() -> Duration {
    PROCESS_START.elapsed()
}
fn current() -> Option<Arc<StartupTimer>> {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(Arc::clone)
}
/// Installs a new attempt, unless startup already ended; after that the returned timer records locally only.
pub fn begin(owner: Owner) -> Arc<StartupTimer> {
    let timer = Arc::new(StartupTimer::new());
    timer.lock().owner = owner;
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let done = DONE.load(Ordering::Relaxed);
    let superseded = (!done).then(|| current.take()).flatten();
    if !done {
        *current = Some(Arc::clone(&timer));
        spawn_slow_phase_warnings();
    }
    drop(current);
    if let Some(prev) = superseded {
        prev.drain_sub_timers(StartupOutcome::Cancelled);
    }
    timer
}
/// Phases already warned about, per timer.
#[derive(Default)]
struct WarnedPhases {
    timer: usize,
    phases: Vec<StartupPhase>,
}
/// A phase left open past the threshold; agent-owned timers idle with a phase open until their first client, so they are skipped.
fn slow_phase_to_warn(
    timer: &Arc<StartupTimer>,
    threshold: Duration,
    warned: &mut WarnedPhases,
) -> Option<(StartupPhase, Duration)> {
    if timer.owner() == Owner::Agent {
        return None;
    }
    let timer_id = Arc::as_ptr(timer) as usize;
    if warned.timer != timer_id {
        *warned = WarnedPhases {
            timer: timer_id,
            phases: Vec::new(),
        };
    }
    let (phase, age) = timer.open_phase_age()?;
    if age < threshold || warned.phases.contains(&phase) {
        return None;
    }
    warned.phases.push(phase);
    Some((phase, age))
}
/// Warns once per phase that runs long.
/// A plain thread, because startup spans runtime construction; exits when startup ends.
fn spawn_slow_phase_warnings() {
    static SPAWNED: std::sync::Once = std::sync::Once::new();
    SPAWNED.call_once(|| {
        std::thread::Builder::new()
            .name("startup-slow-phase".into())
            .spawn(|| {
                let mut warned = WarnedPhases::default();
                while !DONE.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    let Some(timer) = current() else { continue };
                    if let Some((phase, age)) =
                        slow_phase_to_warn(&timer, SLOW_PHASE_WARN_AFTER, &mut warned)
                    {
                        let open_ms = age.as_millis() as u64;
                        tracing::warn!(
                            phase = phase.label(),
                            open_ms,
                            "startup phase running long"
                        );
                        let ctx = serde_json::json!({
                            "phase": phase.label(),
                            "open_ms": open_ms,
                        });
                        crate::unified_log::warn(STARTUP_SLOW_PHASE_MSG, None, Some(ctx));
                    }
                }
            })
            .ok();
    });
}
pub(crate) fn agent_owned() -> Option<Arc<StartupTimer>> {
    current().filter(|p| p.owner() == Owner::Agent)
}
pub(crate) fn is_active() -> bool {
    !DONE.load(Ordering::Relaxed) && current().is_some()
}
pub fn current_phase_span() -> Option<tracing::Span> {
    current()?.lock().current_span.clone()
}
#[derive(Clone)]
pub struct SpawnTraceContext {
    pub parent: tracing::Span,
}
impl SpawnTraceContext {
    pub fn new(startup_span: Option<tracing::Span>, request_span: tracing::Span) -> Self {
        Self {
            parent: startup_span.unwrap_or(request_span),
        }
    }
}
fn clear() {
    DONE.store(true, Ordering::Relaxed);
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
/// Stops recording for a standalone agent at its first client, so idle waiting is not counted; client-owned runs are unaffected.
pub fn mark_agent_serving() {
    if let Some(timer) = agent_owned() {
        timer.discard_spans();
        timer.drain_sub_timers(StartupOutcome::Ok);
        clear();
    }
}
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    DONE.store(false, Ordering::Relaxed);
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *subphases() = SubphaseTimings::default();
    INTERACTIVE.store(false, Ordering::Relaxed);
    *STARTUP_AUTH_MODE.lock().unwrap_or_else(|e| e.into_inner()) = AuthMode::Unknown;
}
/// Lazily installs an agent-owned timer, covering the standalone leader and agent server; a no-op once startup is done.
pub fn enter(phase: StartupPhase) {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let timer = match current() {
        Some(timer) => timer,
        None => begin(Owner::Agent),
    };
    timer.enter(phase);
}
/// Scopes a phase to a region of work: entered on creation, closed on drop, so no failure return can leave the phase open across a retry wait.
#[must_use = "the phase closes when this guard drops"]
pub struct PhaseScope(());
impl Drop for PhaseScope {
    fn drop(&mut self) {
        if let Some(timer) = current() {
            timer.close_open_phase();
        }
    }
}
/// Enter `phase` for the lifetime of the returned guard.
pub fn phase_scope(phase: StartupPhase) -> PhaseScope {
    enter(phase);
    PhaseScope(())
}
pub fn set_auth_mode(mode: AuthMode) {
    *STARTUP_AUTH_MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    if let Some(timer) = current() {
        timer.set_auth_mode(mode);
    }
}
/// The obligation to end startup exactly once; a dropped token ends startup itself and logs a warning, so forgotten paths are visible.
#[must_use = "startup must be finished or abandoned"]
pub struct PendingStartup {
    ended: bool,
}
impl PendingStartup {
    /// One per interactive or headless process; utility commands call [`mark_utility_process`] instead.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        PendingStartup { ended: false }
    }
    /// Records the startup total with `outcome` and ends recording.
    pub fn finish(mut self, outcome: StartupOutcome) {
        report_total(outcome);
        self.ended = true;
    }
    /// Ends recording without a total, for a run the user cancelled or one that never was a startup.
    pub fn abandon(mut self) {
        clear();
        self.ended = true;
    }
    /// Finishes a token still held in an `Option`; does nothing once taken.
    pub fn finish_held(token: &mut Option<Self>, outcome: StartupOutcome) {
        if let Some(pending) = token.take() {
            pending.finish(outcome);
        }
    }
}
impl Drop for PendingStartup {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        tracing::warn!("startup was never finished; ending recording");
        crate::unified_log::warn("startup never finished", None, None);
        clear();
    }
}
/// Excludes a utility command from startup recording entirely.
pub fn mark_utility_process() {
    clear();
}
fn warn_over_budget(snapshot: &PhaseSnapshot) {
    let over: Vec<(&str, u64)> = snapshot
        .over_budget()
        .iter()
        .map(|(phase, elapsed)| (phase.label(), elapsed.as_millis() as u64))
        .collect();
    if over.is_empty() {
        return;
    }
    tracing::warn!(?over, message = STARTUP_OVER_BUDGET_MSG);
    crate::unified_log::warn(
        STARTUP_OVER_BUDGET_MSG,
        None,
        Some(serde_json::json!({ "over_budget": over })),
    );
}
/// Records the startup total, at most once per process.
/// A failure the user can retry records nothing, so the eventual success still counts.
pub(crate) fn report_total(outcome: StartupOutcome) {
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let timer = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).take();
    let auth_mode = timer
        .as_ref()
        .map(|t| t.auth_mode())
        .unwrap_or(AuthMode::Unknown);
    if let Some(timer) = &timer {
        timer.drain_sub_timers(outcome);
    }
    let total_ms = process_elapsed().as_millis() as u64;
    subphases().startup_total_ms = Some(total_ms);
    let phases = match &timer {
        Some(p) => {
            if outcome == StartupOutcome::Ok {
                p.close_open_phase();
            } else {
                warn_over_budget(&p.phase_snapshot());
            }
            p.close_root_span(outcome.label());
            p.summary()
        }
        None => String::new(),
    };
    let sub = *subphases();
    let event = crate::events::StartupCompleted {
        total_ms,
        outcome,
        phases,
        auth_mode,
        prefetch_wait_ms: sub.prefetch_wait_ms,
        session_load_ms: sub.session_load_ms,
        session_replay_ms: sub.session_replay_ms,
        session_git_scan_ms: sub.session_git_scan_ms,
        session_spawn_ms: sub.session_spawn_ms,
        init_process_ms: sub.init_process_ms,
        resolve_config_ms: sub.resolve_config_ms,
        remote_settings_ms: sub.remote_settings_ms,
        models_manager_ms: sub.models_manager_ms,
        managed_policy_auth_wait_ms: sub.managed_policy_auth_wait_ms,
        managed_policy_config_sync_ms: sub.managed_policy_config_sync_ms,
        time_to_first_frame_ms: sub.time_to_first_frame_ms,
    };
    if let Ok(record) = serde_json::to_value(&event) {
        crate::unified_log::info(STARTUP_COMPLETE_MSG, None, Some(record));
    }
    crate::session_ctx::log_event(event);
}
/// A deadline for a readiness-path network step.
/// Naming the phase and bounding the wait are one call, so neither can be forgotten.
pub struct ReadinessBudget {
    limit: Duration,
}
impl ReadinessBudget {
    pub const fn new(limit: Duration) -> Self {
        Self { limit }
    }
    /// Run `fut` under the budget, attributed to `phase` for exactly the run's duration.
    /// Returns `None` on timeout, after logging, instead of blocking readiness.
    pub async fn run<T>(
        &self,
        phase: StartupPhase,
        fut: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        let _scope = phase_scope(phase);
        match tokio::time::timeout(self.limit, fut).await {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(
                    phase = phase.label(),
                    limit_secs = self.limit.as_secs(),
                    "readiness step hit its budget"
                );
                crate::unified_log::warn(
                    "readiness step hit its budget",
                    None,
                    Some(
                        serde_json::json!({ "phase": phase.label(), "limit_secs": self.limit.as_secs() }),
                    ),
                );
                None
            }
        }
    }
}
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}
#[cfg(test)]
pub(crate) static SERIAL: Mutex<()> = Mutex::new(());
#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
