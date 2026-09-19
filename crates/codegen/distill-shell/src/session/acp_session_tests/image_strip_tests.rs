// Modified for Distill by Samuel Fajreldines, 2026.
//! Image-strip persistence policy (`acp_session_impl/image_strip.rs`).
//! Covers which `ImagesStripped` events may rewrite stored history, the deferred persist that
//! waits for the stripped retry to terminal (`Completed` or `Failed`), and the user notifications.

use std::sync::Arc;

use super::support::*;
use super::*;
use distill_sampler::{InferenceLatencyStats, RequestId, SamplingEvent, StripReason};
use distill_sampling_types::{ContentPart, ConversationItem, ConversationResponse};

const PERSIST_GATE_IMAGE_URI: &str = "data:image/png;base64,KEEPME";

fn user_with_image(url: &str) -> ConversationItem {
    let mut user = match ConversationItem::user("look at this") {
        ConversationItem::User(u) => u,
        _ => unreachable!(),
    };
    user.add_image(url);
    ConversationItem::User(user)
}

fn conversation_has_image(conv: &[ConversationItem], url: &str) -> bool {
    conv.iter().any(|item| match item {
        ConversationItem::User(u) => u
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Image { url: u } if u.as_ref() == url)),
        _ => false,
    })
}

async fn seed_image(actor: &SessionActor, url: &str) {
    actor
        .chat_state_handle
        .push_user_message(user_with_image(url));
    let conv = actor.chat_state_handle.get_conversation().await;
    assert!(
        conversation_has_image(&conv, url),
        "precondition: seeded image must be in chat-state"
    );
}

/// Drain the gateway channel into debug strings for notification assertions.
fn drain_gateway_debug(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> String {
    let mut out = String::new();
    while let Ok(msg) = rx.try_recv() {
        out.push_str(&format!("{msg:?}\n"));
    }
    out
}

/// Several callers assert that a notification never arrives; that needs a window, not a completion signal.
/// Yield to the LocalSet for a wall-clock bound.
async fn settle() {
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Wait until the stored conversation satisfies `cond`, bounded by wall clock.
/// On timeout returns the last-read conversation so the caller's assertion fails showing the real state.
async fn wait_for_conversation(
    actor: &SessionActor,
    cond: impl Fn(&[ConversationItem]) -> bool,
) -> Vec<ConversationItem> {
    let poll = async {
        loop {
            let conv = actor.chat_state_handle.get_conversation().await;
            if cond(&conv) {
                return conv;
            }
            tokio::task::yield_now().await;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), poll).await {
        Ok(conv) => conv,
        Err(_) => actor.chat_state_handle.get_conversation().await,
    }
}

fn own_request(actor: &SessionActor, request_id: &RequestId) {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    actor.turn_stream_drained.lock().insert(
        request_id.clone(),
        crate::session::acp_session::StreamOwnership::with_waiter(Some(tx)),
    );
}

fn completed_event(request_id: &RequestId) -> SamplingEvent {
    SamplingEvent::Completed {
        request_id: request_id.clone(),
        response: Box::new(ConversationResponse {
            items: vec![ConversationItem::assistant("recovered")],
            stop_reason: None,
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 1,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        }),
        metrics: InferenceLatencyStats::default(),
    }
}

fn failed_info() -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        kind: distill_sampler::SamplingErrorKind::Api,
        message: "400 Bad Request".to_string(),
        status_code: Some(400),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: distill_sampling_types::SentCredential::Unknown,
    }
}

fn images_stripped(request_id: &RequestId, urls: &[&str], reason: StripReason) -> SamplingEvent {
    SamplingEvent::ImagesStripped {
        request_id: request_id.clone(),
        stripped_urls: urls.iter().map(|u| Arc::<str>::from(*u)).collect(),
        reason,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn heuristic_images_stripped_does_not_rewrite_history() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;
            let rid = RequestId::from("req-heuristic");
            own_request(&actor, &rid);

            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::PayloadHeuristic,
                ))
                .await;
            actor.handle_sampling_event(completed_event(&rid)).await;
            settle().await;

            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "PayloadHeuristic must stay request-local even after Completed: {conv:?}"
            );
            let sent = drain_gateway_debug(&mut gateway_rx);
            assert!(
                sent.contains("left out of the retry"),
                "request-local strip must tell the user with the retry wording, sent: {sent}"
            );
        })
        .await;
}

