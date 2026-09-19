// Modified for Distill by Samuel Fajreldines, 2026.
//! Actor-based terminal backend for foreground and background execution.
//! `LocalTerminalBackend` is a channel handle; `LocalTerminalActor` runs in a
//! spawned task and owns all mutable state, so no locks are needed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::computer::local::cgroup::{
    CgroupGuard, CgroupMemoryConfig, MemoryMonitor, PROCESS_OOM_EXIT_CODE,
};
use crate::computer::task_log;
use crate::computer::types::{
    BackgroundHandle, BackgroundedForeground, ComputerError, KillOutcome, KillSource, TaskSnapshot,
    TerminalBackend, TerminalRunRequest, TerminalRunResult,
};
use crate::notification::types::{BashNotificationBase, BashOutputChunk, ToolNotificationHandle};
use crate::util::truncate::FRONT_BACK_TRUNCATION_MARKER;

use super::SearchShadowConfig;
#[cfg(unix)]
use super::shell_state;

struct SpawnResult {
    child: tokio::process::Child,
    process_group: crate::util::ProcessGroup,
    /// Handle for reading the state dump from fd 4 (persistent shell only).
    state_dump_handle: Option<tokio::task::JoinHandle<std::io::Result<String>>>,
}

const READ_BUFFER_SIZE: usize = 8192;
const DEFAULT_NOTIFICATION_INTERVAL_MS: u64 = 100;
const COMMAND_CHANNEL_SIZE: usize = 32;
/// Eviction delay for completed tasks; the on-disk output file persists for the session.
const COMPLETED_TASK_TTL: Duration = Duration::from_secs(300);
/// SIGTERM → SIGKILL grace period.
const SIGTERM_GRACE: Duration = Duration::from_secs(1);
/// Max background task lifetime; 10 hours to support long monitor and bash runs.
pub(crate) const BACKGROUND_MAX_RUNTIME: Duration = Duration::from_secs(36_000);
/// Max time an auto-backgroundable foreground command blocks the turn before it is
/// backgrounded (never killed), independent of `timeout`. Env: `GROK_FOREGROUND_BLOCK_BUDGET_MS`.
pub(crate) const FOREGROUND_BLOCK_BUDGET: Duration = Duration::from_secs(15);

