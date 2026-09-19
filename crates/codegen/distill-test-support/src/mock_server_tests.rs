#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use super::*;
use crate::conversation_script::{MockToolCall, mock_call_id};
use crate::failure::{ObservedFailure, StatusFailure};
use crate::tools::Tool;

const MERMAID_TEXT: &str = "Here is a flow:\n\n```mermaid\nflowchart TD\n  A --> B\n```\n\nDone.\n";

fn sse_data_payloads(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.trim_start().to_owned())
        .filter(|d| d != "[DONE]")
        .collect()
}

fn chat_stream_text(body: &str) -> String {
    sse_data_payloads(body)
        .iter()
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| {
            v.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

fn responses_stream_text(body: &str) -> String {
    sse_data_payloads(body)
        .iter()
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
        .filter_map(|v| v.get("delta").and_then(Value::as_str).map(String::from))
        .collect()
}

fn foreground_body(endpoint: InferenceEndpoint, content: &str) -> Value {
    let tools = json!([
        { "type": "function", "function": { "name": "read_file" } },
        { "type": "function", "function": { "name": "write" } }
    ]);
    match endpoint {
        InferenceEndpoint::ChatCompletions | InferenceEndpoint::Messages => json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": content }],
            "tools": tools,
        }),
        InferenceEndpoint::Responses => json!({
            "model": "test-model",
            "input": [{ "role": "user", "content": content }],
            "tools": tools,
        }),
    }
}

fn endpoint_url(server: &MockInferenceServer, endpoint: InferenceEndpoint) -> String {
    format!("{}{}", server.origin(), endpoint.path())
}

async fn post_chat(server: &MockInferenceServer, content: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/chat/completions", server.url()))
        .json(&json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": content }]
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions")
}

async fn read_foreground(
    server: &MockInferenceServer,
    endpoint: InferenceEndpoint,
    request_id: &str,
    content: &str,
) -> (reqwest::StatusCode, String) {
    read_foreground_body(
        server,
        endpoint,
        request_id,
        foreground_body(endpoint, content),
    )
    .await
}

async fn read_foreground_body(
    server: &MockInferenceServer,
    endpoint: InferenceEndpoint,
    request_id: &str,
    body: Value,
) -> (reqwest::StatusCode, String) {
    let response = reqwest::Client::new()
        .post(endpoint_url(server, endpoint))
        .header("x-grok-req-id", request_id)
        .header("x-grok-turn-idx", "1")
        .json(&body)
        .send()
        .await
        .expect("POST foreground inference request");
    let status = response.status();
    let body = response.text().await.expect("read inference response body");
    (status, body)
}

fn chat_body(system: &str) -> Value {
    json!({
        "model": "m",
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": "go" }
        ],
        "tools": [
            { "type": "function", "function": { "name": "read_file" } },
            { "type": "function", "function": { "name": "run_terminal_command" } }
        ]
    })
}

async fn post_chat_turn(server: &MockInferenceServer, system: &str) -> String {
    read_foreground_body(
        server,
        InferenceEndpoint::ChatCompletions,
        "turn",
        chat_body(system),
    )
    .await
    .1
}

async fn send_chat_turn(
    server: &MockInferenceServer,
    system: &str,
) -> reqwest::Result<reqwest::Response> {
    reqwest::Client::new()
        .post(endpoint_url(server, InferenceEndpoint::ChatCompletions))
        .header("x-grok-turn-idx", "1")
        .json(&chat_body(system))
        .send()
        .await
}

fn observed_failures(server: &MockInferenceServer) -> Vec<Option<ObservedFailure>> {
    server
        .requests()
        .iter()
        .map(|entry| entry.observed_failure)
        .collect()
}

