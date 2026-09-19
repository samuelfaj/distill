// Modified for Distill by Samuel Fajreldines, 2026.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use distill_pager_pty_harness::{ContentController, PtyExitPoll, PtyHarness};

const WELCOME_SENTINEL: &str = "Quit";
const COMPOSER_PROBE_KEYS: &str = "zzx";
const CTRL_Q: &[u8] = b"\x11";
const EXIT_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
enum BootMode {
    /// A first boot into an empty grok home.
    Cold,
    /// Seed a session with real turns, quit, then measure `--continue`.
    Resume,
}

#[derive(Parser, Debug)]
#[command(
    about = "Time one interactive pager boot in a PTY, up to first interactive frame.",
    long_about = None
)]
struct Args {
    /// Pager binary under test.
    #[arg(long)]
    binary: PathBuf,
    /// Workspace the pager boots in; the repo-size fixture.
    #[arg(long)]
    cwd: PathBuf,
    #[arg(long, default_value_t = 50)]
    rows: u16,
    #[arg(long, default_value_t = 120)]
    cols: u16,
    #[arg(long, value_enum, default_value_t = BootMode::Cold)]
    mode: BootMode,
    /// Turns to drive into the seed session before a `Resume` boot.
    #[arg(long, default_value_t = 12)]
    seed_turns: usize,
    /// Words per seeded agent response; the knob that sets resume log size.
    #[arg(long, default_value_t = 400)]
    seed_response_words: usize,
    /// Deadline for each individual wait (welcome, echo, turn idle).
    #[arg(long, default_value_t = 120_000)]
    timeout_ms: u64,
    /// Where the JSON observation record is written.
    #[arg(long)]
    out: PathBuf,
    /// Where the measured boot's `unified.jsonl` is copied before the sandbox is removed.
    #[arg(long)]
    log_out: PathBuf,
}

#[derive(Serialize)]
struct Observation {
    mode: BootMode,
    cwd: String,
    seed_turns: usize,
    resume_updates_bytes: u64,
    spawn_at_unix_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    first_frame_ms: Option<u64>,
    welcome_ms: u64,
    interactive_ms: u64,
    keystroke_latency_ms: u64,
    frames_to_interactive: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("build probe runtime")?;
    let observation = runtime.block_on(run(&args))?;

    let json = serde_json::to_string(&observation).context("serialize observation")?;
    std::fs::write(&args.out, format!("{json}\n"))
        .with_context(|| format!("write {}", args.out.display()))?;
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{json}").ok();
    Ok(())
}

async fn run(args: &Args) -> Result<Observation> {
    let timeout = Duration::from_millis(args.timeout_ms);
    let content = ContentController::start()
        .await
        .context("start mock inference server")?;

    let mut resume_updates_bytes = 0;
    if args.mode == BootMode::Resume {
        seed_session(args, &content, timeout).context("seed the session to resume")?;
        resume_updates_bytes = largest_updates_log(content.sandbox().distill_home());
    }

    let measured = measure_boot(args, &content, timeout, resume_updates_bytes)?;

    let log = content
        .sandbox()
        .distill_home()
        // `distill_telemetry::unified_log::LOG_DIR` is unreachable without a telemetry dependency.
        .join("logs")
        .join("unified.jsonl");
    std::fs::copy(&log, &args.log_out).with_context(|| {
        format!(
            "copy {} to {} (the sandbox is removed when this process exits)",
            log.display(),
            args.log_out.display()
        )
    })?;
    Ok(measured)
}

fn seed_session(args: &Args, content: &ContentController, timeout: Duration) -> Result<()> {
    let mut seeder = PtyHarness::spawn_with_content_in_dir(
        &args.binary,
        args.rows,
        args.cols,
        content,
        &[],
        Some(&args.cwd),
    )
    .context("spawn the seeding pager")?;
    seeder
        .wait_for_text(WELCOME_SENTINEL, timeout)
        .context("seeding pager reached the welcome screen")?;

    for turn in 0..args.seed_turns {
        content.set_response(seed_response(turn, args.seed_response_words));
        seeder
            .inject_keys(format!("seed turn {turn}\r").as_bytes())
            .with_context(|| format!("submit seed turn {turn}"))?;
        seeder
            .wait_for_turn_idle(timeout)
            .with_context(|| format!("seed turn {turn} settled"))?;
    }
    quit_pager(&mut seeder).context("quit the seeding pager")
}

