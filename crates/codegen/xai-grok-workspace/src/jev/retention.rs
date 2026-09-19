//! Task-aware payload retention: **which lines still matter for the work**, asked
//! of the decision layer instead of guessed by a heuristic.
//!
//! The deterministic lanes (`crushers`, `reduce::extract_important`) decide by
//! shape: they keep failures, locations and numbers and drop repetition. They
//! cannot answer the question that actually decides whether a line is needed —
//! "does this matter for the task the session is doing?" — because that depends
//! on the turn and on what was already decided. This module asks it, one `noul`
//! per chunk, and keeps everything a reading of the answer leaves in doubt.
//!
//! Three safety rules, taken from the plugin this is modelled on and made
//! properties of the code rather than of a prompt:
//!
//! * **never touch a document.** Output the reader will parse as one thing (a
//!   file dump, a diff, JSON that parses, XML/HTML) and the output of the
//!   commands that produce documents (`cat`, `jq`, `git diff`, `base64`, …) are
//!   left exactly as they arrived: cutting a hole in a document leaves something
//!   that still looks complete.
//! * **never discard what was not fully scored.** A chunk whose answer never
//!   arrived, or arrived only for part of the state, is kept — a partially
//!   understood chunk is not a disposable one.
//! * **never lose the original silently.** The archive is written before the
//!   first question; a payload that looks secret-bearing is not archived at all,
//!   and its marker says to run the command again instead.

use std::collections::BTreeMap;

use super::crushers;
use super::error::JevError;
use super::types::{JevAnswerSet, Json, Question, QuestionId};

/// Below this estimated size there is nothing to gain: a request costs more than
/// the bytes it would remove.
///
/// The plugin this lane is modelled on uses 10 000 tokens, which is right where
/// it runs — Claude Code hands whole file dumps and build logs through. This
/// harness caps a tool result at roughly 20 KB (≈5 000 tokens) before the lanes
/// see it, so a 10 000-token gate would mean the lane never fires. Four thousand
/// is the same trade at this harness's scale: the request costs about a thousand
/// tokens, so the payload has to be able to give back several times that.
pub const MIN_TOKENS_TO_PRUNE: u64 = 4_000;
/// Lines per chunk. Small enough that the answer can be specific, large enough
/// that one request covers a screenful.
pub const CHUNK_LINES: usize = 120;
/// A payload is chunked at most this many times: beyond it the lane stands down
/// rather than paying for a hundred questions.
pub const MAX_CHUNKS: usize = 200;
/// A single line longer than this is split first, so one line cannot become an
/// untouchable chunk.
pub const MAX_LINE_CHARS: usize = 2_000;
/// A chunk whose `noul` is at least this is kept.
pub const KEEP_THRESHOLD: f64 = 0.5;
/// Estimated tokens one scoring request may carry (questions plus state).
pub const MAX_REQUEST_TOKENS: u64 = 30_000;

/// What a command's output is, which changes the guidance and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCategory {
    Build,
    Search,
    Unknown,
}

impl OutputCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Search => "search",
            Self::Unknown => "unknown",
        }
    }

    /// The extra line the state carries for a category. Guidance only: it never
    /// marks a whole command's output disposable.
    pub const fn guidance(self) -> Option<&'static str> {
        match self {
            Self::Build => Some(
                "This is build/test/install output. Progress lines, download percentages and \
                 repeated status lines are disposable; errors, warnings, failing test names, \
                 counts and the final status are not.",
            ),
            Self::Search => Some(
                "This is search output. Matched lines are the answer; ordering, progress and \
                 repeated headers are not. A path or a line number in a match is a location, and \
                 locations stay.",
            ),
            Self::Unknown => None,
        }
    }
}

