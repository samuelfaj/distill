// Modified for Distill by Samuel Fajreldines, 2026.
//! Shell policy for sampler image strips: which strips may rewrite stored history, when, and what the user is told.
//!
//! - Only `ServerRejected` with unambiguous blame (exactly one unique URL in the rejected request) may touch history.
//!   The server's verdict names the request, not an image.
//! - The rewrite waits for that request to terminal (`Completed` or `Failed`). The write is awaited
//!   before the drain barrier releases so the next prompt cannot reread the image. Heuristic strips stay request-local.
//! - The write is gated on a backup and acknowledged from disk ([`StripOutcome`]); only `Applied` claims the stored conversation changed.
//! - Scope: `chat_history.jsonl` only.
//!   A rebuild replaying `updates.jsonl` (e.g. a remote pull) restores the image and pays one more strip cycle.

use distill_chat_state::StripOutcome;
use distill_sampler::{RequestId, StripReason};

use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
use crate::session::acp_session::{PendingImageStrip, SessionActor};

const MAX_PENDING_IMAGE_STRIPS: usize = 16;

fn enforce_pending_image_strip_bound(
    pending: &mut std::collections::HashMap<RequestId, PendingImageStrip>,
    preferred: Option<&RequestId>,
) {
    if pending.len() <= MAX_PENDING_IMAGE_STRIPS {
        return;
    }
    let applying = pending.values().filter(|strip| strip.applying).count();
    let preferred_slots =
        usize::from(preferred.is_some_and(|request_id| {
            pending.get(request_id).is_some_and(|strip| !strip.applying)
        }));
    let remaining = MAX_PENDING_IMAGE_STRIPS
        .saturating_sub(applying)
        .saturating_sub(preferred_slots);
    let other_url_limit = pending
        .iter()
        .filter(|(request_id, strip)| {
            !strip.applying
                && !strip.urls.is_empty()
                && preferred.is_none_or(|preferred| preferred != *request_id)
        })
        .count()
        .min(remaining);
    let other_placeholder_limit = remaining.saturating_sub(other_url_limit);
    let mut retained_urls = 0usize;
    let mut retained_placeholders = 0usize;
    let before = pending.len();
    pending.retain(|request_id, strip| {
        strip.applying
            || preferred == Some(request_id)
            || if strip.urls.is_empty() {
                let retain = retained_placeholders < other_placeholder_limit;
                retained_placeholders += usize::from(retain);
                retain
            } else {
                let retain = retained_urls < other_url_limit;
                retained_urls += usize::from(retain);
                retain
            }
    });
    tracing::warn!(
        maximum = MAX_PENDING_IMAGE_STRIPS,
        applying,
        dropped = before.saturating_sub(pending.len()),
        "dropping excess timed-out image strips after pending-state bound was exceeded"
    );
}

impl SessionActor {
    pub(crate) fn should_defer_image_strip(
        stripped_urls: &[std::sync::Arc<str>],
        reason: &StripReason,
    ) -> bool {
        let Some(first) = stripped_urls.first() else {
            return false;
        };
        reason == &StripReason::ServerRejected
            && stripped_urls
                .iter()
                .all(|url| url.as_ref() == first.as_ref())
    }

    /// Drop abandoned strips at a turn boundary while retaining timed-out requests whose terminal event still owes one request-scoped side effect.
    pub(crate) fn retain_timed_out_image_strips_for_new_turn(&self) {
        let ownership = self.turn_stream_drained.lock();
        let mut pending = self.pending_image_strip.lock();
        pending.retain(|request_id, strip| match ownership.get(request_id) {
            Some(o) if o.waiter.is_none() => {
                strip.timed_out = true;
                true
            }
            Some(_) => false,
            None => strip.timed_out || strip.applying,
        });
        for request_id in ownership
            .iter()
            .filter_map(|(request_id, o)| o.waiter.is_none().then_some(request_id))
        {
            pending
                .entry(request_id.clone())
                .or_insert_with(|| PendingImageStrip {
                    urls: Vec::new(),
                    timed_out: true,
                    applying: false,
                });
            enforce_pending_image_strip_bound(&mut pending, Some(request_id));
        }
    }

    /// Drop the ordering waiter while retaining request-scoped strip ownership.
    pub(crate) fn mark_stream_drain_timed_out(&self, request_id: &RequestId) {
        let mut ownership = self.turn_stream_drained.lock();
        let Some(o) = ownership.get_mut(request_id) else {
            return;
        };
        o.waiter.take();
        let mut pending = self.pending_image_strip.lock();
        pending
            .entry(request_id.clone())
            .or_insert_with(|| PendingImageStrip {
                urls: Vec::new(),
                timed_out: true,
                applying: false,
            })
            .timed_out = true;
        enforce_pending_image_strip_bound(&mut pending, Some(request_id));
    }

