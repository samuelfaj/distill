// Modified for Distill by Samuel Fajreldines, 2026.
//! `ask_stored_output`: the main model asks the utility model about a payload the
//! harness stored (see [`crate::jev_store`]) instead of re-reading the whole file.
//!
//! The utility model only picks line ids (`select_units`, as tool-result
//! selection does); the harness copies the picked lines verbatim with their line
//! numbers, so an answer cannot hold text the file does not. Whenever the
//! utility cannot answer, the model is told to read the file directly, with the
//! lines matching the question's literal terms when it names any.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use distill_workspace::jev::flags::JevLever;

use crate::utility_select::ChunkAnswer;

const MAX_CHUNKS: usize = 8;
const SOURCE_KIND: &str = "stored_output";
/// At most this many matching lines, and bytes, in a fallback excerpt.
const EXCERPT_LINES: usize = 40;
const EXCERPT_BYTES: usize = 8 * 1024;
/// At most this many bytes of picked lines in an answer: a pick of most of a
/// large file stops here with a pointer to the rest, so the answer never
/// brings the whole file back into history.
const ANSWER_BYTES: usize = 16 * 1024;
/// A line longer than this is not answered by line: a footer does not offer
/// `ask_stored_output` for an output holding one (a minified JSON line).
const LINE_ANSWER_BYTES: usize = 4 * 1024;

/// Whether `ask_stored_output` can answer about `text` by picking lines: no
/// line is too long to be a narrow answer, or to fit one chunk.
pub(crate) fn answers_by_line(text: &str) -> bool {
    text.lines().all(|line| line.len() <= LINE_ANSWER_BYTES)
}

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

/// Whether `path` names a stored original `ask_stored_output` accepts: a file
/// in the store or a session terminal log.
pub(crate) fn is_stored_original(path: &str) -> bool {
    resolve_in_store(&crate::jev_store::store_dir(), path).is_ok()
        || resolve_terminal_log(&sessions_dir(), path).is_some()
}

/// Whether a command or a path argument names a stored original (`~/` paths
/// included), so a recovery read of it is not narrowed a second time.
pub(crate) fn mentions_stored_original(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '=' | '<' | '>' | '|' | ';' | '(' | ')')
    })
    .filter(|token| token.contains("/jev/store/") || token.contains("/terminal/"))
    .any(|token| match token.strip_prefix("~/") {
        Some(rest) => distill_dirs::home_dir()
            .is_some_and(|home| is_stored_original(&home.join(rest).display().to_string())),
        None => is_stored_original(token),
    })
}

/// Terms of `question` the harness can match without a model: quoted spans and
/// code-shaped tokens (paths, `snake_case`, `a::b`, `x.y`, digits mixed in).
/// Plain words are left out; they would match most lines.
fn literal_terms(question: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for quote in ['`', '"'] {
        let parts: Vec<&str> = question.split(quote).collect();
        // Odd parts sit between a pair of quotes; an unpaired last one does not.
        let pairs = (parts.len() - 1) / 2;
        for part in parts.iter().skip(1).step_by(2).take(pairs) {
            let part = part.trim();
            if part.len() >= 2 && !terms.iter().any(|term| term == part) {
                terms.push(part.to_owned());
            }
        }
    }
    for token in question.split_whitespace() {
        let token = token
            .trim_matches(|c: char| !c.is_alphanumeric() && !matches!(c, '_' | '/' | '~'));
        let code_shaped = token.len() >= 3
            && token.chars().any(char::is_alphanumeric)
            && (token.contains(['_', ':', '/', '.', '-', '('])
                || token.chars().any(|c| c.is_ascii_digit())
                || token.chars().skip(1).any(char::is_uppercase));
        if code_shaped && !terms.iter().any(|term| term.contains(token)) {
            terms.push(token.to_owned());
        }
    }
    terms.truncate(8);
    terms
}

/// The lines of `text` holding any of `terms`, verbatim with their line
/// numbers (`grep_handle`), as the excerpt a fallback answer carries. `None`
/// when there are no terms or nothing matches.
fn literal_excerpt(text: &str, terms: &[String]) -> Option<String> {
    let mut hits = std::collections::BTreeMap::new();
    for term in terms {
        hits.extend(distill_workspace::jev::crushers::grep_handle(text, term));
    }
    if hits.is_empty() {
        return None;
    }
    let names: Vec<String> = terms.iter().map(|term| format!("`{term}`")).collect();
    let mut excerpt = format!(
        "Lines containing {} (exact match by the harness):\n",
        names.join(", ")
    );
    let mut shown = 0;
    for (number, line) in &hits {
        let rendered = format!("{number}→{line}\n");
        if shown == EXCERPT_LINES || excerpt.len() + rendered.len() > EXCERPT_BYTES {
            break;
        }
        excerpt.push_str(&rendered);
        shown += 1;
    }
    if shown < hits.len() {
        excerpt.push_str(&format!(
            "[… {} more matching lines; grep the path for the rest]\n",
            hits.len() - shown
        ));
    }
    Some(excerpt)
}

