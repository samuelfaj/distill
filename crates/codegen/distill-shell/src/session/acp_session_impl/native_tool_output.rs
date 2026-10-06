// Modified for Distill by Samuel Fajreldines, 2026.
//! Local command-output filters; the original must be stored before replacement.

use super::SessionActor;
use crate::jev::TestFailures;
use distill_tool_types::TaskOutputOutput;
use distill_tools::types::output::{BashOutput, ReadFileOutput, ToolOutput};
use distill_workspace::jev::{crushers, reduce, retention};
use std::collections::BTreeSet;

const MIN_BYTES: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandFilter {
    Cargo,
    JsTest,
    PythonTest,
    GoTest,
    GitStatus,
}

impl CommandFilter {
    fn name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::JsTest => "js-test",
            Self::PythonTest => "python-test",
            Self::GoTest => "go-test",
            Self::GitStatus => "git-status",
        }
    }

    fn for_command(command: &str) -> Option<Self> {
        // One direct invocation: mixed stdout cannot be attributed to one runner.
        if command.chars().any(shell_meta) {
            return None;
        }
        let mut words = command.split_whitespace();
        let program = words.next()?.trim_matches(['\'', '"']);
        let program = program.rsplit('/').next()?;
        let mut action = words.next().unwrap_or_default();
        if program == "cargo" && action.starts_with('+') {
            action = words.next()?;
        }
        if std::iter::once(action).chain(words.clone()).any(|arg| {
            let arg = arg.trim_matches(['\'', '"']);
            matches!(arg, "--json" | "-json") || arg.starts_with("--message-format")
        }) {
            return None;
        }
        if program == "git" {
            loop {
                match action {
                    "-C" | "-c" => {
                        words.next()?;
                        action = words.next()?;
                    }
                    "--no-pager" => action = words.next()?,
                    _ => break,
                }
            }
            if words
                .clone()
                .any(|arg| matches!(arg, "-s" | "--short" | "-z") || arg.starts_with("--porcelain"))
            {
                return None;
            }
        }
        match (program, action) {
            ("cargo", "test" | "build" | "check" | "clippy") => Some(Self::Cargo),
            ("bun" | "npm" | "pnpm" | "yarn", "test") => Some(Self::JsTest),
            ("bun" | "npm" | "pnpm" | "yarn", "run") if words.next() == Some("test") => {
                Some(Self::JsTest)
            }
            ("jest" | "vitest", _) => Some(Self::JsTest),
            ("npx", "jest" | "vitest") => Some(Self::JsTest),
            ("pytest", _) => Some(Self::PythonTest),
            ("python" | "python3", "-m") if words.next() == Some("pytest") => {
                Some(Self::PythonTest)
            }
            ("go", "test") => Some(Self::GoTest),
            ("git", "status") => Some(Self::GitStatus),
            _ => None,
        }
    }

    fn passing_line(self, line: &str) -> bool {
        match self {
            Self::Cargo => line.starts_with("test ") && line.ends_with(" ... ok"),
            Self::JsTest => {
                line.starts_with("(pass) ")
                    || line.starts_with("PASS ")
                    || line.starts_with("✓ ")
                    || line.starts_with("✔ ")
            }
            Self::PythonTest => {
                line.contains("::") && line.split_whitespace().any(|word| word == "PASSED")
            }
            Self::GoTest => line.starts_with("ok\t") || line.starts_with("ok "),
            Self::GitStatus => false,
        }
    }
}

fn shell_meta(ch: char) -> bool {
    matches!(
        ch,
        ';' | '|' | '&' | '$' | '`' | '(' | ')' | '\n' | '\r' | '<' | '>'
    )
}

/// One shell word with nothing to expand: bare, or quoted with no quote inside.
fn plain_word(word: &str) -> bool {
    let quoted = ['\'', '"']
        .into_iter()
        .find_map(|quote| word.strip_prefix(quote)?.strip_suffix(quote));
    let inner = quoted.unwrap_or(word);
    !inner.is_empty()
        && inner.chars().all(|ch| {
            !shell_meta(ch)
                && !matches!(ch, '\'' | '"' | '\\')
                && (quoted.is_some() || !ch.is_whitespace())
        })
}

