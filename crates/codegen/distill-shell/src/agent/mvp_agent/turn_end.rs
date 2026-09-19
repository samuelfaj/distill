// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::agent::session_registry_client::RegisterRequest;
use crate::agent::session_registry_client::SessionRegistryClient;
use crate::agent::session_registry_client::UpdateRequest;
use tracing::Instrument;

pub(super) struct TurnResultArgs {
    pub(super) request_id: String,
    pub(super) completed: bool,
    pub(super) stop_reason: String,
    pub(super) total_tokens: Option<u64>,
    pub(super) error: Option<String>,
    pub(super) finished_at: String,
    pub(super) turn_snapshot: Option<crate::session::signals::TurnDeltaSnapshot>,
    pub(super) prompt_mode: String,
    pub(super) subagents_spawned: Vec<crate::upload::trace::SubagentSpawnedRef>,
}

impl TurnResultArgs {
    pub(super) fn into_metadata(self, resolved_model: Option<String>) -> TurnResultMetadata {
        let snapshot = self.turn_snapshot;
        TurnResultMetadata {
            schema_version: GCS_SCHEMA_VERSION,
            request_id: self.request_id,
            completed: self.completed,
            stop_reason: Some(self.stop_reason),
            total_tokens: self.total_tokens,
            input_tokens: snapshot.as_ref().map(|s| s.turn_input_tokens),
            cached_input_tokens: snapshot.as_ref().map(|s| s.turn_cached_input_tokens),
            output_tokens: snapshot.as_ref().map(|s| s.turn_output_tokens),
            error: self.error,
            finished_at: self.finished_at,
            signals: snapshot.as_ref().map(|s| s.current.clone()),
            turn_delta: snapshot.as_ref().map(|s| s.delta.clone()),
            start_prompt_mode: snapshot
                .as_ref()
                .and_then(|s| s.start_prompt_mode.clone())
                .or(Some(self.prompt_mode)),
            end_prompt_mode: snapshot.as_ref().and_then(|s| s.end_prompt_mode.clone()),
            resolved_model,
            subagents_spawned: self.subagents_spawned,
        }
    }
}

struct RegisterTurnZero {
    model_id: String,
    hostname: String,
    device_id: Option<String>,
    first_prompt: Option<String>,
    suppress: bool,
}

pub(super) struct RegistryTurnEndArgs {
    client: Option<SessionRegistryClient>,
    session_id: String,
    turn: i32,
    cwd: String,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    register: Option<RegisterTurnZero>,
    order: crate::session::handle::RegistryWriteOrder,
    claim: crate::session::handle::RegistryTurnClaim,
    head: GitRead,
    head_branch: Option<String>,
}

impl MvpAgent {
    pub(super) fn build_registry_turn_end_args(
        &self,
        session_id: &acp::SessionId,
        turn_number: u64,
        handle: &SessionHandle,
        prompt: &[acp::ContentBlock],
        claim: crate::session::handle::RegistryTurnClaim,
        head: GitRead,
        head_branch: Option<String>,
    ) -> RegistryTurnEndArgs {
        let client = self.session_registry_client();
        let register =
            (turn_number == 0 && client.is_some()).then(|| self.turn_zero_register(prompt));
        RegistryTurnEndArgs {
            client,
            session_id: session_id.to_string(),
            turn: i32::try_from(turn_number).unwrap_or(i32::MAX),
            cwd: handle.info.cwd.clone(),
            cmd_tx: handle.cmd_tx.clone(),
            register,
            order: handle.registry_write_order.clone(),
            claim,
            head,
            head_branch,
        }
    }

    fn turn_zero_register(&self, prompt: &[acp::ContentBlock]) -> RegisterTurnZero {
        let suppress = self
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_zdr_team());
        RegisterTurnZero {
            model_id: self.models_manager.current_model_id().0.to_string(),
            hostname: gethostname::gethostname().to_string_lossy().to_string(),
            device_id: if suppress { None } else { Some(agent_id()) },
            first_prompt: if suppress {
                None
            } else {
                prompt.iter().find_map(|b| {
                    if let acp::ContentBlock::Text(t) = b {
                        Some(t.text.clone())
                    } else {
                        None
                    }
                })
            },
            suppress,
        }
    }
}

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const REGISTRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const SUMMARY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn bounded_registry<T>(
    op: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(REGISTRY_TIMEOUT, op)
        .await
        .unwrap_or_else(|elapsed| Err(anyhow::Error::from(elapsed)))
}

pub(super) enum GitRead {
    Value(String),
    Empty,
    Failed,
}

