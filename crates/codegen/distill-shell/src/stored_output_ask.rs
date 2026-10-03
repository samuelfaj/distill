// Modified for Distill by Samuel Fajreldines, 2026.
//! `ask_stored_output`: the main model asks the utility model about a payload the
//! harness stored (see [`crate::jev_store`]) instead of re-reading the whole file.
//!
//! Answers are verbatim spans of the stored text (`ask_handle` task, span guard).

use std::path::{Path, PathBuf};

use distill_workspace::jev::flags::JevLever;

const MAX_CHUNKS: usize = 8;
const TASK: &str = "ask_handle";

/// Canonical path of `path` when it is a file inside `store`. This is the trust
/// boundary: symlinks and `..` are resolved before the containment check.
fn resolve_in_store(store: &Path, path: &str) -> Result<PathBuf, String> {
    let store = std::fs::canonicalize(store)
        .map_err(|_| "no stored outputs exist in this installation".to_owned())?;
    let resolved = std::fs::canonicalize(path)
        .map_err(|_| format!("`{path}` is not a readable file"))?;
    if !resolved.starts_with(&store) || !resolved.is_file() {
        return Err(format!(
            "`{path}` is not a stored tool output; use read_file or grep for other files"
        ));
    }
    Ok(resolved)
}

fn sessions_dir() -> PathBuf {
    crate::util::distill_home::distill_home().join("sessions")
}

/// A truncated shell result points at its full log under
/// `<sessions>/<cwd>/<session>/terminal/<call>.log`; accept exactly that shape.
fn resolve_terminal_log(sessions: &Path, path: &str) -> Option<PathBuf> {
    let resolved = resolve_in_store(sessions, path).ok()?;
    let in_terminal_dir = resolved.parent()?.file_name()? == "terminal";
    let is_log = resolved.extension().is_some_and(|ext| ext == "log");
    (in_terminal_dir && is_log).then_some(resolved)
}

/// Splits `text` into chunks of at most `max_bytes`, cutting on line boundaries
/// (a single longer line is cut on a char boundary). `None` when more than
/// `max_chunks` are needed.
fn chunk_lines(text: &str, max_bytes: usize, max_chunks: usize) -> Option<Vec<String>> {
    let max_bytes = max_bytes.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while current.len() + rest.len() > max_bytes {
            if current.is_empty() {
                let mut cut = max_bytes.min(rest.len());
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                if cut == 0 {
                    cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
                }
                chunks.push(rest[..cut].to_owned());
                rest = &rest[cut..];
                if rest.is_empty() {
                    break;
                }
            } else {
                chunks.push(std::mem::take(&mut current));
            }
            if chunks.len() > max_chunks {
                return None;
            }
        }
        current.push_str(rest);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    (chunks.len() <= max_chunks).then_some(chunks)
}

/// Joins per-chunk answers in chunk order under the path; `None` when no chunk answered.
fn join_answers(path: &str, answers: &[Option<String>]) -> Option<String> {
    let found: Vec<&str> = answers.iter().flatten().map(String::as_str).collect();
    (!found.is_empty()).then(|| format!("{path}\n{}", found.join("\n---\n")))
}

pub(crate) struct ShellStoredOutputAsker {
    pub(crate) models_manager: crate::agent::remote_config::ModelsManager,
}

impl ShellStoredOutputAsker {
    /// ponytail: only non-catalog specs (the default OpenRouter chain) resolve here.
    /// A catalog model id needs the session's credentials (`SessionActor::cheap_lane`),
    /// which this holder does not carry; those sessions get the read-directly message.
    fn lane(&self) -> Option<crate::jev_cheap::CheapLane> {
        let spec = crate::jev_cheap::configured_model_spec();
        if crate::agent::config::find_model_by_id(&self.models_manager.models(), &spec).is_some() {
            return None;
        }
        crate::jev_cheap::CheapLane::from_spec(&spec)
    }
}

