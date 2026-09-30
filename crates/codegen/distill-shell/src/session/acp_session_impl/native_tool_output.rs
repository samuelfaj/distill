// Modified for Distill by Samuel Fajreldines, 2026.
//! Local command-output filters; the original must be stored before replacement.

use super::SessionActor;
use distill_tool_types::TaskOutputOutput;
use distill_tools::types::output::{BashOutput, ToolOutput};
use distill_workspace::jev::{crushers, retention};
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
        if command.chars().any(|ch| {
            matches!(
                ch,
                ';' | '|' | '&' | '$' | '`' | '(' | ')' | '\n' | '\r' | '<' | '>'
            )
        }) {
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

struct CompressedOutput {
    text: String,
    filter: &'static str,
}

fn compress_tool_output(
    output: &ToolOutput,
    text: &str,
    store: &crate::jev_lanes::Store,
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
    let filter = CommandFilter::for_command(command)?;
    if source.is_empty()
        || crushers::is_exact_output("run_terminal_command", command)
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
    })
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
            return None;
        }
        let compressed = compress_tool_output(output, text, &|payload| {
            crate::jev_store::store_payload(payload).map(|path| path.display().to_string())
        })?;
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
        let reduced = compress_tool_output(&bash("cargo test --lib", &body, 0), &text, &store)
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
        let reduced = compress_tool_output(&task, &rendered, &store)
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
        let reduced = compress_tool_output(&bash("cargo test", &body, 101), &text, &store)
            .expect("only passing records are collapsed");
        assert!(reduced.text.starts_with("exit: 101\n"));
        assert!(reduced.text.contains(failure));
        assert!(reduced.text.contains("test suite::later ... ignored"));
        assert!(reduced.text.contains("80 passed; 1 failed; 1 ignored"));
        assert!(compress_tool_output(&bash("cargo test", &body, 101), &text, &|_| None).is_none());
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
                    compress_tool_output(&bash(command, &body, 0), &text, &|_| {
                        Some("/tmp/native-output.txt".to_owned())
                    })
                    .is_none()
                );
                body.push_str("  (use \"git add <file>...\" to update what will be committed)\n");
            }
            let text = BashOutput::make_output_for_prompt(&body);
            let reduced = compress_tool_output(&bash(command, &body, 0), &text, &|_| {
                Some("/tmp/native-output.txt".to_owned())
            })
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
            assert!(compress_tool_output(&bash(command, &body, 0), &body, &store).is_none());
        }
        let mut output = bash("cargo test", &body, 0);
        let ToolOutput::Bash(ref mut result) = output else {
            unreachable!()
        };
        result.truncated = true;
        assert!(compress_tool_output(&output, &body, &store).is_none());
        let text = format!("{body}<system-reminder>preserve notice</system-reminder>");
        assert!(compress_tool_output(&bash("cargo test", &body, 0), &text, &store).is_none());
        let json = serde_json::json!({"output": body}).to_string();
        assert!(compress_tool_output(&bash("cargo test", &json, 0), &json, &store).is_none());
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
}