    /// Relinquish normal stream ownership immediately when cancellation claims a turn.
    /// Retain only work still owned by a timeout from older turns; late events for the cancelled request are otherwise stale.
    pub(crate) fn cancel_active_sampling_requests(&self) {
        self.close_stream_apply_span_any();
        self.turn_stream_drained.lock().clear();
        self.pending_image_strip
            .lock()
            .retain(|_, strip| strip.timed_out || strip.applying);
    }

    /// Invalidate queued strip work synchronously when rewind claims history.
    pub(crate) fn cancel_pending_image_strips_for_rewind(&self) {
        self.pending_image_strip.lock().clear();
    }

    /// Wait for any active strip write before validating and restoring history.
    pub(crate) async fn prepare_image_strips_for_rewind(
        &self,
    ) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.image_strip_rewrite_barrier.lock_rewind().await
    }

    /// Handle `SamplingEvent::ImagesStripped`: buffer a persistable strip for [`Self::apply_pending_image_strip`].
    /// A request-local strip instead notifies the user immediately.
    pub(crate) async fn handle_images_stripped(
        &self,
        request_id: RequestId,
        stripped_urls: Vec<std::sync::Arc<str>>,
        reason: StripReason,
    ) {
        let stripped = stripped_urls.len();
        // Blame is judged on unique URLs: the same image attached twice is still one suspect
        // Distinct images are ambiguous, so the strip stays request-local
        let persist_deferred = Self::should_defer_image_strip(&stripped_urls, &reason);
        let mut unique = stripped_urls;
        unique.sort();
        unique.dedup();
        if persist_deferred {
            let mut pending = self.pending_image_strip.lock();
            let timed_out = pending
                .get(&request_id)
                .is_some_and(|strip| strip.timed_out);
            pending.insert(
                request_id.clone(),
                PendingImageStrip {
                    urls: unique,
                    timed_out,
                    applying: false,
                },
            );
            enforce_pending_image_strip_bound(&mut pending, Some(&request_id));
        }
        distill_telemetry::unified_log::warn(
            "shell.turn.images_stripped",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "sampler_request_id": request_id.as_str(),
                "stripped": stripped,
                "reason": reason.as_ref(),
                "persist_deferred": persist_deferred,
            })),
        );
        if !persist_deferred {
            // Request-local only: tell the user now, on the same channel as load-time image drops, rendered as a system scrollback note
            self.send_xai_notification(XaiSessionUpdate::ImageDropped {
                notes: vec![format!(
                    "This request failed over its images (or was too large); \
                     {stripped} image(s) were left out of the retry."
                )],
            })
            .await;
        }
    }

    /// Persist a buffered `ServerRejected` strip once the stripped retry terminals (`Completed` or `Failed`).
    pub(crate) async fn apply_pending_image_strip(&self, request_id: &RequestId) {
        // Acquire rewrite ownership before claiming URLs
        // Rewind either clears queued work first, or waits until this proven strip finishes
        let _rewrite_guard = self.image_strip_rewrite_barrier.lock_strip().await;
        let urls = {
            let mut pending = self.pending_image_strip.lock();
            let Some(strip) = pending.get_mut(request_id) else {
                return;
            };
            if strip.urls.is_empty() {
                if !strip.applying {
                    pending.remove(request_id);
                }
                return;
            }
            strip.applying = true;
            std::mem::take(&mut strip.urls)
        };
        let outcome = self.chat_state_handle.strip_conversation_images(urls).await;
        let still_owned = self
            .pending_image_strip
            .lock()
            .remove(request_id)
            .is_some_and(|strip| strip.applying);
        if !still_owned {
            return;
        }
        let (outcome_label, persisted) = match outcome {
            StripOutcome::Applied { stripped } => ("applied", stripped),
            StripOutcome::NoMatch => ("no_match", 0),
            StripOutcome::WriteFailed { .. } => ("write_failed", 0),
            StripOutcome::ActorUnavailable => ("actor_unavailable", 0),
        };
        distill_telemetry::unified_log::warn(
            "shell.turn.images_strip_persisted",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "sampler_request_id": request_id.as_str(),
                "outcome": outcome_label,
                "persisted": persisted,
            })),
        );
        // Every outcome answered without the user's image, so every outcome says so
        // Only `Applied` may also claim the stored conversation changed (a failed or missed write leaves the image on disk)
        let notes = match outcome {
            StripOutcome::Applied { .. } => vec![
                "The server could not process an image, so it was removed from \
                 the conversation. Re-attach it if it is still needed."
                    .to_string(),
            ],
            StripOutcome::NoMatch
            | StripOutcome::WriteFailed { .. }
            | StripOutcome::ActorUnavailable => vec![
                "The server could not process an image, so it was left out of \
                 this request."
                    .to_string(),
            ],
        };
        self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes })
            .await;
    }
}