pub(crate) fn foreground_block_budget_from_env() -> Duration {
    std::env::var("GROK_FOREGROUND_BLOCK_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(FOREGROUND_BLOCK_BUDGET)
}

/// Output-file size at which the actor kills the command, stopping an unbounded
/// writer from filling the disk. Env override: `GROK_MAX_OUTPUT_FILE_BYTES`.
const MAX_OUTPUT_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

fn output_file_cap_from_env() -> u64 {
    std::env::var("GROK_MAX_OUTPUT_FILE_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(MAX_OUTPUT_FILE_BYTES)
}
/// Post-exit drain cap: an inherited pipe (`cmd &`, no redirect) would
/// otherwise block the actor loop forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a kill waits for the reap before taking the output there is: a
/// process that never dies must not hold its task open forever.
const REAP_GRACE: Duration = Duration::from_secs(5);
/// Post-exit output-file retention cap so snapshots don't materialize huge strings.
const MAX_RETAINED_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Tombstones are metadata-only, so 100 entries is ~10 KB.
const MAX_COMPLETED_TASK_SNAPSHOTS: usize = 100;

fn notification_interval() -> Duration {
    Duration::from_millis(DEFAULT_NOTIFICATION_INTERVAL_MS)
}

struct ActorSettings {
    completed_task_ttl: Duration,
    foreground_block_budget: Duration,
    output_file_cap: u64,
    tick_interval: Duration,
}

impl Default for ActorSettings {
    fn default() -> Self {
        Self {
            completed_task_ttl: COMPLETED_TASK_TTL,
            foreground_block_budget: FOREGROUND_BLOCK_BUDGET,
            output_file_cap: MAX_OUTPUT_FILE_BYTES,
            tick_interval: notification_interval(),
        }
    }
}

impl ActorSettings {
    fn from_env() -> Self {
        Self {
            foreground_block_budget: foreground_block_budget_from_env(),
            output_file_cap: output_file_cap_from_env(),
            ..Self::default()
        }
    }
}

/// Wakes the sweep when a child of this actor exits. Unix listens to the
/// shared SIGCHLD; Windows registers one handle wait per spawned child.
struct ChildExitWake {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
    #[cfg(not(unix))]
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl ChildExitWake {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok(),
            #[cfg(not(unix))]
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        match &mut self.signal {
            Some(signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
        #[cfg(not(unix))]
        self.notify.notified().await;
    }

    /// Unix needs no per-child registration; the signal covers every child.
    #[cfg(unix)]
    fn watch(&self, _pid: u32) {}

    #[cfg(not(unix))]
    fn watch(&self, pid: u32) {
        let notify = std::sync::Arc::clone(&self.notify);
        tokio::task::spawn_blocking(move || {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Threading::{
                INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
            };
            // SAFETY: the handle is opened by pid at spawn time (the child is
            // alive), used only for a synchronize wait, and closed here; an
            // open failure falls back to the tick as the only trigger.
            unsafe {
                if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                    WaitForSingleObject(handle, INFINITE);
                    let _ = CloseHandle(handle);
                }
            }
            notify.notify_one();
        });
    }
}

/// Sleeps until `deadline`; pends forever when there is none, mirroring
/// [`ChildExitWake::recv`] so the select arm needs no separate guard.
async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

#[path = "lifecycle.rs"]
mod lifecycle;
use lifecycle::{Collection, Lifecycle};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

enum TerminalCommand {
    /// A detached post-exit drain finished; complete the task with its output.
    DrainedOutput {
        task_id: String,
        output: Vec<u8>,
        status: ExitStatus,
    },

    Run {
        request: TerminalRunRequest,
        reply: oneshot::Sender<Result<TerminalRunResult, ComputerError>>,
    },

    RunBackground {
        request: TerminalRunRequest,
        reply: oneshot::Sender<Result<BackgroundHandle, ComputerError>>,
    },

    GetTask {
        task_id: String,
        reply: oneshot::Sender<Option<TaskSnapshot>>,
    },

    Kill {
        task_id: String,
        source: KillSource,
        reply: oneshot::Sender<KillOutcome>,
    },

    /// Sent on turn cancellation.
    KillForegroundCommands,

    /// Unblocks the foreground waiter with signal="backgrounded".
    BackgroundForeground {
        tool_call_id: String,
        reply: oneshot::Sender<bool>,
    },

    /// Sent on a mid-turn redirect: in-flight commands are kept alive instead of SIGKILLed.
    BackgroundForegroundCommands {
        owner_session_id: Option<String>,
        reply: oneshot::Sender<Vec<BackgroundedForeground>>,
    },

    WaitForCompletion {
        task_id: String,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Option<TaskSnapshot>>,
    },

    ListTasks {
        include_output: bool,
        reply: oneshot::Sender<Vec<TaskSnapshot>>,
    },

    GetShellCwd {
        reply: oneshot::Sender<Option<PathBuf>>,
    },

    WarmShell {
        cwd: PathBuf,
    },

    KillForegroundCommandsByOwner {
        owner_session_id: String,
    },

    KillTasksByOwner {
        owner_session_id: String,
        reply: oneshot::Sender<()>,
    },

    /// Reroute surviving tasks' notifications to the parent session; monitor
    /// pipelines are re-spawned so their events keep streaming.
    ReparentNotifications {
        old_owner_session_id: String,
        new_owner_session_id: String,
        new_handle: crate::notification::types::ToolNotificationHandle,
        /// Weak so a reparented monitor doesn't pin the backend.
        backend_weak: std::sync::Weak<dyn crate::computer::types::TerminalBackend>,
        reply: oneshot::Sender<()>,
    },
}

// ============================================================================
// Per-process state (for each running command)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundStatus {
    Foreground { auto_bg_on_timeout: bool },
    Backgrounded { reason: BackgroundReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundReason {
    /// Model requested `is_background=true`.
    Explicit,
    /// User pressed Ctrl+G.
    UserSignal,
    /// Foreground command exceeded default timeout.
    ForegroundTimeout,
}

impl BackgroundStatus {
    fn is_backgrounded(self) -> bool {
        matches!(self, Self::Backgrounded { .. })
    }
}

impl BackgroundReason {
    fn as_signal(&self) -> &'static str {
        match self {
            Self::Explicit | Self::UserSignal => "backgrounded",
            Self::ForegroundTimeout => "auto_backgrounded",
        }
    }
}

struct ProcessState {
    child: tokio::process::Child,
    /// Unix: dropped to `None` at reap — kept through the completed-task TTL,
    /// `kill_all` could `killpg` a recycled pid. Windows JobObjects have no such hazard.
    process_group: Option<std::sync::Arc<crate::util::ProcessGroup>>,
    /// Tail of the output; the front is frozen in `front_buffer` once truncation fires.
    output_buffer: Vec<u8>,
    /// First half of the char budget, frozen at first truncation.
    front_buffer: Option<Vec<u8>>,
    truncated: bool,
    /// Monotonic byte count, unaffected by buffer truncation.
    total_bytes: usize,
    lifecycle: Lifecycle,

    /// A detached drain owns the pipes; sweeps skip the task until it lands.
    draining: bool,
    bg_status: BackgroundStatus,
    /// Foreground only.
    completion_waiters: Vec<oneshot::Sender<Result<TerminalRunResult, ComputerError>>>,
    output_byte_limit: usize,
    timeout: Duration,
    /// Max FG block before auto-bg when `auto_bg_on_timeout` is set.
    foreground_block_budget: Duration,
    start_time: Instant,
    output_file: PathBuf,
    file_handle: Option<File>,
    /// May be isolation-wrapped; `display_command` keeps the original for display.
    command: String,
    display_command: Option<String>,
    cwd: String,
    start_wall_time: std::time::SystemTime,
    end_wall_time: Option<std::time::SystemTime>,

    notification_handle: ToolNotificationHandle,
    tool_call_id: String,
    kind: crate::computer::types::TaskKind,
    /// Chunk gate keyed off monotonic `total_bytes`, not `output_buffer.len()`:
    /// the truncated tail shrinks, so a length gate would go (and stay) false.
    last_notified_total: usize,
    /// Set when a `block=true` waiter consumed this task's result.
    block_waited: bool,
    /// Kill tool, UI, or teardown — not a natural exit.
    explicitly_killed: bool,
    kill_result_delivered: bool,

    /// fd-4 state dump reader (persistent shell only); collected on exit to
    /// update the canonical `ShellState`.
    state_dump_handle: Option<tokio::task::JoinHandle<std::io::Result<String>>>,

    /// Scopes kill operations so subagent teardown only kills its own tasks.
    owner_session_id: Option<String>,
    description: Option<String>,
}

impl ProcessState {
    fn to_result(&self) -> TerminalRunResult {
        TerminalRunResult {
            combined_output: self.ring_output(),
            exit_code: self.lifecycle.exit_status().and_then(|s| s.exit_code),
            truncated: self.truncated,
            signal: match self.bg_status {
                BackgroundStatus::Backgrounded { reason } => Some(reason.as_signal().to_string()),
                _ => self.lifecycle.exit_status().and_then(|s| s.signal.clone()),
            },
            timed_out: self
                .lifecycle
                .exit_status()
                .map(|s| s.signal.as_deref() == Some("timeout"))
                .unwrap_or(false),
            output_file: self.output_file.clone(),
            total_bytes: self.total_bytes,
            pid: self.child.id(),
        }
    }

    fn notify_waiters(&mut self, result: Result<TerminalRunResult, ComputerError>) {
        for waiter in self.completion_waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
    }

    /// Front-and-back truncation by char count (not bytes); `to_result` re-joins the halves.
    fn maybe_truncate(&mut self) {
        let s = String::from_utf8_lossy(&self.output_buffer);
        let char_count = s.chars().count();
        if char_count <= self.output_byte_limit {
            return;
        }
        let half = self.output_byte_limit / 2;

        if self.front_buffer.is_none() {
            let front_end = s
                .char_indices()
                .nth(half)
                .map(|(i, _)| i)
                .unwrap_or(s.len());
            self.front_buffer = Some(s.get(..front_end).unwrap_or("").as_bytes().to_vec());
        }

        let tail_start_char = char_count.saturating_sub(half);
        let tail_start_byte = s
            .char_indices()
            .nth(tail_start_char)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        self.output_buffer = s.get(tail_start_byte..).unwrap_or("").as_bytes().to_vec();
        self.truncated = true;
    }

    async fn flush_and_truncate_output_file(&mut self) {
        if let Some(ref mut file) = self.file_handle {
            let _ = file.flush().await;
            if self.total_bytes as u64 > MAX_RETAINED_OUTPUT_FILE_BYTES {
                let _ = file.set_len(MAX_RETAINED_OUTPUT_FILE_BYTES).await;
                // Seek to new end so post-exit drain appends correctly.
                let _ = file.seek(std::io::SeekFrom::End(0)).await;
            }
        }
    }

    fn is_timed_out(&self) -> bool {
        self.start_time.elapsed() > self.timeout
    }

    fn is_complete(&self) -> bool {
        self.lifecycle.is_complete()
    }

    fn is_running(&self) -> bool {
        !self.lifecycle.has_exited() && !self.draining
    }

    /// The output is not final until `finish_output`.
    fn mark_exited(&mut self, status: ExitStatus) {
        if !self.lifecycle.has_exited() {
            self.lifecycle = Lifecycle::Exiting {
                status,
                since: Instant::now(),
            };
        }
    }

    fn finish_output(&mut self, collection: Collection) {
        self.lifecycle.finish_output(collection);
    }

    async fn to_task_snapshot(&self, task_id: &str) -> TaskSnapshot {
        let swept = matches!(self.lifecycle, Lifecycle::Swept { .. });
        let (output, short_of_full_log) = if swept && !self.output_file.as_os_str().is_empty() {
            task_log::read_prefix(&self.output_file, task_log::MAX_SNAPSHOT_BYTES).await
        } else {
            (self.ring_output(), false)
        };

        TaskSnapshot {
            task_id: task_id.to_string(),
            command: self.command.clone(),
            display_command: self.display_command.clone(),
            cwd: self.cwd.clone(),
            start_time: self.start_wall_time,
            end_time: if self.lifecycle.has_exited() {
                Some(
                    self.end_wall_time
                        .unwrap_or_else(std::time::SystemTime::now),
                )
            } else {
                None
            },
            output,
            output_file: self.output_file.clone(),
            truncated: self.truncated || short_of_full_log,
            output_total_bytes: self.total_bytes,
            exit_code: self.lifecycle.exit_status().and_then(|s| s.exit_code),
            signal: self.lifecycle.exit_status().and_then(|s| s.signal.clone()),
            completed: self.is_complete(),
            block_waited: self.block_waited,
            explicitly_killed: self.explicitly_killed,
            kill_result_delivered: self.kill_result_delivered,
            kind: self.kind,
            owner_session_id: self.owner_session_id.clone(),
            description: self.description.clone(),
            is_backgrounded: self.bg_status.is_backgrounded(),
        }
    }

    /// Metadata-only row: no log reads, empty stdout.
    fn to_task_snapshot_metadata(&self, task_id: &str) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.to_string(),
            command: self.command.clone(),
            display_command: self.display_command.clone(),
            cwd: self.cwd.clone(),
            start_time: self.start_wall_time,
            end_time: if self.lifecycle.has_exited() {
                Some(
                    self.end_wall_time
                        .unwrap_or_else(std::time::SystemTime::now),
                )
            } else {
                None
            },
            output: String::new(),
            output_file: self.output_file.clone(),
            truncated: false,
            output_total_bytes: 0,
            exit_code: self.lifecycle.exit_status().and_then(|s| s.exit_code),
            signal: self.lifecycle.exit_status().and_then(|s| s.signal.clone()),
            completed: self.is_complete(),
            block_waited: self.block_waited,
            explicitly_killed: self.explicitly_killed,
            kill_result_delivered: self.kill_result_delivered,
            kind: self.kind,
            owner_session_id: self.owner_session_id.clone(),
            description: self.description.clone(),
            is_backgrounded: self.bg_status.is_backgrounded(),
        }
    }

    fn ring_output(&self) -> String {
        match self.front_buffer.as_ref() {
            Some(front) => format!(
                "{}{FRONT_BACK_TRUNCATION_MARKER}{}",
                String::from_utf8_lossy(front).trim_end(),
                String::from_utf8_lossy(&self.output_buffer).trim_start()
            ),
            None => String::from_utf8_lossy(&self.output_buffer).into_owned(),
        }
    }
}

// ============================================================================
// Actor
// ============================================================================

/// Stored instead of blocking the actor loop; the sweep fires it on child
/// exit, at its deadline, or on the safety tick, whichever comes first.
struct CompletionWaiter {
    reply: oneshot::Sender<Option<TaskSnapshot>>,
    deadline: Instant,
}

struct LocalTerminalActor {
    cmd_rx: mpsc::Receiver<TerminalCommand>,

    cancel_token: CancellationToken,

    /// Spawned children enroll here so the TUI exit paths can `kill_all()`
    /// setsid-detached trees. Tests inject their own to avoid latching the global.
    scope: crate::util::ProcessScope,

    /// Owning session's scope, enrolled additionally so closing the session reaps
    /// its commands; whichever reaper fires first wins, the other finds a dead group.
    session_scope: Option<crate::util::ProcessScope>,

    processes: HashMap<String, ProcessState>,

    child_exit: ChildExitWake,

    self_tx: mpsc::WeakSender<TerminalCommand>,

    completion_waiters: HashMap<String, Vec<CompletionWaiter>>,

    /// Metadata-only tombstones so `get_task` still answers after the process
    /// eviction TTL; retained for the session lifetime.
    completed_task_snapshots: HashMap<String, TaskSnapshot>,

    completed_task_ttl: Duration,

    /// On the actor so tests can shorten it; see [`FOREGROUND_BLOCK_BUDGET`].
    foreground_block_budget: Duration,

    /// On the actor so tests can shrink it; see [`MAX_OUTPUT_FILE_BYTES`].
    output_file_cap: u64,

    /// Poll cadence absent a child-exit or deadline wake; also paces output streaming.
    tick_interval: Duration,

    /// Owns the child cgroup; spawned processes are moved in so their memory is bounded.
    _cgroup_guard: CgroupGuard,

    memory_monitor: MemoryMonitor,

    persistent_shell: bool,

    login_shell_capture: bool,

    /// Baked in at construction, not read from a process-global, so a subagent
    /// reusing this backend can't clobber the parent's search shadows.
    search_shadows: SearchShadowConfig,

    /// Baked in at construction; `None` inherits the full environment.
    shell_env_policy: Option<crate::util::ShellEnvironmentPolicy>,

    /// Lazily initialized on first command when `persistent_shell` is true.
    #[cfg(unix)]
    shell_state: Option<shell_state::ShellState>,

    #[cfg(unix)]
    static_shell: Option<super::static_shell::StaticShellSnapshot>,

    #[cfg(unix)]
    login_env: Option<HashMap<String, String>>,
}

impl LocalTerminalActor {
    fn new(
        cmd_rx: mpsc::Receiver<TerminalCommand>,
        self_tx: mpsc::WeakSender<TerminalCommand>,
        cancel_token: CancellationToken,
        cgroup_guard: CgroupGuard,
        memory_monitor: MemoryMonitor,
        persistent_shell: bool,
        login_shell_capture: bool,
        search_shadows: SearchShadowConfig,
        settings: ActorSettings,
        scope: crate::util::ProcessScope,
        session_scope: Option<crate::util::ProcessScope>,
        shell_env_policy: Option<crate::util::ShellEnvironmentPolicy>,
    ) -> Self {
        let ActorSettings {
            completed_task_ttl,
            foreground_block_budget,
            output_file_cap,
            tick_interval,
        } = settings;
        Self {
            cmd_rx,
            // Weak: a strong sender would keep the channel open and the actor
            // alive after every backend handle is dropped.
            self_tx,
            cancel_token,
            scope,
            session_scope,
            shell_env_policy,
            processes: HashMap::new(),
            child_exit: ChildExitWake::new(),
            completion_waiters: HashMap::new(),
            completed_task_snapshots: HashMap::new(),
            completed_task_ttl,
            foreground_block_budget,
            output_file_cap,
            tick_interval,
            _cgroup_guard: cgroup_guard,
            memory_monitor,
            persistent_shell,
            login_shell_capture,
            search_shadows,
            #[cfg(unix)]
            shell_state: None,
            #[cfg(unix)]
            static_shell: None,
            #[cfg(unix)]
            login_env: None,
        }
    }

    async fn spawn_command(
        &mut self,
        command: &str,
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
    ) -> Result<SpawnResult, ComputerError> {
        #[cfg(unix)]
        if self.persistent_shell {
            return self.spawn_persistent_command(command, cwd, env).await;
        }

        #[cfg(unix)]
        if self.login_shell_capture && login_env_capture_enabled() {
            self.ensure_static_shell_initialized(cwd).await;
            return self.spawn_static_command(command, cwd, env).await;
        }

        #[cfg(unix)]
        if self.login_env.is_none() {
            self.login_env = Some(capture_login_env().await);
        }

        #[cfg(unix)]
        let login_env = self.login_env.as_ref();
        #[cfg(not(unix))]
        let login_env: Option<&HashMap<String, String>> = None;

        let (child, process_group) = spawn_shell_command(
            command,
            cwd,
            env,
            login_env,
            self.search_shadows,
            self.shell_env_policy.as_ref(),
        )?;
        Ok(SpawnResult {
            child,
            process_group,
            state_dump_handle: None,
        })
    }

    #[cfg(unix)]
    async fn ensure_static_shell_initialized(&mut self, cwd: &std::path::Path) {
        if self.static_shell.is_some() && self.login_env.is_some() {
            return;
        }
        let (snapshot, login_env) = tokio::join!(
            async {
                if self.static_shell.is_none() {
                    Some(super::static_shell::StaticShellSnapshot::init(cwd).await)
                } else {
                    None
                }
            },
            async {
                if self.login_env.is_none() {
                    Some(capture_login_env().await)
                } else {
                    None
                }
            }
        );
        if let Some(snapshot) = snapshot {
            self.static_shell = Some(snapshot);
        }
        if let Some(env) = login_env {
            self.login_env = Some(env);
        }
    }

    #[cfg(unix)]
    async fn spawn_static_command(
        &mut self,
        command: &str,
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
    ) -> Result<SpawnResult, ComputerError> {
        use command_fds::CommandFdExt;

        let static_shell = self.static_shell.as_ref().unwrap();
        let prep = static_shell
            .prepare_command(command, self.search_shadows)
            .map_err(|e| ComputerError::io(format!("prepare static command: {e}")))?;

        let mut cmd = tokio::process::Command::new(&prep.binary);
        cmd.args(&prep.args)
            .current_dir(cwd)
            .stdin(distill_tty_utils::null_stdio())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        apply_child_env(
            &mut cmd,
            self.shell_env_policy.as_ref(),
            self.login_env.as_ref(),
            env,
        );

        cmd.fd_mappings(prep.fd_mappings)
            .map_err(|e| ComputerError::io(format!("fd mapping: {e}")))?;

        unsafe {
            cmd.pre_exec(distill_tty_utils::detach_pre_exec_hook());
        }

        distill_sandbox::child_net::restrict_child_network(&mut cmd);

        #[allow(clippy::disallowed_methods)] // attached to a process group below
        let child = cmd.spawn().map_err(|e| {
            ComputerError::io_with_kind(format!("spawn shell in {}: {e}", cwd.display()), e.kind())
        })?;
        drop(cmd);

        let mut process_group = crate::util::ProcessGroup::new()
            .map_err(|e| ComputerError::io(format!("ProcessGroup::new: {e}")))?;
        if let Err(e) = process_group.attach(&child) {
            tracing::debug!("Failed to attach static-shell child to ProcessGroup: {e}");
        }

        let snapshot = static_shell.snapshot.clone();
        tokio::spawn(async move {
            if let Err(e) =
                super::static_shell::write_snapshot_to_pipe(&snapshot, prep.state_in_write).await
            {
                tracing::debug!("failed to write static shell snapshot to pipe: {e}");
            }
        });

        Ok(SpawnResult {
            child,
            process_group,
            state_dump_handle: None,
        })
    }

    #[cfg(unix)]
    async fn ensure_persistent_shell_initialized(&mut self, cwd: &std::path::Path) {
        if self.shell_state.is_some() {
            return;
        }
        let shell = shell_state::ShellKind::detect();
        match shell_state::ShellState::init(shell, cwd, self.shell_env_policy.as_ref()).await {
            Ok(state) => self.shell_state = Some(state),
            Err(e) => {
                tracing::warn!("persistent shell init failed, using empty state: {e}");
                self.shell_state = Some(shell_state::ShellState {
                    cwd: cwd.to_path_buf(),
                    snapshot: String::new(),
                    shell,
                });
            }
        }
    }

    /// Spawn a command with persistent shell state: restore the prior snapshot
    /// via fd 3, run the user command, dump the new state to fd 4.
    #[cfg(unix)]
    async fn spawn_persistent_command(
        &mut self,
        command: &str,
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
    ) -> Result<SpawnResult, ComputerError> {
        use command_fds::CommandFdExt;

        self.ensure_persistent_shell_initialized(cwd).await;

        let shell_state = self.shell_state.as_ref().unwrap();
        let tracked_cwd_alive = match tokio::fs::metadata(&shell_state.cwd).await {
            Ok(m) => m.is_dir(),
            Err(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ),
        };
        let (cwd_override, spawn_notice): (Option<&std::path::Path>, Option<String>) =
            if tracked_cwd_alive {
                (None, None)
            } else {
                tracing::warn!(
                    tracked_cwd = %shell_state.cwd.display(),
                    fallback = %cwd.display(),
                    "persistent shell cwd no longer exists; falling back to request working directory"
                );
                (
                    Some(cwd),
                    Some(format!(
                        "warning: shell working directory {} no longer exists; this command ran in {} instead\n",
                        shell_state.cwd.display(),
                        cwd.display()
                    )),
                )
            };
        let prep = shell_state
            .prepare_command(
                command,
                cwd_override,
                self.search_shadows,
                spawn_notice.as_deref(),
            )
            .map_err(|e| ComputerError::io(format!("prepare persistent command: {e}")))?;

        let mut cmd = tokio::process::Command::new(&prep.binary);
        cmd.args(&prep.args)
            .current_dir(&prep.cwd)
            .stdin(distill_tty_utils::null_stdio())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // The persistent backend restores login state from its snapshot, so no
        // login-env layering here.
        apply_child_env(&mut cmd, self.shell_env_policy.as_ref(), None, env);

        cmd.fd_mappings(prep.fd_mappings)
            .map_err(|e| ComputerError::io(format!("fd mapping: {e}")))?;

        unsafe {
            cmd.pre_exec(distill_tty_utils::detach_pre_exec_hook());
        }

        distill_sandbox::child_net::restrict_child_network(&mut cmd);

        #[allow(clippy::disallowed_methods)] // attached to a process group below
        let child = cmd.spawn().map_err(|e| {
            ComputerError::io_with_kind(
                format!("spawn shell in {}: {e}", prep.cwd.display()),
                e.kind(),
            )
        })?;
        // Releases the FdMapping OwnedFds: otherwise the parent keeps the state-out
        // pipe's write end open and the dump reader never sees EOF.
        drop(cmd);

        let mut process_group = crate::util::ProcessGroup::new()
            .map_err(|e| ComputerError::io(format!("ProcessGroup::new: {e}")))?;
        if let Err(e) = process_group.attach(&child) {
            tracing::debug!("Failed to attach persistent-shell child to ProcessGroup: {e}");
        }

        let snapshot = shell_state.snapshot.clone();
        tokio::spawn(async move {
            if let Err(e) =
                shell_state::write_snapshot_to_pipe(&snapshot, prep.state_in_write).await
            {
                tracing::debug!("failed to write shell snapshot to pipe: {e}");
            }
        });

        let dump_handle =
            tokio::spawn(
                async move { shell_state::read_dump_from_pipe(prep.state_out_read).await },
            );

        Ok(SpawnResult {
            child,
            process_group,
            state_dump_handle: Some(dump_handle),
        })
    }

    /// Earliest deadline across registered completion waiters, so a wait
    /// timeout is honored on time even when the sweep tick is delayed.
    fn next_waiter_deadline(&self) -> Option<Instant> {
        self.completion_waiters
            .values()
            .flatten()
            .map(|waiter| waiter.deadline)
            .min()
    }

    async fn run(mut self) {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let waiter_deadline = self.next_waiter_deadline();
            tokio::select! {
                // Bias commands (cancel, kill) over ticking so kills are handled
                // promptly even when poll_all_processes was slow (drain timeouts).
                biased;

                _ = self.cancel_token.cancelled() => {
                    self.shutdown_all().await;
                    break;
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => {
                            self.shutdown_all().await;
                            break;
                        }
                    }
                }

                // Deadline outranks the exit wake: a due timeout must not queue
                // behind another child's sweep work.
                _ = sleep_until_deadline(waiter_deadline) => {
                    self.poll_all_processes().await;
                }

                // Gated like the ticker: every actor in the process shares SIGCHLD,
                // so idle sessions must not wake on exits of unrelated children.
                _ = self.child_exit.recv(), if !self.processes.is_empty() => {
                    self.poll_all_processes().await;
                }

                // Gated on live processes: one actor per open session/tab must not
                // wake 10x/sec to poll an empty map; the next spawn re-enables the arm.
                _ = ticker.tick(), if !self.processes.is_empty() => {
                    self.poll_all_processes().await;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: TerminalCommand) {
        match cmd {
            TerminalCommand::DrainedOutput {
                task_id,
                output,
                status,
            } => {
                let Some(process) = self.processes.get_mut(&task_id) else {
                    return;
                };
                if !output.is_empty() {
                    process.output_buffer.extend_from_slice(&output);
                    process.total_bytes += output.len();
                    if let Some(ref mut file) = process.file_handle {
                        let _ = file.write_all(&output).await;
                    }
                    process.maybe_truncate();
                }
                process.draining = false;
                // An exit recorded first (kill, timeout sweep) wins: keep its
                // status and the reply its waiters already got.
                let already_exited = process.lifecycle.has_exited();
                if !already_exited {
                    process.mark_exited(status);
                    process.end_wall_time = Some(std::time::SystemTime::now());
                }
                process.flush_and_truncate_output_file().await;
                process.finish_output(Collection::of(&process.child));
                // The dump must land in shell state before the reply: the
                // caller's next command spawns from that state.
                self.collect_shell_state_dumps(std::slice::from_ref(&task_id))
                    .await;
                if !already_exited && let Some(process) = self.processes.get_mut(&task_id) {
                    let result = Ok(process.to_result());
                    process.notify_waiters(result);
                }
                // Background waits register in completion_waiters, not the
                // foreground oneshot; deliver them now, not on the next sweep.
                self.notify_completion_waiters().await;
                if !already_exited {
                    self.evict_if_foreground(&task_id);
                }
            }
            TerminalCommand::Run { request, reply } => {
                self.handle_run(request, reply).await;
            }
            TerminalCommand::RunBackground { request, reply } => {
                self.handle_run_background(request, reply).await;
            }
            TerminalCommand::GetTask { task_id, reply } => {
                let snapshot = match self.processes.get(&task_id) {
                    Some(p) => Some(p.to_task_snapshot(&task_id).await),
                    None => self.completed_task_snapshots.get(&task_id).cloned(),
                };
                let _ = reply.send(snapshot);
            }
            TerminalCommand::Kill {
                task_id,
                source,
                reply,
            } => {
                let outcome = self.handle_kill(&task_id, source).await;
                let _ = reply.send(outcome);
            }
            TerminalCommand::WaitForCompletion {
                task_id,
                timeout,
                reply,
            } => {
                self.handle_wait_for_completion(task_id, timeout, reply)
                    .await;
            }
            TerminalCommand::ListTasks {
                include_output,
                reply,
            } => {
                let mut snapshots =
                    Vec::with_capacity(self.processes.len() + self.completed_task_snapshots.len());
                for (id, p) in &self.processes {
                    if include_output {
                        snapshots.push(p.to_task_snapshot(id).await);
                    } else {
                        snapshots.push(p.to_task_snapshot_metadata(id));
                    }
                }
                for snap in self.completed_task_snapshots.values() {
                    if include_output {
                        snapshots.push(snap.clone());
                    } else {
                        let mut meta = snap.clone();
                        meta.output.clear();
                        meta.output_total_bytes = 0;
                        meta.truncated = false;
                        snapshots.push(meta);
                    }
                }
                let _ = reply.send(snapshots);
            }
            TerminalCommand::GetShellCwd { reply } => {
                #[cfg(unix)]
                let cwd = if self.persistent_shell {
                    self.shell_state.as_ref().map(|s| s.cwd.clone())
                } else {
                    None
                };
                #[cfg(not(unix))]
                let cwd = None;
                let _ = reply.send(cwd);
            }
            TerminalCommand::WarmShell { cwd } => {
                #[cfg(unix)]
                if self.persistent_shell {
                    // Cursor's persistent shell initializes lazily on first
                    // command; warming is only for the static capture path.
                } else if self.login_shell_capture && login_env_capture_enabled() {
                    self.ensure_static_shell_initialized(&cwd).await;
                } else if self.login_env.is_none() {
                    self.login_env = Some(capture_login_env().await);
                }
                #[cfg(not(unix))]
                let _ = cwd;
            }
            TerminalCommand::KillForegroundCommands => {
                self.kill_foreground_commands().await;
            }
            TerminalCommand::BackgroundForeground {
                tool_call_id,
                reply,
            } => {
                let found = self.handle_background_foreground(&tool_call_id);
                let _ = reply.send(found);
            }
            TerminalCommand::BackgroundForegroundCommands {
                owner_session_id,
                reply,
            } => {
                let backgrounded =
                    self.background_all_foreground_commands(owner_session_id.as_deref());
                let _ = reply.send(backgrounded);
            }
            TerminalCommand::KillForegroundCommandsByOwner { owner_session_id } => {
                self.kill_foreground_commands_by_owner(&owner_session_id)
                    .await;
            }
            TerminalCommand::KillTasksByOwner {
                owner_session_id,
                reply,
            } => {
                self.kill_tasks_by_owner(&owner_session_id).await;
                let _ = reply.send(());
            }
            TerminalCommand::ReparentNotifications {
                old_owner_session_id,
                new_owner_session_id,
                new_handle,
                backend_weak,
                reply,
            } => {
                self.reparent_notifications(
                    &old_owner_session_id,
                    &new_owner_session_id,
                    new_handle,
                    backend_weak,
                );
                let _ = reply.send(());
            }
        }
    }

    /// Enroll a `Weak` in the scope(s) so TUI exit paths can reap the tree; the
    /// actor keeps the only strong ref, so a clean reap leaves a dead `Weak`.
    fn enroll_spawned(
        &self,
        group: crate::util::ProcessGroup,
    ) -> std::sync::Arc<crate::util::ProcessGroup> {
        let group = std::sync::Arc::new(group);
        self.scope.register(&group);
        if let Some(session_scope) = &self.session_scope {
            // A closed session scope kills the group here, which is the point:
            // a command racing session teardown must not survive it.
            session_scope.register(&group);
        }
        group
    }

    async fn handle_run(
        &mut self,
        request: TerminalRunRequest,
        reply: oneshot::Sender<Result<TerminalRunResult, ComputerError>>,
    ) {
        // Foreground callers never see this id; the reply goes back on the oneshot.
        let internal_id = uuid::Uuid::now_v7().to_string();

        let SpawnResult {
            child,
            process_group,
            state_dump_handle,
        } = match self
            .spawn_command(&request.command, &request.working_directory, &request.env)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };

        if let Some(pid) = child.id()
            && let Err(e) = self._cgroup_guard.add_process(pid).await
        {
            tracing::debug!("Failed to add pid {pid} to cgroup (non-fatal): {e}");
        }

        let file_handle = match open_output_file(&request.output_file).await {
            Ok(file) => Some(file),
            Err(e) => {
                tracing::warn!(
                    "Failed to open output file {}: {}",
                    request.output_file.display(),
                    e
                );
                None
            }
        };

        let process_state = ProcessState {
            child,
            process_group: Some(self.enroll_spawned(process_group)),
            output_buffer: Vec::new(),
            front_buffer: None,
            truncated: false,
            total_bytes: 0,
            lifecycle: Lifecycle::Running,
            draining: false,
            bg_status: BackgroundStatus::Foreground {
                auto_bg_on_timeout: request.auto_background_on_timeout,
            },
            completion_waiters: vec![reply],
            output_byte_limit: request.output_byte_limit,
            timeout: request.timeout,
            foreground_block_budget: request
                .foreground_block_budget
                .unwrap_or(self.foreground_block_budget),
            start_time: Instant::now(),
            output_file: request.output_file,
            file_handle,
            command: request.command.clone(),
            display_command: request.display_command.clone(),
            cwd: request.working_directory.display().to_string(),
            start_wall_time: std::time::SystemTime::now(),
            end_wall_time: None,
            notification_handle: request.notification_handle.clone(),
            tool_call_id: request.tool_call_id.clone(),
            kind: request.kind,
            last_notified_total: 0,
            block_waited: false,
            explicitly_killed: false,
            kill_result_delivered: false,
            state_dump_handle,
            owner_session_id: request.owner_session_id.clone(),
            description: request.description.filter(|d| !d.trim().is_empty()),
        };

        // Initial empty notification so the TUI shows the execution timer
        // before any stdout/stderr arrives.
        request
            .notification_handle
            .send_output_chunk(BashOutputChunk {
                base: BashNotificationBase {
                    tool_call_id: request.tool_call_id.clone(),
                    command: request.command.clone(),
                    output: Vec::new(),
                    total_bytes: 0,
                    truncated: false,
                    cwd: request.working_directory.clone(),
                },
            });

        if let Some(pid) = process_state.child.id() {
            self.child_exit.watch(pid);
        }
        self.processes.insert(internal_id, process_state);
    }

    async fn handle_kill(&mut self, terminal_id: &str, source: KillSource) -> KillOutcome {
        let Some(process) = self.processes.get_mut(terminal_id) else {
            return if self.completed_task_snapshots.contains_key(terminal_id) {
                KillOutcome::AlreadyExited
            } else {
                KillOutcome::NotFound
            };
        };

        if process.lifecycle.has_exited() {
            return KillOutcome::AlreadyExited;
        }

        // Must precede the kill signal so the exit watcher's snapshot carries the flag.
        process.explicitly_killed = true;

        let outcome = kill_and_finalize(process).await;

        // Resolve waiters here so wait_for_completion unblocks immediately.
        let mut any_delivered = false;
        if let Some(mut waiters) = self.completion_waiters.remove(terminal_id) {
            let snapshot = match self.processes.get(terminal_id) {
                Some(p) => Some(p.to_task_snapshot(terminal_id).await),
                None => None,
            };
            if let Some(last) = waiters.pop() {
                for waiter in waiters {
                    if waiter.reply.send(snapshot.clone()).is_ok() {
                        any_delivered = true;
                    }
                }
                if last.reply.send(snapshot).is_ok() {
                    any_delivered = true;
                }
            }
        }

        if let Some(process) = self.processes.get_mut(terminal_id) {
            process.kill_result_delivered = source.marks_result_delivered(any_delivered);
            if !any_delivered {
                process.block_waited = false;
            }
        }

        outcome
    }

    async fn handle_run_background(
        &mut self,
        request: TerminalRunRequest,
        reply: oneshot::Sender<Result<BackgroundHandle, ComputerError>>,
    ) {
        // Background commands fork the current shell state but don't update it on exit.
        let SpawnResult {
            child,
            process_group,
            state_dump_handle,
        } = match self
            .spawn_command(&request.command, &request.working_directory, &request.env)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };

        if let Some(pid) = child.id()
            && let Err(e) = self._cgroup_guard.add_process(pid).await
        {
            tracing::debug!("Failed to add pid {pid} to cgroup (non-fatal): {e}");
        }

        let file_handle = match open_output_file(&request.output_file).await {
            Ok(file) => Some(file),
            Err(e) => {
                tracing::warn!(
                    "Failed to open output file {}: {}",
                    request.output_file.display(),
                    e
                );
                None
            }
        };

        let task_id = uuid::Uuid::now_v7().to_string();

        let process_state = ProcessState {
            child,
            process_group: Some(self.enroll_spawned(process_group)),
            output_buffer: Vec::new(),
            front_buffer: None,
            truncated: false,
            total_bytes: 0,
            lifecycle: Lifecycle::Running,
            draining: false,
            bg_status: BackgroundStatus::Backgrounded {
                reason: BackgroundReason::Explicit,
            },
            completion_waiters: vec![],
            output_byte_limit: request.output_byte_limit,
            timeout: request.timeout,
            // Unused for already-backgrounded tasks; keep a defined value.
            foreground_block_budget: request
                .foreground_block_budget
                .unwrap_or(self.foreground_block_budget),
            start_time: Instant::now(),
            output_file: request.output_file.clone(),
            file_handle,
            command: request.command.clone(),
            display_command: request.display_command.clone(),
            cwd: request.working_directory.display().to_string(),
            start_wall_time: std::time::SystemTime::now(),
            end_wall_time: None,
            notification_handle: request.notification_handle.clone(),
            tool_call_id: request.tool_call_id.clone(),
            kind: request.kind,
            last_notified_total: 0,
            block_waited: false,
            explicitly_killed: false,
            kill_result_delivered: false,
            // Spawned with the state wrapping so bg commands inherit the session env,
            // but the dump reader is discarded: hours-long tasks must not leak env mutations.
            state_dump_handle: if self.persistent_shell {
                drop(state_dump_handle);
                None
            } else {
                None
            },
            owner_session_id: request.owner_session_id.clone(),
            description: request.description.filter(|d| !d.trim().is_empty()),
        };

        let pid = process_state.child.id();
        if let Some(pid) = pid {
            self.child_exit.watch(pid);
        }
        self.processes.insert(task_id.clone(), process_state);

        let _ = reply.send(Ok(BackgroundHandle {
            task_id,
            output_file: request.output_file,
            pid,
        }));
    }

    /// Register a completion waiter and return immediately.
    async fn handle_wait_for_completion(
        &mut self,
        task_id: String,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Option<TaskSnapshot>>,
    ) {
        let Some(process) = self.processes.get_mut(&task_id) else {
            // Imprint block_waited on the tombstone in place, but only when the reply
            // is delivered: a dropped receiver (cancelled turn) means the model never saw it.
            let snapshot = self.completed_task_snapshots.get(&task_id).map(|s| {
                let mut s = s.clone();
                s.block_waited = true;
                s
            });
            let found = snapshot.is_some();
            let delivered = reply.send(snapshot).is_ok();
            if found
                && delivered
                && let Some(s) = self.completed_task_snapshots.get_mut(&task_id)
            {
                s.block_waited = true;
            }
            return;
        };

        // block_waited makes the notification bridge skip auto-wake; cleared again
        // in `poll_all_processes` (steps 1-2) if the waiter is cancelled undelivered.
        let prev_block_waited = process.block_waited;
        process.block_waited = true;

        if process.is_complete() {
            let snapshot = process.to_task_snapshot(&task_id).await;
            if reply.send(Some(snapshot)).is_err() {
                // Dropped receiver: the model never saw the result; don't suppress auto-wake.
                process.block_waited = prev_block_waited;
            }
            return;
        }

        let timeout = timeout.unwrap_or(Duration::from_secs(30));
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        self.completion_waiters
            .entry(task_id)
            .or_default()
            .push(CompletionWaiter { reply, deadline });
    }

    async fn poll_all_processes(&mut self) {
        self.expire_timed_out_waiters().await;
        self.kill_newest_on_memory_breach().await;
        self.kill_expired_background_tasks().await;
        let task_ids: Vec<String> = self.processes.keys().cloned().collect();

        for task_id in &task_ids {
            self.poll_process(task_id).await;
        }

        // Drop the strong `Arc` at reap so the scope's `Weak` dies (see
        // `ProcessState::process_group`); after the loop so every reap path is caught.
        #[cfg(unix)]
        for process in self.processes.values_mut() {
            if process.process_group.is_some() && process.child.id().is_none() {
                process.process_group = None;
            }
        }

        self.collect_shell_state_dumps(&task_ids).await;
        self.notify_completion_waiters().await;
        self.sweep_finished_background_tasks().await;
        self.evict_exited_processes().await;
    }

    async fn kill_newest_on_memory_breach(&mut self) {
        if let Some(event) = self.memory_monitor.try_recv() {
            tracing::warn!(
                memory_current = event.memory_current,
                memory_high = event.memory_high_threshold,
                "Memory high threshold breached — killing newest running process"
            );

            let newest_id = self
                .processes
                .iter()
                .filter(|(_, p)| p.is_running())
                .max_by_key(|(_, p)| p.start_time)
                .map(|(id, _)| id.clone());

            if let Some(id) = newest_id
                && let Some(process) = self.processes.get_mut(&id)
            {
                send_sigkill_to_group(process);
                drain_remaining_output(process).await;
                process.mark_exited(ExitStatus {
                    exit_code: Some(PROCESS_OOM_EXIT_CODE),
                    signal: Some("oom".to_owned()),
                });
                process.end_wall_time = Some(std::time::SystemTime::now());
                process.flush_and_truncate_output_file().await;
                process.finish_output(Collection::of(&process.child));
                let result = Ok(process.to_result());
                process.notify_waiters(result);
            }
        }
    }

    async fn kill_expired_background_tasks(&mut self) {
        let bg_expired: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| {
                p.is_running()
                    && p.bg_status.is_backgrounded()
                    && p.start_time.elapsed() > BACKGROUND_MAX_RUNTIME
            })
            .map(|(id, _)| id.clone())
            .collect();

        for task_id in &bg_expired {
            if let Some(process) = self.processes.get_mut(task_id) {
                tracing::warn!(task_id, "Background task exceeded max runtime, killing");
                // Fire-and-forget SIGTERM; the poll loop escalates to SIGKILL next tick.
                send_sigterm_to_group(process);
                process.mark_exited(ExitStatus {
                    exit_code: None,
                    signal: Some("max_runtime".to_owned()),
                });
                process.end_wall_time = Some(std::time::SystemTime::now());
                process.flush_and_truncate_output_file().await;
            }
        }

        // 0b (size). Kill any running task whose output file passed the cap.
        let output_cap = self.output_file_cap;
        let size_exceeded: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| p.is_running() && p.total_bytes as u64 > output_cap)
            .map(|(id, _)| id.clone())
            .collect();

        for task_id in &size_exceeded {
            if let Some(process) = self.processes.get_mut(task_id) {
                tracing::warn!(
                    task_id,
                    total_bytes = process.total_bytes,
                    cap = output_cap,
                    "Task exceeded output size cap, killing"
                );
                send_sigterm_to_group(process);
                process.mark_exited(ExitStatus {
                    exit_code: None,
                    signal: Some("output_limit".to_owned()),
                });
                process.end_wall_time = Some(std::time::SystemTime::now());
                process.flush_and_truncate_output_file().await;
                // Unlike the bg-only max-runtime sweep this may be a foreground
                // command, so notify waiters now (mirrors OOM).
                let result = Ok(process.to_result());
                process.notify_waiters(result);
            }
        }
    }

    async fn collect_shell_state_dumps(&mut self, task_ids: &[String]) {
        #[cfg(unix)]
        if self.persistent_shell {
            for task_id in task_ids {
                let handle = {
                    let Some(process) = self.processes.get_mut(task_id) else {
                        continue;
                    };
                    // Only foreground processes update the canonical state.
                    if !process.lifecycle.has_exited() || process.bg_status.is_backgrounded() {
                        continue;
                    }
                    process.state_dump_handle.take()
                };
                if let Some(handle) = handle {
                    match handle.await {
                        Ok(Ok(dump)) => {
                            if let Some(ref mut state) = self.shell_state {
                                state.update_from_dump(&dump);
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::debug!("failed to read shell state dump: {e}");
                        }
                        Err(e) => {
                            tracing::debug!("shell state dump task panicked: {e}");
                        }
                    }
                }
            }
        }
    }

    async fn notify_completion_waiters(&mut self) {
        let waiter_task_ids: Vec<String> = self.completion_waiters.keys().cloned().collect();
        for task_id in waiter_task_ids {
            let completed = self
                .processes
                .get(&task_id)
                .map(ProcessState::is_complete)
                .unwrap_or(true); // process gone = treat as completed

            if completed && let Some(waiters) = self.completion_waiters.remove(&task_id) {
                let snapshot = match self.processes.get(&task_id) {
                    Some(p) => Some(p.to_task_snapshot(&task_id).await),
                    None => None,
                };
                let mut any_delivered = false;
                for waiter in waiters {
                    if waiter.reply.send(snapshot.clone()).is_ok() {
                        any_delivered = true;
                    }
                }
                // Every receiver dropped (turns cancelled): clear block_waited so the
                // auto-wake fires; must run before step 3 snapshots the completion.
                if !any_delivered && let Some(process) = self.processes.get_mut(&task_id) {
                    process.block_waited = false;
                }
            }
        }
    }

    async fn expire_timed_out_waiters(&mut self) {
        let now = Instant::now();
        let waiter_keys: Vec<String> = self.completion_waiters.keys().cloned().collect();
        let mut timed_out_tasks: Vec<String> = Vec::new();
        for task_id in waiter_keys {
            // A cheap exit probe, no drains: an exited child's waiters get the
            // completion from its drain instead of a timeout.
            if let Some(p) = self.processes.get_mut(&task_id)
                && !p.lifecycle.has_exited()
                && matches!(p.child.try_wait(), Ok(Some(_)))
            {
                // Push near deadlines out to the drain bound, else a past
                // deadline re-fires the deadline arm every loop iteration.
                if let Some(waiters) = self.completion_waiters.get_mut(&task_id) {
                    let drain_bound = now + DRAIN_TIMEOUT;
                    for waiter in waiters {
                        waiter.deadline = waiter.deadline.max(drain_bound);
                    }
                }
                continue;
            }
            let snapshot = match self.processes.get(&task_id) {
                Some(p) => Some(p.to_task_snapshot(&task_id).await),
                None => None,
            };
            if let Some(waiters) = self.completion_waiters.get_mut(&task_id) {
                let mut i = 0;
                while i < waiters.len() {
                    if waiters.get(i).is_some_and(|w| now >= w.deadline) {
                        let waiter = waiters.swap_remove(i);
                        let _ = waiter.reply.send(snapshot.clone());
                        timed_out_tasks.push(task_id.clone());
                    } else {
                        i += 1;
                    }
                }
            }
        }
        self.completion_waiters.retain(|_, v| !v.is_empty());

        // All waiters timed out without the completion: clear block_waited, else
        // auto-wake stays suppressed though the agent never saw the result.
        for task_id in timed_out_tasks {
            if !self.completion_waiters.contains_key(&task_id)
                && let Some(process) = self.processes.get_mut(&task_id)
            {
                process.block_waited = false;
            }
        }
    }

    async fn sweep_finished_background_tasks(&mut self) {
        let mut newly_completed: Vec<String> = Vec::new();
        for (task_id, process) in self.processes.iter_mut() {
            if process.is_complete()
                && process.bg_status.is_backgrounded()
                && process.lifecycle.swept_at().is_none()
            {
                process.lifecycle.sweep();
                if process.end_wall_time.is_none() {
                    process.end_wall_time = Some(std::time::SystemTime::now());
                }
                // The log file has everything, and a drained task adds no more.
                process.output_buffer.clear();
                process.front_buffer = None;
                newly_completed.push(task_id.clone());
            }
        }
        // Completion notifications fire unconditionally: pager UI, persistence, and
        // reservation bookkeeping need them; wait-suppression lives in the bridge.
        for task_id in newly_completed {
            if let Some(process) = self.processes.get(&task_id) {
                let snapshot = process.to_task_snapshot(&task_id).await;
                process.notification_handle.send_task_complete(snapshot);
            }
        }
    }

    fn evict_if_foreground(&mut self, task_id: &str) {
        if self
            .processes
            .get(task_id)
            .is_some_and(|p| !p.bg_status.is_backgrounded())
        {
            self.processes.remove(task_id);
        }
    }

    async fn evict_exited_processes(&mut self) {
        let evict_ids: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| {
                // A mid-drain entry stays: its DrainedOutput resolves by this key.
                if p.draining || !p.lifecycle.has_exited() {
                    return false;
                }
                if !p.bg_status.is_backgrounded() {
                    return true; // foreground already replied
                }
                matches!(p.lifecycle.swept_at(), Some(t) if t.elapsed() >= self.completed_task_ttl)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &evict_ids {
            if let Some(p) = self.processes.get(id)
                && p.bg_status.is_backgrounded()
            {
                // Metadata-only: reading the output into memory here would leak
                // unbounded data for long-running tasks; it stays on disk.
                let snapshot = TaskSnapshot {
                    task_id: id.clone(),
                    command: p.command.clone(),
                    display_command: p.display_command.clone(),
                    cwd: p.cwd.clone(),
                    start_time: p.start_wall_time,
                    end_time: p.end_wall_time,
                    output: String::new(),
                    output_file: p.output_file.clone(),
                    // The output is dropped here; the log file keeps it.
                    truncated: p.truncated || p.total_bytes > 0,
                    exit_code: p.lifecycle.exit_status().and_then(|s| s.exit_code),
                    signal: p.lifecycle.exit_status().and_then(|s| s.signal.clone()),
                    completed: true,
                    kind: p.kind,
                    block_waited: p.block_waited,
                    explicitly_killed: p.explicitly_killed,
                    kill_result_delivered: p.kill_result_delivered,
                    owner_session_id: p.owner_session_id.clone(),
                    description: p.description.clone(),
                    is_backgrounded: true,
                    output_total_bytes: p.total_bytes,
                };
                self.completed_task_snapshots.insert(id.clone(), snapshot);
            }
            self.processes.remove(id);
        }
        while self.completed_task_snapshots.len() > MAX_COMPLETED_TASK_SNAPSHOTS {
            let oldest = self
                .completed_task_snapshots
                .iter()
                .min_by_key(|(_, s)| s.start_time)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.completed_task_snapshots.remove(&id);
            } else {
                break;
            }
        }
    }

    async fn poll_process(&mut self, terminal_id: &str) {
        let Some(process) = self.processes.get_mut(terminal_id) else {
            return;
        };
        if process.draining {
            return;
        }

        // An exited task may still hold a live child. Escalate to SIGKILL if
        // needed, drain the pipes once it dies, and keep trying to collect it.
        if let Some(recorded_status) = process.lifecycle.exit_status().cloned() {
            if process.lifecycle.is_settled() {
                return;
            }
            let waiting_since = match &process.lifecycle {
                Lifecycle::Exiting { since, .. } => Some(*since),
                Lifecycle::Running | Lifecycle::Finished { .. } | Lifecycle::Swept { .. } => None,
            };
            match process.child.try_wait() {
                Ok(None) if process.is_complete() => {
                    // Already abandoned; keep the kill fresh and keep trying to collect.
                    send_sigkill_to_group(process);
                }
                Ok(None) => {
                    send_sigkill_to_group(process);
                    let gave_up = waiting_since.is_some_and(|since| since.elapsed() >= REAP_GRACE);
                    if gave_up {
                        // Not dying: take the output there is so the task can report
                        // completion instead of waiting forever.
                        take_available_output(process).await;
                        process.flush_and_truncate_output_file().await;
                        process.finish_output(Collection::ABANDONED);
                    }
                }
                Ok(Some(_)) | Err(_) => {
                    // Off-actor like fresh exits: the handler keeps the recorded
                    // status and this drain only contributes the output tail.
                    process.draining = true;
                    spawn_detached_drain(
                        self.self_tx.clone(),
                        terminal_id.to_owned(),
                        process.child.stdout.take(),
                        process.child.stderr.take(),
                        recorded_status,
                    );
                }
            }
            return;
        }

        let mut new_bytes: Vec<u8> = Vec::new();

        let mut stdout_eof = false;
        if let Some(stdout) = process.child.stdout.as_mut() {
            loop {
                let mut buf = [0u8; READ_BUFFER_SIZE];
                match try_read_nonblocking(stdout, &mut buf) {
                    Some(Ok(0)) => {
                        stdout_eof = true;
                        break;
                    }
                    Some(Ok(n)) => {
                        if let Some(read) = buf.get(..n) {
                            new_bytes.extend_from_slice(read);
                        }
                    }
                    Some(Err(_)) => {
                        stdout_eof = true;
                        break;
                    }
                    None => break,
                }
            }
        }

        let mut stderr_eof = false;
        if let Some(stderr) = process.child.stderr.as_mut() {
            loop {
                let mut buf = [0u8; READ_BUFFER_SIZE];
                match try_read_nonblocking(stderr, &mut buf) {
                    Some(Ok(0)) => {
                        stderr_eof = true;
                        break;
                    }
                    Some(Ok(n)) => {
                        if let Some(read) = buf.get(..n) {
                            new_bytes.extend_from_slice(read);
                        }
                    }
                    Some(Err(_)) => {
                        stderr_eof = true;
                        break;
                    }
                    None => break,
                }
            }
        }

        // Flush so readers (`read_file` on the output file) see output promptly.
        if !new_bytes.is_empty() {
            process.output_buffer.extend_from_slice(&new_bytes);
            process.total_bytes += new_bytes.len();
            if let Some(ref mut file) = process.file_handle {
                let _ = file.write_all(&new_bytes).await;
                let _ = file.flush().await;
            }
        }

        process.maybe_truncate();

        // Gate keyed off monotonic `total_bytes`; see `last_notified_total`.
        if process.total_bytes > process.last_notified_total {
            process
                .notification_handle
                .send_output_chunk(BashOutputChunk {
                    base: BashNotificationBase {
                        tool_call_id: process.tool_call_id.clone(),
                        command: process.command.clone(),
                        output: process.output_buffer.clone(),
                        total_bytes: process.total_bytes,
                        truncated: process.truncated,
                        cwd: process.cwd.clone().into(),
                    },
                });
            process.last_notified_total = process.total_bytes;
        }

        // Auto-bg budget: a second timer, independent of `timeout`, that only
        // backgrounds, never kills; the `timeout` check below kills when auto-bg is off.
        if !process.lifecycle.has_exited()
            && matches!(
                process.bg_status,
                BackgroundStatus::Foreground {
                    auto_bg_on_timeout: true
                }
            )
            && process.start_time.elapsed() > process.foreground_block_budget
        {
            self.transition_to_background(terminal_id, BackgroundReason::ForegroundTimeout);
            return;
        }

        if process.is_timed_out() && !process.lifecycle.has_exited() {
            if matches!(
                process.bg_status,
                BackgroundStatus::Foreground {
                    auto_bg_on_timeout: true
                }
            ) {
                self.transition_to_background(terminal_id, BackgroundReason::ForegroundTimeout);
                return;
            }

            send_sigterm_to_group(process);
            process.mark_exited(ExitStatus {
                exit_code: None,
                signal: Some("timeout".to_owned()),
            });
            process.end_wall_time = Some(std::time::SystemTime::now());
            process.flush_and_truncate_output_file().await;
            let result = Ok(process.to_result());
            process.notify_waiters(result);
            return;
        }

        let process_done = stdout_eof && stderr_eof;
        // Post-exit drains run off the actor: fast commands can exit before the
        // reads above see their output, but a due deadline elsewhere must not
        // wait out this task's DRAIN_TIMEOUT.
        match process.child.try_wait() {
            Ok(Some(status)) => {
                process.draining = true;
                spawn_detached_drain(
                    self.self_tx.clone(),
                    terminal_id.to_owned(),
                    process.child.stdout.take(),
                    process.child.stderr.take(),
                    extract_exit_status(status),
                );
            }
            Ok(None) if process_done => {
                // Streams closed but the process hasn't exited yet.
            }
            Ok(None) => {}
            // An erroring `try_wait` is no proof the child was collected; keep polling.
            Err(e) => {
                process.draining = true;
                spawn_detached_drain(
                    self.self_tx.clone(),
                    terminal_id.to_owned(),
                    process.child.stdout.take(),
                    process.child.stderr.take(),
                    ExitStatus {
                        exit_code: None,
                        signal: Some(format!("error: {}", e)),
                    },
                );
            }
        }
    }

    async fn shutdown_all(&mut self) {
        for (_, process) in self.processes.iter_mut() {
            send_sigkill_to_group(process);
            // The dump reader's spawn_blocking thread must not outlive the actor.
            if let Some(handle) = process.state_dump_handle.take() {
                handle.abort();
            }
        }
        self.processes.clear();
    }

    /// Shared by auto-timeout and user Ctrl+G. Re-keys the entry to `tool_call_id`.
    fn transition_to_background(&mut self, old_key: &str, reason: BackgroundReason) -> bool {
        // A draining task keeps its key: the in-flight DrainedOutput resolves by
        // this key, and the exit it carries completes the task within DRAIN_TIMEOUT.
        if self.processes.get(old_key).is_none_or(|p| p.draining) {
            return false;
        }
        let Some(mut process) = self.processes.remove(old_key) else {
            return false;
        };
        process.bg_status = BackgroundStatus::Backgrounded { reason };
        process.timeout = BACKGROUND_MAX_RUNTIME;
        let result = Ok(process.to_result());
        process.notify_waiters(result);
        let tool_call_id = process.tool_call_id.clone();

        tracing::info!(
            tool_call_id = %tool_call_id,
            ?reason,
            "Foreground command transitioned to background"
        );
        self.processes.insert(tool_call_id, process);
        true
    }

    /// User Ctrl+G path.
    fn handle_background_foreground(&mut self, tool_call_id: &str) -> bool {
        let internal_id = self
            .processes
            .iter()
            .find(|(_, p)| p.tool_call_id == tool_call_id && !p.bg_status.is_backgrounded())
            .map(|(id, _)| id.clone());

        let Some(internal_id) = internal_id else {
            return false;
        };

        self.transition_to_background(&internal_id, BackgroundReason::UserSignal)
    }

    /// The non-lethal twin of [`Self::kill_foreground_commands`]: on a mid-turn
    /// redirect a running command is kept alive, not SIGKILLed.
    fn background_all_foreground_commands(
        &mut self,
        owner_session_id: Option<&str>,
    ) -> Vec<BackgroundedForeground> {
        // Collect ids first: `transition_to_background` re-keys the map, so no
        // iterator may be held across the mutation.
        let targets: Vec<(String, BackgroundedForeground)> = self
            .processes
            .iter()
            .filter(|(_, p)| {
                !p.bg_status.is_backgrounded()
                    && !p.lifecycle.has_exited()
                    && owner_session_id
                        .is_none_or(|owner| p.owner_session_id.as_deref() == Some(owner))
            })
            .map(|(id, p)| {
                (
                    id.clone(),
                    BackgroundedForeground {
                        tool_call_id: p.tool_call_id.clone(),
                    },
                )
            })
            .collect();

        let mut backgrounded = Vec::with_capacity(targets.len());
        for (internal_id, info) in targets {
            if self.transition_to_background(&internal_id, BackgroundReason::UserSignal) {
                if let Some(process) = self.processes.get(&info.tool_call_id) {
                    Self::announce_backgrounded_command(process);
                }
                backgrounded.push(info);
            }
        }
        backgrounded
    }

    /// The bash tool normally announces backgrounding itself, but its turn was
    /// stopped; a rare duplicate announcement beats the client never hearing it.
    fn announce_backgrounded_command(process: &ProcessState) {
        process.notification_handle.send_backgrounded(
            crate::notification::BashExecutionBackgrounded {
                base: crate::notification::BashNotificationBase {
                    tool_call_id: process.tool_call_id.clone(),
                    command: process
                        .display_command
                        .clone()
                        .unwrap_or_else(|| process.command.clone()),
                    // The shell drops these three before the wire; copying the buffer would be waste.
                    output: Vec::new(),
                    total_bytes: 0,
                    truncated: false,
                    cwd: std::path::PathBuf::from(&process.cwd),
                },
                output_file: process.output_file.clone(),
                task_id: process.tool_call_id.clone(),
                monitor_description: None,
                description: process.description.clone().filter(|d| !d.trim().is_empty()),
            },
        );
    }

    /// Waits (≤5s each) for killed children to exit so the kernel reclaims RSS;
    /// otherwise a rapid OOM → recover → OOM cycle can hit memory.max.
    async fn kill_foreground_commands(&mut self) {
        let fg_ids: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| !p.bg_status.is_backgrounded() && p.is_running())
            .map(|(id, _)| id.clone())
            .collect();

        for id in &fg_ids {
            if let Some(process) = self.processes.get_mut(id) {
                send_sigkill_to_group(process);

                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait())
                        .await;

                // Abort the dump reader: a grandchild that inherited fd 4 and escaped
                // the group keeps the pipe open, hanging the blocking read forever.
                if let Some(handle) = process.state_dump_handle.take() {
                    handle.abort();
                }

                process.mark_exited(ExitStatus {
                    exit_code: None,
                    signal: Some("cancelled".to_owned()),
                });
                process.end_wall_time = Some(std::time::SystemTime::now());
                process.flush_and_truncate_output_file().await;

                let result = Ok(process.to_result());
                process.notify_waiters(result);
            }
        }

        for id in &fg_ids {
            self.processes.remove(id);
        }
    }

    async fn kill_foreground_commands_by_owner(&mut self, owner_session_id: &str) {
        let fg_ids: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| {
                p.owner_session_id.as_deref() == Some(owner_session_id)
                    && !p.bg_status.is_backgrounded()
                    && p.is_running()
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &fg_ids {
            if let Some(process) = self.processes.get_mut(id) {
                send_sigkill_to_group(process);
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait())
                        .await;
                if let Some(handle) = process.state_dump_handle.take() {
                    handle.abort();
                }
                process.mark_exited(ExitStatus {
                    exit_code: None,
                    signal: Some("cancelled".to_owned()),
                });
                process.end_wall_time = Some(std::time::SystemTime::now());
                process.flush_and_truncate_output_file().await;
                let result = Ok(process.to_result());
                process.notify_waiters(result);
            }
        }

        for id in &fg_ids {
            self.processes.remove(id);
        }
    }

    /// Backgrounded only; foreground tasks go through `kill_foreground_commands_by_owner`.
    async fn kill_tasks_by_owner(&mut self, owner_session_id: &str) {
        let owned_ids: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, p)| {
                p.owner_session_id.as_deref() == Some(owner_session_id)
                    && !p.lifecycle.has_exited()
                    && p.bg_status.is_backgrounded()
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in owned_ids {
            self.handle_kill(&id, KillSource::Teardown).await;
        }
    }

    /// Reroute surviving background tasks' notifications to the parent session.
    /// A synthetic `BashExecutionBackgrounded` per task gives the parent's TUI a
    /// `bg_tasks` entry for later events to attach to; monitor pipelines are re-spawned.
    fn reparent_notifications(
        &mut self,
        old_owner_session_id: &str,
        new_owner_session_id: &str,
        new_handle: crate::notification::types::ToolNotificationHandle,
        backend_weak: std::sync::Weak<dyn crate::computer::types::TerminalBackend>,
    ) {
        for (task_id, process) in self.processes.iter_mut() {
            if process.owner_session_id.as_deref() == Some(old_owner_session_id)
                && process.bg_status.is_backgrounded()
                && !process.lifecycle.has_exited()
            {
                // Foreground processes keep their owner so the follow-up
                // kill_foreground_commands_by_owner can still reap them.
                process.owner_session_id = Some(new_owner_session_id.to_string());
                process.notification_handle = new_handle.clone();

                // Recover the monitor label from the baked "[monitor] <desc>" display
                // command so the pager renders a Monitor row, not bash-highlighted text.
                let is_monitor = process.kind == crate::computer::types::TaskKind::Monitor;
                // Filter blank recoveries like spawn does, else Some("") blocks the
                // command fallback for the re-spawned pipeline label.
                let recovered_monitor_description = if is_monitor {
                    process
                        .display_command
                        .as_deref()
                        .and_then(|d| d.strip_prefix("[monitor] "))
                        .map(str::to_string)
                        .filter(|d| !d.trim().is_empty())
                } else {
                    None
                };
                let effective_description = process
                    .description
                    .clone()
                    .filter(|d| !d.trim().is_empty())
                    .or_else(|| recovered_monitor_description.clone());
                let reparent_command = if is_monitor {
                    process.command.clone()
                } else {
                    process
                        .display_command
                        .clone()
                        .unwrap_or_else(|| process.command.clone())
                };
                new_handle.send_backgrounded(crate::notification::BashExecutionBackgrounded {
                    base: crate::notification::BashNotificationBase {
                        tool_call_id: process.tool_call_id.clone(),
                        command: reparent_command,
                        output: Vec::new(),
                        total_bytes: 0,
                        truncated: false,
                        cwd: std::path::PathBuf::from(&process.cwd),
                    },
                    output_file: process.output_file.clone(),
                    task_id: task_id.clone(),
                    monitor_description: recovered_monitor_description,
                    description: effective_description.clone(),
                });

                // The old monitor pipeline died with the child session; re-spawn it.
                if process.kind == crate::computer::types::TaskKind::Monitor {
                    let pipeline_task_id = task_id.clone();
                    let pipeline_description =
                        effective_description.unwrap_or_else(|| process.command.clone());
                    // Weak so the reparented monitor doesn't pin the backend.
                    let pipeline_terminal = backend_weak.clone();
                    let pipeline_notif = new_handle.clone();
                    let pipeline_output_file = process.output_file.clone();
                    // Start at the current size so already-delivered events aren't re-emitted.
                    let start_offset = std::fs::metadata(&pipeline_output_file)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    tokio::spawn(async move {
                        crate::implementations::distill::monitor::tool::run_monitor_pipeline(
                            &pipeline_task_id,
                            &pipeline_description,
                            pipeline_terminal,
                            &pipeline_notif,
                            &pipeline_output_file,
                            Some("kill_command_or_subagent".to_string()),
                            start_offset,
                        )
                        .await;
                    });
                }
            }
        }
    }
}

