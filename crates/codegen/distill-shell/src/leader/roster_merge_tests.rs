use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;
use crate::agent::roster::{RosterActivity, RosterListResponse, RosterOrigin};
use crate::session::ExtMethodResult;

fn row(session_id: &str) -> RosterEntry {
    RosterEntry {
        session_id: session_id.to_owned(),
        title: Some("External agent bc-1".to_owned()),
        cwd: "/home/u/.grok/worktrees/proj/cursor-bc-1".to_owned(),
        is_worktree: true,
        session_kind: Some("cursor-worker".to_owned()),
        model_id: None,
        reasoning_effort: None,
        yolo: false,
        activity: RosterActivity::Idle,
        last_turn_summary: None,
        resident: false,
        last_change_unix_ms: 1_762_000_000_000,
        origin: RosterOrigin::Local,
    }
}

fn live_row(session_id: &str) -> RosterEntry {
    RosterEntry {
        session_id: session_id.to_owned(),
        title: None,
        cwd: "/repo".to_owned(),
        is_worktree: false,
        session_kind: None,
        model_id: Some("grok-4".to_owned()),
        reasoning_effort: None,
        yolo: false,
        activity: RosterActivity::Working,
        last_turn_summary: None,
        resident: true,
        last_change_unix_ms: 7,
        origin: RosterOrigin::Local,
    }
}

fn merge_with(rows: Vec<RosterEntry>) -> RosterListMerge {
    let roster = ExternalRoster::new();
    roster.replace(rows);
    RosterListMerge::new(roster)
}

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

/// A JSON-RPC response whose body is what `handle_roster_list` really emits.
fn list_response(id: Value, result: &ExtMethodResult<RosterListResponse>) -> String {
    let body: Value = serde_json::from_str(result.to_ext_response().unwrap().0.get()).unwrap();
    json!({"jsonrpc": "2.0", "id": id, "result": body}).to_string()
}

fn list_request(id: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "_x.ai/sessions/list", "params": {}}).to_string()
}

fn session_ids(out: &str, path: &[&str]) -> Vec<Value> {
    let mut node = parsed(out);
    for key in path {
        node = node.get_mut(*key).map(Value::take).unwrap_or(Value::Null);
    }
    node.as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].clone())
        .collect()
}

#[test]
fn appends_rows_to_envelope_response_with_string_id() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!("3|5")));
    let response = list_response(
        json!("3|5"),
        &ExtMethodResult::success(RosterListResponse {
            sessions: vec![live_row("live")],
        }),
    );

    let out = merge.filter_outbound(&response);

    assert_eq!(
        vec![json!("live"), json!("cursor-worker:bc-1")],
        session_ids(&out, &["result", "result", "sessions"])
    );
    assert_eq!(
        json!("cursor-worker"),
        parsed(&out)
            .pointer("/result/result/sessions/1/sessionKind")
            .cloned()
            .unwrap_or(Value::Null)
    );
    assert_eq!(0, merge.pending_len(), "answered id is forgotten");
}

#[test]
fn appends_rows_to_bare_response_with_numeric_id() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!(7)));

    let out = merge.filter_outbound(r#"{"jsonrpc":"2.0","id":7,"result":{"sessions":[]}}"#);

    assert_eq!(
        vec![json!("cursor-worker:bc-1")],
        session_ids(&out, &["result", "sessions"])
    );
}

#[test]
fn records_the_nested_method_of_a_wrapped_ext_request() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(
        r#"{"jsonrpc":"2.0","id":"1|1","method":"_x.ai/sessions/list","params":{"method":"x.ai/sessions/list","params":{}}}"#,
    );
    assert_eq!(1, merge.pending_len());
    // The nested method is authoritative: a wrapper naming another method is not a list.
    merge.observe_inbound(
        r#"{"jsonrpc":"2.0","id":"1|2","method":"_x.ai/sessions/list","params":{"method":"x.ai/session/info","params":{}}}"#,
    );
    assert_eq!(1, merge.pending_len());
}

#[test]
fn numeric_and_string_ids_do_not_match_each_other() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!(7)));

    let response = r#"{"jsonrpc":"2.0","id":"7","result":{"sessions":[]}}"#;
    assert_eq!(Cow::Borrowed(response), merge.filter_outbound(response));
    assert_eq!(1, merge.pending_len());
}

#[test]
fn two_pending_lists_answered_out_of_order_each_get_the_rows_once() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!("1|1")));
    merge.observe_inbound(&list_request(json!(2)));
    let empty = ExtMethodResult::success(RosterListResponse::default());

    let second_line = list_response(json!(2), &empty);
    let first_line = list_response(json!("1|1"), &empty);
    let second = merge.filter_outbound(&second_line);
    let first = merge.filter_outbound(&first_line);
    for out in [&second, &first] {
        assert_eq!(
            vec![json!("cursor-worker:bc-1")],
            session_ids(out, &["result", "result", "sessions"])
        );
    }
    assert_eq!(0, merge.pending_len());
    // A repeat of an answered id is no longer pending.
    let again = list_response(json!(2), &empty);
    assert_eq!(Cow::Borrowed(again.as_str()), merge.filter_outbound(&again));
}

