// Modified for Distill by Samuel Fajreldines, 2026.
//! One parser serves the resume picker ([`super::Effect::FetchSessionList`]) and the dashboard's
//! non-leader idle-session fallback ([`super::Effect::FetchDashboardSessions`]), so both surfaces
//! label rows identically.

use std::collections::HashSet;

use agent_client_protocol as acp;
use distill_shell::session::resolve_local_session_ids_any_cwd;
use distill_shell::session::unified_list::ListScope;
use distill_tools::implementations::skills::skill::extract_skill_display_text;
use serde_json::Value;

use super::helpers::extract_first_user_prompt;
use crate::app::app_view::SessionPickerEntry;
use crate::app::foreign_sessions::is_foreign_picker_source;
use crate::app::roster::{RosterActivity, RosterEntry, RosterOrigin};
use crate::views::session_picker::repo_name_from_cwd;
use crate::views::session_picker_surface::SessionPickerHost;

/// Which rows the local-storage lookup applies to; the store is walked at most once either way.
#[derive(Debug, Clone, Copy)]
pub(super) enum LocalPresence {
    /// `remote` rows found on disk become `local`; everything else keeps the shell's label.
    Relabel,
    /// Only rows found on disk survive, labelled `local`. Conversation and foreign rows never qualify.
    Require,
}

impl LocalPresence {
    /// The dashboard picker only shows sessions on this machine; the other pickers keep remote rows.
    pub(super) fn for_host(host: SessionPickerHost) -> Self {
        match host {
            SessionPickerHost::Dashboard => Self::Require,
            SessionPickerHost::Welcome | SessionPickerHost::AgentModal => Self::Relabel,
        }
    }
}

/// Degraded conversations lane on `x.ai/session/list`, parsed from the response's `_meta["x.ai/partial"]` envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationsPartial {
    NoOauth,
    Timeout,
    Error,
}

impl ConversationsPartial {
    pub(crate) fn picker_notice(self) -> &'static str {
        match self {
            Self::NoOauth => "Couldn't load your chats: log in with /login",
            Self::Timeout | Self::Error => "Couldn't load conversations: retry",
        }
    }
}

/// `x.ai/session/list` answers arrive either as a JSON-RPC envelope (`result` or `error`) or as the bare list payload.
pub(super) fn read_session_list_response(raw: &str) -> Result<Value, String> {
    let mut wrapper: Value = serde_json::from_str(raw).unwrap_or_default();
    if let Some(err) = wrapper.get("error") {
        return Err(err.as_str().unwrap_or("unknown error").to_owned());
    }
    Ok(wrapper
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(wrapper))
}

/// `None` when the conversations lane completed (or was skipped); unknown reasons degrade to [`ConversationsPartial::Error`].
pub(super) fn parse_session_list_partial(payload: &Value) -> Option<ConversationsPartial> {
    let partial = payload.get("_meta")?.get("x.ai/partial")?;
    if partial.get("conversations").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(match partial.get("reason").and_then(Value::as_str) {
        Some("no_oauth") => ConversationsPartial::NoOauth,
        Some("timeout") => ConversationsPartial::Timeout,
        _ => ConversationsPartial::Error,
    })
}

pub(super) fn parse_session_list_scope(payload: &Value) -> ListScope {
    match payload
        .get("_meta")
        .and_then(|m| m.get("x.ai/listScope"))
        .and_then(Value::as_str)
    {
        Some("repo") => ListScope::Repo,
        Some("all") => ListScope::All,
        _ => ListScope::Cwd,
    }
}

/// The storage walk and the `chat_history.jsonl` reads belong on the blocking pool.
pub(super) async fn parse_session_picker_entries_blocking(
    payload: Value,
    presence: LocalPresence,
) -> Result<Vec<SessionPickerEntry>, String> {
    tokio::task::spawn_blocking(move || {
        parse_session_picker_entries_with(payload, presence, |ids| {
            resolve_local_session_ids_any_cwd(ids).map_err(|error| error.to_string())
        })
    })
    .await
    .map_err(|error| format!("session list parse task failed: {error}"))?
}