#[tokio::test]
async fn foreground_requests_are_logged_under_their_conversation_and_side_work_under_none() {
    let server = MockInferenceServer::start().await.unwrap();
    let endpoint = InferenceEndpoint::ChatCompletions;
    read_foreground_body(&server, endpoint, "parent-1", chat_body("parent")).await;
    read_foreground_body(&server, endpoint, "child-1", chat_body("child")).await;
    post_chat(&server, "title?").await;
    read_foreground_body(&server, endpoint, "parent-2", chat_body("parent")).await;

    let conversations: Vec<(usize, Vec<Option<String>>)> = server
        .conversations()
        .iter()
        .map(|conversation| {
            (
                conversation.number(),
                conversation
                    .requests()
                    .iter()
                    .map(LogEntry::first_system_prompt)
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        (
            vec![
                (
                    1,
                    vec![Some("parent".to_owned()), Some("parent".to_owned())]
                ),
                (2, vec![Some("child".to_owned())]),
            ],
            Some(2),
            None,
        ),
        (
            conversations,
            server
                .conversation_for_system_prompt("child")
                .map(|conversation| conversation.number()),
            server
                .conversation(3)
                .map(|conversation| conversation.number()),
        )
    );
}

#[tokio::test]
async fn dropping_entries_keeps_the_count_exact() {
    let server = MockInferenceServer::start().await.unwrap();
    post_chat(&server, "kept").await;
    let counted = server.request_count();
    let kept = server.requests().len();

    server.set_keep_requests(false);
    post_chat(&server, "dropped").await;

    assert_eq!(counted + 1, server.request_count());
    let entries = server.requests();
    assert_eq!(kept, entries.len());
    assert_eq!(
        Some(&json!("kept")),
        entries
            .last()
            .unwrap()
            .body
            .as_ref()
            .unwrap()
            .pointer("/messages/0/content")
    );
}

#[tokio::test]
async fn auxiliary_request_does_not_consume_foreground_expectation() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut expected = server.expect_response(
        "foreground turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::ChatCompletions),
        ScriptedResponse::text(209, "foreground"),
    );

    let aux = post_chat(&server, "generate a title").await;
    assert_eq!(200, aux.status());
    assert!(!expected.is_satisfied());

    let (status, body) = read_foreground(
        &server,
        InferenceEndpoint::ChatCompletions,
        "turn-1",
        "run the task",
    )
    .await;
    assert_eq!(209, status.as_u16());
    assert_eq!("foreground", body);
    expected.wait_received().await;
    expected.wait_satisfied().await;
}

#[tokio::test]
async fn concurrent_matching_requests_claim_each_expectation_once() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut first = server.expect_response(
        "first concurrent turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::ChatCompletions),
        ScriptedResponse::text(210, "first"),
    );
    let mut second = server.expect_response(
        "second concurrent turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::ChatCompletions),
        ScriptedResponse::text(211, "second"),
    );

    let (left, right) = tokio::join!(
        read_foreground(
            &server,
            InferenceEndpoint::ChatCompletions,
            "concurrent-left",
            "left",
        ),
        read_foreground(
            &server,
            InferenceEndpoint::ChatCompletions,
            "concurrent-right",
            "right",
        )
    );
    let mut responses = vec![(left.0.as_u16(), left.1), (right.0.as_u16(), right.1)];
    responses.sort_unstable();
    assert_eq!(
        vec![(210, "first".to_owned()), (211, "second".to_owned())],
        responses
    );
    first.wait_satisfied().await;
    second.wait_satisfied().await;
}

#[tokio::test]
async fn blocked_expectation_reports_lifecycle_and_drop_releases() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut expected = server.expect_response_blocked(
        "blocked foreground turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::ChatCompletions),
        ScriptedResponse::sse(vec![
            SseEvent::data(r#"{"chunk":1}"#),
            SseEvent::data("done"),
        ]),
    );
    let request = read_foreground(
        &server,
        InferenceEndpoint::ChatCompletions,
        "blocked-turn",
        "block me",
    );
    tokio::pin!(request);

    tokio::select! {
        response = &mut request => panic!("blocked expectation completed early: {:?}", response.0),
        _ = expected.wait_blocked() => {}
    }
    assert!(!expected.is_satisfied());
    let diagnostic = expected.diagnostic();
    assert!(diagnostic.contains("blocked foreground turn"));
    assert!(diagnostic.contains("Blocked"));
    drop(expected);
    let (status, _) = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("dropping handle releases blocked response");
    assert_eq!(200, status.as_u16());
}

#[tokio::test]
async fn release_only_signals_and_late_blocked_waiters_still_succeed() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut expected = server.expect_response_blocked(
        "release ownership",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::json(200, json!({ "ok": true })),
    );
    let request = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "release-ownership",
        "hello",
    );
    tokio::pin!(request);
    tokio::select! {
        response = &mut request => panic!("response completed before barrier: {:?}", response.0),
        _ = expected.wait_blocked() => {}
    }

    expected.release();
    assert!(!expected.is_satisfied());
    let (status, _) = request.await;
    assert_eq!(200, status.as_u16());
    expected.wait_satisfied().await;
    expected.wait_blocked().await;
}

