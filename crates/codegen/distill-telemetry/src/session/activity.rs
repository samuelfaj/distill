//! Process-wide activity gauges. Each gauge registers itself the first time it
//! is entered, so [`ActivitySnapshot`] and [`work_is_idle`] enumerate the
//! registry rather than naming any gauge — a gauge defined in any crate is
//! picked up without this module referencing it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, Once, PoisonError};

/// Wire keys for the activity gauges; each domain crate builds its gauge from the
/// matching const, so a rename here propagates to the gauge, the snapshot, the
/// reserved-keys list, and every reader.
pub const SESSIONS_ACTIVE_KEY: &str = "sessions_active";
pub const SUBAGENTS_ACTIVE_KEY: &str = "subagents_active";
pub const COMPACTIONS_ACTIVE_KEY: &str = "compaction_active";
pub const MCP_SERVERS_CONNECTED_KEY: &str = "mcp_servers_connected";
pub const TURNS_ACTIVE_KEY: &str = "turns_active";
pub const WORKFLOW_RUNS_ACTIVE_KEY: &str = "workflow_runs_active";

static GAUGES: Mutex<Vec<&'static ActivityGauge>> = Mutex::new(Vec::new());

/// Whether a gauge's activity counts toward [`work_is_idle`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum GaugeKind {
    /// Turns, compactions, subagents, workflows.
    Work,
    /// Sessions, MCP — residency, so a quiet resident process still reads idle.
    Residency,
}

pub struct ActivityGauge {
    name: &'static str,
    value: AtomicU32,
    kind: GaugeKind,
    registered: Once,
}

impl ActivityGauge {
    pub const fn work(name: &'static str) -> Self {
        Self::new(name, GaugeKind::Work)
    }

    pub const fn residency(name: &'static str) -> Self {
        Self::new(name, GaugeKind::Residency)
    }

    const fn new(name: &'static str, kind: GaugeKind) -> Self {
        Self {
            name,
            value: AtomicU32::new(0),
            kind,
            registered: Once::new(),
        }
    }

    pub fn get(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    fn register(&'static self) {
        self.registered.call_once(|| {
            GAUGES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self);
        });
    }

    fn dec(&self) {
        let _ = self
            .value
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    pub fn enter(&'static self) -> ActivityGaugeGuard {
        self.register();
        self.value.fetch_add(1, Ordering::Relaxed);
        ActivityGaugeGuard { gauge: self }
    }
}

#[must_use]
pub struct ActivityGaugeGuard {
    gauge: &'static ActivityGauge,
}

impl Drop for ActivityGaugeGuard {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

pub(crate) static COMPACTIONS_ACTIVE: ActivityGauge = ActivityGauge::work(COMPACTIONS_ACTIVE_KEY);

pub fn gauge_value(name: &str) -> u32 {
    GAUGES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|gauge| gauge.name == name)
        .map_or(0, |gauge| gauge.get())
}

pub fn work_is_idle() -> bool {
    GAUGES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|gauge| gauge.kind == GaugeKind::Work)
        .all(|gauge| gauge.get() == 0)
}

#[derive(serde::Serialize)]
#[serde(transparent)]
pub(crate) struct ActivitySnapshot(std::collections::BTreeMap<&'static str, u32>);

impl ActivitySnapshot {
    pub(crate) fn read() -> Self {
        Self(
            GAUGES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|gauge| (gauge.name, gauge.get()))
                .collect(),
        )
    }
}
