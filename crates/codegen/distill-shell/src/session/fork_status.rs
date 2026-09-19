//! One-shot status written when a session is forked, consumed on the first
//! prompt so the model learns this is a new session and what was not carried.

use std::path::Path;

use serde::{Deserialize, Serialize};

const FILENAME: &str = "fork_status.json";
const CLAIMED: &str = "fork_status.json.claimed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForkStatus {
    pub kind: String,
    pub source_cwd: String,
    pub new_cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_display_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Resume copies reuse the fork copier. They must not announce themselves as forks.
/// Subagent forks are not announced yet.
pub(crate) fn should_persist(session_kind: &str, fork_context_source: Option<&str>) -> bool {
    session_kind != "subagent_resume"
        && session_kind != "subagent_fork"
        && fork_context_source != Some("resumed")
}

pub(crate) fn capture(
    kind: &str,
    source_cwd: &str,
    new_cwd: &str,
    source_workspace_dir: Option<&str>,
    prompt_display_cwd: Option<&str>,
    truncated: bool,
) -> ForkStatus {
    ForkStatus {
        kind: kind.to_string(),
        source_cwd: source_cwd.to_string(),
        new_cwd: new_cwd.to_string(),
        source_workspace_dir: source_workspace_dir.map(str::to_string),
        prompt_display_cwd: prompt_display_cwd.map(str::to_string),
        truncated,
    }
}

pub(crate) fn persist(session_dir: &Path, status: &ForkStatus) {
    let path = session_dir.join(FILENAME);
    let Ok(data) = serde_json::to_vec_pretty(status) else {
        tracing::warn!("failed to serialize fork status");
        return;
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(e) = std::io::Write::write_all(&mut file, &data) {
                tracing::warn!(%e, "failed to write fork status");
                let _ = std::fs::remove_file(&path);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => tracing::warn!(%e, "failed to create fork status"),
    }
}

/// Rename the marker so only one injector owns it. A crash before delivery leaves
/// the claim in place for the next prompt.
pub(crate) fn claim(session_dir: &Path) -> Option<ForkStatus> {
    let path = session_dir.join(FILENAME);
    let claimed = session_dir.join(CLAIMED);
    if !claimed.is_file() && std::fs::rename(&path, &claimed).is_err() {
        return None;
    }
    let data = std::fs::read(&claimed).ok()?;
    match serde_json::from_slice(&data) {
        Ok(status) => Some(status),
        Err(e) => {
            tracing::warn!(%e, "failed to parse fork status");
            let _ = std::fs::remove_file(&claimed);
            None
        }
    }
}

pub(crate) fn commit_claim(session_dir: &Path) {
    let _ = std::fs::remove_file(session_dir.join(CLAIMED));
}

pub(crate) fn release_claim(session_dir: &Path) {
    let claimed = session_dir.join(CLAIMED);
    let path = session_dir.join(FILENAME);
    if path.exists() {
        let _ = std::fs::remove_file(&claimed);
        return;
    }
    let _ = std::fs::rename(&claimed, &path);
}

pub(crate) fn format_reminder(status: &ForkStatus) -> String {
    let mut body = String::from(opening(status.kind.as_str()));
    push_cwd_lines(&mut body, status);
    body.push('\n');
    body.push_str(history_line(status));
    body.push_str(
        "Shells and background tasks from the source session are not attached to this session. \
         Do not treat task ids in the copied history as this session's.\n\
         Subagents from the source are not attached to this session.\n\
         A server mentioned in the copied history is not this session's. It may still be listening.\n\
         This session does not inherit the source's scheduled loops, workflows, or goal.\n",
    );
    body
}

fn opening(kind: &str) -> &'static str {
    match kind {
        "worktree" => {
            "This session was forked into a worktree. It is a new session; the source \
             continues independently and does not see work done here.\n"
        }
        _ => {
            "This session was forked from another session. It is a new session; the source \
             continues independently and does not see this conversation.\n"
        }
    }
}