#[tokio::test]
async fn overlapping_duplicate_replays_but_sequential_identical_request_claims_next() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut first = server.expect_response_blocked(
        "overlapping call",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::sse(vec![SseEvent::data("first"), SseEvent::data("terminal")]),
    );
    let mut second = server.expect_response(
        "later identical call",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(215, "second expectation"),
    );

    let primary = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "turn-id",
        "same body",
    );
    tokio::pin!(primary);
    tokio::select! {
        response = &mut primary => panic!("primary completed before barrier: {:?}", response.0),
        _ = first.wait_blocked() => {}
    }
    let replay = tokio::spawn({
        let url = endpoint_url(&server, InferenceEndpoint::Responses);
        let body = foreground_body(InferenceEndpoint::Responses, "same body");
        async move {
            let response = reqwest::Client::new()
                .post(url)
                .header("x-grok-req-id", "turn-id")
                .header("x-grok-turn-idx", "1")
                .json(&body)
                .send()
                .await
                .expect("POST overlapping duplicate");
            let status = response.status();
            let body = response.text().await.expect("read overlapping duplicate");
            (status, body)
        }
    });
    first.wait_claims(2).await;
    assert!(
        !replay.is_finished(),
        "overlapping duplicate bypassed shared barrier"
    );
    first.release();
    let (primary_result, replay_result) = tokio::join!(primary, replay);
    let (primary_status, _) = primary_result;
    let (replay_status, _) = replay_result.expect("replay task");
    assert_eq!(200, primary_status.as_u16());
    assert_eq!(200, replay_status.as_u16());
    first.wait_satisfied().await;

    let (status, body) = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "turn-id",
        "same body",
    )
    .await;
    assert_eq!(215, status.as_u16());
    assert_eq!("second expectation", body);
    second.wait_satisfied().await;
}

#[tokio::test]
async fn changed_body_followup_claims_next_expectation() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut first = server.expect_response(
        "tool call",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(214, "tool-call-script"),
    );
    let mut followup = server.expect_response(
        "tool follow-up",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(215, "follow-up-script"),
    );
    let first_body = foreground_body(InferenceEndpoint::Responses, "run tool");
    let (status, body) =
        read_foreground_body(&server, InferenceEndpoint::Responses, "turn-id", first_body).await;
    assert_eq!(214, status.as_u16());
    assert_eq!("tool-call-script", body);
    first.wait_satisfied().await;

    let mut followup_body = foreground_body(InferenceEndpoint::Responses, "run tool");
    followup_body
        .get_mut("input")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(json!({ "type": "function_call_output", "call_id": "call_1", "output": "done" }));
    let (status, body) = read_foreground_body(
        &server,
        InferenceEndpoint::Responses,
        "turn-id",
        followup_body,
    )
    .await;
    assert_eq!(215, status.as_u16());
    assert_eq!("follow-up-script", body);
    followup.wait_satisfied().await;
}

#[tokio::test]
async fn cancelling_primary_cleans_up_without_satisfying_or_replaying() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut cancelled = server.expect_response_blocked(
        "cancel primary",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::sse(vec![SseEvent::data("chunk"), SseEvent::data("terminal")]),
    );
    let mut next = server.expect_response(
        "after cancellation",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(216, "next expectation"),
    );
    let request = Box::pin(read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "cancel-primary",
        "same body",
    ));
    let mut request = request;
    tokio::select! {
        response = &mut request => panic!("primary completed before cancellation: {:?}", response.0),
        _ = cancelled.wait_blocked() => {}
    }
    drop(request);
    assert!(!cancelled.is_satisfied());

    let (status, body) = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "cancel-primary",
        "same body",
    )
    .await;
    assert_eq!(216, status.as_u16());
    assert_eq!("next expectation", body);
    next.wait_satisfied().await;
}

#[tokio::test]
async fn cancelling_replay_waits_for_primary_before_satisfaction() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut expected = server.expect_response_blocked(
        "cancel replay",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::sse(vec![SseEvent::data("chunk"), SseEvent::data("terminal")]),
    );
    let primary = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "cancel-replay",
        "same body",
    );
    tokio::pin!(primary);
    tokio::select! {
        response = &mut primary => panic!("primary completed before barrier: {:?}", response.0),
        _ = expected.wait_blocked() => {}
    }

    let replay = tokio::spawn({
        let url = endpoint_url(&server, InferenceEndpoint::Responses);
        let body = foreground_body(InferenceEndpoint::Responses, "same body");
        async move {
            reqwest::Client::new()
                .post(url)
                .header("x-grok-req-id", "cancel-replay")
                .header("x-grok-turn-idx", "1")
                .json(&body)
                .send()
                .await
                .expect("POST replay cancellation")
                .text()
                .await
                .expect("read replay cancellation")
        }
    });
    expected.wait_claims(2).await;
    assert!(!replay.is_finished(), "replay bypassed shared barrier");
    replay.abort();
    let _ = replay.await;
    assert!(!expected.is_satisfied());

    expected.release();
    let (status, _) = primary.await;
    assert_eq!(200, status.as_u16());
    expected.wait_satisfied().await;
}

