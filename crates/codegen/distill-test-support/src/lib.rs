// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Shared test utilities for distill crates.
//!
//! Provides:
//! - [`GrokStdioClient`]: ACP client that drives `grok agent stdio` as a subprocess, with a scripted [`ClientPolicy`] (set through [`SpawnOptions`]) and a typed transcript ([`TranscriptEntry`])
//! - [`AcpTestClient`]: writes verbatim JSON-RPC lines and reads until the response with the same id, for wire shapes the typed client can't produce (Foundation `\/` methods, string UUID ids)
//! - [`MockInferenceServer`]: Mock `/v1/chat/completions`, `/v1/responses`, and `/v1/messages` with a request log and per conversation scripts
//! - [`Conversation`]: Per conversation tool calls and replies, with [`Tool`] picking the name the request offers, turns pinned to a request, and failures answering in place of the content
//! - [`ObservedFailure`]: What the mock did to a request in place of answering it plainly, on its log entry
//! - [`leader::LeaderStdioClient`]: ACP client that drives `grok agent --leader stdio` (unix)
//! - [`TestSandbox`]: Own isolated paths, hermetic child env, optional git setup, diagnostics
//! - [`TestProcess`]: Own detached child lifecycle, process-tree teardown, bounded output tails
//! - [`run_headless`]: Run `grok -p` against the mock server and capture output
//! - [`git_workdir`]: Create a git-initialized [`TestSandbox`]
//! - [`grok_binary`]: Resolve the grok binary path (GROK_BINARY env or cargo_bin)
//! - [`spawn_counting_server`]: Connection-counting HTTP/1.1 server for wire/pooling tests
//! - [`uds_proxy::UdsProxy`]: Frame-aware fault-injection proxy for leader IPC sockets (unix)
//! - [`ResourceSnapshot`]: RSS/threads/fds sampling for soak tests
//! - [`MockOtelServer`]: OTLP/HTTP collector recording the shell's exported logs and metrics as typed events in its [`OtelRecorder`]
//! - [`OtelRecorder`]: the mock OTLP server's log, which a test reads and waits on, or fills from its own OTLP transport
//! - [`MockManagedConfigServer`]: mock of the server the managed configuration supervisor fetches policy from
//! - [`ManagedPolicy`]: the configuration row the mock server serves for one principal, signed by a [`TestSigningKey`] or not
#![deny(clippy::indexing_slicing)]
/// Multiply a harness timeout by `GROK_TEST_TIMEOUT_SCALE` (positive integer, default 1).
/// CI lanes on shared runner pools raise it so pool load slows tests instead of failing them (see the Distill merge CI workflow).
pub fn scaled(base: std::time::Duration) -> std::time::Duration {
    let scale = std::env::var("GROK_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(1);
    base * scale
}
/// Runs a test body inside a `tokio::task::LocalSet`, which the stdio clients require: their connections are
/// not `Send`, so their tasks are spawned locally.
pub async fn run_in_local_set<F, Fut>(body: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    tokio::task::LocalSet::new().run_until(body()).await;
}
mod acp_agent_connection;
mod acp_agent_process;
mod acp_ask_user_question;
mod acp_client;
mod acp_hold_registry;
mod acp_policy;
mod acp_scripted_client;
mod acp_test_client;
mod acp_transcript;
mod bounded_log;
mod conversation;
mod conversation_replay;
mod conversation_script;
pub mod counting_server;
pub mod env;
mod failure;
mod feedback_endpoint;
mod gated_upload_proxy;
pub mod headless;
mod inference_override;
mod inference_request;
mod inference_route;
#[cfg(unix)]
pub mod leader;
mod loopback;
#[cfg(test)]
mod loopback_client;
mod mock_otel_server;
pub mod mock_server;
mod mock_server_tls;
mod model_reply;
mod otel_decode;
mod otel_event;
#[cfg(test)]
mod otel_fixtures;
mod otel_recorder;
pub mod process;
mod request_log;
pub mod resources;
pub mod sandbox;
pub mod scripted;
pub mod sse;
mod storage_endpoint;
mod telemetry_events;
mod tool_call_turn;
mod tools;
#[cfg(unix)]
pub mod uds_proxy;
mod watched;
pub use acp_client::{GrokStdioClient, SpawnOptions};
pub use acp_policy::{
    ClientPolicy, ElicitationDecision, Interactivity, PermissionDecision, QuestionDecision,
    RequestPolicy, TrustDecision,
};
pub use acp_test_client::AcpTestClient;
pub use acp_transcript::TranscriptEntry;
pub use conversation::ReadConversation;
pub use conversation_script::{Conversation, MockToolCall, ScriptViolation, mock_call_id};
pub use counting_server::spawn_counting_server;
pub use env::{EnvGuard, git_workdir, grok_binary, isolate_grok_env};
pub use failure::{
    CUT_REPLY, DOOM_LOOP_CHECK_HEADER, DOOM_LOOP_TRIGGER, ErrorPosition, LOOPING_REPLY,
    ObservedFailure, StatusFailure, StreamError,
};
pub use headless::{
    HeadlessResult, assert_headless_success, assert_no_crashes, run_headless,
    run_headless_in_sandbox, run_headless_in_sandbox_borrowed,
    run_headless_in_sandbox_borrowed_with_env, run_headless_in_sandbox_with_env,
    run_headless_with_env, stderr_tail,
};
pub use inference_override::{InferenceExpectation, InferenceRequestMatcher};
pub use inference_request::{DEFAULT_MODEL, InferenceEndpoint};
#[cfg(unix)]
pub use leader::LeaderFixture;
pub use mock_otel_server::MockOtelServer;
pub use mock_server::{
    FeedbackPost, GatedUploadProxy, MockInferenceServer, MockModelEntry, ScriptedResponse,
    SseEvent, StorageUpload,
};
pub use otel_event::{
    OtelAttributes, OtelBody, OtelDecodeError, OtelEvent, OtelExport, OtelFault, OtelLogRecord,
    OtelMetricData, OtelMetricPoint, OtelNumber, OtelSignal, OtelTemporality, OtelUnreadBody,
};
pub use otel_recorder::{OtelRecorder, OtelRecorderError};
#[cfg(unix)]
pub use process::process_has_exited_without_reap;
pub use process::{
    TestOutput, TestOutputSnapshot, TestProcess, TestProcessConfig, TestProcessState,
    TestProcessStderr, TestProcessStdout, TestProcessTermination, TestProcessTree, TestStdin,
};
pub use resources::{ResourceGrowth, ResourceSnapshot, RssMeasurement, RssOutcome, RssSampler};
pub use sandbox::{TestSandbox, TestSandboxBuilder};
pub use tools::Tool;