/// `tail -N` / `head -N`, also spelled `-n N` and `-nN`.
fn line_window(stage: &str) -> bool {
    let mut words = stage.split_whitespace();
    let (Some("tail" | "head"), Some(flag)) = (words.next(), words.next()) else {
        return false;
    };
    let count = match flag.strip_prefix("-n") {
        Some("") => words.next(),
        Some(count) => Some(count),
        None => flag.strip_prefix('-'),
    };
    count.is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
        && words.next().is_none()
}

/// The runner invocation inside the wrapping agents add to almost every run:
/// one leading `cd <dir> &&`, one trailing `| tail -N` / `| head -N`, and a
/// trailing `2>&1`. Returns the directory ("" for none) and the invocation.
/// Any other shape comes back whole, so a real pipeline or chain still
/// refuses in [`CommandFilter::for_command`].
fn unwrap_command(command: &str) -> (&str, &str) {
    let mut invocation = command.trim();
    let mut dir = "";
    if let Some((target, rest)) = invocation
        .strip_prefix("cd ")
        .and_then(|rest| rest.split_once("&&"))
        && plain_word(target.trim())
    {
        dir = target.trim();
        invocation = rest.trim_start();
    }
    if let Some((head, stage)) = invocation.rsplit_once('|')
        && !head.ends_with('|')
        && line_window(stage)
    {
        invocation = head.trim_end();
    }
    if let Some(head) = invocation.strip_suffix(" 2>&1") {
        invocation = head.trim_end();
    }
    (dir, invocation)
}

/// Cargo's per-test failure blocks (`---- name stdout ----` up to the next
/// block, the closing `failures:` list or the result line), by name with a
/// hash of the block. A block the previous filtered run of the same command
/// showed with the same text becomes one line naming it; a new failure, a
/// changed message and every line outside a block stay verbatim.
fn collapse_unchanged_failures(
    text: &str,
    previous: &TestFailures,
) -> (String, TestFailures, Vec<String>) {
    struct Collapse<'a> {
        previous: &'a TestFailures,
        kept: String,
        failures: TestFailures,
        unchanged: Vec<String>,
        note_at: Option<usize>,
    }
    impl Collapse<'_> {
        fn finish(&mut self, block: Option<(String, String)>) {
            let Some((name, body)) = block else {
                return;
            };
            let hash = reduce::content_hash(&body);
            if self.previous.get(&name) == Some(&hash) {
                self.note_at.get_or_insert(self.kept.len());
                self.unchanged.push(name.clone());
            } else {
                self.kept.push_str(&body);
            }
            self.failures.insert(name, hash);
        }
    }
    let mut state = Collapse {
        previous,
        kept: String::with_capacity(text.len()),
        failures: TestFailures::new(),
        unchanged: Vec::new(),
        note_at: None,
    };
    let mut block: Option<(String, String)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let header = trimmed
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" ----"));
        if header.is_some() || trimmed == "failures:" || trimmed.starts_with("test result:") {
            state.finish(block.take());
        }
        if let Some(name) = header {
            let name = name.strip_suffix(" stdout").unwrap_or(name);
            block = Some((name.to_owned(), line.to_owned()));
        } else if let Some((_, body)) = block.as_mut() {
            body.push_str(line);
        } else {
            state.kept.push_str(line);
        }
    }
    state.finish(block.take());
    if let Some(at) = state.note_at {
        let note = format!(
            "[still failing with the same output as the previous run of this command: {}]\n",
            state.unchanged.join(", ")
        );
        state.kept.insert_str(at, &note);
    }
    (state.kept, state.failures, state.unchanged)
}

/// The rerun baseline's key: directory and bare invocation.
fn baseline_key(dir: &str, invocation: &str) -> String {
    format!("{dir}\n{invocation}")
}

/// The baseline key of a finished run of a Cargo command, compressed or not.
/// A running task's poll is skipped: its finished result settles the baseline.
fn cargo_baseline_key(output: &ToolOutput) -> Option<String> {
    let command = match output {
        ToolOutput::Bash(bash) => bash.command.as_str(),
        ToolOutput::TaskOutput(TaskOutputOutput::Result(result))
            if matches!(result.status.as_str(), "completed" | "failed") =>
        {
            result.command.as_str()
        }
        _ => return None,
    };
    let (dir, invocation) = unwrap_command(command);
    (CommandFilter::for_command(invocation)? == CommandFilter::Cargo)
        .then(|| baseline_key(dir, invocation))
}

