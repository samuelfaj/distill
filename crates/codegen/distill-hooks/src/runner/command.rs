// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use distill_tools::util::ProcessGroup;

use crate::config::{HookSpec, RUNNER_ALWAYS_SET_ENV};
use crate::event::{
    HookEventEnvelope, MAX_HOOK_FEEDBACK_CHARS, MAX_HOOK_OUTPUT_REPLACEMENT_CHARS, clip_reason,
    clip_text,
};
use crate::result::StopHookOutcome;

use super::{
    GateHookJson, GateKind, GateOutcome, HookHealth, HookRunnerResult, PostToolUseHookJson,
    PostToolUseParse, PromptHookJson, RunContext, StopHookJson, extract_system_message,
    gate_outcome, post_tool_use_json_to_outcome, prompt_json_to_block, stop_json_to_outcome,
};

const CAPTURE_HEADROOM_OVER_REPLACEMENT: usize = 16;
pub(crate) const MAX_OUTPUT_BYTES: usize =
    CAPTURE_HEADROOM_OVER_REPLACEMENT * MAX_HOOK_OUTPUT_REPLACEMENT_CHARS;

const GATE_EXIT_CODE: i32 = 2;
const HOOK_GROUP_REAP: Duration = Duration::from_millis(500);

tokio::task_local! {
    static HOOK_GROUP_REAPS: std::cell::RefCell<Vec<tokio::task::JoinHandle<()>>>;
}

pub async fn join_hook_group_reaps<F: std::future::Future>(work: F) -> F::Output {
    HOOK_GROUP_REAPS
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let out = work.await;
            let joins = HOOK_GROUP_REAPS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
            if !joins.is_empty() {
                let _ = tokio::time::timeout(HOOK_GROUP_REAP, async {
                    for join in joins {
                        let _ = join.await;
                    }
                })
                .await;
            }
            out
        })
        .await
}

struct HookProcessGuard {
    group: Option<Arc<ProcessGroup>>,
}

impl HookProcessGuard {
    fn arm(group: Option<Arc<ProcessGroup>>) -> Self {
        Self { group }
    }

    fn disarm(&mut self) {
        self.group = None;
    }
}

impl Drop for HookProcessGuard {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            let _ = group.kill();
            if let Some(join) = group.schedule_reap(HOOK_GROUP_REAP) {
                let _ = HOOK_GROUP_REAPS.try_with(|slot| slot.borrow_mut().push(join));
            }
        }
    }
}

// SECURITY: a process group lets session close killpg the whole tree; kill_on_drop would leak detached grandchildren.
fn hook_process_group(child: &tokio::process::Child) -> Option<Arc<ProcessGroup>> {
    let mut group = ProcessGroup::new()
        .inspect_err(
            |e| tracing::warn!(pid = child.id(), error = %e, "hook: no process group; not reaped on session close"),
        )
        .ok()?;
    group
        .attach(child)
        .inspect_err(
            |e| tracing::warn!(pid = child.id(), error = %e, "hook: process group attach failed; not reaped on session close"),
        )
        .ok()?;
    Some(Arc::new(group))
}

pub async fn run_command_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> (HookRunnerResult, Duration, Option<String>) {
    let start = Instant::now();

    let Some(ref command) = spec.command else {
        return (
            HookRunnerResult::Failed("command hook has no 'command' field".into()),
            start.elapsed(),
            None,
        );
    };
    let command_str = command.to_string_lossy();

    let stdin_json = match serde_json::to_string(&envelope.to_hook_json()) {
        Ok(j) => j,
        Err(e) => {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("failed to serialize envelope: {e}")),
                elapsed,
                None,
            );
        }
    };

    let debug_payloads = std::env::var("GROK_HOOK_DEBUG").is_ok_and(|v| v == "1");
    if debug_payloads {
        tracing::trace!(
            hook_name = %spec.name,
            stdin_bytes = stdin_json.len(),
            "hook stdin payload"
        );
    }

    let is_shell_command = command_str.contains(' ')
        || command_str.contains('|')
        || command_str.contains('&')
        || command_str.contains(';')
        || command_str.contains('>')
        || command_str.contains('<')
        || command_str.contains('$')
        || command_str.starts_with('~');

    let mut cmd = if is_shell_command {
        let unresolved = find_unresolved_env_vars(&command_str, &spec.extra_env);
        if !unresolved.is_empty() {
            let elapsed = start.elapsed();
            let list = unresolved
                .iter()
                .map(|v| format!("${{{v}}}"))
                .collect::<Vec<_>>()
                .join(", ");
            return (
                HookRunnerResult::Failed(format!(
                    "hook not executed: required env var(s) not set: {list}"
                )),
                elapsed,
                None,
            );
        }
        #[cfg(unix)]
        {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command_str.as_ref());
            c
        }
        #[cfg(not(unix))]
        {
            let command_str = rewrite_hook_command_for_windows_shell(&command_str, &spec.extra_env);
            let inv = distill_config::shell::shell_command_argv(command_str.as_ref());
            let mut c = tokio::process::Command::new(&inv.program);
            c.args(&inv.args).envs(inv.env);
            c
        }
    } else {
        let command_path = if command.is_absolute() {
            command.clone()
        } else {
            spec.source_dir.join(command)
        };
        if !command_path.exists() {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("command not found: {}", command_path.display())),
                elapsed,
                None,
            );
        }
        tokio::process::Command::new(command_path)
    };

    distill_tools::util::detach_command(&mut cmd);
    distill_sandbox::child_net::restrict_child_network(&mut cmd);

    #[cfg(not(unix))]
    let env_root = {
        use distill_config::shell::{WindowsShell, detect_windows_shell};
        if is_shell_command && matches!(detect_windows_shell(), WindowsShell::GitBash(_)) {
            Cow::Owned(ctx.workspace_root.replace('\\', "/"))
        } else {
            Cow::Borrowed(ctx.workspace_root)
        }
    };
    #[cfg(unix)]
    let env_root = Cow::Borrowed(ctx.workspace_root);

    #[allow(clippy::disallowed_methods)]
    let mut child = match cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(ctx.workspace_root)
        // SECURITY: extra_env is applied before the GROK_* identity vars so a hook cannot spoof them.
        .envs(&spec.extra_env)
        .env("GROK_HOOK_EVENT", envelope.hook_event_name.to_string())
        .env("GROK_HOOK_NAME", &spec.name)
        .env("GROK_SESSION_ID", ctx.session_id)
        .env("GROK_WORKSPACE_ROOT", env_root.as_ref())
        .env("CLAUDE_PROJECT_DIR", env_root.as_ref())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let elapsed = start.elapsed();
            return (
                HookRunnerResult::Failed(format!("failed to spawn command: {e}")),
                elapsed,
                None,
            );
        }
    };

    let hook_group = hook_process_group(&child);
    if let (Some(scope), Some(group)) = (ctx.process_scope.as_ref(), hook_group.as_ref())
        && !scope.register(group)
    {
        let _ = group.kill();
        if let Some(join) = group.schedule_reap(HOOK_GROUP_REAP) {
            let _ = join.await;
        }
        drop(child);
        return (
            HookRunnerResult::Failed("session closed before the hook ran".to_string()),
            start.elapsed(),
            None,
        );
    }
    let mut reap = HookProcessGuard::arm(hook_group.clone());

    let stdin = child.stdin.take();
    let timeout = Duration::from_millis(spec.timeout_ms);
    let result = tokio::time::timeout(timeout, async move {
        let write = async {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(stdin_json.as_bytes()).await;
            }
        };
        let (_, output) = tokio::join!(write, child.wait_with_output());
        output
    })
    .await;

    let elapsed = start.elapsed();

    if matches!(result, Ok(Ok(_))) {
        reap.disarm();
    } else if let Some(group) = &hook_group {
        let _ = group.kill();
    }

    match result {
        Err(_) => (
            HookRunnerResult::Failed(format!("timed out after {}ms", spec.timeout_ms)),
            elapsed,
            None,
        ),
        Ok(Err(e)) => (
            HookRunnerResult::Failed(format!("command execution failed: {e}")),
            elapsed,
            None,
        ),
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);

            let stdout = truncate_output(&output.stdout);
            let stderr = truncate_output(&output.stderr);

            if !stderr.is_empty() {
                if exit_code != 0 {
                    tracing::debug!(
                        hook_name = %spec.name,
                        stderr_bytes = stderr.len(),
                        stderr_first_line = stderr_first_line(&stderr).map(clip_reason).unwrap_or_default(),
                        "hook stderr output captured"
                    );
                } else {
                    tracing::debug!(
                        hook_name = %spec.name,
                        stderr_bytes = stderr.len(),
                        "hook stderr output captured"
                    );
                }
            }

            if debug_payloads {
                tracing::trace!(
                    hook_name = %spec.name,
                    stdout_bytes = stdout.len(),
                    "hook stdout payload"
                );
            }

            tracing::debug!(
                hook_name = %spec.name,
                exit_code,
                stdout_bytes = stdout.len(),
                stderr_bytes = stderr.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "hook command completed"
            );

            let system_message = extract_system_message(&stdout);
            let (result, elapsed) = match mode {
                GateKind::Observe => {
                    if exit_code == 0 {
                        (HookRunnerResult::Success, elapsed)
                    } else {
                        (failed_with_exit_code(exit_code, &stderr), elapsed)
                    }
                }
                GateKind::Tool => {
                    parse_blocking_result(&stdout, &stderr, exit_code, &spec.name, elapsed)
                }
                GateKind::Stop => {
                    parse_stop_result(&stdout, &stderr, exit_code, &spec.name, elapsed)
                }
                GateKind::PostTool => {
                    parse_post_tool_use_result(&stdout, &stderr, exit_code, &spec.name, elapsed)
                }
                GateKind::Prompt => {
                    parse_prompt_result(&stdout, &stderr, exit_code, &spec.name, elapsed)
                }
            };
            (result, elapsed, system_message)
        }
    }
}