#[tokio::test]
#[should_panic(expected = "duplicate inference expectation name `duplicate`")]
async fn duplicate_expectation_names_are_rejected() {
    let server = MockInferenceServer::start().await.unwrap();
    let _first = server.expect_response(
        "duplicate",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(200, "first"),
    );
    let _second = server.expect_response(
        "duplicate",
        InferenceRequestMatcher::auxiliary(InferenceEndpoint::Responses),
        ScriptedResponse::text(200, "second"),
    );
}

#[tokio::test]
async fn unsatisfied_expectation_diagnostic_includes_name_and_state() {
    let server = MockInferenceServer::start().await.unwrap();
    let expected = server.expect_response(
        "must receive a foreground turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(200, "unused"),
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        expected.assert_satisfied();
    }))
    .expect_err("unsatisfied expectation must panic");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("must receive a foreground turn"));
    assert!(message.contains("Pending"));
}

#[tokio::test]
async fn echo_mode_collapses_whitespace_in_the_last_user_message() {
    let server = MockInferenceServer::start().await.unwrap();

    let body = post_chat(&server, "a  b\nc").await.text().await.unwrap();

    assert_eq!("Echo: a b c", chat_stream_text(&body));
}

#[tokio::test]
async fn fixed_mode_reconstructs_byte_exact_over_http() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_response(MERMAID_TEXT);

    let body = post_chat(&server, "ignored").await.text().await.unwrap();
    assert_eq!(MERMAID_TEXT, chat_stream_text(&body));

    let body = reqwest::Client::new()
        .post(format!("{}/responses", server.url()))
        .json(&json!({
            "model": "test-model",
            "input": [{ "role": "user", "content": "ignored" }]
        }))
        .send()
        .await
        .expect("POST /v1/responses")
        .text()
        .await
        .unwrap();
    assert_eq!(MERMAID_TEXT, responses_stream_text(&body));

    let body = reqwest::Client::new()
        .post(format!("{}/messages", server.url()))
        .json(&json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": "ignored" }]
        }))
        .send()
        .await
        .expect("POST /v1/messages")
        .text()
        .await
        .unwrap();
    let messages_text: String = sse_data_payloads(&body)
        .iter()
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("content_block_delta"))
        .filter_map(|v| {
            v.get("delta")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    assert_eq!(MERMAID_TEXT, messages_text);
}

#[tokio::test]
async fn settings_404_until_set_then_200() {
    let server = MockInferenceServer::start().await.unwrap();
    let url = format!("{}/settings", server.url());

    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(404, resp.status());

    server.set_settings(json!({ "tips": ["t1"] }));
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(200, resp.status());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(json!({ "tips": ["t1"] }), body);
}

#[tokio::test]
async fn startup_fetch_delay_slows_models_and_settings_then_clears() {
    let server = MockInferenceServer::start().await.unwrap();
    let models_url = format!("{}/models", server.url());
    let settings_url = format!("{}/settings", server.url());

    let delay = Duration::from_millis(600);
    server.set_startup_fetch_delay(delay);

    for url in [&models_url, &settings_url] {
        let started = std::time::Instant::now();
        let resp = reqwest::get(url).await.unwrap();
        assert!(resp.status().is_success() || resp.status() == 404, "{url}");
        assert!(
            started.elapsed() >= delay,
            "{url} returned in {:?}, faster than the injected {delay:?}",
            started.elapsed(),
        );
    }

    server.set_hang(true);
    server.set_startup_fetch_delay(delay);
    let started = std::time::Instant::now();
    tokio::time::timeout(delay * 4, reqwest::get(&models_url))
        .await
        .expect("the delay must replace the hang")
        .unwrap();
    assert!(
        started.elapsed() >= delay,
        "the delay must replace the hang"
    );

    server.clear_startup_fetch_stall();
    let started = std::time::Instant::now();
    reqwest::get(&models_url).await.unwrap();
    assert!(
        started.elapsed() < delay,
        "clearing the delay should make /v1/models fast again, took {:?}",
        started.elapsed(),
    );
}

#[tokio::test]
async fn privacy_coding_data_retention_serves_scripted_denial_then_echoes_and_logs() {
    let server = MockInferenceServer::start().await.unwrap();
    let url = format!("{}/privacy/coding-data-retention", server.url());
    server.enqueue_response(
        "/v1/privacy/coding-data-retention",
        ScriptedResponse::json(403, json!({ "error": "team policy" })),
    );
    let put = || {
        reqwest::Client::new()
            .put(&url)
            .json(&json!({ "codingDataRetentionOptOut": true }))
            .send()
    };

    let resp = put().await.unwrap();
    assert_eq!(403, resp.status());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(json!({ "error": "team policy" }), body);

    let resp = put().await.unwrap();
    assert_eq!(200, resp.status(), "an empty queue falls back to the echo");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(json!({ "codingDataRetentionOptOut": true }), body);

    let entries = server.requests();
    let puts: Vec<_> = entries
        .iter()
        .filter(|e| e.method == "PUT" && e.path == "/v1/privacy/coding-data-retention")
        .collect();
    assert_eq!(2, puts.len(), "the refused write is logged too");
    assert_eq!(
        Some(json!({ "codingDataRetentionOptOut": true })),
        puts.first().unwrap().body
    );
}

#[tokio::test]
async fn scripted_responses_serve_fifo_per_path_then_fall_back() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::text(401, "Unauthorized"),
    );
    server.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::json(500, json!({ "error": { "message": "boom" } })),
    );

    let resp = post_chat(&server, "hi").await;
    assert_eq!(401, resp.status());
    assert_eq!("Unauthorized", resp.text().await.unwrap());

    let resp = post_chat(&server, "hi").await;
    assert_eq!(500, resp.status());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(json!({ "error": { "message": "boom" } }), body);

    let body = post_chat(&server, "ping pong").await.text().await.unwrap();
    assert_eq!("Echo: ping pong", chat_stream_text(&body));

    server.enqueue_response("/v1/chat/completions", ScriptedResponse::text(503, "later"));
    let resp = reqwest::Client::new()
        .post(format!("{}/responses", server.url()))
        .json(&json!({
            "model": "test-model",
            "input": [{ "role": "user", "content": "hi there" }]
        }))
        .send()
        .await
        .expect("POST /v1/responses");
    assert_eq!(200, resp.status());
    assert_eq!(
        "Echo: hi there ",
        responses_stream_text(&resp.text().await.unwrap())
    );
}

