//! [`GrokStdioClient`] drives `grok agent stdio`: it owns the child, the [`AgentConnection`] over its pipes,
//! and runs every request under a scaled budget; a timeout, or a failed setup request or cancel, panics with
//! the child's stderr.

use std::path::Path;
use std::pin::pin;
use std::time::Duration;

use agent_client_protocol as acp;

use crate::acp_agent_connection::{AgentConnection, timed, timed_ok};
use crate::acp_agent_process::{AgentProcessOptions, SpawnedAgent};
use crate::acp_policy::ClientPolicy;
use crate::acp_transcript::TranscriptEntry;
use crate::mock_server::MockInferenceServer;
use crate::process::TestProcess;
use crate::sandbox::TestSandbox;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A prompt runs a whole turn against the mock server, as does the wait for a request held inside one.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// `session/load` replays history and is slower under Rosetta (macos-x86_64 lifecycle CI), where 20s flaked.
const LOAD_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// How [`GrokStdioClient::spawn_with_options`] starts the agent. [`SpawnOptions::new`] is the sandbox alone,
/// with no overrides and a policy that allows every permission and dismisses every question.
pub struct SpawnOptions {
    agent: AgentProcessOptions,
    policy: ClientPolicy,
    turn_budget: Option<Duration>,
}

impl SpawnOptions {
    #[must_use]
    pub fn new(sandbox: TestSandbox) -> Self {
        SpawnOptions {
            agent: AgentProcessOptions::new(sandbox),
            policy: ClientPolicy::default(),
            turn_budget: None,
        }
    }