#[async_trait::async_trait]
impl distill_tools::types::resources::StoredOutputAsker for ShellStoredOutputAsker {
    async fn ask(
        &self,
        path: &str,
        question: &str,
    ) -> Result<String, distill_tool_runtime::ToolError> {
        use distill_tool_runtime::ToolError;
        let resolved = resolve_in_store(&crate::jev_store::store_dir(), path)
            .or_else(|err| resolve_terminal_log(&sessions_dir(), path).ok_or(err))
            .map_err(ToolError::invalid_arguments)?;
        let text = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| ToolError::invalid_arguments(format!("cannot read `{path}`: {e}")))?;
        let display = resolved.display().to_string();
        let unavailable = || {
            format!(
                "Utility model unavailable or gave no answer for {display}; read the file directly with read_file (offset/limit) or grep."
            )
        };
        let Some(lane) = self.lane() else {
            return Ok(unavailable());
        };
        let Some(chunks) = chunk_lines(&text, lane.max_payload_bytes(), MAX_CHUNKS) else {
            return Err(ToolError::invalid_arguments(format!(
                "{display} is too large for ask_stored_output (over {MAX_CHUNKS} chunks); use read_file with offset/limit or grep on the path."
            )));
        };
        let mut answers = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let outcome = lane
                .run_task_with_acceptance(
                    JevLever::ECheapCompress,
                    TASK,
                    chunk,
                    question,
                    "stored_output",
                    false,
                    |answer| {
                        (!answer.trim().eq_ignore_ascii_case("none")).then(|| answer.to_owned())
                    },
                )
                .await;
            answers.push(outcome.map(|o| o.text));
        }
        Ok(join_answers(&display, &answers).unwrap_or_else(unavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model-supplied path must never escape the store: outside files, `..`
    /// traversal and symlinks out of the store are all rejected.
    #[test]
    fn paths_outside_the_store_are_rejected() {
        let root = tempfile::tempdir().expect("root");
        let store = root.path().join("store");
        std::fs::create_dir(&store).expect("store");
        let inside = store.join("a.txt");
        std::fs::write(&inside, "x").expect("inside");
        let outside = root.path().join("secret.txt");
        std::fs::write(&outside, "s").expect("outside");

        assert!(resolve_in_store(&store, inside.to_str().unwrap()).is_ok());
        assert!(resolve_in_store(&store, outside.to_str().unwrap()).is_err());
        let traversal = format!("{}/../secret.txt", store.display());
        assert!(resolve_in_store(&store, &traversal).is_err());
        #[cfg(unix)]
        {
            let link = store.join("link.txt");
            std::os::unix::fs::symlink(&outside, &link).expect("symlink");
            assert!(resolve_in_store(&store, link.to_str().unwrap()).is_err());
        }
        assert!(resolve_in_store(&store, store.to_str().unwrap()).is_err());
    }

    /// Truncated shell results point at session terminal logs; only that
    /// shape under the sessions root is accepted, not other session files.
    #[test]
    fn only_terminal_logs_under_sessions_are_accepted() {
        let root = tempfile::tempdir().expect("root");
        let terminal = root.path().join("cwd").join("sid").join("terminal");
        std::fs::create_dir_all(&terminal).expect("terminal");
        let log = terminal.join("call.log");
        std::fs::write(&log, "x").expect("log");
        let other = root.path().join("cwd").join("sid").join("chat_history.jsonl");
        std::fs::write(&other, "x").expect("other");
        assert!(resolve_terminal_log(root.path(), log.to_str().unwrap()).is_some());
        assert!(resolve_terminal_log(root.path(), other.to_str().unwrap()).is_none());
    }

    /// Chunks must cut between lines and respect both byte and count caps, or the
    /// utility call would exceed its payload bound or fan out without limit.
    #[test]
    fn chunks_split_on_lines_and_cap_at_the_limit() {
        let text = "aaaa\nbbbb\ncccc\ndddd\n";
        let chunks = chunk_lines(text, 10, 8).expect("fits");
        assert_eq!(chunks, vec!["aaaa\nbbbb\n", "cccc\ndddd\n"]);
        assert_eq!(chunks.concat(), text);
        assert!(chunk_lines(text, 5, 3).is_none());
        assert_eq!(chunk_lines(text, 5, 4).expect("four").len(), 4);
        let long = chunk_lines("abcdefghij", 4, 8).expect("long line");
        assert_eq!(long, vec!["abcd", "efgh", "ij"]);
        assert_eq!(chunk_lines("ééé", 3, 8).expect("utf8").concat(), "ééé");
    }

    /// Answers keep chunk order, skip chunks that said nothing, and an
    /// all-empty result is `None` so the caller can tell the model to read the file.
    #[test]
    fn answers_join_in_chunk_order() {
        let joined = join_answers(
            "/s/x.txt",
            &[Some("first".into()), None, Some("third".into())],
        );
        assert_eq!(joined.as_deref(), Some("/s/x.txt\nfirst\n---\nthird"));
        assert!(join_answers("/s/x.txt", &[None, None]).is_none());
    }
}