async fn git_read(cwd: &str, args: &[&str]) -> GitRead {
    let mut cmd = tokio::process::Command::from(distill_tty_utils::git_command());
    cmd.current_dir(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(output)) = tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await else {
        return GitRead::Failed;
    };
    match output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(value) => GitRead::Value(value),
        None => GitRead::Empty,
    }
}

async fn git_out(cwd: &str, args: &[&str]) -> Option<String> {
    match git_read(cwd, args).await {
        GitRead::Value(value) => Some(value),
        GitRead::Empty | GitRead::Failed => None,
    }
}

async fn sample_git_head(cwd: &str) -> (GitRead, Option<String>) {
    (
        git_read(cwd, &["rev-parse", "HEAD"]).await,
        git_out(cwd, &["branch", "--show-current"]).await,
    )
}

#[must_use = "dropping a TurnEndCapture without finish() skips this turn's registry writes"]
pub(super) struct TurnEndCapture {
    registry_claim: crate::session::handle::RegistryTurnClaim,
    git_sample: tokio_util::task::AbortOnDropHandle<(GitRead, Option<String>)>,
    session_copy_rx:
        oneshot::Receiver<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
}

impl TurnEndCapture {
    pub(super) fn begin(handle: &SessionHandle, head_cwd: String) -> Self {
        let registry_claim = handle.registry_write_order.begin_turn_end();
        // Enqueue CopyFile before any await so the actor snapshots this turn.
        let (session_copy_tx, session_copy_rx) = oneshot::channel();
        if handle
            .cmd_tx
            .send(crate::session::SessionCommand::CopyFile {
                respond_to: session_copy_tx,
            })
            .is_err()
        {
            tracing::warn!("Failed to send CopyFile command, skipping session state upload");
        }
        let git_sample = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            sample_git_head(&head_cwd).await
        }));
        Self {
            registry_claim,
            git_sample,
            session_copy_rx,
        }
    }

    pub(super) async fn finish(
        self,
    ) -> (
        GitRead,
        Option<String>,
        crate::session::handle::RegistryTurnClaim,
        oneshot::Receiver<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
    ) {
        let (head, head_branch) = self.git_sample.await.unwrap_or((GitRead::Failed, None));
        (head, head_branch, self.registry_claim, self.session_copy_rx)
    }
}

fn plan_git_head(
    commit: GitRead,
    branch: Option<String>,
) -> Option<(Option<String>, Option<String>)> {
    match commit {
        GitRead::Value(sha) => Some((Some(sha), branch)),
        GitRead::Empty => Some((None, branch)),
        GitRead::Failed => None,
    }
}

async fn register_session_turn_zero(
    client: &SessionRegistryClient,
    session_id: &str,
    cwd: &str,
    reg: RegisterTurnZero,
) {
    let repo_remote_url = git_out(cwd, &["remote", "get-url", "origin"]).await;
    let repo_branch = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let repo_head_at_start = git_out(cwd, &["rev-parse", "HEAD"]).await;
    let reg_req = RegisterRequest {
        session_id: session_id.to_owned(),
        cwd: cwd.to_owned(),
        gcs_trace_prefix: session_id.to_owned(),
        model_id: Some(reg.model_id),
        repo_remote_url,
        repo_branch,
        repo_head_at_start,
        hostname: Some(reg.hostname),
        device_id: reg.device_id,
        parent_session_id: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
    };
    if let Err(e) = bounded_registry(client.register(&reg_req)).await {
        tracing::warn!(
            error = %e,
            "session registry register failed (non-fatal)"
        );
    }
    let info = crate::session::info::Info {
        id: acp::SessionId::new(session_id.to_owned()),
        cwd: cwd.to_owned(),
    };
    let summary_path = crate::session::persistence::session_dir(&info).join("summary.json");
    let summary = if reg.suppress {
        None
    } else {
        tokio::time::timeout(SUMMARY_READ_TIMEOUT, tokio::fs::read(&summary_path))
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|bytes| {
                serde_json::from_slice::<crate::session::persistence::Summary>(&bytes).ok()
            })
            .map(|s| s.session_summary)
            .filter(|s| !s.is_empty())
    };
    if reg.first_prompt.is_some() || summary.is_some() {
        let upd_req = UpdateRequest {
            summary,
            first_prompt: reg.first_prompt,
            last_turn_number: None,
            repo_head_at_end: None,
            restorable_turn_number: None,
        };
        tracing::debug!(
            session_id = %session_id,
            has_summary = upd_req.summary.is_some(),
            "session registry post-register update"
        );
        if let Err(e) = bounded_registry(client.update(session_id, &upd_req)).await {
            tracing::warn!(
                error = %e,
                "session registry first-prompt update failed (non-fatal)"
            );
        }
    }
}

