// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashMap;
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
const MAX_STDOUT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 1024 * 1024;
const TERM_GRACE: Duration = Duration::from_millis(250);
const CHILD_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
pub const GIT_DEADLINE: Duration = grove_git::DEFAULT_GIT_DEADLINE;

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub struct CgroupV2 {
    path: PathBuf,
}

impl CgroupV2 {
    pub fn create(label: &str) -> Result<Option<Arc<Self>>> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = label;
            Ok(None)
        }
        #[cfg(target_os = "linux")]
        {
            let current = std::fs::read_to_string("/proc/self/cgroup")?;
            let Some(relative) = current.lines().find_map(|line| line.strip_prefix("0::")) else {
                return Ok(None);
            };
            let parent = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
            if !parent.join("cgroup.controllers").is_file() {
                return Ok(None);
            }
            let path = parent.join(format!("grok-lifecycle-{label}"));
            match std::fs::create_dir(&path) {
                Ok(()) => Ok(Some(Arc::new(Self { path }))),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::ReadOnlyFilesystem
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error).context("create lifecycle cgroup"),
            }
        }
    }

    pub fn support_available() -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        let label = format!("probe-{}", uuid::Uuid::new_v4().simple());
        let Ok(Some(cgroup)) = Self::create(&label) else {
            return false;
        };
        let mut command = Command::new("sh");
        distill_tty_utils::detach_std_command(&mut command);
        command.args(["-c", "kill -STOP $$; sleep 30"]);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        #[allow(clippy::disallowed_methods)]
        // Probe child is immediately stopped, migrated, killed, and reaped.
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let pid = child.id();
        let ok = wait_for_stopped_child(pid).is_ok()
            && cgroup.add_process(pid).is_ok()
            && cgroup.kill_all().is_ok()
            && distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT)
                .is_ok_and(|status| status.is_some())
            && cgroup
                .member_pids_internal()
                .is_ok_and(|pids| pids.is_empty());
        if !ok {
            let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            let _ = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
        }
        ok
    }

    fn add_process(&self, pid: u32) -> Result<()> {
        std::fs::write(self.path.join("cgroup.procs"), pid.to_string())
            .context("enroll lifecycle worker in cgroup")
    }

    pub fn kill_all(&self) -> Result<()> {
        std::fs::write(self.path.join("cgroup.kill"), "1").context("kill lifecycle cgroup")
    }

    fn member_pids_internal(&self) -> Result<Vec<u32>> {
        let text = std::fs::read_to_string(self.path.join("cgroup.procs"))?;
        Ok(text.lines().filter_map(|line| line.parse().ok()).collect())
    }

    #[cfg(test)]
    pub fn member_pids(&self) -> Result<Vec<u32>> {
        self.member_pids_internal()
    }

    #[cfg(test)]
    pub fn remove_for_test(&self) -> Result<()> {
        std::fs::remove_dir(&self.path).context("remove lifecycle cgroup for test")
    }
}

impl Drop for CgroupV2 {
    fn drop(&mut self) {
        let _ = self.kill_all();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {
            let empty = std::fs::read_to_string(self.path.join("cgroup.procs"))
                .is_ok_and(|text| text.trim().is_empty());
            if empty && std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::yield_now();
        }
    }
}

#[derive(Clone)]
pub struct CommandRegistry {
    inner: Arc<CommandRegistryInner>,
}

struct CommandRegistryInner {
    next_id: AtomicU64,
    enrollment: Mutex<()>,
    commands: Mutex<HashMap<u64, ActiveCommand>>,
}

#[derive(Clone)]
struct ActiveCommand {
    group: Arc<distill_tty_utils::ProcessGroup>,
    cgroup: Option<Arc<CgroupV2>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CommandRegistryInner {
                next_id: AtomicU64::new(1),
                enrollment: Mutex::new(()),
                commands: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn register(
        &self,
        group: Arc<distill_tty_utils::ProcessGroup>,
        cgroup: Option<Arc<CgroupV2>>,
    ) -> CommandGuard {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, ActiveCommand { group, cgroup });
        CommandGuard {
            registry: self.clone(),
            id,
        }
    }