#[test]
fn errored_responses_are_left_untouched_but_settle_the_id() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);

    merge.observe_inbound(&list_request(json!("3|5")));
    let envelope_error = list_response(
        json!("3|5"),
        &ExtMethodResult::<RosterListResponse>::failure("no roster"),
    );
    assert_eq!(
        Cow::Borrowed(envelope_error.as_str()),
        merge.filter_outbound(&envelope_error)
    );
    assert_eq!(0, merge.pending_len());

    merge.observe_inbound(&list_request(json!("3|6")));
    let rpc_error = r#"{"jsonrpc":"2.0","id":"3|6","error":{"code":-32601,"message":"nope"}}"#;
    assert_eq!(Cow::Borrowed(rpc_error), merge.filter_outbound(rpc_error));
    assert_eq!(0, merge.pending_len());
}

#[test]
fn pending_response_of_another_shape_is_untouched_but_settles_the_id() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!("3|5")));

    let odd = r#"{"jsonrpc":"2.0","id":"3|5","result":{"result":{"count":3}}}"#;
    assert_eq!(Cow::Borrowed(odd), merge.filter_outbound(odd));
    assert_eq!(0, merge.pending_len());
}

#[test]
fn unrelated_traffic_is_untouched_and_unrecorded() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(r#"{"jsonrpc":"2.0","id":"3|5","method":"session/prompt","params":{}}"#);
    merge.observe_inbound(
        r#"{"jsonrpc":"2.0","method":"_x.ai/sessions/list","params":{"note":"no id, not a request"}}"#,
    );
    merge.observe_inbound(
        r#"{"jsonrpc":"2.0","id":null,"method":"_x.ai/sessions/list","params":{}}"#,
    );
    merge.observe_inbound(r#"[{"jsonrpc":"2.0","id":1,"method":"_x.ai/sessions/list"}]"#);
    merge.observe_inbound("not json but mentions sessions/list");
    assert_eq!(0, merge.pending_len());

    let chunk = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#;
    assert_eq!(Cow::Borrowed(chunk), merge.filter_outbound(chunk));
    let other = r#"{"jsonrpc":"2.0","id":"3|5","result":{"result":{"sessions":[]}}}"#;
    assert_eq!(Cow::Borrowed(other), merge.filter_outbound(other));
}

#[test]
fn garbage_outbound_lines_pass_through_while_a_list_is_pending() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!(1)));

    for line in [
        r#"[{"jsonrpc":"2.0","id":1,"result":{"sessions":[]}}]"#,
        r#"not json with an "id" inside"#,
        r#"{"jsonrpc":"2.0","id":null,"result":{"sessions":[]}}"#,
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"\"id\": 1"}}"#,
    ] {
        assert_eq!(Cow::Borrowed(line), merge.filter_outbound(line));
    }
    assert_eq!(1, merge.pending_len(), "none of those answered the list");
}

#[test]
fn empty_roster_records_nothing_and_leaves_the_response_untouched() {
    let merge = merge_with(Vec::new());
    merge.observe_inbound(&list_request(json!("3|5")));
    assert_eq!(
        0,
        merge.pending_len(),
        "no rows, nothing to merge, no parse"
    );
    let response = list_response(
        json!("3|5"),
        &ExtMethodResult::success(RosterListResponse {
            sessions: vec![live_row("live")],
        }),
    );

    assert_eq!(
        Cow::Borrowed(response.as_str()),
        merge.filter_outbound(&response)
    );
    assert_eq!(0, merge.pending_len());
}

#[test]
fn ids_pending_when_the_roster_empties_are_dropped_not_kept_for_a_later_door() {
    let roster = ExternalRoster::new();
    roster.replace(vec![row("cursor-worker:bc-1")]);
    let merge = RosterListMerge::new(roster.clone());
    merge.observe_inbound(&list_request(json!(7)));
    assert_eq!(1, merge.pending_len());

    // The door stops; any outbound line settles the leftover ids.
    roster.replace(Vec::new());
    let chunk = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#;
    assert_eq!(Cow::Borrowed(chunk), merge.filter_outbound(chunk));
    assert_eq!(0, merge.pending_len());

    // A later door must not merge rows into a reused id's response.
    roster.replace(vec![row("cursor-worker:bc-2")]);
    let response = list_response(
        json!(7),
        &ExtMethodResult::success(RosterListResponse::default()),
    );
    assert_eq!(
        Cow::Borrowed(response.as_str()),
        merge.filter_outbound(&response)
    );
}

