// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

fn workspace_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(|p| p.to_path_buf())
        .context("failed to resolve workspace root from CARGO_MANIFEST_DIR")
}

fn target_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            workspace_root()
                .expect("workspace root for target_dir fallback")
                .join("target")
        }))
}

fn local_pager_binary_path() -> Result<PathBuf> {
    Ok(target_dir()?
        .join("debug")
        .join(format!("distill-pager{}", std::env::consts::EXE_SUFFIX)))
}

fn ensure_local_pager_binary(binary: &std::path::Path) -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(workspace_root()?)
        .args([
            "build",
            "-p",
            "distill-pager-bin",
            "--bin",
            "distill-pager",
        ])
        .stdin(Stdio::null())
        .envs(distill_tty_utils::pager_env());
    distill_tty_utils::detach_std_command(&mut cmd);
    let output = cmd
        .output()
        .with_context(|| format!("failed to spawn {cargo} to build distill-pager"))?;

    if !output.status.success() {
        bail!(
            "failed to build distill-pager (exit {:?})\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    if !binary.exists() {
        bail!(
            "distill-pager build completed but binary missing at {}",
            binary.display()
        );
    }
    Ok(())
}

/// `PAGER_BINARY`, then `CARGO_BIN_EXE_distill`, else build `distill-pager-bin` (the package that owns the binary).
pub fn pager_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("PAGER_BINARY") {
        let p = PathBuf::from(path);
        if !p.exists() {
            bail!("PAGER_BINARY does not exist: {}", p.display());
        }
        // Bazel sets PAGER_BINARY to a runfiles-relative path; portable_pty resolves non-absolute paths via PATH lookup instead of the cwd
        return std::path::absolute(&p)
            .with_context(|| format!("failed to absolutize PAGER_BINARY: {}", p.display()));
    }

    if let Ok(path) = std::env::var("CARGO_BIN_EXE_distill") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    let binary = local_pager_binary_path()?;
    ensure_local_pager_binary(&binary)?;
    Ok(binary)
}