// ============================================================================
// Handle (public API)
// ============================================================================

/// Channel handle to the terminal actor; the public `TerminalBackend` API.
#[derive(Clone)]
pub struct LocalTerminalBackend {
    cmd_tx: mpsc::Sender<TerminalCommand>,
    cancel_token: CancellationToken,
}

/// Grouped inputs for [`LocalTerminalBackend::new_inner`]; constructors override
/// only the fields they vary via `..Default::default()`.
struct LocalTerminalConfig {
    memory_config: Option<CgroupMemoryConfig>,
    use_spawn_local: bool,
    persistent_shell: bool,
    login_shell_capture: bool,
    search_shadows: SearchShadowConfig,
    shell_env_policy: Option<crate::util::ShellEnvironmentPolicy>,
    /// The owning session's scope; see [`LocalTerminalActor::session_scope`].
    process_scope: Option<crate::util::ProcessScope>,
    /// See [`LocalTerminalActor::scope`].
    scope: crate::util::ProcessScope,
    settings: ActorSettings,
}

impl Default for LocalTerminalConfig {
    fn default() -> Self {
        Self {
            memory_config: None,
            use_spawn_local: false,
            persistent_shell: false,
            login_shell_capture: true,
            search_shadows: SearchShadowConfig::default(),
            shell_env_policy: None,
            process_scope: None,
            scope: crate::util::global_process_scope().clone(),
            settings: ActorSettings::from_env(),
        }
    }
}

