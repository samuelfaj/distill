// Modified for Distill by Samuel Fajreldines, 2026.
//! A prefire pass-1 summarizes the history while the main thread keeps
//! working. It pays only when it reads that history from the provider's prompt
//! cache: its request must repeat the main request's prefix and end where an
//! earlier main request wrote a cache entry. These tests drive real turns
//! against the mock inference server and compare the request bodies.
use super::super::support::*;
use super::*;
use distill_sampling_types::ToolCall;
use distill_test_support::MockInferenceServer;
use std::future::Future;
use std::pin::Pin;

/// Large enough that the pass-1 input fits beside the summary reserve, as on
/// the long-context models that prefire on, so it is sent unfitted.
const WINDOW: u64 = 400_000;
/// The history (about 80% of [`WINDOW`]) stays under this threshold's prefire line.
const QUIET_THRESHOLD: u8 = 99;
/// The history is over this threshold's prefire line and under the threshold.
const PREFIRE_THRESHOLD: u8 = 85;

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// What the mock answers every request: a summary long enough to be applied.
fn summary() -> String {
    format!(
        "<summary>\n{}\n</summary>",
        "The parser fix is in and its tests pass. ".repeat(40)
    )
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: Arc::from(id),
        name: "todo_write".into(),
        arguments: Arc::from(r#"{"todos":[]}"#),
    }
}

/// About 80% of [`WINDOW`].
fn history() -> Vec<ConversationItem> {
    let mut items = vec![
        ConversationItem::system("you are a coding agent"),
        ConversationItem::user("<user_query>\nfix the parser\n</user_query>"),
    ];
    for round in 0..12 {
        let id = format!("call-{round}");
        items.push(ConversationItem::assistant_tool_calls(vec![call(&id)]));
        items.push(ConversationItem::tool_result(
            id,
            format!("round {round}\n{}", "parser output line\n".repeat(5_600)),
        ));
    }
    items.push(ConversationItem::assistant("The parser is fixed."));
    items
}

/// Runs `body` on its own runtime, on a thread with room for a debug-build turn future.
fn on_turn_thread<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(body)
        .expect("spawn test thread")
        .join()
        .expect("test thread")
}

fn block_on_local<T>(future: impl Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    tokio::task::LocalSet::new().block_on(&runtime, future)
}

// The session futures below are boxed so a test's own future does not embed
// their layout, which would pass the crate's query depth limit.

fn prompt_turn<'a>(
    actor: &'a Arc<SessionActor>,
    text: &str,
) -> Boxed<'a, crate::session::commands::PromptTurnResult> {
    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        text.to_owned(),
    ))];
    Box::pin(actor.handle_prompt(
        "prefire-prefix",
        prompt,
        crate::session::plan_mode::PromptMode::Agent,
        None,
        None,
        None,
        None,
        true,
        false,
        None,
        None,
        None,
    ))
}

fn compact(actor: &Arc<SessionActor>) -> Boxed<'_, Result<(), acp::Error>> {
    Box::pin(actor.run_compact_only(
        AutoCompactTriggerInfo {
            tokens_used: WINDOW * 4 / 5,
            context_window: WINDOW,
            percentage: 80,
            reason_override: None,
        },
        false,
    ))
}

/// A switch to the same model under another prompt label: the agent's prompt
/// is re-rendered and the history's head rewritten, as on any switch that
/// refreshes the prompt.
fn switch_prompt<'a>(
    actor: &'a Arc<SessionActor>,
    server: &MockInferenceServer,
    backend: ApiBackend,
    model: &str,
) -> Boxed<'a, Result<acp::ModelId, acp::Error>> {
    Box::pin(
        actor.handle_set_session_model(crate::session::SessionModelSwitch {
            rebuild_definition: None,
            sampling_config: sampler_config(server, backend, model),
            canonical_model_id: None,
            model_selection_intent: false,
            use_concise: false,
            is_family_switch: false,
            apply_prompt_override: true,
            skip_prompt_rewrite: false,
            auto_compact_threshold_percent: PREFIRE_THRESHOLD,
            system_prompt_label: "Aurora".to_owned(),
        }),
    )
}

/// One prompt turn, then the pass-1 it may have started.
async fn turn(actor: &Arc<SessionActor>, text: &str) {
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(60), prompt_turn(actor, text))
            .await
            .expect("turn must finish within timeout");
    assert!(outcome.is_ok(), "turn must not error: {outcome:?}");
    if let Some(handle) = actor.compaction.prefire.take_handle() {
        let _ = handle.await;
    }
}

fn sampler_config(
    server: &MockInferenceServer,
    backend: ApiBackend,
    model: &str,
) -> distill_sampler::SamplerConfig {
    distill_sampler::SamplerConfig {
        api_key: Some("test-key".to_string()),
        base_url: server.url(),
        model: model.to_string(),
        api_backend: backend,
        context_window: WINDOW,
        max_retries: Some(0),
        idle_timeout_secs: Some(30),
        ..Default::default()
    }
}