/// The command with wrappers and env assignments removed, or empty when it is a
/// compound command (a pipeline can put a document on either side).
fn simple_command(command: &str) -> &str {
    if command
        .chars()
        .any(|c| matches!(c, '\r' | '\n' | '|' | ';' | '&' | '<' | '>' | '`' | '$' | '\\'))
    {
        return "";
    }
    let trimmed = command.trim();
    // Leading `VAR=x` assignments.
    let mut rest = trimmed;
    loop {
        let Some((head, tail)) = rest.split_once(char::is_whitespace) else {
            break;
        };
        let looks_like_assignment = head
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'));
        if !looks_like_assignment {
            break;
        }
        rest = tail.trim_start();
    }
    // A leading path on the command itself (`/usr/bin/cat`), never a slash that
    // belongs to an argument (`cat src/main.rs` is still `cat`).
    let (first, tail) = match rest.split_once(char::is_whitespace) {
        Some((first, tail)) => (first, tail),
        None => (rest, ""),
    };
    let program = first.rsplit('/').next().unwrap_or(first);
    if tail.trim_start().is_empty() {
        program
    } else {
        Box::leak(format!("{program} {}", tail.trim_start()).into_boxed_str())
    }
}

/// The commands whose output *is* the document the reader asked for.
const DOCUMENT_COMMANDS: [&str; 10] = [
    "cat", "bat", "jq", "yq", "diff", "base64", "openssl", "xxd", "hexdump", "pdfinfo",
];

/// Whether the output is a document or the output of a whole-document command.
pub fn looks_structured(command: &str, output: &str) -> bool {
    let head = output.trim_start();
    if head.starts_with('{') || head.starts_with('[') {
        if serde_json::from_str::<Json>(output).is_ok() {
            return true;
        }
    }
    if head.starts_with("<?xml") || head.starts_with("<!DOCTYPE") {
        return true;
    }
    if head.starts_with("<") && head.chars().nth(1).is_some_and(|c| c.is_ascii_alphabetic()) {
        return true;
    }
    if output
        .lines()
        .any(|line| line.starts_with("diff --git ") || line.starts_with("@@ "))
    {
        return true;
    }
    let simple = simple_command(command);
    if simple.is_empty() {
        // A compound command that mentions a document producer is treated the
        // same way: `find . | head` and `cmd && cat x` both end in a document.
        return command
            .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&'))
            .any(|token| DOCUMENT_COMMANDS.contains(&token.rsplit('/').next().unwrap_or(token)));
    }
    is_document_command(simple)
}

/// Whether the command *is* a document producer: a bare dumper, or git's own
/// dumpers (`git diff`, `git show`, `git cat-file`), flags ignored.
fn is_document_command(simple: &str) -> bool {
    let mut tokens = simple.split_whitespace();
    let Some(program) = tokens.next() else {
        return false;
    };
    if DOCUMENT_COMMANDS.contains(&program) {
        return true;
    }
    if program == "git" {
        return tokens
            .find(|token| !token.starts_with('-'))
            .is_some_and(|sub| matches!(sub, "diff" | "show" | "cat-file"));
    }
    false
}

/// What kind of output this is.
pub fn classify(command: &str, output: &str) -> OutputCategory {
    if looks_structured(command, output) {
        return OutputCategory::Unknown;
    }
    let simple = simple_command(command);
    let first = simple.split_whitespace().next().unwrap_or("").to_owned();
    let second = simple.split_whitespace().nth(1).unwrap_or("").to_owned();
    let is_search = matches!(
        first.as_str(),
        "rg" | "grep" | "egrep" | "fgrep" | "find" | "fd" | "head" | "tail" | "sed"
    ) || (first == "git" && second == "grep");
    if is_search {
        return OutputCategory::Search;
    }
    let is_build = matches!(
        first.as_str(),
        "make" | "gmake" | "ninja" | "pytest" | "jest" | "vitest" | "mvn" | "gradle" | "gradlew"
    ) || (matches!(first.as_str(), "npm" | "pnpm" | "yarn" | "bun")
        && matches!(
            second.as_str(),
            "build" | "test" | "lint" | "typecheck" | "check" | "install" | "ci" | "add" | "run"
        ))
        || (matches!(first.as_str(), "cargo" | "go")
            && matches!(second.as_str(), "build" | "test" | "check" | "clippy" | "install"))
        || first == "cmake";
    if is_build {
        return OutputCategory::Build;
    }
    OutputCategory::Unknown
}

/// One chunk of the payload, as it is sent and as it is kept or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: String,
    pub text: String,
    pub lines: usize,
    pub chars: usize,
    /// Position in the original payload, for the marker.
    pub index: usize,
}