#[tokio::test]
async fn scripted_response_takes_precedence_over_required_auth() {
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("test-model")],
        "secret-token",
    )
    .await
    .unwrap();
    server.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::text(200, "scripted"),
    );

    let resp = post_chat(&server, "hi").await;
    assert_eq!(200, resp.status());
    assert_eq!("scripted", resp.text().await.unwrap());

    let resp = post_chat(&server, "hi").await;
    assert_eq!(401, resp.status());
}

#[tokio::test]
async fn scripted_response_headers_reach_the_client() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut rate_limited = ScriptedResponse::text(429, "slow down");
    rate_limited
        .headers
        .push(("retry-after".to_string(), "7".to_string()));
    server.enqueue_response("/v1/chat/completions", rate_limited);

    let resp = post_chat(&server, "hi").await;
    assert_eq!(429, resp.status());
    assert_eq!(
        Some("7"),
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
    );
    assert_eq!("slow down", resp.text().await.unwrap());
}

#[tokio::test]
async fn scripted_sse_preserves_event_names_and_order() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::sse(vec![
            SseEvent::with_event("custom.kind", "{\"a\":1}"),
            SseEvent::data("{\"b\":2}"),
        ]),
    );

    let body = post_chat(&server, "hi").await.text().await.unwrap();
    let named_then_plain = body
        .find("event: custom.kind")
        .zip(body.find("data: {\"b\":2}"))
        .is_some_and(|(named, plain)| named < plain);
    assert!(
        body.contains("event: custom.kind") && body.contains("data: {\"a\":1}"),
        "named event must carry both fields, got:\n{body}"
    );
    assert!(named_then_plain, "events must be served in order:\n{body}");
}

#[tokio::test]
async fn matched_barriers_cover_all_endpoints_and_body_modes() {
    let sse = ScriptedResponse::sse(vec![SseEvent::data("chunk"), SseEvent::data("terminal")]);
    let cases = [
        (InferenceEndpoint::ChatCompletions, sse.clone()),
        (InferenceEndpoint::Responses, sse.clone()),
        (InferenceEndpoint::Messages, sse),
        (
            InferenceEndpoint::Responses,
            ScriptedResponse::sse(Vec::new()),
        ),
        (
            InferenceEndpoint::Responses,
            ScriptedResponse::json(200, json!({ "ok": true })),
        ),
        (
            InferenceEndpoint::Responses,
            ScriptedResponse::text(200, "raw body"),
        ),
    ];
    for (endpoint, response) in cases {
        let label = format!("{endpoint:?} {response:?}");
        let server = MockInferenceServer::start().await.unwrap();
        let mut expected = server.expect_response_blocked(
            label.clone(),
            InferenceRequestMatcher::foreground(endpoint),
            response,
        );
        let request = read_foreground(&server, endpoint, "body-gate", "hello");
        tokio::pin!(request);
        tokio::select! {
            response = &mut request => panic!("{label} completed before release: {:?}", response.0),
            _ = expected.wait_blocked() => {}
        }
        expected.release();
        let (status, _) = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap_or_else(|_| panic!("{label} did not complete after release"));
        assert_eq!(200, status.as_u16());
        expected.wait_satisfied().await;
    }
}