/// A two-pass session over [`history`] on `backend`, sampling from `server`.
async fn actor_on(
    server: &MockInferenceServer,
    backend: ApiBackend,
    model: &str,
    threshold: u8,
) -> Arc<SessionActor> {
    let (sampler_event_tx, mut sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_sampler::SamplingEvent>();
    let sampler_handle = distill_sampler::SamplerActor::spawn(
        sampler_config(server, backend.clone(), model),
        distill_sampler::RetryPolicy {
            max_retries: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    drain_gateway(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    drain_persistence(persistence_rx);
    let mut actor = create_test_actor(0, WINDOW, threshold, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    let base = test_distill_agent_with_todo().await;
    let mut policy = base.compaction_policy().clone();
    policy.two_pass_enabled = true;
    *actor.agent.borrow_mut() = distill_agent::Agent::new(
        base.definition().clone(),
        base.prompt_context().clone(),
        base.system_prompt().to_owned(),
        base.tool_bridge().clone(),
        base.reminder_policy().clone(),
        policy,
        base.hosted_tools().to_vec(),
        base.backend_search_enabled(),
    );
    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = backend;
    cfg.model = model.to_string();
    actor.chat_state_handle.update_sampling_config(cfg);
    let mut creds = actor.chat_state_handle.get_credentials().await;
    creds.api_key = Some("test-key".to_string());
    actor.chat_state_handle.update_credentials(creds);
    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.as_path().to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            actor.agent.borrow().tool_bridge().toolset(),
            None,
        )
        .expect("bind_local_session");
    actor.chat_state_handle.replace_conversation(history());
    let actor = Arc::new(actor);
    let drainer = actor.clone();
    tokio::task::spawn_local(async move {
        while let Some(event) = sampler_event_rx.recv().await {
            drainer.handle_sampling_event(event).await;
        }
    });
    actor
}

/// The inference request bodies the mock received, in arrival order.
fn bodies(server: &MockInferenceServer) -> Vec<serde_json::Value> {
    server
        .requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/messages") || r.path.ends_with("/responses"))
        .filter_map(|r| r.body)
        .collect()
}

/// Every message or input item of a request body.
fn items(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array())
        .cloned()
        .unwrap_or_default()
}

/// A compaction request (either pass, or single pass) ends with the summary instruction.
fn is_compaction(body: &serde_json::Value) -> bool {
    let prompt = build_compaction_prompt(None, false);
    let opening: String = prompt
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .take(24)
        .collect();
    items(body)
        .last()
        .is_some_and(|last| last.to_string().contains(opening.trim()))
}

fn without_cache_control(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for v in map.values_mut() {
                *v = without_cache_control(std::mem::take(v));
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                *v = without_cache_control(std::mem::take(v));
            }
        }
        _ => {}
    }
    value
}

/// Indexes of the items that carry a cache breakpoint.
fn breakpoints(body: &serde_json::Value) -> Vec<usize> {
    items(body)
        .iter()
        .enumerate()
        .filter(|(_, item)| item.to_string().contains("cache_control"))
        .map(|(at, _)| at)
        .collect()
}

/// The pass-1 a turn over the prefire line sends on a warm history, with the
/// main request before it and the main request of its own turn.
fn prefire_on_a_warm_history(
    backend: ApiBackend,
    model: &'static str,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    on_turn_thread(move || {
        block_on_local(async move {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.set_response(summary());
            let actor = actor_on(&server, backend, model, QUIET_THRESHOLD).await;
            turn(&actor, "now add a test").await;
            // The mock reports a token count far below the history it was sent.
            actor.chat_state_handle.record_token_usage(WINDOW * 4 / 5);
            actor.compaction.threshold_percent.set(PREFIRE_THRESHOLD);
            turn(&actor, "and run it").await;
            let bodies = bodies(&server);
            let pass1: Vec<_> = bodies.iter().filter(|b| is_compaction(b)).collect();
            assert_eq!(
                pass1.len(),
                1,
                "one pass-1, from the turn over the prefire line"
            );
            let pass1 = pass1[0].clone();
            let mains: Vec<_> = bodies
                .into_iter()
                .filter(|b| !is_compaction(b) && b["tools"] == pass1["tools"])
                .collect();
            assert_eq!(mains.len(), 2, "one main request per turn");
            (mains[0].clone(), pass1, mains[1].clone())
        })
    })
}

/// The pass-1 sends the main request's system prompt, tools and history item
/// for item, and adds only its instruction.
fn assert_repeats_main_prefix(pass1: &serde_json::Value, main: &serde_json::Value) {
    let (pass1, main) = (
        without_cache_control(pass1.clone()),
        without_cache_control(main.clone()),
    );
    for key in ["tools", "system", "instructions", "thinking", "reasoning"] {
        assert_eq!(pass1[key], main[key], "{key} differs");
    }
    let (pass1, main) = (items(&pass1), items(&main));
    assert_eq!(pass1.len(), main.len() + 1, "only the instruction is added");
    if let Some(at) = (0..main.len()).find(|&at| pass1[at] != main[at]) {
        panic!("item {at} differs: pass1={} main={}", pass1[at], main[at]);
    }
}

