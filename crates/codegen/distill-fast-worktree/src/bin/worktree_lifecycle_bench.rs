// Modified for Distill by Samuel Fajreldines, 2026.
//! Cross-transport product worktree lifecycle benchmark.
//!
//! Uses `WorktreeBuilder` and `remove_worktree` for native standalone, native
//! linked, and Grove-projected worktrees. Latencies are samples, not CI gates.

#![deny(clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde::Serialize;
use tempfile::NamedTempFile;
use distill_fast_worktree::{
    CreationMode, IgnoredFilesMode, NfsWorktreeClient, NfsWorktreeOpts, WorkingTreeMode,
    WorktreeBuilder, dest_is_known_unmounted, is_grove_strategy, remove_worktree,
};

#[path = "worktree_lifecycle_bench/runtime.rs"]
mod runtime;

const SCHEMA_VERSION: u32 = 1;
const MIN_WARM_ITERATIONS: usize = 5;
const CREATE_WORKER_GRACE: Duration = Duration::from_secs(30);
const REDACTED_PATH: &str = "<redacted-path>";

#[derive(Debug, Parser)]
#[command(name = "worktree-lifecycle-bench")]
#[command(about = "Cross-transport distill-fast-worktree/Grove lifecycle sampler")]
struct Cli {
    /// Source repository whose HEAD and working-tree state are preserved.
    #[arg(long, default_value = ".")]
    source: PathBuf,

    /// Warm lifecycle iterations after the first uncontrolled-cache sample (minimum 5).
    #[arg(long, default_value_t = MIN_WARM_ITERATIONS)]
    warm_iterations: usize,

    /// Also fail when Grove is unsupported or product dispatch falls back.
    #[arg(long)]
    require_grove: bool,

    /// Write the same versioned JSON emitted on stdout to this file.
    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker: bool,

    #[arg(long, hide = true)]
    worker_dest: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker_id: Option<String>,

    #[arg(long, hide = true)]
    worker_kind: Option<String>,

    #[arg(long, hide = true)]
    worker_hook: Option<String>,

    #[arg(long, hide = true)]
    worker_gate: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker_stop_before_work: bool,

    #[arg(long, hide = true)]
    controller_gate: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker_control_sock: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker_data_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    worker_runtime_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
enum CaseKind {
    NativeOrdinary,
    NativeLinked,
    GroveProjected,
}

impl CaseKind {
    fn name(self) -> &'static str {
        match self {
            Self::NativeOrdinary => "native_ordinary",
            Self::NativeLinked => "native_linked_worktree",
            Self::GroveProjected => "grove_projected",
        }
    }

    fn worktree_shape(self) -> &'static str {
        match self {
            Self::NativeOrdinary => "ordinary_repository",
            Self::NativeLinked => "linked_worktree",
            Self::GroveProjected => "grove_projected",
        }
    }

    fn requested_transport(self) -> &'static str {
        match self {
            Self::NativeOrdinary | Self::NativeLinked => "native",
            Self::GroveProjected => expected_grove_transport(),
        }
    }
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    provenance: Provenance,
    cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize)]
