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

/// Tool results remembered per request.
const MAX_EVENTS: usize = 400;
/// Characters of a check's output kept as evidence.
const TEST_EXCERPT_CHARS: usize = 2_000;
const MAX_CHECKS: usize = 20;
/// Distinct check commands whose failures are counted per request.
const MAX_FAILURE_HISTORIES: usize = 400;
/// Budget for one working-tree marker. A check is already slow, but a large
/// or locked repository must not hold the tool result back.
const WORKTREE_MARKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// Changed paths whose size and modification time join the marker.
const WORKTREE_MARKER_PATHS_MAX: usize = 5_000;

/// What a recorded check showed about the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CheckOutcome {
    Passed,
    Failed,
    /// The command's exit code belongs to a later stage (a pipe to a filter,
    /// `|| true`, `; echo …`) and the kept output shows no runner verdict.
    Unknown,
}

/// One build, test or lint the model ran.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TestEvidence {
    /// The full command, as run.
    pub(crate) command: String,
    pub(crate) command_hash: String,
    output_hash: String,
    pub(crate) cwd: String,
    pub(crate) outcome: CheckOutcome,
    pub(crate) excerpt: String,
    /// Failed runs of this command in this directory during the request,
    /// this one included. Merging identical runs never lowers it.
    pub(crate) failures: u32,
    /// Whether this command failed again after the working tree changed
    /// since an earlier failure; `None` when a marker was missing.
    pub(crate) failed_again_after_change: Option<bool>,
    /// The harness's working-tree marker when the check finished; `None`
    /// when it could not be taken (see [`worktree_marker`]).
    pub(crate) worktree: Option<String>,
    /// When the harness saw the command finish; the goal verifier compares it
    /// with the delivered files' modification times.
    #[serde(skip)]
    pub(crate) finished_at: std::time::SystemTime,
}

/// Where a tool ran, as the harness knows it beyond the tool's own output.
#[derive(Debug, Clone, Default)]
pub(crate) struct CheckContext {
    /// The session directory; background task results do not carry one.
    pub(crate) cwd: String,
    /// See [`worktree_marker`].
    pub(crate) worktree: Option<String>,
}

impl CheckContext {
    /// The context of `output`: the working-tree marker is taken only when
    /// the output records a check, so other tools pay nothing.
    pub(crate) async fn for_output(output: &ToolOutput, root: &std::path::Path) -> Self {
        let worktree = if records_check(output) {
            worktree_marker(root).await
        } else {
            None
        };
        Self {
            cwd: root.display().to_string(),
            worktree,
        }
    }
}

/// Failures of one command in one directory, across merged records.
#[derive(Debug, Default)]
struct FailureHistory {
    count: u32,
    last_worktree: Option<String>,
    changed_between: Option<bool>,
}

/// Everything the harness saw of the current request.
#[derive(Debug, Default)]
pub(crate) struct TurnFacts {
    /// `(call id, failed)` of each finished call, oldest first.
    events: Vec<(String, bool)>,
    failed_tool_count: u64,
    checks: Vec<TestEvidence>,
    failure_histories: std::collections::HashMap<String, FailureHistory>,
}

impl TurnFacts {
    /// [`Self::note_tool_result_at`] with no harness context.
    #[cfg(test)]
    pub(crate) fn note_tool_result(&mut self, call_id: &str, output: &ToolOutput) {
        self.note_tool_result_at(call_id, output, &CheckContext::default());
    }

    /// Records what one finished tool call adds to the request.
    pub(crate) fn note_tool_result_at(
        &mut self,
        call_id: &str,
        output: &ToolOutput,
        context: &CheckContext,
    ) {
        if self.events.len() == MAX_EVENTS {
            self.events.remove(0);
        }
        let failed = output.is_error()
            || matches!(output, ToolOutput::Bash(bash) if bash.timed_out || bash.signal.is_some());
        self.failed_tool_count = self.failed_tool_count.saturating_add(u64::from(failed));
        self.events.push((call_id.to_owned(), failed));
        match output {
            ToolOutput::Bash(bash) if looks_like_check_command(&bash.command) => {
                let exit = (!bash.timed_out && bash.signal.is_none()).then_some(bash.exit_code);
                let cwd = if bash.current_dir.is_empty() {
                    context.cwd.clone()
                } else {
                    bash.current_dir.clone()
                };
                self.note_check(&bash.command, cwd, exit, &bash.output_for_prompt, context);
            }
            // A check run in the background reports its outcome when its task
            // output is read after it finishes.
            ToolOutput::TaskOutput(TaskOutputOutput::Result(task)) => {
                self.note_task_check(task, context);
            }
            ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => {
                for task in &multi.results {
                    self.note_task_check(task, context);
                }
            }
            _ => {}
        }
    }

