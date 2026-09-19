// Modified for Distill by Samuel Fajreldines, 2026.
//! Generates a compact git status for the system prompt.
//!
//! Uses the git CLI for performance: libgit2's status is 5-10x slower than the native git binary on large repos due to inefficient index refresh.
//! Output is prioritized by change type and limited to ~1k characters.

use crate::file_system::FsError;
use crate::file_system::fsmonitor::FsmonitorOverride;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Gets a compact git status for the system prompt using the git CLI.
/// Total output is capped at ~1k characters.
#[tracing::instrument(skip_all)]
pub async fn git_status(working_directory: impl Into<PathBuf>) -> Result<String, FsError> {
    let working_directory = working_directory.into();
    let permit = crate::git_odb::try_acquire_odb();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        git_status_impl(&working_directory)
    })
    .await
    .map_err(|e| FsError::Other(format!("git status task failed: {e}")))?
}

/// Matches Node's default `execFile` `maxBuffer` (1 MiB).
/// `git status` output at or above the cap is treated as an error, so the repo is dropped from `<git_status>` entirely (never truncated).
const GIT_STATUS_BUFFER_LIMIT: usize = 1024 * 1024;

/// Whether `git status` stdout is large enough that the repo is dropped (`>= 1 MiB`).
fn git_status_exceeds_buffer(stdout_len: usize) -> bool {
    stdout_len >= GIT_STATUS_BUFFER_LIMIT
}

/// Collapse runs of two or more spaces to one (porcelain two-column status); newlines untouched.
fn collapse_status_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch);
    }
    out
}

