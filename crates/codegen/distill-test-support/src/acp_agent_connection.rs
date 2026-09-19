// Modified for Distill by Samuel Fajreldines, 2026.
//! The typed `agent-client-protocol` connection a test client holds to an agent child: the pipes, the
//! [`ScriptedClient`] that answers the agent, and the reader, io, and request handler tasks, which stop with
//! the connection. Every request shape a test client sends lives here once, untimed; [`timed`] and
//! [`timed_ok`] are the budget each client puts on top.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::{self as acp, Agent as _};
use futures_util::future::LocalBoxFuture;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::AbortOnDropHandle;
use distill_acp_lib::LineBufferedRead;

use crate::acp_policy::{ClientPolicy, Interactivity};
use crate::acp_scripted_client::ScriptedClient;
use crate::process::TestProcess;
use crate::scaled;

const API_KEY_AUTH_METHOD: &str = "xai.api_key";
/// The non-interactive session-login method the agent advertises when a cached `auth.json` login is
/// present and no first-party env key is set.
const CACHED_TOKEN_AUTH_METHOD: &str = "cached_token";

/// Prefer the env-key method, then the cached login; both authenticate without a browser. Returns
/// `None` when the agent offers only interactive methods, which a headless test cannot complete.
fn select_auth_method(methods: &[acp::AuthMethod]) -> Option<acp::AuthMethodId> {
    let by_id = |wanted: &str| {
        methods
            .iter()
            .find(|method| method.id().0.as_ref() == wanted)
            .map(|method| method.id().clone())
    };
    by_id(API_KEY_AUTH_METHOD).or_else(|| by_id(CACHED_TOKEN_AUTH_METHOD))
}

/// The `client_capabilities.meta` that advertises `x.ai/folderTrust.interactive`, so the agent knows
/// this client can answer an interactive folder-trust prompt.
fn interactive_trust_capability() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({ "x.ai/folderTrust": { "interactive": true } })
        .as_object()
        .cloned()
        .expect("object literal is a JSON object")
}

/// Built inside a `tokio::task::LocalSet`: the connection is not `Send`, so its tasks are spawned locally.
pub(crate) struct AgentConnection {
    conn: acp::ClientSideConnection,
    handler: ScriptedClient,
    /// Aborted on drop so the connection loop and the pipe writer stop with the connection.
    _io_task: AbortOnDropHandle<acp::Result<()>>,
    /// Cancels, on drop, the line reader and every request handler spawned through the connection's spawn
    /// callback, so a handler still waiting on a hold stops with the connection.
    _stop_spawned_tasks: DropGuard,
}

