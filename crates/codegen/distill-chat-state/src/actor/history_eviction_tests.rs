// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::{ConversationItem, SamplingConfig, ToolCall, ToolSpec};

use super::*;
use crate::actor::ChatStateActor;
use crate::persistence::MockChatPersistence;
use crate::types::PruningConfig;

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ConversationItem {
    ConversationItem::assistant_tool_calls(vec![ToolCall {
        id: id.into(),
        name: name.to_owned(),
        arguments: arguments.to_string().into(),
    }])
}

/// A numbered multi-line output of about `bytes` bytes.
fn output(tag: &str, bytes: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    while out.len() < bytes {
        out.push_str(&format!("{tag} line {line}: some build or file text here\n"));
        line += 1;
    }
    out
}

/// `rounds` tool rounds of a small shell command each, after a system prompt.
fn filler(conversation: &mut Vec<ConversationItem>, from: usize, rounds: usize) {
    for i in from..from + rounds {
        let id = format!("f{i}");
        conversation.push(call(&id, "run_terminal_command", serde_json::json!({"command": format!("echo {i}")})));
        conversation.push(ConversationItem::tool_result(id, format!("{i}")));
    }
}

fn result_text<'a>(conversation: &'a [ConversationItem], id: &str) -> &'a str {
    conversation
        .iter()
        .find_map(|item| match item {
            ConversationItem::ToolResult(tr) if tr.tool_call_id == id => Some(tr.content.as_ref()),
            _ => None,
        })
        .expect("result present")
}

fn stored(_: &str, _: &str, _: &str) -> Option<String> {
    Some("/store/abc.txt".to_owned())
}

fn evict(conversation: &mut Vec<ConversationItem>, cold: Option<ColdReason>) -> usize {
    let plan = plan(conversation);
    if batch_label(&plan, cold, 0, false).is_none() {
        return 0;
    }
    let (replacements, _) = render(plan, true, stored);
    apply(conversation, &replacements)
}

/// The main saving: a big result the model used 20+ rounds ago stops being
/// resent whole, and what replaces it still says where the bytes are.
#[test]
fn an_old_large_result_keeps_its_edges_and_a_stored_pointer() {
    let big = output("cargo", 20_000);
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("go")];
    conversation.push(call("old", "run_terminal_command", serde_json::json!({"command": "cargo build"})));
    conversation.push(ConversationItem::tool_result("old", big.clone()));
    filler(&mut conversation, 0, 25);
    let recent = output("recent", 20_000);
    conversation.push(call("new", "run_terminal_command", serde_json::json!({"command": "cargo test"})));
    conversation.push(ConversationItem::tool_result("new", recent.clone()));

    assert_eq!(evict(&mut conversation, Some(ColdReason::ModelSwitch)), 1);
    let digest = result_text(&conversation, "old");
    assert!(digest.starts_with(EVICTED_MARKER), "{digest}");
    assert!(digest.contains("/store/abc.txt"), "the original must stay reachable: {digest}");
    assert!(digest.contains("ask_stored_output"), "{digest}");
    assert!(digest.contains("cargo line 0:"), "the head stays: {digest}");
    let last = big.lines().last().expect("lines");
    assert!(digest.contains(last), "the tail stays: {digest}");
    assert!(digest.len() < 1_200, "a digest is small: {}", digest.len());
    assert_eq!(result_text(&conversation, "new"), recent, "a recent result is never touched");
}

/// Fallback: no store (or a refusal for a secret, a skill, a user answer)
/// means the bytes stay exactly as they were.
#[test]
fn a_store_refusal_keeps_todays_bytes() {
    let big = output("secret-ish", 10_000);
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("old", "run_terminal_command", serde_json::json!({"command": "env"})));
    conversation.push(ConversationItem::tool_result("old", big.clone()));
    filler(&mut conversation, 0, 25);
    let before = conversation.clone();

    let plan = plan(&conversation);
    assert!(batch_label(&plan, Some(ColdReason::Compaction), 0, false).is_some());
    let (replacements, refused) = render(plan, true, |_, _, _| None);
    assert_eq!((replacements.len(), refused), (0, 1));
    assert_eq!(apply(&mut conversation, &replacements), 0);
    assert_eq!(result_text(&conversation, "old"), result_text(&before, "old"));
}

