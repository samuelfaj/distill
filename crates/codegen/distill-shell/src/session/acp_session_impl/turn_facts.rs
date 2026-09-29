// Modified for Distill by Samuel Fajreldines, 2026.
//! What the harness itself saw during a request: which tool calls failed, and
//! the build, test and lint checks the model ran.
//!
//! The per-round decisions read whether a call failed, and a goal hands the
//! recorded checks to its evaluator and verifiers, so neither has to take the
//! model's word for an outcome. Lives in the turn ledger, so it starts empty
//! with every user prompt.

use distill_tool_types::{TaskOutputOutput, TaskOutputResult};
use distill_tools::types::output::ToolOutput;

/// Characters of a check's command kept as its identity.
const COMMAND_CHARS: usize = 200;
/// Tool results remembered per request.
const MAX_EVENTS: usize = 400;
/// Characters of a check's output kept as evidence.
const TEST_EXCERPT_CHARS: usize = 2_000;
const MAX_CHECKS: usize = 20;

/// One build, test or lint the model ran.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TestEvidence {
    pub(crate) command: String,
    pub(crate) command_hash: String,
    output_hash: String,
    pub(crate) cwd: String,
    pub(crate) failed: bool,
    pub(crate) excerpt: String,
    /// When the harness saw the command finish; the goal verifier compares it
    /// with the delivered files' modification times.
    #[serde(skip)]
    pub(crate) finished_at: std::time::SystemTime,
}

/// Everything the harness saw of the current request.
#[derive(Debug, Default)]
pub(crate) struct TurnFacts {
    /// `(call id, failed)` of each finished call, oldest first.
    events: Vec<(String, bool)>,
    checks: Vec<TestEvidence>,
}

impl TurnFacts {
    /// Records what one finished tool call adds to the request.
    pub(crate) fn note_tool_result(&mut self, call_id: &str, output: &ToolOutput) {
        if self.events.len() == MAX_EVENTS {
            self.events.remove(0);
        }
        self.events.push((
            call_id.to_owned(),
            output.is_error()
                || matches!(output, ToolOutput::Bash(bash) if bash.timed_out || bash.signal.is_some()),
        ));
        match output {
            ToolOutput::Bash(bash) if looks_like_check_command(&bash.command) => {
                let failed = bash.exit_code != 0 || bash.timed_out || bash.signal.is_some();
                self.note_check(
                    &bash.command,
                    bash.current_dir.clone(),
                    failed,
                    &bash.output_for_prompt,
                );
            }
            // A check run in the background reports its outcome when its task
            // output is read after it finishes.
            ToolOutput::TaskOutput(TaskOutputOutput::Result(task)) => self.note_task_check(task),
            ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => {
                for task in &multi.results {
                    self.note_task_check(task);
                }
            }
            _ => {}
        }
    }

    fn note_task_check(&mut self, task: &TaskOutputResult) {
        if task.is_terminal() && looks_like_check_command(&task.command) {
            let failed = task.status != "completed" || task.exit_code != Some(0);
            self.note_check(&task.command, String::new(), failed, &task.output);
        }
    }

    fn note_check(&mut self, command: &str, cwd: String, failed: bool, output: &str) {
        let check = TestEvidence {
            command: command.chars().take(COMMAND_CHARS).collect(),
            command_hash: blake3::hash(command.as_bytes()).to_hex().to_string(),
            output_hash: check_output_hash(output, failed),
            cwd,
            failed,
            excerpt: tail_chars(output, TEST_EXCERPT_CHARS),
            finished_at: std::time::SystemTime::now(),
        };
        if let Some(previous) = self.checks.iter_mut().find(|previous| {
            previous.command_hash == check.command_hash
                && previous.cwd == check.cwd
                && previous.failed == check.failed
                && (!check.failed || previous.output_hash == check.output_hash)
        }) {
            *previous = check;
        } else {
            if self.checks.len() == MAX_CHECKS {
                self.checks.remove(0);
            }
            self.checks.push(check);
        }
    }

    /// Whether the call with `call_id` failed; `None` when it is not recorded.
    pub(crate) fn tool_failed(&self, call_id: &str) -> Option<bool> {
        self.events
            .iter()
            .rev()
            .find(|(id, _)| !call_id.is_empty() && id == call_id)
            .map(|(_, failed)| *failed)
    }

