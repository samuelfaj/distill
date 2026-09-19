// Modified for Distill by Samuel Fajreldines, 2026.
//! Worktree create and resume RPCs shared by the interactive `Effect::CreateWorktreeSession`
//! and headless `-w` startup, so the request shape, response unwrap, and failure text live once.

use std::path::{Path, PathBuf};

use agent_client_protocol as acp;
use distill_acp_lib::{AcpAgentTx, acp_send};
use distill_workspace::session::git::RestoreDegree;
use serde::Serialize;

use super::effects::{
    acp_send_bounded, parse_worktree_restore_payload, parse_worktree_strategy_summary,
    sanitize_user_error,
};
use super::session_startup::worktree_session_cwd;
use super::session_title_resolve::worktree_resume_failure_message;

pub(crate) const CREATE_METHOD: &str = "x.ai/git/worktree/create_from_worktree_sync";
pub(crate) const RESUME_METHOD: &str = "x.ai/git/worktree/resume_session";

/// How the new worktree's working tree is seeded from the source checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CopyMode {
    /// Checkout of `gitRef` only.
    Clean,
    /// Source's uncommitted changes overlaid on top.
    Dirty,
}

/// What `-w [NAME]` and `--worktree-ref` asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorktreeSpec {
    pub label: Option<String>,
    pub git_ref: Option<String>,
}

impl WorktreeSpec {
    /// From the CLI pair. `worktree == Some("")` is a bare `-w`.
    pub(crate) fn from_cli(worktree: Option<&str>, worktree_ref: Option<&str>) -> Option<Self> {
        worktree.map(|label| Self {
            label: Some(label).filter(|l| !l.is_empty()).map(str::to_owned),
            git_ref: worktree_ref.map(str::to_owned),
        })
    }

    /// A specific ref bases a clean checkout; otherwise the dirty tree is carried over.
    pub(crate) fn copy_mode(&self) -> CopyMode {
        if self.git_ref.is_some() {
            CopyMode::Clean
        } else {
            CopyMode::Dirty
        }
    }
}

/// Worktree and session identity for a create. A CLI `--session-id` names both; otherwise a
/// `pager-*` id (dashboard, palette, and unlabeled headless paths).
pub(crate) fn new_worktree_id(preferred_session_id: Option<&str>) -> String {
    preferred_session_id.map(str::to_owned).unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("pager-{}", id.get(..12).unwrap_or(id.as_str()))
    })
}

/// User-facing failure text, already sanitized and (for resume) hinted, carried to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeRpcError {
    pub message: String,
}

impl std::fmt::Display for WorktreeRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorktreeRpcError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatedWorktree {
    pub worktree_root: PathBuf,
    /// Where the session opens: the launch cwd's offset inside the source, replayed under the root.
    pub session_cwd: PathBuf,
    /// Worktree strategy line, present only when it carries news (Grove asked for, or a fallback).
    pub strategy_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResumedWorktree {
    /// The agent may fork a local session into the worktree under a new id.
    pub session_id: String,
    pub worktree_root: PathBuf,
    pub session_cwd: PathBuf,
    pub code_restored: bool,
    pub restore_summary: Option<String>,
    pub restore_degree: Option<RestoreDegree>,
    /// Worktree strategy line, present only when it carries news (Grove asked for, or a fallback).
    pub strategy_summary: Option<String>,
}

pub(crate) fn create_worktree_params(
    source_cwd: &Path,
    spec: &WorktreeSpec,
    worktree_id: &str,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "sourceWorktreePath": source_cwd.to_string_lossy(),
        "newSessionId": worktree_id,
        "copyMode": spec.copy_mode(),
    });
    if let Some(obj) = params.as_object_mut() {
        if let Some(label) = &spec.label {
            obj.insert("label".into(), serde_json::Value::String(label.clone()));
        }
        if let Some(r) = &spec.git_ref {
            obj.insert("gitRef".into(), serde_json::Value::String(r.clone()));
        }
    }
    params
}

pub(crate) fn resume_worktree_params(
    source_cwd: &Path,
    spec: &WorktreeSpec,
    session_id: &str,
    restore_code: Option<bool>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "sessionId": session_id,
        "sourceCwd": source_cwd.to_string_lossy(),
        "copyMode": spec.copy_mode(),
        "worktreeType": distill_shell::util::config::worktree_type(),
    });
    // Omitted when unset so the agent-side default applies.
    if let Some(obj) = params.as_object_mut() {
        if let Some(rc) = restore_code {
            obj.insert("restoreCode".into(), serde_json::Value::Bool(rc));
        }
        if let Some(r) = &spec.git_ref {
            obj.insert("gitRef".into(), serde_json::Value::String(r.clone()));
        }
    }
    params
}

/// Unwrap an `x.ai/*` extension envelope: an `error` member is a failure, `result` (or the
/// bare object) is the payload.
fn ext_result(raw: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if let Some(err) = value.get("error").filter(|v| !v.is_null()) {
        return Err(err
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| err.to_string()));
    }
    Ok(value.get("result").cloned().unwrap_or(value))
}

