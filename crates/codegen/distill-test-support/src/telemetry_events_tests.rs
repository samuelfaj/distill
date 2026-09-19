use serde_json::{Value, json};

use crate::MockInferenceServer;

async fn post_events(server: &MockInferenceServer, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/events", server.url()))
        .json(body)
        .send()
        .await
        .expect("POST /v1/events")
}

#[tokio::test]
async fn post_v1_events_flattens_batches_in_arrival_order() {
    let server = MockInferenceServer::start().await.unwrap();
    let first = json!({ "event_name": "grok-shell-user_feedback", "event_value": 1 });
    let second = json!({ "event_name": "grok-shell-session_start", "event_value": 1 });
    let third = json!({ "event_name": "grok-shell-turn_end", "event_value": 1 });

    let response = post_events(
        &server,
        &json!({ "api_key": "pty-capture", "events": [first.clone(), second.clone()] }),
    )
    .await;
    assert_eq!(reqwest::StatusCode::OK, response.status());
    post_events(&server, &json!({ "events": [third.clone()] })).await;

    assert_eq!(vec![first, second, third], server.telemetry_events());
}

#[tokio::test]
async fn post_v1_events_keeps_an_unparseable_body_visible() {
    let server = MockInferenceServer::start().await.unwrap();

    let response = reqwest::Client::new()
        .post(format!("{}/events", server.url()))
        .body("not json")
        .send()
        .await
        .expect("POST /v1/events");

    assert_eq!(reqwest::StatusCode::OK, response.status());
    assert_eq!(
        vec![json!({ "unparsed_events_body": "not json" })],
        server.telemetry_events()
    );
}