/// Cache economics: a warm batch breaks the cached prefix from its first
/// change on, so it waits for the interval and fires only when the bytes it
/// removes outweigh re-billing that suffix.
#[test]
fn a_warm_batch_waits_for_the_interval_and_for_payback() {
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("old", "run_terminal_command", serde_json::json!({"command": "cargo build"})));
    conversation.push(ConversationItem::tool_result("old", output("cargo", 6_000)));
    filler(&mut conversation, 0, 25);
    // A huge recent tail the batch would have to re-bill.
    conversation.push(call("huge", "run_terminal_command", serde_json::json!({"command": "cat big.log"})));
    conversation.push(ConversationItem::tool_result("huge", output("log", 400_000)));

    let small = plan(&conversation);
    assert_eq!(small.candidates.len(), 1);
    assert_eq!(batch_label(&small, None, 0, true), None, "5 KB saved cannot pay for a 400 KB re-bill");
    assert_eq!(batch_label(&small, Some(ColdReason::Idle), 0, false), Some("batch:cold-idle"));

    let mut conversation = vec![ConversationItem::system("sys")];
    for i in 0..6 {
        let id = format!("big{i}");
        conversation.push(call(&id, "read_file", serde_json::json!({"target_file": format!("src/{i}.rs")})));
        conversation.push(ConversationItem::tool_result(id, output("src", 30_000)));
    }
    filler(&mut conversation, 0, 25);
    let large = plan(&conversation);
    assert_eq!(batch_label(&large, None, 0, true), Some("batch:warm"));
    assert_eq!(
        batch_label(&large, None, large.rounds - 10, true),
        None,
        "a warm batch is at least the interval after the last one"
    );
    assert_eq!(
        batch_label(&large, None, 0, false),
        None,
        "without d6_warm_batches sent history is never rewritten on a warm cache"
    );
}

/// Supersession: an output a later identical call replaced is stale, so it
/// goes earlier and becomes a one-line pointer; a different range is not a
/// supersession.
#[test]
fn a_superseded_copy_goes_earlier_as_a_pointer() {
    let mut conversation = vec![ConversationItem::system("sys")];
    let read = serde_json::json!({"target_file": "src/lib.rs"});
    conversation.push(call("first", "read_file", read.clone()));
    conversation.push(ConversationItem::tool_result("first", output("v1", 9_000)));
    conversation.push(call("ranged", "read_file", serde_json::json!({"target_file": "src/lib.rs", "offset": 10, "limit": 400})));
    conversation.push(ConversationItem::tool_result("ranged", output("window", 9_000)));
    filler(&mut conversation, 0, 4);
    conversation.push(call("second", "read_file", read));
    conversation.push(ConversationItem::tool_result("second", output("v2", 9_000)));
    filler(&mut conversation, 4, 8);

    assert_eq!(evict(&mut conversation, Some(ColdReason::ModelSwitch)), 1);
    let pointer = result_text(&conversation, "first");
    assert!(pointer.starts_with(SUPERSEDED_MARKER), "{pointer}");
    assert!(pointer.contains("(second)") && pointer.contains("/store/abc.txt"), "{pointer}");
    assert!(!pointer.contains('\n'), "a superseded copy is one line: {pointer}");
    assert!(
        result_text(&conversation, "ranged").starts_with("window"),
        "another range of the same file is not superseded, and 13 rounds is under the age floor"
    );
    assert!(result_text(&conversation, "second").starts_with("v2"));
}

/// Old `write` bodies and long scripts in call arguments are replayed too;
/// their stub must stay valid JSON with the path, so strict providers accept
/// the call and the model knows which file to re-read.
#[test]
fn old_argument_values_become_valid_json_stubs() {
    let body = output("fn", 12_000);
    let script = format!("python3 - <<'EOF'\n{}EOF", output("py", 9_000));
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("w", "write", serde_json::json!({"file_path": "/repo/src/new.rs", "content": body})));
    conversation.push(ConversationItem::tool_result("w", "Wrote file"));
    conversation.push(call("s", "run_terminal_command", serde_json::json!({"command": script, "timeout": 60})));
    conversation.push(ConversationItem::tool_result("s", "ok"));
    filler(&mut conversation, 0, 25);

    assert_eq!(evict(&mut conversation, Some(ColdReason::Compaction)), 2);
    let arguments: Vec<serde_json::Value> = conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::Assistant(a) if a.tool_calls.iter().any(|c| c.id.as_ref() == "w" || c.id.as_ref() == "s") => {
                serde_json::from_str(&a.tool_calls[0].arguments).ok()
            }
            _ => None,
        })
        .collect();
    assert_eq!(arguments.len(), 2, "both stubs still parse as JSON");
    assert_eq!(arguments[0]["file_path"], "/repo/src/new.rs");
    let content = arguments[0]["content"].as_str().expect("content");
    assert!(content.starts_with(EVICTED_MARKER) && content.contains("/store/abc.txt"), "{content}");
    let command = arguments[1]["command"].as_str().expect("command");
    assert!(command.starts_with("python3 - <<'EOF'"), "the command head stays: {command}");
    assert!(command.len() < 1_200);
    assert_eq!(arguments[1]["timeout"], 60, "other fields are kept");
}