    fn note_task_check(&mut self, task: &TaskOutputResult, context: &CheckContext) {
        if task.is_terminal() && looks_like_check_command(&task.command) {
            // Cancelled and timed-out tasks did not finish their run.
            let exit = task
                .exit_code
                .filter(|_| matches!(task.status.as_str(), "completed" | "failed"));
            self.note_check(
                &task.command,
                context.cwd.clone(),
                exit,
                &task.output,
                context,
            );
        }
    }

    /// `exit` is `None` when the run was cut short (timeout, signal, cancel).
    fn note_check(
        &mut self,
        command: &str,
        cwd: String,
        exit: Option<i32>,
        output: &str,
        context: &CheckContext,
    ) {
        let outcome = check_outcome(command, exit, output);
        let command_hash = blake3::hash(command.as_bytes()).to_hex().to_string();
        let (failures, failed_again_after_change) =
            self.count_failure(&command_hash, &cwd, outcome, context.worktree.as_deref());
        let check = TestEvidence {
            command: command.to_owned(),
            command_hash,
            output_hash: check_output_hash(output, outcome != CheckOutcome::Passed),
            cwd,
            outcome,
            excerpt: tail_chars(output, TEST_EXCERPT_CHARS),
            failures,
            failed_again_after_change,
            worktree: context.worktree.clone(),
            finished_at: std::time::SystemTime::now(),
        };
        if let Some(previous) = self.checks.iter_mut().find(|previous| {
            previous.command_hash == check.command_hash
                && previous.cwd == check.cwd
                && previous.outcome == check.outcome
                && (check.outcome == CheckOutcome::Passed
                    || previous.output_hash == check.output_hash)
        }) {
            *previous = check;
        } else {
            if self.checks.len() == MAX_CHECKS {
                self.checks.remove(0);
            }
            self.checks.push(check);
        }
    }

    /// Counts a run of a check; returns the failures of that command in that
    /// directory so far and whether it failed again after the tree changed.
    fn count_failure(
        &mut self,
        command_hash: &str,
        cwd: &str,
        outcome: CheckOutcome,
        worktree: Option<&str>,
    ) -> (u32, Option<bool>) {
        let key = format!("{command_hash}\0{cwd}");
        if self.failure_histories.len() == MAX_FAILURE_HISTORIES
            && !self.failure_histories.contains_key(&key)
        {
            return (u32::from(outcome == CheckOutcome::Failed), None);
        }
        let history = self.failure_histories.entry(key).or_default();
        if outcome == CheckOutcome::Failed {
            if history.count > 0 {
                let changed = match (history.last_worktree.as_deref(), worktree) {
                    (Some(before), Some(now)) => Some(before != now),
                    _ => None,
                };
                history.changed_between = if history.count == 1 {
                    changed
                } else {
                    match (history.changed_between, changed) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                };
            }
            history.count = history.count.saturating_add(1);
            history.last_worktree = worktree.map(str::to_owned);
        }
        let again = if history.count < 2 {
            Some(false)
        } else {
            history.changed_between
        };
        (history.count, again)
    }

    /// Whether the call with `call_id` failed; `None` when it is not recorded.
    pub(crate) fn tool_failed(&self, call_id: &str) -> Option<bool> {
        self.events
            .iter()
            .rev()
            .find(|(id, _)| !call_id.is_empty() && id == call_id)
            .map(|(_, failed)| *failed)
    }