#[tokio::test]
async fn matched_expectation_precedes_auth_then_auth_resumes() {
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("test-model")],
        "secret-token",
    )
    .await
    .unwrap();
    let mut expected = server.expect_response(
        "auth bypass",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::text(218, "matched without auth"),
    );

    let (status, body) = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "auth-bypass",
        "first",
    )
    .await;
    assert_eq!(
        (218, "matched without auth"),
        (status.as_u16(), body.as_str())
    );
    expected.wait_satisfied().await;

    let (status, _) = read_foreground(
        &server,
        InferenceEndpoint::Responses,
        "auth-fallback",
        "second",
    )
    .await;
    assert_eq!(401, status);
}

#[tokio::test]
async fn compatibility_completion_gate_holds_sse_terminals_but_not_json_or_raw() {
    enum Gate {
        Holds,
        Passes,
    }
    let cases = [
        (Gate::Holds, InferenceEndpoint::ChatCompletions, None),
        (Gate::Holds, InferenceEndpoint::Responses, None),
        (Gate::Holds, InferenceEndpoint::Messages, None),
        (
            Gate::Holds,
            InferenceEndpoint::Responses,
            Some(ScriptedResponse::sse(vec![
                SseEvent::data("chunk"),
                SseEvent::data("terminal"),
            ])),
        ),
        (
            Gate::Passes,
            InferenceEndpoint::Responses,
            Some(ScriptedResponse::json(219, json!({ "ok": true }))),
        ),
        (
            Gate::Passes,
            InferenceEndpoint::Responses,
            Some(ScriptedResponse::text(220, "raw body")),
        ),
    ];
    for (gate, endpoint, scripted) in cases {
        let label = format!("{endpoint:?} {scripted:?}");
        let server = MockInferenceServer::start().await.unwrap();
        server.hold_agent_completions();
        match scripted {
            Some(response) => server.enqueue_response(endpoint.path(), response),
            None => server.set_agent_turns(["turn".to_owned()]),
        }
        let request = read_foreground(&server, endpoint, "compat-gate", "hello");
        tokio::pin!(request);
        match gate {
            Gate::Holds => {
                let probe = tokio::time::timeout(Duration::from_millis(50), &mut request).await;
                assert!(probe.is_err(), "{label} bypassed the completion gate");
                server.release_agent_completions();
            }
            Gate::Passes => {}
        }
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap_or_else(|_| panic!("{label} did not complete"));
    }
}

#[tokio::test]
async fn request_log_captures_arbitrary_headers() {
    let server = MockInferenceServer::start().await.unwrap();

    reqwest::Client::new()
        .post(format!("{}/chat/completions", server.url()))
        .header("authorization", "Bearer log-me")
        .header("x-test-marker", "zap")
        .json(&json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions");

    let entry = server.requests().pop().expect("one logged request");
    assert_eq!(Some("zap"), entry.header("x-test-marker"));
    assert_eq!(Some("zap"), entry.header("X-Test-Marker"));
    assert_eq!(Some("Bearer log-me"), entry.header("authorization"));
    assert_eq!(Some("Bearer log-me"), entry.authorization.as_deref());
    assert_eq!(None, entry.header("x-absent"));
}

#[tokio::test]
async fn required_auth_rejects_a_missing_bearer_and_admits_the_token() {
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("test-model")],
        "secret-token",
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", server.url());
    let req_body = json!({
        "model": "test-model",
        "messages": [{ "role": "user", "content": "hi there" }]
    });

    let resp = client.post(&url).json(&req_body).send().await.unwrap();
    assert_eq!(401, resp.status());
    let resp = client
        .post(&url)
        .header("authorization", "Bearer secret-token")
        .json(&req_body)
        .send()
        .await
        .unwrap();
    assert_eq!(200, resp.status());
    assert_eq!(
        "Echo: hi there",
        chat_stream_text(&resp.text().await.unwrap())
    );
}