/// The picked units, verbatim with their file line numbers; a jump between
/// picks is marked so two runs never read as one. Past [`ANSWER_BYTES`] the
/// rest of the picks are named by where they start instead.
fn render_picked(units: &[(usize, &str)], picked: &BTreeSet<usize>) -> String {
    let mut rendered = String::new();
    let mut previous: Option<usize> = None;
    let lines: Vec<(usize, &str)> =
        picked.iter().filter_map(|index| units.get(*index)).copied().collect();
    for (shown, &(number, line)) in lines.iter().enumerate() {
        let gap = if previous.is_some_and(|previous| number > previous + 1) { "…\n" } else { "" };
        let next = format!("{gap}{number}→{line}\n");
        if rendered.len() + next.len() > ANSWER_BYTES {
            rendered.push_str(&format!(
                "[… {} more picked lines from line {number}; read them with read_file offset/limit …]\n",
                lines.len() - shown
            ));
            break;
        }
        rendered.push_str(&next);
        previous = Some(number);
    }
    rendered
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
        let terms = literal_terms(question);
        let unavailable = || {
            let message = format!(
                "Utility model unavailable or gave no answer for {display}; read the file directly with read_file (offset/limit) or grep."
            );
            match literal_excerpt(&text, &terms) {
                Some(excerpt) => format!("{message}\n{excerpt}"),
                None => message,
            }
        };
        let record = |decision: &str, chunks: usize, bytes_out: usize| {
            crate::jev_cheap::record_utility_outcome(
                SOURCE_KIND,
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
        // One unit per non-blank line, carrying its line number in the file.
        let lines: Vec<(usize, &str)> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| (index + 1, line))
            .collect();
        let ask = format!(
            "The payload is a stored tool output. Pick the units that answer this question: {question}"
        );
        // A chunk and the question must fit the lane's input bound together.
        let cap = lane
            .max_payload_bytes()
            .min(lane.max_input_bytes().saturating_sub(ask.len().saturating_add(512)));
        let plan = |units: &[(usize, &str)]| {
            let texts: Vec<String> = units.iter().map(|(_, line)| (*line).to_owned()).collect();
            crate::utility_select::plan_chunks(&texts, cap, MAX_CHUNKS)
        };
        let (units, chunks, narrowed) = match plan(&lines) {
            Ok(chunks) => (lines, chunks, false),
            Err(reason) => {
                // Too large to check whole: the lines naming the question's
                // literal terms, when those fit.
                let named: Vec<(usize, &str)> = lines
                    .iter()
                    .copied()
                    .filter(|(_, line)| terms.iter().any(|term| line.contains(term.as_str())))
                    .collect();
                match plan(&named) {
                    Ok(chunks) if !named.is_empty() => (named, chunks, true),
                    _ if reason == "defer:too-many-chunks" => {
                        record(reason, 0, 0);
                        let message = format!(
                            "{display} is too large for ask_stored_output (over {MAX_CHUNKS} chunks); use read_file with offset/limit or grep on the path."
                        );
                        return match literal_excerpt(&text, &terms) {
                            Some(excerpt) => Ok(format!("{message}\n{excerpt}")),
                            None => Err(ToolError::invalid_arguments(message)),
                        };
                    }
                    _ => {
                        let message = unavailable();
                        record(reason, 0, message.len());
                        return Ok(message);
                    }
                }
            }
        };
        let answers = futures::future::join_all(chunks.iter().map(|chunk| {
            let refs: Vec<&str> = units[chunk.clone()].iter().map(|(_, line)| *line).collect();
            let payload = distill_workspace::jev::tasks::render_units(&refs, chunk.start + 1);
            let valid = chunk.start + 1..=chunk.end;
            let (lane, ask, units) = (&lane, &ask, &units);
            async move {
                let outcome = lane
                    .run_task_with_acceptance(
                        JevLever::ECheapCompress,
                        distill_workspace::jev::tasks::SELECT_UNITS_TASK,
                        &payload,
                        ask,
                        SOURCE_KIND,
                        false,
                        // The post-review reads the lines the harness would copy.
                        |answer| {
                            if answer.trim().eq_ignore_ascii_case("none") {
                                return Some(String::new());
                            }
                            let ids =
                                distill_workspace::jev::tasks::parse_unit_ids(answer, valid.clone())
                                    .ok()?;
                            Some(render_picked(units, &ids.into_iter().map(|id| id - 1).collect()))
                        },
                    )
                    .await;
                match outcome {
                    Some(outcome) if outcome.text.trim().eq_ignore_ascii_case("none") => {
                        ChunkAnswer::Nothing
                    }
                    Some(outcome) => {
                        distill_workspace::jev::tasks::parse_unit_ids(&outcome.text, valid)
                            .map(ChunkAnswer::Ids)
                            .unwrap_or(ChunkAnswer::Failed)
                    }
                    None => ChunkAnswer::Failed,
                }
            }
        }))
        .await;
        let mut picked = BTreeSet::new();
        let mut unchecked = Vec::new();
        for (chunk, answer) in chunks.iter().zip(&answers) {
            match answer {
                ChunkAnswer::Ids(ids) => picked.extend(ids.iter().map(|id| id - 1)),
                ChunkAnswer::Nothing => {}
                ChunkAnswer::Failed => unchecked.push(chunk.clone()),
            }
        }
        if unchecked.len() == chunks.len() {
            let message = unavailable();
            record("defer:all-chunks-failed", chunks.len(), message.len());
            return Ok(message);
        }
        let checked = units.len() - unchecked.iter().map(|chunk| chunk.len()).sum::<usize>();
        let mut answer = if picked.is_empty() {
            format!(
                "{display}: the utility model found no line that answers the question ({checked} lines checked); for exact text use read_file (offset/limit) or grep.\n"
            )
        } else {
            format!(
                "{display}: {} of {checked} lines picked by the utility model, copied verbatim with their line numbers:\n{}",
                picked.len(),
                render_picked(&units, &picked)
            )
        };
        if narrowed {
            answer.push_str(
                "[too large to check whole: only lines containing the question's literal terms were checked]\n",
            );
        }
        for chunk in &unchecked {
            answer.push_str(&format!(
                "[lines {}-{} were not checked (utility call failed); read them with read_file offset/limit]\n",
                units[chunk.start].0,
                units[chunk.end - 1].0
            ));
        }
        let decision = if picked.is_empty() { "nothing" } else { "answered" };
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

    /// The fallback greps only for what the question spells out literally;
    /// plain words would match most lines and bury the evidence.
    #[test]
    fn literal_terms_keep_code_shaped_tokens_and_skip_plain_words() {
        assert!(literal_terms("what failed in this run?").is_empty());
        assert_eq!(
            literal_terms("why did `cargo build` fail in src/main.rs for parse_unit_ids?"),
            ["cargo build", "src/main.rs", "parse_unit_ids"]
        );
        assert_eq!(literal_terms("where is MAX_CHUNKS set, and \"E0308\"?"), ["E0308", "MAX_CHUNKS"]);
        assert_eq!(literal_terms("an unpaired `quote stays out"), Vec::<String>::new());
    }

    /// The answer is the file's own lines with the numbers read_file would
    /// show, so the main model can quote or edit from it; a jump is marked.
    #[test]
    fn picked_lines_are_copied_verbatim_with_their_file_line_numbers() {
        let units = [(1, "fn a() {"), (2, "  \"quoted\" `tick`"), (5, "}")];
        let picked = [1, 2].into_iter().collect();
        assert_eq!(render_picked(&units, &picked), "2→  \"quoted\" `tick`\n…\n5→}\n");
    }

    /// The tool exists so the main model does not re-read a whole stored
    /// output: a utility that picks every line of a large file still returns a
    /// bounded answer that says where the rest starts.
    #[test]
    fn a_pick_of_every_line_stays_bounded() {
        let text: String = (1..=5_000).map(|n| format!("row {n} compiled crate ok\n")).collect();
        let units: Vec<(usize, &str)> = text.lines().enumerate().map(|(i, line)| (i + 1, line)).collect();
        let picked: BTreeSet<usize> = (0..units.len()).collect();
        let rendered = render_picked(&units, &picked);
        assert!(rendered.len() < ANSWER_BYTES + 200, "{}", rendered.len());
        assert!(rendered.starts_with("1→row 1 compiled crate ok\n"));
        let marker = rendered.lines().last().expect("marker");
        assert!(marker.starts_with("[… ") && marker.contains("more picked lines from line "), "{marker}");
    }

    /// A footer offers the tool only for an output it can answer by line: a
    /// minified JSON line could only be picked whole or not at all.
    #[test]
    fn only_line_shaped_outputs_are_answered_by_line() {
        assert!(answers_by_line("error: x\n  --> src/a.rs:3\n"));
        assert!(!answers_by_line(&format!("{{\"items\":[{}]}}", "{\"id\":1},".repeat(1_000))));
    }

    /// A fallback excerpt is exact (copied, never generated) and bounded, so
    /// a broad term cannot turn the guidance into a whole re-read.
    #[test]
    fn a_fallback_excerpt_is_exact_and_bounded() {
        let text: String = (1..=100).map(|n| format!("row {n} status=ok\n")).collect();
        let excerpt = literal_excerpt(&text, &["status=ok".to_owned()]).expect("matches");
        assert!(excerpt.contains("1→row 1 status=ok\n"));
        assert!(excerpt.contains(&format!("{EXCERPT_LINES}→row {EXCERPT_LINES} status=ok\n")));
        assert!(!excerpt.contains(&format!("{}→", EXCERPT_LINES + 1)));
        assert!(excerpt.contains("[… 60 more matching lines"));
        assert!(literal_excerpt(&text, &["missing".to_owned()]).is_none());
        assert!(literal_excerpt(&text, &[]).is_none());
    }

    /// A read of a stored original is the model asking for the full text, so
    /// it must be recognised (and left alone); an ordinary file is not one.
    #[test]
    fn recovery_reads_of_stored_originals_are_recognised() {
        let path = crate::jev_store::store_payload("stored original for recognition\n")
            .expect("store test payload");
        let shown = path.display().to_string();
        assert!(is_stored_original(&shown));
        assert!(mentions_stored_original(&format!("cat '{shown}' | head -50")));
        assert!(mentions_stored_original(&format!("sed -n 1,20p {shown}")));
        let workspace = tempfile::NamedTempFile::new().expect("workspace file");
        let workspace = workspace.path().display().to_string();
        assert!(!is_stored_original(&workspace));
        assert!(!mentions_stored_original(&format!("cat {workspace}")));
        assert!(!mentions_stored_original("cat /no/such/jev/store/x.txt"));
        let _ = std::fs::remove_file(&path);
    }

    fn utility_answer(content: &str) -> distill_test_support::ScriptedResponse {
        distill_test_support::ScriptedResponse::json(
            200,
            serde_json::json!({
                "id": "stored-output-ask",
                "model": "aux-model",
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": content}
                }],
                "usage": {"prompt_tokens": 17, "completion_tokens": 3}
            }),
        )
    }

    /// An asker whose utility lane is `aux-model` at `base_url`, with the
    /// selection lever on and the post-review approving.
    fn asker_with_utility(base_url: String, session: &str) -> ShellStoredOutputAsker {
        crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
        crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
            model: Some("aux-model".to_owned()),
            effort: Some("none".to_owned()),
            ..Default::default()
        });
        crate::jev::set_test_worker_model(None);
        crate::jev::set_test_decision_answers([Some(
            crate::jev_cheap::test_utility_review_answer("accept"),
        )]);
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
        entry.info.base_url = base_url;
        entry.info.context_window = std::num::NonZeroU64::new(48_000).expect("utility window");
        entry.info.api_backend = distill_sampling_types::ApiBackend::ChatCompletions;
        entry.api_key = Some("aux-key".to_owned());
        models_manager.insert_test_entry("aux-model", entry);
        ShellStoredOutputAsker {
            models_manager,
            session_id: session.to_owned(),
            usage_recorder: None,
        }
    }

    fn clear_utility() {
        crate::jev::clear_test_flags();
        crate::jev::clear_test_local_config();
        crate::jev::clear_test_worker_model();
        crate::jev::clear_test_decision_answers();
    }

    const STORED: &str = "build started\nwarning: unused import `Foo`\n\nerror[E0308]: mismatched types\n  --> src/client.rs:868\nbuild failed\n";

    /// The utility only names unit ids; the harness copies those lines from the
    /// file. A line holding quotes and backticks, which the old quoting
    /// contract could not carry, comes back byte for byte with its number.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn the_utility_picks_ids_and_the_harness_copies_the_lines() {
        use distill_test_support::{MockInferenceServer, MockModelEntry};
        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        let server = MockInferenceServer::start_with_models(vec![
            MockModelEntry::new("aux-model").with_api_backend("chat_completions"),
        ])
        .await
        .expect("start utility stub");
        // Units skip the blank line 3: U2 is line 2, U3-U4 are lines 4-5.
        server.enqueue_response("/v1/chat/completions", utility_answer("U2, U3-U4"));
        let asker = asker_with_utility(server.url(), "stored-output-pick");
        let path = crate::jev_store::store_payload(STORED).expect("store test payload");
        let path = path.display().to_string();

        let answer = distill_tools::types::resources::StoredOutputAsker::ask(
            &asker,
            &path,
            "which error broke the build?",
        )
        .await;
        clear_utility();
        let _ = std::fs::remove_file(&path);

        let answer = answer.expect("an answer");
        assert_eq!(server.request_count_for("/v1/chat/completions"), 1);
        assert!(answer.contains("3 of 5 lines picked"), "{answer}");
        assert!(answer.contains("2→warning: unused import `Foo`\n…\n4→error[E0308]: mismatched types\n5→  --> src/client.rs:868\n"), "{answer}");
        assert!(!answer.contains("build started"), "{answer}");
        assert!(!answer.contains("unavailable"), "{answer}");
    }

    /// NONE means the output holds no answer: the model is told so instead of
    /// being sent to re-read a file the utility already checked.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn a_none_answer_is_an_answer_not_a_failure() {
        use distill_test_support::{MockInferenceServer, MockModelEntry};
        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        let server = MockInferenceServer::start_with_models(vec![
            MockModelEntry::new("aux-model").with_api_backend("chat_completions"),
        ])
        .await
        .expect("start utility stub");
        server.enqueue_response("/v1/chat/completions", utility_answer("NONE"));
        let asker = asker_with_utility(server.url(), "stored-output-none");
        let path = crate::jev_store::store_payload(STORED).expect("store test payload");
        let path = path.display().to_string();

        let answer = distill_tools::types::resources::StoredOutputAsker::ask(
            &asker,
            &path,
            "did any test time out?",
        )
        .await;
        clear_utility();
        let _ = std::fs::remove_file(&path);

        let answer = answer.expect("an answer");
        assert!(answer.contains("found no line that answers"), "{answer}");
        assert!(answer.contains("5 lines checked"), "{answer}");
        assert!(!answer.contains("unavailable"), "{answer}");
    }

    /// A utility that cannot answer (here an invented id, then a dead port)
    /// leaves today's guidance: read the file directly. A question naming a
    /// literal term also gets those lines, matched exactly by the harness.
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn a_failed_utility_keeps_the_read_directly_guidance() {
        use distill_test_support::{MockInferenceServer, MockModelEntry};
        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        let server = MockInferenceServer::start_with_models(vec![
            MockModelEntry::new("aux-model").with_api_backend("chat_completions"),
        ])
        .await
        .expect("start utility stub");
        server.enqueue_response("/v1/chat/completions", utility_answer("U99"));
        let path = crate::jev_store::store_payload(STORED).expect("store test payload");
        let path = path.display().to_string();
        let shown = std::fs::canonicalize(&path).expect("stored").display().to_string();
        let ask = |asker: ShellStoredOutputAsker, question: &'static str| {
            let path = path.clone();
            async move {
                distill_tools::types::resources::StoredOutputAsker::ask(&asker, &path, question)
                    .await
                    .expect("guidance, not a tool error")
            }
        };

        let invented = ask(
            asker_with_utility(server.url(), "stored-output-invented"),
            "where is `src/client.rs` mentioned?",
        )
        .await;
        clear_utility();
        let dead = ask(
            asker_with_utility("http://127.0.0.1:9/v1".to_owned(), "stored-output-dead"),
            "what went wrong?",
        )
        .await;
        clear_utility();
        let _ = std::fs::remove_file(&path);

        let guidance = format!(
            "Utility model unavailable or gave no answer for {shown}; read the file directly with read_file (offset/limit) or grep."
        );
        assert!(
            server.request_count_for("/v1/chat/completions") >= 1,
            "the invented id came from the utility, not a missing lane"
        );
        assert!(invented.starts_with(&guidance), "{invented}");
        assert!(invented.contains("5→  --> src/client.rs:868\n"), "{invented}");
        assert!(!invented.contains("build started"), "{invented}");
        assert_eq!(dead, guidance, "no literal term: exactly today's message");
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