fn required_path(obj: &serde_json::Value, key: &str) -> Result<PathBuf, String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| format!("response missing {key}"))
}

pub(crate) fn parse_create_response(
    raw: &str,
    source_cwd: &Path,
) -> Result<CreatedWorktree, String> {
    let result = ext_result(raw)?;
    let worktree_root = required_path(&result, "worktreePath")?;
    let session_cwd = worktree_session_cwd(
        &worktree_root,
        result.get("sourceGitRoot").and_then(|v| v.as_str()),
        source_cwd,
    );
    Ok(CreatedWorktree {
        worktree_root,
        session_cwd,
        strategy_summary: parse_worktree_strategy_summary(&result),
    })
}

pub(crate) fn parse_resume_response(
    raw: &str,
    requested_session_id: &str,
) -> Result<ResumedWorktree, String> {
    let result = ext_result(raw)?;
    let worktree_root = required_path(&result, "worktreePath")?;
    let session_cwd = result
        .get("effectiveCwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree_root.clone());
    let session_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or(requested_session_id)
        .to_owned();
    let (code_restored, restore_summary, restore_degree) = parse_worktree_restore_payload(&result);
    Ok(ResumedWorktree {
        session_id,
        worktree_root,
        session_cwd,
        code_restored,
        restore_summary,
        restore_degree,
        strategy_summary: parse_worktree_strategy_summary(&result),
    })
}

fn create_failure(detail: &str) -> WorktreeRpcError {
    WorktreeRpcError {
        message: sanitize_user_error(&format!("couldn't create worktree: {detail}")),
    }
}

/// Create a worktree from `source_cwd`. Unbounded send: hydrating a large checkout can outlast
/// the session RPC budget, and the agent has no way to resume a half-built worktree.
pub(crate) async fn create_worktree(
    acp_tx: &AcpAgentTx,
    source_cwd: &Path,
    spec: &WorktreeSpec,
    worktree_id: &str,
) -> Result<CreatedWorktree, WorktreeRpcError> {
    let params = create_worktree_params(source_cwd, spec, worktree_id);
    let req = acp::ExtRequest::new(
        CREATE_METHOD,
        serde_json::value::to_raw_value(&params)
            .map_err(|e| create_failure(&e.to_string()))?
            .into(),
    );
    let resp = acp_send(req, acp_tx)
        .await
        .map_err(|e| create_failure(&e.to_string()))?;
    parse_create_response(resp.0.get(), source_cwd).map_err(|d| create_failure(&d))
}

/// Resume `session_id` into a fresh worktree; the agent restores the snapshot when asked.
/// `local_miss` is the requested target when it missed locally, so the failure text can say so.
pub(crate) async fn resume_session_into_worktree(
    acp_tx: &AcpAgentTx,
    source_cwd: &Path,
    spec: &WorktreeSpec,
    session_id: &str,
    restore_code: Option<bool>,
    local_miss: Option<&str>,
) -> Result<ResumedWorktree, WorktreeRpcError> {
    // Sanitize before appending the hint; the sanitizer collapses disk-full chains whole.
    let fail = |detail: &str| WorktreeRpcError {
        message: worktree_resume_failure_message(local_miss, &sanitize_user_error(detail)),
    };
    let params = resume_worktree_params(source_cwd, spec, session_id, restore_code);
    let req = acp::ExtRequest::new(
        RESUME_METHOD,
        serde_json::value::to_raw_value(&params)
            .map_err(|e| fail(&e.to_string()))?
            .into(),
    );
    let started = std::time::Instant::now();
    let resp = match acp_send_bounded(req, acp_tx, "Worktree session resume").await {
        Ok(resp) => {
            tracing::info!(
                session_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "worktree resume_session: ACP call completed"
            );
            resp
        }
        Err(e) => {
            tracing::warn!(
                session_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "worktree resume_session: ACP call failed"
            );
            return Err(fail(&e.to_string()));
        }
    };
    parse_resume_response(resp.0.get(), session_id).map_err(|d| fail(&d))
}

/// Append the worktree location to a failure that happened after the worktree was created, so
/// the caller can reclaim it.
pub(crate) fn note_orphaned_worktree(message: &str, worktree_root: &Path) -> String {
    format!(
        "{message} (worktree {} was created and is still on disk; remove it with `grok worktree rm`)",
        worktree_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j<'a>(v: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
        let Some(got) = v.get(key) else {
            panic!("missing json key {key}: {v}");
        };
        got
    }

    #[test]
    fn spec_from_cli_flags() {
        assert_eq!(WorktreeSpec::from_cli(None, None), None);
        let bare = WorktreeSpec::from_cli(Some(""), None).unwrap();
        assert_eq!(bare, WorktreeSpec::default());
        assert_eq!(bare.copy_mode(), CopyMode::Dirty);
        let named = WorktreeSpec::from_cli(Some("fix"), Some("origin/main")).unwrap();
        assert_eq!(named.label.as_deref(), Some("fix"));
        assert_eq!(named.git_ref.as_deref(), Some("origin/main"));
        assert_eq!(named.copy_mode(), CopyMode::Clean);
    }

    #[test]
    fn worktree_id_prefers_cli_session_id() {
        assert_eq!(new_worktree_id(Some("abc")), "abc");
        let generated = new_worktree_id(None);
        assert!(generated.starts_with("pager-"));
        assert_eq!(generated.len(), "pager-".len() + 12);
    }

    #[test]
    fn create_params_carry_label_ref_and_copy_mode() {
        let spec = WorktreeSpec::from_cli(Some("fix"), Some("origin/main")).unwrap();
        let p = create_worktree_params(Path::new("/repo/sub"), &spec, "wt-1");
        assert_eq!(j(&p, "sourceWorktreePath"), "/repo/sub");
        assert_eq!(j(&p, "newSessionId"), "wt-1");
        assert_eq!(j(&p, "copyMode"), "clean");
        assert_eq!(j(&p, "label"), "fix");
        assert_eq!(j(&p, "gitRef"), "origin/main");

        let bare = create_worktree_params(Path::new("/repo"), &WorktreeSpec::default(), "wt-2");
        assert_eq!(j(&bare, "copyMode"), "dirty");
        assert!(bare.get("label").is_none());
        assert!(bare.get("gitRef").is_none());
    }

    #[test]
    fn resume_params_omit_restore_code_when_unset() {
        let spec = WorktreeSpec::default();
        let p = resume_worktree_params(Path::new("/repo"), &spec, "sid", None);
        assert_eq!(j(&p, "sessionId"), "sid");
        assert_eq!(j(&p, "sourceCwd"), "/repo");
        assert_eq!(j(&p, "copyMode"), "dirty");
        assert!(p.get("restoreCode").is_none());
        assert!(p.get("worktreeType").is_some());

        let p = resume_worktree_params(Path::new("/repo"), &spec, "sid", Some(true));
        assert_eq!(j(&p, "restoreCode"), true);
    }

    #[test]
    fn create_response_lands_in_matching_subdirectory() {
        let raw = r#"{"result":{"worktreePath":"/wt/x","sourceGitRoot":"/repo"}}"#;
        let created = parse_create_response(raw, Path::new("/repo/crates/pager")).unwrap();
        assert_eq!(created.worktree_root, PathBuf::from("/wt/x"));
        assert_eq!(created.session_cwd, PathBuf::from("/wt/x/crates/pager"));

        let bare = r#"{"worktreePath":"/wt/y"}"#;
        let created = parse_create_response(bare, Path::new("/repo")).unwrap();
        assert_eq!(created.session_cwd, PathBuf::from("/wt/y"));
    }

    #[test]
    fn create_response_errors_surface() {
        assert_eq!(
            parse_create_response(r#"{"error":"disk full"}"#, Path::new("/repo")).unwrap_err(),
            "disk full"
        );
        assert_eq!(
            parse_create_response(r#"{"error":{"code":1}}"#, Path::new("/repo")).unwrap_err(),
            r#"{"code":1}"#
        );
        assert_eq!(
            parse_create_response(r#"{"result":{}}"#, Path::new("/repo")).unwrap_err(),
            "response missing worktreePath"
        );
        assert!(parse_create_response("not json", Path::new("/repo")).is_err());
        assert!(
            parse_create_response(r#"{"error":null,"worktreePath":"/wt"}"#, Path::new("/repo"))
                .is_ok()
        );
    }

    #[test]
    fn resume_response_adopts_agent_session_id_and_effective_cwd() {
        let raw = r#"{"result":{"sessionId":"forked","worktreePath":"/wt/z","effectiveCwd":"/wt/z/sub","codeRestored":true,"restoreSummary":"3 files"}}"#;
        let resumed = parse_resume_response(raw, "orig").unwrap();
        assert_eq!(resumed.session_id, "forked");
        assert_eq!(resumed.worktree_root, PathBuf::from("/wt/z"));
        assert_eq!(resumed.session_cwd, PathBuf::from("/wt/z/sub"));
        assert!(resumed.code_restored);
        assert_eq!(resumed.restore_summary.as_deref(), Some("3 files"));

        let minimal = parse_resume_response(r#"{"worktreePath":"/wt/z"}"#, "orig").unwrap();
        assert_eq!(minimal.session_id, "orig");
        assert_eq!(minimal.session_cwd, PathBuf::from("/wt/z"));
        assert!(!minimal.code_restored);
    }

    #[test]
    fn resume_response_requires_worktree_path() {
        assert_eq!(
            parse_resume_response(r#"{"result":{"sessionId":"x"}}"#, "x").unwrap_err(),
            "response missing worktreePath"
        );
    }

    #[test]
    fn orphan_note_names_the_path() {
        let msg = note_orphaned_worktree("Couldn't create session: boom", Path::new("/wt/q"));
        assert!(msg.starts_with("Couldn't create session: boom"));
        assert!(msg.contains("/wt/q"));
        assert!(msg.contains("grok worktree rm"));
    }
}