/// On a breakpoint cache an entry is read only at a position an earlier
/// request marked. A pass-1 over 95% of the history ended a few rounds before
/// any live main entry and wrote its whole input again; it now ends with the
/// main request's last item, so it marks where the previous main request wrote.
#[test]
fn a_messages_pass1_repeats_the_main_prefix_and_reads_the_previous_entry() {
    let (previous, pass1, main) =
        prefire_on_a_warm_history(ApiBackend::Messages, "claude-opus-5-5");
    assert_repeats_main_prefix(&pass1, &main);
    let previous_tip = items(&previous).len() - 1;
    assert!(
        breakpoints(&previous).contains(&previous_tip),
        "the previous main request wrote its entry at its tip: {:?}",
        breakpoints(&previous)
    );
    assert!(
        breakpoints(&pass1).contains(&previous_tip),
        "the pass-1 must mark item {previous_tip}, where that entry is: {:?}",
        breakpoints(&pass1)
    );
}

/// Prefix caching without breakpoints reads any repeated prefix.
#[test]
fn a_responses_pass1_repeats_the_main_prefix() {
    let (_, pass1, main) = prefire_on_a_warm_history(ApiBackend::Responses, "test");
    assert_repeats_main_prefix(&pass1, &main);
}

fn system_head(conversation: &[ConversationItem]) -> String {
    conversation
        .first()
        .map(ConversationItem::text_content)
        .unwrap_or_default()
}

/// A switch that refreshes the prompt used to drop a prepared pass-1, which
/// the next turn then sent again in full. The compaction that follows the
/// switch must apply the prepared note (pass 2 over the note and the tail),
/// not summarize the whole history again.
#[test]
fn a_prepared_pass1_survives_a_switch_that_rewrites_the_prompt() {
    let (compactions, history_items, pass1_outcomes) = on_turn_thread(|| {
        block_on_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.set_response(summary());
            let actor = actor_on(&server, ApiBackend::Responses, "test", PREFIRE_THRESHOLD).await;
            turn(&actor, "now add a test").await;
            assert!(
                actor.compaction.prefire.has_cache(),
                "the turn over the prefire line prepared a pass-1 note"
            );
            let head = system_head(&actor.chat_state_handle.get_conversation().await);
            switch_prompt(&actor, &server, ApiBackend::Responses, "test")
                .await
                .expect("switch");
            let conversation = actor.chat_state_handle.get_conversation().await;
            assert_ne!(
                system_head(&conversation),
                head,
                "the switch rewrote the head"
            );
            assert!(
                actor.compaction.prefire.has_cache(),
                "a prompt refresh keeps the prepared note"
            );
            compact(&actor).await.expect("compaction");
            let usage = actor
                .chat_state_handle
                .try_get_session_usage()
                .await
                .expect("usage");
            let compactions: Vec<_> = bodies(&server).into_iter().filter(is_compaction).collect();
            (
                compactions,
                conversation.len(),
                usage
                    .utility_outcomes
                    .get(PASS1_OUTCOME_SOURCE)
                    .map(|counts| counts.decisions.clone())
                    .unwrap_or_default(),
            )
        })
    });
    assert_eq!(compactions.len(), 2, "pass 1, then one compaction request");
    let applied = items(&compactions[1]).len();
    assert!(
        applied < history_items,
        "the compaction read the note and the tail ({applied} items), not the {history_items}-item history"
    );
    assert_eq!(pass1_outcomes.get("sent"), Some(&1));
    assert_eq!(pass1_outcomes.get("used"), Some(&1));
    assert!(
        !pass1_outcomes
            .keys()
            .any(|label| label.starts_with("discard")),
        "{pass1_outcomes:?}"
    );
}

/// The single-pass compaction request: on a cold history (right after the
/// session switched to its model) or, with `warm`, after a turn sent it.
fn single_pass_compaction(warm: bool) -> serde_json::Value {
    on_turn_thread(move || {
        block_on_local(async move {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.set_response(summary());
            let actor = actor_on(
                &server,
                ApiBackend::Messages,
                "claude-opus-5-5",
                QUIET_THRESHOLD,
            )
            .await;
            if warm {
                turn(&actor, "now add a test").await;
            }
            compact(&actor).await.expect("compaction");
            bodies(&server)
                .into_iter()
                .filter(is_compaction)
                .next_back()
                .expect("a compaction request")
        })
    })
}

/// A cold history is written again by the compaction anyway, and the
/// compacted history that follows never repeats it: a conversation breakpoint
/// would pay the cache-write premium for an entry nobody reads. The system
/// prompt and tools, which the next main request repeats, keep theirs.
#[test]
fn a_cold_compaction_input_marks_only_the_system_prompt() {
    let cold = single_pass_compaction(false);
    assert!(
        cold["system"].to_string().contains("cache_control"),
        "{}",
        cold["system"]
    );
    assert_eq!(breakpoints(&cold), Vec::<usize>::new());
    let warm = single_pass_compaction(true);
    assert!(
        !breakpoints(&warm).is_empty(),
        "a warm input reads the main entries through its conversation breakpoints"
    );
}