    pub fn terminate_all(&self) -> Vec<String> {
        let enrollment = self
            .inner
            .enrollment
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let groups = self
            .inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for command in groups {
            attempt_command_teardown(command.cgroup.as_deref(), &command.group, &mut failures);
        }
        drop(enrollment);
        let started = Instant::now();
        while self.live_count_for_artifact() != 0 && started.elapsed() < READER_JOIN_TIMEOUT {
            std::thread::yield_now();
        }
        if self.live_count_for_artifact() != 0 {
            failures.push("commands did not unregister after bounded reader drain".into());
        }
        failures
    }

    pub fn live_count_for_artifact(&self) -> usize {
        self.inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

struct CommandGuard {
    registry: CommandRegistry,
    id: u64,
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        self.registry
            .inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

pub fn run_git(registry: &CommandRegistry, cwd: &Path, args: &[&str]) -> Result<CommandOutput> {
    let mut command = grove_git::hermetic_git_command().context("resolve hermetic git")?;
    command.args(args).current_dir(cwd);
    run_contained_command(registry, command, GIT_DEADLINE)
        .with_context(|| format!("git {}", args.first().copied().unwrap_or("command")))
}

pub fn run_contained_command(
    registry: &CommandRegistry,
    command: Command,
    deadline: Duration,
) -> Result<CommandOutput> {
    run_contained_command_with_cgroup(registry, command, deadline, None)
}

pub fn run_contained_command_with_cgroup(
    registry: &CommandRegistry,
    command: Command,
    deadline: Duration,
    cgroup: Option<Arc<CgroupV2>>,
) -> Result<CommandOutput> {
    run_contained_command_impl(
        registry,
        command,
        deadline,
        cgroup,
        false,
        #[cfg(test)]
        ReaderSpawnFailure::None,
    )
}

pub fn run_stopped_worker_with_cgroup(
    registry: &CommandRegistry,
    command: Command,
    deadline: Duration,
    cgroup: Arc<CgroupV2>,
) -> Result<CommandOutput> {
    run_contained_command_impl(
        registry,
        command,
        deadline,
        Some(cgroup),
        true,
        #[cfg(test)]
        ReaderSpawnFailure::None,
    )
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderSpawnFailure {
    None,
    First,
    Second,
}

#[cfg(test)]
pub fn run_contained_command_with_reader_failure(
    registry: &CommandRegistry,
    command: Command,
    cgroup: Option<Arc<CgroupV2>>,
    failure: ReaderSpawnFailure,
) -> Result<CommandOutput> {
    run_contained_command_impl(
        registry,
        command,
        Duration::from_secs(30),
        cgroup,
        false,
        failure,
    )
}

fn run_contained_command_impl(
    registry: &CommandRegistry,
    command: Command,
    deadline: Duration,
    cgroup: Option<Arc<CgroupV2>>,
    child_self_stops: bool,
    #[cfg(test)] reader_spawn_failure: ReaderSpawnFailure,
) -> Result<CommandOutput> {
    let starts_stopped = cgroup.is_some();
    let mut command = if starts_stopped && !child_self_stops {
        stopped_command(command)
    } else {
        command
    };
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(distill_tty_utils::null_stdio());
    let enrollment = registry
        .inner
        .enrollment
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    #[allow(clippy::disallowed_methods)] // Enrollment lock closes spawn/signal ownership race.
    let mut child = command.spawn().context("spawn contained subprocess")?;
    let scope = distill_tty_utils::ProcessScope::new();
    let group = match scope.enroll_std(&child) {
        Ok(group) => group,
        Err(error) => {
            kill_and_reap_unenrolled_child(&mut child);
            return Err(error).context("enroll subprocess group");
        }
    };
    if starts_stopped {
        wait_for_stopped_child(child.id()).inspect_err(|_| {
            let _ = group.kill();
            let _ = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
        })?;
    }
    if let Some(cgroup) = cgroup.as_deref()
        && let Err(error) = cgroup.add_process(child.id())
    {
        let _ = group.kill();
        let _ = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
        return Err(error);
    }
    let command_guard = registry.register(Arc::clone(&group), cgroup.clone());
    if starts_stopped {
        send_continue(child.id()).inspect_err(|_| {
            let _ = group.kill();
            let _ = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
        })?;
    }
    drop(enrollment);
    let stdout = child.stdout.take().context("capture subprocess stdout")?;
    let stderr = child.stderr.take().context("capture subprocess stderr")?;
    #[cfg(test)]
    let first_reader = (reader_spawn_failure == ReaderSpawnFailure::First)
        .then_some("injected first subprocess output reader failure");
    #[cfg(not(test))]
    let first_reader = None;
    let stdout_reader =
        match spawn_reader("lifecycle-stdout", stdout, MAX_STDOUT_BYTES, first_reader) {
            Ok(reader) => reader,
            Err(error) => {
                return cleanup_after_reader_spawn_failure(
                    child,
                    command_guard,
                    &group,
                    cgroup.as_deref(),
                    None,
                    error,
                );
            }
        };
    #[cfg(test)]
    let second_reader = (reader_spawn_failure == ReaderSpawnFailure::Second)
        .then_some("injected second subprocess output reader failure");
    #[cfg(not(test))]
    let second_reader = None;
    let stderr_reader =
        match spawn_reader("lifecycle-stderr", stderr, MAX_STDERR_BYTES, second_reader) {
            Ok(reader) => reader,
            Err(error) => {
                return cleanup_after_reader_spawn_failure(
                    child,
                    command_guard,
                    &group,
                    cgroup.as_deref(),
                    Some(stdout_reader),
                    error,
                );
            }
        };

    let status = match distill_tty_utils::wait_child_bounded(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let mut cleanup_errors = Vec::new();
            attempt_command_teardown(cgroup.as_deref(), &group, &mut cleanup_errors);
            let reap = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
            if !matches!(reap, Ok(Some(_))) {
                cleanup_errors.push(match &reap {
                    Ok(None) => "child did not reap after bounded teardown".into(),
                    Err(error) => format!("wait after teardown: {error}"),
                    Ok(Some(_)) => unreachable!("covered by matches"),
                });
                let _ = distill_tty_utils::spawn_child_reaper(
                    "lifecycle-timeout-reaper",
                    child,
                    Some(Arc::clone(&group)),
                );
            }
            let stdout = join_reader_bounded(stdout_reader, "stdout", READER_JOIN_TIMEOUT);
            let stderr = join_reader_bounded(stderr_reader, "stderr", READER_JOIN_TIMEOUT);
            cleanup_errors.extend(reader_failures(&stdout, &stderr));
            drop(command_guard);
            bail!(
                "subprocess exceeded {}s deadline; cleanup={cleanup_errors:?}",
                deadline.as_secs_f64()
            );
        }
        Err(error) => {
            let mut cleanup_errors = Vec::new();
            attempt_command_teardown(cgroup.as_deref(), &group, &mut cleanup_errors);
            if !distill_tty_utils::is_child_wait_identity_uncertain(&error) {
                let reap = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
                if !matches!(reap, Ok(Some(_))) {
                    cleanup_errors.push(match &reap {
                        Ok(None) => "child did not reap after wait error teardown".into(),
                        Err(reap_error) => format!("wait after teardown: {reap_error}"),
                        Ok(Some(_)) => unreachable!("covered by matches"),
                    });
                    let _ = distill_tty_utils::spawn_child_reaper(
                        "lifecycle-wait-error-reaper",
                        child,
                        Some(Arc::clone(&group)),
                    );
                }
            } else {
                let _ = distill_tty_utils::spawn_child_reaper(
                    "lifecycle-uncertain-wait-reaper",
                    child,
                    Some(Arc::clone(&group)),
                );
            }
            let stdout = join_reader_bounded(stdout_reader, "stdout", READER_JOIN_TIMEOUT);
            let stderr = join_reader_bounded(stderr_reader, "stderr", READER_JOIN_TIMEOUT);
            cleanup_errors.extend(reader_failures(&stdout, &stderr));
            drop(command_guard);
            return Err(error).with_context(|| {
                format!("wait for contained subprocess; cleanup={cleanup_errors:?}")
            });
        }
    };
    let mut cleanup_errors = Vec::new();
    attempt_command_teardown(cgroup.as_deref(), &group, &mut cleanup_errors);
    let stdout = join_reader_bounded(stdout_reader, "stdout", READER_JOIN_TIMEOUT);
    let stderr = join_reader_bounded(stderr_reader, "stderr", READER_JOIN_TIMEOUT);
    cleanup_errors.extend(reader_failures(&stdout, &stderr));
    if !cleanup_errors.is_empty() {
        drop(command_guard);
        bail!("subprocess teardown failed: {cleanup_errors:?}");
    }
    let stdout = stdout?;
    let stderr = stderr?;
    drop(command_guard);
    drop(group);
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(target_os = "linux")]
fn stopped_command(command: Command) -> Command {
    let mut shell = Command::new("sh");
    distill_tty_utils::detach_std_command(&mut shell);
    let mut words = command.get_args().map(|arg| arg.as_bytes().to_vec());
    let program = command.get_program().as_bytes().to_vec();
    let mut script = b"kill -STOP $$; exec \"$0\"".to_vec();
    for index in 0..words.len() {
        script.extend_from_slice(format!(" \"${}\"", index + 1).as_bytes());
    }
    shell.arg("-c").arg(OsString::from_vec(script));
    shell.arg(OsString::from_vec(program));
    for word in words.by_ref() {
        shell.arg(OsString::from_vec(word));
    }
    shell.current_dir(command.get_current_dir().unwrap_or_else(|| Path::new(".")));
    shell.envs(
        command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    shell
}

#[cfg(not(target_os = "linux"))]
fn stopped_command(command: Command) -> Command {
    command
}

fn kill_and_reap_unenrolled_child(child: &mut std::process::Child) {
    // SAFETY: kill targets the direct child PID, which this caller still owns.
    let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = distill_tty_utils::wait_child_bounded(child, CHILD_KILL_REAP_TIMEOUT);
}

#[cfg(target_os = "linux")]
fn wait_for_stopped_child(pid: u32) -> Result<()> {
    let mut status = 0;
    // SAFETY: waitpid writes one status integer for our direct child PID.
    let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WUNTRACED) };
    if waited < 0 {
        return Err(std::io::Error::last_os_error()).context("wait for stopped worker");
    }
    ensure_stopped_status(status)
}

#[cfg(not(target_os = "linux"))]
fn wait_for_stopped_child(_pid: u32) -> Result<()> {
    bail!("stopped worker enrollment unsupported on this platform")
}

#[cfg(target_os = "linux")]
fn ensure_stopped_status(status: libc::c_int) -> Result<()> {
    if libc::WIFSTOPPED(status) {
        Ok(())
    } else {
        bail!("worker exited before cgroup enrollment")
    }
}

#[cfg(target_os = "linux")]
fn send_continue(pid: u32) -> Result<()> {
    // SAFETY: kill targets the exact worker PID with SIGCONT.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("continue enrolled worker")
    }
}

#[cfg(not(target_os = "linux"))]
fn send_continue(_pid: u32) -> Result<()> {
    bail!("stopped worker enrollment unsupported on this platform")
}

fn cleanup_after_reader_spawn_failure(
    mut child: std::process::Child,
    command_guard: CommandGuard,
    group: &Arc<distill_tty_utils::ProcessGroup>,
    cgroup: Option<&CgroupV2>,
    started_reader: Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>,
    primary: anyhow::Error,
) -> Result<CommandOutput> {
    let mut cleanup_errors = Vec::new();
    attempt_command_teardown(cgroup, group, &mut cleanup_errors);
    let reap = distill_tty_utils::wait_child_bounded(&mut child, CHILD_KILL_REAP_TIMEOUT);
    if !matches!(reap, Ok(Some(_))) {
        cleanup_errors.push(match &reap {
            Ok(None) => "child did not reap after reader spawn failure".into(),
            Err(error) => format!("wait after reader spawn failure: {error}"),
            Ok(Some(_)) => unreachable!("covered by matches"),
        });
        let _ = distill_tty_utils::spawn_child_reaper(
            "lifecycle-reader-spawn-failure-reaper",
            child,
            Some(Arc::clone(group)),
        );
    }
    if let Some(reader) = started_reader {
        let result = join_reader_bounded(reader, "stdout", READER_JOIN_TIMEOUT);
        if let Err(error) = result {
            cleanup_errors.push(error.to_string());
        }
    }
    drop(command_guard);
    bail!("spawn subprocess output reader: {primary:#}; cleanup={cleanup_errors:?}")
}

fn spawn_reader(
    name: &str,
    mut reader: impl Read + Send + 'static,
    max_bytes: u64,
    injected_failure: Option<&str>,
) -> Result<std::thread::JoinHandle<std::io::Result<Vec<u8>>>> {
    if let Some(message) = injected_failure {
        bail!("{message}");
    }
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take(max_bytes + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max_bytes {
                return Err(std::io::Error::other("subprocess output exceeded cap"));
            }
            Ok(bytes)
        })
        .context("spawn subprocess output reader")
}