impl LocalTerminalBackend {
    pub fn new() -> Self {
        Self::new_inner(LocalTerminalConfig::default())
    }

    /// Env vars, cwd, functions, and aliases persist across commands; the login
    /// shell's rc files load once on first command.
    pub fn with_persistent_shell() -> Self {
        Self::new_inner(LocalTerminalConfig {
            persistent_shell: true,
            ..Default::default()
        })
    }

    /// See [`CgroupMemoryConfig`] for the soft/hard limit model.
    pub fn with_memory_limit(config: CgroupMemoryConfig) -> Self {
        Self::new_inner(LocalTerminalConfig {
            memory_config: Some(config),
            ..Default::default()
        })
    }

    /// spawn_local variant for single-threaded runtimes.
    pub fn new_local(search_shadows: SearchShadowConfig) -> Self {
        Self::new_inner(LocalTerminalConfig {
            use_spawn_local: true,
            search_shadows,
            ..Default::default()
        })
    }

    pub fn new_local_with_login_shell_capture(
        search_shadows: SearchShadowConfig,
        login_shell_capture: bool,
        shell_env_policy: Option<crate::util::ShellEnvironmentPolicy>,
        process_scope: Option<crate::util::ProcessScope>,
    ) -> Self {
        Self::new_inner(LocalTerminalConfig {
            use_spawn_local: true,
            login_shell_capture,
            search_shadows,
            shell_env_policy,
            process_scope,
            ..Default::default()
        })
    }

    pub fn new_local_with_persistent_shell(
        search_shadows: SearchShadowConfig,
        shell_env_policy: Option<crate::util::ShellEnvironmentPolicy>,
        process_scope: Option<crate::util::ProcessScope>,
    ) -> Self {
        Self::new_inner(LocalTerminalConfig {
            use_spawn_local: true,
            persistent_shell: true,
            search_shadows,
            shell_env_policy,
            process_scope,
            ..Default::default()
        })
    }

