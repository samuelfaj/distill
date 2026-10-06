// Modified for Distill by Samuel Fajreldines, 2026.
use std::io;
use std::ops::Range;
use std::path::Path;

use distill_chat_state::{ChatPersistence, StrictAppendAck, StrictAppendError};
use distill_sampling_types::ConversationItem;
use tokio::sync::{mpsc, oneshot};

use super::persistence::PersistenceMsg;

/// Production `ChatPersistence` that sends to the existing session persistence channel.
pub(crate) struct ChannelChatPersistence {
    tx: mpsc::UnboundedSender<PersistenceMsg>,
    /// The session whose read-reuse index a history eviction invalidates.
    session_id: Option<String>,
}

impl ChannelChatPersistence {
    pub(crate) fn new(tx: mpsc::UnboundedSender<PersistenceMsg>) -> Self {
        Self {
            tx,
            session_id: None,
        }
    }

    pub(crate) fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }
}

impl ChatPersistence for ChannelChatPersistence {
    fn persist_message(&mut self, item: &ConversationItem) {
        let _ = self.tx.send(PersistenceMsg::Chat(item.clone()));
    }

    fn archive_tool_result(
        &mut self,
        tool_name: &str,
        tool_arguments: &str,
        payload: &str,
    ) -> Option<(String, Range<usize>)> {
        archive_tool_result_at(
            tool_name,
            tool_arguments,
            payload,
            &crate::jev_store::store_dir(),
        )
    }

    fn archive_evicted_text(
        &mut self,
        tool_name: &str,
        tool_arguments: &str,
        payload: &str,
    ) -> Option<String> {
        archive_evicted_text_at(
            tool_name,
            tool_arguments,
            payload,
            &crate::jev_store::store_dir(),
        )
    }

    fn history_evicted(&mut self) {
        // A reuse note must never point at a copy that is no longer in history.
        if let Some(session_id) = &self.session_id {
            crate::jev::invalidate_payload_reads_for_session(session_id);
        }
    }

    fn persist_working_directory_switch_and_ack(
        &mut self,
        item: &ConversationItem,
    ) -> oneshot::Receiver<Result<StrictAppendAck, StrictAppendError>> {
        let (reply, receiver) = oneshot::channel();
        if self
            .tx
            .send(PersistenceMsg::AppendCwdSwitchAndAck {
                item: item.clone(),
                respond_to: reply,
            })
            .is_err()
        {
            let (reply, receiver) = oneshot::channel();
            let _ = reply.send(Err(StrictAppendError::Indeterminate(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor unavailable; retry by generation",
            ))));
            return receiver;
        }
        receiver
    }

    fn replace_history(&mut self, items: &[ConversationItem]) {
        let _ = self
            .tx
            .send(PersistenceMsg::ReplaceChatHistory(items.to_vec()));
    }

    fn replace_history_for_strip_and_ack(
        &mut self,
        items: &[ConversationItem],
    ) -> oneshot::Receiver<io::Result<()>> {
        let (respond_to, receiver) = oneshot::channel();
        if self
            .tx
            .send(PersistenceMsg::ReplaceChatHistoryForStripAndAck {
                messages: items.to_vec(),
                respond_to,
            })
            .is_err()
        {
            let (reply, receiver) = oneshot::channel();
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor unavailable for strip rewrite",
            )));
            return receiver;
        }
        receiver
    }

    fn flush(&mut self) {
        let _ = self.tx.send(PersistenceMsg::Flush);
    }
}