impl AgentConnection {
    /// Takes the child's piped stdin and stdout.
    pub(crate) fn connect(process: &mut TestProcess, policy: ClientPolicy) -> Self {
        let outgoing = process
            .take_stdin()
            .expect("child stdin missing")
            .compat_write();
        let incoming = process
            .take_stdout()
            .expect("child stdout missing")
            .compat();

        let stop = CancellationToken::new();
        let spawn_until_stopped = {
            let stop = stop.clone();
            move |future: LocalBoxFuture<'static, ()>| {
                tokio::task::spawn_local(stop.clone().run_until_cancelled_owned(future));
            }
        };
        let handler = ScriptedClient::new(policy);
        let incoming = LineBufferedRead::new(incoming, &spawn_until_stopped);
        let (conn, handle_io) = acp::ClientSideConnection::new(
            handler.clone(),
            outgoing,
            incoming,
            spawn_until_stopped,
        );
        AgentConnection {
            conn,
            handler,
            _io_task: AbortOnDropHandle::new(tokio::task::spawn_local(handle_io)),
            _stop_spawned_tasks: stop.drop_guard(),
        }
    }

    pub(crate) fn handler(&self) -> &ScriptedClient {
        &self.handler
    }

    /// `initialize` as a test client, then `authenticate` with the `xai.api_key` method in headless
    /// mode. The client advertises `nonInteractive` per its [`Interactivity`]: `Headless` (the default)
    /// stays non-interactive; `Interactive` opts in so the agent forwards reverse interactions such as
    /// MCP elicitation instead of auto-cancelling them. An agent that offers no non-interactive auth
    /// method is an error naming the methods it offered.
    pub(crate) async fn initialize_and_authenticate(&self) -> acp::Result<acp::InitializeResponse> {
        let mut capabilities = acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false);
        if self.handler.advertises_interactive_trust() {
            capabilities = capabilities.meta(interactive_trust_capability());
        }
        let non_interactive = matches!(self.handler.interactivity(), Interactivity::Headless);
        let response = self
            .conn
            .initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                    .client_capabilities(capabilities)
                    .meta(
                        serde_json::json!({
                            "startupHints": {
                                "nonInteractive": non_interactive,
                                "skipGitStatus": true,
                                "skipProjectLayout": true
                            },
                            "clientType": "test-client",
                            "clientVersion": "0.0.0-test"
                        })
                        .as_object()
                        .cloned(),
                    ),
            )
            .await?;

        let chosen = select_auth_method(&response.auth_methods).ok_or_else(|| {
            let offered: Vec<_> = response
                .auth_methods
                .iter()
                .map(|method| &method.id().0)
                .collect();
            acp::Error::new(
                i32::from(acp::ErrorCode::AuthRequired),
                format!(
                    "no non-interactive auth method ({API_KEY_AUTH_METHOD} or {CACHED_TOKEN_AUTH_METHOD}); the agent offered {offered:?}"
                ),
            )
        })?;
        self.conn
            .authenticate(
                acp::AuthenticateRequest::new(chosen)
                    .meta(serde_json::json!({"headless": true}).as_object().cloned()),
            )
            .await?;
        Ok(response)
    }

    pub(crate) async fn new_session(&self, cwd: &Path) -> acp::Result<acp::SessionId> {
        self.send_new_session(acp::NewSessionRequest::new(cwd.to_path_buf()))
            .await
    }

    pub(crate) async fn new_session_with_model(
        &self,
        cwd: &Path,
        model_id: &str,
    ) -> acp::Result<acp::SessionId> {
        self.send_new_session(
            acp::NewSessionRequest::new(cwd.to_path_buf()).meta(
                serde_json::json!({ "modelId": model_id })
                    .as_object()
                    .cloned(),
            ),
        )
        .await
    }

    async fn send_new_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> acp::Result<acp::SessionId> {
        let response = self.conn.new_session(request.mcp_servers(vec![])).await?;
        Ok(response.session_id)
    }

    pub(crate) async fn load_session(
        &self,
        session_id: &acp::SessionId,
        cwd: &Path,
    ) -> acp::Result<acp::LoadSessionResponse> {
        self.conn
            .load_session(
                acp::LoadSessionRequest::new(session_id.clone(), cwd.to_path_buf())
                    .mcp_servers(vec![]),
            )
            .await
    }

    pub(crate) async fn prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        self.conn
            .prompt(acp::PromptRequest::new(
                session_id.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_owned(),
                ))],
            ))
            .await
    }

    pub(crate) async fn prompt_blocks(
        &self,
        session_id: &acp::SessionId,
        blocks: Vec<acp::ContentBlock>,
    ) -> acp::Result<acp::PromptResponse> {
        self.conn
            .prompt(acp::PromptRequest::new(session_id.clone(), blocks))
            .await
    }

    pub(crate) async fn set_model(
        &self,
        session_id: &acp::SessionId,
        model_id: &str,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        self.conn
            .set_session_model(acp::SetSessionModelRequest::new(
                session_id.clone(),
                acp::ModelId::new(model_id),
            ))
            .await
    }

    pub(crate) async fn set_mode(
        &self,
        session_id: &acp::SessionId,
        mode_id: &str,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        self.conn
            .set_session_mode(acp::SetSessionModeRequest::new(
                session_id.clone(),
                acp::SessionModeId::new(mode_id),
            ))
            .await
    }

    pub(crate) async fn ext_method(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> acp::Result<acp::ExtResponse> {
        let raw = serde_json::value::to_raw_value(&params).expect("serialize ext params");
        self.conn
            .ext_method(acp::ExtRequest::new(method, Arc::from(raw)))
            .await
    }

    /// Sends `session/cancel`, then answers every request the session holds under `HoldUntilCancel` with
    /// `cancelled`. The notification goes first because that is the order the protocol asks of a cancelling
    /// client; resolves once every released reply is recorded.
    pub(crate) async fn cancel_and_release_holds(
        &self,
        session_id: &acp::SessionId,
    ) -> acp::Result<()> {
        self.conn
            .cancel(acp::CancelNotification::new(session_id.clone()))
            .await?;
        self.handler.holds().release_held_requests(session_id).await;
        Ok(())
    }
}

/// Runs `request` under the scaled `budget`, panicking with the child's stderr on timeout, and logs the
/// elapsed time for tuning CI budgets (visible with --nocapture).
pub(crate) async fn timed<T>(
    process: &TestProcess,
    what: &str,
    budget: Duration,
    request: impl Future<Output = T>,
) -> T {
    let started = Instant::now();
    let result = tokio::time::timeout(scaled(budget), request)
        .await
        .unwrap_or_else(|_| panic!("{what} timed out\nstderr:\n{}", process.stderr_tail().text));
    eprintln!("[harness-timing] {what}: {:?}", started.elapsed());
    result
}

/// [`timed`] for a request no test expects to fail: an error panics with the child's stderr too.
pub(crate) async fn timed_ok<T>(
    process: &TestProcess,
    what: &str,
    budget: Duration,
    request: impl Future<Output = acp::Result<T>>,
) -> T {
    timed(process, what, budget, request)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{what} failed: {error}\nstderr:\n{}",
                process.stderr_tail().text
            )
        })
}
