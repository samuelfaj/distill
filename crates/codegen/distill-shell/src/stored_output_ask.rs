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
    pub(crate) session_id: String,
    /// Where the utility attempts are billed when the tool runs outside the
    /// turn's recorder scope. `None` (or a closed session) records nothing.
    pub(crate) usage_recorder: Option<distill_chat_state::WeakChatStateHandle>,
}

impl ShellStoredOutputAsker {
    /// The session's utility lane, from the same resolver as `SessionActor::cheap_lane`,
    /// so catalog and subscription utility models work here too. The session's
    /// own model and credentials come from its chat state; with the session gone,
    /// the agent's current model stands in for the same-model check.
    async fn lane(&self) -> Option<crate::jev_cheap::CheapLane> {
        let chat = self
            .usage_recorder
            .as_ref()
            .and_then(distill_chat_state::WeakChatStateHandle::upgrade);
        let (main_model, creds) = match &chat {
            Some(chat) => (
                chat.get_sampling_config().await.map(|config| config.model),
                chat.get_credentials().await,
            ),
            None => (None, distill_chat_state::Credentials::default()),
        };
        let main_model =
            main_model.unwrap_or_else(|| self.models_manager.current_model_id().0.to_string());
        let auth = self.models_manager.auth_manager();
        let session_key = auth.current_or_expired().map(|a| a.key.clone());
        let disable_api_key_auth = auth.grok_com_config().api_key_auth_disabled();
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        crate::jev_cheap::resolve_utility_lane(&self.models_manager, &[&main_model], &|slug| {
            crate::agent::config::resolve_aux_model_sampling_config(
                slug,
                &models,
                &endpoints,
                session_key.as_deref(),
                disable_api_key_auth,
                creds.alpha_test_key.clone(),
                creds.client_version.clone(),
            )
        })
    }
}

#[async_trait::async_trait]
impl distill_tools::types::resources::StoredOutputAsker for ShellStoredOutputAsker {
    async fn ask(
        &self,
        path: &str,
        question: &str,
    ) -> Result<String, distill_tool_runtime::ToolError> {
        let recorder = self
            .usage_recorder
            .as_ref()
            .and_then(distill_chat_state::WeakChatStateHandle::upgrade);
        crate::jev::with_recorder_unless_scoped(
            self.session_id.clone(),
            recorder,
            self.ask_in_scope(path, question),
        )
        .await
    }
}

impl ShellStoredOutputAsker {
    async fn ask_in_scope(
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
        let record = |decision: &str, chunks: usize, bytes_out: usize| {
            crate::jev_cheap::record_utility_outcome(
                "stored_output",
                decision,
                chunks,
                text.len(),
                bytes_out,
            );
        };
        let Some(lane) = self.lane().await else {
            let message = unavailable();
            record("keep:lane-unavailable", 0, message.len());
            return Ok(message);
        };
        // A terminal log is stored unscreened; a secret in it, or in the
        // question, never goes to the utility model.
        if [text.as_str(), question]
            .iter()
            .any(|part| distill_workspace::jev::crushers::utility_secret_presence(part).is_some())
        {
            let message = format!(
                "{display} looks secret-bearing, so it stays off the utility model; read the file directly with read_file (offset/limit) or grep."
            );
            record("keep:secret", 0, message.len());
            return Ok(message);
        }
        let Some(chunks) = chunk_lines(&text, lane.max_payload_bytes(), MAX_CHUNKS) else {
            record("defer:too-many-chunks", 0, 0);
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
        let answer = join_answers(&display, &answers);
        let decision = if answer.is_some() { "answered" } else { "nothing" };
        let answer = answer.unwrap_or_else(unavailable);
        record(decision, chunks.len(), answer.len());
        Ok(answer)
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

    /// A catalog utility model (a subscription one, say) serves `ask_stored_output`
    /// through the same resolver as the session's other utility work, instead of
    /// every call answering "unavailable"; the session's own model never does.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn a_catalog_utility_model_answers_stored_output_questions() {
        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
            model: Some("aux-model".to_owned()),
            effort: Some("none".to_owned()),
            ..Default::default()
        });
        crate::jev::set_test_worker_model(None);
        let home = tempfile::tempdir().expect("auth home");
        let models_manager = crate::agent::remote_config::ModelsManager::new(
            None,
            indexmap::IndexMap::new(),
            agent_client_protocol::ModelId::new("main-model"),
            std::sync::Arc::new(distill_login::AuthManager::new(
                home.path(),
                distill_login::GrokComConfig::default(),
            )),
            crate::agent::config::Config::default(),
        );
        let mut entry = crate::agent::config::ModelEntry::fallback(
            "aux-model",
            &crate::agent::config::EndpointsConfig::default(),
        );
        entry.info.base_url = "https://aux.example/v1".to_owned();
        entry.info.api_backend = distill_sampling_types::ApiBackend::ChatCompletions;
        entry.api_key = Some("aux-key".to_owned());
        models_manager.insert_test_entry("aux-model", entry);
        let asker = ShellStoredOutputAsker {
            models_manager: models_manager.clone(),
            session_id: "stored-output-lane".to_owned(),
            usage_recorder: None,
        };
        let lane = asker.lane().await.expect("the catalog utility model serves");
        assert_eq!(lane.model(), "aux-model");