    /// Test-only: enrolls children into `scope` instead of the process-global
    /// one, so a test's `kill_all()` doesn't latch the scope shared by other tests.
    #[cfg(test)]
    pub(crate) fn new_local_with_scope(
        search_shadows: SearchShadowConfig,
        scope: crate::util::ProcessScope,
        session_scope: Option<crate::util::ProcessScope>,
    ) -> Self {
        Self::new_inner(LocalTerminalConfig {
            use_spawn_local: true,
            search_shadows,
            scope,
            process_scope: session_scope,
            settings: ActorSettings::default(),
            ..Default::default()
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_completed_task_ttl(ttl: Duration) -> Self {
        Self::new_for_test(ActorSettings {
            completed_task_ttl: ttl,
            ..Default::default()
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_foreground_budget(budget: Duration) -> Self {
        Self::new_for_test(ActorSettings {
            foreground_block_budget: budget,
            ..Default::default()
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_output_cap(output_file_cap: u64) -> Self {
        Self::new_for_test(ActorSettings {
            output_file_cap,
            ..Default::default()
        })
    }

    /// Stretched tick (test-only): waits must resolve on child exit or their
    /// own deadline, never on this interval.
    #[cfg(test)]
    pub(crate) fn new_with_tick_interval(tick_interval: Duration) -> Self {
        Self::new_for_test(ActorSettings {
            tick_interval,
            ..Default::default()
        })
    }

    /// Pinned to the constants rather than the env overrides (test-only).
    #[cfg(test)]
    fn new_for_test(settings: ActorSettings) -> Self {
        Self::new_inner(LocalTerminalConfig {
            settings,
            ..Default::default()
        })
    }

    fn new_inner(config: LocalTerminalConfig) -> Self {
        let LocalTerminalConfig {
            memory_config,
            use_spawn_local,
            persistent_shell,
            login_shell_capture,
            search_shadows,
            shell_env_policy,
            process_scope: session_scope,
            scope,
            settings,
        } = config;
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_SIZE);
        let actor_tx = cmd_tx.downgrade();
        let cancel_token = CancellationToken::new();

        let cancel_token_clone = cancel_token.clone();
        let actor_fut = async move {
            let (cgroup_guard, memory_monitor) = match memory_config {
                Some(config) => {
                    let guard = CgroupGuard::try_create(&config).await;
                    let monitor = MemoryMonitor::start(&guard, &config, use_spawn_local).await;
                    (guard, monitor)
                }
                None => (CgroupGuard::noop(), MemoryMonitor::noop()),
            };
            let actor = LocalTerminalActor::new(
                cmd_rx,
                actor_tx,
                cancel_token_clone,
                cgroup_guard,
                memory_monitor,
                persistent_shell,
                login_shell_capture,
                search_shadows,
                settings,
                scope,
                session_scope,
                shell_env_policy,
            );
            actor.run().await;
        };

        if use_spawn_local {
            tokio::task::spawn_local(actor_fut);
        } else {
            tokio::spawn(actor_fut);
        }

        Self {
            cmd_tx,
            cancel_token,
        }
    }

    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

impl Default for LocalTerminalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TerminalBackend for LocalTerminalBackend {
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, ComputerError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.cmd_tx
            .send(TerminalCommand::Run {
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ComputerError::io("terminal actor shut down"))?;

        reply_rx
            .await
            .map_err(|_| ComputerError::io("terminal actor dropped reply channel"))?
    }

    async fn run_background(
        &self,
        request: TerminalRunRequest,
    ) -> Result<BackgroundHandle, ComputerError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.cmd_tx
            .send(TerminalCommand::RunBackground {
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ComputerError::io("terminal actor shut down"))?;

        reply_rx
            .await
            .map_err(|_| ComputerError::io("terminal actor dropped reply channel"))?
    }

    async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(TerminalCommand::GetTask {
                task_id: task_id.to_string(),
                reply: reply_tx,
            })
            .await
            .ok()?;
        reply_rx.await.ok().flatten()
    }

    async fn kill_task(&self, task_id: &str) -> KillOutcome {
        self.kill_task_with_source(task_id, KillSource::ModelTool)
            .await
    }

    async fn kill_task_with_source(&self, task_id: &str, source: KillSource) -> KillOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::Kill {
                task_id: task_id.to_string(),
                source,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return KillOutcome::NotFound;
        }
        reply_rx.await.unwrap_or(KillOutcome::NotFound)
    }

    async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<Duration>,
    ) -> Option<TaskSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(TerminalCommand::WaitForCompletion {
                task_id: task_id.to_string(),
                timeout,
                reply: reply_tx,
            })
            .await
            .ok()?;
        reply_rx.await.ok().flatten()
    }

    async fn list_tasks(&self) -> Vec<TaskSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::ListTasks {
                include_output: true,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return vec![];
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn list_tasks_metadata(&self) -> Vec<TaskSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::ListTasks {
                include_output: false,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return vec![];
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn get_shell_cwd(&self) -> Option<PathBuf> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(TerminalCommand::GetShellCwd { reply: reply_tx })
            .await
            .ok()?;
        reply_rx.await.ok().flatten()
    }

    async fn warm_shell(&self, cwd: &std::path::Path) {
        let _ = self
            .cmd_tx
            .send(TerminalCommand::WarmShell {
                cwd: cwd.to_path_buf(),
            })
            .await;
    }

    async fn kill_foreground_commands(&self) {
        let _ = self
            .cmd_tx
            .send(TerminalCommand::KillForegroundCommands)
            .await;
    }

    async fn kill_all_background_tasks(&self) {
        let tasks = self.list_tasks().await;
        for task in tasks {
            if task.exit_code.is_none() && task.signal.is_none() {
                self.kill_task_with_source(&task.task_id, KillSource::Teardown)
                    .await;
            }
        }
    }

    async fn kill_foreground_commands_by_owner(&self, owner_session_id: &str) {
        let _ = self
            .cmd_tx
            .send(TerminalCommand::KillForegroundCommandsByOwner {
                owner_session_id: owner_session_id.to_string(),
            })
            .await;
    }

    async fn kill_all_background_tasks_by_owner(&self, owner_session_id: &str) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::KillTasksByOwner {
                owner_session_id: owner_session_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return;
        }
        let _ = reply_rx.await;
    }

    async fn reparent_notifications(
        &self,
        old_owner_session_id: &str,
        new_owner_session_id: &str,
        new_handle: crate::notification::types::ToolNotificationHandle,
        backend_weak: std::sync::Weak<dyn crate::computer::types::TerminalBackend>,
    ) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::ReparentNotifications {
                old_owner_session_id: old_owner_session_id.to_string(),
                new_owner_session_id: new_owner_session_id.to_string(),
                new_handle,
                backend_weak,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return;
        }
        // Block until the actor processed the reparent: the caller shuts down
        // the old session right after, which would drop notifications.
        let _ = reply_rx.await;
    }

    async fn background_foreground_command(&self, tool_call_id: &str) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::BackgroundForeground {
                tool_call_id: tool_call_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    async fn background_foreground_commands(
        &self,
        owner_session_id: Option<&str>,
    ) -> Vec<BackgroundedForeground> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(TerminalCommand::BackgroundForegroundCommands {
                owner_session_id: owner_session_id.map(str::to_string),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// `Waker::noop()` is safe: the actor polls periodically and needs no pipe
/// wake-ups. Avoids a timeout-per-read costing O(N × 20 ms) per tick.
fn try_read_nonblocking(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    buf: &mut [u8],
) -> Option<std::io::Result<usize>> {
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use tokio::io::ReadBuf;

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut read_buf = ReadBuf::new(buf);
    match Pin::new(reader).poll_read(&mut cx, &mut read_buf) {
        Poll::Ready(Ok(())) => Some(Ok(read_buf.filled().len())),
        Poll::Ready(Err(e)) => Some(Err(e)),
        Poll::Pending => None,
    }
}
/// Fire-and-forget; the poll loop reaps on the next tick.
fn send_sigterm_to_group(process: &ProcessState) {
    // Unix: skip if already reaped — the stored leader_pid may have been recycled.
    #[cfg(unix)]
    if process.child.id().is_none() {
        return;
    }
    if let Some(pg) = process.process_group.as_ref() {
        let _ = pg.terminate();
    }
}

/// `start_kill` on the immediate child is the fallback when group teardown is degraded.
fn send_sigkill_to_group(process: &mut ProcessState) {
    // Unix: skip if already reaped (see send_sigterm_to_group).
    #[cfg(unix)]
    if process.child.id().is_none() {
        let _ = process.child.start_kill();
        return;
    }
    if let Some(pg) = process.process_group.as_ref() {
        let _ = pg.kill();
    }
    let _ = process.child.start_kill();
}

/// Drains the taken pipes off the actor, then hands the output and the
/// already-determined exit status back as a [`TerminalCommand::DrainedOutput`].
fn spawn_detached_drain(
    self_tx: mpsc::WeakSender<TerminalCommand>,
    task_id: String,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    status: ExitStatus,
) {
    tokio::spawn(async move {
        let mut output = Vec::new();
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
            if let Some(mut stdout) = stdout {
                let mut buf = [0u8; READ_BUFFER_SIZE];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.extend_from_slice(buf.get(..n).unwrap_or(&[])),
                    }
                }
            }
            if let Some(mut stderr) = stderr {
                let mut buf = [0u8; READ_BUFFER_SIZE];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.extend_from_slice(buf.get(..n).unwrap_or(&[])),
                    }
                }
            }
        })
        .await;
        if let Some(tx) = self_tx.upgrade() {
            let _ = tx
                .send(TerminalCommand::DrainedOutput {
                    task_id,
                    output,
                    status,
                })
                .await;
        }
    });
}

/// Bounded by `DRAIN_TIMEOUT`: a backgrounded child holding the pipe open
/// must not block the actor loop indefinitely.
async fn drain_remaining_output(process: &mut ProcessState) {
    let timed_out = tokio::time::timeout(DRAIN_TIMEOUT, async {
        if let Some(stdout) = process.child.stdout.as_mut() {
            let mut buf = [0u8; READ_BUFFER_SIZE];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        process
                            .output_buffer
                            .extend_from_slice(buf.get(..n).unwrap_or(&[]));
                        process.total_bytes += n;
                        if let Some(ref mut file) = process.file_handle {
                            let _ = file.write_all(buf.get(..n).unwrap_or(&[])).await;
                        }
                    }
                }
            }
        }

        if let Some(stderr) = process.child.stderr.as_mut() {
            let mut buf = [0u8; READ_BUFFER_SIZE];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        process
                            .output_buffer
                            .extend_from_slice(buf.get(..n).unwrap_or(&[]));
                        process.total_bytes += n;
                        if let Some(ref mut file) = process.file_handle {
                            let _ = file.write_all(buf.get(..n).unwrap_or(&[])).await;
                        }
                    }
                }
            }
        }
    })
    .await;

    if timed_out.is_err() {
        tracing::debug!(
            command = %process.command,
            "drain timed out after {:?}, a backgrounded child may be holding the pipe open",
            DRAIN_TIMEOUT,
        );
    }

    // Drop the pipes so orphans holding them open can't cause repeated drain timeouts.
    process.child.stdout.take();
    process.child.stderr.take();

    process.maybe_truncate();
}

/// Never waits: a live pipe would hold the single-threaded actor for the full
/// drain timeout, so this is safe on a process that is still running.
async fn take_available_output(process: &mut ProcessState) {
    let mut collected = Vec::new();
    if let Some(stdout) = process.child.stdout.as_mut() {
        read_available(stdout, &mut collected);
    }
    if let Some(stderr) = process.child.stderr.as_mut() {
        read_available(stderr, &mut collected);
    }
    process.child.stdout.take();
    process.child.stderr.take();

    if collected.is_empty() {
        return;
    }
    process.output_buffer.extend_from_slice(&collected);
    process.total_bytes += collected.len();
    if let Some(file) = process.file_handle.as_mut() {
        let _ = file.write_all(&collected).await;
    }
    process.maybe_truncate();
}

fn read_available(reader: &mut (impl tokio::io::AsyncRead + Unpin), out: &mut Vec<u8>) {
    let mut buf = [0u8; READ_BUFFER_SIZE];
    loop {
        match try_read_nonblocking(reader, &mut buf) {
            Some(Ok(0)) | Some(Err(_)) | None => return,
            Some(Ok(n)) => out.extend_from_slice(buf.get(..n).unwrap_or(&[])),
        }
    }
}

/// Synchronous two-phase kill for the explicit kill_task path; every await is
/// bounded so the actor loop never blocks indefinitely.
async fn graceful_kill_and_wait(process: &mut ProcessState) {
    send_sigterm_to_group(process);

    if tokio::time::timeout(SIGTERM_GRACE, process.child.wait())
        .await
        .is_ok()
    {
        drain_remaining_output(process).await;
        return;
    }

    send_sigkill_to_group(process);

    // SIGKILL almost always reaps instantly; the cap protects against D-state
    // (uninterruptible kernel I/O). On timeout, abandon — poll_process retries.
    const SIGKILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);
    if tokio::time::timeout(SIGKILL_REAP_TIMEOUT, process.child.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            pid = ?process.child.id(),
            "Process did not exit after SIGKILL within {:?}, \
             abandoning reap — poll loop will pick it up",
            SIGKILL_REAP_TIMEOUT
        );
    }

    drain_remaining_output(process).await;
}

#[tracing::instrument(
    name = "terminal.kill_and_finalize",
    skip_all,
    fields(pid = process.child.id())
)]
async fn kill_and_finalize(process: &mut ProcessState) -> KillOutcome {
    // Reaped between the caller's check and here (race with poll_process).
    if process.lifecycle.has_exited() {
        return KillOutcome::AlreadyExited;
    }

    match process.child.try_wait() {
        Ok(Some(status)) => {
            drain_remaining_output(process).await;
            finalize_process(process, Some(status)).await;
            return KillOutcome::AlreadyExited;
        }
        Err(_) => {
            drain_remaining_output(process).await;
            finalize_process(process, None).await;
            return KillOutcome::AlreadyExited;
        }
        Ok(None) => {}
    }

    graceful_kill_and_wait(process).await;

    // Drop the scope handle now (not at the next sweep) so a racing kill_all()
    // can't killpg the recycled pid; kept if the reap was abandoned (id still Some).
    #[cfg(unix)]
    if process.child.id().is_none() {
        process.process_group = None;
    }

    // Abort the dump reader (same fd-4 hang rationale as kill_foreground_commands).
    if let Some(handle) = process.state_dump_handle.take() {
        handle.abort();
    }

    finalize_process(process, None).await;
    KillOutcome::Killed
}

/// Callers read the remaining output first. A process that could not be
/// collected stays unsettled, so the poll loop keeps trying.
async fn finalize_process(process: &mut ProcessState, status: Option<std::process::ExitStatus>) {
    if process.lifecycle.has_exited() {
        return;
    }

    process.mark_exited(match status {
        Some(s) => extract_exit_status(s),
        None => ExitStatus {
            exit_code: None,
            signal: Some("killed".to_owned()),
        },
    });
    if process.end_wall_time.is_none() {
        process.end_wall_time = Some(std::time::SystemTime::now());
    }

    process.flush_and_truncate_output_file().await;
    process.finish_output(Collection::of(&process.child));

    let result = Ok(process.to_result());
    process.notify_waiters(result);
}

#[tracing::instrument(name = "fs.open_output_file", skip_all)]
async fn open_output_file(path: &std::path::Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
}

#[cfg(unix)]
const ENV_LOGIN_ENV: &str = "GROK_LOGIN_ENV";

#[cfg(unix)]
fn login_env_capture_enabled() -> bool {
    !matches!(
        std::env::var(ENV_LOGIN_ENV).as_deref(),
        Ok("0") | Ok("false")
    )
}

#[cfg(unix)]
fn login_env_var_excluded(key: &str) -> bool {
    matches!(
        key,
        "PWD"
            | "OLDPWD"
            | "SHLVL"
            | "_"
            | "TERM"
            | "GROK_AGENT"
            | "SUDO_ASKPASS"
            | "GROK_ASKPASS"
            | "ELECTRON_RUN_AS_NODE"
            | "SSH_AUTH_SOCK"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "XDG_RUNTIME_DIR"
            | "WAYLAND_DISPLAY"
            | "GPG_TTY"
    ) || key.to_ascii_lowercase().ends_with("_proxy")
        || key.starts_with("GROK_SANDBOX")
}

#[cfg(unix)]
fn parse_login_env_capture(stdout: &str) -> (Option<String>, HashMap<String, String>) {
    let parts: Vec<&str> = stdout.split('\x01').collect();
    let login_path = parts
        .get(1)
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let mut env_map = HashMap::new();
    if let Some(blob) = parts.get(2) {
        for pair in blob.split('\0') {
            if let Some((key, value)) = pair.split_once('=')
                && !key.is_empty()
                && key != "PATH"
                && !login_env_var_excluded(key)
            {
                env_map.insert(key.to_string(), value.to_string());
            }
        }
    }
    (login_path, env_map)
}

#[cfg(unix)]
async fn capture_login_env() -> HashMap<String, String> {
    use tokio::io::AsyncReadExt;

    let shell = shell_state::ShellKind::detect();
    let rc_file = shell.rc_file_name();

    // Use $HOME inside the script (not interpolated from Rust) to avoid
    // shell injection if HOME contains special characters.
    let script = format!(
        "source \"$HOME/{rc_file}\" 2>/dev/null; printf '\\x01%s\\x01' \"$PATH\"; command env -0 2>/dev/null; printf '\\x01'"
    );

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut cmd = tokio::process::Command::new(shell.binary_path());
        cmd.args(["-lc", &script])
            .stdin(distill_tty_utils::null_stdio())
            .stdout(Stdio::piped())
            .stderr(distill_tty_utils::null_stdio())
            .kill_on_drop(true);
        crate::util::detach_command(&mut cmd);
        distill_sandbox::child_net::restrict_child_network(&mut cmd);
        cmd.envs(crate::util::pager_env());
        #[allow(clippy::disallowed_methods)] // probe killed on drop
        let mut child = cmd.spawn().ok()?;

        let mut stdout_buf = Vec::new();
        if let Some(ref mut stdout) = child.stdout {
            stdout.read_to_end(&mut stdout_buf).await.ok();
        }

        let status = child.wait().await.ok()?;
        if !status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&stdout_buf);
        let (login_path, mut env_map) = parse_login_env_capture(&stdout);
        let login_path = login_path?;

        if !login_env_capture_enabled() {
            env_map.clear();
        }

        // Merge: login PATH first, then current-process entries not already present.
        let current_path = std::env::var("PATH").unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let merged: Vec<&str> = login_path
            .split(':')
            .chain(current_path.split(':'))
            .filter(|e| !e.is_empty() && seen.insert(*e))
            .collect();
        env_map.insert("PATH".to_string(), merged.join(":"));

        Some(env_map)
    })
    .await;

    match result {
        Ok(Some(env_map)) => env_map,
        Ok(None) => HashMap::new(),
        Err(_) => {
            tracing::warn!("login-shell env capture timed out after 5s");
            HashMap::new()
        }
    }
}

/// Layer login-shell captured vars (except `PATH`) onto `cmd`, dropping those the
/// active policy filters out and those already set in grok's own environment.
#[cfg(unix)]
fn layer_login_env_vars(
    cmd: &mut tokio::process::Command,
    login_env: Option<&HashMap<String, String>>,
    active_policy: Option<&crate::util::ShellEnvironmentPolicy>,
) {
    if let Some(login) = login_env {
        for (key, value) in login {
            // `var_os` reads grok's own env (not the possibly-cleared child env);
            // the policy filters the capture so an rc export cannot bypass it.
            if key != "PATH"
                && std::env::var_os(key).is_none()
                && active_policy.is_none_or(|p| p.allows_with_inherit(key))
            {
                cmd.env(key, value);
            }
        }
    }
}

/// Drops names the active policy excludes so a request-supplied secret cannot
/// bypass it; honors `exclude`/`include_only` but not `inherit` (request env is explicit).
fn layer_request_env(
    cmd: &mut tokio::process::Command,
    env: &HashMap<String, String>,
    active_policy: Option<&crate::util::ShellEnvironmentPolicy>,
) {
    for (key, value) in env {
        if active_policy.is_none_or(|p| p.allows(key)) {
            cmd.env(key, value);
        }
    }
}

/// Login `PATH` goes last so rc-file additions win, unless the policy filters `PATH`.
#[cfg(unix)]
fn layer_login_path(
    cmd: &mut tokio::process::Command,
    login_env: Option<&HashMap<String, String>>,
    active_policy: Option<&crate::util::ShellEnvironmentPolicy>,
) {
    if let Some(path) = login_env.and_then(|l| l.get("PATH"))
        && active_policy.is_none_or(|p| p.allows_with_inherit("PATH"))
    {
        cmd.env("PATH", path);
    }
}

/// Fixed layer order: policy base, login capture (filtered), grok control vars, request env (filtered), pager vars,
/// login `PATH`, agent marker last. Applied incrementally, not via `env_clear`: the no-op-policy path must inherit
/// grok's environment untouched (non-UTF-8 vars included).
#[cfg(unix)]
fn apply_child_env(
    cmd: &mut tokio::process::Command,
    policy: Option<&crate::util::ShellEnvironmentPolicy>,
    login_env: Option<&HashMap<String, String>>,
    request_env: &HashMap<String, String>,
) {
    let active_policy = policy.filter(|p| !p.is_noop());
    crate::util::shell_env_policy::install_policy_base_env(cmd, active_policy);
    layer_login_env_vars(cmd, login_env, active_policy);
    cmd.envs(shell_state::shell_env_overrides());
    layer_request_env(cmd, request_env, active_policy);
    cmd.envs(crate::util::pager_env());
    layer_login_path(cmd, login_env, active_policy);
    crate::util::apply_grok_agent_marker(cmd);
}