    /// The build, test and lint outcomes this request recorded.
    pub(crate) fn recorded_checks(&self) -> &[TestEvidence] {
        &self.checks
    }
}

/// Whether a shell command checks the work: a test run, a build, a type check
/// or a lint.
pub(crate) fn looks_like_check_command(command: &str) -> bool {
    const CHECK_TOOLS: &[&str] = &[
        "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "ctest", "tox", "nox", "tsc",
        "eslint", "ruff", "mypy", "pyright", "clippy",
    ];
    // These check only through one subcommand: `playwright install` or
    // `cypress open` do not.
    const CHECK_SUBCOMMANDS: &[(&str, &str)] = &[
        ("playwright", "test"),
        ("cypress", "run"),
        ("detox", "test"),
        ("maestro", "test"),
    ];
    // `xcodebuild -list` or `-showBuildSettings` name no action.
    const XCODEBUILD_ACTIONS: &[&str] =
        &["build", "test", "build-for-testing", "test-without-building", "analyze"];
    const RUNNERS: &[&str] = &[
        "cargo", "npm", "pnpm", "yarn", "bun", "go", "make", "mix", "dotnet", "deno", "swift",
        "flutter", "gradle", "./gradlew", "mvn", "uv", "poetry",
    ];
    const VERBS: &[&str] = &[
        "test", "tests", "check", "build", "lint", "typecheck", "vet", "clippy", "nextest",
    ];
    // A package script such as `e2e`, `test:e2e` or `ios:build`.
    let is_check_script =
        |word: &str| word.split(':').any(|part| part == "e2e" || VERBS.contains(&part));
    let lowered = command.to_ascii_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .collect();
    words.iter().enumerate().any(|(index, word)| {
        let name = word.rsplit('/').next().unwrap_or(word);
        if CHECK_TOOLS.contains(&name) || name == "unittest" {
            return true;
        }
        if let Some((_, subcommand)) = CHECK_SUBCOMMANDS.iter().find(|(tool, _)| *tool == name) {
            return words.get(index + 1) == Some(subcommand);
        }
        if name == "xcodebuild" {
            return words
                .get(index + 1..)
                .unwrap_or_default()
                .iter()
                .any(|arg| XCODEBUILD_ACTIONS.contains(arg));
        }
        if !RUNNERS.contains(word) {
            return false;
        }
        // `npm run test`, `uv run pytest`: the verb may sit after `run`.
        words
            .get(index + 1..(index + 3).min(words.len()))
            .unwrap_or_default()
            .iter()
            .any(|next| is_check_script(next) || CHECK_TOOLS.contains(next))
    })
}

fn check_output_hash(output: &str, failed: bool) -> String {
    if failed {
        return blake3::hash(output.as_bytes()).to_hex().to_string();
    }
    let mut hash = blake3::Hasher::new();
    for line in output.lines() {
        // Cargo/pytest append execution time as "in 0.12s". Keep every
        // other byte, including evidence outside the displayed tail.
        let stable = line.rsplit_once(" in ").filter(|(_, duration)| {
            duration.strip_suffix('s').is_some_and(|seconds| {
                seconds.parse::<f64>().is_ok_and(|value| value.is_finite() && value >= 0.0)
            })
        }).map_or(line, |(prefix, _)| prefix);
        hash.update(stable.as_bytes());
        hash.update(b"\n");
    }
    hash.finalize().to_hex().to_string()
}