#[tracing::instrument(skip_all)]
pub async fn git_status_short_pinned(
    working_directory: impl Into<PathBuf>,
    fsmonitor: FsmonitorOverride,
) -> Result<String, FsError> {
    let working_directory = working_directory.into();
    // Held for the whole run to bound ODB contention.
    let _permit = crate::git_odb::try_acquire_odb();

    // libgit2 config walk is blocking; keep it off the async scheduler.
    let pin_cwd = working_directory.clone();
    let filter_pins = tokio::task::spawn_blocking(move || {
        crate::git_content_filters::content_filter_config_pins(&pin_cwd)
    })
    .await
    .map_err(|e| FsError::Other(format!("git config pin task failed: {e}")))?
    .ok_or_else(|| FsError::Other("unreadable git config".to_string()))?;

    let mut cmd = distill_tty_utils::git_command();
    for pin in &filter_pins {
        cmd.args(["-c", pin.as_str()]);
    }
    cmd.args(["-c", fsmonitor.git_config_arg()]);
    cmd.args(["status", "--short", "--branch", "--untracked-files=normal"])
        .current_dir(&working_directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // GIT_OPTIONAL_LOCKS=0: a session-triggered status must not fight the user's git on index.lock.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");

    let output = super::process::output_killing_group_on_drop(cmd)
        .await
        .map_err(|e| {
            FsError::Other(format!(
                "git status --short --branch --untracked-files=normal failed: {}",
                e
            ))
        })?;

    if !output.status.success() {
        return Err(FsError::Other(format!(
            "git status --short --branch exited with code {:?}",
            output.status.code()
        )));
    }

    if git_status_exceeds_buffer(output.stdout.len()) {
        return Err(FsError::Other(
            "git status --short --branch output exceeded 1 MiB buffer".to_string(),
        ));
    }

    Ok(collapse_status_spaces(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[tracing::instrument(skip_all)]
fn git_status_impl(working_directory: &Path) -> Result<String, FsError> {
    let max_status_chars = 1000;
    let mut output = String::with_capacity(max_status_chars);

    let branch_name = { run_git(working_directory, &["rev-parse", "--abbrev-ref", "HEAD"]) };

    match &branch_name {
        Some(branch) if branch == "HEAD" => {
            if let Some(hash) = run_git(working_directory, &["rev-parse", "--short", "HEAD"]) {
                let _ = writeln!(output, "HEAD detached at {}", hash);
            }
        }
        Some(branch) => {
            let _ = writeln!(output, "On branch {}", branch);
        }
        None => {
            return Err(FsError::Other("not a git repository".to_string()));
        }
    }

    {
        if let Some(upstream_name) = run_git(
            working_directory,
            &["rev-parse", "--abbrev-ref", "@{upstream}"],
        ) && let Some(counts) = run_git(
            working_directory,
            &["rev-list", "--count", "--left-right", "@{upstream}...HEAD"],
        ) {
            let parts: Vec<&str> = counts.split_whitespace().collect();
            if let (Some(behind_str), Some(ahead_str)) = (parts.first(), parts.get(1)) {
                let behind: usize = behind_str.parse().unwrap_or(0);
                let ahead: usize = ahead_str.parse().unwrap_or(0);

                let status_msg = match (ahead, behind) {
                    (0, 0) => {
                        format!("Your branch is up to date with '{}'.", upstream_name)
                    }
                    (a, 0) => format!(
                        "Your branch is ahead of '{}' by {} commit{}.",
                        upstream_name,
                        a,
                        if a == 1 { "" } else { "s" }
                    ),
                    (0, b) => format!(
                        "Your branch is behind '{}' by {} commit{}.",
                        upstream_name,
                        b,
                        if b == 1 { "" } else { "s" }
                    ),
                    (a, b) => format!(
                        "Your branch and '{}' have diverged ({} ahead, {} behind).",
                        upstream_name, a, b
                    ),
                };
                let _ = writeln!(output, "{}", status_msg);
            }
        }
    }

    let staged_output = {
        run_git(
            working_directory,
            &["diff", "--cached", "--name-status", "HEAD"],
        )
    };

    let mut staged: Vec<String> = Vec::new();
    if let Some(ref diff_output) = staged_output {
        for line in diff_output.lines() {
            let mut parts = line.splitn(2, '\t');
            let status_char = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let formatted = match status_char.chars().next() {
                Some('A') => format!("\tnew file: {}", path),
                Some('M') => format!("\tmodified: {}", path),
                Some('D') => format!("\tdeleted: {}", path),
                Some('R') => format!("\trenamed: {}", path),
                _ => format!("\t{}: {}", status_char, path),
            };
            staged.push(formatted);
        }
    }

    if staged.is_empty() {
        let _ = writeln!(output, "\nnothing to commit, working tree clean");
        return Ok(output);
    }

    let reserve_for_truncation = 50;
    let char_budget = max_status_chars - reserve_for_truncation;

    if !staged.is_empty() && output.len() < char_budget {
        let _ = writeln!(output, "\nChanges to be committed:");
        for (shown, line) in staged.iter().enumerate() {
            if output.len() + line.len() + 1 > char_budget {
                let remaining = staged.len() - shown;
                if remaining > 0 {
                    let _ = writeln!(output, "\t... and {} more staged", remaining);
                }
                break;
            }
            let _ = writeln!(output, "{}", line);
        }
    }

    Ok(output)
}

/// Run a read-only git command and return trimmed stdout, or `None` on failure.
/// `--no-optional-locks` avoids `index.lock`: this runs from background tasks and must not contend with foreground git.
#[tracing::instrument(level = "debug", skip(cwd))]
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    // Caller is already on a blocking pool (`git_status_impl`); do not nest spawn_blocking.
    let filter_pins = crate::git_content_filters::content_filter_config_pins(cwd)?;
    let mut cmd = distill_tty_utils::git_command();
    for pin in &filter_pins {
        cmd.args(["-c", pin.as_str()]);
    }
    let output = cmd
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_buffer_cap_matches_spec() {
        assert!(!git_status_exceeds_buffer(0));
        assert!(!git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT - 1));
        assert!(git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT));
        assert!(git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT + 1));
    }

    #[cfg(unix)]
    fn init_attributed_repo(tmp: &Path) -> git2::Repository {
        let repo = git2::Repository::init(tmp).unwrap();
        std::fs::write(tmp.join(".gitattributes"), "data.txt filter=Pwn\n").unwrap();
        std::fs::write(tmp.join("data.txt"), "hello\n").unwrap();
        // Commit before installing the filter so commit itself does not run it.
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitattributes")).unwrap();
        index.add_path(Path::new("data.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        repo
    }

    #[cfg(unix)]
    fn write_filter_script(tmp: &Path, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = tmp.join("pwn.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexec cat\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    // Same length as the committed body plus index mtime: status re-hashes via clean.
    #[cfg(unix)]
    fn dirty_for_clean_filter(tmp: &Path) {
        std::fs::write(tmp.join("data.txt"), "HELLO\n").unwrap();
        let data = std::fs::File::open(tmp.join("data.txt")).unwrap();
        let index = std::fs::File::open(tmp.join(".git/index")).unwrap();
        let mtime = index.metadata().unwrap().modified().unwrap();
        data.set_modified(mtime).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_status_short_does_not_run_content_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let _repo = init_attributed_repo(tmp.path());
        let marker = tmp.path().join("pwned");
        let script = write_filter_script(tmp.path(), &marker);
        let cfg_path = tmp.path().join(".git/config");
        let mut cfg = std::fs::read_to_string(&cfg_path).unwrap();
        cfg.push_str(&format!(
            "\n[filter \"Pwn\"]\n\tclean = {}\n",
            script.display()
        ));
        std::fs::write(&cfg_path, cfg).unwrap();
        dirty_for_clean_filter(tmp.path());

        let _ = std::fs::remove_file(&marker);
        distill_tty_utils::git_command()
            .args(["status", "--short"])
            .current_dir(tmp.path())
            .output()
            .expect("unpinned status");
        assert!(marker.exists(), "unpinned status must run the filter");
        std::fs::remove_file(&marker).unwrap();

        git_status_short_pinned(tmp.path(), FsmonitorOverride::Disabled)
            .await
            .expect("pinned status");
        assert!(!marker.exists(), "pinned status must not run the filter");
    }

    /// Staged entries collapse the porcelain double space, while leading single spaces and ` -> ` are preserved.
    #[test]
    fn collapse_status_spaces_matches_spec() {
        let raw = "## main...origin/main\n M committed.txt\nA  staged.txt\nM  mod.txt\nR  old.txt -> new.txt\n?? untracked.txt\n";
        let want = "## main...origin/main\n M committed.txt\nA staged.txt\nM mod.txt\nR old.txt -> new.txt\n?? untracked.txt\n";
        assert_eq!(collapse_status_spaces(raw), want);

        // Newlines are never touched (empty input and runs of blank lines survive).
        assert_eq!(collapse_status_spaces(""), "");
        assert_eq!(collapse_status_spaces("a\n\n\nb"), "a\n\n\nb");
    }
}