/// A finished Cargo run that reaches the model verbatim (truncated, timed
/// out, not shorter, not storable, ...) replaces what the model last saw of
/// that command, so the baseline from an earlier filtered run goes.
fn forget_rerun_baseline(output: &ToolOutput) {
    if let Some(key) = cargo_baseline_key(output) {
        crate::jev::forget_test_failures_of(&key);
    }
}

struct CompressedOutput {
    text: String,
    filter: &'static str,
    /// The baseline key: directory and bare invocation.
    command: String,
    /// Cargo test failure blocks of this run, the next rerun's baseline.
    failures: Option<TestFailures>,
    /// Failures shown as unchanged instead of verbatim.
    unchanged: usize,
}

fn compress_tool_output(
    output: &ToolOutput,
    text: &str,
    store: &crate::jev_lanes::Store,
    previous_failures: &dyn Fn(&str) -> TestFailures,
) -> Option<CompressedOutput> {
    if text.len() < MIN_BYTES || text.contains("<system-reminder>") {
        return None;
    }
    let (command, source, prompt_source) = match output {
        ToolOutput::Bash(bash) if !bash.truncated && !bash.timed_out && bash.signal.is_none() => {
            let raw = String::from_utf8_lossy(&bash.output);
            (
                bash.command.as_str(),
                crushers::strip_ansi(&raw).unwrap_or_else(|| raw.to_string()),
                BashOutput::make_output_for_prompt(&raw),
            )
        }
        ToolOutput::TaskOutput(TaskOutputOutput::Result(result))
            if matches!(result.status.as_str(), "completed" | "failed")
                && result.exit_code.is_some()
                && !result.truncated =>
        {
            (
                result.command.as_str(),
                result.output.clone(),
                result.output.clone(),
            )
        }
        _ => return None,
    };
    let (dir, invocation) = unwrap_command(command);
    let filter = CommandFilter::for_command(invocation)?;
    if source.is_empty()
        || crushers::is_exact_output("run_terminal_command", invocation)
        || retention::looks_structured(command, &source)
        || crushers::secret_presence(text).is_some()
        || prompt_source.is_empty()
        || text.match_indices(&prompt_source).count() != 1
    {
        return None;
    }
    let important = crate::jev_lanes::required_tool_evidence(&source);
    let important: BTreeSet<&str> = important.iter().map(String::as_str).collect();
    let mut selected = String::with_capacity(source.len());
    let mut omitted = 0;
    let mut diagnostics = false;
    for original in source.split_inclusive('\n') {
        let line = original.trim();
        diagnostics |= line == "failures:"
            || line.starts_with("FAIL ")
            || line.starts_with("(fail) ")
            || line.starts_with("--- FAIL:")
            || (line.starts_with('=') && (line.contains("FAILURES") || line.contains("ERRORS")));
        if diagnostics || important.contains(line) {
            selected.push_str(original);
        } else if filter.passing_line(line)
            || (filter == CommandFilter::Cargo
                && ["Compiling ", "Checking ", "Downloading ", "Downloaded "]
                    .iter()
                    .any(|prefix| line.starts_with(prefix)))
            || (filter == CommandFilter::GitStatus && line.starts_with("(use \"git "))
        {
            omitted += 1;
        } else if filter == CommandFilter::GitStatus {
            let entry = ["modified:", "new file:", "deleted:", "renamed:"]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix).map(|path| (*prefix, path)));
            if let Some((state, path)) = entry {
                selected.push_str(state);
                selected.push(' ');
                selected.push_str(path.trim_start());
                if original.ends_with('\n') {
                    selected.push('\n');
                }
            } else {
                selected.push_str(original);
            }
        } else {
            selected.push_str(original);
        }
    }
    let key = baseline_key(dir, invocation);
    let mut failures = None;
    let mut unchanged = 0;
    if filter == CommandFilter::Cargo {
        let (kept, current, collapsed) =
            collapse_unchanged_failures(&selected, &previous_failures(&key));
        selected = kept;
        failures = Some(current);
        unchanged = collapsed.len();
    }
    let selected = if matches!(output, ToolOutput::Bash(_)) {
        BashOutput::make_output_for_prompt(&selected)
    } else {
        selected
    };
    if selected.len() >= prompt_source.len() {
        return None;
    }
    let handle = store(text)?;
    let replacement = format!(
        "{}\n[native {}: {omitted} routine lines omitted; full output stored at {handle}]",
        selected.trim_end(),
        filter.name()
    );
    let reduced = text.replacen(&prompt_source, &replacement, 1);
    (reduced.len() < text.len()
        && crushers::estimate_tokens(&reduced) < crushers::estimate_tokens(text))
    .then_some(CompressedOutput {
        text: reduced,
        filter: filter.name(),
        command: key,
        failures,
        unchanged,
    })
}