#[tokio::test]
async fn session_writeback_routes_are_logged_and_gated_by_required_auth() {
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("test-model")],
        "secret-token",
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let origin = server.origin();
    let data =
        json!({ "messages": [{ "content": "x" }], "metadata": { "title": "Manual", "cwd": "/" } });
    let upsert = json!({ "session": { "title": "Manual" }, "agentId": "a" });

    let denied = client
        .post(format!("{origin}/sessions/abc/data"))
        .json(&data)
        .send()
        .await
        .unwrap();
    let saved = client
        .post(format!("{origin}/sessions/abc/data"))
        .header("authorization", "Bearer secret-token")
        .json(&data)
        .send()
        .await
        .unwrap();
    let upserted = client
        .put(format!("{origin}/sessions/abc"))
        .header("authorization", "Bearer secret-token")
        .json(&upsert)
        .send()
        .await
        .unwrap();

    let logged: Vec<(String, String, Option<Value>)> = server
        .requests()
        .into_iter()
        .filter(|entry| entry.path.starts_with("/sessions/"))
        .map(|entry| (entry.method, entry.path, entry.body))
        .collect();
    assert_eq!(
        (
            401,
            200,
            200,
            vec![
                (
                    "POST".to_owned(),
                    "/sessions/abc/data".to_owned(),
                    Some(data)
                ),
                ("PUT".to_owned(), "/sessions/abc".to_owned(), Some(upsert)),
            ]
        ),
        (
            denied.status().as_u16(),
            saved.status().as_u16(),
            upserted.status().as_u16(),
            logged
        )
    );
}

#[tokio::test]
async fn scripted_tool_call_is_served_then_its_result_advances_to_the_reply() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![
        Conversation::nth(1)
            .calls([MockToolCall::new(
                Tool::Read,
                json!({ "target_file": "a.rs" }),
            )])
            .reply("DONE"),
    ]);
    let call_id = mock_call_id(1, 1);
    let endpoint = InferenceEndpoint::ChatCompletions;
    let mut with_result = chat_body("s");
    with_result
        .get_mut("messages")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(json!({ "role": "tool", "tool_call_id": call_id, "content": "fn main" }));

    let call_turn = post_chat_turn(&server, "s").await;
    let reply_turn = read_foreground_body(&server, endpoint, "turn", with_result)
        .await
        .1;

    let first_frame: Value =
        serde_json::from_str(sse_data_payloads(&call_turn).first().unwrap()).unwrap();
    let call = first_frame
        .pointer("/choices/0/delta/tool_calls/0")
        .unwrap();
    assert_eq!(
        (
            Some(call_id.as_str()),
            Some("read_file"),
            "DONE".to_owned(),
            Some("fn main".to_owned()),
        ),
        (
            call.pointer("/id").unwrap().as_str(),
            call.pointer("/function/name").unwrap().as_str(),
            chat_stream_text(&reply_turn),
            server.conversation(1).unwrap().tool_result(&call_id),
        )
    );
}

#[tokio::test]
async fn conversation_reads_the_last_reply_and_the_tool_calls_the_agent_carried_back() {
    let server = MockInferenceServer::start().await.unwrap();
    let mut body = chat_body("s");
    body.get_mut("messages")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .extend([
            json!({ "role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"target_file\":\"a.rs\"}" }
        }] }),
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "fn main" }),
            json!({ "role": "assistant", "content": "DONE" }),
            json!({ "role": "user", "content": "more" }),
        ]);
    read_foreground_body(&server, InferenceEndpoint::ChatCompletions, "turn", body).await;

    let conversation = server.conversation(1).unwrap();

    assert_eq!(
        (Some("DONE".to_owned()), true, false),
        (
            conversation.last_reply(),
            conversation.saw_tool_call(Tool::Read),
            conversation.saw_tool_call(Tool::Shell),
        )
    );
}

#[tokio::test]
async fn expectation_precedes_the_conversation_script() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![Conversation::nth(1).reply("scripted")]);
    let expectation = server.expect_response(
        "first turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::ChatCompletions),
        ScriptedResponse::text(200, "expected first"),
    );

    let first = post_chat_turn(&server, "parent").await;
    let second = post_chat_turn(&server, "parent").await;

    assert_eq!(
        ("expected first", "scripted"),
        (first.as_str(), chat_stream_text(&second).as_str())
    );
    expectation.assert_satisfied();
}

#[tokio::test]
#[should_panic(expected = "conversation scripts departed from")]
async fn dropping_the_server_with_an_unserved_script_panics() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![Conversation::nth(1).reply("never reached")]);
    drop(server);
}

