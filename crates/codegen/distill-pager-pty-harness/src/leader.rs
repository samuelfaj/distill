// Modified for Distill by Samuel Fajreldines, 2026.
//! Multi-client leader cluster: one shared leader plus N pager clients.
//!
//! Every other leader test is single-client/single-leader; [`LeaderCluster`] covers "one leader, several pager clients sharing its session".
//! One [`ContentController`] gives one shared `$HOME` (hence one elected leader) plus a fixed leader socket beneath its `GROK_HOME`.
//! Clients spawn with the `--leader`/`--leader-socket` flags so they all attach to the SAME leader.
//! It also exposes the leader's durable `updates.jsonl` log so a reattach test can assert on the persisted, replayable turn-completion records.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{ContentController, PtyHarness, pager_binary};

/// One shared mock-backed leader plus the pager clients attached to it.
pub struct LeaderCluster {
    content: ContentController,
    binary: PathBuf,
    socket: PathBuf,
    rows: u16,
    cols: u16,
}

impl LeaderCluster {
    /// Start the cluster: one [`ContentController`] (one shared `$HOME`, so one leader) and a fixed leader socket under its `GROK_HOME`.
    pub async fn start(rows: u16, cols: u16) -> Result<Self> {
        let content = ContentController::start()
            .await
            .context("start content controller")?;
        // One shared GROK_HOME means one leader; the socket lives beneath it so every client (sharing the same env) elects/attaches to the same one
        let distill_home = content.home().join(".grok");
        std::fs::create_dir_all(&distill_home).context("create grok home")?;
        let socket = distill_home.join("leader-e2e.sock");
        let binary = pager_binary().context("resolve pager binary")?;
        Ok(Self {
            content,
            binary,
            socket,
            rows,
            cols,
        })
    }

    /// Spawn the leader-electing client (`--leader --leader-socket <S>` plus `extra_args`); it starts a fresh session and brings up the leader.
    pub fn spawn_leader(&self, extra_args: &[&str]) -> Result<PtyHarness> {
        self.spawn_client(&[], extra_args)
    }

    /// Attach another client that resumes the shared session through the SAME leader (`--leader --leader-socket <S> --resume` plus `extra_args`).
    pub fn attach(&self, extra_args: &[&str]) -> Result<PtyHarness> {
        self.spawn_client(&["--resume"], extra_args)
    }

    /// Spawn a client wired to the shared leader socket.
    /// `mode_args` carries the per-role flag (`--resume` for attachers); `extra_args` is the caller's.
    fn spawn_client(&self, mode_args: &[&str], extra_args: &[&str]) -> Result<PtyHarness> {
        let socket = self.socket.to_str().context("socket path is utf-8")?;
        let mut args: Vec<&str> = vec!["--leader", "--leader-socket", socket];
        args.extend_from_slice(mode_args);
        args.extend_from_slice(extra_args);
        PtyHarness::spawn_with_content(&self.binary, self.rows, self.cols, &self.content, &args)
            .context("spawn pager client on shared leader")
    }

    /// The shared content controller (mock inference server and sandbox env).
    pub fn content(&self) -> &ContentController {
        &self.content
    }

    /// The cluster's sessions root: `GROK_HOME/sessions` (layout below is `sessions/<encoded-cwd>/<session-id>/updates.jsonl`).
    fn sessions_dir(&self) -> PathBuf {
        self.content.home().join(".grok").join("sessions")
    }

    /// `params.update` from every session file. A vanished or torn multi-byte tail is skipped; the next call retries.
    pub fn session_updates(&self) -> Vec<Value> {
        let mut files = Vec::new();
        collect_updates_files(&self.sessions_dir(), &mut files);
        let mut out = Vec::new();
        for file in files {
            // Skip a vanished file or a torn multi-byte tail; the next poll retries.
            if let Ok(text) = std::fs::read_to_string(&file) {
                out.extend(parse_update_payloads(&text));
            }
        }
        out
    }

    /// Poll [`session_updates`](Self::session_updates) until a record with `sessionUpdate == "turn_completed"` appears.
    /// Returns that (inner) update payload, or errors on timeout.
    /// Scans ALL sessions under the cluster (fine for the single-session clusters these tests build).
    pub fn wait_for_turn_completed(&self, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let updates = self.session_updates();
            if let Some(rec) = updates.iter().find(|u| is_turn_completed(u)) {
                return Ok(rec.clone());
            }
            if Instant::now() >= deadline {
                // Include what WAS persisted in the error
                // Zero records means an env problem; records without a turn_completed mean a producer regression
                let tags: std::collections::BTreeSet<&str> = updates
                    .iter()
                    .filter_map(|u| u.get("sessionUpdate").and_then(Value::as_str))
                    .collect();
                anyhow::bail!(
                    "timed out after {timeout:?} waiting for a turn_completed record under {}; \
                     saw {} update record(s) with sessionUpdate tags {tags:?}",
                    self.sessions_dir().display(),
                    updates.len(),
                );
            }
            // Sync FS poll mirrors the harness's blocking wait_for_text; a stat every 150ms is cheap and fine on a multi_thread runtime worker
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

/// Whether a session-update payload is a `turn_completed` terminal.
fn is_turn_completed(update: &Value) -> bool {
    update.get("sessionUpdate").and_then(Value::as_str) == Some("turn_completed")
}

/// Skips blank, unparseable, or update-less lines. A torn multi-byte tail fails the file read upstream instead.
fn parse_update_payloads(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let envelope: Value = serde_json::from_str(line).ok()?;
            envelope.get("params")?.get("update").cloned()
        })
        .collect()
}

/// Recursively collect every `updates.jsonl` beneath `dir` (a manual walk to avoid a new crate dep).
/// A missing/unreadable dir yields nothing; sessions may not exist yet, and the walk is re-run on every poll.
fn collect_updates_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // No-follow file type: a symlinked directory has `is_dir() == false`, so a symlink cycle can never recurse forever here
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_updates_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("updates.jsonl") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a session update as the envelope stored in `updates.jsonl`.
    fn envelope(update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{update_json}}}}}"#
        )
    }

    #[test]
    fn parse_update_payloads_unwraps_and_tolerates_torn_trailing_line() {
        let body = format!(
            "{}\n{}\n{{\"timestamp\":2,\"method\":\"_x.ai/sess",
            envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#
            ),
            envelope(
                r#"{"sessionUpdate":"turn_completed","prompt_id":"p-1","stop_reason":"end_turn"}"#
            ),
        );
        let updates = parse_update_payloads(&body);
        // The two complete lines parse; the torn final line is dropped.
        assert_eq!(updates.len(), 2);
        let completed = updates
            .iter()
            .find(|u| is_turn_completed(u))
            .expect("turn_completed payload is unwrapped from params.update");
        assert_eq!(completed["stop_reason"], "end_turn");
        assert_eq!(completed["prompt_id"], "p-1");
    }

    #[test]
    fn parse_update_payloads_skips_blank_and_payloadless_lines() {
        let body = format!(
            "\n   \n{}\n{{\"timestamp\":3,\"method\":\"x\",\"params\":{{\"sessionId\":\"s\"}}}}\n",
            envelope(
                r#"{"sessionUpdate":"turn_completed","prompt_id":"p","stop_reason":"cancelled"}"#
            ),
        );
        let updates = parse_update_payloads(&body);
        // Only the one envelope carrying params.update survives.
        assert_eq!(updates.len(), 1);
        assert!(is_turn_completed(&updates[0]));
        assert_eq!(updates[0]["stop_reason"], "cancelled");
    }
}