/// Counts in usage.json how often a failing terminal output's first cited
/// `file:line` sites (at most three, what an autoquote would quote) are read
/// afterwards, so an autoquote can be judged before it adds bytes to every
/// failing run. Nothing the model sees changes.
pub(super) fn measure_error_site_reads(output: &ToolOutput, text: &str) {
    let failing = match output {
        ToolOutput::Bash(bash) => bash.exit_code != 0,
        ToolOutput::TaskOutput(TaskOutputOutput::Result(result)) => {
            result.exit_code.is_some_and(|code| code != 0)
        }
        ToolOutput::ReadFile(ReadFileOutput::FileContent(file)) => {
            let start = file.offset.unwrap_or(1);
            let covered = crate::jev::take_error_site(|path, line| {
                file.absolute_path.ends_with(path.trim_start_matches("./"))
                    && line + 1 >= start
                    && file.limit.is_none_or(|limit| line <= start + limit)
            });
            if covered {
                crate::jev_cheap::record_utility_outcome("error_site", "read-after-cite", 0, 0, 0);
            }
            return;
        }
        _ => return,
    };
    if !failing {
        return;
    }
    let sites: Vec<_> = reduce::error_site_refs(text).into_iter().take(3).collect();
    if !sites.is_empty() {
        crate::jev_cheap::record_utility_outcome("error_site", "cited", sites.len(), text.len(), 0);
    }
    crate::jev::note_error_sites(sites);
}