/// A whole-file reuse note points at an earlier copy; evicting that copy
/// would leave the note pointing at nothing, so it stays.
#[test]
fn a_copy_a_reuse_note_points_at_stays_whole() {
    let file = output("lib", 9_000);
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("orig", "read_file", serde_json::json!({"target_file": "src/lib.rs"})));
    conversation.push(ConversationItem::tool_result("orig", file.clone()));
    filler(&mut conversation, 0, 22);
    conversation.push(call("again", "grep", serde_json::json!({"pattern": "x"})));
    conversation.push(ConversationItem::tool_result(
        "again",
        format!("{READ_REUSE_NOTE_PREFIX}orig — this read returned the same bytes"),
    ));

    assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 0);
    assert_eq!(result_text(&conversation, "orig"), file);
}

/// A digest is written once: the next pass finds nothing to do, so each
/// evicted item breaks the provider cache exactly once.
#[test]
fn digests_are_never_evicted_again() {
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("old", "run_terminal_command", serde_json::json!({"command": "cargo build"})));
    conversation.push(ConversationItem::tool_result("old", output("cargo", 30_000)));
    filler(&mut conversation, 0, 25);
    assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 1);
    let once = conversation.clone();
    assert!(plan(&conversation).candidates.is_empty());
    assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 0);
    assert_eq!(result_text(&conversation, "old"), result_text(&once, "old"));
}

fn test_config(model: &str) -> SamplingConfig {
    SamplingConfig {
        base_url: "https://api.example.com".to_string(),
        model: model.to_string(),
        context_window: std::num::NonZeroU64::new(1_000_000).expect("non-zero"),
        ..Default::default()
    }
}

fn spawn(history_eviction: bool, archive: bool, conversation: Vec<ConversationItem>) -> crate::handle::ChatStateHandle {
    spawn_with(
        PruningConfig {
            history_eviction,
            ..Default::default()
        },
        archive,
        conversation,
    )
}

fn spawn_with(
    pruning: PruningConfig,
    archive: bool,
    conversation: Vec<ConversationItem>,
) -> crate::handle::ChatStateHandle {
    let (mock, _rx) = if archive {
        MockChatPersistence::new()
    } else {
        MockChatPersistence::new_failing_tool_archive()
    };
    let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
    ChatStateActor::spawn_with_pruning(
        conversation,
        test_config("model-a"),
        pruning,
        Box::new(mock),
        event_tx,
        tokio_util::sync::CancellationToken::new(),
    )
}

fn old_big_session() -> Vec<ConversationItem> {
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("go")];
    conversation.push(call("old", "read_file", serde_json::json!({"target_file": "src/big.rs"})));
    conversation.push(ConversationItem::tool_result("old", output("big", 8_000)));
    filler(&mut conversation, 0, 21);
    // A large recent tail keeps the warm payback gate shut.
    conversation.push(call("tail", "run_terminal_command", serde_json::json!({"command": "cat x"})));
    conversation.push(ConversationItem::tool_result("tail", output("tail", 300_000)));
    conversation
}

async fn build(handle: &crate::handle::ChatStateHandle) -> ConversationRequest {
    handle
        .build_request(
            vec![ToolSpec {
                name: "ask_stored_output".to_owned(),
                description: None,
                parameters: serde_json::json!({}),
            }],
            None,
            false,
            None,
            "conv".to_owned(),
            "req".to_owned(),
        )
        .await
        .expect("request")
}

fn request_result(request: &ConversationRequest, id: &str) -> String {
    result_text(&request.items, id).to_owned()
}

use distill_sampling_types::ConversationRequest;

