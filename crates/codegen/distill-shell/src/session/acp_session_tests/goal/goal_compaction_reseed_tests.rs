// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use crate::session::helpers::compaction_context::{CompactionInputs, CompactionStateContext};
use distill_chat_state::compaction_utils::{
    CompactedHistoryInput, build_compacted_history, wrap_user_query,
};
use distill_sampling_types::{ConversationItem, SyntheticReason};
use std::sync::Arc as StdArc;
use tempfile::TempDir;

const DEVICE_TEST_OBJECTIVE: &str = "ssh to root@example.test and test that a genbw meter profile does NOT produce aggregate meter data on dnp3 or modbus";
const STALE_CODE_REVIEW: &str = "please review the PR for config-json-go";

async fn make_goal_actor() -> (StdArc<SessionActor>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_tracker = StdArc::new(parking_lot::Mutex::new(
        crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
    ));
    (StdArc::new(actor), tmp)
}

fn start_device_test_goal(actor: &SessionActor) {
    actor.goal_tracker.lock().create_goal(
        "g-device-test".into(),
        DEVICE_TEST_OBJECTIVE.into(),
        None,
        0,
        "2026-08-06T00:00:00Z".into(),
        None,
    );
}

fn stale_pre_goal_conversation() -> Vec<ConversationItem> {
    vec![
        ConversationItem::system("You are Grok."),
        ConversationItem::user(format!(
            "<user_info>OS: linux</user_info>\n\n<user_query>\n{STALE_CODE_REVIEW}\n</user_query>"
        )),
        ConversationItem::assistant("I'll start the branch code review."),
        ConversationItem::system_reminder(format!(
            "A goal has been set: {DEVICE_TEST_OBJECTIVE}\nStart now."
        )),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn last_user_query_seeds_from_active_goal_not_stale_pre_goal_prompt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            let conversation = stale_pre_goal_conversation();

            let without_goal =
                CompactionStateContext::build(&conversation, CompactionInputs::default()).await;
            assert_eq!(
                without_goal.last_user_query.as_deref(),
                Some(STALE_CODE_REVIEW),
                "without the goal field, compact would revive the pre-goal code review"
            );

            let with_goal = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                with_goal.last_user_query.as_deref(),
                Some(DEVICE_TEST_OBJECTIVE),
                "active goal must seed last_user_query from GoalTracker.objective"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_goal_section_is_continuation_not_create_style_rules() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);

            let section = actor
                .compaction_goal_section()
                .await
                .expect("active goal must produce a compaction reminder section");
            assert!(
                section.contains(DEVICE_TEST_OBJECTIVE),
                "reminder must pin the live objective:\n{section}"
            );
            assert!(
                section.contains(GOAL_CONTINUATION_SENTINEL),
                "reminder must be continuation-style, not create-style rules:\n{section}"
            );
            assert!(
                !section.contains("A goal has been set"),
                "create-style goal_rules must not be the post-compact cue:\n{section}"
            );
            assert!(
                section.contains("Do not restart this goal"),
                "reminder must tell the model not to restart:\n{section}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn summarizer_user_context_pins_objective() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);

            let ctx = actor
                .merge_goal_compaction_user_context(None)
                .expect("active goal must produce summarizer context");
            assert!(ctx.contains(DEVICE_TEST_OBJECTIVE), "{ctx}");
            assert!(ctx.contains("Do not restart this goal"), "{ctx}");

            let merged = actor
                .merge_goal_compaction_user_context(Some("keep auth".into()))
                .expect("merge keeps caller text");
            assert!(merged.contains("keep auth"), "{merged}");
            assert!(merged.contains(DEVICE_TEST_OBJECTIVE), "{merged}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_compact_history_keeps_objective_and_goal_summary_continuation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            let conversation = stale_pre_goal_conversation();
            actor
                .chat_state_handle
                .replace_conversation(conversation.clone());
            let _ = actor.chat_state_handle.get_conversation().await;

            let state_context = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await
            .for_compaction();
            assert_eq!(
                state_context.last_user_query.as_deref(),
                Some(DEVICE_TEST_OBJECTIVE)
            );

            let goal_section = actor
                .compaction_goal_section()
                .await
                .expect("active goal reminder");
            let system_reminder = format!(
                "<system-reminder>\n## Files Edited This Session\n- src/auth.rs\n\n{goal_section}\n</system-reminder>"
            );

            let compacted = build_compacted_history(CompactedHistoryInput {
                system_message: ConversationItem::system("You are Grok."),
                user_message_prefix: "<user_info>OS: linux</user_info>".into(),
                agents_md_reminder: None,
                state_context: &state_context,
                compaction_summary: "<summary>\n1. Primary Request and Intent: code review\n</summary>"
                    .into(),
                system_reminder: Some(system_reminder),
                summary_before_recent: false,
                transcript_hint: None,
                summary_count: 1,
            });

            let query_texts: Vec<String> = compacted
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) if u.synthetic_reason.is_human() => {
                        Some(item.text_content())
                    }
                    _ => None,
                })
                .collect();
            assert!(
                query_texts.iter().any(|t| t.contains(&wrap_user_query(DEVICE_TEST_OBJECTIVE))),
                "compacted <user_query> must be the goal objective, not the code review:\n{query_texts:?}"
            );
            assert!(
                query_texts.iter().all(|t| !t.contains(STALE_CODE_REVIEW)),
                "stale pre-goal prompt must not occupy last_user_query:\n{query_texts:?}"
            );

            actor
                .chat_state_handle
                .replace_conversation_for_compaction(compacted);
            let _ = actor.chat_state_handle.get_conversation().await;
            actor.reseed_active_goal_after_compaction().await;

            let after = actor.chat_state_handle.get_conversation().await;
            let goal_summaries: Vec<&ConversationItem> = after
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    )
                })
                .collect();
            assert_eq!(
                goal_summaries.len(),
                1,
                "exactly one GoalSummary continuation after compact: {after:?}"
            );
            let directive = goal_summaries
                .first()
                .expect("len==1")
                .text_content();
            assert!(
                directive.contains(DEVICE_TEST_OBJECTIVE),
                "GoalSummary must carry the live objective:\n{directive}"
            );
            assert!(
                directive.contains(GOAL_CONTINUATION_SENTINEL),
                "GoalSummary must be the next-step continuation:\n{directive}"
            );
            assert!(
                !directive.contains("A goal has been set"),
                "GoalSummary must not be create-style rules:\n{directive}"
            );

            let last_user = after.iter().rev().find(|item| {
                matches!(item, ConversationItem::User(_))
            });
            let last_user = last_user.expect("post-compact history has a user item");
            assert!(
                matches!(
                    last_user,
                    ConversationItem::User(u)
                        if u.synthetic_reason == SyntheticReason::GoalSummary
                ),
                "last cue after compact must be GoalSummary, not create-style rules:\n{:?}",
                last_user.text_content()
            );
            assert!(
                last_user.text_content().contains(GOAL_CONTINUATION_SENTINEL),
                "last cue must continue the goal:\n{}",
                last_user.text_content()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn paused_goal_does_not_override_later_human_query() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::User)
            );

            assert_eq!(actor.goal_objective_for_compaction(), None);
            assert_eq!(actor.merge_goal_compaction_user_context(None), None);

            let conversation = stale_pre_goal_conversation();
            let ctx = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                ctx.last_user_query.as_deref(),
                Some(STALE_CODE_REVIEW),
                "paused goal must not replace the later human prompt"
            );

            let section = actor
                .compaction_goal_section()
                .await
                .expect("paused goal still belongs in the reminder");
            assert!(
                section.contains(DEVICE_TEST_OBJECTIVE),
                "reminder still pins the live objective:\n{section}"
            );
            assert!(
                !section.contains(GOAL_CONTINUATION_SENTINEL),
                "paused reminder must not be a continue-working cue:\n{section}"
            );

            actor.chat_state_handle.replace_conversation(conversation);
            let _ = actor.chat_state_handle.get_conversation().await;
            actor.reseed_active_goal_after_compaction().await;
            let after = actor.chat_state_handle.get_conversation().await;
            assert!(
                after.iter().all(|item| {
                    !matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    )
                }),
                "paused compact must not inject GoalSummary: {after:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compact_reseed_skips_turn_end_drain_and_round_count() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().expect("goal").rounds_since_verify = 4;
            }
            actor.pending_classifier_completions.lock().push_back(
                distill_tools::implementations::distill::update_goal::UpdateGoalInput {
                    completed: None,
                    message: Some("progress".into()),
                    blocked_reason: None,
                },
            );

            actor.reseed_active_goal_after_compaction().await;

            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .expect("goal")
                    .rounds_since_verify,
                4,
                "compact reseed must not count as a worker round"
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                1,
                "compact reseed must not TurnEnd-drain pending classifier completions"
            );

            let after = actor.chat_state_handle.get_conversation().await;
            assert!(
                after.iter().any(|item| {
                    matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    ) && item.text_content().contains(GOAL_CONTINUATION_SENTINEL)
                }),
                "compact still injects a continuation: {after:?}"
            );
        })
        .await;
}