/// Attaches the child to a [`ProcessGroup`] for whole-tree teardown.
fn spawn_shell_command(
    command: &str,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
    login_env: Option<&HashMap<String, String>>,
    search_shadows: SearchShadowConfig,
    shell_env_policy: Option<&crate::util::ShellEnvironmentPolicy>,
) -> std::io::Result<(tokio::process::Child, crate::util::ProcessGroup)> {
    // Keep unix-only args live on Windows to avoid unused-arg warnings.
    #[cfg(not(unix))]
    let _ = (&login_env, &search_shadows);
    #[cfg(unix)]
    let mut cmd = {
        let shell = shell_state::ShellKind::detect();
        let wrapped_command = {
            let inject = super::embedded_search_tools::search_injection(search_shadows);
            if inject.is_empty() {
                command.to_string()
            } else {
                format!("{inject}{command}")
            }
        };
        let mut cmd = tokio::process::Command::new(shell.binary_path());
        // Non-interactive zsh still defaults to NOMATCH; pass via argv like init's -o extendedglob.
        if matches!(shell, shell_state::ShellKind::Zsh) {
            cmd.arg("-o").arg("nonomatch");
        }
        cmd.arg("-c")
            .arg(&wrapped_command)
            .current_dir(cwd)
            .stdin(distill_tty_utils::null_stdio())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Do NOT set .process_group(0): std runs setpgid() before pre_exec
            // hooks, so setsid() in the detach hook would fail with EPERM.
            .kill_on_drop(true);

        apply_child_env(&mut cmd, shell_env_policy, login_env, env);

        // Detach from the controlling terminal so subprocesses cannot open
        // /dev/tty and compete with the TUI for terminal input.
        crate::util::detach_command(&mut cmd);

        distill_sandbox::child_net::restrict_child_network(&mut cmd);
        cmd
    };

    #[cfg(not(unix))]
    let mut build_cmd = |with_breakaway: bool| {
        use windows::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        };

        let inv = distill_config::shell::shell_command_argv(command);
        let mut cmd = tokio::process::Command::new(&inv.program);
        cmd.args(&inv.args)
            .current_dir(cwd)
            .stdin(distill_tty_utils::null_stdio())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Mirrors the unix `apply_child_env` order; `inv.env` is grok's trusted
        // shell setup, so it is not filtered.
        let active_policy = shell_env_policy.filter(|p| !p.is_noop());
        crate::util::shell_env_policy::install_policy_base_env(&mut cmd, active_policy);
        cmd.envs(inv.env);
        layer_request_env(&mut cmd, env, active_policy);
        cmd.envs(crate::util::pager_env());
        crate::util::apply_grok_agent_marker(&mut cmd);
        // After the env layers so a policy PATH is prepended, not replaced. A
        // policy base env replaced the inherited one; grok's own PATH must not
        // come back through the prepend (`inherit = none`, an excluded PATH).
        let path_base = if active_policy.is_some() {
            distill_tty_utils::PathBase::ExplicitOnly
        } else {
            distill_tty_utils::PathBase::Process
        };
        distill_tty_utils::prepend_bundled_git_path(cmd.as_std_mut(), path_base);

        // Flags set inline: tokio's creation_flags is a SET, not OR, so the detach
        // helpers don't compose. CREATE_BREAKAWAY_FROM_JOB fails with os error 5 when
        // the parent's job lacks JOB_OBJECT_LIMIT_BREAKAWAY_OK; the caller retries without it.
        let mut flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        if with_breakaway {
            flags |= CREATE_BREAKAWAY_FROM_JOB;
        }
        cmd.creation_flags(flags.0);
        cmd
    };

    #[cfg(unix)]
    let mut group = crate::util::ProcessGroup::new()?;
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods)] // attached to the process group built above
    let child = cmd.spawn().map_err(|e| {
        std::io::Error::new(e.kind(), format!("spawn shell in {}: {e}", cwd.display()))
    })?;

    #[cfg(not(unix))]
    #[allow(clippy::disallowed_methods)] // attached to the process group built in this block
    let (child, mut group) = {
        let group = crate::util::ProcessGroup::new()?;
        let mut cmd = build_cmd(true);
        match cmd.spawn() {
            Ok(child) => (child, group),
            Err(e) if e.raw_os_error() == Some(5) => {
                // Job disallows breakaway: retry without the flag. attach() below
                // will also fail, but kill_on_drop still reaps the immediate child.
                tracing::debug!(
                    "spawn with CREATE_BREAKAWAY_FROM_JOB returned ERROR_ACCESS_DENIED; \
                     retrying without breakaway (process-tree teardown disabled for this child)"
                );
                drop(cmd);
                let mut cmd = build_cmd(false);
                let child = cmd.spawn()?;
                (child, group)
            }
            Err(e) => return Err(e),
        }
    };

    if let Err(e) = group.attach(&child) {
        tracing::debug!("Failed to attach child to ProcessGroup: {e}");
    }
    Ok((child, group))
}