fn ready_sentinel(args: &Args) -> String {
    match args.mode {
        BootMode::Cold => WELCOME_SENTINEL.to_owned(),
        // `--continue` skips the welcome screen; ready is the replayed history tail.
        BootMode::Resume => last_seed_word(args),
    }
}

fn last_seed_word(args: &Args) -> String {
    format!(
        "t{}w{}",
        args.seed_turns.saturating_sub(1),
        args.seed_response_words.saturating_sub(1)
    )
}

fn measure_boot(
    args: &Args,
    content: &ContentController,
    timeout: Duration,
    resume_updates_bytes: u64,
) -> Result<Observation> {
    let sentinel = ready_sentinel(args);
    let mut argv: Vec<String> = Vec::new();
    if args.mode == BootMode::Resume {
        argv.push("--continue".to_owned());
    }
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    // Wall-clock twin of `spawn`, joined with the child record's `ts`; the two monotonic clocks share no origin.
    let spawn_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .ok();
    let spawn = Instant::now();
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &args.binary,
        args.rows,
        args.cols,
        content,
        &argv_refs,
        Some(&args.cwd),
    )
    .context("spawn the measured pager")?;

    let mut first_byte_ms = None;
    let mut first_frame_ms = None;
    harness
        .wait_until("the boot-ready marker", timeout, |h| {
            if first_byte_ms.is_none() && !h.raw_output().is_empty() {
                first_byte_ms = Some(spawn.elapsed().as_millis() as u64);
            }
            if first_frame_ms.is_none() && h.frame_count() > 0 {
                first_frame_ms = Some(spawn.elapsed().as_millis() as u64);
            }
            h.contains_text(&sentinel)
        })
        .context("measured pager reached its boot-ready marker")?;
    let welcome_ms = spawn.elapsed().as_millis() as u64;

    let injected = Instant::now();
    harness
        .inject_keys(COMPOSER_PROBE_KEYS.as_bytes())
        .context("type the composer probe keys")?;
    harness
        .wait_until("the composer to echo the probe keystroke", timeout, |h| {
            h.contains_text(COMPOSER_PROBE_KEYS)
        })
        .context("composer echoed the probe keystroke")?;
    let interactive_ms = spawn.elapsed().as_millis() as u64;
    let keystroke_latency_ms = injected.elapsed().as_millis() as u64;
    let frames_to_interactive = harness.frame_count();

    quit_pager(&mut harness).context("quit the measured pager")?;

    let seed_turns = match args.mode {
        BootMode::Cold => 0,
        BootMode::Resume => args.seed_turns,
    };
    Ok(Observation {
        mode: args.mode,
        cwd: args.cwd.to_string_lossy().into_owned(),
        seed_turns,
        resume_updates_bytes,
        spawn_at_unix_ms,
        first_byte_ms,
        first_frame_ms,
        welcome_ms,
        interactive_ms,
        keystroke_latency_ms,
        frames_to_interactive,
    })
}

fn quit_pager(harness: &mut PtyHarness) -> Result<()> {
    harness.inject_keys(CTRL_Q)?;
    harness.update(Duration::from_millis(200));
    harness.inject_keys(CTRL_Q)?;
    match harness.wait_exit_code(EXIT_WAIT)? {
        PtyExitPoll::Running => harness.quit(),
        PtyExitPoll::Exited(_) | PtyExitPoll::PendingStatus => Ok(()),
    }
}

// Every word is unique to its turn, so the resume ready marker (the final turn's final word) cannot match an earlier turn's replayed paint
fn seed_response(turn: usize, words: usize) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(words * 10);
    for word in 0..words {
        if word > 0 {
            text.push(' ');
        }
        let _ = write!(text, "t{turn}w{word}");
    }
    text
}

fn largest_updates_log(distill_home: &Path) -> u64 {
    let mut largest = 0;
    let mut pending = vec![distill_home.join("sessions")];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                eprintln!("warning: skipping {}: {err}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == "updates.jsonl") {
                largest = largest.max(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
            }
        }
    }
    largest
}