fn push_cwd_lines(body: &mut String, status: &ForkStatus) {
    let display = status.prompt_display_cwd.as_deref();
    if let Some(display) = display
        && display != status.new_cwd
    {
        let _ = std::fmt::Write::write_fmt(
            body,
            format_args!(
                "The workspace path in the prompt is {display}. Tools run in {}.\n",
                status.new_cwd
            ),
        );
    } else {
        let _ = std::fmt::Write::write_fmt(
            body,
            format_args!("Working directory: {}.\n", status.new_cwd),
        );
    }
    if status.kind == "worktree"
        && let Some(source) = status.source_workspace_dir.as_deref()
    {
        let _ = std::fmt::Write::write_fmt(
            body,
            format_args!(
                "The source checkout is {source}. Edits here do not change that checkout.\n"
            ),
        );
    } else if status.source_cwd == status.new_cwd {
        body.push_str("Filesystem edits in this checkout are shared with the source session.\n");
    } else if display != Some(status.source_cwd.as_str()) {
        let _ = std::fmt::Write::write_fmt(
            body,
            format_args!(
                "The source session's working directory was {}.\n",
                status.source_cwd
            ),
        );
    }
}

fn history_line(status: &ForkStatus) -> &'static str {
    if status.truncated {
        "Conversation history was copied up to the selected point. Later source turns are not here.\n"
    } else {
        "Conversation history up to the fork point was copied. Later work in the source is not here.\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kind: &str) -> ForkStatus {
        ForkStatus {
            kind: kind.into(),
            source_cwd: "/src".into(),
            new_cwd: "/dst".into(),
            source_workspace_dir: None,
            prompt_display_cwd: None,
            truncated: false,
        }
    }

    #[test]
    fn resume_and_subagent_fork_are_not_announced() {
        assert!(!should_persist("subagent_resume", Some("resumed")));
        assert!(!should_persist("fork", Some("resumed")));
        assert!(!should_persist("subagent_fork", Some("forked")));
        assert!(should_persist("fork", None));
        assert!(should_persist("worktree", None));
    }

    #[test]
    fn format_says_this_is_a_new_session() {
        let reminder = format_reminder(&status("fork"));
        assert!(reminder.contains("This session was forked from another session."));
        assert!(reminder.contains("does not see this conversation"));
        assert!(!reminder.contains("does not see work done here"));
        assert!(reminder.contains("Working directory: /dst."));
        assert!(reminder.contains("The source session's working directory was /src."));
        assert!(!reminder.contains("Filesystem edits in this checkout are shared"));
        assert!(reminder.contains("Later work in the source is not here."));
        assert!(reminder.contains("are not attached to this session"));
        assert!(reminder.contains("task ids in the copied history"));
        assert!(reminder.contains("may still be listening"));
        assert!(reminder.contains("does not inherit the source's scheduled loops"));
        assert!(!reminder.contains("were not copied"));
        assert!(!reminder.contains("temp files"));
    }

    #[test]
    fn format_worktree_names_display_path_and_source_checkout() {
        let mut worktree = status("worktree");
        worktree.source_workspace_dir = Some("/proj".into());
        worktree.prompt_display_cwd = Some("/proj".into());
        let reminder = format_reminder(&worktree);
        assert!(reminder.contains("forked into a worktree"));
        assert!(reminder.contains("The workspace path in the prompt is /proj."));
        assert!(reminder.contains("Tools run in /dst."));
        assert!(reminder.contains("The source checkout is /proj."));
        assert!(reminder.contains("Edits here do not change that checkout."));
    }

    #[test]
    fn format_truncated_history_names_the_selected_point() {
        let mut status = status("fork");
        status.new_cwd = "/src".into();
        status.truncated = true;
        let reminder = format_reminder(&status);
        assert!(reminder.contains("copied up to the selected point"));
        assert!(reminder.contains("Filesystem edits in this checkout are shared"));
        assert!(!reminder.contains("source session's working directory"));
    }

    #[test]
    fn claim_retries_until_committed() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), &status("fork"));
        let loaded = claim(dir.path()).expect("marker");
        assert_eq!(loaded.kind, "fork");
        assert!(claim(dir.path()).is_some(), "a crashed claim is retried");
        commit_claim(dir.path());
        assert!(claim(dir.path()).is_none());

        persist(dir.path(), &status("fork"));
        assert!(claim(dir.path()).is_some());
        release_claim(dir.path());
        let again = claim(dir.path()).expect("released marker");
        assert_eq!(again.kind, "fork");
        commit_claim(dir.path());
    }
}