async fn advance_last_turn(
    client: &SessionRegistryClient,
    session_id: &str,
    turn: i32,
    repo_head_at_end: Option<String>,
) -> bool {
    let req = UpdateRequest {
        summary: None,
        first_prompt: None,
        last_turn_number: Some(turn),
        repo_head_at_end,
        restorable_turn_number: None,
    };
    match bounded_registry(client.update(session_id, &req)).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "session registry last_turn_number update failed (non-fatal)"
            );
            false
        }
    }
}

async fn advance_restorable_turn(
    client: &SessionRegistryClient,
    session_id: &str,
    turn: i32,
) -> bool {
    let req = UpdateRequest {
        summary: None,
        first_prompt: None,
        last_turn_number: None,
        repo_head_at_end: None,
        restorable_turn_number: Some(turn),
    };
    match bounded_registry(client.update(session_id, &req)).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "session registry restorable_turn_number update failed (non-fatal)"
            );
            false
        }
    }
}

struct OrderedTurnWrites {
    client: Option<SessionRegistryClient>,
    session_id: String,
    turn: i32,
    cwd: String,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    order: crate::session::handle::RegistryWriteOrder,
    register: Option<RegisterTurnZero>,
}

impl OrderedTurnWrites {
    async fn write_git_head(&self, head: GitRead, head_branch: Option<String>) {
        async {
            if let Some((commit, branch)) = plan_git_head(head, head_branch) {
                let _ = self
                    .cmd_tx
                    .send(crate::session::SessionCommand::PersistGitHead { commit, branch });
            }
        }
        .instrument(tracing::debug_span!("turn_end.persist_git_head"))
        .await;
    }

    async fn write_register(&mut self) {
        let (Some(client), Some(reg)) = (self.client.as_ref(), self.register.take()) else {
            return;
        };
        register_session_turn_zero(client, &self.session_id, &self.cwd, reg).await;
    }

    async fn write_last_turn(&self, repo_head_at_end: Option<String>) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        if self.order.should_write_last_turn(self.turn)
            && advance_last_turn(client, &self.session_id, self.turn, repo_head_at_end).await
        {
            self.order.commit_last_turn(self.turn);
        }
    }

    async fn write_restorable(&self) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let _apply = self.order.lock_restorable_apply().await;
        if self.order.should_write_restorable(self.turn)
            && advance_restorable_turn(client, &self.session_id, self.turn).await
        {
            self.order.commit_restorable(self.turn);
        }
    }
}

pub(super) async fn run_registry_turn_end(
    args: RegistryTurnEndArgs,
    archive_confirmed: oneshot::Receiver<bool>,
) {
    let RegistryTurnEndArgs {
        client,
        session_id,
        turn,
        cwd,
        cmd_tx,
        register,
        order,
        mut claim,
        head,
        head_branch,
    } = args;
    claim.wait_predecessor().await;
    let repo_head_at_end = match &head {
        GitRead::Value(sha) => Some(sha.clone()),
        GitRead::Empty | GitRead::Failed => None,
    };
    let mut writes = OrderedTurnWrites {
        client,
        session_id,
        turn,
        cwd,
        cmd_tx,
        order,
        register,
    };
    writes.write_git_head(head, head_branch).await;
    writes.write_register().await;
    writes.write_last_turn(repo_head_at_end).await;
    drop(claim);
    if let Ok(true) = archive_confirmed.await {
        writes.write_restorable().await;
    }
}

pub(super) struct TraceCaptures {
    pub(super) permission_events: Vec<PermissionEvent>,
    pub(super) session_copy_rx:
        oneshot::Receiver<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
    pub(super) turn_messages: Option<distill_chat_state::TurnCapture>,
    pub(super) streaming_partial: Option<crate::session::acp_session::StreamingTurnCapture>,
}