/// Sessions older than 30 days, and sessions with no usable user prompt (empty `summary` after fallbacks), are dropped.
///
/// `resolve_local` receives the candidate ids for `presence` and returns the subset that exists on disk; each call is a full `~/.grok/sessions` walk.
fn parse_session_picker_entries_with(
    mut payload: Value,
    presence: LocalPresence,
    resolve_local: impl FnOnce(&[&str]) -> Result<HashSet<String>, String>,
) -> Result<Vec<SessionPickerEntry>, String> {
    let entries = match payload.get_mut("sessions").map(Value::take) {
        Some(Value::Array(entries)) => entries,
        _ => Vec::new(),
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);

    let mut parsed: Vec<SessionPickerEntry> = entries
        .into_iter()
        .filter_map(|v| {
            let id = v
                .get("sessionId")
                .or_else(|| v.get("session_id"))
                .and_then(Value::as_str)?
                .to_owned();
            let summary = v
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let first_prompt = v
                .get("firstPrompt")
                .or_else(|| v.get("first_prompt"))
                .and_then(Value::as_str)
                .map(String::from);
            let is_conversation = v
                .get("_meta")
                .and_then(|m| m.get("x.ai/session"))
                .and_then(|s| s.get("kind"))
                .and_then(Value::as_str)
                == Some("chat");

            let parsed_updated: Option<chrono::DateTime<chrono::Utc>> = v
                .get("updatedAt")
                .or_else(|| v.get("updated_at"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());
            let parsed_created: Option<chrono::DateTime<chrono::Utc>> = v
                .get("createdAt")
                .or_else(|| v.get("created_at"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());

            let updated_at: chrono::DateTime<chrono::Utc> = match parsed_updated {
                Some(ts) => {
                    if !is_conversation && ts < cutoff {
                        return None;
                    }
                    ts
                }
                None => {
                    if !is_conversation {
                        return None;
                    }
                    parsed_created.unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
                }
            };

            // Prefer first_prompt for skill display: it has the complete XML including <command-args>
            // The LLM-generated summary can be a truncated clean string like "/implement" (no XML, no args)
            let display = if let Some(ref fp) = first_prompt {
                if let Some(d) = extract_skill_display_text(fp) {
                    d
                } else if !summary.is_empty() {
                    extract_skill_display_text(&summary).unwrap_or(summary)
                } else {
                    fp.lines().next().unwrap_or_default().trim().to_owned()
                }
            } else if !summary.is_empty() {
                extract_skill_display_text(&summary).unwrap_or(summary)
            } else {
                let info_cwd = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let info = distill_shell::session::info::Info {
                    id: acp::SessionId::new(id.clone()),
                    cwd: info_cwd,
                };
                extract_first_user_prompt(&info).unwrap_or_default()
            };

            let created_at: chrono::DateTime<chrono::Utc> = parsed_created.unwrap_or(updated_at);
            let cwd_str = v
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let hostname = v.get("hostname").and_then(Value::as_str).map(String::from);
            let source = if is_conversation {
                "conversation".to_owned()
            } else {
                v.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("local")
                    .to_owned()
            };
            let model_id = v
                .get("modelId")
                .or_else(|| v.get("model_id"))
                .and_then(Value::as_str)
                .map(String::from);
            let num_messages = v
                .get("numMessages")
                .or_else(|| v.get("num_messages"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let last_active_at: Option<chrono::DateTime<chrono::Utc>> = v
                .get("lastActiveAt")
                .or_else(|| v.get("last_active_at"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());

            let branch = v.get("branch").and_then(Value::as_str).map(String::from);
            let worktree_label = v
                .get("worktreeLabel")
                .or_else(|| v.get("worktree_label"))
                .and_then(Value::as_str)
                .map(String::from);
            let last_turn_summary = v
                .get("lastTurnSummary")
                .or_else(|| v.get("last_turn_summary"))
                .and_then(Value::as_str)
                .map(String::from);
            let last_recap = v
                .get("lastRecap")
                .or_else(|| v.get("last_recap"))
                .and_then(Value::as_str)
                .map(String::from);
            let session_kind = v
                .get("sessionKind")
                .or_else(|| v.get("session_kind"))
                .and_then(Value::as_str)
                .map(String::from);
            let repo_name = repo_name_from_cwd(&cwd_str);

            Some(SessionPickerEntry {
                id,
                summary: display,
                updated_at,
                created_at,
                cwd: cwd_str,
                hostname,
                source,
                model_id,
                num_messages,
                last_active_at,
                branch,
                repo_name,
                worktree_label,
                last_turn_summary,
                last_recap,
                session_kind,
                card_detail: None,
            })
        })
        .filter_map(|mut e| {
            // A Build row with no prompt is one the user opened but never typed into; listing it in `/resume` is noise
            // Conversation rows stay: new grok.com chats have no title until server-side titling runs
            if e.summary.is_empty() {
                if e.source == "conversation" {
                    e.summary = "Untitled".to_owned();
                } else {
                    return None;
                }
            }
            Some(e)
        })
        .collect();

    let resolve_local = |ids: &[&str]| {
        resolve_local(ids).map_err(|error| {
            tracing::warn!(%error, ?presence, "session list local-session resolution failed");
            error
        })
    };
    match presence {
        // The shell labels a row `remote` when it is absent from the cwd buckets it scanned, so a session stored under another cwd is still local
        LocalPresence::Relabel => {
            let remote_ids: Vec<&str> = parsed
                .iter()
                .filter(|e| e.source == "remote")
                .map(|e| e.id.as_str())
                .collect();
            if remote_ids.is_empty() {
                return Ok(parsed);
            }
            // The shell's labels are still usable without the disk check, so the list degrades instead of failing
            let Ok(local_ids) = resolve_local(&remote_ids) else {
                return Ok(parsed);
            };
            // A conversation row can share an id with a Build row; only the rows that supplied the ids may flip
            for e in parsed
                .iter_mut()
                .filter(|e| e.source == "remote" && local_ids.contains(&e.id))
            {
                e.source = "local".to_owned();
            }
            Ok(parsed)
        }
        LocalPresence::Require => {
            parsed.retain(|e| e.source != "conversation" && !is_foreign_picker_source(&e.source));
            if parsed.is_empty() {
                return Ok(parsed);
            }
            let candidate_ids: Vec<&str> = parsed.iter().map(|e| e.id.as_str()).collect();
            let local_ids = resolve_local(&candidate_ids)
                .map_err(|error| format!("couldn't resolve local sessions: {error}"))?;
            parsed.retain(|e| local_ids.contains(&e.id));
            for e in &mut parsed {
                e.source = "local".to_owned();
            }
            Ok(parsed)
        }
    }
}

/// Local on-disk sessions have no live activity signal, so they map to [`RosterActivity::Dormant`] and render in the dashboard's **Inactive** group.
pub(super) fn session_picker_entry_to_roster(e: SessionPickerEntry) -> RosterEntry {
    let last_change = e.last_active_at.unwrap_or(e.updated_at);
    RosterEntry {
        session_id: e.id,
        title: Some(e.summary).filter(|s| !s.trim().is_empty()),
        cwd: e.cwd,
        is_worktree: e.worktree_label.is_some(),
        session_kind: None,
        model_id: e.model_id,
        yolo: false,
        activity: RosterActivity::Dormant,
        last_turn_summary: e.last_turn_summary,
        resident: false,
        last_change_unix_ms: last_change.timestamp_millis(),
        origin: RosterOrigin {
            kind: e.source,
            host: e.hostname,
        },
    }
}

#[cfg(test)]
#[path = "session_list_tests.rs"]
mod tests;