/// The durable path: a server-confirmed single-image strip is buffered on `ImagesStripped` (history untouched).
/// It is persisted when the stripped retry's `Completed` proves it helped, and the user is told only then.
#[tokio::test(flavor = "current_thread")]
async fn server_rejected_strip_persists_only_after_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;
            let rid = RequestId::from("req-rejected");
            own_request(&actor, &rid);

            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "deletion must wait for the stripped retry to succeed: {conv:?}"
            );
            assert!(
                !drain_gateway_debug(&mut gateway_rx).contains("removed from the conversation"),
                "no durable-removal note before the retry succeeds"
            );

            // A Completed for a DIFFERENT request must not consume the buffer.
            actor
                .handle_sampling_event(completed_event(&RequestId::from("req-unrelated")))
                .await;
            settle().await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "a mismatched request id must leave the buffer intact: {conv:?}"
            );

            actor.handle_sampling_event(completed_event(&rid)).await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "Completed must persist the strip before the handler returns: {conv:?}"
            );
            let sent = drain_gateway_debug(&mut gateway_rx);
            assert!(
                sent.contains("removed from the conversation"),
                "persisted strip must tell the user it is permanent, sent: {sent}"
            );
        })
        .await;
}

/// A drain timeout retains request-scoped strip state across turn boundaries.
/// The late completion must still persist its own buffered strip without consuming or mutating the newer request's ownership.
#[tokio::test(flavor = "current_thread")]
async fn timed_out_strip_survives_new_turn_until_late_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            let timed_out = RequestId::from("req-timeout-strip");
            own_request(&actor, &timed_out);
            actor
                .handle_sampling_event(images_stripped(
                    &timed_out,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor
                .turn_stream_drained
                .lock()
                .get_mut(&timed_out)
                .expect("timed-out request remains owned")
                .waiter
                .take();

            // The next turn keeps only timeout-owned durable work, then clears ordinary stream ownership before registering its own request
            actor.retain_timed_out_image_strips_for_new_turn();
            actor.turn_stream_drained.lock().clear();
            // A second turn boundary must not erase a strip already marked as timeout-owned while its terminal event is still in flight
            actor.retain_timed_out_image_strips_for_new_turn();
            let newer = RequestId::from("req-newer-turn");
            own_request(&actor, &newer);

            assert!(
                actor.pending_image_strip.lock().contains_key(&timed_out),
                "new-turn cleanup must retain the timed-out request's strip"
            );
            {
                let mut capture = actor.streaming_turn_capture.lock();
                capture.begin_turn(Some("newer-prompt".to_string()), 2);
                capture.start_request_stream(timed_out.as_str(), 1);
                capture.append(true, "retained partial reasoning");
            }
            actor
                .handle_sampling_event(completed_event(&timed_out))
                .await;

            let conv = wait_for_conversation(&actor, |conv| {
                !conversation_has_image(conv, PERSIST_GATE_IMAGE_URI)
            })
            .await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "late Completed must persist the timed-out request's strip: {conv:?}"
            );
            assert!(
                actor.turn_stream_drained.lock().contains_key(&newer),
                "late Completed must not consume the newer request's ownership"
            );
            assert_eq!(
                actor.streaming_turn_capture.lock().reasoning_text,
                "retained partial reasoning",
                "strip-only completion must not clear capture after stream ownership was revoked"
            );
            assert!(
                actor.pending_image_strip.lock().is_empty(),
                "late Completed must consume only its request-scoped pending strip"
            );
        })
        .await;
}

