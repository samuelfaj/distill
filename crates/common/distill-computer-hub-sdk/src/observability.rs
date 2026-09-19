// Modified for Distill by Samuel Fajreldines, 2026.
//! Server-side session event emitter.
//!
//! [`ObservabilityBridge`] is a thin facade for emitting session-level
//! events (turn lifecycle, phase changes) to the connected server. Tool-call events
//! (`ToolCallStarted` / `ToolCallCompleted`) are emitted automatically
//! by [`crate::harness::ToolHarness::call`] and do not need the bridge.
//!
//! The caller is responsible for also emitting to the local sink
//! (`EventTracker` in the shell, `EventProcPublisher` in the
//! chat service) — the bridge handles only the server leg.
//!
//! This separation is deliberate: each sampler's local sink has a
//! different type and API surface.  Forcing a trait/callback into the
//! bridge would add abstraction overhead without benefit, since the
//! call sites already have the local sink in scope.

use std::sync::Arc;

use distill_tool_protocol::{SessionId, session_event::SessionEvent};

use crate::harness::ToolHarness;

/// Emits [`SessionEvent`]s to the connected server as `ToolNotificationFrame` custom
/// notifications with `kind = "session_event"`.
///
/// No-ops gracefully when no harness is present (i.e. `harness` is
/// `None`).  Server notification failures are silently ignored — the bridge
/// is fire-and-forget so server issues never affect the sampler's main loop.
///
/// Callers MUST also emit to their local sink separately:
/// - Shell: `self.events.emit(Event::...)`
/// - Chat service: `publisher.publish_agent_event(...)`
pub struct ObservabilityBridge {
    harness: Option<Arc<ToolHarness>>,
    /// Retained for future payload enrichment and logging.
    session_id: SessionId,
}

impl ObservabilityBridge {
    pub fn new(harness: Option<Arc<ToolHarness>>, session_id: SessionId) -> Self {
        Self {
            harness,
            session_id,
        }
    }

    /// The session id this bridge was created for.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Whether a harness is present (i.e. server emission is active).
    pub fn has_harness(&self) -> bool {
        self.harness.is_some()
    }

    /// Emit a session event to the connected server.  No-ops if no harness is present.
    ///
    /// Delegates frame construction + wire dispatch to
    /// [`ToolHarness::emit_session_event`] so the SDK keeps a single
    /// canonical encoding path.
    ///
    /// Callers MUST also emit to their local sink separately:
    /// - Shell: `self.events.emit(Event::...)`
    /// - Chat service: `publisher.publish_agent_event(...)`
    pub async fn emit(&self, event: SessionEvent) {
        let event_type = match &event {
            SessionEvent::TurnStarted { .. } => "turn_started",
            SessionEvent::TurnEnded { .. } => "turn_ended",
            SessionEvent::ToolCallStarted { .. } => "tool_call_started",
            SessionEvent::ToolCallCompleted { .. } => "tool_call_completed",
            SessionEvent::PhaseChanged { .. } => "phase_changed",
            SessionEvent::Unknown => "unknown",
        };
        crate::metrics::session_event(event_type);
        if let Some(harness) = &self.harness {
            harness.emit_session_event(event).await;
        }
    }
}