/// The last `limit` characters of `text`: a check's verdict is at its end.
fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_check(command: &str, timed_out: bool, output: &str) -> ToolOutput {
        use distill_tools::types::output::BashOutput;
        ToolOutput::Bash(BashOutput {
            output: Vec::new(),
            output_for_prompt: output.to_owned(),
            exit_code: 0,
            command: command.to_owned(),
            truncated: false,
            signal: None,
            timed_out,
            description: None,
            current_dir: "/tmp".to_owned(),
            output_file: String::new(),
            total_bytes: 0,
            output_delta: None,
            was_bare_echo: false,
        })
    }

    /// The verifier must see a check that failed even after a later one
    /// passed; rerunning the same passing check replaces it instead of
    /// crowding the record.
    #[test]
    fn earlier_failed_checks_stay_recorded_after_a_later_pass() {
        let mut facts = TurnFacts::default();
        facts.note_tool_result("", &bash_check("cargo test parser", true, "timed out"));
        facts.note_tool_result("", &bash_check("cargo test lexer", false, "ok in 0.10s"));
        facts.note_tool_result("", &bash_check("cargo test lexer", false, "ok in 0.20s"));
        let checks = facts.recorded_checks();
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(checks[0].command, "cargo test parser");
        assert!(checks[0].failed);
        assert_eq!(checks[1].command, "cargo test lexer");
        assert!(!checks[1].failed);
        assert_eq!(checks[1].excerpt, "ok in 0.20s");
    }

    /// A long request keeps the newest calls: a failure after hundreds of
    /// reads is still known to the decisions that ask about it.
    #[test]
    fn long_requests_keep_the_newest_failures_after_eviction() {
        let mut facts = TurnFacts::default();
        for i in 0..MAX_EVENTS + 5 {
            facts.note_tool_result(&i.to_string(), &ToolOutput::Text("ok".into()));
        }
        facts.note_tool_result(
            "failed-1",
            &ToolOutput::ReadFile(distill_tools::types::output::ReadFileOutput::FileReadError(
                "missing".into(),
            )),
        );
        assert_eq!(facts.events.len(), MAX_EVENTS);
        assert_eq!(facts.tool_failed("failed-1"), Some(true));
        assert_eq!(facts.tool_failed("402"), Some(false));
        assert_eq!(facts.tool_failed("0"), None, "evicted");
        assert_eq!(facts.tool_failed(""), None);
    }

    /// Only commands that check the work count as test evidence.
    #[test]
    fn check_commands_are_tests_builds_type_checks_and_lints() {
        for command in [
            "cargo test -p distill-shell",
            "npm run test",
            "pnpm build",
            "uv run pytest -q",
            "python -m pytest",
            "npx tsc --noEmit",
            "go vet ./...",
            "cd api && make test",
            "python -m unittest",
            // Browser, device and native runs are the evidence reviewers kept
            // asking for; missing them made every delivery look unchecked.
            "npx playwright test e2e/web/onboarding.spec.ts",
            "pnpm exec playwright test",
            "npm run e2e",
            "npm run test:e2e",
            "yarn e2e",
            "npx detox test -c ios.sim.debug",
            "maestro test flows/onboarding.yaml",
            "npx cypress run",
            "xcodebuild -scheme App -destination 'platform=macOS' build",
            "xcodebuild test -scheme App",
        ] {
            assert!(looks_like_check_command(command), "{command}");
        }
        for command in [
            "ls -la",
            "cat Cargo.toml",
            "git status",
            "test -f a.txt",
            "npm install",
            "npx playwright install chromium",
            "npx cypress open",
            "npm run dev",
            "npx expo start --web",
            "xcodebuild -list",
        ] {
            assert!(!looks_like_check_command(command), "{command}");
        }
    }

    /// A suite started in the background is evidence once it finishes: its
    /// result arrives through the task output, not a shell result.
    #[test]
    fn a_finished_background_check_is_recorded_but_a_running_one_is_not() {
        let task = |status: &str, exit_code| {
            ToolOutput::TaskOutput(TaskOutputOutput::Result(TaskOutputResult {
                task_id: "bg-1".to_owned(),
                command: "npx playwright test e2e/web/zz-goal-proof.spec.ts".to_owned(),
                status: status.to_owned(),
                exit_code,
                output: "1 passed (14.1s)".to_owned(),
                ..Default::default()
            }))
        };
        let mut facts = TurnFacts::default();
        facts.note_tool_result("", &task("running", None));
        assert!(facts.recorded_checks().is_empty());
        facts.note_tool_result("", &task("completed", Some(0)));
        let checks = facts.recorded_checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].command, "npx playwright test e2e/web/zz-goal-proof.spec.ts");
        assert!(!checks[0].failed);
        facts.note_tool_result("", &task("completed", Some(1)));
        assert!(facts.recorded_checks().last().is_some_and(|check| check.failed));
    }
}