/// A model switch makes the next request cold, so the pass runs on it, writes
/// the digest into the retained history, and counts what it did in usage.json.
#[tokio::test]
async fn a_model_switch_evicts_on_the_next_request_and_counts_it() {
    let handle = spawn(true, true, old_big_session());
    let warm = build(&handle).await;
    assert_eq!(
        request_result(&warm, "old").len(),
        result_text(&old_big_session(), "old").len(),
        "warm and not paying back: today's bytes"
    );

    handle.update_sampling_config(test_config("model-b"));
    let cold = build(&handle).await;
    assert!(request_result(&cold, "old").starts_with(EVICTED_MARKER));
    let retained = handle.get_conversation().await;
    assert!(result_text(&retained, "old").starts_with(EVICTED_MARKER), "the digest is retained, so it is stable");

    // A later read of the evicted path is counted as a re-read.
    handle.push_assistant_response(call("re", "read_file", serde_json::json!({"target_file": "src/big.rs"})));
    let ledger = handle.try_get_session_usage().await.expect("ledger");
    assert_eq!(ledger.utility_outcomes["history_batch"].decisions["batch:cold-model-switch"], 1);
    let evicted = &ledger.utility_outcomes["history_evict"];
    assert_eq!(evicted.decisions["evict:head-tail"], 1);
    assert!(evicted.bytes_in > evicted.bytes_out);
    assert_eq!(ledger.utility_outcomes["history_reread"].decisions["reread:source"], 1);
}

/// With the lever off, or with no store, a cold moment changes nothing.
#[tokio::test]
async fn without_the_lever_or_a_store_history_is_untouched() {
    for (lever, archive) in [(false, true), (true, false)] {
        let handle = spawn(lever, archive, old_big_session());
        handle.update_sampling_config(test_config("model-b"));
        let request = build(&handle).await;
        assert_eq!(
            request_result(&request, "old"),
            result_text(&old_big_session(), "old"),
            "lever={lever} archive={archive}"
        );
    }
}

/// The old user-turn hard clear must not wipe a digest's pointer: the digest
/// is the only way back to the stored original.
#[tokio::test]
async fn the_user_turn_hard_clear_keeps_eviction_digests() {
    let digest = format!("{EVICTED_MARKER} 30 rounds after this call: stored at /store/abc.txt]");
    let conversation = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("q"),
        call("a", "read_file", serde_json::json!({"target_file": "a.rs"})),
        ConversationItem::tool_result("a", digest.clone()),
        call("b", "read_file", serde_json::json!({"target_file": "b.rs"})),
        ConversationItem::tool_result("b", "plain old output"),
    ];
    let (mock, _rx) = MockChatPersistence::new();
    let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let handle = ChatStateActor::spawn_with_pruning(
        conversation,
        test_config("model-a"),
        PruningConfig {
            hard_clear_age_turns: 2,
            history_eviction: true,
            ..Default::default()
        },
        Box::new(mock),
        event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    for i in 0..5 {
        handle.push_user_message(ConversationItem::user(format!("u{i}")));
        handle.increment_prompt_index();
    }
    let retained = handle.get_conversation().await;
    assert_eq!(result_text(&retained, "b"), "[Tool result omitted — too old]");
    assert_eq!(result_text(&retained, "a"), digest);
}

/// A compaction input that is cold anyway: an old big output becomes its
/// edges plus the stored path, while the turn the user is in now (the output
/// the summary needs most) and images stay as they are.
#[test]
fn cold_compaction_digests_old_output_and_keeps_the_current_turn() {
    let old = output("old", 20_000);
    let current = output("current", 20_000);
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("first")];
    conversation.push(call("old", "run_terminal_command", serde_json::json!({"command": "cargo build"})));
    conversation.push(ConversationItem::tool_result("old", old.clone()));
    conversation.push(ConversationItem::user("second"));
    conversation.push(call("cur", "run_terminal_command", serde_json::json!({"command": "cargo test"})));
    conversation.push(ConversationItem::tool_result("cur", current.clone()));

    let candidates = cold_compaction_candidates(&conversation);
    assert_eq!(candidates.len(), 1, "only the output before the newest human turn");
    let candidate = &candidates[0];
    assert_eq!(candidate.tool_name, "run_terminal_command");
    let digest = cold_compaction_digest(&candidate.payload, "/store/old.txt");
    assert!(digest.starts_with(COMPACTION_DIGEST_MARKER), "{digest}");
    assert!(digest.contains("/store/old.txt"), "the original stays reachable: {digest}");
    assert!(digest.contains("old line 0:"), "the head stays: {digest}");
    assert!(digest.contains(old.lines().last().expect("lines")), "the tail stays: {digest}");
    assert!(shrink_tool_result(&mut conversation, candidate.index, &digest));
    assert_eq!(result_text(&conversation, "cur"), current, "the current turn is never touched");
    assert!(cold_compaction_candidates(&conversation).is_empty(), "a digest is never digested again");
}