/// Deliver `Completed` the way the sampler drainer does: on its own local task, so the caller can hold
/// the strip gate and observe the persist blocked on it without deadlocking the single-threaded runtime.
fn deliver_completed(
    actor: &Arc<SessionActor>,
    request_id: &RequestId,
) -> tokio::task::JoinHandle<()> {
    let actor = Arc::clone(actor);
    let request_id = request_id.clone();
    tokio::task::spawn_local(async move {
        actor
            .handle_sampling_event(completed_event(&request_id))
            .await;
    })
}

/// Rewind can commit after `Completed` arrives but before its persist acquires the strip gate.
/// The persist must acquire rewrite ownership before claiming URLs.
/// A waiting successful rewind then clears queued work while preserving the restored image and emitting no stale note.
#[tokio::test(flavor = "current_thread")]
async fn rewind_cancels_detached_image_strip_before_it_runs() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            let timed_out = RequestId::from("req-rewind-detached-strip");
            own_request(&actor, &timed_out);
            actor
                .handle_sampling_event(images_stripped(
                    &timed_out,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor
                .turn_stream_drained
                .lock()
                .get_mut(&timed_out)
                .expect("timed-out request remains owned")
                .waiter
                .take();
            actor.retain_timed_out_image_strips_for_new_turn();
            actor.turn_stream_drained.lock().clear();

            let mut snapshot = actor
                .chat_state_handle
                .snapshot()
                .await
                .expect("snapshot available");
            snapshot.prompt_index = 2;
            snapshot.prompt_texts = vec!["image turn".into(), "later turn".into()];
            let Some(ConversationItem::User(image_turn)) = snapshot.conversation.first_mut() else {
                panic!("seeded image must be a user turn");
            };
            image_turn.prompt_index = Some(0);
            snapshot
                .conversation
                .push(ConversationItem::assistant("image answer"));
            let mut later_turn = match ConversationItem::user("later turn") {
                ConversationItem::User(user) => user,
                _ => unreachable!(),
            };
            later_turn.prompt_index = Some(1);
            snapshot
                .conversation
                .push(ConversationItem::User(later_turn));
            snapshot
                .conversation
                .push(ConversationItem::assistant("later answer"));
            actor.chat_state_handle.restore_snapshot(snapshot);
            let _ = actor.chat_state_handle.get_conversation().await;

            let strip_blocker = actor.image_strip_rewrite_barrier.lock_strip().await;
            let completed = deliver_completed(&actor, &timed_out);
            tokio::task::yield_now().await;
            assert!(
                actor
                    .pending_image_strip
                    .lock()
                    .get(&timed_out)
                    .is_some_and(|strip| !strip.applying && !strip.urls.is_empty()),
                "Completed must not claim URLs before its persist owns the gate"
            );

            let rewind_actor = Arc::clone(&actor);
            let rewind = tokio::task::spawn_local(async move {
                rewind_actor
                    .handle_rewind(RewindRequest {
                        target_prompt_index: 1,
                        force: true,
                        mode: RewindMode::ConversationOnly,
                    })
                    .await
            });
            tokio::task::yield_now().await;
            assert!(
                actor
                    .pending_image_strip
                    .lock()
                    .get(&timed_out)
                    .is_some_and(|strip| !strip.applying && !strip.urls.is_empty()),
                "rewind preflight must leave queued ownership untouched while waiting for the gate"
            );
            drop(strip_blocker);

            let rewind = rewind
                .await
                .expect("rewind task completes")
                .expect("rewind succeeds");
            assert!(rewind.success, "rewind should commit: {rewind:?}");
            completed.await.expect("Completed handler finishes");

            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "rewind invalidation must keep the restored image: {conv:?}"
            );
            assert!(actor.pending_image_strip.lock().is_empty());
            assert!(
                !drain_gateway_debug(&mut gateway_rx).contains("removed from the conversation"),
                "cancelled detached persistence must not emit a stale durable-removal note"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_rewind_preserves_queued_image_strip() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            let request_id = RequestId::from("req-rejected-rewind-strip");
            own_request(&actor, &request_id);
            actor
                .handle_sampling_event(images_stripped(
                    &request_id,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;

            let strip_blocker = actor.image_strip_rewrite_barrier.lock_strip().await;
            let rewind_actor = Arc::clone(&actor);
            let rewind = tokio::task::spawn_local(async move {
                rewind_actor
                    .handle_rewind(RewindRequest {
                        target_prompt_index: usize::MAX,
                        force: true,
                        mode: RewindMode::ConversationOnly,
                    })
                    .await
            });
            tokio::task::yield_now().await;

            let completed = deliver_completed(&actor, &request_id);
            tokio::task::yield_now().await;
            assert!(
                actor
                    .pending_image_strip
                    .lock()
                    .get(&request_id)
                    .is_some_and(|strip| !strip.applying && !strip.urls.is_empty()),
                "rejected rewind preflight must not revoke queued strip ownership"
            );
            drop(strip_blocker);

            let rewind = rewind
                .await
                .expect("rewind task completes")
                .expect("rewind returns a response");
            assert!(
                !rewind.success,
                "invalid rewind must be rejected: {rewind:?}"
            );
            completed.await.expect("Completed handler finishes");

            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "strip must resume after rejected rewind: {conv:?}"
            );
            assert!(actor.pending_image_strip.lock().is_empty());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_compaction_replay_preserves_queued_image_strip() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            actor.session_info.id = acp::SessionId::new(format!("strip-replay-fail-{unique}"));
            let actor = Arc::new(actor);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            let mut snapshot = actor
                .chat_state_handle
                .snapshot()
                .await
                .expect("snapshot available");
            snapshot.prompt_index = 2;
            snapshot.prompt_texts = vec!["image turn".into(), "later turn".into()];
            snapshot.last_compaction_prompt_index = Some(1);
            actor.chat_state_handle.restore_snapshot(snapshot);

            let session_dir = crate::session::persistence::session_dir(&actor.session_info);
            std::fs::create_dir_all(&session_dir).expect("create session dir");
            let checkpoint = crate::session::storage::SessionUpdate::Xai(Box::new(
                crate::extensions::notification::SessionNotification {
                    session_id: actor.session_info.id.clone(),
                    update: XaiSessionUpdate::CompactionCheckpoint(Box::new(
                        crate::extensions::notification::CompactionCheckpointInfo {
                            checkpoint_id: "missing".into(),
                            prompt_index_at_compaction: 1,
                            checkpoint_file: "compaction_checkpoints/missing.json".into(),
                            auto_continue: None,
                            schema_version: 1,
                            created_at: "2026-01-01T00:00:00Z".into(),
                        },
                    )),
                    meta: None,
                },
            ));
            let envelope = crate::session::storage::SessionUpdateEnvelope::from_update(&checkpoint)
                .expect("serialize checkpoint update");
            std::fs::write(
                session_dir.join("updates.jsonl"),
                format!(
                    "{}\n",
                    serde_json::to_string(&envelope).expect("serialize envelope")
                ),
            )
            .expect("write updates fixture");

            let request_id = RequestId::from("req-failed-replay-strip");
            own_request(&actor, &request_id);
            actor
                .handle_sampling_event(images_stripped(
                    &request_id,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;

            let strip_blocker = actor.image_strip_rewrite_barrier.lock_strip().await;
            let completed = deliver_completed(&actor, &request_id);
            tokio::task::yield_now().await;
            let rewind_actor = Arc::clone(&actor);
            let rewind = tokio::task::spawn_local(async move {
                rewind_actor
                    .handle_rewind(RewindRequest {
                        target_prompt_index: 1,
                        force: true,
                        mode: RewindMode::ConversationOnly,
                    })
                    .await
            });
            tokio::task::yield_now().await;
            assert!(
                actor
                    .pending_image_strip
                    .lock()
                    .get(&request_id)
                    .is_some_and(|strip| !strip.applying && !strip.urls.is_empty()),
                "failed replay preflight must not revoke queued strip ownership"
            );
            drop(strip_blocker);

            let rewind = rewind
                .await
                .expect("rewind task completes")
                .expect("rewind returns a response");
            assert!(!rewind.success, "missing checkpoint must reject rewind");
            completed.await.expect("Completed handler finishes");

            let conv = actor.chat_state_handle.get_conversation().await;
            let _ = std::fs::remove_dir_all(&session_dir);
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "strip must resume after replay failure: {conv:?}"
            );
            assert!(actor.pending_image_strip.lock().is_empty());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pending_strip_bound_preserves_detached_and_new_url_entries() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            let applying_id = RequestId::from("req-applying-at-bound");
            own_request(&actor, &applying_id);
            actor
                .handle_sampling_event(images_stripped(
                    &applying_id,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;

            let strip_blocker = actor.image_strip_rewrite_barrier.lock_strip().await;
            let completed = deliver_completed(&actor, &applying_id);
            tokio::task::yield_now().await;
            {
                let mut pending = actor.pending_image_strip.lock();
                for index in 0..16 {
                    pending.insert(
                        RequestId::from(format!("timed-out-{index}")),
                        PendingImageStrip {
                            urls: Vec::new(),
                            timed_out: true,
                            applying: false,
                        },
                    );
                }
            }
            let queued_id = RequestId::from("req-url-at-bound");
            own_request(&actor, &queued_id);
            actor
                .handle_sampling_event(images_stripped(
                    &queued_id,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            {
                let pending = actor.pending_image_strip.lock();
                assert!(
                    pending
                        .get(&applying_id)
                        .is_some_and(|strip| !strip.applying && !strip.urls.is_empty()),
                    "bound enforcement must retain detached work waiting for the rewrite gate"
                );
                assert!(
                    pending
                        .get(&queued_id)
                        .is_some_and(|strip| !strip.urls.is_empty()),
                    "bound enforcement must prioritize queued URL-bearing work over placeholders"
                );
                assert_eq!(16, pending.len());
            }
            drop(strip_blocker);
            completed.await.expect("Completed handler finishes");

            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "retained write must finish after acquiring the gate: {conv:?}"
            );
            assert!(!actor.pending_image_strip.lock().contains_key(&applying_id));
        })
        .await;
}

/// A timeout can happen before the ordered event drainer reaches `ImagesStripped`.
/// The timeout placeholder must admit that late event and its following `Completed`, while still keeping all other late events stale.
#[tokio::test(flavor = "current_thread")]
async fn timed_out_strip_survives_when_images_stripped_is_still_queued() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;

            {
                let mut pending = actor.pending_image_strip.lock();
                for index in 0..16 {
                    pending.insert(
                        RequestId::from(format!("url-pressure-{index}")),
                        PendingImageStrip {
                            urls: vec![PERSIST_GATE_IMAGE_URI.into()],
                            timed_out: false,
                            applying: false,
                        },
                    );
                }
            }
            let timed_out = RequestId::from("req-timeout-before-strip-event");
            own_request(&actor, &timed_out);
            actor.mark_stream_drain_timed_out(&timed_out);
            {
                let pending = actor.pending_image_strip.lock();
                assert_eq!(16, pending.len());
                assert!(
                    pending
                        .get(&timed_out)
                        .is_some_and(|strip| strip.timed_out && strip.urls.is_empty()),
                    "the just-timed-out placeholder must displace older URL work under pressure"
                );
            }
            actor.cancel_active_sampling_requests();
            assert!(
                actor
                    .pending_image_strip
                    .lock()
                    .get(&timed_out)
                    .is_some_and(|strip| strip.timed_out && strip.urls.is_empty()),
                "timeout must retain a placeholder before cancellation clears stream ownership"
            );

            actor
                .handle_sampling_event(images_stripped(
                    &timed_out,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor
                .handle_sampling_event(completed_event(&timed_out))
                .await;

            let conv = wait_for_conversation(&actor, |conv| {
                !conversation_has_image(conv, PERSIST_GATE_IMAGE_URI)
            })
            .await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "queued strip and completion must resolve timeout-owned work: {conv:?}"
            );
            assert!(actor.pending_image_strip.lock().is_empty());
        })
        .await;
}

/// A strip that does not reach `Applied` must still tell the user the answer was produced without the image.
/// It just must not claim the stored conversation changed.
#[tokio::test(flavor = "current_thread")]
async fn non_applied_strip_outcome_still_notifies_the_user() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            // Nothing seeded: the buffered URL matches no stored image, so the apply resolves as `NoMatch` rather than `Applied`
            let rid = RequestId::from("req-no-match");
            own_request(&actor, &rid);

            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor.handle_sampling_event(completed_event(&rid)).await;
            settle().await;

            let sent = drain_gateway_debug(&mut gateway_rx);
            assert!(
                sent.contains("left out of"),
                "a non-Applied outcome must still tell the user, sent: {sent}"
            );
            assert!(
                !sent.contains("removed from the conversation"),
                "only Applied may claim the stored conversation changed, sent: {sent}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_rejected_strip_persists_when_retry_fails() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;
            let rid = RequestId::from("req-rejected-then-fatal");
            own_request(&actor, &rid);

            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor
                .handle_sampling_event(SamplingEvent::Failed {
                    request_id: rid.clone(),
                    error: failed_info(),
                })
                .await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "Failed after ServerRejected must persist the strip before the handler returns: {conv:?}"
            );
            let sent = drain_gateway_debug(&mut gateway_rx);
            assert!(
                sent.contains("removed from the conversation"),
                "persisted Failed strip must tell the user it is permanent, sent: {sent}"
            );

            actor.handle_sampling_event(completed_event(&rid)).await;
            settle().await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "a later Completed must not restore the stripped image: {conv:?}"
            );
        })
        .await;
}