#[tokio::test(start_paused = true)]
async fn pending_ids_expire_after_ttl() {
    let merge = merge_with(vec![row("cursor-worker:bc-1")]);
    merge.observe_inbound(&list_request(json!("3|5")));
    assert_eq!(1, merge.pending_len());

    tokio::time::advance(PENDING_LIST_TTL + Duration::from_millis(1)).await;

    let response = list_response(
        json!("3|5"),
        &ExtMethodResult::success(RosterListResponse::default()),
    );
    assert_eq!(
        Cow::Borrowed(response.as_str()),
        merge.filter_outbound(&response)
    );
    assert_eq!(0, merge.pending_len());
}

/// Runs the notifier on a `LocalSet` with a channel sink; the returned receiver yields one
/// parsed `params` per emitted line.
struct Notifier {
    local: tokio::task::LocalSet,
    lines: mpsc::UnboundedReceiver<String>,
}

impl Notifier {
    fn spawn(roster: &ExternalRoster) -> Self {
        let (tx, lines) = mpsc::unbounded_channel();
        let local = tokio::task::LocalSet::new();
        local.spawn_local(run_changed_notifier(roster.clone(), move |line| {
            let _ = tx.send(line);
        }));
        Self { local, lines }
    }

    async fn next_params(&mut self) -> Value {
        let line = self
            .local
            .run_until(tokio::time::timeout(
                Duration::from_secs(5),
                self.lines.recv(),
            ))
            .await
            .expect("notifier emitted within the timeout")
            .expect("sink open");
        let json = parsed(&line);
        assert_eq!(Some(&json!("_x.ai/sessions/changed")), json.get("method"));
        json.get("params").cloned().unwrap_or(Value::Null)
    }

    /// Drives the notifier for a while and asserts it stayed silent.
    async fn assert_silent(&mut self) {
        self.local
            .run_until(async {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        assert!(
            self.lines.try_recv().is_err(),
            "notifier emitted unexpectedly"
        );
    }
}

fn ids(params: &Value, key: &str) -> Vec<Value> {
    params[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.get("sessionId").cloned().unwrap_or_else(|| v.clone()))
        .collect()
}

/// Pins the `mark_changed()` fix: the generation is published before the notifier is ever
/// polled, and must still be emitted.
#[tokio::test]
async fn notifier_does_not_skip_a_generation_published_before_first_poll() {
    let roster = ExternalRoster::new();
    roster.replace(vec![row("cursor-worker:bc-1")]);
    let mut notifier = Notifier::spawn(&roster);

    let params = notifier.next_params().await;
    assert_eq!(vec![json!("cursor-worker:bc-1")], ids(&params, "upserted"));
    assert_eq!(Some(&json!([])), params.get("removed"));
}

#[tokio::test]
async fn notifier_stays_silent_on_an_empty_initial_roster_and_identical_replace() {
    let roster = ExternalRoster::new();
    let mut notifier = Notifier::spawn(&roster);
    notifier.assert_silent().await;

    roster.replace(vec![row("cursor-worker:bc-1")]);
    let _first = notifier.next_params().await;
    roster.replace(vec![row("cursor-worker:bc-1")]);
    notifier.assert_silent().await;
}

#[tokio::test]
async fn notifier_emits_only_changed_rows_and_removals() {
    let roster = ExternalRoster::new();
    let mut notifier = Notifier::spawn(&roster);

    roster.replace(vec![row("cursor-worker:bc-1"), row("cursor-worker:bc-2")]);
    let params = notifier.next_params().await;
    assert_eq!(
        vec![json!("cursor-worker:bc-1"), json!("cursor-worker:bc-2")],
        ids(&params, "upserted")
    );

    // bc-1 changes activity, bc-2 is unchanged, bc-3 is new.
    let mut busy = row("cursor-worker:bc-1");
    busy.activity = RosterActivity::Working;
    roster.replace(vec![
        busy,
        row("cursor-worker:bc-2"),
        row("cursor-worker:bc-3"),
    ]);
    let params = notifier.next_params().await;
    assert_eq!(
        vec![json!("cursor-worker:bc-1"), json!("cursor-worker:bc-3")],
        ids(&params, "upserted")
    );
    assert_eq!(Some(&json!([])), params.get("removed"));

    roster.replace(vec![row("cursor-worker:bc-2")]);
    let params = notifier.next_params().await;
    assert_eq!(Some(&json!([])), params.get("upserted"));
    let mut removed = ids(&params, "removed");
    removed.sort_by_key(|v| v.to_string());
    assert_eq!(
        vec![json!("cursor-worker:bc-1"), json!("cursor-worker:bc-3")],
        removed
    );
}

#[test]
fn changed_notification_is_machine_wide_broadcast_shape() {
    let line = changed_notification(vec![row("cursor-worker:bc-1")], Vec::new());
    let json = parsed(&line);
    assert_eq!(Some(&json!("2.0")), json.get("jsonrpc"));
    assert!(json.get("id").is_none());
    assert_eq!(Some(&json!("_x.ai/sessions/changed")), json.get("method"));
    assert!(json.pointer("/params/sessionId").is_none());
}
