// Modified for Distill by Samuel Fajreldines, 2026.
//! An image the client accepts but the server rejects is stripped within the failing turn.
//! The strip persists so the next turn does not resend the image.

mod acp_harness;

use acp_harness::{AutoApproveClient, RPC_TIMEOUT, connect_and_auth, prompt_turn, run_agent_test};
use agent_client_protocol::{self as acp, Agent as _};
use base64::Engine as _;
use distill_shell::sampling::{ContentPart, ConversationItem};
use distill_shell::session::info::Info;
use distill_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use distill_test_support::ScriptedResponse;
use serde_json::json;

const SESSION_ID: &str = "poisoned-image-session";

/// Valid PNG above the dimension floor, so only the server's 400 rejects it.
fn poisoned_image_data_uri() -> String {
    let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
        image::ImageBuffer::from_fn(32, 32, |x, y| image::Rgb([x as u8, y as u8, 0]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode fixture png");
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    )
}

/// Seed `chat_history.jsonl` with the poisoned image.
async fn seed_poisoned_session(cwd: &std::path::Path, image_url: &str) -> Info {
    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: cwd.to_string_lossy().into_owned(),
    };
    // Same adapter `session/load` uses.
    let storage = JsonlStorageAdapter::new();
    storage
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .expect("init session");
    let mut user = match ConversationItem::user("what is in this image?") {
        ConversationItem::User(u) => u,
        _ => unreachable!(),
    };
    user.content.push(ContentPart::Image {
        url: std::sync::Arc::<str>::from(image_url),
    });
    storage
        .append_chat_message(&info, &ConversationItem::User(user))
        .await
        .expect("append user message");
    storage
        .append_chat_message(&info, &ConversationItem::assistant("A test pattern."))
        .await
        .expect("append assistant message");
    info
}

/// Sampling bodies from the main turn, excluding turn-summary requests.
/// The agent may call either chat completions or the responses API.
fn sampling_bodies(server: &distill_test_support::MockInferenceServer) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions" || r.path == "/v1/responses")
        .filter(|r| {
            !r.header("x-grok-req-id")
                .is_some_and(|id| id.starts_with("xai-turn-summary-"))
        })
        .map(|r| r.body.map(|b| b.to_string()).unwrap_or_default())
        .collect()
}

#[test]
fn poisoned_image_session_recovers_within_the_failing_turn() {
    run_agent_test(|cwd, server| async move {
        let image_url = poisoned_image_data_uri();
        let image_marker = &image_url[image_url.len() - 48..];
        let info = seed_poisoned_session(&cwd, &image_url).await;

        let (conn, _init) = connect_and_auth(AutoApproveClient, "test-client").await;
        tokio::time::timeout(
            RPC_TIMEOUT,
            conn.load_session(acp::LoadSessionRequest::new(
                info.id.clone(),
                cwd.to_path_buf(),
            )),
        )
        .await
        .expect("session/load timed out")
        .expect("session/load failed");

        // The 400 fires once on the foreground turn; auxiliary calls must not
        // consume it. The strip-retry falls through to the mock's default echo.
        let invalid_image = ScriptedResponse::json(
            400,
            json!({
                "code": "invalid_image",
                "error": "Base64 string of provided image cannot be decoded.",
            }),
        );
        let _reject_chat = server.expect_response(
            "invalid-image-chat",
            distill_test_support::InferenceRequestMatcher::foreground(
                distill_test_support::InferenceEndpoint::ChatCompletions,
            ),
            invalid_image.clone(),
        );
        let _reject_responses = server.expect_response(
            "invalid-image-responses",
            distill_test_support::InferenceRequestMatcher::foreground(
                distill_test_support::InferenceEndpoint::Responses,
            ),
            invalid_image,
        );

        prompt_turn(&conn, &info.id, "hi").await;

        let bodies = sampling_bodies(&server);
        let rejected = bodies
            .iter()
            .position(|body| body.contains(image_marker))
            .expect("a sampling attempt must carry the poisoned image");
        assert!(
            rejected + 1 < bodies.len(),
            "strip-retry must follow the rejected attempt, saw {} request(s)",
            bodies.len()
        );
        let retry = &bodies[rejected + 1];
        assert!(
            !retry.contains(image_marker),
            "strip-retry must not resend the poisoned image"
        );
        assert!(
            retry.contains("[image removed"),
            "strip-retry must carry the placeholder so the model knows an image was there"
        );

        // Persist: next turn must not resend or strip-retry.
        let turn_one_requests = sampling_bodies(&server).len();
        prompt_turn(&conn, &info.id, "hi again").await;
        let bodies = sampling_bodies(&server);
        assert_eq!(
            bodies.len(),
            turn_one_requests + 1,
            "second turn must succeed on its first attempt (no retry cycle)"
        );
        let second_turn = &bodies[bodies.len() - 1];
        assert!(
            !second_turn.contains(image_marker),
            "second turn must not resend the stripped image"
        );

        // Persist is async; poll for the placeholder.
        let chat_file = session_chat_jsonl();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let on_disk = loop {
            let contents = tokio::fs::read_to_string(&chat_file)
                .await
                .expect("read session chat jsonl");
            if contents.contains("[image removed") || tokio::time::Instant::now() >= deadline {
                break contents;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(
            !on_disk.contains(image_marker),
            "persisted conversation must not retain the stripped image bytes"
        );
        assert!(
            on_disk.contains("[image removed"),
            "persisted conversation must carry the strip placeholder"
        );
    });
}

/// The session's `chat_history.jsonl`; the sessions dir name encodes the cwd in an internal format, so we scan for the file.
fn session_chat_jsonl() -> std::path::PathBuf {
    let sessions = std::path::PathBuf::from(std::env::var("GROK_HOME").expect("GROK_HOME set"))
        .join("sessions");
    std::fs::read_dir(&sessions)
        .expect("read sessions dir")
        .filter_map(|e| Some(e.ok()?.path().join(SESSION_ID).join("chat_history.jsonl")))
        .find(|p| p.exists())
        .expect("session chat_history.jsonl not found")
}
