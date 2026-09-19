// Modified for Distill by Samuel Fajreldines, 2026.
//! Composition root for the session's extensions: each lives in its own submodule and installs itself here.

use std::rc::Rc;
use std::sync::Weak;

use distill_agent_lifecycle::LocalExtensionRegistry;
use distill_agent_lifecycle::LocalExtensionRegistryBuilder;

use super::*;

#[path = "extensions/idle_prompt.rs"]
mod idle_prompt;

/// An event that asks for the user's attention, delivered through the vendor-compatible `Notification` hook.
pub(super) struct NotificationEvent {
    pub(super) notification_type: &'static str,
    pub(super) message: Option<String>,
    pub(super) title: Option<String>,
    pub(super) level: Option<String>,
}

/// Sink that extensions emit [`NotificationEvent`]s through, fire and forget; the host owns delivery.
pub(super) trait NotificationEventSink {
    fn emit(&self, event: NotificationEvent);
}

/// The host's sink.
/// Holds the actor weakly, so extensions cannot keep a dead session alive; emitting on a gone session is a no-op.
struct SessionNotificationSink {
    session: Weak<SessionActor>,
}

impl NotificationEventSink for SessionNotificationSink {
    fn emit(&self, event: NotificationEvent) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        tokio::task::spawn_local(async move {
            session
                .dispatch_notification_hook(
                    event.notification_type,
                    event.message,
                    event.title,
                    event.level,
                )
                .await;
        });
    }
}

/// Frozen at session construction; registration order is dispatch order.
pub(super) fn session_extension_registry(actor: Weak<SessionActor>) -> LocalExtensionRegistry {
    let notification_event_sink = Rc::new(SessionNotificationSink { session: actor });
    let mut builder = LocalExtensionRegistryBuilder::new();
    idle_prompt::install(&mut builder, notification_event_sink);
    builder.build()
}