fn extract_exit_status(status: std::process::ExitStatus) -> ExitStatus {
    let exit_code = status.code();

    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| format!("signal {}", s))
    };

    #[cfg(not(unix))]
    let signal: Option<String> = None;

    ExitStatus { exit_code, signal }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::TaskKind;
    use std::path::PathBuf;

    fn make_request(command: &str) -> TerminalRunRequest {
        let output_file = std::env::temp_dir().join(format!(
            "terminal-test-{}-{}.out",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        TerminalRunRequest {
            command: command.to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(30),
            output_byte_limit: 10000,
            output_file,
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        }
    }

    #[tokio::test]
    async fn run_background_preserves_description_on_snapshot() {
        let backend = LocalTerminalBackend::new();
        let mut with_desc = make_request("sleep 30");
        with_desc.description = Some("build frontend".to_string());
        let handle = backend.run_background(with_desc).await.unwrap();
        let snap = backend
            .get_task(&handle.task_id)
            .await
            .expect("running task snapshot");
        assert_eq!(snap.description.as_deref(), Some("build frontend"));
        let listed = backend.list_tasks().await;
        let listed_snap = listed
            .iter()
            .find(|t| t.task_id == handle.task_id)
            .expect("task listed");
        assert_eq!(listed_snap.description.as_deref(), Some("build frontend"));
        let _ = backend.kill_task(&handle.task_id).await;

        let without = make_request("sleep 30");
        let handle = backend.run_background(without).await.unwrap();
        let snap = backend
            .get_task(&handle.task_id)
            .await
            .expect("running task snapshot");
        assert!(
            snap.description.is_none(),
            "absent description must stay None"
        );
        let _ = backend.kill_task(&handle.task_id).await;
    }

    /// Poll `get_task` every 25ms until `completed`, or `false` at `timeout`.
    async fn poll_until_task_completed(
        backend: &LocalTerminalBackend,
        task_id: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if backend
                .get_task(task_id)
                .await
                .map(|s| s.completed)
                .unwrap_or(false)
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn layer_request_env_drops_names_the_policy_excludes() {
        use crate::util::{EnvironmentVariablePattern, ShellEnvironmentPolicy};

        let glob = EnvironmentVariablePattern::new_case_insensitive;
        let policy = ShellEnvironmentPolicy {
            exclude: vec![glob("AWS_*")],
            include_only: vec![glob("PATH"), glob("SAFE_*")],
            ..Default::default()
        };
        let env = HashMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("SAFE_FLAG".to_string(), "1".to_string()),
            ("AWS_SECRET".to_string(), "leak".to_string()),
            ("OTHER".to_string(), "x".to_string()),
        ]);

        let mut cmd = tokio::process::Command::new("true");
        layer_request_env(&mut cmd, &env, Some(&policy));
        let applied: HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();

        assert_eq!(applied.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(applied.get("SAFE_FLAG").map(String::as_str), Some("1"));
        assert!(!applied.contains_key("AWS_SECRET"));
        assert!(!applied.contains_key("OTHER"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_child_env_layers_in_fixed_order() {
        use crate::util::{EnvironmentVariablePattern, ShellEnvironmentPolicy};

        let policy = ShellEnvironmentPolicy {
            exclude: vec![EnvironmentVariablePattern::new_case_insensitive("*SECRET*")],
            set: HashMap::from([("GROK_TEST_BASE".to_string(), "1".to_string())]),
            ..Default::default()
        };
        let login = HashMap::from([
            ("GROK_TEST_LOGIN".to_string(), "l".to_string()),
            ("PATH".to_string(), "/login/bin".to_string()),
        ]);
        let request = HashMap::from([
            ("GROK_TEST_REQ".to_string(), "r".to_string()),
            ("PATH".to_string(), "/req/bin".to_string()),
            ("GROK_TEST_SECRET".to_string(), "s".to_string()),
        ]);

        let mut cmd = tokio::process::Command::new("true");
        apply_child_env(&mut cmd, Some(&policy), Some(&login), &request);
        let env: HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();

        assert_eq!(env.get("GROK_TEST_BASE").map(String::as_str), Some("1"));
        assert_eq!(env.get("GROK_TEST_LOGIN").map(String::as_str), Some("l"));
        assert_eq!(env.get("GROK_TEST_REQ").map(String::as_str), Some("r"));
        assert!(!env.contains_key("GROK_TEST_SECRET"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/login/bin"));
        assert_eq!(
            env.get(crate::util::GROK_AGENT_ENV).map(String::as_str),
            Some(crate::util::GROK_AGENT_ENV_VALUE)
        );
    }

    #[tokio::test]
    async fn test_command_with_exit_code() {
        let backend = LocalTerminalBackend::new();
        let result = backend.run(make_request("exit 42")).await.unwrap();

        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_timeout() {
        let backend = LocalTerminalBackend::new();

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-timeout-{}.out", std::process::id()));

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_millis(200),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-timeout".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert!(result.timed_out);

        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_auto_background_on_timeout() {
        let backend = LocalTerminalBackend::new();

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-auto-bg-{}.out", std::process::id()));
        let tool_call_id = "test-auto-bg";

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_millis(500),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: true,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();

        assert!(
            !result.timed_out,
            "auto-backgrounded result must not be timed_out"
        );
        assert_eq!(
            result.signal.as_deref(),
            Some("auto_backgrounded"),
            "signal should be auto_backgrounded, got {:?}",
            result.signal
        );

        let snapshot = backend
            .get_task(tool_call_id)
            .await
            .expect("auto-backgrounded task should be queryable by tool_call_id");
        assert!(
            !snapshot.completed,
            "auto-backgrounded process should still be running"
        );

        let outcome = backend.kill_task(tool_call_id).await;
        assert!(
            matches!(outcome, KillOutcome::Killed | KillOutcome::AlreadyExited),
            "kill after auto-bg should succeed: {outcome:?}"
        );
        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_background_foreground_commands_keeps_process_alive() {
        let backend = std::sync::Arc::new(LocalTerminalBackend::new());

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-bg-all-{}.out", std::process::id()));
        let tool_call_id = "test-bg-all";

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(60),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            // Not auto-backgroundable: must be backgrounded on demand, never killed.
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let run_backend = backend.clone();
        let run = tokio::spawn(async move { run_backend.run(request).await });

        tokio::time::sleep(Duration::from_millis(500)).await;

        let backgrounded = backend.background_foreground_commands(None).await;

        assert_eq!(
            backgrounded.len(),
            1,
            "the running command was backgrounded"
        );
        assert_eq!(
            backgrounded.first().map(|t| t.tool_call_id.as_str()),
            Some(tool_call_id)
        );

        let result = run.await.unwrap().unwrap();
        assert_eq!(
            result.signal.as_deref(),
            Some("backgrounded"),
            "run must return a backgrounded signal, got {:?}",
            result.signal
        );

        let snapshot = backend
            .get_task(tool_call_id)
            .await
            .expect("backgrounded task should be queryable by tool_call_id");
        assert!(
            !snapshot.completed,
            "the backgrounded process must still be running, not killed"
        );

        assert!(
            backend
                .background_foreground_commands(None)
                .await
                .is_empty(),
            "no foreground commands remain after backgrounding"
        );

        let _ = backend.kill_task(tool_call_id).await;
        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_foreground_block_budget_backgrounds_before_timeout() {
        let backend = LocalTerminalBackend::new_with_foreground_budget(Duration::from_millis(300));

        let output_file = std::env::temp_dir().join(format!(
            "terminal-test-fg-budget-{}.out",
            std::process::id()
        ));
        let tool_call_id = "test-fg-budget";

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(3600),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: true,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let start = Instant::now();
        let result = backend.run(request).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            !result.timed_out,
            "budget-backgrounded result must not be timed_out"
        );
        assert_eq!(
            result.signal.as_deref(),
            Some("auto_backgrounded"),
            "signal should be auto_backgrounded, got {:?}",
            result.signal
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "should background on the budget, not block on the timeout (took {elapsed:?})"
        );

        let snapshot = backend
            .get_task(tool_call_id)
            .await
            .expect("budget-backgrounded task should be queryable by tool_call_id");
        assert!(
            !snapshot.completed,
            "budget-backgrounded process should still be running"
        );

        let outcome = backend.kill_task(tool_call_id).await;
        assert!(
            matches!(outcome, KillOutcome::Killed | KillOutcome::AlreadyExited),
            "kill after budget-bg should succeed: {outcome:?}"
        );
        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_foreground_block_budget_skips_non_backgroundable() {
        let backend = LocalTerminalBackend::new_with_foreground_budget(Duration::from_millis(300));

        let output_file = std::env::temp_dir().join(format!(
            "terminal-test-fg-budget-skip-{}.out",
            std::process::id()
        ));

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_millis(500),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-fg-budget-skip".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();

        assert!(
            result.timed_out,
            "non-backgroundable command should time out, not background"
        );
        assert_ne!(
            result.signal.as_deref(),
            Some("auto_backgrounded"),
            "non-backgroundable command must not be auto-backgrounded by the budget"
        );

        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_per_request_foreground_block_budget_overrides_backend() {
        let backend = LocalTerminalBackend::new_with_foreground_budget(Duration::from_secs(10));

        let output_file = std::env::temp_dir().join(format!(
            "terminal-test-per-req-budget-{}.out",
            std::process::id()
        ));
        let tool_call_id = "test-per-req-budget";

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(3600),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: true,
            foreground_block_budget: Some(Duration::from_millis(300)),
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let start = Instant::now();
        let result = backend.run(request).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            !result.timed_out,
            "per-request budget should auto-bg, not kill"
        );
        assert_eq!(result.signal.as_deref(), Some("auto_backgrounded"));
        assert!(
            elapsed < Duration::from_secs(5),
            "should fire on per-request 300ms budget, not backend 10s (took {elapsed:?})"
        );

        let outcome = backend.kill_task(tool_call_id).await;
        assert!(matches!(
            outcome,
            KillOutcome::Killed | KillOutcome::AlreadyExited
        ));
        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_duration_max_budget_waits_for_timeout_only() {
        let backend = LocalTerminalBackend::new_with_foreground_budget(Duration::from_millis(100));

        let output_file = std::env::temp_dir().join(format!(
            "terminal-test-max-budget-{}.out",
            std::process::id()
        ));
        let tool_call_id = "test-max-budget";

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_millis(800),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: true,
            foreground_block_budget: Some(Duration::MAX),
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let start = Instant::now();
        let result = backend.run(request).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(result.signal.as_deref(), Some("auto_backgrounded"));
        assert!(
            elapsed >= Duration::from_millis(500),
            "Duration::MAX budget must not short-circuit at 100ms backend default (took {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "should auto-bg around the 800ms timeout (took {elapsed:?})"
        );

        let outcome = backend.kill_task(tool_call_id).await;
        assert!(matches!(
            outcome,
            KillOutcome::Killed | KillOutcome::AlreadyExited
        ));
        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_output_size_guard_kills_runaway() {
        // Tiny cap so `yes` trips it within a tick or two.
        let backend = LocalTerminalBackend::new_with_output_cap(2_000);

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-size-{}.out", std::process::id()));

        let request = TerminalRunRequest {
            command: "yes".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(30),
            output_byte_limit: 10_000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-size".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();

        assert_eq!(
            result.signal.as_deref(),
            Some("output_limit"),
            "runaway output should be killed by the size guard, got {:?}",
            result.signal
        );
        assert!(!result.timed_out, "size kill is not a timeout");

        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_stderr_captured() {
        let backend = LocalTerminalBackend::new();
        let result = backend.run(make_request("echo error >&2")).await.unwrap();

        assert!(result.combined_output.contains("error"));
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_run_background_and_get_task() {
        let backend = LocalTerminalBackend::new();

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-bg-{}.out", std::process::id()));

        let request = TerminalRunRequest {
            command: "echo background_test && sleep 0.1".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(30),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-bg".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let handle = backend.run_background(request).await.unwrap();
        assert!(!handle.task_id.is_empty());

        let snapshot = backend
            .wait_for_completion(&handle.task_id, Some(Duration::from_secs(5)))
            .await;
        assert!(snapshot.is_some());
        let snapshot = snapshot.unwrap();
        assert!(snapshot.completed);
        assert_eq!(snapshot.exit_code, Some(0));

        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn test_kill_background_task() {
        let backend = LocalTerminalBackend::new();

        let output_file =
            std::env::temp_dir().join(format!("terminal-test-kill-{}.out", std::process::id()));

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(300),
            output_byte_limit: 10000,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-kill".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let handle = backend.run_background(request).await.unwrap();

        let outcome = backend.kill_task(&handle.task_id).await;
        assert_eq!(outcome, KillOutcome::Killed);

        let _ = tokio::fs::remove_file(&output_file).await;
    }

    #[tokio::test]
    async fn chunk_notifications_sent_during_execution() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = ToolNotificationHandle::from_sender(tx);

        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "for i in 1 2 3; do echo chunk_$i; sleep 0.15; done".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            output_byte_limit: 1024 * 1024,
            output_file: tmp.path().join("output.log"),
            notification_handle: handle,
            tool_call_id: "test-call-123".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert_eq!(result.exit_code, Some(0));

        let mut chunks = vec![];
        while let Ok(notification) = rx.try_recv() {
            if let crate::notification::types::ToolNotification::BashOutputChunk(chunk) =
                notification
            {
                chunks.push(chunk);
            }
        }

        assert!(
            chunks.len() >= 2,
            "Expected at least 2 BashOutputChunk notifications (initial + output), got {}",
            chunks.len()
        );

        let Some(initial) = chunks.first() else {
            panic!("expected an initial chunk");
        };
        assert_eq!(initial.base.tool_call_id, "test-call-123");
        assert!(!initial.base.command.is_empty());
        assert!(
            initial.base.output.is_empty(),
            "Initial chunk should have empty output"
        );

        let Some(first_with_output) = chunks.get(1) else {
            panic!("expected a follow-up chunk");
        };
        assert!(!first_with_output.base.output.is_empty());

        assert!(
            chunks.last().unwrap().base.output.len() >= first_with_output.base.output.len(),
            "Output should accumulate across chunks"
        );

        assert!(result.combined_output.contains("chunk_1"));
        assert!(result.combined_output.contains("chunk_3"));
    }

    #[tokio::test]
    async fn chunk_notifications_keep_flowing_after_truncation() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = ToolNotificationHandle::from_sender(tx);

        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            // ~1.8 KB against a 200-char limit: truncation fires early and keeps firing.
            command: "for i in $(seq 1 60); do printf 'LINE%03d-XXXXXXXXXXXXXXXXXXXX\\n' \"$i\"; sleep 0.03; done".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(10),
            output_byte_limit: 200,
            output_file: tmp.path().join("output.log"),
            notification_handle: handle,
            tool_call_id: "trunc-call".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.truncated, "expected output to be truncated");

        let mut chunks = vec![];
        while let Ok(notification) = rx.try_recv() {
            if let crate::notification::types::ToolNotification::BashOutputChunk(chunk) =
                notification
            {
                chunks.push(chunk);
            }
        }

        for w in chunks.windows(2) {
            let [a, b] = w else { continue };
            assert!(
                b.base.total_bytes >= a.base.total_bytes,
                "total_bytes regressed: {} < {}",
                b.base.total_bytes,
                a.base.total_bytes
            );
        }

        let first_truncated = chunks
            .iter()
            .position(|c| c.base.truncated)
            .expect("expected at least one truncated chunk past the byte limit");
        assert!(
            first_truncated < chunks.len() - 1,
            "chunks stopped after truncation (first_truncated={first_truncated}, total={})",
            chunks.len()
        );
        assert!(
            chunks.last().unwrap().base.total_bytes > 200,
            "expected total_bytes to exceed the byte limit"
        );
    }

    #[tokio::test]
    async fn output_file_capped_by_size_guard() {
        let cap: u64 = 5000;
        let output_amount = cap * 1000;
        let backend = LocalTerminalBackend::new_with_output_cap(cap);
        let tmp = tempfile::TempDir::new().unwrap();
        let output_file = tmp.path().join("output.log");

        let request = TerminalRunRequest {
            command: format!("head -c {output_amount} /dev/zero | tr '\\0' 'x'"),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(30),
            output_byte_limit: 1024,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "cap-test".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert_eq!(result.signal.as_deref(), Some("output_limit"));

        let file_size = tokio::fs::metadata(&output_file).await.unwrap().len();
        // The guard fires on a 100ms tick, so allow overshoot (~512 KB seen on arm64 CI).
        assert!(
            file_size < output_amount / 2,
            "output file should be bounded by size guard, got {file_size} bytes (cap={cap})"
        );
    }

    #[tokio::test]
    async fn small_output_file_not_truncated_after_exit() {
        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let output_file = tmp.path().join("output.log");

        let request = TerminalRunRequest {
            command: "head -c 200000 /dev/zero | tr '\\0' 'x'".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(10),
            output_byte_limit: 1024,
            output_file: output_file.clone(),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "trunc-exit-test".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert_eq!(result.exit_code, Some(0));

        let file_size = tokio::fs::metadata(&output_file).await.unwrap().len();
        assert!(
            file_size >= 190_000,
            "output file should have full ~200 KB, got {file_size}"
        );
        assert_eq!(MAX_RETAINED_OUTPUT_FILE_BYTES, 64 * 1024 * 1024);
    }

    #[tokio::test]
    async fn no_chunks_when_noop_handle() {
        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "echo hello".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            output_byte_limit: 1024 * 1024,
            output_file: tmp.path().join("output.log"),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-call".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        // noop() drops the receiver; the test passes if nothing panics.
    }

    #[tokio::test]
    async fn chunk_tool_call_id_matches_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = ToolNotificationHandle::from_sender(tx);

        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "echo test; sleep 0.15; echo done".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            output_byte_limit: 1024 * 1024,
            output_file: tmp.path().join("output.log"),
            notification_handle: handle,
            tool_call_id: "unique-id-abc".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        backend.run(request).await.unwrap();

        while let Ok(notification) = rx.try_recv() {
            if let crate::notification::types::ToolNotification::BashOutputChunk(chunk) =
                notification
            {
                assert_eq!(
                    chunk.base.tool_call_id, "unique-id-abc",
                    "Every chunk must carry the request's tool_call_id"
                );
            }
        }
    }

    #[tokio::test]
    async fn no_chunk_sent_when_no_new_output() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = ToolNotificationHandle::from_sender(tx);

        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "echo once; sleep 0.5".to_string(),
            working_directory: tmp.path().to_path_buf(),
            env: HashMap::new(),
            timeout: Duration::from_secs(5),
            output_byte_limit: 1024 * 1024,
            output_file: tmp.path().join("output.log"),
            notification_handle: handle,
            tool_call_id: "test-idle".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        backend.run(request).await.unwrap();

        let mut chunks = vec![];
        while let Ok(notification) = rx.try_recv() {
            if let crate::notification::types::ToolNotification::BashOutputChunk(chunk) =
                notification
            {
                chunks.push(chunk);
            }
        }

        assert!(
            chunks.len() <= 3,
            "Expected at most 3 chunks (not one per idle tick), got {}",
            chunks.len()
        );
    }

    #[tokio::test]
    async fn test_timeout_uses_sigterm_then_sigkill() {
        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_millis(200),
            output_byte_limit: 10000,
            output_file: tmp.path().join("timeout-test.out"),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-timeout-graceful".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert!(result.timed_out);
        assert_eq!(result.signal.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn test_output_preserved_on_timeout() {
        // 2s timeout so the poll loop gets enough ticks to read the echo
        // before the timeout handler snapshots the buffer.
        let backend = LocalTerminalBackend::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let request = TerminalRunRequest {
            command: "echo before_timeout; sleep 60".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(2),
            output_byte_limit: 10000,
            output_file: tmp.path().join("timeout-output.out"),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-timeout-output".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let result = backend.run(request).await.unwrap();
        assert!(result.timed_out);
        assert!(
            result.combined_output.contains("before_timeout"),
            "Timed-out output should contain 'before_timeout', got: {:?}",
            result.combined_output
        );
    }

    #[tokio::test]
    async fn test_background_child_with_inherited_pipe_does_not_block() {
        // `sleep 300 &` inherits the pipe — without drain timeout this blocks forever.
        let backend = LocalTerminalBackend::new();
        let request = TerminalRunRequest {
            command: "sleep 300 &\nsleep 1\necho done".to_string(),
            working_directory: PathBuf::from("/tmp"),
            env: HashMap::new(),
            timeout: Duration::from_secs(30),
            output_byte_limit: 10000,
            output_file: std::env::temp_dir().join(format!(
                "terminal-test-drain-timeout-{}.out",
                std::process::id()
            )),
            notification_handle: ToolNotificationHandle::noop(),
            tool_call_id: "test-drain-timeout".to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: TaskKind::Bash,
            owner_session_id: None,
            description: None,
        };

        let start = Instant::now();
        let result = backend.run(request).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "Command should complete in ~3s (1s sleep + 2s drain timeout), got {:?}",
            elapsed
        );
        assert_eq!(result.exit_code, Some(0));
        assert!(
            result.combined_output.contains("done"),
            "Output should contain 'done', got: {:?}",
            result.combined_output
        );
    }

    #[tokio::test]
    async fn draining_foreground_command_survives_a_kill() {
        let backend = std::sync::Arc::new(LocalTerminalBackend::new_with_tick_interval(
            Duration::from_millis(20),
        ));
        let run = tokio::spawn({
            let backend = backend.clone();
            async move { backend.run(make_request("sleep 5 &\necho done")).await }
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
        backend.kill_foreground_commands().await;

        let result = run.await.unwrap().expect("run returns a result");
        assert_eq!(result.exit_code, Some(0), "signal={:?}", result.signal);
        assert!(
            result.combined_output.contains("done"),
            "output={:?}",
            result.combined_output
        );
    }

    #[test]
    fn new_local_runs_on_current_thread_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let backend = LocalTerminalBackend::new_local(SearchShadowConfig::default());
            let result = backend
                .run(make_request("echo hello"))
                .await
                .expect("command should succeed");
            assert_eq!(result.exit_code, Some(0));
            assert!(result.combined_output.contains("hello"));
        });
    }

    #[test]
    fn new_local_sequential_commands_dont_stall() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let backend = LocalTerminalBackend::new_local(SearchShadowConfig::default());
            for i in 0..5 {
                let result = backend
                    .run(make_request(&format!("echo run_{i}")))
                    .await
                    .expect("command should succeed");
                assert_eq!(result.exit_code, Some(0));
                assert!(result.combined_output.contains(&format!("run_{i}")));
            }
        });
    }

    #[test]
    fn new_local_background_task_lifecycle() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let backend = LocalTerminalBackend::new_local(SearchShadowConfig::default());

            let mut bg_req = make_request("sleep 60");
            bg_req.tool_call_id = "bg-1".to_string();
            let bg = backend
                .run_background(bg_req)
                .await
                .expect("background spawn should succeed");

            let snap = backend
                .get_task(&bg.task_id)
                .await
                .expect("task should exist");
            assert!(!snap.completed);

            let outcome = backend.kill_task(&bg.task_id).await;
            assert!(
                matches!(outcome, KillOutcome::Killed | KillOutcome::AlreadyExited),
                "kill should succeed: {outcome:?}"
            );
        });
    }

    #[test]
    fn background_child_is_reaped_via_process_scope() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let scope = crate::util::ProcessScope::new();
            let backend = LocalTerminalBackend::new_local_with_scope(
                SearchShadowConfig::default(),
                scope.clone(),
                None,
            );

            let mut bg_req = make_request("sleep 120");
            bg_req.tool_call_id = "bg-scope-1".to_string();
            let bg = backend
                .run_background(bg_req)
                .await
                .expect("background spawn should succeed");

            assert!(
                !backend
                    .get_task(&bg.task_id)
                    .await
                    .expect("task should exist")
                    .completed,
                "task should be running before kill_all"
            );

            // Reap via the scope, exactly like the TUI exit handlers do.
            scope.kill_all();

            assert!(
                poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(10)).await,
                "kill_all did not reap the enrolled background child"
            );
        });
    }

    #[test]
    fn session_scoped_child_is_still_reaped_via_base_scope() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let base = crate::util::ProcessScope::new();
            let session = crate::util::ProcessScope::new();
            let backend = LocalTerminalBackend::new_local_with_scope(
                SearchShadowConfig::default(),
                base.clone(),
                Some(session),
            );

            let mut bg_req = make_request("sleep 120");
            bg_req.tool_call_id = "bg-dual-scope".to_string();
            let bg = backend
                .run_background(bg_req)
                .await
                .expect("background spawn should succeed");

            base.kill_all();

            assert!(
                poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(10)).await,
                "base-scope kill_all did not reap a session-scoped child"
            );
        });
    }

    #[test]
    fn reaped_background_child_leaves_scope_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let scope = crate::util::ProcessScope::new();
            let backend = LocalTerminalBackend::new_local_with_scope(
                SearchShadowConfig::default(),
                scope.clone(),
                None,
            );

            // A brief sleep, not `true`: the first poll tick fires right after
            // spawn and could reap `true` before the live_count == 1 read.
            let mut bg_req = make_request("sleep 1");
            bg_req.tool_call_id = "bg-reap-1".to_string();
            let bg = backend
                .run_background(bg_req)
                .await
                .expect("background spawn should succeed");

            // Enrollment is synchronous with the reply.
            assert_eq!(
                scope.live_count(),
                1,
                "spawn must enroll exactly one live group"
            );

            assert!(
                poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(10)).await,
                "background `sleep 1` was never reaped"
            );

            // The reap sweep runs in the same poll tick that sets exit_status,
            // so completion implies the Arc is already dropped.
            assert_eq!(
                scope.live_count(),
                0,
                "a reaped child must leave no live group enrolled in the scope"
            );
        });
    }

    // ================================================================
    // Persistent shell tests
    // ================================================================

    #[tokio::test]
    async fn test_persistent_shell_cd_persists() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let result = backend.run(make_request("cd /tmp")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend.run(make_request("pwd")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        let pwd = result.combined_output.trim();
        assert!(
            pwd == "/tmp" || pwd == "/private/tmp",
            "cwd should persist across commands, got: {pwd}"
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_env_var_persists() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let result = backend
            .run(make_request("export GROK_PERSIST_TEST=hello123"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend
            .run(make_request("echo $GROK_PERSIST_TEST"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.combined_output.trim(),
            "hello123",
            "env var should persist across commands"
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_clears_gpg_tty() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let mut req = make_request("echo \"[$GPG_TTY]\"");
        req.env
            .insert("GPG_TTY".to_string(), "/grok-sentinel-tty".to_string());

        let result = backend.run(req).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.combined_output.trim(),
            "[]",
            "GPG_TTY must be empty on the live tool path, got: {:?}",
            result.combined_output
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_function_persists() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let result = backend
            .run(make_request("myfunc() { echo \"called with $1\"; }"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend.run(make_request("myfunc test_arg")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.combined_output.trim(),
            "called with test_arg",
            "function should persist across commands"
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_variable_capture() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let result = backend
            .run(make_request("export CAPTURED=$(echo captured_value)"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend.run(make_request("echo $CAPTURED")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.combined_output.trim(),
            "captured_value",
            "variable from command substitution should persist"
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_deleted_cwd_falls_back_to_request_cwd() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let scratch = tempfile::TempDir::new().unwrap();
        let result = backend
            .run(make_request(&format!("cd {}", scratch.path().display())))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        drop(scratch);

        let result = backend.run(make_request("pwd")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        let output = &result.combined_output;
        assert!(
            output.contains("no longer exists"),
            "fallback warning must be in the command output, got: {output:?}"
        );
        let pwd = output.lines().last().unwrap_or_default().trim();
        assert!(
            pwd == "/tmp" || pwd == "/private/tmp",
            "command must run in the request working directory, got: {pwd:?}"
        );

        let result = backend.run(make_request("pwd")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(
            !result.combined_output.contains("no longer exists"),
            "state must heal after the fallback, got: {:?}",
            result.combined_output
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_spawn_error_names_missing_cwd() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let scratch = tempfile::TempDir::new().unwrap();
        let result = backend
            .run(make_request(&format!("cd {}", scratch.path().display())))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        drop(scratch);

        let gone = tempfile::TempDir::new().unwrap();
        let gone_path = gone.path().to_path_buf();
        drop(gone);
        let mut req = make_request("pwd");
        req.working_directory = gone_path.clone();

        let Err(err) = backend.run(req).await else {
            panic!("spawn must fail when both directories are missing");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("spawn shell in") && msg.contains(&gone_path.display().to_string()),
            "error must name the spawn directory, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_persistent_shell_does_not_inherit_dump_errexit() {
        let backend = LocalTerminalBackend::with_persistent_shell();

        let result = backend.run(make_request("true")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend
            .run(make_request("false; echo STILL_ALIVE"))
            .await
            .unwrap();
        assert_eq!(
            result.exit_code,
            Some(0),
            "a failing statement must not abort the command: {:?}",
            result.combined_output
        );
        assert!(
            result.combined_output.contains("STILL_ALIVE"),
            "execution must continue past a failing statement: {:?}",
            result.combined_output
        );
    }

    #[tokio::test]
    async fn test_non_persistent_shell_unaffected_by_deleted_cd_target() {
        let backend = LocalTerminalBackend::new();

        let scratch = tempfile::TempDir::new().unwrap();
        let result = backend
            .run(make_request(&format!("cd {}", scratch.path().display())))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        drop(scratch);

        let result = backend.run(make_request("pwd")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        let pwd = result.combined_output.trim();
        assert!(
            pwd == "/tmp" || pwd == "/private/tmp",
            "spawns must use the request cwd, got: {pwd:?}"
        );
    }

    #[test]
    fn test_parse_login_env_capture() {
        let stdout = "motd noise\n\x01/opt/rc/bin:/usr/bin\x01\
                      XDG_CONFIG_HOME=/Users/u/.config\0\
                      GH_CONFIG_DIR=/Users/u/.config/gh\0\
                      MULTILINE=a\nb\0\
                      PATH=/login/path\0\
                      PWD=/somewhere\0\
                      SHLVL=2\0\
                      GPG_TTY=/dev/ttys001\0\
                      http_proxy=http://p:3128\0\x01";
        let (path, env) = parse_login_env_capture(stdout);
        assert_eq!(path.as_deref(), Some("/opt/rc/bin:/usr/bin"));
        assert_eq!(
            env.get("XDG_CONFIG_HOME").map(String::as_str),
            Some("/Users/u/.config")
        );
        assert_eq!(
            env.get("GH_CONFIG_DIR").map(String::as_str),
            Some("/Users/u/.config/gh")
        );
        assert_eq!(env.get("MULTILINE").map(String::as_str), Some("a\nb"));
        for excluded in ["PATH", "PWD", "SHLVL", "GPG_TTY", "http_proxy"] {
            assert!(
                !env.contains_key(excluded),
                "{excluded} must be filtered from the captured login env"
            );
        }
    }

    #[test]
    fn test_parse_login_env_capture_path_only() {
        let (path, env) = parse_login_env_capture("\x01/usr/bin\x01");
        assert_eq!(path.as_deref(), Some("/usr/bin"));
        assert!(env.is_empty());
    }

    #[tokio::test]
    async fn test_non_persistent_shell_no_state() {
        let backend = LocalTerminalBackend::new();

        let result = backend
            .run(make_request("export SHOULD_NOT_PERSIST=yes"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));

        let result = backend
            .run(make_request("echo ${SHOULD_NOT_PERSIST:-empty}"))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.combined_output.trim(),
            "empty",
            "non-persistent mode should not carry state"
        );
    }

    #[tokio::test]
    async fn completed_bg_task_queryable_after_eviction() {
        let ttl = Duration::from_millis(200);
        let backend = LocalTerminalBackend::new_with_completed_task_ttl(ttl);

        let mut req = make_request("echo eviction_test_output");
        req.tool_call_id = "evict-test".to_string();
        let bg = backend
            .run_background(req)
            .await
            .expect("background spawn should succeed");

        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(5)))
            .await
            .expect("task should complete");
        assert!(snap.completed, "task should be completed");
        assert_eq!(snap.exit_code, Some(0));

        tokio::time::sleep(ttl + Duration::from_millis(200)).await;

        let snap_after = backend
            .get_task(&bg.task_id)
            .await
            .expect("task should still be queryable after eviction");
        assert!(snap_after.completed);
        assert_eq!(snap_after.exit_code, Some(0));
        assert_eq!(snap_after.task_id, bg.task_id);
        assert!(snap_after.output.is_empty());
        assert!(snap_after.output_total_bytes > 0);
        assert!(
            snap_after.truncated,
            "a tombstone that reports bytes must not claim complete output"
        );

        let all = backend.list_tasks().await;
        assert!(
            all.iter().any(|t| t.task_id == bg.task_id),
            "evicted task should appear in list_tasks"
        );

        let snap_wait = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_millis(100)))
            .await
            .expect("wait_for_completion should return evicted task");
        assert!(snap_wait.completed);
    }

    #[tokio::test]
    async fn wait_after_eviction_returns_snapshot_with_block_waited_true() {
        let ttl = Duration::from_millis(100);
        let backend = LocalTerminalBackend::new_with_completed_task_ttl(ttl);

        // No wait_for_completion before eviction: the tombstone must be born with
        // block_waited=false or the late-wait assertions below pass trivially.
        let mut req = make_request("echo evict_and_wait");
        req.tool_call_id = "evict-wait".to_string();
        let bg = backend
            .run_background(req)
            .await
            .expect("background spawn should succeed");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut completed = false;
        while Instant::now() < deadline {
            if let Some(snap) = backend.get_task(&bg.task_id).await
                && snap.completed
            {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(completed, "task should complete within deadline");

        tokio::time::sleep(ttl + Duration::from_millis(250)).await;

        let snap_pre_wait = backend
            .get_task(&bg.task_id)
            .await
            .expect("evicted task should still be queryable via tombstone");
        assert!(
            snap_pre_wait.completed,
            "tombstone should reflect completion"
        );
        assert!(
            !snap_pre_wait.block_waited,
            "tombstone must be born with block_waited=false (no wait_for_completion \
             was called before eviction); got block_waited=true which would make \
             the late-wait assertion below trivial"
        );

        let snap_after = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_millis(100)))
            .await
            .expect("late wait should still return evicted snapshot");
        assert!(snap_after.completed);
        assert!(
            snap_after.block_waited,
            "late wait must set block_waited=true on the returned snapshot"
        );

        let snap_via_get = backend
            .get_task(&bg.task_id)
            .await
            .expect("get_task should still return after eviction");
        assert!(
            snap_via_get.block_waited,
            "get_task after late wait must see the persisted block_waited flag \
             — the in-place mutation must be observable to other readers"
        );
    }

    #[tokio::test]
    async fn wait_on_already_completed_task_returns_immediately() {
        let backend = LocalTerminalBackend::new();
        let bg = backend
            .run_background(make_request("echo already-done"))
            .await
            .expect("spawn");
        assert!(
            poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(5)).await,
            "echo should finish"
        );

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(600)))
            .await
            .expect("completed snapshot");
        assert!(snap.completed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "already-completed wait must not burn the 600s cap; elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_on_already_killed_task_returns_immediately() {
        let backend = LocalTerminalBackend::new();
        let bg = backend
            .run_background(make_request("sleep 60"))
            .await
            .expect("spawn");
        assert_eq!(backend.kill_task(&bg.task_id).await, KillOutcome::Killed);
        assert!(
            poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(5)).await,
            "kill should complete the task"
        );

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(600)))
            .await
            .expect("killed snapshot");
        assert!(snap.completed);
        assert!(snap.explicitly_killed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "already-killed wait must not burn the 600s cap; elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_on_tombstoned_task_returns_immediately() {
        let ttl = Duration::from_millis(100);
        let backend = LocalTerminalBackend::new_with_completed_task_ttl(ttl);
        let bg = backend
            .run_background(make_request("echo tombstone-wait"))
            .await
            .expect("spawn");
        assert!(
            poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(5)).await,
            "echo should finish"
        );
        tokio::time::sleep(ttl + Duration::from_millis(250)).await;

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(600)))
            .await
            .expect("tombstone snapshot");
        assert!(snap.completed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "tombstone wait must not burn the 600s cap; elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_on_unknown_task_returns_immediately() {
        let backend = LocalTerminalBackend::new();
        let started = Instant::now();
        let snap = backend
            .wait_for_completion("never-existed", Some(Duration::from_secs(600)))
            .await;
        assert!(snap.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "not-found wait must not burn the 600s cap; elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_on_still_running_task_times_out() {
        let backend = LocalTerminalBackend::new();
        let bg = backend
            .run_background(make_request("sleep 60"))
            .await
            .expect("spawn");

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_millis(200)))
            .await
            .expect("timeout snapshot");
        assert!(
            !snap.completed,
            "still-running wait must not take the already-terminal path"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "still-running wait must block until its timeout; elapsed {:?}",
            started.elapsed()
        );
        let _ = backend.kill_task(&bg.task_id).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completion_mid_wait_wakes_waiter_before_tick() {
        let backend = LocalTerminalBackend::new_with_tick_interval(Duration::from_secs(30));
        // Let the interval's immediate first tick pass so the wake below can
        // only come from the child-exit signal.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bg = backend
            .run_background(make_request("sleep 0.3; echo done"))
            .await
            .expect("spawn");

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(600)))
            .await
            .expect("completed snapshot");
        assert!(snap.completed);
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "wait must resolve on child exit (~0.3s), not the 30s tick; waited {waited:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn due_deadline_beats_unrelated_drain_work() {
        let backend = LocalTerminalBackend::new_with_tick_interval(Duration::from_millis(100));
        let drainer = backend
            .run_background(make_request("(sleep 60 &); exit 0"))
            .await
            .expect("spawn drainer");
        let sleeper = backend
            .run_background(make_request("sleep 60"))
            .await
            .expect("spawn sleeper");

        let started = Instant::now();
        let snap = tokio::time::timeout(
            Duration::from_secs(5),
            backend.wait_for_completion(&sleeper.task_id, Some(Duration::from_millis(300))),
        )
        .await
        .expect("deadline must fire on time despite drain work")
        .expect("timeout snapshot");
        assert!(!snap.completed);
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(1),
            "due deadline queued behind another task's drain: waited {waited:?}"
        );
        let _ = backend.kill_task(&drainer.task_id).await;
        let _ = backend.kill_task(&sleeper.task_id).await;
    }

    #[tokio::test]
    async fn wait_timeout_fires_at_deadline_not_tick() {
        let backend = LocalTerminalBackend::new_with_tick_interval(Duration::from_secs(30));
        let bg = backend
            .run_background(make_request("sleep 60"))
            .await
            .expect("spawn");

        let started = Instant::now();
        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_millis(300)))
            .await
            .expect("timeout snapshot");
        assert!(!snap.completed, "timeout must return the running snapshot");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(250),
            "timeout must not fire early; waited {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "timeout must fire at its deadline, not the 30s tick; waited {waited:?}"
        );
        let _ = backend.kill_task(&bg.task_id).await;
    }

    #[tokio::test]
    async fn cancelled_wait_does_not_suppress_auto_wake_on_completion() {
        let backend = LocalTerminalBackend::new();

        let mut req = make_request("sleep 1; echo done");
        req.tool_call_id = "cancelled-wait".to_string();
        let bg = backend
            .run_background(req)
            .await
            .expect("background spawn should succeed");

        // Dropping the wait future drops the oneshot receiver, mirroring a turn
        // abort while `get_task_output` blocks.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            backend.wait_for_completion(&bg.task_id, Some(Duration::from_secs(120))),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "wait must still be pending when the caller is cancelled"
        );

        assert!(
            poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(10)).await,
            "task should complete within deadline"
        );

        let snap = backend
            .get_task(&bg.task_id)
            .await
            .expect("completed task should be queryable");
        assert!(
            !snap.block_waited,
            "a cancelled (never-delivered) blocking wait must not leave \
             block_waited=true — that suppresses the completion auto-wake"
        );
    }

    #[tokio::test]
    async fn cancelled_wait_alongside_live_waiter_keeps_block_waited() {
        let backend = LocalTerminalBackend::new();

        let mut req = make_request("sleep 1; echo done");
        req.tool_call_id = "mixed-waiters".to_string();
        let bg = backend
            .run_background(req)
            .await
            .expect("background spawn should succeed");

        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            backend.wait_for_completion(&bg.task_id, Some(Duration::from_secs(120))),
        )
        .await;
        assert!(cancelled.is_err(), "first wait should be cancelled");

        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(30)))
            .await
            .expect("live wait should return the completed snapshot");
        assert!(snap.completed, "live wait should observe completion");
        assert!(
            snap.block_waited,
            "delivered wait must keep block_waited=true on the returned snapshot"
        );

        let snap_via_get = backend
            .get_task(&bg.task_id)
            .await
            .expect("completed task should be queryable");
        assert!(
            snap_via_get.block_waited,
            "block_waited must remain true when at least one waiter received \
             the completion — auto-wake would be redundant"
        );
    }

    #[tokio::test]
    async fn kill_with_live_waiter_marks_result_delivered() {
        for source in [KillSource::ClientUi, KillSource::ModelTool] {
            let backend = LocalTerminalBackend::new();
            let mut req = make_request("sleep 60");
            req.tool_call_id = format!("live-waiter-{source:?}");
            let bg = backend.run_background(req).await.expect("spawn");

            let waiter_backend = backend.clone();
            let task_id = bg.task_id.clone();
            let wait = tokio::spawn(async move {
                waiter_backend
                    .wait_for_completion(&task_id, Some(Duration::from_secs(30)))
                    .await
            });
            let waiter_deadline = std::time::Instant::now() + Duration::from_secs(2);
            let waiter_ready = loop {
                if backend
                    .get_task(&bg.task_id)
                    .await
                    .is_some_and(|s| s.block_waited)
                {
                    break true;
                }
                if std::time::Instant::now() >= waiter_deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert!(
                waiter_ready,
                "waiter must register before kill ({source:?})"
            );

            assert_eq!(
                backend.kill_task_with_source(&bg.task_id, source).await,
                KillOutcome::Killed
            );
            let snap = backend.get_task(&bg.task_id).await.expect("killed task");
            assert!(snap.explicitly_killed, "{source:?}");
            assert!(
                snap.kill_result_delivered,
                "live waiter must mark kill_result_delivered ({source:?})"
            );
            assert!(
                snap.block_waited,
                "live waiter must keep block_waited ({source:?})"
            );
            assert!(
                snap.is_auto_wake_suppressed(),
                "delivered kill must suppress ({source:?})"
            );
            let waited = wait.await.expect("join").expect("wait snapshot");
            assert!(waited.completed, "{source:?}");
        }
    }

    #[tokio::test]
    async fn kill_with_dropped_waiter_clears_block_waited() {
        for source in [KillSource::ClientUi, KillSource::ModelTool] {
            let backend = LocalTerminalBackend::new();
            let mut req = make_request("sleep 60");
            req.tool_call_id = format!("dropped-waiter-{source:?}");
            let bg = backend.run_background(req).await.expect("spawn");

            let cancelled = tokio::time::timeout(
                Duration::from_millis(100),
                backend.wait_for_completion(&bg.task_id, Some(Duration::from_secs(120))),
            )
            .await;
            assert!(cancelled.is_err(), "wait must be cancelled ({source:?})");

            assert_eq!(
                backend.kill_task_with_source(&bg.task_id, source).await,
                KillOutcome::Killed
            );
            let snap = backend.get_task(&bg.task_id).await.expect("killed task");
            assert!(snap.explicitly_killed, "{source:?}");
            assert!(
                !snap.block_waited,
                "dropped waiter must clear block_waited ({source:?})"
            );
            let expect_delivered = source.marks_result_delivered(false);
            assert_eq!(snap.kill_result_delivered, expect_delivered, "{source:?}");
            assert_eq!(
                snap.is_auto_wake_suppressed(),
                expect_delivered,
                "ClientUi + dropped waiter must wake; ModelTool still suppresses ({source:?})"
            );
        }
    }

    #[tokio::test]
    async fn kill_without_waiter_delivery_depends_on_source() {
        // Hardcoded per source so a no-op handle_kill or a formula change fails.
        for (source, expect_delivered) in [
            (KillSource::ClientUi, false),
            (KillSource::ModelTool, true),
            (KillSource::Teardown, true),
        ] {
            let backend = LocalTerminalBackend::new();
            let mut req = make_request("sleep 60");
            req.tool_call_id = format!("no-waiter-{source:?}");
            let bg = backend.run_background(req).await.expect("spawn");

            assert_eq!(
                backend.kill_task_with_source(&bg.task_id, source).await,
                KillOutcome::Killed
            );
            let snap = backend.get_task(&bg.task_id).await.expect("killed task");
            assert!(snap.explicitly_killed, "{source:?}");
            assert!(!snap.block_waited, "{source:?}");
            assert_eq!(snap.kill_result_delivered, expect_delivered, "{source:?}");
            assert_eq!(
                snap.is_auto_wake_suppressed(),
                expect_delivered,
                "{source:?}"
            );
        }
    }

    #[tokio::test]
    async fn kill_task_defaults_to_model_tool_source() {
        let backend = LocalTerminalBackend::new();
        let mut req = make_request("sleep 60");
        req.tool_call_id = "default-model-tool".into();
        let bg = backend.run_background(req).await.expect("spawn");
        assert_eq!(backend.kill_task(&bg.task_id).await, KillOutcome::Killed);
        let snap = backend.get_task(&bg.task_id).await.expect("killed task");
        assert!(snap.explicitly_killed);
        assert!(
            snap.kill_result_delivered,
            "bare kill_task is a model-tool kill"
        );
        assert!(snap.is_auto_wake_suppressed());
    }

    #[tokio::test]
    async fn teardown_sweeps_mark_result_delivered() {
        let backend = LocalTerminalBackend::new();
        let mut owned = make_request("sleep 60");
        owned.tool_call_id = "teardown-owned".into();
        owned.owner_session_id = Some("session-a".into());
        let owned = backend.run_background(owned).await.expect("spawn");

        let mut unowned = make_request("sleep 60");
        unowned.tool_call_id = "teardown-all".into();
        let unowned = backend.run_background(unowned).await.expect("spawn");

        backend
            .kill_all_background_tasks_by_owner("session-a")
            .await;
        let snap = backend.get_task(&owned.task_id).await.expect("owned");
        assert!(snap.completed && snap.explicitly_killed);
        assert!(
            snap.kill_result_delivered && snap.is_auto_wake_suppressed(),
            "owner teardown must suppress auto-wake"
        );

        backend.kill_all_background_tasks().await;
        let snap = backend.get_task(&unowned.task_id).await.expect("unowned");
        assert!(snap.completed && snap.explicitly_killed);
        assert!(
            snap.kill_result_delivered && snap.is_auto_wake_suppressed(),
            "global teardown must suppress auto-wake"
        );
    }

    #[tokio::test]
    async fn kill_bits_survive_ttl_eviction() {
        let ttl = Duration::from_millis(100);
        let backend = LocalTerminalBackend::new_with_completed_task_ttl(ttl);
        for (source, expect_delivered) in
            [(KillSource::ModelTool, true), (KillSource::ClientUi, false)]
        {
            let mut req = make_request("sleep 60");
            req.tool_call_id = format!("evict-kill-{source:?}");
            let bg = backend.run_background(req).await.expect("spawn");
            assert_eq!(
                backend.kill_task_with_source(&bg.task_id, source).await,
                KillOutcome::Killed
            );
            assert!(
                poll_until_task_completed(&backend, &bg.task_id, Duration::from_secs(5)).await,
                "killed task should complete ({source:?})"
            );
            tokio::time::sleep(ttl + Duration::from_millis(250)).await;
            let snap = backend
                .get_task(&bg.task_id)
                .await
                .expect("tombstone must remain queryable");
            assert!(snap.completed, "{source:?}");
            assert!(snap.explicitly_killed, "{source:?}");
            assert_eq!(snap.kill_result_delivered, expect_delivered, "{source:?}");
            assert_eq!(
                snap.is_auto_wake_suppressed(),
                expect_delivered,
                "{source:?}"
            );
        }
    }

    fn make_owned_request(command: &str, owner: &str) -> TerminalRunRequest {
        let mut req = make_request(command);
        req.owner_session_id = Some(owner.to_string());
        req
    }

    #[tokio::test]
    async fn kill_foreground_by_owner_only_kills_matching_session() {
        let backend = LocalTerminalBackend::new();

        let mut req_a = make_owned_request("sleep 60", "session-a");
        req_a.tool_call_id = "fg-a".to_string();
        req_a.timeout = Duration::from_secs(300);

        let mut req_b = make_owned_request("sleep 60", "session-b");
        req_b.tool_call_id = "fg-b".to_string();
        req_b.timeout = Duration::from_secs(300);

        let handle_a = backend.run_background(req_a).await.unwrap();
        let handle_b = backend.run_background(req_b).await.unwrap();

        let snap_a = backend.get_task(&handle_a.task_id).await;
        let snap_b = backend.get_task(&handle_b.task_id).await;
        assert!(snap_a.is_some(), "task A should exist");
        assert!(snap_b.is_some(), "task B should exist");

        backend
            .kill_all_background_tasks_by_owner("session-a")
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let snap_a = backend.get_task(&handle_a.task_id).await;
        assert!(
            snap_a.is_some_and(|s| s.completed && s.explicitly_killed),
            "task A should be killed"
        );

        let snap_b = backend.get_task(&handle_b.task_id).await;
        assert!(
            snap_b.is_some_and(|s| !s.completed),
            "task B should still be running"
        );

        backend.kill_task(&handle_b.task_id).await;
    }

    #[tokio::test]
    async fn kill_by_owner_ignores_unowned_tasks() {
        let backend = LocalTerminalBackend::new();

        let mut req = make_request("sleep 60");
        req.tool_call_id = "fg-none".to_string();
        let handle = backend.run_background(req).await.unwrap();

        backend
            .kill_all_background_tasks_by_owner("some-session")
            .await;

        tokio::time::sleep(Duration::from_millis(100)).await;

        let snap = backend.get_task(&handle.task_id).await;
        assert!(
            snap.is_some_and(|s| !s.completed),
            "unowned task should NOT be killed by owner-scoped kill"
        );

        backend.kill_task(&handle.task_id).await;
    }

    #[tokio::test]
    async fn reparent_notifications_changes_owner_and_sends_synthetic() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let new_handle = ToolNotificationHandle::from_sender(tx);

        let backend: std::sync::Arc<dyn crate::computer::types::TerminalBackend> =
            std::sync::Arc::new(LocalTerminalBackend::new());

        let mut req = make_owned_request("sleep 60", "child-session");
        req.tool_call_id = "reparent-test".to_string();
        let bg = backend.run_background(req).await.unwrap();

        let snap = backend.get_task(&bg.task_id).await.unwrap();
        assert!(!snap.completed);
        assert_eq!(snap.owner_session_id.as_deref(), Some("child-session"));

        backend
            .reparent_notifications(
                "child-session",
                "parent-session",
                new_handle,
                std::sync::Arc::downgrade(&backend),
            )
            .await;

        let snap = backend.get_task(&bg.task_id).await.unwrap();
        assert_eq!(
            snap.owner_session_id.as_deref(),
            Some("parent-session"),
            "owner should be reparented to parent-session"
        );

        let mut found_backgrounded = false;
        while let Ok(notification) = rx.try_recv() {
            if let crate::notification::types::ToolNotification::BashExecutionBackgrounded(
                bg_notif,
            ) = notification
            {
                assert_eq!(bg_notif.task_id, bg.task_id);
                found_backgrounded = true;
            }
        }
        assert!(
            found_backgrounded,
            "reparent should send a synthetic BashExecutionBackgrounded notification"
        );

        backend
            .kill_all_background_tasks_by_owner("parent-session")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let snap = backend.get_task(&bg.task_id).await;
        assert!(
            snap.is_some_and(|s| s.completed),
            "reparented task should be killed when parent-session tasks are killed"
        );
    }

    #[tokio::test]
    async fn reparent_skips_tasks_owned_by_other_sessions() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let new_handle = ToolNotificationHandle::from_sender(tx);

        let backend: std::sync::Arc<dyn crate::computer::types::TerminalBackend> =
            std::sync::Arc::new(LocalTerminalBackend::new());

        let mut req_child = make_owned_request("sleep 60", "child-session");
        req_child.tool_call_id = "reparent-child".to_string();
        let bg_child = backend.run_background(req_child).await.unwrap();

        let mut req_sibling = make_owned_request("sleep 60", "sibling-session");
        req_sibling.tool_call_id = "reparent-sibling".to_string();
        let bg_sibling = backend.run_background(req_sibling).await.unwrap();

        backend
            .reparent_notifications(
                "child-session",
                "parent-session",
                new_handle,
                std::sync::Arc::downgrade(&backend),
            )
            .await;

        let snap_child = backend.get_task(&bg_child.task_id).await.unwrap();
        assert_eq!(
            snap_child.owner_session_id.as_deref(),
            Some("parent-session")
        );

        let snap_sibling = backend.get_task(&bg_sibling.task_id).await.unwrap();
        assert_eq!(
            snap_sibling.owner_session_id.as_deref(),
            Some("sibling-session"),
            "sibling task should not be reparented"
        );

        let mut bg_count = 0;
        while let Ok(notification) = rx.try_recv() {
            if matches!(
                notification,
                crate::notification::types::ToolNotification::BashExecutionBackgrounded(_)
            ) {
                bg_count += 1;
            }
        }
        assert_eq!(
            bg_count, 1,
            "only the child task should produce a synthetic notification"
        );

        backend.kill_task(&bg_child.task_id).await;
        backend.kill_task(&bg_sibling.task_id).await;
    }

    #[tokio::test]
    async fn owner_session_id_propagated_through_run_and_snapshot() {
        let backend = LocalTerminalBackend::new();

        let mut req = make_owned_request("echo owned", "test-owner");
        req.tool_call_id = "owned-test".to_string();
        let bg = backend.run_background(req).await.unwrap();

        let snap = backend
            .wait_for_completion(&bg.task_id, Some(Duration::from_secs(5)))
            .await
            .expect("task should complete");

        assert!(snap.completed);
        assert_eq!(
            snap.owner_session_id.as_deref(),
            Some("test-owner"),
            "owner_session_id should propagate from request to snapshot"
        );
    }
}