fn archive_tool_result_at(
    tool_name: &str,
    tool_arguments: &str,
    payload: &str,
    store_dir: &Path,
) -> Option<(String, Range<usize>)> {
    let tool = tool_name
        .rsplit([':', '/'])
        .next()
        .unwrap_or(tool_name)
        .to_ascii_lowercase();
    if !matches!(tool.as_str(), "run_terminal_command" | "bash" | "shell") {
        return None;
    }
    let command = serde_json::from_str::<serde_json::Value>(tool_arguments)
        .ok()
        .and_then(|args| {
            ["command", "cmd", "script"].into_iter().find_map(|key| {
                args.get(key)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
        })
        .unwrap_or_default();
    let body_range = native_concise_progress_body_range(payload).unwrap_or(0..payload.len());
    let progress_payload = payload.get(body_range.clone()).unwrap_or(payload);
    if distill_workspace::jev::crushers::is_exact_output(tool_name, &command)
        || !matches!(
            distill_workspace::jev::retention::classify(&command, payload),
            distill_workspace::jev::retention::OutputCategory::Build
        )
        || !matches!(
            distill_workspace::jev::retention::gate(&command, payload),
            distill_workspace::jev::retention::Gate::Prune
        )
        || contains_critical_evidence(progress_payload)
        || distill_workspace::jev::crushers::secret_presence(payload).is_some()
        || !is_disposable_build_progress(progress_payload)
    {
        return None;
    }
    let path = crate::jev_store::store_payload_in(store_dir, payload)?;
    (std::fs::read(&path).ok()?.as_slice() == payload.as_bytes())
        .then(|| (path.display().to_string(), body_range))
}

/// Stores an old tool output before history eviction replaces it. What the
/// model works from rather than looks up stays whole: a user's answer, the
/// plan, the goal and the todo list, a skill and an instruction file (read by
/// any tool), and anything the utility path's secret screen flags, which is
/// never written to the store (the same rule as a utility-stored original).
/// Compaction stores what it digests in an already-cold input the same way.
pub(crate) fn archive_evicted_text_at(
    tool_name: &str,
    tool_arguments: &str,
    payload: &str,
    store_dir: &Path,
) -> Option<String> {
    let tool = tool_name
        .rsplit([':', '/'])
        .next()
        .unwrap_or(tool_name)
        .to_ascii_lowercase();
    if matches!(
        tool.as_str(),
        "ask_user_question"
            | "skill"
            | "todo_write"
            | "todowrite"
            | "update_goal"
            | "enter_plan_mode"
            | "exit_plan_mode"
    ) || ["/skills/", "SKILL.md", "AGENTS.md", "CLAUDE.md"]
        .iter()
        .any(|marker| tool_arguments.contains(marker))
        || distill_workspace::jev::crushers::utility_secret_presence(payload).is_some()
    {
        return None;
    }
    let path = crate::jev_store::store_payload_in(store_dir, payload)?;
    (std::fs::read(&path).ok()?.as_slice() == payload.as_bytes())
        .then(|| path.display().to_string())
}

const NATIVE_CONCISE_PREFIX: &str = "Exit code: 0\n\nCommand output:\n\n```\n";
const NATIVE_CONCISE_FOOTER: &str = "\n```\n\nCommand completed.\n\nThe previous shell command ended, so on the next invocation of this tool, you will be using a new shell session.\n\nOn the next terminal tool call, the directory of the shell will be ";

fn native_concise_progress_body_range(payload: &str) -> Option<Range<usize>> {
    let rest = payload.strip_prefix(NATIVE_CONCISE_PREFIX)?;
    let (body, current_dir) = rest.rsplit_once(NATIVE_CONCISE_FOOTER)?;
    let current_dir = current_dir.strip_suffix('.')?;
    if current_dir.is_empty() || current_dir.contains('\r') || current_dir.contains('\n') {
        return None;
    }
    Some(NATIVE_CONCISE_PREFIX.len()..NATIVE_CONCISE_PREFIX.len() + body.len())
}

fn is_disposable_build_progress(payload: &str) -> bool {
    let mut saw_progress = false;
    for line in payload.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if [
            "Compiling ",
            "Checking ",
            "Fresh ",
            "Downloading ",
            "Downloaded ",
            "Building ",
            "Bundling ",
            "Transpiling ",
            "Transforming ",
            "Installing ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            saw_progress = true;
        } else {
            return false;
        }
    }
    saw_progress
}

fn contains_critical_evidence(payload: &str) -> bool {
    const MARKERS: [&str; 14] = [
        "error", "panic", "failed", "failure", "warning", "traceback", "assert", "not found",
        "skipped", "skip", "test result", "finished", "exit code", "status",
    ];
    let lowered = payload.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_persistence_sends_chat_messages() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let item = ConversationItem::user("test");
        persistence.persist_message(&item);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::Chat(_)));
    }

    #[test]
    fn channel_persistence_archives_eligible_output_byte_identically() {
        let dir = tempfile::tempdir().expect("temp store");
        let payload = (0..2_500)
            .map(|i| format!("   Compiling crate-{i} v0.1.0\n"))
            .collect::<String>();
        let (path, body_range) = archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"cargo build"}"#,
            &payload,
            dir.path(),
        )
        .expect("eligible build progress should have a recovery path");
        assert_eq!(body_range, 0..payload.len());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), payload);
        std::fs::write(&path, "corrupt archive").unwrap();
        assert!(archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"cargo build"}"#,
            &payload,
            dir.path(),
        )
        .is_none());

        let unknown = format!(
            "prefix\nIMPORTANT specification text in the middle\nsuffix\n{}",
            "plain output ".repeat(2_500)
        );
        assert!(archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"python3 export_reference.py"}"#,
            &unknown,
            dir.path(),
        )
        .is_none());
        let mixed_build = format!("Compiling crate-0 v0.1.0\nIMPORTANT middle text\n{payload}");
        assert!(archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"cargo build"}"#,
            &mixed_build,
            dir.path(),
        )
        .is_none());
        assert!(archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"cargo build"}"#,
            &format!("error: command failed\n{payload}"),
            dir.path(),
        )
        .is_none());
    }

    #[test]
    fn channel_persistence_archives_native_concise_output_byte_identically() {
        let dir = tempfile::tempdir().expect("temp store");
        let body = (0..2_500)
            .map(|i| format!("   Compiling crate-{i} v0.1.0\n"))
            .collect::<String>();
        let payload =
            format!("{NATIVE_CONCISE_PREFIX}{body}{NATIVE_CONCISE_FOOTER}/workspace/project.");
        let parsed_body_range = native_concise_progress_body_range(&payload).unwrap();
        assert_eq!(payload.get(parsed_body_range), Some(body.as_str()));

        let (path, body_range) = archive_tool_result_at(
            "run_terminal_command",
            r#"{"command":"cargo build"}"#,
            &payload,
            dir.path(),
        )
        .expect("eligible native concise build progress should have a recovery path");
        assert_eq!(payload.get(body_range).unwrap(), body);
        assert_eq!(std::fs::read_to_string(path).unwrap(), payload);
    }

    /// History eviction may replace an old output only after the original is
    /// stored byte for byte; what must stay whole is refused, so the caller
    /// keeps today's bytes.
    #[test]
    fn evicted_output_is_stored_byte_identically_and_protected_text_is_refused() {
        let dir = tempfile::tempdir().expect("temp store");
        let payload = "src/lib.rs:1 fn main() {}\n".repeat(400);
        let path = archive_evicted_text_at(
            "read_file",
            r#"{"target_file":"src/lib.rs"}"#,
            &payload,
            dir.path(),
        )
        .expect("an ordinary read is stored");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), payload);

        for (tool, args) in [
            ("ask_user_question", r#"{"question":"which db?"}"#),
            ("skill", r#"{"name":"review"}"#),
            ("todo_write", r#"{"todos":[]}"#),
            ("read_file", r#"{"target_file":"/home/u/.claude/skills/x/SKILL.md"}"#),
            ("read_file", r#"{"target_file":"AGENTS.md"}"#),
        ] {
            assert!(
                archive_evicted_text_at(tool, args, &payload, dir.path()).is_none(),
                "{tool} {args} must stay whole"
            );
        }
        let secret = format!("{payload}OPENAI_API_KEY=sk-proj-AbCdEf1234567890GhIjKlMnOpQr\n");
        assert!(
            archive_evicted_text_at("run_terminal_command", r#"{"command":"env"}"#, &secret, dir.path())
                .is_none(),
            "a secret never reaches the store"
        );
    }

    #[tokio::test]
    async fn channel_persistence_sends_acknowledged_chat_append() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let item = ConversationItem::working_directory_switch("moved", 1);
        let ack = persistence.persist_working_directory_switch_and_ack(&item);
        let msg = rx.recv().await.unwrap();
        let PersistenceMsg::AppendCwdSwitchAndAck {
            item: persisted,
            respond_to,
        } = msg
        else {
            panic!("expected acknowledged chat append");
        };
        assert_eq!(
            serde_json::to_vec(&persisted).unwrap(),
            serde_json::to_vec(&item).unwrap()
        );
        respond_to.send(Ok(StrictAppendAck::Appended)).unwrap();
        assert!(matches!(
            ack.await.unwrap().unwrap(),
            StrictAppendAck::Appended
        ));
    }

    #[tokio::test]
    async fn channel_persistence_sends_replace_history() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        persistence.replace_history(&[ConversationItem::system("compacted")]);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::ReplaceChatHistory(_)));
    }

    #[tokio::test]
    async fn channel_persistence_sends_acked_strip_rewrite() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let ack = persistence.replace_history_for_strip_and_ack(&[ConversationItem::system("s")]);
        let msg = rx.recv().await.unwrap();
        let PersistenceMsg::ReplaceChatHistoryForStripAndAck { respond_to, .. } = msg else {
            panic!("expected acked strip rewrite, got {msg:?}");
        };
        respond_to.send(Ok(())).unwrap();
        assert!(ack.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn channel_persistence_acks_strip_error_when_actor_gone() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut persistence = ChannelChatPersistence::new(tx);
        let ack = persistence.replace_history_for_strip_and_ack(&[ConversationItem::system("s")]);
        assert!(
            ack.await.unwrap().is_err(),
            "dead persistence actor must ack an error, not hang or succeed"
        );
    }

    #[tokio::test]
    async fn channel_persistence_sends_flush() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        persistence.flush();
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::Flush));
    }
}