#[cfg(any(test, not(unix)))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PsQuote {
    Bare,
    Single,
    Double,
}

#[cfg(any(test, not(unix)))]
fn rewrite_posix_env_refs_for_powershell<'a>(
    command: &'a str,
    extra_env: &std::collections::HashMap<String, String>,
) -> Cow<'a, str> {
    let mut out: Option<String> = None;
    let mut cursor = 0;
    let mut first_rewrite_at: Option<usize> = None;
    for r in crate::env_expand::iter_env_var_references(command) {
        if r.start < cursor {
            continue;
        }
        if r.name.is_empty() || r.has_modifier {
            continue;
        }
        if !RUNNER_ALWAYS_SET_ENV.contains(&r.name) && !extra_env.contains_key(r.name) {
            continue;
        }
        let (quote, escaped) = powershell_ctx_at(command, r.start);
        if quote == PsQuote::Single || escaped {
            continue;
        }
        let buf = out.get_or_insert_with(|| String::with_capacity(command.len() + 24));
        if quote == PsQuote::Bare {
            let token_end = command
                .get(r.start..)
                .and_then(|s| {
                    s.find(|c: char| {
                        c.is_whitespace()
                            || matches!(
                                c,
                                ';' | '|' | '&' | '<' | '>' | '(' | ')' | '[' | ']' | ','
                            )
                    })
                })
                .map_or(command.len(), |i| r.start + i);
            if let Some(lit) = command.get(cursor..r.start) {
                buf.push_str(lit);
            }
            buf.push('"');
            if let Some(span) = command.get(r.start..token_end) {
                rewrite_ps_env_refs_in_span(buf, span, extra_env);
            }
            buf.push('"');
            cursor = token_end;
        } else {
            if let Some(lit) = command.get(cursor..r.start) {
                buf.push_str(lit);
            }
            push_ps_env_ref(buf, r.braced, r.name);
            cursor = r.end;
        }
        if first_rewrite_at.is_none() {
            first_rewrite_at = Some(r.start);
        }
    }
    match out {
        None => Cow::Borrowed(command),
        Some(mut buf) => {
            if let Some(tail) = command.get(cursor..) {
                buf.push_str(tail);
            }
            if first_rewrite_at.is_some_and(|at| {
                let pad = command.len() - command.trim_start().len();
                at == pad || (command.as_bytes().get(pad) == Some(&b'"') && at == pad + 1)
            }) && !buf.starts_with("& ")
            {
                buf.insert_str(0, "& ");
            }
            Cow::Owned(buf)
        }
    }
}

#[cfg(any(test, not(unix)))]
fn rewrite_ps_env_refs_in_span(
    buf: &mut String,
    span: &str,
    extra_env: &std::collections::HashMap<String, String>,
) {
    let mut cur = 0;
    for r in crate::env_expand::iter_env_var_references(span) {
        if r.name.is_empty() || r.has_modifier {
            continue;
        }
        if !RUNNER_ALWAYS_SET_ENV.contains(&r.name) && !extra_env.contains_key(r.name) {
            continue;
        }
        if let Some(lit) = span.get(cur..r.start) {
            buf.push_str(lit);
        }
        push_ps_env_ref(buf, r.braced, r.name);
        cur = r.end;
    }
    if let Some(tail) = span.get(cur..) {
        buf.push_str(tail);
    }
}

#[cfg(any(test, not(unix)))]
fn push_ps_env_ref(buf: &mut String, braced: bool, name: &str) {
    if braced {
        buf.push_str("${env:");
        buf.push_str(name);
        buf.push('}');
    } else {
        buf.push_str("$env:");
        buf.push_str(name);
    }
}