    pub(crate) fn failed_tool_count(&self) -> u64 {
        self.failed_tool_count
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

/// Whether [`TurnFacts::note_tool_result_at`] records `output` as a check.
fn records_check(output: &ToolOutput) -> bool {
    let finished_check =
        |task: &TaskOutputResult| task.is_terminal() && looks_like_check_command(&task.command);
    match output {
        ToolOutput::Bash(bash) => looks_like_check_command(&bash.command),
        ToolOutput::TaskOutput(TaskOutputOutput::Result(task)) => finished_check(task),
        ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => {
            multi.results.iter().any(finished_check)
        }
        _ => false,
    }
}

/// A hash of the working tree under `root`: `git status` plus the size and
/// modification time of every listed path, so a second edit to an already
/// modified file still changes it. It does not depend on which tool edited.
///
/// `None` outside a Git worktree, on any git error, or past
/// [`WORKTREE_MARKER_TIMEOUT`]: an unknown change, never an error.
pub(crate) async fn worktree_marker(root: &std::path::Path) -> Option<String> {
    let capture = async {
        let mut command = tokio::process::Command::new(crate::util::subprocess::git_bin());
        // Optional locks off: a status refresh must never make the model's own
        // concurrent git command fail on `index.lock`.
        command
            .args([
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ])
            .current_dir(root)
            .kill_on_drop(true);
        let output = command.output().await.ok()?;
        if !output.status.success() {
            return None;
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&output.stdout);
        let mut entries = output.stdout.split(|byte| *byte == 0);
        let mut stated = 0;
        while let Some(entry) = entries.next() {
            // `XY path`: two status letters and a space.
            let (Some(status), Some(path)) = (entry.get(..2), entry.get(3..)) else {
                continue;
            };
            // A rename or copy is followed by its source path.
            if status.iter().any(|status| matches!(status, b'R' | b'C')) {
                entries.next();
            }
            if path.is_empty() || stated == WORKTREE_MARKER_PATHS_MAX {
                continue;
            }
            let path = root.join(String::from_utf8_lossy(path).as_ref());
            stated += 1;
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                hasher.update(&metadata.len().to_le_bytes());
                if let Some(modified) = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                {
                    hasher.update(&modified.as_nanos().to_le_bytes());
                }
            }
        }
        Some(hasher.finalize().to_hex().to_string())
    };
    tokio::time::timeout(WORKTREE_MARKER_TIMEOUT, capture)
        .await
        .ok()
        .flatten()
}

/// The outcome of a check: `exit` is `None` when the run was cut short.
///
/// A runner's failure marker in the kept output wins. Otherwise the exit code
/// decides when the check is the command's last stage. When a later stage
/// owns the exit code, a runner verdict in the output decides, and without
/// one the outcome is unknown: a pipeline's code is its last filter's, so a
/// failing suite piped to `tail` exits zero and a `grep` with no match exits
/// one.
fn check_outcome(command: &str, exit: Option<i32>, output: &str) -> CheckOutcome {
    let Some(code) = exit else {
        return CheckOutcome::Failed;
    };
    let verdict = runner_verdict(output);
    if verdict.failed {
        return CheckOutcome::Failed;
    }
    let keeps_end = match exit_scope(command) {
        ExitScope::Check if code == 0 => return CheckOutcome::Passed,
        ExitScope::Check => return CheckOutcome::Failed,
        // `check && next`: zero means the check passed; non-zero may be either.
        ExitScope::CheckOrLater if code == 0 => return CheckOutcome::Passed,
        ExitScope::CheckOrLater => true,
        ExitScope::Masked { keeps_end } => keeps_end,
    };
    if verdict.weak_failed && !verdict.passed {
        CheckOutcome::Failed
    } else if verdict.passed && keeps_end && !verdict.weak_failed {
        CheckOutcome::Passed
    } else {
        CheckOutcome::Unknown
    }
}

/// Which stage of a shell command owns its exit code, as far as the check is
/// concerned.
#[derive(Debug, PartialEq, Eq)]
enum ExitScope {
    /// The check is the last stage.
    Check,
    /// Only `&&` stages follow the check.
    CheckOrLater,
    /// A pipe, `||`, `;` or `&` follows the check. `keeps_end` is false when a
    /// filter such as `head` or `grep` may have dropped a later failure, so
    /// a success marker in what is left does not prove the run passed.
    Masked { keeps_end: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageOp {
    Pipe,
    And,
    Or,
    Then,
}

/// Quotes are not parsed: an operator inside a quoted argument only splits
/// the command further, which can only move the check away from the end and
/// make the outcome less certain, never more.
fn exit_scope(command: &str) -> ExitScope {
    let stages = shell_stages(command);
    let Some(index) = stages
        .iter()
        .rposition(|(_, stage)| looks_like_check_command(stage))
    else {
        return ExitScope::Masked { keeps_end: false };
    };
    let rest = stages.get(index + 1..).unwrap_or_default();
    if rest.is_empty() {
        return ExitScope::Check;
    }
    if rest.iter().all(|(op, _)| *op == StageOp::And) {
        return ExitScope::CheckOrLater;
    }
    // Only the pipe stages right after the check filter its output.
    let keeps_end = rest
        .iter()
        .take_while(|(op, _)| *op == StageOp::Pipe)
        .all(|(_, stage)| {
            let name = stage.split_whitespace().next().unwrap_or_default();
            matches!(name.rsplit('/').next(), Some("tail" | "tee" | "cat"))
        });
    ExitScope::Masked { keeps_end }
}

/// Splits `command` at `|`, `|&`, `&&`, `||`, `;`, `&` and newlines, keeping
/// redirections such as `2>&1`, `&>file` and `>|file` inside their stage.
/// Each stage carries the operator before it; empty stages are dropped.
fn shell_stages(command: &str) -> Vec<(StageOp, &str)> {
    let bytes = command.as_bytes();
    let mut stages = Vec::new();
    let mut op = StageOp::Then;
    let mut start = 0;
    let mut previous = None;
    let mut skip_next = false;
    for (index, &byte) in bytes.iter().enumerate() {
        let after_redirect = matches!(previous, Some(b'>' | b'<'));
        previous = Some(byte);
        // The second byte of a two-byte operator.
        if std::mem::take(&mut skip_next) {
            continue;
        }
        let next = bytes.get(index + 1).copied();
        let (next_op, width) = match byte {
            b'|' if after_redirect => (None, 1),
            b'|' if next == Some(b'|') => (Some(StageOp::Or), 2),
            b'|' if next == Some(b'&') => (Some(StageOp::Pipe), 2),
            b'|' => (Some(StageOp::Pipe), 1),
            b'&' if after_redirect || next == Some(b'>') => (None, 1),
            b'&' if next == Some(b'&') => (Some(StageOp::And), 2),
            b'&' | b';' | b'\n' => (Some(StageOp::Then), 1),
            _ => (None, 1),
        };
        if let Some(next_op) = next_op {
            stages.push((op, command.get(start..index).unwrap_or_default()));
            op = next_op;
            start = index + width;
            skip_next = width == 2;
        }
    }
    stages.push((op, command.get(start..).unwrap_or_default()));
    stages.retain(|(_, stage)| !stage.trim().is_empty());
    stages
}

/// What runner markers a check's output carries.
#[derive(Debug, Default, PartialEq, Eq)]
struct RunnerVerdict {
    /// A runner reported a failure.
    failed: bool,
    /// A bare `error:` line: a failure unless a runner also reported success.
    weak_failed: bool,
    /// A runner reported success.
    passed: bool,
}

fn runner_verdict(output: &str) -> RunnerVerdict {
    let mut verdict = RunnerVerdict::default();
    for line in output.lines() {
        match line_verdict(line) {
            Some(LineVerdict::Failed) => verdict.failed = true,
            Some(LineVerdict::WeakFailed) => verdict.weak_failed = true,
            Some(LineVerdict::Passed) => verdict.passed = true,
            None => {}
        }
    }
    verdict
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineVerdict {
    Failed,
    WeakFailed,
    Passed,
}

/// The runner marker tables. Each marker is a whole summary or status line
/// of one runner, matched strictly: a missed marker only makes an outcome
/// unknown, while a loose one would record a wrong pass or failure.
fn line_verdict(raw: &str) -> Option<LineVerdict> {
    use LineVerdict::{Failed, Passed, WeakFailed};
    let line = raw.trim();
    // cargo test and rustc.
    if line.starts_with("test result: FAILED") {
        return Some(Failed);
    }
    if line.starts_with("test result: ok") {
        return Some(Passed);
    }
    if line.starts_with("error[E") {
        return Some(Failed);
    }
    if let Some(message) = line.strip_prefix("error: ") {
        // cargo's own summary of a failed build or test run.
        const SUMMARIES: &[&str] = &["could not compile", "aborting due to", "test failed"];
        let failed_targets = message.contains(" target") && message.ends_with("failed:");
        if SUMMARIES.iter().any(|summary| message.starts_with(summary)) || failed_targets {
            return Some(Failed);
        }
        return Some(WeakFailed);
    }
    // tsc: `src/a.ts(3,5): error TS2322: …`.
    if let Some((_, after)) = line.split_once("error TS")
        && after.starts_with(|c: char| c.is_ascii_digit())
    {
        return Some(Failed);
    }
    // pytest: short summary lines and the final count line.
    if line.starts_with("FAILED ") && line.contains("::") {
        return Some(Failed);
    }
    if let Some(verdict) = pytest_summary(line) {
        return Some(verdict);
    }
    // jest (`Tests: 1 failed, 2 passed, 3 total`) and vitest
    // (`Tests  1 failed | 2 passed (3)`).
    const COUNT_LINES: &[&str] = &["Tests:", "Test Suites:", "Tests ", "Test Files "];
    if let Some(prefix) = COUNT_LINES.iter().find(|prefix| line.starts_with(**prefix)) {
        let counts = &line[prefix.len()..];
        if has_count(counts, "failed") {
            return Some(Failed);
        }
        if has_count(counts, "passed") {
            return Some(Passed);
        }
    }
    // go test: `--- FAIL: TestX`, `FAIL\tpkg\t0.1s`, `ok  \tpkg\t0.1s`.
    if line.starts_with("--- FAIL") || raw.starts_with("FAIL\t") || line == "FAIL" {
        return Some(Failed);
    }
    if (raw.starts_with("ok ") || raw.starts_with("ok\t")) && raw.contains('\t') {
        return Some(Passed);
    }
    // xcodebuild: `** BUILD FAILED **`, `** TEST SUCCEEDED **`.
    if line.starts_with("** ") && line.ends_with(" FAILED **") {
        return Some(Failed);
    }
    if line.starts_with("** ") && line.ends_with(" SUCCEEDED **") {
        return Some(Passed);
    }
    None
}

/// pytest's count line, with or without the `=` rule around it:
/// `1 failed, 2 passed in 0.12s`, `== 3 passed, 1 warning in 1.02s ==`.
fn pytest_summary(line: &str) -> Option<LineVerdict> {
    let inner = line.trim_matches(|c: char| c == '=' || c.is_whitespace());
    let (counts, _) = inner.rsplit_once(" in ")?;
    let (mut failed, mut passed) = (false, false);
    for part in counts.split(", ") {
        let (count, word) = part.split_once(' ')?;
        let count: u64 = count.parse().ok()?;
        match word {
            "failed" | "error" | "errors" => failed |= count > 0,
            "passed" => passed |= count > 0,
            "skipped" | "deselected" | "xfailed" | "xpassed" | "warning" | "warnings" | "rerun" => {
            }
            _ => return None,
        }
    }
    if failed {
        Some(LineVerdict::Failed)
    } else {
        passed.then_some(LineVerdict::Passed)
    }
}

/// Whether `counts` holds a positive `<n> <word>` count.
fn has_count(counts: &str, word: &str) -> bool {
    let tokens: Vec<&str> = counts
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '|' | '(' | ')'))
        .filter(|token| !token.is_empty())
        .collect();
    tokens.windows(2).any(|pair| {
        matches!(pair, [count, name]
            if *name == word && count.parse::<u64>().is_ok_and(|count| count > 0))
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
        bash_run(command, 0, timed_out, output)
    }

    fn bash_run(command: &str, exit_code: i32, timed_out: bool, output: &str) -> ToolOutput {
        use distill_tools::types::output::BashOutput;
        ToolOutput::Bash(BashOutput {
            output: Vec::new(),
            output_for_prompt: output.to_owned(),
            exit_code,
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
        assert_eq!(checks[0].outcome, CheckOutcome::Failed);
        assert_eq!(checks[1].command, "cargo test lexer");
        assert_eq!(checks[1].outcome, CheckOutcome::Passed);
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
        assert_eq!(checks[0].outcome, CheckOutcome::Passed);
        facts.note_tool_result("", &task("completed", Some(1)));
        assert!(
            facts
                .recorded_checks()
                .last()
                .is_some_and(|check| check.outcome == CheckOutcome::Failed)
        );
    }

    fn outcome(command: &str, exit_code: i32, output: &str) -> CheckOutcome {
        check_outcome(command, Some(exit_code), output)
    }

    /// The pipeline exits with `tail`'s zero, but the runner said FAILED: a
    /// verifier reading "passed" here would accept broken work.
    #[test]
    fn a_failing_suite_piped_to_tail_is_failed() {
        let output = "failures:\n    parser::empty\n\ntest result: FAILED. 3 passed; 1 failed; 0 ignored\n\nerror: test failed, to rerun pass `--lib`";
        assert_eq!(
            outcome("cargo test 2>&1 | tail -5", 0, output),
            CheckOutcome::Failed
        );
    }

    /// `grep` with no match exits 1 and `head` exits 0; neither says anything
    /// about the build, so neither may record a failure or a pass.
    #[test]
    fn a_filter_with_no_runner_marker_is_unknown_whatever_its_exit_code() {
        let command = "cargo build 2>&1 | grep -E '^error' | head";
        assert_eq!(outcome(command, 0, ""), CheckOutcome::Unknown);
        assert_eq!(
            outcome("cargo build 2>&1 | grep -E '^error'", 1, ""),
            CheckOutcome::Unknown
        );
        assert_eq!(
            outcome("npm test 2>&1 | tail -3", 0, "done\n"),
            CheckOutcome::Unknown
        );
        assert_eq!(
            outcome("cargo test; echo exit=$?", 0, "exit=101"),
            CheckOutcome::Unknown
        );
        assert_eq!(
            outcome("pytest -q || true", 0, "collected 0 items"),
            CheckOutcome::Unknown
        );
    }

    /// A filtered error line is still the build's failure.
    #[test]
    fn a_filtered_error_line_is_failed_unless_a_runner_also_passed() {
        let command = "cargo clippy 2>&1 | grep -E '^error' | head";
        assert_eq!(
            outcome(command, 0, "error: unused variable: `x`"),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome(
                "cargo test -- --nocapture | tail",
                0,
                "error: bad input\ntest result: ok. 2 passed"
            ),
            CheckOutcome::Unknown,
            "a test may log an error line and still pass"
        );
    }

    /// A success marker counts only when nothing after it can have been cut:
    /// `head` may hide a later failing binary.
    #[test]
    fn a_success_marker_passes_only_when_the_end_of_the_output_is_kept() {
        let output = "test result: ok. 12 passed; 0 failed";
        assert_eq!(
            outcome("cargo test 2>&1 | tail -5", 0, output),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome("cargo test 2>&1 | tee log.txt", 0, output),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome("cargo test || true", 0, output),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome("cargo test 2>&1 | head -40", 0, output),
            CheckOutcome::Unknown
        );
        assert_eq!(
            outcome("cargo test 2>&1 | grep 'test result'", 0, output),
            CheckOutcome::Unknown
        );
    }

    /// When the check is the last stage its exit code is the verdict.
    #[test]
    fn the_exit_code_decides_when_the_check_is_the_last_stage() {
        assert_eq!(
            outcome("cd api && cargo test", 101, ""),
            CheckOutcome::Failed
        );
        assert_eq!(outcome("cd api && cargo test", 0, ""), CheckOutcome::Passed);
        assert_eq!(
            outcome("cargo test > log.txt 2>&1", 0, ""),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome("cargo test &> log.txt", 1, ""),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome("cargo test && git push", 0, ""),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome("cargo test && git push", 1, ""),
            CheckOutcome::Unknown,
            "the push may be what failed"
        );
        assert_eq!(
            check_outcome("cargo test", None, ""),
            CheckOutcome::Failed,
            "cut short"
        );
    }

    #[test]
    fn shell_stages_keep_redirections_inside_their_stage() {
        let stages = shell_stages("cd a && cargo test 2>&1 |& tail -5 >| out; echo $? || true &");
        let ops: Vec<StageOp> = stages.iter().map(|(op, _)| *op).collect();
        assert_eq!(
            ops,
            [
                StageOp::Then,
                StageOp::And,
                StageOp::Pipe,
                StageOp::Then,
                StageOp::Or
            ]
        );
        assert_eq!(stages[1].1.trim(), "cargo test 2>&1");
        assert_eq!(stages[2].1.trim(), "tail -5 >| out");
    }

    /// One test per runner marker table: each failure line must record failed
    /// behind a masking pipe, and each success line passed behind `tail`.
    #[test]
    fn cargo_and_rustc_markers() {
        let masked = "cargo build 2>&1 | tail -3";
        for failed in [
            "test result: FAILED. 0 passed; 1 failed",
            "error[E0425]: cannot find value `x` in this scope",
            "error: could not compile `app` (lib) due to 1 previous error",
            "error: 1 target failed:",
        ] {
            assert_eq!(outcome(masked, 0, failed), CheckOutcome::Failed, "{failed}");
        }
        assert_eq!(
            outcome(masked, 0, "test result: ok. 1 passed"),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome(
                masked,
                0,
                "test result: ok. 1 passed\nerror: 1 target failed:"
            ),
            CheckOutcome::Failed,
            "--no-fail-fast reports an earlier failing target at the end"
        );
    }

    #[test]
    fn tsc_markers() {
        let output = "src/a.ts(3,5): error TS2322: Type 'string' is not assignable";
        assert_eq!(
            outcome("npx tsc --noEmit | head", 0, output),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome("npx tsc --noEmit | head", 0, "no error TSX here"),
            CheckOutcome::Unknown
        );
    }

    #[test]
    fn pytest_markers() {
        let masked = "pytest -q 2>&1 | tail -5";
        for failed in [
            "FAILED tests/test_api.py::test_login - AssertionError",
            "1 failed, 2 passed in 0.12s",
            "==== 1 error in 0.30s ====",
        ] {
            assert_eq!(outcome(masked, 0, failed), CheckOutcome::Failed, "{failed}");
        }
        for passed in [
            "3 passed in 0.12s",
            "===== 3 passed, 1 warning in 1.02s =====",
        ] {
            assert_eq!(outcome(masked, 0, passed), CheckOutcome::Passed, "{passed}");
        }
        assert_eq!(
            outcome(masked, 0, "logged in 3 places"),
            CheckOutcome::Unknown
        );
    }

    #[test]
    fn jest_and_vitest_markers() {
        let masked = "npx jest 2>&1 | tail -5";
        assert_eq!(
            outcome(masked, 0, "Tests:       1 failed, 3 passed, 4 total"),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome(masked, 0, "Tests:       4 passed, 4 total"),
            CheckOutcome::Passed
        );
        let masked = "npx vitest run | tail -5";
        assert_eq!(
            outcome(masked, 0, " Test Files  1 failed | 2 passed (3)"),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome(masked, 0, "      Tests  6 passed (6)"),
            CheckOutcome::Passed
        );
        assert_eq!(
            outcome(masked, 0, "Tests passed!"),
            CheckOutcome::Unknown,
            "an echoed message is not a runner count"
        );
    }

    #[test]
    fn go_markers() {
        let masked = "go test ./... 2>&1 | tail -5";
        for failed in [
            "--- FAIL: TestParse (0.00s)",
            "FAIL\texample.com/app\t0.012s",
            "FAIL",
        ] {
            assert_eq!(
                outcome(masked, 0, failed),
                CheckOutcome::Failed,
                "{failed:?}"
            );
        }
        assert_eq!(
            outcome(masked, 0, "ok  \texample.com/app\t0.012s"),
            CheckOutcome::Passed
        );
    }

    #[test]
    fn xcodebuild_markers() {
        let masked = "xcodebuild test -scheme App | tail -5";
        assert_eq!(
            outcome(masked, 0, "** TEST FAILED **"),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome(masked, 0, "** BUILD FAILED **"),
            CheckOutcome::Failed
        );
        assert_eq!(
            outcome(masked, 0, "** TEST SUCCEEDED **"),
            CheckOutcome::Passed
        );
    }

    /// The record keeps what was run and where: a truncated command could
    /// not be re-run, and a background check ran in the session directory.
    #[test]
    fn the_full_command_and_the_background_directory_are_kept() {
        let long = format!("cargo test -p app -- {}", "very_long_filter ".repeat(30));
        let mut facts = TurnFacts::default();
        facts.note_tool_result("", &bash_check(&long, false, "test result: ok."));
        assert_eq!(facts.recorded_checks()[0].command, long);
        let task = ToolOutput::TaskOutput(TaskOutputOutput::Result(TaskOutputResult {
            command: "pytest -q".to_owned(),
            status: "completed".to_owned(),
            exit_code: Some(0),
            output: "3 passed in 0.10s".to_owned(),
            ..Default::default()
        }));
        let context = CheckContext {
            cwd: "/work/app".to_owned(),
            worktree: None,
        };
        facts.note_tool_result_at("", &task, &context);
        assert_eq!(facts.recorded_checks()[1].cwd, "/work/app");
    }

    /// Identical failures merge into one record, but their count does not:
    /// "failed twice with a change in between" must stay computable.
    #[test]
    fn repeated_failures_are_counted_and_a_change_between_them_is_seen() {
        let failing = || bash_run("cargo test", 101, false, "test result: FAILED. 1 failed");
        let at = |worktree: Option<&str>| CheckContext {
            cwd: String::new(),
            worktree: worktree.map(str::to_owned),
        };
        let mut facts = TurnFacts::default();
        facts.note_tool_result_at("", &failing(), &at(Some("tree-1")));
        assert_eq!(facts.recorded_checks()[0].failures, 1);
        assert_eq!(
            facts.recorded_checks()[0].failed_again_after_change,
            Some(false)
        );
        facts.note_tool_result_at("", &failing(), &at(Some("tree-1")));
        let check = &facts.recorded_checks()[0];
        assert_eq!(facts.recorded_checks().len(), 1, "identical failures merge");
        assert_eq!(check.failures, 2);
        assert_eq!(check.failed_again_after_change, Some(false), "same tree");
        facts.note_tool_result_at("", &failing(), &at(Some("tree-2")));
        let check = &facts.recorded_checks()[0];
        assert_eq!(check.failures, 3);
        assert_eq!(check.failed_again_after_change, Some(true));
        assert_eq!(check.worktree.as_deref(), Some("tree-2"));
        facts.note_tool_result_at(
            "",
            &bash_run("cargo test", 0, false, "test result: ok."),
            &at(Some("tree-3")),
        );
        let passed = facts.recorded_checks().last().unwrap();
        assert_eq!(passed.outcome, CheckOutcome::Passed);
        assert_eq!(passed.failures, 3, "a later pass keeps the count");

        let mut facts = TurnFacts::default();
        facts.note_tool_result_at("", &failing(), &at(Some("tree-1")));
        facts.note_tool_result_at("", &failing(), &at(None));
        assert_eq!(
            facts.recorded_checks()[0].failed_again_after_change,
            None,
            "an unknown marker is not a known non-change"
        );
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// The marker must move with any edit, including a second edit to a file
    /// that is already modified, and stay put when nothing changed; outside
    /// Git it is unknown rather than an error.
    #[tokio::test]
    async fn the_worktree_marker_moves_with_every_edit() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            worktree_marker(tmp.path()).await,
            None,
            "not a Git worktree"
        );
        git(tmp.path(), &["init", "-q"]);
        let clean = worktree_marker(tmp.path()).await.unwrap();
        std::fs::write(tmp.path().join("app.rs"), "one\n").unwrap();
        let first = worktree_marker(tmp.path()).await.unwrap();
        assert_ne!(first, clean);
        assert_eq!(
            worktree_marker(tmp.path()).await.unwrap(),
            first,
            "no change"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(tmp.path().join("app.rs"), "two\n").unwrap();
        assert_ne!(
            worktree_marker(tmp.path()).await.unwrap(),
            first,
            "same status line, new content"
        );
    }
}
