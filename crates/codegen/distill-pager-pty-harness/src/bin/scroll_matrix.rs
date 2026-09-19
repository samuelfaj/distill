// Modified for Distill by Samuel Fajreldines, 2026.
//! Runs matrix cells (`scroll_matrix::CELLS`) against a real pager binary in a PTY and prints the per-cell verdict table.
//! Writes `report.json` into the artifacts dir, next to each cell's recorder capture.
//! Exits nonzero iff any cell failed or an xfail cell passed.
//! The curated tier also runs in CI as `tests/scroll_matrix_curated.rs`.
//! This binary is the local entry point for the full sweep and for one-off cell reruns (`--filter`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser as ClapParser, ValueEnum};
use distill_pager_pty_harness::pager_binary;
use distill_pager_pty_harness::scroll_matrix::{
    CELLS, CellReport, MatrixCell, Tier, exit_code, run_cell, summary_table, write_report_json,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TierArg {
    /// The CI subset.
    Curated,
    /// Every cell (curated and full-tier rows).
    Full,
}

#[derive(ClapParser, Debug)]
#[command(
    name = "scroll-matrix",
    about = "Run the scroll validation matrix against distill-pager",
    long_about = None,
)]
struct Cli {
    /// Cell tier to run.
    #[arg(long, value_enum, default_value_t = TierArg::Curated)]
    tier: TierArg,

    /// Only run cells whose id contains this substring.
    #[arg(long, value_name = "SUBSTR")]
    filter: Option<String>,

    /// Default 1: parallel cells stretch inter-report sleeps and can flip Auto classification. Invariants use the recorder clock, so more jobs stay sound.
    #[arg(long, value_name = "N", default_value_t = 1)]
    jobs: usize,

    /// Directory for report.json and the per-cell recorder captures.
    #[arg(long, value_name = "DIR", default_value = "target/scroll-matrix")]
    artifacts: PathBuf,

    /// Pager binary.
    /// Defaults to PAGER_BINARY, CARGO_BIN_EXE_distill, or a locally-built debug binary.
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
}

// Multi-thread runtime required: run_cell drives its blocking cell body via Handle::block_on on a spawn_blocking thread (see scroll_matrix::runner)
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("scroll-matrix failed: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let binary = match cli.binary {
        Some(path) => path,
        None => pager_binary().context("resolve pager binary")?,
    };
    if !binary.exists() {
        bail!("pager binary does not exist: {}", binary.display());
    }

    let cells: Vec<&'static MatrixCell> = CELLS
        .iter()
        .filter(|cell| cli.tier == TierArg::Full || cell.tier == Tier::Curated)
        .filter(|cell| {
            cli.filter
                .as_deref()
                .is_none_or(|needle| cell.id.contains(needle))
        })
        .collect();
    if cells.is_empty() {
        bail!(
            "no cells match --tier {:?} --filter {:?}",
            cli.tier,
            cli.filter
        );
    }

    // The pager child resolves GROK_SCROLL_LOG against ITS cwd (the harness's temp workspace)
    // A relative artifacts dir (including the default) would scatter captures there and starve the finalize wait
    // Absolutize against the invoking cwd
    let artifacts = std::path::absolute(&cli.artifacts)
        .with_context(|| format!("absolutize artifacts dir {}", cli.artifacts.display()))?;

    let reports = run_cells(&cells, cli.jobs.max(1), &binary, &artifacts).await;
    print!("{}", summary_table(&reports));
    let report_path = write_report_json(&reports, &artifacts)?;
    eprintln!("report: {}", report_path.display());
    Ok(ExitCode::from(exit_code(&reports)))
}

/// Run every cell, preserving table order.
/// `jobs == 1` runs inline (the default, which keeps gesture timing representative); higher values fan out over a semaphore.
async fn run_cells(
    cells: &[&'static MatrixCell],
    jobs: usize,
    binary: &std::path::Path,
    artifacts: &std::path::Path,
) -> Vec<CellReport> {
    if jobs == 1 {
        let mut reports = Vec::with_capacity(cells.len());
        for (i, cell) in cells.iter().enumerate() {
            eprintln!("[{}/{}] {}", i + 1, cells.len(), cell.id);
            reports.push(run_cell(cell, binary, artifacts).await);
        }
        return reports;
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs));
    let mut set = tokio::task::JoinSet::new();
    for (index, &cell) in cells.iter().enumerate() {
        let semaphore = semaphore.clone();
        let (binary, artifacts) = (binary.to_path_buf(), artifacts.to_path_buf());
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore open");
            eprintln!("running {}", cell.id);
            (index, run_cell(cell, &binary, &artifacts).await)
        });
    }
    let mut indexed = Vec::with_capacity(cells.len());
    while let Some(joined) = set.join_next().await {
        // run_cell converts cell panics into Fail reports itself.
        indexed.push(joined.expect("cell task join"));
    }
    indexed.sort_by_key(|(index, _)| *index);
    indexed.into_iter().map(|(_, report)| report).collect()
}