#[tokio::test]
async fn finishing_the_scripts_reports_them_once_and_disarms_the_drop_check() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![Conversation::nth(1).reply("never reached")]);

    let finished = server.finish_scripts();

    assert_eq!(
        (
            vec![ScriptViolation::Unfinished {
                conversation: 1,
                replies_served: 0,
                superseded: 0,
                entry_count: 1,
            }],
            Vec::new()
        ),
        (finished, server.script_violations())
    );
}

#[tokio::test]
async fn status_failure_refuses_over_the_wire_with_retry_after_and_is_on_the_log() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![
        Conversation::nth(1)
            .refuse(
                StatusFailure::new(503)
                    .with_retry_after(Duration::from_secs(8))
                    .with_body("upstream down"),
            )
            .reply("PONG"),
    ]);

    let refused = send_chat_turn(&server, "s").await.unwrap();
    let retry_after = refused
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let refused = (refused.status().as_u16(), refused.text().await.unwrap());
    let answered = post_chat_turn(&server, "s").await;

    assert_eq!(
        (
            Some("8".to_owned()),
            (503, "upstream down".to_owned()),
            "PONG".to_owned(),
            vec![Some(ObservedFailure::Status(503)), None]
        ),
        (
            retry_after,
            refused,
            chat_stream_text(&answered),
            observed_failures(&server)
        )
    );
}

#[tokio::test]
async fn stalled_answer_arrives_after_the_hold_and_is_on_the_log() {
    const WIRE_HOLD: Duration = Duration::from_millis(300);
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![Conversation::nth(1).stall(WIRE_HOLD).reply("PONG")]);
    let started = std::time::Instant::now();

    let answered = post_chat_turn(&server, "s").await;

    assert!(started.elapsed() >= WIRE_HOLD);
    assert_eq!(
        ("PONG".to_owned(), vec![Some(ObservedFailure::Stalled)]),
        (chat_stream_text(&answered), observed_failures(&server))
    );
}

#[tokio::test]
async fn drop_closes_the_connection_before_the_head_then_the_script_answers() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_conversations(vec![Conversation::nth(1).drop_connection(1).reply("PONG")]);

    let error = send_chat_turn(&server, "s")
        .await
        .expect_err("the head never arrives");
    let answered = post_chat_turn(&server, "s").await;

    assert!(error.is_request(), "{error}");
    assert_eq!(
        (
            "PONG".to_owned(),
            vec![Some(ObservedFailure::Dropped), None]
        ),
        (chat_stream_text(&answered), observed_failures(&server))
    );
}

#[tokio::test]
async fn auth_rejection_and_concurrency_cap_are_on_the_log_as_status_failures() {
    let server = MockInferenceServer::start_with_required_auth(
        vec![MockModelEntry::new("test-model")],
        "secret-token",
    )
    .await
    .unwrap();
    server.set_inference_concurrency_cap(0, Duration::ZERO, 1);
    let client = reqwest::Client::new();
    let url = endpoint_url(&server, InferenceEndpoint::ChatCompletions);
    let body = json!({ "model": "test-model", "messages": [{ "role": "user", "content": "hi" }] });

    let unauthorized = client.post(&url).json(&body).send().await.unwrap();
    let capped = client
        .post(&url)
        .header("authorization", "Bearer secret-token")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(
        (
            401,
            429,
            vec![
                Some(ObservedFailure::Status(401)),
                Some(ObservedFailure::Status(429))
            ]
        ),
        (
            unauthorized.status().as_u16(),
            capped.status().as_u16(),
            observed_failures(&server)
        )
    );
}

#[tokio::test]
async fn tls_server_serves_models_only_to_a_client_trusting_the_throwaway_ca() {
    let server = MockInferenceServer::start_tls().await.unwrap();
    let url = format!("{}/models", server.url());

    // An ambient HTTPS_PROXY would intercept the loopback handshake and
    // make a proxy failure look like a CA or server bug.
    let untrusting = reqwest::Client::builder()
        .use_rustls_tls()
        .no_proxy()
        .build()
        .expect("untrusting client");
    let rejected = untrusting
        .get(&url)
        .send()
        .await
        .expect_err("unknown CA must fail the handshake");
    assert!(rejected.is_connect(), "{rejected:?}");
    assert_eq!(0, server.request_count_for("/v1/models"));

    let ca_pem = std::fs::read(server.ca_pem_path().expect("TLS server exposes its CA")).unwrap();
    let trusting = reqwest::Client::builder()
        .use_rustls_tls()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).unwrap())
        .build()
        .unwrap();
    let resp = trusting
        .get(&url)
        .send()
        .await
        .expect("GET /v1/models over TLS");

    assert_eq!(200, resp.status());
    assert_eq!(1, server.request_count_for("/v1/models"));
}