    /// Applied to the sandbox after its hermetic baseline, in order, so a later pair overrides an earlier one
    /// with the same key.
    #[must_use]
    pub fn with_extra_env(
        mut self,
        extra_env: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.agent.extra_env.extend(
            extra_env
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    /// Keys removed from the sandbox baseline after the mock URL and `with_extra_env` are applied.
    /// Use this to drop a baseline variable a scenario must run without, such as the mock's
    /// `XAI_API_KEY`, so a login-only or missing-credential case is exercised faithfully.
    #[must_use]
    pub fn with_removed_env(
        mut self,
        removed_env: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.agent
            .removed_env
            .extend(removed_env.into_iter().map(Into::into));
        self
    }

    /// Global flags placed before the `agent stdio` subcommand.
    #[must_use]
    pub fn with_leading_args(
        mut self,
        leading_args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.agent
            .leading_args
            .extend(leading_args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: ClientPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Budget for `prompt` and `ext_method`, so a caller that wraps the turn in its own deadline
    /// records the timeout instead of the client panicking at the shorter default first. Setup
    /// requests keep the default request budget.
    #[must_use]
    pub fn with_turn_budget(mut self, budget: Duration) -> Self {
        self.turn_budget = Some(budget);
        self
    }
}

/// Spawn and drive it inside a `tokio::task::LocalSet`; the connection's tasks are spawned locally. The child
/// is killed on drop, before its sandbox goes.
pub struct GrokStdioClient {
    connection: AgentConnection,
    process: TestProcess,
    sandbox: TestSandbox,
    turn_budget: Option<Duration>,
}

impl GrokStdioClient {
    pub async fn spawn(server: &MockInferenceServer, cwd: &Path) -> Self {
        Self::spawn_with_options(server, cwd, SpawnOptions::new(TestSandbox::new())).await
    }

    pub async fn spawn_with_sandbox(
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: TestSandbox,
    ) -> Self {
        Self::spawn_with_options(server, cwd, SpawnOptions::new(sandbox)).await
    }

    pub async fn spawn_with_options(
        server: &MockInferenceServer,
        cwd: &Path,
        options: SpawnOptions,
    ) -> Self {
        let SpawnOptions {
            agent,
            policy,
            turn_budget,
        } = options;
        let SpawnedAgent {
            mut process,
            sandbox,
        } = agent.spawn(server, cwd);
        let connection = AgentConnection::connect(&mut process, policy);
        GrokStdioClient {
            connection,
            process,
            sandbox,
            turn_budget,
        }
    }

    /// Initialize, then authenticate with the `xai.api_key` method.
    pub async fn initialize(&self) -> acp::InitializeResponse {
        timed_ok(
            &self.process,
            "initialize",
            REQUEST_TIMEOUT,
            self.connection.initialize_and_authenticate(),
        )
        .await
    }

    pub async fn create_session(&self, cwd: &Path) -> acp::SessionId {
        timed_ok(
            &self.process,
            "session/new",
            REQUEST_TIMEOUT,
            self.connection.new_session(cwd),
        )
        .await
    }

    pub async fn create_session_with_model(&self, cwd: &Path, model_id: &str) -> acp::SessionId {
        timed_ok(
            &self.process,
            &format!("session/new with modelId={model_id}"),
            REQUEST_TIMEOUT,
            self.connection.new_session_with_model(cwd, model_id),
        )
        .await
    }

    pub async fn set_model(
        &self,
        session_id: &acp::SessionId,
        model_id: &str,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        timed(
            &self.process,
            &format!("session/set_model({model_id})"),
            REQUEST_TIMEOUT,
            self.connection.set_model(session_id, model_id),
        )
        .await
    }

    pub async fn set_mode(
        &self,
        session_id: &acp::SessionId,
        mode_id: &str,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        timed(
            &self.process,
            &format!("session/set_mode({mode_id})"),
            REQUEST_TIMEOUT,
            self.connection.set_mode(session_id, mode_id),
        )
        .await
    }

    pub async fn prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        timed(
            &self.process,
            "prompt",
            self.turn_budget.unwrap_or(PROMPT_TIMEOUT),
            self.connection.prompt(session_id, text),
        )
        .await
    }

    pub async fn prompt_blocks(
        &self,
        session_id: &acp::SessionId,
        blocks: Vec<acp::ContentBlock>,
    ) -> acp::Result<acp::PromptResponse> {
        timed(
            &self.process,
            "prompt blocks",
            self.turn_budget.unwrap_or(PROMPT_TIMEOUT),
            self.connection.prompt_blocks(session_id, blocks),
        )
        .await
    }

    pub async fn load_session(
        &self,
        session_id: &acp::SessionId,
        cwd: &Path,
    ) -> acp::LoadSessionResponse {
        timed_ok(
            &self.process,
            "session/load",
            LOAD_SESSION_TIMEOUT,
            self.connection.load_session(session_id, cwd),
        )
        .await
    }

    pub async fn ext_method(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> acp::Result<acp::ExtResponse> {
        timed(
            &self.process,
            &format!("ext {method}"),
            self.turn_budget.unwrap_or(REQUEST_TIMEOUT),
            self.connection.ext_method(method, params),
        )
        .await
    }

    /// Send `session/cancel`, then answer every request this session holds under `HoldUntilCancel` with
    /// `cancelled`. Resolves once every released reply is recorded in the transcript; panics with the agent's
    /// stderr if the notification cannot be sent or a held request is not answered within the request budget.
    pub async fn cancel(&self, session_id: &acp::SessionId) {
        timed_ok(
            &self.process,
            "session/cancel",
            REQUEST_TIMEOUT,
            self.connection.cancel_and_release_holds(session_id),
        )
        .await;
    }

    /// Run a prompt and `cancel` the session the moment one of its requests is held under `HoldUntilCancel`.
    /// Returns the prompt's response, so a test asserts its stop reason without sleeping. Panics at once if
    /// the turn ends before any request is held, and with the agent's stderr if none is held within the
    /// prompt budget.
    pub async fn prompt_then_cancel_at_held_request(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        let mut prompt = pin!(self.prompt(session_id, text));
        let held = timed(
            &self.process,
            "held request",
            PROMPT_TIMEOUT,
            self.connection
                .handler()
                .holds()
                .wait_for_held_request(session_id),
        );
        tokio::select! {
            () = held => {}
            response = &mut prompt => panic!(
                "prompt finished before any request was held: {response:?}\nstderr:\n{}",
                self.stderr()
            ),
        }
        self.cancel(session_id).await;
        prompt.await
    }

    /// Every message the agent sent so far, in the order the client finished handling them; a held request
    /// lands once `cancel` releases it.
    pub fn transcript(&self) -> Vec<TranscriptEntry> {
        self.connection.handler().transcript().entries()
    }

    /// Resolves once the transcript holds an entry `is_match` accepts, so a test waits for a notification
    /// instead of running another turn to flush it. Panics with the agent's stderr after the request budget.
    pub async fn wait_for_transcript_entry(&self, is_match: impl Fn(&TranscriptEntry) -> bool) {
        timed(
            &self.process,
            "transcript entry",
            REQUEST_TIMEOUT,
            self.connection
                .handler()
                .transcript()
                .wait_until(|entries| entries.iter().any(&is_match)),
        )
        .await;
    }

    pub fn captured_text(&self) -> String {
        self.connection.handler().transcript().agent_text()
    }

    pub fn notification_count(&self) -> usize {
        self.connection
            .handler()
            .transcript()
            .session_update_count()
    }

    pub fn stderr(&self) -> String {
        self.process.stderr_tail().text
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.process.pid()
    }

    pub fn process_diagnostics(&self) -> String {
        self.process.diagnostic_summary()
    }

    pub fn start_terminate(&mut self) -> std::io::Result<()> {
        self.process.start_terminate()
    }

    pub fn start_kill(&mut self) {
        self.process.start_kill();
    }

    pub async fn close(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.process.close().await
    }

    pub fn sandbox(&self) -> &TestSandbox {
        &self.sandbox
    }

    /// Kills the child and hands its sandbox back, so a restart on the same sandbox never overlaps the old
    /// process.
    pub fn into_sandbox(self) -> TestSandbox {
        self.sandbox
    }
}
