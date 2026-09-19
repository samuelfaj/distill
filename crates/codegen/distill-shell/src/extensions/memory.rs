// Modified for Distill by Samuel Fajreldines, 2026.
//! Extension handlers for `x.ai/compact_conversation`, `x.ai/memory/flush`, `x.ai/memory/rewrite`,
//! `x.ai/memory/list`, `x.ai/memory/toggle`, and `x.ai/memory/forget`.
//! `memory/rewrite` turns a raw memory note into structured markdown with a one-shot LLM call.
//! `memory/list` and `memory/toggle` back the `/memory` modal without running a prompt turn, so
//! opening it or flipping memory writes nothing to scrollback.
//! `memory/forget` deletes one note from the `/memory` modal through the store's tombstone path.
//! `memory/flush` and `memory/dream` back the pager-local `/flush` and `/dream` commands; each
//! returns a typed disposition so the client can render the outcome.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::notification::{MemoryDisabledReason, MemoryFileInfo};
use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::{
    CompactConversationRequest, CompactConversationResponse, SessionCommand, SessionHandle,
};

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        m if m.starts_with("x.ai/compact_conversation") => handle_compact(agent, args).await,
        MEMORY_FLUSH_METHOD => handle_flush(agent, args).await,
        MEMORY_DREAM_METHOD => handle_dream(agent, args).await,
        "x.ai/memory/rewrite" => handle_rewrite(agent, args).await,
        MEMORY_LIST_METHOD => handle_list(agent, args).await,
        MEMORY_TOGGLE_METHOD => handle_toggle(agent, args).await,
        MEMORY_FORGET_METHOD => handle_forget(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

pub const MEMORY_FLUSH_METHOD: &str = "x.ai/memory/flush";
pub const MEMORY_DREAM_METHOD: &str = "x.ai/memory/dream";
pub const MEMORY_LIST_METHOD: &str = "x.ai/memory/list";
pub const MEMORY_TOGGLE_METHOD: &str = "x.ai/memory/toggle";
pub const MEMORY_FORGET_METHOD: &str = "x.ai/memory/forget";

/// How a `/dream` run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDreamDisposition {
    /// Observations were merged into topics.
    Completed,
    /// Nothing was waiting to be consolidated.
    NoWork,
    /// The consolidation lease is held, by another session or by this session's automatic Dream.
    Busy,
    /// An interrupted earlier Dream was finished instead of starting a new one.
    Recovered,
    /// This attempt failed; the observations stay queued for the next Dream.
    RetryRequired,
    Failed,
    /// Shadow rollout: the plan ran but nothing was written.
    Shadow,
    Cancelled,
    /// Memory or manual Dream is turned off for this session.
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDreamResponse {
    pub disposition: MemoryDreamDisposition,
    pub observation_count: usize,
    pub topics_affected: usize,
}

impl MemoryDreamResponse {
    pub(crate) const fn new(disposition: MemoryDreamDisposition) -> Self {
        Self {
            disposition,
            observation_count: 0,
            topics_affected: 0,
        }
    }

    /// Observations were written to topics.
    fn did_work(&self) -> bool {
        matches!(
            self.disposition,
            MemoryDreamDisposition::Completed | MemoryDreamDisposition::Recovered
        ) && self.topics_affected > 0
    }

    /// Combine this pass's outcome with a coalesced follow-up pass so the run reports the
    /// work it did: a later `NoWork` must not hide an earlier merge.
    pub(crate) fn then(self, next: Self) -> Self {
        match (self.did_work(), next.did_work()) {
            (true, true) => Self {
                disposition: next.disposition,
                observation_count: self.observation_count + next.observation_count,
                topics_affected: self.topics_affected + next.topics_affected,
            },
            (true, false) if next.disposition == MemoryDreamDisposition::NoWork => self,
            _ => next,
        }
    }

    /// The run did what the user asked, or there was nothing to do.
    pub fn succeeded(&self) -> bool {
        matches!(
            self.disposition,
            MemoryDreamDisposition::Completed
                | MemoryDreamDisposition::NoWork
                | MemoryDreamDisposition::Recovered
                | MemoryDreamDisposition::Shadow
        )
    }

    /// One-line outcome for scrollback; shared by the pager and non-pager ACP clients.
    pub fn summary(&self) -> String {
        match self.disposition {
            MemoryDreamDisposition::Completed if self.topics_affected > 0 => format!(
                "Dream merged {} into {}.",
                plural(self.observation_count, "observation"),
                plural(self.topics_affected, "topic")
            ),
            MemoryDreamDisposition::Completed => "Dream completed.".to_owned(),
            MemoryDreamDisposition::NoWork => "Nothing to consolidate.".to_owned(),
            MemoryDreamDisposition::Busy => {
                "Dream is already running; try again when it finishes.".to_owned()
            }
            MemoryDreamDisposition::Recovered if self.topics_affected > 0 => format!(
                "Dream finished an interrupted earlier run, updating {}.",
                plural(self.topics_affected, "topic")
            ),
            MemoryDreamDisposition::Recovered => {
                "Dream finished an interrupted earlier run.".to_owned()
            }
            MemoryDreamDisposition::RetryRequired => {
                "Dream did not finish; it will retry automatically.".to_owned()
            }
            MemoryDreamDisposition::Failed => "Dream failed.".to_owned(),
            MemoryDreamDisposition::Shadow => {
                "Dream ran in shadow mode; nothing was written.".to_owned()
            }
            MemoryDreamDisposition::Cancelled => "Dream cancelled.".to_owned(),
            MemoryDreamDisposition::Disabled => "Dream is turned off for this session.".to_owned(),
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// How a `/flush` run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFlushDisposition {
    /// Every completed turn is captured and indexed.
    Flushed,
    /// Capture failed but will be retried in the background.
    RetryRequired,
    Failed,
    TimedOut,
    /// Memory or capture is turned off for this session.
    Disabled,
    /// Legacy memory: another flush was already running.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFlushResponse {
    /// `disposition == Flushed`; kept because the headless `--memory-flush` barrier reads it.
    pub flushed: bool,
    pub disposition: MemoryFlushDisposition,
    /// Last turn the flush covered (memory v2 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through_turn: Option<u32>,
}

impl MemoryFlushResponse {
    pub fn succeeded(&self) -> bool {
        self.disposition == MemoryFlushDisposition::Flushed
    }

    /// One-line outcome for scrollback; shared by the pager and non-pager ACP clients.
    pub fn summary(&self) -> String {
        match (self.disposition, self.through_turn) {
            (MemoryFlushDisposition::Flushed, Some(turn)) => {
                format!("Memory flushed through turn {turn}.")
            }
            (MemoryFlushDisposition::Flushed, None) => "Memory flushed.".to_owned(),
            (MemoryFlushDisposition::RetryRequired, _) => {
                "Memory flush did not finish; capture will retry in the background.".to_owned()
            }
            (MemoryFlushDisposition::Failed, _) => "Memory flush failed.".to_owned(),
            (MemoryFlushDisposition::TimedOut, _) => {
                "Memory flush timed out; capture continues in the background.".to_owned()
            }
            (MemoryFlushDisposition::Disabled, _) => {
                "Memory is turned off for this session.".to_owned()
            }
            (MemoryFlushDisposition::Busy, _) => {
                "Another memory flush is already running.".to_owned()
            }
        }
    }
}

/// Largest note `x.ai/memory/forget` will hash and delete, for both v2 and legacy stores.
pub const MEMORY_FORGET_MAX_FILE_BYTES: u64 = distill_memory::MAX_FORGET_FILE_BYTES;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryListRequest {
    pub session_id: String,
}

/// What the `/memory` modal renders: the same payload as the `MemoryFiles` notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryListing {
    pub files: Vec<MemoryFileInfo>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<MemoryDisabledReason>,
    pub capture_enabled: bool,
    pub dream_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryToggleRequest {
    pub session_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryToggleResponse {
    /// Outcome for the user (also covers "already on" and refusals).
    pub message: String,
    /// Authoritative state after the toggle; refusals leave it unchanged.
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<MemoryDisabledReason>,
    /// Files after the toggle, so an open modal can refresh without a second round trip.
    /// `None` when the store could not be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<MemoryListing>,
}

fn resident_session(agent: &MvpAgent, session_id: String) -> Result<SessionHandle, acp::Error> {
    agent
        .resident_handle(&acp::SessionId::new(session_id.clone()))
        .ok_or_else(|| {
            acp::Error::invalid_params().data(format!("session not found: {session_id}"))
        })
}

async fn handle_list(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MemoryListRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session
        .cmd_tx
        .send(SessionCommand::MemoryList { respond_to: tx });
    let listing = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?
        .map_err(|e| acp::Error::internal_error().data(e))?;
    to_raw_response(&listing)
}

async fn handle_toggle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MemoryToggleRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session.cmd_tx.send(SessionCommand::MemoryToggle {
        enabled: req.enabled,
        respond_to: tx,
    });
    let response = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    to_raw_response(&response)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryForgetRequest {
    pub session_id: String,
    /// Absolute path as listed by `MemoryFiles`.
    pub path: String,
    /// BLAKE3 hex of the bytes the user previewed; the store refuses to delete anything else.
    pub expected_content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum MemoryForgetResponse {
    Forgotten {
        was_already_forgotten: bool,
    },
    Rejected {
        reason: MemoryForgetRejection,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryForgetRejection {
    MemoryDisabled,
    /// Manifests, files outside the store, or anything the store's access policy protects.
    NotDeletable,
    /// The file no longer matches the previewed bytes.
    Changed,
    /// Dream holds the consolidation lease; retry once it finishes.
    DreamRunning,
    Failed,
}

async fn handle_forget(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MemoryForgetRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session.cmd_tx.send(SessionCommand::MemoryForget {
        path: req.path,
        expected_content_hash: req.expected_content_hash,
        respond_to: tx,
    });
    let response = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    to_raw_response(&response)
}

async fn handle_compact(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: CompactConversationRequest = parse_params(args)?;
    // send over the compact query here properly
    let sid: acp::SessionId = req.session_id.into();
    let session_handle = agent.resident_handle(&sid);
    let (tx, rx) = oneshot::channel();
    if let Some(session) = session_handle {
        let _ = session.cmd_tx.send(SessionCommand::CompactSession {
            user_context: req.user_context,
            respond_to: tx,
        });
    }
    // Pass the session error through; rewrapping buries the detail in a Debug dump.
    rx.await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))??;
    to_raw_response(&CompactConversationResponse {})
}

/// Request body for `x.ai/memory/flush` and `x.ai/memory/dream` (snake_case, unlike the modal methods).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFlushRequest {
    pub session_id: String,
}

async fn handle_flush(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MemoryFlushRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session
        .cmd_tx
        .send(SessionCommand::FlushMemory { respond_to: tx });
    let response = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    to_raw_response(&response)
}

async fn handle_dream(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MemoryFlushRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session
        .cmd_tx
        .send(SessionCommand::MemoryDream { respond_to: tx });
    let response = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    to_raw_response(&response)
}

async fn handle_rewrite(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RewriteRequest {
        session_id: String,
        raw_text: String,
        context_summary: String,
    }

    let req: RewriteRequest = parse_params(args)?;
    let session = resident_session(agent, req.session_id)?;
    let (tx, rx) = oneshot::channel();
    let _ = session.cmd_tx.send(SessionCommand::RewriteMemoryNote {
        raw_text: req.raw_text,
        context_summary: req.context_summary,
        respond_to: tx,
    });
    let rewritten = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?
        .map_err(|e| acp::Error::internal_error().data(e))?;
    to_raw_response(&serde_json::json!({ "rewritten": rewritten }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dream(
        disposition: MemoryDreamDisposition,
        observations: usize,
        topics: usize,
    ) -> MemoryDreamResponse {
        MemoryDreamResponse {
            disposition,
            observation_count: observations,
            topics_affected: topics,
        }
    }

    #[test]
    fn coalesced_dream_passes_keep_the_work_done() {
        use MemoryDreamDisposition::*;
        let merged = dream(Completed, 7, 3);
        assert_eq!(merged.then(dream(NoWork, 0, 0)), merged);
        assert_eq!(merged.then(dream(Completed, 2, 1)), dream(Completed, 9, 4));
        assert_eq!(
            merged.then(dream(RetryRequired, 2, 0)).disposition,
            RetryRequired
        );
        assert_eq!(
            dream(Cancelled, 0, 0).then(dream(NoWork, 0, 0)).disposition,
            NoWork
        );
    }
}