#[cfg(any(test, not(unix)))]
fn powershell_ctx_at(command: &str, at: usize) -> (PsQuote, bool) {
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut quote = PsQuote::Bare;
    while i < at {
        let Some(&c) = bytes.get(i) else {
            break;
        };
        match quote {
            PsQuote::Single => {
                if c == b'\'' {
                    quote = PsQuote::Bare;
                }
                i += 1;
            }
            PsQuote::Double => {
                if c == b'`' {
                    i = i.saturating_add(2);
                } else if c == b'"' {
                    quote = PsQuote::Bare;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            PsQuote::Bare => {
                if c == b'`' {
                    i = i.saturating_add(2);
                } else if c == b'\'' {
                    quote = PsQuote::Single;
                    i += 1;
                } else if c == b'"' {
                    quote = PsQuote::Double;
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    let escaped = quote != PsQuote::Single
        && at.checked_sub(1).and_then(|j| bytes.get(j)).copied() == Some(b'`');
    (quote, escaped)
}

#[cfg(not(unix))]
fn rewrite_hook_command_for_windows_shell<'a>(
    command: &'a str,
    extra_env: &std::collections::HashMap<String, String>,
) -> Cow<'a, str> {
    use distill_config::shell::{WindowsShell, detect_windows_shell};
    match detect_windows_shell() {
        WindowsShell::Pwsh | WindowsShell::PowerShell => {
            rewrite_posix_env_refs_for_powershell(command, extra_env)
        }
        WindowsShell::GitBash(_) => Cow::Borrowed(command),
        WindowsShell::Cmd => {
            if command.contains('$') {
                tracing::warn!(
                    "hook command uses $VAR but the Windows shell is cmd, which expands %VAR%"
                );
            }
            Cow::Borrowed(command)
        }
    }
}

fn find_unresolved_env_vars(
    command_str: &str,
    extra_env: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let locally_assigned = find_local_shell_assignments(command_str);
    let mut out: Vec<String> = Vec::new();
    for r in crate::env_expand::iter_env_var_references(command_str) {
        if r.name.is_empty() || r.has_modifier {
            continue;
        }
        if RUNNER_ALWAYS_SET_ENV.contains(&r.name) {
            continue;
        }
        if extra_env.contains_key(r.name) {
            continue;
        }
        if std::env::var_os(r.name).is_some() {
            continue;
        }
        if locally_assigned.contains(r.name) {
            continue;
        }
        out.push(r.name.to_string());
    }
    out.sort();
    out.dedup();
    out
}

fn find_local_shell_assignments(command_str: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let bytes = command_str.as_bytes();
    let mut i = 0;
    let is_statement_start = |idx: usize| -> bool {
        if idx == 0 {
            return true;
        }
        let mut j = idx;
        while j > 0 {
            let Some(prev) = j.checked_sub(1) else {
                return true;
            };
            let Some(&c) = bytes.get(prev) else {
                return true;
            };
            if c == b' ' || c == b'\t' {
                j = prev;
                continue;
            }
            return matches!(c, b';' | b'&' | b'|' | b'\n' | b'(' | b'{');
        }
        true
    };
    while i < bytes.len() {
        let Some(&c) = bytes.get(i) else {
            break;
        };
        if !(c.is_ascii_alphabetic() || c == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            i += 1;
        }
        let ident = bytes
            .get(start..i)
            .and_then(|s| std::str::from_utf8(s).ok())
            .unwrap_or("");
        if ident.is_empty() {
            continue;
        }
        if bytes.get(i).copied() == Some(b'=') && is_statement_start(start) {
            names.insert(ident.to_string());
            continue;
        }
        if ident == "read" && is_statement_start(start) {
            while bytes.get(i).is_some_and(|&b| b == b' ' || b == b'\t') {
                i += 1;
            }
            while i < bytes.len() {
                let Some(&c2) = bytes.get(i) else {
                    break;
                };
                if matches!(c2, b';' | b'&' | b'|' | b'\n' | b'<' | b'>') {
                    break;
                }
                if c2 == b' ' || c2 == b'\t' {
                    i += 1;
                    continue;
                }
                if c2 == b'-' {
                    while bytes.get(i).is_some_and(|&b| b != b' ' && b != b'\t') {
                        i += 1;
                    }
                    continue;
                }
                if !(c2.is_ascii_alphabetic() || c2 == b'_') {
                    break;
                }
                let s = i;
                while bytes
                    .get(i)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    i += 1;
                }
                let read_ident = bytes
                    .get(s..i)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                if !read_ident.is_empty() {
                    names.insert(read_ident.to_string());
                }
            }
        }
    }
    names
}

fn stderr_first_line(stderr: &str) -> Option<&str> {
    stderr.lines().map(str::trim).find(|line| !line.is_empty())
}

fn append_stderr_line(message: &str, stderr: &str) -> String {
    match stderr_first_line(stderr) {
        Some(line) => format!("{message}: {}", clip_reason(line)),
        None => message.to_string(),
    }
}

/// Same shape in every mode (`exit code N: <first stderr line>`); the dispatcher and the UI name the hook and the verb.
fn failed_with_exit_code(exit_code: i32, stderr: &str) -> HookRunnerResult {
    HookRunnerResult::Failed(append_stderr_line(
        &format!("exit code {exit_code}"),
        stderr,
    ))
}

fn parse_blocking_result(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let gate_document = if !stdout.trim().is_empty() {
        serde_json::from_str::<GateHookJson>(stdout.trim())
            .ok()
            .filter(GateHookJson::is_gate_document)
    } else {
        None
    };

    if let Some(json) = gate_document {
        let health = HookHealth::from_success(exit_code == 0);
        match gate_outcome(json, hook_name, stderr_first_line(stderr), health) {
            GateOutcome::Deny(reason) => {
                if exit_code != GATE_EXIT_CODE && exit_code != 0 {
                    tracing::warn!(
                        hook_name,
                        exit_code,
                        "JSON decision is 'deny' but exit code is not 0 or 2 — using JSON decision"
                    );
                }
                return (
                    HookRunnerResult::Deny {
                        reason,
                        hook_name: hook_name.to_string(),
                    },
                    elapsed,
                );
            }
            GateOutcome::Allow { .. } | GateOutcome::Ask { .. } if exit_code == GATE_EXIT_CODE => {
                tracing::warn!(
                    hook_name,
                    "JSON decision is 'allow' or 'ask' but exit code is 2 — denying (stdout is ignored on exit 2)"
                );
            }
            GateOutcome::Defer if exit_code == GATE_EXIT_CODE => {
                tracing::warn!(
                    hook_name,
                    "JSON decision is 'defer' but exit code is 2 — denying (stdout is ignored on exit 2)"
                );
            }
            GateOutcome::Allow {
                updated_input,
                additional_context,
            } => {
                return (
                    HookRunnerResult::Allow {
                        updated_input,
                        additional_context,
                    },
                    elapsed,
                );
            }
            GateOutcome::Ask {
                reason,
                updated_input,
                additional_context,
            } => {
                return (
                    HookRunnerResult::Ask {
                        reason,
                        updated_input,
                        additional_context,
                    },
                    elapsed,
                );
            }
            GateOutcome::Defer => return (HookRunnerResult::Defer, elapsed),
            GateOutcome::Failed(err) if exit_code != GATE_EXIT_CODE => {
                return (
                    HookRunnerResult::Failed(append_stderr_line(&err, stderr)),
                    elapsed,
                );
            }
            GateOutcome::Failed(err) => {
                let reason = clip_reason(&match stderr_first_line(stderr) {
                    Some(line) => format!("{err}: {line}"),
                    None => err,
                });
                return (
                    HookRunnerResult::Deny {
                        reason,
                        hook_name: hook_name.to_string(),
                    },
                    elapsed,
                );
            }
        }
    }

    match exit_code {
        0 => (
            HookRunnerResult::Allow {
                updated_input: None,
                additional_context: None,
            },
            elapsed,
        ),
        GATE_EXIT_CODE => (
            HookRunnerResult::Deny {
                reason: stderr_first_line(stderr)
                    .map(clip_reason)
                    .unwrap_or_else(|| {
                        format!("denied by hook '{hook_name}' (exit code {GATE_EXIT_CODE})")
                    }),
                hook_name: hook_name.to_string(),
            },
            elapsed,
        ),
        _ => (failed_with_exit_code(exit_code, stderr), elapsed),
    }
}

fn parse_stop_result(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        match serde_json::from_str::<StopHookJson>(trimmed) {
            Ok(json) => {
                return match stop_json_to_outcome(json, hook_name) {
                    Ok(outcome) => (HookRunnerResult::Stop(outcome), elapsed),
                    Err(err) => (HookRunnerResult::Failed(err), elapsed),
                };
            }
            Err(err) => {
                if trimmed.starts_with('{') {
                    tracing::warn!(
                        hook_name,
                        error = %err,
                        "stop hook stdout looks like JSON but failed to parse; falling back to the exit code"
                    );
                }
            }
        }
    }
    match exit_code {
        0 => (HookRunnerResult::Stop(StopHookOutcome::default()), elapsed),
        GATE_EXIT_CODE => {
            let feedback = stderr.trim();
            let block_reason = if feedback.is_empty() {
                format!("Blocked by stop hook '{hook_name}' (exit code {GATE_EXIT_CODE})")
            } else {
                feedback.to_string()
            };
            (
                HookRunnerResult::Stop(StopHookOutcome {
                    block_reason: Some(block_reason),
                    ..Default::default()
                }),
                elapsed,
            )
        }
        _ => (failed_with_exit_code(exit_code, stderr), elapsed),
    }
}

fn parse_prompt_result(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let stderr_message = {
        let trimmed = stderr.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        match serde_json::from_str::<PromptHookJson>(trimmed) {
            Ok(json) => match prompt_json_to_block(&json, hook_name, stderr_message.as_deref()) {
                Ok(Some(reason)) => {
                    return (
                        HookRunnerResult::Block {
                            reason,
                            hook_name: hook_name.to_string(),
                        },
                        elapsed,
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    if exit_code == GATE_EXIT_CODE {
                        tracing::warn!(
                            hook_name,
                            hook_failure = %err,
                            "prompt hook JSON is invalid but exit 2 still blocks"
                        );
                    } else {
                        return (
                            HookRunnerResult::Failed(append_stderr_line(&err, stderr)),
                            elapsed,
                        );
                    }
                }
            },
            Err(err) => {
                if trimmed.starts_with('{') {
                    tracing::warn!(
                        hook_name,
                        error = %err,
                        "prompt hook stdout looks like JSON but failed to parse; falling back to the exit code"
                    );
                }
            }
        }
    }
    match exit_code {
        0 => (HookRunnerResult::Success, elapsed),
        GATE_EXIT_CODE => (
            HookRunnerResult::Block {
                reason: stderr_message.unwrap_or_else(|| {
                    format!("Prompt blocked by hook '{hook_name}' (exit code {GATE_EXIT_CODE})")
                }),
                hook_name: hook_name.to_string(),
            },
            elapsed,
        ),
        _ => (failed_with_exit_code(exit_code, stderr), elapsed),
    }
}

fn parse_post_tool_use_result(
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    hook_name: &str,
    elapsed: Duration,
) -> (HookRunnerResult, Duration) {
    let health = HookHealth::from_success(exit_code == 0);
    let trimmed = stdout.trim();
    let PostToolUseParse {
        mut outcome,
        failure,
    } = if trimmed.is_empty() {
        PostToolUseParse::default()
    } else {
        match serde_json::from_str::<PostToolUseHookJson>(trimmed) {
            Ok(json) => post_tool_use_json_to_outcome(json, hook_name, health),
            Err(err) => {
                if trimmed.starts_with('{') {
                    tracing::warn!(
                        hook_name,
                        error = %err,
                        "post_tool_use hook stdout looks like JSON but failed to parse; ignoring"
                    );
                }
                PostToolUseParse::default()
            }
        }
    };

    if exit_code == GATE_EXIT_CODE && outcome.block_reason.is_none() {
        let feedback = stderr.trim();
        if !feedback.is_empty() {
            outcome.block_reason = Some(clip_text(feedback, MAX_HOOK_FEEDBACK_CHARS));
        }
    }

    if exit_code != 0 && exit_code != GATE_EXIT_CODE {
        let exit_failure = append_stderr_line(&format!("exit code {exit_code}"), stderr);
        if outcome.is_empty() {
            return (HookRunnerResult::Failed(exit_failure), elapsed);
        }
        return (
            HookRunnerResult::PostToolUse {
                outcome,
                failure: Some(exit_failure),
            },
            elapsed,
        );
    }

    (HookRunnerResult::PostToolUse { outcome, failure }, elapsed)
}

fn truncate_output(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let Some(head) = bytes.get(..MAX_OUTPUT_BYTES) else {
            return String::from_utf8_lossy(bytes).into_owned();
        };
        let mut truncated = String::from_utf8_lossy(head).into_owned();
        truncated.push_str(" [truncated]");
        tracing::warn!(
            total_bytes = bytes.len(),
            max_bytes = MAX_OUTPUT_BYTES,
            "hook output truncated"
        );
        truncated
    }
}

pub fn resolve_command_path(spec: &HookSpec) -> Option<std::path::PathBuf> {
    let command = spec.command.as_ref()?;
    if command.is_absolute() {
        Some(command.clone())
    } else {
        Some(spec.source_dir.join(command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        MAX_HOOK_FEEDBACK_CHARS, MAX_HOOK_OUTPUT_REPLACEMENT_CHARS, MAX_REASON_CHARS,
    };
    use crate::result::{OutputReplacement, ReplacementKind};

    fn parse(json: &str) -> HookRunnerResult {
        parse_blocking_result(json, "", 0, "test", Duration::ZERO).0
    }

    #[test]
    fn parse_rewrite_shapes() {
        let rewrite = |r: HookRunnerResult| match r {
            HookRunnerResult::Allow {
                updated_input: Some(rw),
                ..
            } => rw,
            other => panic!("expected Allow with rewrite, got {other:?}"),
        };
        for json in [
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"xb build"}}}"#,
            r#"{"decision":"allow","hookSpecificOutput":{"updatedInput":{"command":"xb build"}}}"#,
        ] {
            assert_eq!(
                rewrite(parse(json)).get("command").and_then(|v| v.as_str()),
                Some("xb build")
            );
        }
        assert!(matches!(
            parse(r#"{"decision":"deny","hookSpecificOutput":{"updatedInput":{"command":"x"}}}"#),
            HookRunnerResult::Deny { .. }
        ));
        let (failed_hook, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"xb build"}}}"#,
            "",
            1,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(
            failed_hook,
            HookRunnerResult::Allow {
                updated_input: None,
                ..
            }
        ));
    }

    #[test]
    fn unknown_decision_with_exit_2_still_denies() {
        let (result, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"denied"}}"#,
            "writes outside the repo\n",
            2,
            "typo",
            Duration::ZERO,
        );
        match result {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(
                reason,
                "unknown decision value 'denied' in 'hookSpecificOutput.permissionDecision': writes outside the repo"
            ),
            other => panic!("expected Deny, got {other:?}"),
        }

        let (result, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"denied"}}"#,
            &"x".repeat(MAX_REASON_CHARS),
            2,
            "typo",
            Duration::ZERO,
        );
        match result {
            HookRunnerResult::Deny { reason, .. } => assert!(
                reason.starts_with(
                    "unknown decision value 'denied' in 'hookSpecificOutput.permissionDecision'"
                ),
                "the error must survive a full-length stderr line, got: {reason}"
            ),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn parse_decisions() {
        fn summarize(result: HookRunnerResult) -> String {
            match result {
                HookRunnerResult::Allow {
                    updated_input: Some(_),
                    ..
                } => "allow + rewrite".to_string(),
                HookRunnerResult::Allow { .. } => "allow".to_string(),
                HookRunnerResult::Ask {
                    reason,
                    updated_input,
                    ..
                } => {
                    let label = if updated_input.is_some() {
                        "ask + rewrite"
                    } else {
                        "ask"
                    };
                    match reason {
                        Some(reason) => format!("{label}: {reason}"),
                        None => label.to_string(),
                    }
                }
                HookRunnerResult::Defer => "defer".to_string(),
                HookRunnerResult::Deny { reason, .. } => format!("deny: {reason}"),
                HookRunnerResult::Failed(error) => format!("failed: {error}"),
                other => panic!("expected a gate result, got {other:?}"),
            }
        }
        for (json, expected) in [
            (r#"{"decision":"allow"}"#, "allow"),
            (r#"{"decision":"approve"}"#, "allow"),
            (r#"{"decision":"block","reason":"nope"}"#, "deny: nope"),
            (r#"{"decision":"deny"}"#, "deny: denied by hook 'test'"),
            (
                r#"{"decision":"maybe"}"#,
                "failed: unknown decision value 'maybe' in 'decision'",
            ),
            (
                r#"{"decision":"allow","continue":false,"systemMessage":"hi"}"#,
                "allow",
            ),
            (r#"{"continue":false,"stopReason":"enough"}"#, "allow"),
            (
                r#"{"decision":"allow","hookSpecificOutput":{"updateInput":{"command":"x"}}}"#,
                "allow",
            ),
            (r#"{"updatedInput":{"command":"x"}}"#, "allow"),
            (r#"{"permissionDecision":"deny"}"#, "allow"),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":"nope"}}"#,
                "allow",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":"nope"}}"#,
                "ask",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
                "ask",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"command":"x"}}}"#,
                "ask + rewrite",
            ),
            (
                r#"{"decision":"allow","reason":"allow-reason","hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"ask-reason"}}"#,
                "ask: ask-reason",
            ),
            (
                r#"{"decision":"ask","hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"allow-reason"}}"#,
                "allow",
            ),
            (
                r#"{"decision":"deny","reason":"nope","hookSpecificOutput":{"permissionDecision":"ask"}}"#,
                "ask: nope",
            ),
            (
                r#"{"decision":"allow","reason":"allow-reason","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"block-reason"}}"#,
                "deny: block-reason",
            ),
            (
                r#"{"decision":"deny","hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"allow side"}}"#,
                "allow",
            ),
            (
                r#"{"reason":"blocked: writes outside repo","hookSpecificOutput":{"permissionDecision":"deny"}}"#,
                "deny: blocked: writes outside repo",
            ),
            (
                r#"{"decision":"deny","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"same call, one reason"}}"#,
                "deny: same call, one reason",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"maybe"}}"#,
                "failed: unknown decision value 'maybe' in 'hookSpecificOutput.permissionDecision'",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"defer","permissionDecisionReason":"because"}}"#,
                "defer",
            ),
            (r#"{"decision":"defer"}"#, "defer"),
            (
                r#"{"decision":"defer","hookSpecificOutput":{"permissionDecision":"ask"}}"#,
                "ask",
            ),
            (
                r#"{"decision":"defer","hookSpecificOutput":{"permissionDecision":"maybe"}}"#,
                "failed: unknown decision value 'maybe' in 'hookSpecificOutput.permissionDecision'",
            ),
        ] {
            assert_eq!(summarize(parse(json)), expected, "for {json}");
        }
    }

    #[test]
    fn json_reason_is_capped() {
        let long = "é".repeat(MAX_REASON_CHARS + 50);
        let json = serde_json::json!({ "decision": "deny", "reason": long }).to_string();
        match parse(&json) {
            HookRunnerResult::Deny { reason, .. } => {
                assert_eq!(
                    reason,
                    format!("{}… [+50 chars]", "é".repeat(MAX_REASON_CHARS))
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        let ask = serde_json::json!({ "decision": "ask", "reason": long }).to_string();
        match parse(&ask) {
            HookRunnerResult::Ask { reason, .. } => {
                assert_eq!(
                    reason,
                    Some(format!("{}… [+50 chars]", "é".repeat(MAX_REASON_CHARS)))
                );
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn allow_and_ask_carry_additional_context_and_drop_a_blank() {
        let context = |result: HookRunnerResult| match result {
            HookRunnerResult::Allow {
                additional_context, ..
            }
            | HookRunnerResult::Ask {
                additional_context, ..
            } => additional_context,
            other => panic!("expected Allow or Ask, got {other:?}"),
        };
        for decision in ["allow", "ask"] {
            assert_eq!(
                context(parse(&format!(
                    r#"{{"hookSpecificOutput":{{"permissionDecision":"{decision}","additionalContext":"note"}}}}"#
                )))
                .as_deref(),
                Some("note"),
                "for {decision}"
            );
            assert_eq!(
                context(parse(&format!(
                    r#"{{"hookSpecificOutput":{{"permissionDecision":"{decision}","additionalContext":"  "}}}}"#
                ))),
                None,
                "a blank additionalContext is not context, for {decision}"
            );
        }
    }

    #[test]
    fn additional_context_is_capped_in_characters_not_bytes() {
        let long = "é".repeat(MAX_HOOK_FEEDBACK_CHARS + 50);
        let json = serde_json::json!({
            "hookSpecificOutput": { "permissionDecision": "allow", "additionalContext": long }
        })
        .to_string();
        match parse(&json) {
            HookRunnerResult::Allow {
                additional_context, ..
            } => assert_eq!(
                additional_context,
                Some(format!(
                    "{}… [+50 chars]",
                    "é".repeat(MAX_HOOK_FEEDBACK_CHARS)
                ))
            ),
            other => panic!("expected Allow, got {other:?}"),
        }

        let stop = serde_json::json!({ "hookSpecificOutput": { "additionalContext": long } });
        let (result, _) = parse_stop_result(&stop.to_string(), "", 0, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).additional_context,
            Some(format!(
                "{}… [+50 chars]",
                "é".repeat(MAX_HOOK_FEEDBACK_CHARS)
            ))
        );
    }

    #[test]
    fn broken_hook_loses_its_additional_context() {
        let (result, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"note"}}"#,
            "",
            1,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(
            result,
            HookRunnerResult::Allow {
                additional_context: None,
                ..
            }
        ));
    }

    #[test]
    fn deny_drops_updated_input() {
        assert!(matches!(
            parse(r#"{"decision":"deny","hookSpecificOutput":{"updatedInput":{"command":"x"}}}"#),
            HookRunnerResult::Deny { .. }
        ));
    }

    #[test]
    fn failure_keeps_allow_but_drops_updated_input() {
        let (result, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"updatedInput":{"command":"xb build"}}}"#,
            "",
            1,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(
            result,
            HookRunnerResult::Allow {
                updated_input: None,
                ..
            }
        ));
    }

    #[test]
    fn non_object_updated_input_drops_the_rewrite() {
        assert!(matches!(
            parse(r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":"nope"}}"#),
            HookRunnerResult::Allow {
                updated_input: None,
                ..
            }
        ));
    }

    #[test]
    fn unknown_decision_is_failed_naming_the_field() {
        let cases = [
            (
                r#"{"decision":"maybe"}"#,
                "unknown decision value 'maybe' in 'decision'",
            ),
            (
                r#"{"hookSpecificOutput":{"permissionDecision":"maybe"}}"#,
                "unknown decision value 'maybe' in 'hookSpecificOutput.permissionDecision'",
            ),
        ];
        for (json, expected) in cases {
            match parse(json) {
                HookRunnerResult::Failed(error) => assert_eq!(error, expected, "for {json}"),
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn ask_forces_the_prompt_with_its_reason() {
        assert!(matches!(
            parse(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#),
            HookRunnerResult::Ask { reason: None, .. }
        ));
        match parse(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"ask-reason"}}"#,
        ) {
            HookRunnerResult::Ask { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("ask-reason"))
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn legacy_decision_applies_when_permission_decision_absent() {
        let cases = [
            (r#"{"decision":"approve"}"#, None),
            (r#"{"decision":"allow"}"#, None),
            (r#"{"decision":"block","reason":"nope"}"#, Some("nope")),
            (r#"{"decision":"deny"}"#, Some("denied by hook 'test'")),
        ];
        for (json, deny_reason) in cases {
            match (parse(json), deny_reason) {
                (HookRunnerResult::Allow { .. }, None) => {}
                (HookRunnerResult::Deny { reason, .. }, Some(expected)) => {
                    assert_eq!(reason, expected, "for {json}")
                }
                (other, _) => panic!("unexpected result for {json}: {other:?}"),
            }
        }
    }

    #[test]
    fn canonical_permission_decision_overrides_legacy_decision() {
        for json in [
            r#"{"decision":"block","hookSpecificOutput":{"permissionDecision":"allow"}}"#,
            r#"{"decision":"deny","hookSpecificOutput":{"permissionDecision":"allow"}}"#,
        ] {
            assert!(
                matches!(parse(json), HookRunnerResult::Allow { .. }),
                "canonical allow must override legacy block/deny, for {json}"
            );
        }
        match parse(
            r#"{"decision":"approve","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
        ) {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, "nope"),
            other => panic!("canonical deny must override legacy approve, got {other:?}"),
        }
    }

    #[test]
    fn deny_reason_and_stderr_excerpt_are_capped() {
        let long = "é".repeat(MAX_REASON_CHARS + 50);
        let capped = format!("{}… [+50 chars]", "é".repeat(MAX_REASON_CHARS));

        let json = serde_json::json!({ "decision": "deny", "reason": long }).to_string();
        match parse(&json) {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, capped),
            other => panic!("expected Deny, got {other:?}"),
        }

        let ask = serde_json::json!({ "decision": "ask", "reason": long }).to_string();
        match parse(&ask) {
            HookRunnerResult::Ask { reason, .. } => {
                assert_eq!(reason.as_deref(), Some(capped.as_str()));
            }
            other => panic!("expected Ask, got {other:?}"),
        }

        let (deny, _) = parse_blocking_result("", &long, 2, "test", Duration::ZERO);
        match deny {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, capped),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn exit_code_decides_when_no_gate_json() {
        for (stdout, code, expect_allow) in [
            ("", 0, true),
            ("not json at all", 0, true),
            (r#"{"detail":"ok"}"#, 0, true),
            ("", 2, false),
        ] {
            let (result, _) = parse_blocking_result(stdout, "", code, "test", Duration::ZERO);
            if expect_allow {
                assert!(
                    matches!(result, HookRunnerResult::Allow { .. }),
                    "for {stdout}"
                );
            } else {
                assert!(
                    matches!(result, HookRunnerResult::Deny { .. }),
                    "for {stdout}"
                );
            }
        }
        let (fail, _) = parse_blocking_result(r#"{"detail":"x"}"#, "", 1, "test", Duration::ZERO);
        assert!(matches!(fail, HookRunnerResult::Failed(_)));
    }

    #[test]
    fn exit_2_denies_over_json_allow() {
        let (deny, _) =
            parse_blocking_result(r#"{"decision":"allow"}"#, "", 2, "test", Duration::ZERO);
        assert!(matches!(deny, HookRunnerResult::Deny { .. }));
    }

    #[test]
    fn deny_and_failure_carry_first_stderr_line() {
        let (deny, _) = parse_blocking_result(
            "",
            "  \nrejected by policy\nmore\n",
            2,
            "test",
            Duration::ZERO,
        );
        match deny {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, "rejected by policy"),
            other => panic!("expected Deny, got {other:?}"),
        }

        let (fail, _) = parse_blocking_result("", "config missing\n", 1, "test", Duration::ZERO);
        match fail {
            HookRunnerResult::Failed(error) => assert!(
                error.contains("exit code 1") && error.contains("config missing"),
                "failure must carry exit code AND stderr text, got: {error}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn stderr_line_is_capped() {
        let long = "é".repeat(MAX_REASON_CHARS + 50);
        let (deny, _) = parse_blocking_result("", &long, 2, "test", Duration::ZERO);
        match deny {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(
                reason,
                format!("{}… [+50 chars]", "é".repeat(MAX_REASON_CHARS))
            ),
            other => panic!("expected Deny, got {other:?}"),
        }

        let exact = "x".repeat(MAX_REASON_CHARS);
        let (deny, _) = parse_blocking_result("", &exact, 2, "test", Duration::ZERO);
        match deny {
            HookRunnerResult::Deny { reason, .. } => assert_eq!(reason, exact),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn blank_json_reason_falls_back() {
        let blank = || {
            serde_json::from_str::<GateHookJson>(r#"{"decision":"deny","reason":"  "}"#)
                .expect("valid gate JSON")
        };
        assert!(
            matches!(gate_outcome(blank(), "h", Some("quota exceeded"), HookHealth::Healthy), GateOutcome::Deny(ref reason) if reason == "quota exceeded")
        );
        assert!(
            matches!(gate_outcome(blank(), "h", None, HookHealth::Healthy), GateOutcome::Deny(ref reason) if reason == "denied by hook 'h'")
        );
    }

    #[test]
    fn unknown_decision_failure_carries_stderr() {
        let (result, _) = parse_blocking_result(
            r#"{"decision":"maybe"}"#,
            "config missing\n",
            1,
            "test",
            Duration::ZERO,
        );
        match result {
            HookRunnerResult::Failed(error) => assert!(
                error.contains("maybe") && error.contains("config missing"),
                "got: {error}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn blank_deny_reason_falls_back_to_stderr_then_generic() {
        let blank = || {
            serde_json::from_str::<GateHookJson>(r#"{"decision":"deny","reason":"  "}"#)
                .expect("valid gate JSON")
        };
        assert!(matches!(
            gate_outcome(blank(), "h", Some("quota exceeded"), HookHealth::Healthy),
            GateOutcome::Deny(ref reason) if reason == "quota exceeded"
        ));
        assert!(matches!(
            gate_outcome(blank(), "h", None, HookHealth::Healthy),
            GateOutcome::Deny(ref reason) if reason == "denied by hook 'h'"
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn observe_failure_reports_exit_code_and_stderr_line() {
        let spec = make_shell_spec("echo 'disk full' >&2; exit 1");
        let (result, _, _) =
            run_command_hook(&spec, &make_envelope(), &make_ctx(), GateKind::Observe).await;
        match result {
            HookRunnerResult::Failed(error) => assert_eq!(error, "exit code 1: disk full"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn json_decision_vs_exit_code() {
        let (deny, _) = parse_blocking_result(
            r#"{"decision":"deny","reason":"nope"}"#,
            "",
            0,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(deny, HookRunnerResult::Deny { .. }));

        let (blocked, _) =
            parse_blocking_result(r#"{"decision":"allow"}"#, "", 2, "test", Duration::ZERO);
        assert!(matches!(blocked, HookRunnerResult::Deny { .. }));

        let (ask_blocked, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm"}}"#,
            "",
            2,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(ask_blocked, HookRunnerResult::Deny { .. }));

        let (defer_blocked, _) = parse_blocking_result(
            r#"{"hookSpecificOutput":{"permissionDecision":"defer"}}"#,
            "",
            2,
            "test",
            Duration::ZERO,
        );
        assert!(matches!(defer_blocked, HookRunnerResult::Deny { .. }));
    }

    fn stop_outcome(result: HookRunnerResult) -> StopHookOutcome {
        match result {
            HookRunnerResult::Stop(outcome) => outcome,
            other => panic!("expected Stop outcome, got {other:?}"),
        }
    }

    #[test]
    fn stop_block_decision_with_reason() {
        let (result, _) = parse_stop_result(
            r#"{"decision":"block","reason":"tests are failing"}"#,
            "",
            0,
            "my-stop",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                block_reason: Some("tests are failing".into()),
                ..Default::default()
            }
        );

        let (result, _) =
            parse_stop_result(r#"{"decision":"block"}"#, "", 0, "my-stop", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("Blocked by stop hook 'my-stop'")
        );
    }

    #[test]
    fn stop_exit_2_blocks_with_stderr() {
        let (result, _) =
            parse_stop_result("", "run the test suite first\n", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("run the test suite first")
        );

        let (result, _) = parse_stop_result("", "", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("Blocked by stop hook 's' (exit code 2)")
        );
    }

    #[test]
    fn stop_stdout_json_wins_over_exit_2() {
        let (result, _) = parse_stop_result(
            r#"{"continue":false,"stopReason":"enough","hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "log noise\n",
            2,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome
                .force_stop
                .as_ref()
                .and_then(|f| f.reason.as_deref()),
            Some("enough")
        );
        assert_eq!(outcome.additional_context.as_deref(), Some("ctx"));

        let (result, _) = parse_stop_result("log noise\n", "blocked", 2, "s", Duration::ZERO);
        assert_eq!(
            stop_outcome(result).block_reason.as_deref(),
            Some("blocked")
        );
    }

    #[test]
    fn stop_continue_false_prevents_continuation() {
        let (result, _) = parse_stop_result(
            r#"{"continue":false,"stopReason":"budget exhausted"}"#,
            "",
            0,
            "s",
            Duration::ZERO,
        );
        let outcome = stop_outcome(result);
        assert_eq!(
            outcome,
            StopHookOutcome {
                force_stop: Some(crate::result::StopOverride {
                    reason: Some("budget exhausted".into()),
                }),
                ..Default::default()
            }
        );
        let (result, _) = parse_stop_result(r#"{"continue":true}"#, "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());
    }

    #[test]
    fn stop_allow_failure_and_unknown_decision() {
        let (result, _) = parse_stop_result("", "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());

        let (result, _) = parse_stop_result("all done!", "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());

        let (result, _) = parse_stop_result("", "boom", 1, "s", Duration::ZERO);
        match result {
            HookRunnerResult::Failed(error) => assert!(
                error.contains("exit code 1") && error.contains("boom"),
                "stop failure must carry exit code AND stderr text, got: {error}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }

        let (result, _) = parse_stop_result(r#"{"decision":"deny"}"#, "", 0, "s", Duration::ZERO);
        assert!(matches!(result, HookRunnerResult::Failed(_)));

        let (result, _) =
            parse_stop_result(r#"{"decision":"approve"}"#, "", 0, "s", Duration::ZERO);
        assert!(stop_outcome(result).is_empty());
    }

    fn prompt_block_reason(result: HookRunnerResult) -> String {
        match result {
            HookRunnerResult::Block { reason, .. } => reason,
            other => panic!("expected Block (prompt block), got {other:?}"),
        }
    }

    #[test]
    fn prompt_json_block_honored_on_any_exit_code() {
        for exit_code in [0, 1, 2, 127] {
            let (result, _) = parse_prompt_result(
                r#"{"decision":"block","reason":"policy says no"}"#,
                "",
                exit_code,
                "p",
                Duration::ZERO,
            );
            assert_eq!(
                prompt_block_reason(result),
                "policy says no",
                "exit code {exit_code}"
            );
        }
    }

    #[test]
    fn prompt_json_block_reason_falls_back_to_stderr_then_generic() {
        let (result, _) = parse_prompt_result(
            r#"{"decision":"block"}"#,
            "explained on stderr\n",
            0,
            "p",
            Duration::ZERO,
        );
        assert_eq!(prompt_block_reason(result), "explained on stderr");

        let (result, _) =
            parse_prompt_result(r#"{"decision":"block"}"#, "", 0, "p", Duration::ZERO);
        assert_eq!(prompt_block_reason(result), "Prompt blocked by hook 'p'");
    }

    #[test]
    fn prompt_exit_2_blocks_with_full_multiline_stderr() {
        let (result, _) = parse_prompt_result(
            "",
            "policy violated:\n- no prod deploys on friday\n",
            2,
            "p",
            Duration::ZERO,
        );
        assert_eq!(
            prompt_block_reason(result),
            "policy violated:\n- no prod deploys on friday"
        );

        let (result, _) = parse_prompt_result("", "", 2, "p", Duration::ZERO);
        assert_eq!(
            prompt_block_reason(result),
            "Prompt blocked by hook 'p' (exit code 2)"
        );
    }

    #[test]
    fn prompt_allow_on_exit_0_discards_stdout() {
        for stdout in [
            "",
            "plain context text",
            "{}",
            r#"{"hookSpecificOutput":{"additionalContext":"ctx","sessionTitle":"t"}}"#,
        ] {
            let (result, _) = parse_prompt_result(stdout, "", 0, "p", Duration::ZERO);
            assert!(
                matches!(result, HookRunnerResult::Success),
                "stdout {stdout:?} must allow"
            );
        }
    }

    #[test]
    fn prompt_unknown_decision_is_failure() {
        for stdout in [r#"{"decision":"deny"}"#, r#"{"decision":"allow"}"#] {
            let (result, _) = parse_prompt_result(stdout, "", 0, "p", Duration::ZERO);
            assert!(
                matches!(result, HookRunnerResult::Failed(_)),
                "stdout {stdout:?} must fail so typos surface"
            );
        }
    }

    #[test]
    fn prompt_invalid_decision_json_with_exit_2_blocks() {
        let (result, _) = parse_prompt_result(
            r#"{"decision":"allow"}"#,
            "still blocked",
            2,
            "p",
            Duration::ZERO,
        );
        assert_eq!(prompt_block_reason(result), "still blocked");
    }

    #[test]
    fn prompt_approve_decision_renders_no_verdict() {
        let (result, _) =
            parse_prompt_result(r#"{"decision":"approve"}"#, "", 0, "p", Duration::ZERO);
        assert!(matches!(result, HookRunnerResult::Success));
        let (result, _) = parse_prompt_result(
            r#"{"decision":"approve"}"#,
            "blocked anyway\n",
            2,
            "p",
            Duration::ZERO,
        );
        assert_eq!(prompt_block_reason(result), "blocked anyway");
    }

    #[test]
    fn prompt_other_exit_codes_fail_open() {
        let (result, _) = parse_prompt_result("", "boom\n", 1, "p", Duration::ZERO);
        match result {
            HookRunnerResult::Failed(error) => assert!(
                error.contains("exit code 1") && error.contains("boom"),
                "prompt failure must carry exit code AND stderr text, got: {error}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn post_tool_use_outcome(result: HookRunnerResult) -> crate::result::PostToolUseHookOutcome {
        match result {
            HookRunnerResult::PostToolUse { outcome, .. } => outcome,
            other => panic!("expected PostToolUse outcome, got {other:?}"),
        }
    }

    #[test]
    fn post_tool_use_exit_2_feeds_stderr() {
        let (result, _) =
            parse_post_tool_use_result("", "lint found 3 issues\n", 2, "p", Duration::ZERO);
        assert_eq!(
            post_tool_use_outcome(result).block_reason.as_deref(),
            Some("lint found 3 issues")
        );
    }

    #[test]
    fn post_tool_use_broken_hook_keeps_only_its_block_reason() {
        let json = serde_json::json!({
            "decision": "block",
            "reason": "tests failed",
            "hookSpecificOutput": {
                "additionalContext": "all tests passed",
                "updatedToolOutput": { "type": "Bash" },
                "updatedMCPToolOutput": "all tests passed",
            },
        })
        .to_string();
        let (result, _) = parse_post_tool_use_result(&json, "boom", 1, "p", Duration::ZERO);
        let HookRunnerResult::PostToolUse { outcome, failure } = result else {
            panic!("a surviving block reason must still be delivered");
        };
        assert_eq!(outcome.block_reason.as_deref(), Some("tests failed"));
        assert_eq!(outcome.additional_context, None);
        assert_eq!(outcome.output_replacement, None);
        let failure = failure.expect("a non-zero exit is recorded even when a field parsed");
        assert!(
            failure.contains("exit code 1") && failure.contains("boom"),
            "failure must carry exit code and stderr, got: {failure}"
        );
    }

    #[test]
    fn truncate_output_respects_limit() {
        assert_eq!(truncate_output(b"hello world"), "hello world");

        let large = truncate_output(&vec![b'x'; MAX_OUTPUT_BYTES + 1000]);
        assert!(large.ends_with(" [truncated]"));
    }

    #[test]
    fn post_tool_use_replacement_at_ceiling_survives_capture_and_parse() {
        let at_ceiling = "x".repeat(MAX_HOOK_OUTPUT_REPLACEMENT_CHARS);
        let document = serde_json::json!({
            "hookSpecificOutput": { "updatedMCPToolOutput": at_ceiling },
        })
        .to_string();

        let captured = truncate_output(document.as_bytes());
        assert!(
            !captured.ends_with(" [truncated]"),
            "a ceiling replacement must fit the capture cap without truncation"
        );
        let (result, _) = parse_post_tool_use_result(&captured, "", 0, "p", Duration::ZERO);
        let outcome = post_tool_use_outcome(result);
        let Some(OutputReplacement {
            kind: ReplacementKind::Mcp,
            value,
            ..
        }) = outcome.output_replacement.as_ref()
        else {
            panic!("the ceiling replacement must survive, not be dropped as an empty success");
        };
        assert_eq!(
            value.as_str().map(str::len),
            Some(MAX_HOOK_OUTPUT_REPLACEMENT_CHARS),
            "the full ceiling-length replacement survives, unclipped"
        );
    }

    #[test]
    fn resolve_command_path_variants() {
        let spec =
            |handler: crate::config::HandlerType, command: Option<&str>, source: &str| HookSpec {
                name: "test".into(),
                event: crate::event::HookEventName::PreToolUse,
                handler_type: handler,
                configured_matcher: None,
                matcher: None,
                enabled: true,
                command: command.map(std::path::PathBuf::from),
                command_raw: command.map(str::to_string),
                url: None,
                url_raw: None,
                timeout_ms: 5000,
                source_dir: std::path::PathBuf::from(source),
                extra_env: std::collections::HashMap::new(),
                layer: crate::config::HookProvenance::File,
            };
        use crate::config::HandlerType;
        assert_eq!(
            resolve_command_path(&spec(
                HandlerType::Command,
                Some("/usr/bin/hook"),
                "/some/dir"
            )),
            Some(std::path::PathBuf::from("/usr/bin/hook"))
        );
        assert_eq!(
            resolve_command_path(&spec(
                HandlerType::Command,
                Some("bin/check.sh"),
                "/project/.grok/hooks"
            )),
            Some(std::path::PathBuf::from(
                "/project/.grok/hooks/bin/check.sh"
            ))
        );
        assert_eq!(
            resolve_command_path(&spec(HandlerType::Http, None, "/project")),
            None
        );
    }

    fn make_shell_spec(command: &str) -> HookSpec {
        HookSpec {
            name: "test-hook".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(command.into()),
            command_raw: Some(command.to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: std::env::temp_dir(),
            extra_env: std::collections::HashMap::new(),
            layer: crate::config::HookProvenance::File,
        }
    }

    fn make_envelope() -> HookEventEnvelope {
        use crate::event::HookPayload;
        HookEventEnvelope {
            hook_event_name: crate::event::HookEventName::Stop,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::Stop {
                reason: "test".into(),
                stop_hook_active: false,
                last_assistant_message: None,
                background_tasks: None,
                session_crons: None,
            },
        }
    }

    fn make_ctx() -> RunContext<'static> {
        RunContext {
            session_id: "test-session",
            workspace_root: "/tmp",
            process_scope: None,
            disabled: Default::default(),
        }
    }

    fn make_scoped_ctx(scope: distill_tools::util::ProcessScope) -> RunContext<'static> {
        RunContext {
            process_scope: Some(scope),
            ..make_ctx()
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn hook_times_out() {
        let mut spec = make_shell_spec("sleep 5");
        spec.timeout_ms = 100;
        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;
        assert!(
            matches!(&result, HookRunnerResult::Failed(msg) if msg.contains("timed out")),
            "expected a timeout failure, got {result:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn large_envelope_with_unreading_hook_does_not_deadlock() {
        use crate::event::HookPayload;
        let spec = make_shell_spec("head -c 200000 /dev/zero | tr '\\0' x");
        let mut envelope = make_envelope();
        envelope.payload = HookPayload::Stop {
            reason: "test".into(),
            stop_hook_active: false,
            last_assistant_message: Some("x".repeat(256 * 1024)),
            background_tasks: None,
            session_crons: None,
        };
        let ctx = make_ctx();
        let run = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe);
        let (result, _, _) = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("hook must not deadlock on a large envelope");
        assert!(matches!(result, HookRunnerResult::Success));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn hook_child_cannot_open_dev_tty() {
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .is_err()
        {
            eprintln!("skipping: no controlling terminal");
            return;
        }

        let spec = make_shell_spec("exec 3>/dev/tty 2>/dev/null && exit 1 || exit 0");
        let envelope = make_envelope();
        let ctx = make_ctx();

        let (result, _duration, _) =
            run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook child should not be able to open /dev/tty after setsid(), got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn project_dir_env_is_exported_to_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("hook.sh");
        let workspace = tmp.path().to_string_lossy().into_owned();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntest \"${{CLAUDE_PROJECT_DIR}}\" = \"{workspace}\"\n",
                workspace = workspace
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let mut spec = make_shell_spec("${CLAUDE_PROJECT_DIR}/hook.sh");
        spec.source_dir = tmp.path().to_path_buf();

        let envelope = make_envelope();
        let ctx = RunContext {
            session_id: "test-session",
            workspace_root: &workspace,
            process_scope: None,
            disabled: Default::default(),
        };
        let (result, _, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook should see CLAUDE_PROJECT_DIR set to the workspace root, got {:?}",
            result
        );
    }

    #[test]
    fn powershell_rewrite_cases() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("PLUGIN_ROOT".to_string(), "/unused".to_string());
        let cases = [
            (
                r#"powershell -File "$CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1" ${PLUGIN_ROOT}"#,
                r#"powershell -File "$env:CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1" "${env:PLUGIN_ROOT}""#,
            ),
            (
                r#"$UNKNOWN ${CLAUDE_PROJECT_DIR:-.}/x bash -c '$CLAUDE_PROJECT_DIR/x.sh' `$CLAUDE_PROJECT_DIR"#,
                r#"$UNKNOWN ${CLAUDE_PROJECT_DIR:-.}/x bash -c '$CLAUDE_PROJECT_DIR/x.sh' `$CLAUDE_PROJECT_DIR"#,
            ),
            (
                "$CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1",
                r#"& "$env:CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1""#,
            ),
            (
                "$CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1; echo done",
                r#"& "$env:CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1"; echo done"#,
            ),
            (
                r#""$CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1""#,
                r#"& "$env:CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1""#,
            ),
            (
                r#"powershell -File $CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1"#,
                r#"powershell -File "$env:CLAUDE_PROJECT_DIR/.claude/hooks/foo.ps1""#,
            ),
            (
                "$CLAUDE_PROJECT_DIR/$GROK_HOOK_NAME.ps1",
                r#"& "$env:CLAUDE_PROJECT_DIR/$env:GROK_HOOK_NAME.ps1""#,
            ),
            (
                "Join-Path ($CLAUDE_PROJECT_DIR) hooks",
                r#"Join-Path ("$env:CLAUDE_PROJECT_DIR") hooks"#,
            ),
            (
                r#"Write-Host "don't skip $CLAUDE_PROJECT_DIR""#,
                r#"Write-Host "don't skip $env:CLAUDE_PROJECT_DIR""#,
            ),
        ];
        for (input, want) in cases {
            assert_eq!(
                rewrite_posix_env_refs_for_powershell(input, &extra).as_ref(),
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn find_unresolved_detects_and_dedups() {
        let mut env = std::collections::HashMap::new();
        env.insert("KNOWN".to_string(), "x".to_string());
        assert_eq!(
            find_unresolved_env_vars("${KNOWN}/${SOME_GB1183_UNSET_VAR}/foo", &env),
            vec!["SOME_GB1183_UNSET_VAR".to_string()]
        );
        assert_eq!(
            find_unresolved_env_vars("$SOME_GB1183_BARE_UNSET/foo", &env),
            vec!["SOME_GB1183_BARE_UNSET".to_string()]
        );
        assert_eq!(
            find_unresolved_env_vars(
                "${MISSING_GB1183_DUP} && ${MISSING_GB1183_DUP}/foo $MISSING_GB1183_DUP",
                &env,
            ),
            vec!["MISSING_GB1183_DUP".to_string()]
        );
    }

    #[test]
    fn find_unresolved_skips_resolvable_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("CLAUDE_PLUGIN_ROOT".to_string(), "/plugins/foo".to_string());
        let v = find_unresolved_env_vars(
            "${GROK_HOOK_EVENT}/${CLAUDE_PROJECT_DIR}/${GROK_SESSION_ID}/${CLAUDE_PLUGIN_ROOT}/foo",
            &env,
        );
        assert!(
            v.is_empty(),
            "resolvable vars should not be flagged, got {v:?}"
        );
    }

    #[test]
    fn find_unresolved_skips_non_var_dollars() {
        let env = std::collections::HashMap::new();
        let v = find_unresolved_env_vars("echo $1 $$ $? $# $(date)", &env);
        assert!(
            v.is_empty(),
            "shell special params should not be flagged, got {v:?}"
        );
    }

    #[test]
    fn find_unresolved_skips_local_assignments() {
        let env = std::collections::HashMap::new();
        for cmd in [
            r#"INPUT=$(cat); echo "$INPUT" | grep -q foo"#,
            "read -r LINE; echo $LINE",
            "echo first; X=hello && echo $X | cat",
        ] {
            let v = find_unresolved_env_vars(cmd, &env);
            assert!(v.is_empty(), "`{cmd}` should not flag any var, got {v:?}");
        }
    }

    #[test]
    fn find_unresolved_skips_parameter_expansion_modifiers() {
        let env = std::collections::HashMap::new();
        let cases = [
            "${MISSING_GB1183_MOD:-/default/path.sh}",
            "${MISSING_GB1183_MOD-/default/path.sh}",
            "${MISSING_GB1183_MOD:=/assigned/path.sh}",
            "${MISSING_GB1183_MOD:?msg here}",
            "${MISSING_GB1183_MOD:+/used/if/set.sh}",
            "${MISSING_GB1183_MOD%.sh}",
            "${MISSING_GB1183_MOD#prefix/}",
            "${MISSING_GB1183_MOD/foo/bar}",
            "${MISSING_GB1183_MOD:0:5}",
        ];
        for case in cases {
            let v = find_unresolved_env_vars(case, &env);
            assert!(
                v.is_empty(),
                "parameter-expansion form `{case}` should not be flagged, got {v:?}"
            );
        }
    }

    #[tokio::test]
    async fn undefined_env_var_refuses_to_spawn() {
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("UNRELATED_GB1183".to_string(), "/tmp".to_string());

        let spec = HookSpec {
            name: "test-undef".into(),
            event: crate::event::HookEventName::Stop,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: Some(std::path::PathBuf::from(
                "${NEVER_SET_GB1183}/does/not/exist.sh",
            )),
            command_raw: Some("${NEVER_SET_GB1183}/does/not/exist.sh".to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: std::env::temp_dir(),
            extra_env,
            layer: crate::config::HookProvenance::File,
        };

        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        match result {
            HookRunnerResult::Failed(reason) => {
                assert!(
                    reason.contains("NEVER_SET_GB1183"),
                    "failure reason should name the undefined env var, got: {reason}"
                );
                assert!(
                    reason.contains("hook not executed"),
                    "failure reason should make clear the hook did not run, got: {reason}"
                );
                assert!(
                    !reason.contains("exit code"),
                    "failure reason should not reference an exit code (we never spawned), got: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn tilde_expansion_runs_via_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_dir = tmp.path().join(".grok-test-hooks-gb856");
        std::fs::create_dir_all(&hook_dir).unwrap();
        let script = hook_dir.join("tilde-test.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert(
            "HOME".to_string(),
            tmp.path().to_string_lossy().into_owned(),
        );

        let mut spec = make_shell_spec("~/.grok-test-hooks-gb856/tilde-test.sh");
        spec.extra_env = extra_env;

        let envelope = make_envelope();
        let ctx = make_ctx();

        let mut result = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe)
            .await
            .0;
        for _ in 0..8 {
            if !matches!(&result, HookRunnerResult::Failed(msg) if msg.starts_with("exit code 126"))
            {
                break;
            }
            result = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe)
                .await
                .0;
        }

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook with ~/... path should be expanded via sh -c, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn parameter_expansion_default_is_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("default.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let mut spec = make_shell_spec(&format!(
            "${{MISSING_GB1183_DEFAULT:-{}}}",
            script.display()
        ));
        spec.source_dir = tmp.path().to_path_buf();

        let envelope = make_envelope();
        let ctx = make_ctx();
        let (result, _, _) = run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await;

        assert!(
            matches!(result, HookRunnerResult::Success),
            "hook with parameter-expansion default must run, got {:?}",
            result
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_session_close_reaps_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild_alive");
        let mut spec = make_shell_spec(&format!(
            "sh -c 'sleep 2 && echo alive > {}' & wait",
            marker.display()
        ));
        spec.timeout_ms = 60_000;
        let envelope = make_envelope();
        let scope = distill_tools::util::ProcessScope::new();
        let hook_scope = scope.clone();
        let hook = tokio::spawn(async move {
            run_command_hook(
                &spec,
                &envelope,
                &make_scoped_ctx(hook_scope),
                GateKind::Observe,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(800)).await;
        scope.kill_all();

        tokio::time::timeout(Duration::from_secs(15), hook)
            .await
            .expect("kill_all must reap the enrolled hook, not leave it on its 60s timeout")
            .expect("hook task join");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived session close, so the group was not killpg'd"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_command_hook_kills_and_reaps_grandchild() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild_alive");
        let child_pid = tmp.path().join("child.pid");
        let mut spec = make_shell_spec(&format!(
            "sh -c 'echo $$ > \"{}\"; sleep 30; echo alive > \"{}\"' & wait",
            child_pid.display(),
            marker.display()
        ));
        spec.timeout_ms = 60_000;
        let envelope = make_envelope();
        let ctx = make_ctx();
        let hook = tokio::spawn(async move {
            run_command_hook(&spec, &envelope, &ctx, GateKind::Observe).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if child_pid.exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("grandchild pid");
        hook.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), hook).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!marker.exists(), "grandchild wrote after hook drop");
        let pid: u32 = std::fs::read_to_string(&child_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "grandchild {pid} still live after hook drop"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_fails_fast_when_scope_already_closed() {
        let scope = distill_tools::util::ProcessScope::new();
        scope.kill_all();
        let mut spec = make_shell_spec("sleep 600");
        spec.timeout_ms = 60_000;

        let (result, _, _) = tokio::time::timeout(
            Duration::from_secs(15),
            run_command_hook(
                &spec,
                &make_envelope(),
                &make_scoped_ctx(scope),
                GateKind::Observe,
            ),
        )
        .await
        .expect("a closed scope must fail the hook immediately, not run to its 60s timeout");

        assert!(
            matches!(result, HookRunnerResult::Failed(_)),
            "got {result:?}"
        );
    }
}