fn attempt_command_teardown(
    cgroup: Option<&CgroupV2>,
    group: &distill_tty_utils::ProcessGroup,
    failures: &mut Vec<String>,
) {
    if let Err(error) = group.terminate()
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        failures.push(format!("terminate command process group: {error}"));
    }
    let started = Instant::now();
    while started.elapsed() < TERM_GRACE && group.has_live_members() != Some(false) {
        std::thread::yield_now();
    }
    if let Err(error) = group.kill()
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        failures.push(format!("kill command process group: {error}"));
    }
    if let Some(cgroup) = cgroup
        && let Err(error) = cgroup.kill_all()
    {
        failures.push(format!("kill command cgroup: {error:#}"));
    }
}

fn reader_failures<'a>(
    stdout: &'a Result<Vec<u8>>,
    stderr: &'a Result<Vec<u8>>,
) -> impl Iterator<Item = String> + 'a {
    [stdout.as_ref().err(), stderr.as_ref().err()]
        .into_iter()
        .flatten()
        .map(|error| error.to_string())
}

pub(crate) fn join_reader_bounded(
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let mutex = Mutex::new(());
    let condvar = Condvar::new();
    while !reader.is_finished() {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!("subprocess {stream} reader did not drain within bounded teardown");
        }
        let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        let _guard = condvar
            .wait_timeout(guard, remaining.min(Duration::from_millis(20)))
            .unwrap_or_else(PoisonError::into_inner);
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("subprocess {stream} reader panicked"))?
        .with_context(|| format!("read subprocess {stream}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountNamespace {
    Host,
    Private,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct GroveSupportProbe {
    pub os: &'static str,
    pub fuse_exists: bool,
    pub fuse_writable: bool,
    pub has_fusermount: bool,
    pub mount_namespace: MountNamespace,
    pub has_mount_nfs: bool,
    pub daemon_reachable: bool,
}

pub fn probe_grove_support(daemon_reachable: bool) -> GroveSupportProbe {
    GroveSupportProbe {
        os: std::env::consts::OS,
        fuse_exists: Path::new("/dev/fuse").exists(),
        fuse_writable: fuse_is_writable(),
        has_fusermount: ["fusermount3", "fusermount"]
            .into_iter()
            .any(command_on_path),
        mount_namespace: mount_namespace(),
        has_mount_nfs: Path::new("/sbin/mount_nfs").is_file(),
        daemon_reachable,
    }
}

pub fn grove_skip_reason(probe: &GroveSupportProbe) -> Option<String> {
    match probe.os {
        "linux" if !probe.fuse_exists => Some("linux FUSE unavailable: /dev/fuse is absent".into()),
        "linux" if !probe.fuse_writable => {
            Some("linux FUSE unavailable: /dev/fuse is not writable".into())
        }
        "linux" if !probe.has_fusermount => {
            Some("linux FUSE unavailable: fusermount3/fusermount not on PATH".into())
        }
        "linux" if probe.mount_namespace == MountNamespace::Private => {
            Some("linux FUSE unavailable: private mount namespace".into())
        }
        "macos" if !probe.has_mount_nfs => {
            Some("macOS NFS unavailable: /sbin/mount_nfs is absent".into())
        }
        "linux" | "macos" if !probe.daemon_reachable => {
            Some("Grove daemon control socket is unreachable".into())
        }
        "linux" | "macos" => None,
        other => Some(format!("Grove projection unsupported on {other}")),
    }
}

fn fuse_is_writable() -> bool {
    let Ok(path) = CString::new("/dev/fuse") else {
        return false;
    };
    // SAFETY: `path` is a live NUL-terminated CString and access only reads it.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

fn command_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

fn mount_namespace() -> MountNamespace {
    match (
        std::fs::read_link("/proc/self/ns/mnt"),
        std::fs::read_link("/proc/1/ns/mnt"),
    ) {
        (Ok(current), Ok(pid1)) if current == pid1 => MountNamespace::Host,
        (Ok(_), Ok(_)) => MountNamespace::Private,
        _ => MountNamespace::Unknown,
    }
}

pub fn redact_argv(argv: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut redacted = Vec::new();
    let mut redact_next = false;
    for (index, arg) in argv.into_iter().enumerate() {
        if index == 0 {
            redacted.push("worktree-lifecycle-bench".into());
        } else if redact_next {
            redacted.push("<redacted-path>".into());
            redact_next = false;
        } else if arg == "--source" || arg == "--output" {
            redact_next = true;
            redacted.push(arg);
        } else if arg.starts_with("--source=") {
            redacted.push("--source=<redacted-path>".into());
        } else if arg.starts_with("--output=") {
            redacted.push("--output=<redacted-path>".into());
        } else {
            redacted.push(arg);
        }
    }
    redacted
}

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_handler(signal: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: `fd` is the installed self-pipe and `byte` lives for the call.
        unsafe {
            libc::write(fd, std::ptr::addr_of!(byte).cast(), 1);
        }
    }
}

pub struct SignalWatcher {
    write_fd: libc::c_int,
    thread: Option<std::thread::JoinHandle<()>>,
    old_int: libc::sigaction,
    old_term: libc::sigaction,
}

impl SignalWatcher {
    pub fn install(on_signal: Arc<dyn Fn(i32) + Send + Sync + 'static>) -> Result<Self> {
        Self::install_inner(on_signal, true)
    }

    #[cfg(test)]
    pub fn install_for_test(on_signal: Arc<dyn Fn(i32) + Send + Sync + 'static>) -> Result<Self> {
        Self::install_inner(on_signal, false)
    }

    fn install_inner(
        on_signal: Arc<dyn Fn(i32) + Send + Sync + 'static>,
        should_exit: bool,
    ) -> Result<Self> {
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a writable two-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("create signal self-pipe");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);
        // SAFETY: zeroed sigaction is initialized below before use.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = signal_handler as *const () as usize;
        // SAFETY: valid action/mask pointers for process signal disposition.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action.sa_flags = 0;
        // SAFETY: output actions are valid writable structs.
        let mut old_int: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: same.
        let mut old_term: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: installing handlers for SIGINT/SIGTERM with valid pointers.
        // SAFETY: installing SIGINT with valid action/output pointers.
        if unsafe { libc::sigaction(libc::SIGINT, &action, &mut old_int) } != 0 {
            SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
            // SAFETY: fds came from successful pipe and remain owned here.
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(std::io::Error::last_os_error()).context("install SIGINT handler");
        }
        // SAFETY: installing SIGTERM with valid action/output pointers.
        if unsafe { libc::sigaction(libc::SIGTERM, &action, &mut old_term) } != 0 {
            SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
            // SAFETY: restore SIGINT and close both owned fds.
            unsafe {
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(std::io::Error::last_os_error()).context("install SIGTERM handler");
        }
        let thread = std::thread::Builder::new()
            .name("lifecycle-signal".into())
            .spawn(move || {
                let mut byte = 0u8;
                // SAFETY: read_fd is exclusively owned by this thread.
                let read = unsafe { libc::read(read_fd, std::ptr::addr_of_mut!(byte).cast(), 1) };
                // SAFETY: thread owns read_fd after installation.
                unsafe { libc::close(read_fd) };
                if read == 1 && byte != 0 {
                    on_signal(i32::from(byte));
                    if should_exit {
                        std::process::exit(128 + i32::from(byte));
                    }
                }
            })
            .context("spawn signal cleanup thread")?;
        Ok(Self {
            write_fd,
            thread: Some(thread),
            old_int,
            old_term,
        })
    }
}

#[cfg(test)]
pub fn notify_signal_for_test(signal: i32) -> Result<()> {
    let fd = SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if fd < 0 {
        bail!("signal watcher is not installed");
    }
    let byte = signal as u8;
    // SAFETY: fd is the live watcher self-pipe and byte lives for the call.
    if unsafe { libc::write(fd, std::ptr::addr_of!(byte).cast(), 1) } != 1 {
        return Err(std::io::Error::last_os_error()).context("notify signal watcher");
    }
    Ok(())
}

impl Drop for SignalWatcher {
    fn drop(&mut self) {
        SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
        let stop = 0u8;
        // SAFETY: write_fd is live until closed below and stop lives for the call.
        unsafe {
            libc::write(self.write_fd, std::ptr::addr_of!(stop).cast(), 1);
            libc::close(self.write_fd);
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
