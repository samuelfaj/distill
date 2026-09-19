// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::Path;

use distill_workspace::session::git::VcsKind;

// Re-export from distill-chat-state; the canonical definition lives there
pub(crate) use distill_chat_state::compaction_utils::extract_user_query;

pub(crate) fn user_query(user_message: String) -> String {
    format!(
        r#"<user_query>
{user_message}
</user_query>"#
    )
}

/// Environment info for constructing the `<user_info>` block.
/// When `None`, values are read from the local machine.
/// When `Some`, the provided values are used (e.g. from a remote workspace via `workspace.info` RPC).
pub(crate) struct UserInfoOverride {
    pub os: String,
    pub shell: String,
    pub cwd: String,
}

/// Minimal user message prefix for fast-start / headless contexts.
/// Intentionally excludes workspace snapshot and git status.
/// When `override_info` is provided, uses remote workspace info instead of local machine introspection.
pub(crate) fn construct_user_message_minimal(
    working_directory: &Path,
    override_info: Option<&UserInfoOverride>,
) -> String {
    let local_shell;
    let (os, shell, cwd) = match override_info {
        Some(info) => (info.os.as_str(), info.shell.as_str(), info.cwd.clone()),
        None => {
            local_shell = resolve_shell_display();
            (
                std::env::consts::OS,
                local_shell.as_str(),
                working_directory.to_string_lossy().to_string(),
            )
        }
    };
    let today = chrono::Local::now().format("%Y-%m-%d");
    format!(
        r#"<user_info>
OS Version: {os}
Shell: {shell}
Workspace Path: {cwd}
{USER_INFO_DATE_MARKER} {today}
</user_info>"#,
    )
}

/// Date label in the `<user_info>` prefix; `spawn::resumed_prefix_carries_fallback_date` scans for it.
pub(crate) const USER_INFO_DATE_MARKER: &str = "Today's date:";

/// Resolve a display string for the user's shell.
/// Unix: full path from `$SHELL`.
/// Windows: `detect_windows_shell` tries pwsh, then powershell.exe, then Git Bash, then cmd.exe.
fn resolve_shell_display() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }

    #[cfg(not(unix))]
    {
        distill_config::shell::detect_windows_shell()
            .name()
            .to_string()
    }
}

pub(crate) fn format_vcs_status_block(status: &str, vcs_kind: VcsKind) -> String {
    let (tag, description) = if vcs_kind.is_jj() {
        (
            "jj_status",
            "This is the Jujutsu (jj) status at the start of the conversation. This is a \
             jj-managed repository \u{2014} use `jj` commands instead of `git`. There is no staging \
             area; all changes are part of the working-copy commit (@). Use `jj describe` to \
             set commit messages and `jj new` to finalize changes.",
        )
    } else {
        (
            "git_status",
            "This is the git status at the start of the conversation. Note that this status \
             is a snapshot in time, and will not update during the conversation.",
        )
    };
    format!("\n\n<{tag}>\n{description}\n{status}\n</{tag}>\n")
}

// Tests for extract_user_query now live in distill_chat_state::compaction_utils.
// The `<user_info>` + status block is assembled by `SessionActor::construct_legacy_prefix`
// (see `acp_session_impl/prompt_build.rs`) from a single `RepoStatusSnapshot`.