        models_manager.set_current_model_id(agent_client_protocol::ModelId::new("aux-model"));
        assert!(
            asker.lane().await.is_none(),
            "the session's own model is not its utility; the read-directly message applies"
        );
        crate::jev::clear_test_local_config();
        crate::jev::clear_test_worker_model();
    }

    /// Terminal logs and stored outputs are not screened when written, so a
    /// secret in one must not reach the utility model through a question: the
    /// main model is told to read the file itself, as when no lane exists.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn a_secret_bearing_stored_output_stays_off_the_utility() {
        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
            model: Some("aux-model".to_owned()),
            effort: Some("none".to_owned()),
            ..Default::default()
        });
        crate::jev::set_test_worker_model(None);
        let home = tempfile::tempdir().expect("auth home");
        let models_manager = crate::agent::remote_config::ModelsManager::new(
            None,
            indexmap::IndexMap::new(),
            agent_client_protocol::ModelId::new("main-model"),
            std::sync::Arc::new(distill_login::AuthManager::new(
                home.path(),
                distill_login::GrokComConfig::default(),
            )),
            crate::agent::config::Config::default(),
        );
        let mut entry = crate::agent::config::ModelEntry::fallback(
            "aux-model",
            &crate::agent::config::EndpointsConfig::default(),
        );
        // A dispatch would fail fast here instead of answering.
        entry.info.base_url = "http://127.0.0.1:9/v1".to_owned();
        entry.info.api_backend = distill_sampling_types::ApiBackend::ChatCompletions;
        entry.api_key = Some("aux-key".to_owned());
        models_manager.insert_test_entry("aux-model", entry);
        let asker = ShellStoredOutputAsker {
            models_manager,
            session_id: "stored-output-secret".to_owned(),
            usage_recorder: None,
        };
        let key = "sk-proj-FAKEKEYabcdefghijklmnopqrstuvwxyz0123456789";
        let path = crate::jev_store::store_payload(&format!("db: ok\nOPENAI_API_KEY={key}\n"))
            .expect("store test payload");
        let path = path.display().to_string();

        let has_lane = asker.lane().await.is_some();
        let answer = distill_tools::types::resources::StoredOutputAsker::ask(
            &asker,
            &path,
            "which key is configured?",
        )
        .await;
        crate::jev::clear_test_local_config();
        crate::jev::clear_test_worker_model();
        let _ = std::fs::remove_file(&path);

        assert!(has_lane, "the lane resolves, so only the secret keeps it off");
        let answer = answer.expect("a secret is an answer, not a tool error");
        assert!(answer.contains("secret-bearing"), "{answer}");
        assert!(answer.contains("read_file"), "{answer}");
        assert!(!answer.contains(key), "{answer}");
    }
}
