use serde_json::{Value, json};

use crate::MockInferenceServer;

async fn post_feedback(server: &MockInferenceServer, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/feedback", server.url()))
        .header("authorization", "Bearer mock-feedback-token")
        .json(body)
        .send()
        .await
        .expect("POST /v1/feedback")
}

#[tokio::test]
async fn post_v1_feedback_records_body_and_answers_feedback_id() {
    let server = MockInferenceServer::start().await.unwrap();
    let body = json!({
        "sessionId": "session-1",
        "clientType": "tui",
        "feedbackText": "the todo panel clips its last row",
    });

    let response = post_feedback(&server, &body).await;

    assert_eq!(reqwest::StatusCode::OK, response.status());
    assert_eq!(
        json!({ "feedbackId": "mock-feedback-1", "createdAt": "2026-01-01T00:00:00Z" }),
        response.json::<Value>().await.unwrap()
    );
    let posts = server.feedback_posts();
    let [post] = posts.as_slice() else {
        panic!("expected one feedback post, got {}", posts.len());
    };
    assert_eq!(body, post.body);
    assert_eq!(
        Some("Bearer mock-feedback-token"),
        post.authorization.as_deref()
    );
}

#[tokio::test]
async fn post_v1_feedback_failure_toggle_returns_500_and_still_records() {
    let server = MockInferenceServer::start().await.unwrap();

    server.set_feedback_failure(true);
    let response = post_feedback(&server, &json!({ "feedbackText": "first" })).await;
    assert_eq!(
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        response.status()
    );
    assert_eq!(
        json!({ "error": "scripted feedback failure" }),
        response.json::<Value>().await.unwrap()
    );
    assert_eq!(1, server.feedback_posts().len());

    server.set_feedback_failure(false);
    let response = post_feedback(&server, &json!({ "feedbackText": "second" })).await;
    assert_eq!(reqwest::StatusCode::OK, response.status());
    assert_eq!(
        Some(&json!("mock-feedback-2")),
        response.json::<Value>().await.unwrap().get("feedbackId")
    );
    assert_eq!(2, server.feedback_posts().len());
}

#[tokio::test]
async fn post_v1_feedback_keeps_an_unparseable_body_visible() {
    let server = MockInferenceServer::start().await.unwrap();

    let response = reqwest::Client::new()
        .post(format!("{}/feedback", server.url()))
        .body("not json")
        .send()
        .await
        .expect("POST /v1/feedback");

    assert_eq!(reqwest::StatusCode::OK, response.status());
    assert_eq!(
        Some(&json!({ "unparsed_feedback_body": "not json" })),
        server.feedback_posts().first().map(|post| &post.body)
    );
}

/// An image-bearing report can exceed the capture cap; the text and metadata a test asserts on must survive it.
#[tokio::test]
async fn post_v1_feedback_over_cap_drops_only_the_images() {
    let server = MockInferenceServer::start().await.unwrap();
    let body = json!({
        "feedbackText": "screenshot attached",
        "metadata": { "structured_feedback": { "source": "write" } },
        "images": [{ "mimeType": "image/png", "data": "A".repeat(300 * 1024) }],
    });

    let response = post_feedback(&server, &body).await;

    assert_eq!(reqwest::StatusCode::OK, response.status());
    assert_eq!(
        Some(&json!({
            "feedbackText": "screenshot attached",
            "metadata": { "structured_feedback": { "source": "write" } },
            "images": null,
        })),
        server.feedback_posts().first().map(|post| &post.body)
    );
}