/// Blame is judged on unique URLs: two DISTINCT stripped images are ambiguous and stay request-local.
/// The same image stored twice is one suspect and persists (both occurrences).
#[tokio::test(flavor = "current_thread")]
async fn multi_image_blame_is_judged_on_unique_urls() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            let second_uri = "data:image/png;base64,c2Vjb25kLWltYWdl";
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;
            seed_image(&actor, second_uri).await;

            // Two distinct URLs: ambiguous, never persists.
            let rid = RequestId::from("req-ambiguous");
            own_request(&actor, &rid);
            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI, second_uri],
                    StripReason::ServerRejected,
                ))
                .await;
            actor.handle_sampling_event(completed_event(&rid)).await;
            settle().await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI)
                    && conversation_has_image(&conv, second_uri),
                "ambiguous blame must not delete stored images: {conv:?}"
            );

            // The same URL twice (attached in two turns): one suspect, persists, removing both stored occurrences
            seed_image(&actor, PERSIST_GATE_IMAGE_URI).await;
            let rid = RequestId::from("req-duplicate");
            own_request(&actor, &rid);
            actor
                .handle_sampling_event(images_stripped(
                    &rid,
                    &[PERSIST_GATE_IMAGE_URI, PERSIST_GATE_IMAGE_URI],
                    StripReason::ServerRejected,
                ))
                .await;
            actor.handle_sampling_event(completed_event(&rid)).await;
            let conv = wait_for_conversation(&actor, |conv| {
                !conversation_has_image(conv, PERSIST_GATE_IMAGE_URI)
            })
            .await;
            assert!(
                !conversation_has_image(&conv, PERSIST_GATE_IMAGE_URI),
                "a single unique URL is unambiguous blame: {conv:?}"
            );
            assert!(
                conversation_has_image(&conv, second_uri),
                "the unrelated image must survive: {conv:?}"
            );
        })
        .await;
}