/// Splits the payload into chunks of [`CHUNK_LINES`] lines, after breaking the
/// lines that are too long to be one chunk.
pub fn chunk(output: &str) -> Vec<Chunk> {
    let mut lines: Vec<String> = Vec::new();
    for line in output.split('\n') {
        if line.chars().count() <= MAX_LINE_CHARS {
            lines.push(line.to_owned());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for piece in chars.chunks(MAX_LINE_CHARS) {
            lines.push(piece.iter().collect());
        }
    }
    lines
        .chunks(CHUNK_LINES)
        .take(MAX_CHUNKS)
        .enumerate()
        .map(|(index, group)| {
            let text = group.join("\n");
            Chunk {
                id: format!("c{}", index + 1),
                lines: group.len(),
                chars: text.len(),
                text,
                index,
            }
        })
        .collect()
}

/// The one question per chunk: "does any line here still need to be available?"
pub fn retention_questions(
    chunks: &[Chunk],
    category: OutputCategory,
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = BTreeMap::new();
    for chunk in chunks {
        let guidance = category
            .guidance()
            .map(|guidance| format!(" {guidance}"))
            .unwrap_or_default();
        questions.insert(
            chunk.id.clone(),
            Question::Noul {
                instructions: format!(
                    "Chunk {} contains at least one line that should remain available to the \
                     agent for its ongoing task. Judge every line against the instructions and \
                     decisions anywhere in the state, not only against what the next reply should \
                     say.{guidance}",
                    chunk.id
                )
                .into(),
                criteria: Some(super::types::NoulCriteria {
                    is_true: Some(
                        "At least one line carries an error, a warning, a summary, a final \
                         result, or a value a standing requirement still needs. One needed line \
                         is enough even when every other line is noise. Do not assume the removed \
                         text can be recovered from an archive."
                            .into(),
                    ),
                    is_false: Some(
                        "Every line is disposable progress, repeated boilerplate or irrelevant \
                         noise, and removing the whole chunk loses no result and no \
                         task-dependent information."
                            .into(),
                    ),
                }),
            },
        );
    }
    Ok(questions)
}

/// Groups the chunks into requests that fit beside `state_tokens`, keeping the
/// order intact.
pub fn batches(chunks: &[Chunk], state_tokens: u64, category: OutputCategory) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_tokens = 0u64;
    let budget = MAX_REQUEST_TOKENS.saturating_sub(state_tokens).max(1);
    for chunk in chunks {
        let question = retention_questions(std::slice::from_ref(chunk), category)
            .map(|questions| estimate_questions(&questions))
            .unwrap_or(0);
        if !current.is_empty() && current_tokens + question > budget {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(chunk.index);
        current_tokens += question;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// The estimated size of one battery, as the endpoint counts it.
fn estimate_questions(questions: &BTreeMap<QuestionId, Question>) -> u64 {
    let rendered = serde_json::to_string(questions).unwrap_or_default();
    crushers::estimate_tokens(&rendered)
}

/// Lines that must survive regardless of any answer: a failure a reader acts on.
fn carries_failure(text: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "error", "panic", "failed", "failure", "warning", "traceback", "assert", "not found",
    ];
    let lowered = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// Which chunks stay, and why each kept one was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    /// One entry per chunk, in order.
    pub keep: Vec<bool>,
    /// The reasons, for the record (and the tests).
    pub reasons: Vec<&'static str>,
    /// Chunks that were never scored (kept by the coverage rule).
    pub unscored: Vec<String>,
    pub dropped_lines: usize,
    pub dropped_chars: usize,
}

impl Retention {
    /// Whether anything is dropped at all.
    pub fn drops_anything(&self) -> bool {
        self.keep.iter().any(|keep| !keep)
    }
}

/// Reads the answers into a keep/drop decision.
///
/// `scored[i]` says whether chunk `i`'s answer was actually obtained against the
/// whole state: a chunk that was never scored is kept, because "I could not ask"
/// and "the answer said drop" are not the same thing.
pub fn compose_retention(
    answers: Option<&JevAnswerSet>,
    chunks: &[Chunk],
    scored: &[bool],
    threshold: f64,
) -> Retention {
    let mut keep = Vec::with_capacity(chunks.len());
    let mut reasons = Vec::with_capacity(chunks.len());
    let mut unscored = Vec::new();
    let mut dropped_lines = 0usize;
    let mut dropped_chars = 0usize;
    let last = chunks.len().saturating_sub(1);

    for (index, chunk) in chunks.iter().enumerate() {
        let fully_scored = scored.get(index).copied().unwrap_or(false);
        let noul = answers.and_then(|answers| answers.noul(&chunk.id));
        let (kept, reason) = if !fully_scored {
            unscored.push(chunk.id.clone());
            (true, "not fully scored")
        } else if index == 0 || index == last {
            (true, "first or last chunk")
        } else if carries_failure(&chunk.text) {
            (true, "carries a failure")
        } else if noul.is_some_and(|value| value >= threshold) {
            (true, "the answer kept it")
        } else if noul.is_none() {
            // Scored, but with no usable answer for this chunk: same rule as an
            // unscored chunk — a missing answer never drops bytes.
            unscored.push(chunk.id.clone());
            (true, "no usable answer")
        } else {
            (false, "the answer dropped it")
        };
        if !kept {
            dropped_lines += chunk.lines;
            dropped_chars += chunk.chars;
        }
        keep.push(kept);
        reasons.push(reason);
    }

    Retention {
        keep,
        reasons,
        unscored,
        dropped_lines,
        dropped_chars,
    }
}

/// Rebuilds the payload with the dropped runs replaced by markers.
///
/// `archive` is the path the original was written to *before* the questions were
/// asked. When it is `None` — the payload looked secret-bearing, or the store
/// refused — the marker tells the reader to run the command again instead of
/// pointing at a file that does not exist.
pub fn apply(
    chunks: &[Chunk],
    retention: &Retention,
    archive: Option<&str>,
    command: &str,
) -> String {
    let mut out = String::new();
    let mut index = 0usize;
    while index < chunks.len() {
        if retention.keep[index] {
            out.push_str(&chunks[index].text);
            out.push('\n');
            index += 1;
            continue;
        }
        let start = index;
        let mut lines = 0usize;
        let mut chars = 0usize;
        while index < chunks.len() && !retention.keep[index] {
            lines += chunks[index].lines;
            chars += chunks[index].chars;
            index += 1;
        }
        let _ = start;
        match archive {
            Some(path) => out.push_str(&format!(
                "[jev retention trimmed {lines} lines ({chars} chars); full output: {path} \
                 (read or grep it if you need it)]\n"
            )),
            None => out.push_str(&format!(
                "[jev retention trimmed {lines} lines ({chars} chars); this output was not \
                 archived, so re-run `{command}` if you need it again]\n"
            )),
        }
    }
    out
}

/// Whether the lane should act at all on this payload, and why not when it
/// should not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Worth asking about: the payload is big, not a document, not secret-only.
    Prune,
    /// Below the size at which a request pays for itself.
    TooSmall { tokens: u64 },
    /// A document, or the output of a whole-document command.
    Document,
}

/// Applies the gates that come before any request.
pub fn gate(command: &str, output: &str) -> Gate {
    let tokens = crushers::estimate_tokens(output);
    if tokens <= MIN_TOKENS_TO_PRUNE {
        return Gate::TooSmall { tokens };
    }
    if looks_structured(command, output) {
        return Gate::Document;
    }
    Gate::Prune
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::types::{Answer, Usage};

    fn build_output() -> String {
        let mut out = String::new();
        for i in 0..600 {
            out.push_str(&format!(
                "   Compiling crate-{i} v0.1.0 (/Users/x/y/crate-{i})\n       Fresh (0.4s)\n"
            ));
        }
        out.push_str("error[E0308]: mismatched types\n  --> src/client.rs:868:5\n");
        out.push_str("    Finished `dev` profile in 120.5s\n");
        out
    }

    fn answers(entries: &[(&str, f64)]) -> JevAnswerSet {
        let mut map = BTreeMap::new();
        for (id, value) in entries {
            map.insert((*id).to_owned(), Answer::Noul { noul: *value });
        }
        JevAnswerSet {
            model: "typesafe/jev-1.13".to_owned(),
            answers: map,
            usage: Usage::default(),
            request_id: None,
            latency_ms: 0,
        }
    }

    #[test]
    fn a_document_or_a_small_payload_is_never_touched() {
        // The commands whose output is the answer.
        for command in [
            "cat src/main.rs",
            "/usr/bin/cat src/main.rs",
            "jq '.items[]' out.json",
            "git diff",
            "git show HEAD",
            "base64 secrets.bin",
            "openssl x509 -in cert.pem -text",
            "rg foo | jq .",
            "echo hi && cat notes.md",
            "git --no-pager diff HEAD~1",
        ] {
            assert_eq!(
                gate(command, &build_output()),
                Gate::Document,
                "{command} produces a document"
            );
        }
        // Structured payloads are documents whatever the command was.
        assert!(looks_structured("deploy", "{\"ok\": true, \"id\": 3}"));
        assert!(looks_structured("deploy", "<?xml version=\"1.0\"?><a/>"));
        assert!(looks_structured("deploy", "<html><body>x</body></html>"));
        assert!(looks_structured("deploy", "diff --git a/x b/x\n@@ -1 +1 @@\n"));
        assert_eq!(gate("cat src/main.rs", &build_output()), Gate::Document);

        // A search or a build is *not* a document: that is the case this lane is
        // for, and the one the plugin prunes too.
        for command in ["find . -name '*.rs' | head -50", "cargo build", "pytest -q"] {
            assert_eq!(gate(command, &build_output()), Gate::Prune, "{command}");
        }

        // A small payload is not worth a request.
        let small = "   Compiling one crate\n    Finished in 1s\n";
        assert_eq!(
            gate("cargo build", small),
            Gate::TooSmall {
                tokens: crushers::estimate_tokens(small)
            }
        );
        // A big build log is the case this lane exists for.
        assert_eq!(gate("cargo build", &build_output()), Gate::Prune);
    }

    #[test]
    fn chunking_splits_long_lines_and_respects_the_ceiling() {
        let mut payload = format!("{}\n", "x".repeat(MAX_LINE_CHARS + 500));
        for _ in 0..CHUNK_LINES {
            payload.push_str("short\n");
        }
        let chunks = chunk(&payload);
        assert!(chunks.len() >= 2, "the boundary is where the lines are");
        assert!(
            chunks[0].text.starts_with(&"x".repeat(MAX_LINE_CHARS)),
            "the long line was split at the ceiling"
        );
        assert!(chunks[1].text.starts_with("short"));

        // Many lines produce many chunks, and the ceiling is honoured.
        let wide = (0..CHUNK_LINES * (MAX_CHUNKS + 5))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk(&wide);
        assert_eq!(chunks.len(), MAX_CHUNKS, "the chunk ceiling holds");
        assert_eq!(chunks[0].id, "c1");
        assert_eq!(chunks[0].lines, CHUNK_LINES);
    }

    #[test]
    fn the_questions_ask_about_retention_and_never_about_the_archive() {
        let chunks = chunk(&build_output());
        let questions = retention_questions(&chunks, OutputCategory::Build).expect("battery");
        assert_eq!(questions.len(), chunks.len(), "one question per chunk");
        let Question::Noul { instructions, criteria } = questions.get("c1").expect("c1") else {
            panic!("a retention question is a noul");
        };
        let text = instructions.to_string();
        assert!(text.contains("at least one line"), "{text}");
        let criteria = criteria.as_ref().expect("criteria");
        assert!(
            criteria
                .is_true
                .as_ref()
                .expect("true")
                .to_string()
                .contains("Do not assume the removed text can be recovered"),
            "the reader is told the archive is not a guarantee"
        );
        // Guidance is per category and never turns a category into "disposable".
        let search = retention_questions(&chunks, OutputCategory::Search).expect("battery");
        let Question::Noul { instructions, .. } = search.get("c1").expect("c1") else {
            panic!("noul");
        };
        let text = instructions.to_string();
        assert!(text.contains("search output"), "{text}");
        assert!(!text.contains("disposable output"));
    }

    #[test]
    fn the_keep_rules_protect_every_case_a_reader_would_miss() {
        let mut payload = String::new();
        for i in 0..CHUNK_LINES * 5 {
            payload.push_str(&format!("progress line {i}\n"));
        }
        payload.push_str("error: the build failed at src/client.rs:868\n");
        let chunks = chunk(&payload);
        assert!(chunks.len() >= 5, "several chunks to decide between");

        // Every chunk scored, every answer "drop": the middle ones go, the first
        // and last stay, and the one carrying the error stays.
        let dropped = answers(
            &chunks
                .iter()
                .map(|chunk| (chunk.id.as_str(), 0.01))
                .collect::<Vec<_>>(),
        );
        let scored = vec![true; chunks.len()];
        let retention = compose_retention(Some(&dropped), &chunks, &scored, KEEP_THRESHOLD);
        assert!(retention.keep[0], "the first chunk stays");
        assert!(*retention.keep.last().expect("last"), "the last chunk stays");
        assert!(retention.drops_anything(), "the middle goes");
        let error_chunk = chunks
            .iter()
            .position(|chunk| chunk.text.contains("error: the build failed"))
            .expect("the error is in some chunk");
        assert!(retention.keep[error_chunk], "a chunk carrying a failure stays");

        // One chunk kept by the answer stays, with its reason recorded.
        let keep_two = answers(&[
            (chunks[2].id.as_str(), 0.9),
            (chunks[3].id.as_str(), 0.1),
        ]);
        let retention = compose_retention(Some(&keep_two), &chunks, &scored, KEEP_THRESHOLD);
        assert!(retention.keep[2]);
        assert_eq!(retention.reasons[2], "the answer kept it");

        // A chunk that was never scored is kept, whatever the answers say.
        let mut scored_gap = vec![true; chunks.len()];
        scored_gap[3] = false;
        let retention = compose_retention(Some(&dropped), &chunks, &scored_gap, KEEP_THRESHOLD);
        assert!(retention.keep[3], "an unscored chunk is never dropped");
        assert!(retention.unscored.contains(&chunks[3].id));

        // No answers at all ⇒ nothing is dropped.
        let retention = compose_retention(None, &chunks, &vec![true; chunks.len()], KEEP_THRESHOLD);
        assert!(retention.keep.iter().all(|keep| *keep));
        assert!(!retention.drops_anything());
    }

    #[test]
    fn the_marker_counts_what_went_and_points_at_the_archive_or_the_rerun() {
        let chunks = chunk(&build_output());
        let dropped = answers(
            &chunks
                .iter()
                .map(|chunk| (chunk.id.as_str(), 0.0))
                .collect::<Vec<_>>(),
        );
        let scored = vec![true; chunks.len()];
        let retention = compose_retention(Some(&dropped), &chunks, &scored, KEEP_THRESHOLD);
        assert!(retention.dropped_lines > 0);

        let with_archive = apply(&chunks, &retention, Some("/tmp/store/abc.txt"), "cargo build");
        assert!(with_archive.contains("full output: /tmp/store/abc.txt"));
        assert!(with_archive.contains("trimmed"));
        assert!(with_archive.contains("lines"));
        // The kept text is verbatim: no rewriting of what stays.
        let last = chunks.last().expect("last chunk");
        assert!(with_archive.contains(last.text.trim()));

        let without = apply(&chunks, &retention, None, "cargo build");
        assert!(without.contains("not archived"), "{without}");
        assert!(without.contains("re-run `cargo build`"));
        assert!(!without.contains(".txt"), "no phantom archive path");
    }

    #[test]
    fn requests_are_batched_under_the_ceiling_and_keep_their_order() {
        let payload = (0..CHUNK_LINES * 40)
            .map(|i| format!("line {i} with a little text to make it weigh something"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk(&payload);
        // A state that leaves room for only a couple of questions per request:
        // the lane splits rather than sending an oversized request.
        let batches = batches(&chunks, MAX_REQUEST_TOKENS - 200, OutputCategory::Unknown);
        assert!(batches.len() > 1, "a big payload needs more than one request");
        let flat: Vec<usize> = batches.iter().flatten().copied().collect();
        assert_eq!(flat, (0..chunks.len()).collect::<Vec<_>>(), "order is kept");
        assert_eq!(
            batches.iter().map(Vec::len).sum::<usize>(),
            chunks.len(),
            "every chunk is scored exactly once"
        );
    }
}