pub(super) async fn run_trace_completion(
    ctx: &PromptTraceContext,
    captures: TraceCaptures,
    wait: UploadWait,
) -> bool {
    let TraceCaptures {
        permission_events,
        session_copy_rx,
        turn_messages,
        streaming_partial,
    } = captures;
    match complete_prompt_trace(
        ctx.clone(),
        permission_events,
        session_copy_rx,
        turn_messages.into(),
        streaming_partial,
        wait,
    )
    .await
    {
        Ok(true) => true,
        Ok(false) => {
            match wait {
                UploadWait::Defer { .. } => tracing::debug!(
                    "session state unconfirmed within the flush budget; \
                     skipping restorable_turn_number advance"
                ),
                UploadWait::Confirm => tracing::warn!(
                    "session state upload failed; skipping restorable_turn_number advance"
                ),
            }
            false
        }
        Err(e) => {
            tracing::warn!("Failed to complete prompt trace: {e:?}");
            match wait {
                UploadWait::Confirm => write_error_manifest(ctx).await,
                UploadWait::Defer { deadline } => {
                    crate::upload::trace::flush_then_write_error_manifest(ctx, deadline).await
                }
            }
            false
        }
    }
}

pub(super) struct ErrorTurnArtifacts {
    pub(super) turn_messages: Option<distill_chat_state::TurnCapture>,
    pub(super) streaming_partial: Option<crate::session::acp_session::StreamingTurnCapture>,
    pub(super) upload_unified: bool,
}

pub(super) async fn upload_error_turn_artifacts(
    ctx: &PromptTraceContext,
    result: &TurnResultMetadata,
    artifacts: ErrorTurnArtifacts,
    wait: UploadWait,
) {
    let ErrorTurnArtifacts {
        turn_messages,
        streaming_partial,
        upload_unified,
    } = artifacts;
    upload_turn_result(ctx, result, wait).await;
    if let Some(capture) = turn_messages {
        upload_turn_messages(ctx, capture, wait).await;
    }
    if let Some(ref capture) = streaming_partial {
        crate::upload::trace::upload_streaming_partial(ctx, capture, wait).await;
    }
    if upload_unified {
        upload_unified_log(ctx, wait).await;
    }
    match wait {
        UploadWait::Confirm => write_error_manifest(ctx).await,
        UploadWait::Defer { deadline } => {
            crate::upload::trace::flush_then_write_error_manifest(ctx, deadline).await
        }
    }
}

pub(super) enum TurnEndOutcome {
    Completed {
        captures: TraceCaptures,
        registry: Box<RegistryTurnEndArgs>,
    },
    Failed(ErrorTurnArtifacts),
}

pub(super) async fn run_detached_turn_end(
    ctx: PromptTraceContext,
    turn_result: TurnResultArgs,
    resolved_model: Option<String>,
    outcome: TurnEndOutcome,
) {
    let result = turn_result.into_metadata(resolved_model);
    match outcome {
        TurnEndOutcome::Completed { captures, registry } => {
            let (archive_confirmed_tx, archive_confirmed_rx) = oneshot::channel();
            futures::join!(
                upload_turn_result(&ctx, &result, UploadWait::Confirm)
                    .instrument(tracing::debug_span!("turn_end.turn_result_upload")),
                run_registry_turn_end(*registry, archive_confirmed_rx)
                    .instrument(tracing::debug_span!("turn_end.registry")),
                async {
                    let confirmed = run_trace_completion(&ctx, captures, UploadWait::Confirm).await;
                    let _ = archive_confirmed_tx.send(confirmed);
                }
                .instrument(tracing::debug_span!("turn_end.trace_completion")),
            );
        }
        TurnEndOutcome::Failed(artifacts) => {
            upload_error_turn_artifacts(&ctx, &result, artifacts, UploadWait::Confirm).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GitRead;
    use super::plan_git_head;

    #[test]
    fn plan_git_head_decides_what_to_store() {
        assert_eq!(
            plan_git_head(GitRead::Value("abc123".into()), Some("main".into())),
            Some((Some("abc123".into()), Some("main".into())))
        );
        assert_eq!(
            plan_git_head(GitRead::Value("abc123".into()), None),
            Some((Some("abc123".into()), None))
        );
        assert_eq!(
            plan_git_head(GitRead::Empty, Some("main".into())),
            Some((None, Some("main".into())))
        );
        assert_eq!(plan_git_head(GitRead::Failed, Some("main".into())), None);
    }

    #[tokio::test]
    async fn dropped_claim_keeps_later_turn_ordered() {
        let order = crate::session::handle::RegistryWriteOrder::default();
        let running = order.begin_turn_end();
        let mut dropped = order.begin_turn_end();
        let mut next = order.begin_turn_end();
        {
            let mut waiting = std::pin::pin!(dropped.wait_predecessor());
            assert!(futures::poll!(waiting.as_mut()).is_pending());
        }
        drop(dropped);
        let mut waiting = std::pin::pin!(next.wait_predecessor());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(running);
        waiting.await;
    }
}