impl SessionActor {
    pub(super) async fn native_compress_tool_output(
        &self,
        output: &ToolOutput,
        text: &str,
    ) -> Option<String> {
        if matches!(output, ToolOutput::TaskOutput(_))
            && !self.task_output_is_compression_source(output).await
        {
            forget_rerun_baseline(output);
            return None;
        }
        let Some(compressed) = compress_tool_output(
            output,
            text,
            &|payload| {
                crate::jev_store::store_payload(payload).map(|path| path.display().to_string())
            },
            &crate::jev::previous_test_failures,
        ) else {
            forget_rerun_baseline(output);
            return None;
        };
        if let Some(failures) = compressed.failures {
            crate::jev::note_test_failures(&compressed.command, failures);
        }
        if compressed.unchanged > 0 {
            crate::jev_cheap::record_utility_outcome(
                "test_rerun",
                "collapse:unchanged",
                compressed.unchanged,
                text.len(),
                compressed.text.len(),
            );
        }
        tracing::info!(
            session_id = %self.session_info.id,
            filter = compressed.filter,
            input_bytes = text.len(),
            output_bytes = compressed.text.len(),
            estimated_input_tokens = crushers::estimate_tokens(text),
            estimated_output_tokens = crushers::estimate_tokens(&compressed.text),
            "native command output compressed",
        );
        Some(compressed.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_baseline(_: &str) -> TestFailures {
        TestFailures::new()
    }

    fn bash(command: &str, body: &str, exit_code: i32) -> ToolOutput {
        serde_json::from_value(serde_json::json!({
            "type": "Bash", "output": body.as_bytes(),
            "output_for_prompt": BashOutput::make_output_for_prompt(body), "command": command,
            "exit_code": exit_code, "truncated": false, "timed_out": false,
            "current_dir": "/tmp", "output_file": "", "total_bytes": body.len()
        }))
        .expect("typed shell output")
    }

    fn passing_tests() -> String {
        (0..80)
            .map(|index| format!("test suite::case_{index} ... ok\n"))
            .collect()
    }

    #[test]
    fn cargo_output_is_reduced_with_exact_recovery_and_status() {
        let body = format!(
            "running 80 tests\n{}test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            passing_tests()
        );
        let text = format!("exit: 0\n{body}");
        let dir = tempfile::tempdir().expect("store");
        let path = dir.path().to_owned();
        let store = move |payload: &str| {
            crate::jev_store::store_payload_in(&path, payload)
                .map(|path| path.display().to_string())
        };
        let reduced = compress_tool_output(&bash("cargo test --lib", &body, 0), &text, &store, &no_baseline)
            .expect("native compression of passing test noise");
        assert!(reduced.text.len() < text.len() / 2);
        assert!(reduced.text.starts_with("exit: 0\n"));
        assert!(reduced.text.contains("80 passed; 0 failed; 0 ignored"));
        let archive = std::fs::read_dir(dir.path())
            .expect("archive")
            .next()
            .expect("stored file")
            .expect("entry")
            .path();
        assert_eq!(std::fs::read_to_string(&archive).expect("recover"), text);
        assert!(reduced.text.contains(&archive.display().to_string()));

        let task = ToolOutput::TaskOutput(TaskOutputOutput::Result(
            distill_tool_types::TaskOutputResult {
                task_id: "native-task".to_owned(),
                command: "cargo test --lib".to_owned(),
                status: "completed".to_owned(),
                exit_code: Some(0),
                output: body,
                ..Default::default()
            },
        ));
        let rendered = task.to_prompt_format();
        let reduced = compress_tool_output(&task, &rendered, &store, &no_baseline)
            .expect("single completed task body is filterable");
        for metadata in [
            "native-task",
            "cargo test --lib",
            "completed",
            "Exit Code: 0",
        ] {
            assert!(reduced.text.contains(metadata));
        }
    }

    #[test]
    fn failed_tests_keep_diagnostics_skips_and_nonzero_exit() {
        let failure = "test suite::broken ... FAILED\n\nfailures:\nthread 'broken' panicked at src/lib.rs:42:5:\nassertion failed: left == right\n  left: 1\n right: 2\ntest diagnostic::captured_stdout ... ok\n";
        let body = format!(
            "{}{}test suite::later ... ignored\ntest result: FAILED. 80 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out\n",
            passing_tests(),
            failure
        );
        let text = format!("exit: 101\n{body}");
        let store = |_: &str| Some("/tmp/native-output.txt".to_owned());
        let reduced = compress_tool_output(&bash("cargo test", &body, 101), &text, &store, &no_baseline)
            .expect("only passing records are collapsed");
        assert!(reduced.text.starts_with("exit: 101\n"));
        assert!(reduced.text.contains(failure));
        assert!(reduced.text.contains("test suite::later ... ignored"));
        assert!(reduced.text.contains("80 passed; 1 failed; 1 ignored"));
        assert!(compress_tool_output(&bash("cargo test", &body, 101), &text, &|_| None, &no_baseline)
            .is_none());
    }

    #[test]
    fn other_command_families_keep_summaries_and_paths() {
        let cases = [
            (
                "bun test",
                "(pass) suite",
                "40 pass\n0 fail\nRan 40 tests\n",
            ),
            (
                "python3 -m pytest -v",
                "tests.py::case PASSED",
                "40 passed, 1 skipped\n",
            ),
            ("go test ./...", "ok\texample/package", "PASS\n"),
            (
                "git status",
                "\tmodified:   src/file.rs",
                "On branch main\n",
            ),
        ];
        for (command, record, summary) in cases {
            let records = format!("{record}\n").repeat(40);
            let mut body = if command == "git status" {
                format!("{summary}{records}")
            } else {
                format!("{records}{summary}")
            };
            if command == "git status" {
                // Padding alone saves less than the recovery footer costs.
                let text = BashOutput::make_output_for_prompt(&body);
                assert!(
                    compress_tool_output(
                        &bash(command, &body, 0),
                        &text,
                        &|_| Some("/tmp/native-output.txt".to_owned()),
                        &no_baseline,
                    )
                    .is_none()
                );
                body.push_str("  (use \"git add <file>...\" to update what will be committed)\n");
            }
            let text = BashOutput::make_output_for_prompt(&body);
            let reduced = compress_tool_output(
                &bash(command, &body, 0),
                &text,
                &|_| Some("/tmp/native-output.txt".to_owned()),
                &no_baseline,
            )
            .unwrap_or_else(|| panic!("expected reduction for {command}"));
            assert!(reduced.text.len() < body.len());
            assert!(reduced.text.contains(summary.trim_end()));
            if command == "git status" {
                assert_eq!(reduced.text.matches("modified: src/file.rs").count(), 40);
            }
        }
    }

    #[test]
    fn exact_mixed_and_incomplete_results_are_not_archived() {
        let body = passing_tests();
        let store = |_: &str| panic!("excluded output must not be archived");
        for command in [
            "cat results.txt",
            "cargo test && cat results.txt",
            "cargo test --message-format=json",
        ] {
            assert!(compress_tool_output(&bash(command, &body, 0), &body, &store, &no_baseline).is_none());
        }
        let mut output = bash("cargo test", &body, 0);
        let ToolOutput::Bash(ref mut result) = output else {
            unreachable!()
        };
        result.truncated = true;
        assert!(compress_tool_output(&output, &body, &store, &no_baseline).is_none());
        let text = format!("{body}<system-reminder>preserve notice</system-reminder>");
        assert!(compress_tool_output(&bash("cargo test", &body, 0), &text, &store, &no_baseline)
            .is_none());
        let json = serde_json::json!({"output": body}).to_string();
        assert!(compress_tool_output(&bash("cargo test", &json, 0), &json, &store, &no_baseline)
            .is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bridge_delivers_reduced_model_text_and_original_client_output() {
        use super::super::{
            BridgeToolSuccess, DrainedToolSuccess, SessionEvent, SessionNotification,
        };
        use agent_client_protocol as acp;
        use distill_sampling_types::ConversationItem;
        use distill_tools::types::output::ToolRunResult;

        tokio::task::LocalSet::new().run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (actor, mut events) = super::super::support::create_test_actor_ex(
                0, 256_000, 85, gateway_tx, persistence_tx,
            ).await;
            let body = format!("{}test result: ok. 80 passed; 0 failed; 0 ignored\n", passing_tests());
            let output = bash("cargo test --lib", &body, 0);
            let original = output.to_prompt_format();
            let archive = crate::jev_store::store_dir().join(format!(
                "{}.txt", distill_workspace::jev::reduce::content_hash(&original)
            ));
            let existed = archive.is_file();
            actor.handle_bridge_tool_success(BridgeToolSuccess {
                tool_call_id: &acp::ToolCallId::new("native-filter"),
                call_id: "native-filter",
                requested_tool_name: "run_terminal_command",
                effective_tool_name: "run_terminal_command",
                drained: DrainedToolSuccess::new(ToolRunResult {
                    output, prompt_text: original.clone(), effective_tool_name: None,
                }),
                concatenated_json_count: 0,
                model_id: "test-model",
                tool_parsed_args: &serde_json::json!({"command": "cargo test --lib"}),
                model_output_override: None,
            }).await.expect("finished tool result");
            let conversation = actor.chat_state_handle.get_conversation().await;
            let ConversationItem::ToolResult(result) = conversation.last().expect("model context") else {
                panic!("expected tool result")
            };
            assert!(result.content.len() < original.len() / 2);
            assert!(result.content.contains("80 passed; 0 failed; 0 ignored"));
            assert!(result.content.contains(&archive.display().to_string()));
            let recovered = std::fs::read_to_string(&archive).expect("raw recovery");
            if !existed { std::fs::remove_file(&archive).expect("clean test archive"); }
            assert_eq!(recovered, original);
            let mut client_output = String::new();
            while let Ok(event) = events.try_recv() {
                if let SessionEvent::Notification(SessionNotification::Acp(notification)) = event {
                    client_output.push_str(&serde_json::to_string(&notification.update).expect("client update"));
                }
            }
            assert!(client_output.contains("test suite::case_79 ... ok"));
            assert!(!client_output.contains("routine lines omitted"));
            let unregistered_task = ToolOutput::TaskOutput(TaskOutputOutput::Result(
                distill_tool_types::TaskOutputResult {
                    task_id: "unregistered-native-task".to_owned(),
                    command: "cargo test --lib".to_owned(),
                    status: "completed".to_owned(),
                    exit_code: Some(0),
                    output: body,
                    ..Default::default()
                },
            ));
            assert!(actor.native_compress_tool_output(&unregistered_task, &unregistered_task.to_prompt_format()).await.is_none());
            println!("native bridge: {} -> {} bytes; estimates {} -> {} tokens; original client output and archive verified",
                original.len(), result.content.len(), crushers::estimate_tokens(&original), crushers::estimate_tokens(&result.content));
        }).await;
    }

    #[test]
    fn wrapped_runs_get_the_filter_and_real_pipelines_still_refuse() {
        // `cd X && cargo test 2>&1 | tail -50` is how agents run tests; the
        // filter was refusing nearly every one of them.
        for (command, dir, invocation) in [
            ("cd crates/x && cargo test 2>&1 | tail -50", "crates/x", "cargo test"),
            ("cd \"/a b\" && cargo test --lib | head -n 80", "\"/a b\"", "cargo test --lib"),
            ("cargo test 2>&1", "", "cargo test"),
            ("cargo test | tail -n40", "", "cargo test"),
        ] {
            assert_eq!(unwrap_command(command), (dir, invocation), "{command}");
        }
        let body = format!("{}test result: ok. 80 passed; 0 failed; 0 ignored\n", passing_tests());
        let text = BashOutput::make_output_for_prompt(&body);
        let store = |_: &str| Some("/tmp/native-output.txt".to_owned());
        let reduced = compress_tool_output(
            &bash("cd crates/x && cargo test --lib 2>&1 | tail -100", &body, 0),
            &text,
            &store,
            &no_baseline,
        )
        .expect("a wrapped cargo run is still one runner's output");
        assert!(reduced.text.len() < text.len() / 2);
        assert!(reduced.text.contains("80 passed; 0 failed"));
        assert_eq!(reduced.command, "crates/x\ncargo test --lib");

        let refuse = |_: &str| panic!("a mixed output must not be archived");
        for command in [
            "cargo test | grep FAIL",
            "cargo test 2>&1 | tail -50 | grep x",
            "cargo test | tail -f",
            "cd a && cargo test && cat out.txt",
            "cd $HOME && cargo test",
            "cd a; cargo test",
            "cargo test || tail -5",
        ] {
            assert!(
                compress_tool_output(&bash(command, &body, 0), &text, &refuse, &no_baseline)
                    .is_none(),
                "{command}"
            );
        }
    }

    fn cargo_failure(name: &str, message: &str) -> String {
        format!(
            "---- {name} stdout ----\nthread '{name}' panicked at src/lib.rs:42:5:\n{message}\n\n"
        )
    }

    fn red_run(blocks: &[String], names: &[&str]) -> String {
        let listed: String = names.iter().map(|name| format!("    {name}\n")).collect();
        format!(
            "{}failures:\n\n{}failures:\n{listed}\ntest result: FAILED. 80 passed; {} failed\n",
            passing_tests(),
            blocks.concat(),
            names.len()
        )
    }

    #[test]
    fn a_rerun_folds_only_failures_whose_text_did_not_change() {
        // Fix loops rerun the same red suite many times; a failure the model
        // already read verbatim costs the same tokens again on every rerun.
        let command = "cd crates/x && cargo test 2>&1 | tail -200";
        let stale = cargo_failure("suite::stale", "assertion failed: left == right");
        let first = red_run(
            &[stale.clone(), cargo_failure("suite::moving", "expected 1, got 2")],
            &["suite::stale", "suite::moving"],
        );
        let archive = tempfile::tempdir().expect("store");
        let path = archive.path().to_owned();
        let store = move |payload: &str| {
            crate::jev_store::store_payload_in(&path, payload)
                .map(|path| path.display().to_string())
        };
        let text = BashOutput::make_output_for_prompt(&first);
        let run = compress_tool_output(&bash(command, &first, 101), &text, &store, &no_baseline)
            .expect("first red run");
        assert!(run.text.contains(&stale), "nothing to compare with: verbatim");
        let baseline = run.failures.expect("cargo runs leave a baseline");
        assert_eq!(baseline.len(), 2);

        let second = red_run(
            &[
                stale.clone(),
                cargo_failure("suite::moving", "expected 1, got 3"),
                cargo_failure("suite::fresh", "index out of bounds"),
            ],
            &["suite::stale", "suite::moving", "suite::fresh"],
        );
        let text = BashOutput::make_output_for_prompt(&second);
        let seen = |key: &str| {
            assert_eq!(key, "crates/x\ncargo test");
            baseline.clone()
        };
        let rerun = compress_tool_output(&bash(command, &second, 101), &text, &store, &seen)
            .expect("rerun");
        assert!(!rerun.text.contains("assertion failed: left == right"));
        assert!(rerun.text.contains(
            "[still failing with the same output as the previous run of this command: suite::stale]"
        ));
        assert!(rerun.text.contains("expected 1, got 3"), "a changed message stays");
        assert!(rerun.text.contains("index out of bounds"), "a new failure stays");
        assert!(rerun.text.contains("    suite::stale\n"), "the failures list stays whole");
        assert_eq!(rerun.unchanged, 1);
        // The folded block is still recoverable from the stored original.
        let stored = std::fs::read_dir(archive.path())
            .expect("archive")
            .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path()).ok())
            .find(|stored| stored.contains("suite::fresh"))
            .expect("rerun original stored");
        assert_eq!(stored, text);
        assert!(stored.contains(&stale));
    }