/// A system reminder or other synthetic user item is not the user's turn: it
/// must not shrink the protected tail down to nothing.
#[test]
fn cold_compaction_boundary_is_the_human_turn_not_a_reminder() {
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("go")];
    conversation.push(call("cur", "run_terminal_command", serde_json::json!({"command": "cargo test"})));
    conversation.push(ConversationItem::tool_result("cur", output("current", 10_000)));
    let mut reminder = ConversationItem::user("<system-reminder>todo</system-reminder>");
    if let ConversationItem::User(user) = &mut reminder {
        user.synthetic_reason = distill_sampling_types::SyntheticReason::SystemReminder;
    }
    conversation.push(reminder);
    assert!(cold_compaction_candidates(&conversation).is_empty());
}

/// A replacement that would not be shorter keeps today's bytes.
#[test]
fn shrink_tool_result_never_grows_an_item() {
    let mut conversation = vec![ConversationItem::tool_result("a", "short")];
    assert!(!shrink_tool_result(&mut conversation, 0, "a much longer replacement"));
    assert_eq!(result_text(&conversation, "a"), "short");
}

/// A later "identical" call stands in for an earlier copy only when its result
/// holds the output: a reuse note pointing back, or a selection, would leave no
/// copy at all, so the earlier one waits for the head/tail age instead.
#[test]
fn a_later_note_or_selection_does_not_supersede_the_copy_it_points_at() {
    let full = output("tests", 10_000);
    for later in [
        "[unchanged content: 10000 bytes already sent this session at run_terminal_command call first]".to_owned(),
        format!("{}\n[compressed by verified utility selection; full output stored at /s/x]", output("kept", 1_000)),
        "error: command not found".to_owned(),
    ] {
        let mut conversation = vec![ConversationItem::system("sys")];
        let test = serde_json::json!({"command": "cargo test"});
        conversation.push(call("first", "run_terminal_command", test.clone()));
        conversation.push(ConversationItem::tool_result("first", full.clone()));
        filler(&mut conversation, 0, 2);
        conversation.push(call("second", "run_terminal_command", test));
        conversation.push(ConversationItem::tool_result("second", later.clone()));
        filler(&mut conversation, 2, 12);
        assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 0, "15 rounds: {later}");
        assert_eq!(result_text(&conversation, "first"), full);

        filler(&mut conversation, 14, 8);
        assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 1);
        let digest = result_text(&conversation, "first");
        assert!(digest.starts_with(EVICTED_MARKER), "head and tail, not a pointer: {digest}");
        assert!(digest.contains("tests line 0:"), "{digest}");
    }
}

/// The same command run in another directory is another output (another
/// crate's tests): it supersedes nothing.
#[test]
fn the_same_command_in_another_directory_is_not_a_supersession() {
    let mut conversation = vec![ConversationItem::system("sys")];
    conversation.push(call("a", "bash", serde_json::json!({"command": "cargo test", "workdir": "crates/a"})));
    conversation.push(ConversationItem::tool_result("a", output("crate-a", 9_000)));
    filler(&mut conversation, 0, 2);
    conversation.push(call("b", "bash", serde_json::json!({"command": "cargo test", "workdir": "crates/b"})));
    conversation.push(ConversationItem::tool_result("b", output("crate-b", 9_000)));
    filler(&mut conversation, 2, 12);
    assert_eq!(evict(&mut conversation, Some(ColdReason::Idle)), 0);
    assert!(result_text(&conversation, "a").starts_with("crate-a"));
}