struct Provenance {
    generated_at_unix_secs: u64,
    package_version: &'static str,
    run_id: String,
    source: String,
    source_head: String,
    source_tree: String,
    git_revision: String,
    dirty_digest_sha256: String,
    harness_repo_dirty_digest_sha256: String,
    harness_repo_commit: String,
    harness_repo_tree: String,
    harness_inputs: Vec<HarnessInput>,
    harness_executable_sha256: String,
    containment_support: ContainmentSupport,
    tracked_files: usize,
    source_state: StateCoverage,
    os: &'static str,
    arch: &'static str,
    release: bool,
    argv: Vec<String>,
    first_definition: &'static str,
    warm_state: &'static str,
    warm_iterations: usize,
    create_phase_source: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct HarnessInput {
    path: &'static str,
    sha256: String,
    head_sha256: Option<String>,
    matches_head: bool,
    dirty: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ContainmentSupport {
    native: bool,
    grove: bool,
    reason: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct StateCoverage {
    has_staged: bool,
    has_dirty: bool,
    has_untracked: bool,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    name: &'static str,
    worktree_shape: &'static str,
    requested_transport: &'static str,
    support: Support,
    raw_samples: Vec<Sample>,
    summary_ms: Option<SampleSummary>,
}

#[derive(Debug, Serialize)]
struct SampleSummary {
    first: PhaseDurations,
    warm_median: PhaseDurations,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Support {
    Supported,
    Skipped { reason: String },
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct WorkerResult {
    commit: String,
    resolved_strategy: String,
    strategy_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct Sample {
    iteration: usize,
    sample_id: String,
    worktree_id: String,
    sample_class: &'static str,
    daemon_cache_state: &'static str,
    resolved_strategy: String,
    resolution: Resolution,
    durations_ms: PhaseDurations,
    create_phases_ms: BTreeMap<String, f64>,
    correctness: Correctness,
    cleanup: CleanupVerification,
}

#[derive(Debug, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum Resolution {
    Native { reason: String },
    Adopted { reason: String },
    Fallback { reason: String },
}

#[derive(Clone, Debug, Default, Serialize)]
struct PhaseDurations {
    create: f64,
    first_readdir: f64,
    first_read: f64,
    first_git_status: f64,
    second_git_status: f64,
    remove_cleanup: f64,
}

#[derive(Debug, Serialize)]
struct Correctness {
    head_matches: bool,
    tree_matches: bool,
    status_matches: bool,
    staged_state_matches: bool,
    dirty_state_matches: bool,
    untracked_state_matches: bool,
    first_read_matches: bool,
}

#[derive(Debug, Serialize)]
struct CleanupVerification {
    dest_absent: bool,
    mount_absent: bool,
    backing_absent: bool,
    pin_absent: bool,
    worktree_admin_unchanged: bool,
    journal_absent: bool,
}

#[derive(Clone, Debug)]
struct CleanupTarget {
    dest: PathBuf,
    worktree_id: String,
    source: PathBuf,
    opts: NfsWorktreeOpts,
    is_grove: bool,
}

#[derive(Clone, Debug, Serialize)]
struct CleanupFailure {
    target: &'static str,
    error: String,
}

fn cleanup_sensitive_paths(target: &CleanupTarget) -> Vec<&Path> {
    let mut paths = vec![target.dest.as_path(), target.source.as_path()];
    if let Some(path) = target.opts.data_dir.as_deref() {
        paths.push(path);
    }
    if let Some(path) = target.opts.runtime_dir.as_deref() {
        paths.push(path);
    }
    if let Some(path) = target.opts.control_sock.as_deref() {
        paths.push(path);
    }
    paths
}

fn redact_cleanup_error(error: &str, paths: &[&Path]) -> String {
    let mut redacted = error.to_owned();
    let mut values = paths
        .iter()
        .flat_map(|path| {
            let raw = path.to_string_lossy().into_owned();
            let canonical = dunce::canonicalize(path)
                .ok()
                .map(|path| path.to_string_lossy().into_owned());
            [Some(raw), canonical]
        })
        .flatten()
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    values.sort_by_key(|path| std::cmp::Reverse(path.len()));
    values.dedup();
    for path in values {
        redacted = redacted.replace(&path, REDACTED_PATH);
        if let Ok(url) = url::Url::from_file_path(&path) {
            redacted = redacted.replace(url.as_str(), REDACTED_PATH);
        }
    }
    redact_sensitive_tokens(&redacted)
}

fn redact_sensitive_tokens(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let Some(remaining) = text.get(cursor..) else {
            break;
        };
        let url_start = remaining.char_indices().find_map(|(index, character)| {
            if !character.is_ascii_alphabetic()
                || (index > 0
                    && !remaining.get(..index).is_some_and(|prefix| {
                        prefix
                            .chars()
                            .next_back()
                            .is_some_and(is_path_start_delimiter)
                    }))
            {
                return None;
            }
            let candidate = remaining.get(index..)?;
            let end = candidate
                .char_indices()
                .find_map(|(offset, character)| {
                    (offset > 0 && is_sensitive_value_delimiter(character)).then_some(offset)
                })
                .unwrap_or(candidate.len());
            url::Url::parse(candidate.get(..end).unwrap_or(candidate))
                .is_ok()
                .then_some(index)
        });
        let path_start = remaining.char_indices().find_map(|(index, character)| {
            (character == '/'
                && (index == 0
                    || remaining.get(..index).is_some_and(|prefix| {
                        prefix
                            .chars()
                            .next_back()
                            .is_some_and(is_path_start_delimiter)
                    })))
            .then_some(index)
        });
        let Some(start) = url_start.into_iter().chain(path_start).min() else {
            redacted.push_str(remaining);
            break;
        };
        redacted.push_str(remaining.get(..start).unwrap_or(""));
        let Some(sensitive) = remaining.get(start..) else {
            break;
        };
        let end = sensitive
            .char_indices()
            .find_map(|(index, character)| {
                (index > 0 && is_sensitive_value_delimiter(character)).then_some(index)
            })
            .unwrap_or(sensitive.len());
        redacted.push_str(REDACTED_PATH);
        cursor += start + end;
    }
    redacted
}

fn is_path_start_delimiter(character: char) -> bool {
    character.is_whitespace() || "='\"(),[]{}<>:".contains(character)
}

fn is_sensitive_value_delimiter(character: char) -> bool {
    character.is_whitespace() || "'\"(),[]{}<>".contains(character)
}

#[derive(Clone, Debug, Serialize)]
struct InterruptedCleanup {
    commands_remaining: usize,
    worktrees_remaining: usize,
    failures: Vec<CleanupFailure>,
}

#[derive(Debug, Serialize)]
struct FailedArtifact<'a> {
    schema_version: u32,
    status: &'static str,
    run_id: &'a str,
    error: String,
    cleanup: InterruptedCleanup,
    source: &'static str,
    output: &'static str,
}

#[derive(Default)]
struct RedactionSet {
    paths: Vec<PathBuf>,
}

impl RedactionSet {
    fn add(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if !path.as_os_str().is_empty() && !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    fn redact(&self, text: &str) -> String {
        let refs = self.paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        redact_cleanup_error(text, &refs)
    }
}

impl CleanupTarget {
    fn cleanup(&self) -> Result<()> {
        if self.is_grove {
            cleanup_grove_target(self)?;
        } else {
            remove_worktree(&self.dest).context("remove owned benchmark worktree")?;
        }
        ensure!(
            dest_is_known_unmounted(&self.dest),
            "mount state remained mounted or inconclusive after cleanup"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GroveCleanupState {
    Unknown,
    Phase(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroveCleanupRoute {
    Cancel,
    Aborted,
    Committed,
}

fn wait_for_grove_cleanup_route(
    mut query: impl FnMut() -> Result<GroveCleanupState>,
) -> Result<GroveCleanupRoute> {
    match query()? {
        GroveCleanupState::Unknown => Ok(GroveCleanupRoute::Cancel),
        GroveCleanupState::Phase(phase) if phase == "cancelled" || phase == "cancelling" => {
            Ok(GroveCleanupRoute::Cancel)
        }
        GroveCleanupState::Phase(phase) if phase == "aborted" => Ok(GroveCleanupRoute::Aborted),
        GroveCleanupState::Phase(phase) if phase == "committed" => Ok(GroveCleanupRoute::Committed),
        GroveCleanupState::Phase(_) => Ok(GroveCleanupRoute::Cancel),
    }
}

fn cleanup_grove_target(target: &CleanupTarget) -> Result<()> {
    let client = NfsWorktreeClient::from_opts(&target.opts);
    match wait_for_grove_cleanup_route(|| {
        let snapshot = client
            .query_phase(&target.worktree_id)
            .map_err(|error| anyhow::anyhow!("query Grove cleanup phase: {error:?}"))?;
        Ok(if snapshot.unknown {
            GroveCleanupState::Unknown
        } else {
            GroveCleanupState::Phase(
                snapshot
                    .phase
                    .context("Grove cleanup query omitted create phase")?,
            )
        })
    })? {
        GroveCleanupRoute::Cancel => cleanup_cancelled_grove_identity(target),
        GroveCleanupRoute::Aborted => cleanup_grove_identity(target),
        GroveCleanupRoute::Committed => client
            .remove_worktree(&target.dest, false)
            .context("remove committed Grove worktree by destination"),
    }
}

struct CleanupRegistry {
    targets: Mutex<Vec<CleanupTarget>>,
    failures: Mutex<Vec<CleanupFailure>>,
}

impl CleanupRegistry {
    fn new() -> Self {
        Self {
            targets: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
        }
    }

    fn arm(self: &Arc<Self>, target: CleanupTarget) -> CleanupGuard {
        self.targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(target.clone());
        CleanupGuard {
            registry: Arc::clone(self),
            target,
            armed: true,
        }
    }

    fn cleanup_all(&self) -> Result<Vec<CleanupFailure>> {
        self.cleanup_all_with(|target| target.cleanup())
    }

    fn cleanup_all_with(
        &self,
        mut cleanup: impl FnMut(&CleanupTarget) -> Result<()>,
    ) -> Result<Vec<CleanupFailure>> {
        let targets = self
            .targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut failures = Vec::new();
        for target in targets {
            if let Err(error) = cleanup(&target) {
                failures.push(CleanupFailure {
                    target: "worktree",
                    error: redact_cleanup_error(
                        &format!("{error:#}"),
                        cleanup_sensitive_paths(&target).as_slice(),
                    ),
                });
            } else {
                self.disarm(&target.dest);
            }
        }
        self.failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(failures.clone());
        if failures.is_empty() {
            Ok(failures)
        } else {
            bail!("{} cleanup operation(s) failed", failures.len())
        }
    }

    fn live_count(&self) -> usize {
        self.targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    fn failures(&self) -> Vec<CleanupFailure> {
        self.failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn disarm(&self, dest: &Path) {
        self.targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|target| target.dest != dest);
    }
}

struct CleanupGuard {
    registry: Arc<CleanupRegistry>,
    target: CleanupTarget,
    armed: bool,
}

impl CleanupGuard {
    fn remove(mut self) -> Result<f64> {
        let start = Instant::now();
        self.target.cleanup()?;
        self.registry.disarm(&self.target.dest);
        self.armed = false;
        Ok(elapsed_ms(start))
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = self.target.cleanup() {
                self.registry
                    .failures
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(CleanupFailure {
                        target: "worktree",
                        error: redact_cleanup_error(
                            &format!("{error:#}"),
                            cleanup_sensitive_paths(&self.target).as_slice(),
                        ),
                    });
            } else {
                self.registry.disarm(&self.target.dest);
            }
        }
    }
}

impl Drop for CleanupRegistry {
    fn drop(&mut self) {
        let _ = self.cleanup_all();
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.worker {
        return run_worker(&cli);
    }
    run_controller(&cli)
}

fn run_controller(cli: &Cli) -> Result<()> {
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let cleanup_registry = Arc::new(CleanupRegistry::new());
    let command_registry = runtime::CommandRegistry::new();
    let artifact = Arc::new(ArtifactFinalizer::new(cli.output.clone()));
    let signal_registry = Arc::clone(&cleanup_registry);
    let signal_commands = command_registry.clone();
    let signal_artifact = Arc::clone(&artifact);
    let signal_run_id = run_id.clone();
    let _signals = runtime::SignalWatcher::install(Arc::new(move |signal| {
        let mut failures = signal_commands
            .terminate_all()
            .into_iter()
            .map(|error| CleanupFailure {
                target: "command",
                error: redact_cleanup_error(&error, &[]),
            })
            .collect::<Vec<_>>();
        let cleanup_result = signal_registry.cleanup_all();
        failures.extend(signal_registry.failures());
        let verification = InterruptedCleanup {
            commands_remaining: signal_commands.live_count_for_artifact(),
            worktrees_remaining: signal_registry.live_count(),
            failures,
        };
        if let Ok(json) = interrupted_artifact_json(&signal_run_id, signal, &verification) {
            let _ = signal_artifact.finalize(&json);
        }
        let _ = cleanup_result;
    }))?;
    match run_controller_inner(
        cli,
        &run_id,
        &cleanup_registry,
        &command_registry,
        &artifact,
    ) {
        Ok(()) => Ok(()),
        Err(primary) => finalize_controller_failure(
            cli,
            &run_id,
            &cleanup_registry,
            &command_registry,
            &artifact,
            primary,
        ),
    }
}

fn run_controller_inner(
    cli: &Cli,
    run_id: &str,
    cleanup_registry: &Arc<CleanupRegistry>,
    command_registry: &runtime::CommandRegistry,
    artifact: &ArtifactFinalizer,
) -> Result<()> {
    ensure!(
        cli.warm_iterations >= MIN_WARM_ITERATIONS,
        "--warm-iterations must be at least {MIN_WARM_ITERATIONS}"
    );
    if let Some(gate) = cli.controller_gate.as_deref() {
        while gate.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let source = dunce::canonicalize(&cli.source).context("canonicalize benchmark source")?;
    let source_status = git_bytes(
        command_registry,
        &source,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    let source_state = classify_status(&source_status);
    let read_path = choose_read_path(command_registry, &source)?;
    let source_head = git_text(command_registry, &source, &["rev-parse", "HEAD"])?;
    let source_tree = git_text(command_registry, &source, &["rev-parse", "HEAD^{tree}"])?;
    let dirty_digest_sha256 = dirty_repo_digest(command_registry, &source, &source_status)?;
    let harness_repo = dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .context("resolve harness repository")?;
    let harness_repo_commit = git_text(command_registry, &harness_repo, &["rev-parse", "HEAD"])?;
    let harness_status = git_bytes(
        command_registry,
        &harness_repo,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    let harness_repo_dirty_digest_sha256 =
        dirty_repo_digest(command_registry, &harness_repo, &harness_status)?;
    let harness_repo_tree = git_text(
        command_registry,
        &harness_repo,
        &["rev-parse", "HEAD^{tree}"],
    )?;
    let harness_inputs = collect_harness_inputs(command_registry, &harness_repo)?;
    let harness_executable_sha256 = sha256_hex(
        &fs::read(std::env::current_exe().context("resolve harness executable")?)
            .context("read harness executable")?,
    );
    let tracked_files = distill_fast_worktree::count_tracked_files(&source)?;
    ensure!(tracked_files > 0, "source has no tracked files");

    let scratch = tempfile::Builder::new()
        .prefix("worktree-lifecycle-bench-")
        .tempdir()
        .context("create benchmark scratch directory")?;
    let grove_dirs = grove::paths::GroveDirs::from_env().context("resolve Grove directories")?;
    let grove_opts = NfsWorktreeOpts {
        enabled: true,
        control_sock: Some(grove_dirs.control_sock()),
        data_dir: Some(grove_dirs.data_dir),
        runtime_dir: Some(grove_dirs.runtime_dir),
        ..NfsWorktreeOpts::default()
    };
    let containment_support = measured_worker_containment_support();
    let grove_support = grove_support(&grove_opts, &containment_support);
    let context = RunContext {
        source: &source,
        source_status: &source_status,
        source_state,
        source_head: &source_head,
        source_tree: &source_tree,
        read_path: &read_path,
        scratch: scratch.path(),
        run_id,
        warm_iterations: cli.warm_iterations,
        grove_opts: &grove_opts,
        cleanup_registry,
        command_registry,
    };
    let mut cases = Vec::new();
    for kind in [
        CaseKind::NativeOrdinary,
        CaseKind::NativeLinked,
        CaseKind::GroveProjected,
    ] {
        let support = if matches!(kind, CaseKind::GroveProjected) {
            match &grove_support {
                Support::Supported => None,
                Support::Skipped { reason } => Some(reason.clone()),
            }
        } else if containment_support.native {
            None
        } else {
            Some(containment_support.reason.clone())
        };
        if let Some(reason) = support {
            cases.push(CaseReport {
                name: kind.name(),
                worktree_shape: kind.worktree_shape(),
                requested_transport: kind.requested_transport(),
                support: Support::Skipped { reason },
                raw_samples: Vec::new(),
                summary_ms: None,
            });
            continue;
        }
        cases.push(run_case(kind, &context)?);
    }

    let grove_case = cases
        .iter()
        .find(|case| case.name == CaseKind::GroveProjected.name())
        .context("missing Grove case")?;
    let grove_adopted = grove_case
        .raw_samples
        .iter()
        .all(|sample| matches!(sample.resolution, Resolution::Adopted { .. }))
        && !grove_case.raw_samples.is_empty();

    let report = BenchmarkReport {
        schema_version: SCHEMA_VERSION,
        provenance: Provenance {
            generated_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            package_version: env!("CARGO_PKG_VERSION"),
            run_id: run_id.to_owned(),
            source: REDACTED_PATH.into(),
            git_revision: source_head.clone(),
            source_head,
            source_tree,
            dirty_digest_sha256,
            harness_repo_dirty_digest_sha256,
            harness_repo_commit,
            harness_repo_tree,
            harness_inputs,
            harness_executable_sha256,
            containment_support,
            tracked_files,
            source_state,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            release: !cfg!(debug_assertions),
            argv: runtime::redact_argv(std::env::args()),
            first_definition: "first fresh destination per case; host, source, kernel, and daemon caches are not reset",
            warm_state: "later fresh destinations reuse process, source, kernel, and daemon caches",
            warm_iterations: cli.warm_iterations,
            create_phase_source: "WorktreeReport exposes only total create time; no product subphases were available",
        },
        cases,
    };
    if cli.require_grove && !grove_adopted {
        let failure = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "status": "failed",
            "error": "--require-grove requested, but Grove was skipped or fell back",
            "result": report,
        });
        artifact.finalize(&serde_json::to_string_pretty(&failure)?)?;
        bail!("--require-grove requested, but Grove was skipped or fell back");
    }
    let json = serde_json::to_string_pretty(&report).context("serialize benchmark report")?;
    artifact.finalize(&json)
}

fn finalize_controller_failure(
    cli: &Cli,
    run_id: &str,
    cleanup_registry: &CleanupRegistry,
    command_registry: &runtime::CommandRegistry,
    artifact_finalizer: &ArtifactFinalizer,
    primary: anyhow::Error,
) -> Result<()> {
    let mut failures = command_registry
        .terminate_all()
        .into_iter()
        .map(|error| CleanupFailure {
            target: "command",
            error: redact_cleanup_error(&error, &[]),
        })
        .collect::<Vec<_>>();
    let cleanup_error = cleanup_registry.cleanup_all().err();
    failures.extend(cleanup_registry.failures());
    let cleanup = InterruptedCleanup {
        commands_remaining: command_registry.live_count_for_artifact(),
        worktrees_remaining: cleanup_registry.live_count(),
        failures,
    };
    let mut redactions = RedactionSet::default();
    redactions.add(cli.source.clone());
    if let Some(output) = cli.output.as_ref() {
        redactions.add(output.clone());
        if let Some(parent) = output.parent() {
            redactions.add(parent.to_path_buf());
        }
    }
    if let Ok(executable) = std::env::current_exe() {
        redactions.add(executable);
    }
    if let Ok(repo) = dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")) {
        redactions.add(repo);
    }
    for target in cleanup_registry
        .targets
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
    {
        for path in cleanup_sensitive_paths(target) {
            redactions.add(path.to_path_buf());
        }
        if let Some(data_dir) = target.opts.data_dir.as_ref() {
            redactions.add(data_dir.join("worktree-backing").join(&target.worktree_id));
        }
        if let Some(parent) = target.dest.parent() {
            redactions.add(parent.to_path_buf());
        }
    }
    let error = redactions.redact(&format!("{primary:#}"));
    let cleanup = InterruptedCleanup {
        failures: cleanup
            .failures
            .into_iter()
            .map(|failure| CleanupFailure {
                target: failure.target,
                error: redactions.redact(&failure.error),
            })
            .collect(),
        ..cleanup
    };
    let artifact = FailedArtifact {
        schema_version: SCHEMA_VERSION,
        status: "failed",
        run_id,
        error: error.clone(),
        cleanup,
        source: REDACTED_PATH,
        output: REDACTED_PATH,
    };
    let json = serde_json::to_string_pretty(&artifact)?;
    artifact_finalizer.finalize(&json)?;
    if let Some(cleanup_error) = cleanup_error {
        bail!("{error}; cleanup failed: {cleanup_error:#}");
    }
    bail!("{error}")
}

struct RunContext<'a> {
    source: &'a Path,
    source_status: &'a [u8],
    source_state: StateCoverage,
    source_head: &'a str,
    source_tree: &'a str,
    read_path: &'a Path,
    scratch: &'a Path,
    run_id: &'a str,
    warm_iterations: usize,
    grove_opts: &'a NfsWorktreeOpts,
    cleanup_registry: &'a Arc<CleanupRegistry>,
    command_registry: &'a runtime::CommandRegistry,
}

fn run_case(kind: CaseKind, context: &RunContext<'_>) -> Result<CaseReport> {
    let mut samples = Vec::with_capacity(context.warm_iterations + 1);
    for iteration in 0..=context.warm_iterations {
        let identity = sample_identity(context.run_id, kind, iteration);
        let dest = context.scratch.join(&identity);
        let worktree_id = identity;
        let admin_before = git_bytes(
            context.command_registry,
            context.source,
            &["worktree", "list", "--porcelain"],
        )?;
        let guard = context.cleanup_registry.arm(CleanupTarget {
            dest: dest.clone(),
            worktree_id: worktree_id.clone(),
            source: context.source.to_path_buf(),
            opts: context.grove_opts.clone(),
            is_grove: matches!(kind, CaseKind::GroveProjected),
        });
        recover_pre_create(
            context.grove_opts,
            &worktree_id,
            &dest,
            context.source,
            matches!(kind, CaseKind::GroveProjected),
        )?;
        let create_start = Instant::now();
        let created = run_create_worker(context, kind, &dest, &worktree_id)?;
        let create_ms = elapsed_ms(create_start);
        let resolution = classify_resolution(
            kind,
            &created.resolved_strategy,
            context.grove_opts,
            &worktree_id,
        )?;

        let readdir_start = Instant::now();
        let entries = fs::read_dir(&dest)
            .with_context(|| format!("first readdir {}", dest.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        let first_readdir_ms = elapsed_ms(readdir_start);
        ensure!(!entries.is_empty(), "first readdir returned no entries");

        let read_start = Instant::now();
        let dest_bytes = fs::read(dest.join(context.read_path))
            .with_context(|| format!("first read {}", context.read_path.display()))?;
        let first_read_ms = elapsed_ms(read_start);
        let source_bytes = fs::read(context.source.join(context.read_path))?;

        let status_1_start = Instant::now();
        let status_1 = git_bytes(
            context.command_registry,
            &dest,
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        )?;
        let first_git_status_ms = elapsed_ms(status_1_start);
        let status_2_start = Instant::now();
        let status_2 = git_bytes(
            context.command_registry,
            &dest,
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        )?;
        let second_git_status_ms = elapsed_ms(status_2_start);

        let dest_head = git_text(context.command_registry, &dest, &["rev-parse", "HEAD"])?;
        let dest_tree = git_text(
            context.command_registry,
            &dest,
            &["rev-parse", "HEAD^{tree}"],
        )?;
        let dest_state = classify_status(&status_1);
        let correctness = Correctness {
            head_matches: dest_head == context.source_head,
            tree_matches: dest_tree == context.source_tree,
            status_matches: status_1 == context.source_status && status_2 == context.source_status,
            staged_state_matches: dest_state.has_staged == context.source_state.has_staged,
            dirty_state_matches: dest_state.has_dirty == context.source_state.has_dirty,
            untracked_state_matches: dest_state.has_untracked == context.source_state.has_untracked,
            first_read_matches: dest_bytes == source_bytes,
        };
        ensure_correct(&correctness, kind.name(), iteration)?;

        let backing = created
            .strategy_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("grove"))
            .and_then(|grove| grove.get("backing"))
            .and_then(|value| value.as_str())
            .map(PathBuf::from);
        let pin = created
            .strategy_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("grove"))
            .and_then(|grove| grove.get("source_pin"))
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let remove_cleanup_ms = guard.remove()?;
        let cleanup = verify_cleanup(
            context.command_registry,
            context.source,
            &dest,
            &admin_before,
            backing.as_deref(),
            pin.as_deref(),
            matches!(kind, CaseKind::GroveProjected),
            context.grove_opts,
            &worktree_id,
        )?;
        samples.push(Sample {
            iteration,
            sample_id: worktree_id.clone(),
            worktree_id,
            sample_class: if iteration == 0 { "first" } else { "warm" },
            daemon_cache_state: if iteration == 0 {
                "preexisting_uncontrolled"
            } else {
                "reused"
            },
            resolved_strategy: created.resolved_strategy.clone(),
            resolution,
            durations_ms: PhaseDurations {
                create: create_ms,
                first_readdir: first_readdir_ms,
                first_read: first_read_ms,
                first_git_status: first_git_status_ms,
                second_git_status: second_git_status_ms,
                remove_cleanup: remove_cleanup_ms,
            },
            create_phases_ms: BTreeMap::new(),
            correctness,
            cleanup,
        });
    }
    Ok(CaseReport {
        name: kind.name(),
        worktree_shape: kind.worktree_shape(),
        requested_transport: kind.requested_transport(),
        support: Support::Supported,
        summary_ms: Some(SampleSummary {
            first: samples
                .first()
                .map(|s| s.durations_ms.clone())
                .unwrap_or_default(),
            warm_median: median_phases(samples.get(1..).unwrap_or(&[])),
        }),
        raw_samples: samples,
    })
}

fn run_create_worker(
    context: &RunContext<'_>,
    kind: CaseKind,
    dest: &Path,
    worktree_id: &str,
) -> Result<WorkerResult> {
    let executable = std::env::current_exe().context("resolve lifecycle worker executable")?;
    let mut command = std::process::Command::new(executable);
    distill_tty_utils::detach_std_command(&mut command);
    command
        .arg("--worker")
        .arg("--source")
        .arg(context.source)
        .arg("--worker-dest")
        .arg(dest)
        .arg("--worker-id")
        .arg(worktree_id)
        .arg("--worker-kind")
        .arg(kind.name());
    if matches!(kind, CaseKind::GroveProjected) {
        command
            .arg("--worker-control-sock")
            .arg(
                context
                    .grove_opts
                    .control_sock
                    .as_deref()
                    .context("controller omitted Grove control socket")?,
            )
            .arg("--worker-data-dir")
            .arg(
                context
                    .grove_opts
                    .data_dir
                    .as_deref()
                    .context("controller omitted Grove data directory")?,
            )
            .arg("--worker-runtime-dir")
            .arg(
                context
                    .grove_opts
                    .runtime_dir
                    .as_deref()
                    .context("controller omitted Grove runtime directory")?,
            );
    }
    let Some(cgroup) = runtime::CgroupV2::create(worktree_id)? else {
        bail!("measured worker containment became unavailable after the support probe")
    };
    command.arg("--worker-stop-before-work");
    let output = runtime::run_stopped_worker_with_cgroup(
        context.command_registry,
        command,
        context.grove_opts.create_timeout + CREATE_WORKER_GRACE,
        cgroup,
    )?;
    ensure!(
        output.status.success(),
        "product create worker failed: {}",
        redact_cleanup_error(
            &String::from_utf8_lossy(&output.stderr),
            &[context.source, dest]
        )
    );
    serde_json::from_slice(&output.stdout).context("parse product create worker result")
}

fn run_worker(cli: &Cli) -> Result<()> {
    if cli.worker_stop_before_work {
        // SAFETY: raising SIGSTOP for this process needs no pointers or shared memory.
        unsafe { libc::raise(libc::SIGSTOP) };
    }
    if let Some(hook) = cli.worker_hook.as_deref() {
        let mut command = std::process::Command::new("sh");
        distill_tty_utils::detach_std_command(&mut command);
        command.args(["-c", hook]);
        #[allow(clippy::disallowed_methods)]
        // Test-only worker hook; controller owns this worker group.
        command.spawn().context("spawn worker test hook")?;
        if let Some(gate) = cli.worker_gate.as_deref() {
            while gate.exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        return Ok(());
    }
    let dest = cli
        .worker_dest
        .as_deref()
        .context("--worker-dest required")?;
    let worktree_id = cli.worker_id.as_deref().context("--worker-id required")?;
    let kind = match cli.worker_kind.as_deref() {
        Some("native_ordinary") => CaseKind::NativeOrdinary,
        Some("native_linked_worktree") => CaseKind::NativeLinked,
        Some("grove_projected") => CaseKind::GroveProjected,
        _ => bail!("invalid --worker-kind"),
    };
    let mut builder = WorktreeBuilder::new(&cli.source, dest)
        .working_tree_mode(WorkingTreeMode::PreserveWorkingTree)
        .ignored_files_mode(IgnoredFilesMode::Skip)
        .worktree_id(worktree_id)
        .creation_mode(match kind {
            CaseKind::NativeOrdinary => CreationMode::Standalone,
            CaseKind::NativeLinked | CaseKind::GroveProjected => CreationMode::Linked,
        });
    if matches!(kind, CaseKind::GroveProjected) {
        builder = builder.grove_worktree(NfsWorktreeOpts {
            enabled: true,
            control_sock: Some(
                cli.worker_control_sock
                    .clone()
                    .context("--worker-control-sock required")?,
            ),
            data_dir: Some(
                cli.worker_data_dir
                    .clone()
                    .context("--worker-data-dir required")?,
            ),
            runtime_dir: Some(
                cli.worker_runtime_dir
                    .clone()
                    .context("--worker-runtime-dir required")?,
            ),
            ..NfsWorktreeOpts::default()
        });
    }
    let created = builder.create().context("worker product create")?;
    let result = WorkerResult {
        commit: created.commit,
        resolved_strategy: created.resolved_strategy.to_owned(),
        strategy_metadata: created.strategy_metadata,
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn sample_identity(run_id: &str, kind: CaseKind, iteration: usize) -> String {
    format!("lifecycle-{run_id}-{}-{iteration}", kind.name())
}

fn cleanup_grove_identity_with(
    target: &CleanupTarget,
    mut query: impl FnMut() -> Result<(Option<String>, bool)>,
    mut cancel: impl FnMut() -> Result<()>,
) -> Result<()> {
    const ID_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let deadline = Instant::now() + ID_CLEANUP_TIMEOUT;
    loop {
        let (phase, unknown) = query()?;
        if unknown {
            break;
        }
        if phase.as_deref() == Some("cancelled") {
            cancel()?;
            let (after_phase, after_unknown) = query()?;
            ensure!(
                after_unknown,
                "Grove cancellation remained after acknowledgement (phase={})",
                after_phase.as_deref().unwrap_or("unknown")
            );
            break;
        }
        if phase.as_deref() == Some("aborted") {
            cancel()?;
            let (after_phase, after_unknown) = query()?;
            ensure!(
                after_unknown,
                "Grove journal remained after cleanup (phase={})",
                after_phase.as_deref().unwrap_or("unknown")
            );
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "Grove identity remained in-flight during cleanup"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ensure!(
        dest_is_known_unmounted(&target.dest),
        "Grove destination mount state remained mounted or inconclusive"
    );
    Ok(())
}

fn cleanup_cancelled_grove_identity(target: &CleanupTarget) -> Result<()> {
    const CANCEL_INTERVAL: Duration = Duration::from_millis(50);
    let cancel_timeout = target.opts.create_timeout.max(Duration::from_secs(5));
    let client = NfsWorktreeClient::from_opts(&target.opts);
    client
        .cancel_worktree_create(&target.worktree_id)
        .context("cancel Grove create by worktree ID; daemon may be too old")?;
    cleanup_cancelled_grove_identity_after_cancel(
        target,
        cancel_timeout,
        CANCEL_INTERVAL,
        || {
            let snapshot = client
                .query_phase(&target.worktree_id)
                .map_err(|error| anyhow::anyhow!("query cancelled Grove create: {error:?}"))?;
            Ok((snapshot.phase, snapshot.unknown))
        },
        || {
            client
                .remove_worktree(&target.dest, false)
                .context("remove Grove worktree that committed during cancel")
        },
        || client.cleanup_worktree_create(&target.worktree_id),
    )
}

fn cleanup_cancelled_grove_identity_after_cancel(
    target: &CleanupTarget,
    cancel_timeout: Duration,
    cancel_interval: Duration,
    mut query: impl FnMut() -> Result<(Option<String>, bool)>,
    mut remove_committed: impl FnMut() -> Result<()>,
    mut ack: impl FnMut() -> Result<()>,
) -> Result<()> {
    let deadline = Instant::now() + cancel_timeout;
    loop {
        let (phase, unknown) = query()?;
        match phase.as_deref() {
            Some("committed") => {
                // Dest/backing can go away while the cancel tombstone remains.
                // Keep polling so cancelled/unknown can acknowledge the ID.
                remove_committed()?;
            }
            Some("aborted") => return cleanup_grove_identity(target),
            Some("cancelled") => match ack() {
                Ok(()) => break,
                Err(error) if error.to_string().contains("active create") => {}
                Err(error) => return Err(error).context("acknowledge cancelled Grove create"),
            },
            None if unknown => match ack() {
                Ok(()) => break,
                Err(_) => break,
            },
            Some(_) | None => {}
        }
        ensure!(
            Instant::now() < deadline,
            "cancelled Grove create did not reach a terminal cleanup state"
        );
        std::thread::sleep(cancel_interval);
    }
    let (terminal_phase, terminal_unknown) = query().map_err(|error| {
        anyhow::anyhow!("verify cancelled Grove create terminal state: {error:?}")
    })?;
    ensure!(
        terminal_unknown,
        "Grove create could still publish after cancellation (phase={})",
        terminal_phase.as_deref().unwrap_or("unknown")
    );
    remove_worktree(&target.dest).context("remove destination after Grove cancellation proof")?;
    Ok(())
}

fn cleanup_grove_identity(target: &CleanupTarget) -> Result<()> {
    let client = NfsWorktreeClient::from_opts(&target.opts);
    cleanup_grove_identity_with(
        target,
        || {
            let snapshot = client
                .query_phase(&target.worktree_id)
                .map_err(|error| anyhow::anyhow!("query Grove cleanup state: {error:?}"))?;
            Ok((snapshot.phase, snapshot.unknown))
        },
        || {
            client
                .cleanup_worktree_create(&target.worktree_id)
                .context("remove aborted Grove journal by worktree ID")
        },
    )?;
    if let Some(data_dir) = target.opts.data_dir.as_deref() {
        let backing = data_dir.join("worktree-backing").join(&target.worktree_id);
        ensure!(!backing.exists(), "Grove backing remained after ID cleanup");
    }
    let pin = format!("refs/grok/worktrees/{}", target.worktree_id);
    let registry = runtime::CommandRegistry::new();
    ensure!(
        !git_success(
            &registry,
            &target.source,
            &["show-ref", "--verify", "--quiet", &pin]
        )?,
        "Grove pin remained after ID cleanup"
    );
    Ok(())
}

fn recover_pre_create(
    opts: &NfsWorktreeOpts,
    worktree_id: &str,
    dest: &Path,
    source: &Path,
    should_query_grove: bool,
) -> Result<()> {
    if should_query_grove {
        let target = CleanupTarget {
            dest: dest.to_path_buf(),
            worktree_id: worktree_id.to_owned(),
            source: source.to_path_buf(),
            opts: opts.clone(),
            is_grove: true,
        };
        cleanup_grove_target(&target).context("recover pre-create Grove state")?;
    } else {
        remove_worktree(dest).context("remove partial pre-create destination")?;
    }
    ensure!(
        dest_is_known_unmounted(dest),
        "pre-create destination mount state is mounted or inconclusive"
    );
    Ok(())
}

fn classify_resolution(
    kind: CaseKind,
    strategy: &str,
    opts: &NfsWorktreeOpts,
    worktree_id: &str,
) -> Result<Resolution> {
    if !matches!(kind, CaseKind::GroveProjected) {
        return Ok(Resolution::Native {
            reason: format!("Grove not requested; product resolved strategy={strategy}"),
        });
    }
    let query = NfsWorktreeClient::from_opts(opts)
        .query_phase(worktree_id)
        .map_err(|error| anyhow::anyhow!("query Grove create classification: {error:?}"))?;
    if is_grove_strategy(strategy) {
        let phase = query.phase.as_deref().unwrap_or("committed");
        Ok(Resolution::Adopted {
            reason: format!("product adopted strategy={strategy}; create_phase={phase}"),
        })
    } else {
        let detail = query
            .declined
            .as_deref()
            .map(|declined| format!("daemon_declined={declined}"))
            .or_else(|| {
                query
                    .phase
                    .as_deref()
                    .map(|phase| format!("create_phase={phase}"))
            })
            .unwrap_or_else(|| {
                "no durable create journal; decline occurred before adoption".into()
            });
        Ok(Resolution::Fallback {
            reason: format!("product fell back to strategy={strategy}; {detail}"),
        })
    }
}

fn cleanup_mount_absent_with(known_unmounted: bool) -> bool {
    known_unmounted
}

fn cleanup_mount_absent(dest: &Path) -> bool {
    cleanup_mount_absent_with(dest_is_known_unmounted(dest))
}

fn verify_cleanup(
    command_registry: &runtime::CommandRegistry,
    source: &Path,
    dest: &Path,
    admin_before: &[u8],
    backing: Option<&Path>,
    pin: Option<&str>,
    was_grove_attempt: bool,
    opts: &NfsWorktreeOpts,
    worktree_id: &str,
) -> Result<CleanupVerification> {
    let dest_absent = fs::symlink_metadata(dest).is_err();
    let mount_absent = cleanup_mount_absent(dest);
    let backing_absent = backing.is_none_or(|path| !path.exists());
    let pin_absent = match pin {
        Some(pin) => !git_success(
            command_registry,
            source,
            &["show-ref", "--verify", "--quiet", pin],
        )?,
        None => true,
    };
    let worktree_admin_unchanged = git_bytes(
        command_registry,
        source,
        &["worktree", "list", "--porcelain"],
    )? == admin_before;
    let journal_absent = if was_grove_attempt {
        NfsWorktreeClient::from_opts(opts)
            .query_phase(worktree_id)
            .map_err(|error| anyhow::anyhow!("verify Grove journal absence: {error:?}"))?
            .unknown
    } else {
        true
    };
    let cleanup = CleanupVerification {
        dest_absent,
        mount_absent,
        backing_absent,
        pin_absent,
        worktree_admin_unchanged,
        journal_absent,
    };
    ensure!(
        cleanup.dest_absent
            && cleanup.mount_absent
            && cleanup.backing_absent
            && cleanup.pin_absent
            && cleanup.worktree_admin_unchanged
            && cleanup.journal_absent,
        "cleanup verification failed for {}: {:?}",
        dest.display(),
        serde_json::to_value(&cleanup)?
    );
    Ok(cleanup)
}

fn ensure_correct(correctness: &Correctness, case: &str, iteration: usize) -> Result<()> {
    ensure!(
        correctness.head_matches
            && correctness.tree_matches
            && correctness.status_matches
            && correctness.staged_state_matches
            && correctness.dirty_state_matches
            && correctness.untracked_state_matches
            && correctness.first_read_matches,
        "correctness mismatch in {case} iteration {iteration}: {:?}",
        serde_json::to_value(correctness)?
    );
    Ok(())
}

fn measured_worker_containment_support() -> ContainmentSupport {
    if cfg!(target_os = "linux") && runtime::CgroupV2::support_available() {
        ContainmentSupport {
            native: true,
            grove: true,
            reason: "all measured workers self-stop before cgroup-v2 enrollment; teardown also signals their process groups".into(),
        }
    } else {
        let reason = if cfg!(target_os = "linux") {
            "all measured cases skipped: writable cgroup-v2 delegation with cgroup.kill is unavailable"
        } else if cfg!(target_os = "macos") {
            "all measured cases skipped: no supported primitive provides race-free containment of descendants that call setsid on macOS"
        } else {
            "all measured cases skipped: reliable full descendant containment is unsupported on this platform"
        };
        ContainmentSupport {
            native: false,
            grove: false,
            reason: reason.into(),
        }
    }
}

fn grove_support(opts: &NfsWorktreeOpts, containment: &ContainmentSupport) -> Support {
    if !containment.grove {
        return Support::Skipped {
            reason: containment.reason.clone(),
        };
    }
    let client = NfsWorktreeClient::from_opts(opts);
    let probe = runtime::probe_grove_support(client.ping());
    if let Some(reason) = runtime::grove_skip_reason(&probe) {
        return Support::Skipped {
            reason: redact_daemon_status_skip_reason(&reason, opts),
        };
    }
    match client.daemon_status() {
        Ok(status)
            if status
                .capabilities
                .iter()
                .any(|capability| capability == "cancel_worktree_create") =>
        {
            Support::Supported
        }
        Ok(_) => Support::Skipped {
            reason:
                "Grove daemon lacks cancel_worktree_create; refusing unsafe pre-journal cleanup"
                    .into(),
        },
        Err(error) => Support::Skipped {
            reason: redact_daemon_status_skip_reason(
                &format!("Grove cancellation capability check failed: {error}"),
                opts,
            ),
        },
    }
}

fn redact_daemon_status_skip_reason(reason: &str, opts: &NfsWorktreeOpts) -> String {
    let paths = [
        opts.control_sock.as_deref(),
        opts.data_dir.as_deref(),
        opts.runtime_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    redact_cleanup_error(reason, &paths)
}

fn expected_grove_transport() -> &'static str {
    if cfg!(target_os = "linux") {
        "fuse"
    } else {
        "nfs"
    }
}

fn choose_read_path(command_registry: &runtime::CommandRegistry, source: &Path) -> Result<PathBuf> {
    let paths = git_bytes(command_registry, source, &["ls-files", "-z"])?;
    paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .find(|path| source.join(path).is_file())
        .context("source has no readable tracked regular file")
}

fn classify_status(status: &[u8]) -> StateCoverage {
    let mut coverage = StateCoverage {
        has_staged: false,
        has_dirty: false,
        has_untracked: false,
    };
    for record in status.split(|byte| *byte == 0) {
        if record.starts_with(b"? ") {
            coverage.has_untracked = true;
        } else if matches!(record.first(), Some(b'1' | b'2' | b'u')) && record.len() >= 4 {
            coverage.has_staged |= record.get(2).copied().is_some_and(|b| b != b'.');
            coverage.has_dirty |= record.get(3).copied().is_some_and(|b| b != b'.');
        }
    }
    coverage
}

fn median_phases(samples: &[Sample]) -> PhaseDurations {
    PhaseDurations {
        create: median(samples.iter().map(|s| s.durations_ms.create)),
        first_readdir: median(samples.iter().map(|s| s.durations_ms.first_readdir)),
        first_read: median(samples.iter().map(|s| s.durations_ms.first_read)),
        first_git_status: median(samples.iter().map(|s| s.durations_ms.first_git_status)),
        second_git_status: median(samples.iter().map(|s| s.durations_ms.second_git_status)),
        remove_cleanup: median(samples.iter().map(|s| s.durations_ms.remove_cleanup)),
    }
}

// Curated build identity inputs, not an exhaustive dependency manifest.
const HARNESS_INPUT_PATHS: &[&str] = &[
    "crates/codegen/distill-fast-worktree/src/bin/worktree_lifecycle_bench.rs",
    "crates/codegen/distill-fast-worktree/src/bin/worktree_lifecycle_bench/runtime.rs",
    "crates/codegen/distill-fast-worktree/Cargo.toml",
    "crates/codegen/distill-fast-worktree/BUILD.bazel",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".bazelrc",
];

fn collect_harness_inputs(
    command_registry: &runtime::CommandRegistry,
    harness_repo: &Path,
) -> Result<Vec<HarnessInput>> {
    HARNESS_INPUT_PATHS
        .iter()
        .map(|path| {
            let bytes = fs::read(harness_repo.join(path))?;
            let sha256 = sha256_hex(&bytes);
            let head = git_bytes(
                command_registry,
                harness_repo,
                &["show", &format!("HEAD:{path}")],
            )
            .ok();
            let head_sha256 = head.as_deref().map(sha256_hex);
            Ok(HarnessInput {
                path,
                matches_head: head_sha256.as_deref() == Some(sha256.as_str()),
                dirty: head_sha256.as_deref() != Some(sha256.as_str()),
                sha256,
                head_sha256,
            })
        })
        .collect()
}

fn dirty_repo_digest(
    command_registry: &runtime::CommandRegistry,
    repo: &Path,
    status: &[u8],
) -> Result<String> {
    let mut digest = sha2::Sha256::new();
    use sha2::Digest;
    digest.update(b"status\0");
    digest.update(status);
    digest.update(b"staged-diff\0");
    digest.update(git_bytes(
        command_registry,
        repo,
        &["diff", "--binary", "--cached", "--no-ext-diff"],
    )?);
    digest.update(b"worktree-diff\0");
    digest.update(git_bytes(
        command_registry,
        repo,
        &["diff", "--binary", "--no-ext-diff"],
    )?);
    for path in git_bytes(
        command_registry,
        repo,
        &[
            "ls-files",
            "-z",
            "--modified",
            "--deleted",
            "--others",
            "--exclude-standard",
        ],
    )?
    .split(|byte| *byte == 0)
    .filter(|path| !path.is_empty())
    {
        digest.update(b"path\0");
        digest.update(path);
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(path));
        let absolute = repo.join(&path);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) => {
                digest.update(b"mode\0");
                digest.update(metadata.mode().to_le_bytes());
                if metadata.file_type().is_symlink() {
                    digest.update(b"symlink\0");
                    digest.update(fs::read_link(&absolute)?.as_os_str().as_bytes());
                } else if metadata.file_type().is_file() {
                    digest.update(b"contents\0");
                    digest.update(fs::read(&absolute)?);
                } else if metadata.file_type().is_dir() {
                    let gitlink_oid = git_text(
                        command_registry,
                        repo,
                        &["-C", path.to_string_lossy().as_ref(), "rev-parse", "HEAD"],
                    )?;
                    digest.update(b"gitlink\0");
                    digest.update(gitlink_oid.as_bytes());
                } else if metadata.file_type().is_block_device()
                    || metadata.file_type().is_char_device()
                    || metadata.file_type().is_fifo()
                    || metadata.file_type().is_socket()
                {
                    digest.update(b"special\0");
                    digest.update(metadata.rdev().to_le_bytes());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                digest.update(b"missing\0");
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect dirty input {}", path.display()));
            }
        }
    }
    Ok(bytes_to_hex(digest.finalize()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    bytes_to_hex(sha2::Sha256::digest(bytes))
}

// sha2 0.10 digests are `GenericArray` (impls `LowerHex`), sha2 0.11 digests are
// `hybrid_array::Array` (no `LowerHex`); Bazel's `@crates//:sha2` is 0.11 while
// Cargo pins 0.10, so encode via `AsRef<[u8]>` which both implement.
fn bytes_to_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => 0.0,
        len if len % 2 == 1 => values.get(len / 2).copied().unwrap_or(0.0),
        len => match (
            (len / 2)
                .checked_sub(1)
                .and_then(|i| values.get(i))
                .copied(),
            values.get(len / 2).copied(),
        ) {
            (Some(a), Some(b)) => (a + b) / 2.0,
            _ => 0.0,
        },
    }
}

fn git_bytes(registry: &runtime::CommandRegistry, cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = runtime::run_git(registry, cwd, args)?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.first().copied().unwrap_or("command"),
        grove_git::redact_git_text(String::from_utf8_lossy(&output.stderr).trim())
    );
    Ok(output.stdout)
}

fn git_text(registry: &runtime::CommandRegistry, cwd: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(registry, cwd, args)?)?
        .trim()
        .to_owned())
}

fn git_success(registry: &runtime::CommandRegistry, cwd: &Path, args: &[&str]) -> Result<bool> {
    Ok(runtime::run_git(registry, cwd, args)?.status.success())
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn interrupted_artifact_json(
    run_id: &str,
    signal: i32,
    cleanup: &InterruptedCleanup,
) -> Result<String> {
    let artifact = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "status": "interrupted",
        "run_id": run_id,
        "signal": signal,
        "source": REDACTED_PATH,
        "output": REDACTED_PATH,
        "cleanup": cleanup,
    });
    serde_json::to_string_pretty(&artifact).context("serialize interrupted artifact")
}

struct ArtifactFinalizer {
    output: Option<PathBuf>,
    finalized: Mutex<bool>,
}

impl ArtifactFinalizer {
    fn new(output: Option<PathBuf>) -> Self {
        Self {
            output,
            finalized: Mutex::new(false),
        }
    }

    fn finalize(&self, json: &str) -> Result<()> {
        let mut finalized = self
            .finalized
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *finalized {
            return Ok(());
        }
        emit_final_json(self.output.as_deref(), json)?;
        *finalized = true;
        Ok(())
    }
}

fn emit_final_json(output: Option<&Path>, json: &str) -> Result<()> {
    if let Some(output) = output {
        write_atomic(output, json.as_bytes())
    } else {
        println!("{json}");
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persist {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "worktree_lifecycle_bench/tests.rs"]
mod tests;