    #[test]
    fn a_compaction_or_eviction_forgets_the_rerun_baseline() {
        // After a rewrite the verbatim failure a fold points at may be gone,
        // so the next run must show every failure again.
        let failures = TestFailures::from([("suite::stale".to_owned(), "hash".to_owned())]);
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("rt");
        runtime.block_on(crate::jev::with_session_scope("native-baseline-test", async {
            crate::jev::note_test_failures("\ncargo test", failures.clone());
            assert_eq!(crate::jev::previous_test_failures("\ncargo test"), failures);
            crate::jev::invalidate_payload_reads_for_active_session();
            assert!(crate::jev::previous_test_failures("\ncargo test").is_empty());
            crate::jev::note_test_failures("\ncargo test", failures.clone());
            crate::jev::invalidate_payload_reads_for_session("native-baseline-test");
            assert!(crate::jev::previous_test_failures("\ncargo test").is_empty());
        }));
    }

    #[test]
    fn a_run_that_reaches_the_model_verbatim_replaces_the_baseline() {
        // Run 1 is filtered, run 2 goes out verbatim with a changed message,
        // run 3 prints run 1's text again: folding run 3 against run 1 would
        // tell the model "unchanged" about text it last saw differently.
        let failures = TestFailures::from([("suite::stale".to_owned(), "hash".to_owned())]);
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("rt");
        runtime.block_on(crate::jev::with_session_scope("native-verbatim-baseline", async {
            let key = "crates/x\ncargo test";
            crate::jev::note_test_failures(key, failures.clone());
            let mut truncated = bash("cd crates/x && cargo test 2>&1 | tail -80", "red", 101);
            let ToolOutput::Bash(ref mut result) = truncated else {
                unreachable!()
            };
            result.truncated = true;
            assert_eq!(cargo_baseline_key(&truncated).as_deref(), Some(key));
            forget_rerun_baseline(&bash("git status", "clean", 0));
            forget_rerun_baseline(&bash("cd crates/y && cargo test", "red", 101));
            assert_eq!(
                crate::jev::previous_test_failures(key),
                failures,
                "another command keeps this baseline"
            );
            forget_rerun_baseline(&truncated);
            assert!(crate::jev::previous_test_failures(key).is_empty());
        }));
    }