/// A provider that reuses call ids makes "the call of this result" ambiguous
/// (a skill result could be read as a later read_file's): such a history is
/// left as it is, by eviction and by the cold compaction digest alike.
#[test]
fn a_repeated_call_id_leaves_the_history_alone() {
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("go")];
    conversation.push(call("call_0", "skill", serde_json::json!({"name": "review"})));
    conversation.push(ConversationItem::tool_result("call_0", output("skill", 20_000)));
    filler(&mut conversation, 0, 25);
    conversation.push(call("call_0", "read_file", serde_json::json!({"target_file": "a.rs"})));
    conversation.push(ConversationItem::tool_result("call_0", "short"));
    conversation.push(ConversationItem::user("next"));
    assert!(ambiguous_call_ids(&conversation));
    assert_eq!(evict(&mut conversation, Some(ColdReason::ModelSwitch)), 0);
    assert!(cold_compaction_candidates(&conversation).is_empty());
}

/// The user-turn hard clear must not clear the copy a whole-read reuse note
/// names: the note tells the model it holds those bytes, and nothing stored them.
#[tokio::test]
async fn the_user_turn_hard_clear_keeps_the_copy_a_reuse_note_names() {
    let file = output("lib", 6_000);
    let conversation = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("q"),
        call("r1", "read_file", serde_json::json!({"target_file": "src/lib.rs"})),
        ConversationItem::tool_result("r1", file.clone()),
        call("other", "read_file", serde_json::json!({"target_file": "b.rs"})),
        ConversationItem::tool_result("other", "plain old output"),
        ConversationItem::user("again"),
        call("r2", "read_file", serde_json::json!({"target_file": "src/lib.rs"})),
        ConversationItem::tool_result("r2", format!("{READ_REUSE_NOTE_PREFIX}r1 — same bytes")),
    ];
    let handle = spawn_with(
        PruningConfig {
            hard_clear_age_turns: 3,
            ..Default::default()
        },
        true,
        conversation,
    );
    let mut copy_outlived_its_neighbour = false;
    for i in 0..8 {
        handle.push_user_message(ConversationItem::user(format!("u{i}")));
        handle.increment_prompt_index();
        let retained = handle.get_conversation().await;
        if result_text(&retained, "r2").starts_with(READ_REUSE_NOTE_PREFIX) {
            assert_eq!(result_text(&retained, "r1"), file, "a live note's copy stays (turn {i})");
            copy_outlived_its_neighbour |=
                result_text(&retained, "other") == "[Tool result omitted — too old]";
        }
    }
    assert!(copy_outlived_its_neighbour, "the copy was old enough to clear while the note lived");
}

/// A long session big enough that a warm batch would pay for itself.
fn paying_session() -> Vec<ConversationItem> {
    let mut conversation = vec![ConversationItem::system("sys"), ConversationItem::user("go")];
    for i in 0..6 {
        let id = format!("big{i}");
        conversation.push(call(&id, "read_file", serde_json::json!({"target_file": format!("src/{i}.rs")})));
        conversation.push(ConversationItem::tool_result(id, output("src", 30_000)));
    }
    filler(&mut conversation, 0, 25);
    conversation
}

/// A fork (or a resume) starts on history its parent already sent: the
/// fork's first request rides the parent's cached prefix, so even with warm
/// batches on it does not rewrite that history at once.
#[tokio::test]
async fn inherited_history_counts_as_batched() {
    let warm = PruningConfig {
        history_eviction: true,
        history_eviction_warm: true,
        ..Default::default()
    };
    let handle = spawn_with(warm, true, paying_session());
    let request = build(&handle).await;
    assert_eq!(request_result(&request, "big0"), result_text(&paying_session(), "big0"));
}

/// A compaction that re-pinned an inherited prefix (a fork) keeps that prefix
/// cached, so it is not a cold moment; one that rebuilt the prefix is.
#[tokio::test]
async fn a_compaction_that_keeps_a_cached_prefix_is_not_cold() {
    let summary = ConversationItem::user("summary of the child's own work");
    let handle = spawn(true, true, paying_session());
    let mut kept = paying_session();
    kept.push(summary.clone());
    handle.replace_conversation_for_compaction(kept);
    let request = build(&handle).await;
    assert_eq!(
        request_result(&request, "big0"),
        result_text(&paying_session(), "big0"),
        "the re-pinned parent prefix stays as the cache holds it"
    );

    let handle = spawn(true, true, paying_session());
    let mut rebuilt = paying_session();
    rebuilt[0] = ConversationItem::system("a rebuilt system prompt");
    rebuilt.push(summary);
    handle.replace_conversation_for_compaction(rebuilt);
    let request = build(&handle).await;
    assert!(request_result(&request, "big0").starts_with(EVICTED_MARKER));
}