    #[test]
    fn error_sites_are_counted_without_changing_any_output() {
        // Measurement for an autoquote: did the model read a site the failing
        // run cited? The site is consumed once.
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("rt");
        runtime.block_on(crate::jev::with_session_scope("native-error-site-test", async {
            let failing = "thread 'suite::broken' panicked at src/lib.rs:42:5:\nboom\n";
            measure_error_site_reads(&bash("cargo test", failing, 101), failing);
            measure_error_site_reads(&bash("cargo test", "all good\n", 0), "all good\n");
            let read = |offset: Option<usize>| {
                ToolOutput::ReadFile(ReadFileOutput::FileContent(
                    distill_tools::types::output::FileContent {
                        content: String::new(),
                        content_concise: None,
                        absolute_path: "/repo/src/lib.rs".into(),
                        offset,
                        limit: Some(20),
                        raw_output: String::new(),
                        total_lines: 400,
                        extracted_images: Vec::new(),
                    },
                ))
            };
            measure_error_site_reads(&read(Some(300)), "");
            assert!(
                crate::jev::take_error_site(|path, line| path == "src/lib.rs" && line == 42),
                "a read elsewhere in the file does not count, and a green run keeps the sites"
            );
            measure_error_site_reads(&bash("cargo test", failing, 101), failing);
            measure_error_site_reads(&read(Some(30)), "");
            assert!(!crate::jev::take_error_site(|_, _| true), "the covering read consumed it");
        }));
    }
}
