//! Skip-matrix coverage for [`super::run_grove_arm`] with a scripted Status.

use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{GroveArm, SourceMount, grove_source_skip, run_grove_arm};
use crate::grove_api::CAP_FORK_FROM_BACKING;
use crate::grove_client::{ControlConn, GroveWorktreeClient, NfsWorktreeOpts, Transport};
use crate::worktree::GroveSkip;
use crate::worktree::plan::WorktreePlan;
use crate::{CreationMode, IgnoredFilesMode, WorkingTreeMode};

#[derive(Clone)]
struct Script {
    status: serde_json::Value,
    declined: Option<&'static str>,
    creates: Arc<AtomicUsize>,
    endpoint: PathBuf,
}

struct Conn {
    script: Script,
    written: Vec<u8>,
    reply: Option<Cursor<Vec<u8>>>,
}

impl Conn {
    fn reply(&mut self) -> io::Result<&mut Cursor<Vec<u8>>> {
        if self.reply.is_none() {
            let req: serde_json::Value = serde_json::from_slice(&self.written)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
            let data = match op {
                "ping" => serde_json::json!({"v": 1, "pong": true}),
                "status" => self.script.status.clone(),
                "create_worktree" => {
                    self.script.creates.fetch_add(1, Ordering::SeqCst);
                    match self.script.declined {
                        Some(gate) => serde_json::json!({"v": 1, "declined": gate}),
                        None => serde_json::json!({
                            "v": 1,
                            "create_phase": "committed",
                            "mount": {"port": 0, "mount_id": "m1", "transport": "nfs"}
                        }),
                    }
                }
                "remove_worktree" => serde_json::json!({"v": 1}),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected op {op}"),
                    ));
                }
            };
            let line = serde_json::json!({"status": "ok", "data": data}).to_string() + "\n";
            self.reply = Some(Cursor::new(line.into_bytes()));
        }
        Ok(self.reply.as_mut().unwrap())
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reply()?.read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ControlConn for Conn {
    fn shutdown_write(&self) {}
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Script")
    }
}

impl Transport for Script {
    type Conn = Conn;

    fn connect(&self, _timeout: std::time::Duration) -> anyhow::Result<Conn> {
        Ok(Conn {
            script: self.clone(),
            written: Vec::new(),
            reply: None,
        })
    }

    fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    fn endpoint_exists(&self) -> bool {
        true
    }

    fn daemon_lock_free(&self) -> bool {
        false
    }

    fn dest_is_known_unmounted(&self, _dest: &Path) -> bool {
        true
    }
}

struct FakeArm {
    mount: SourceMount,
    script: Script,
}

impl GroveArm for FakeArm {
    type Transport = Script;

    fn client(&self, opts: &NfsWorktreeOpts) -> GroveWorktreeClient<Script> {
        GroveWorktreeClient::new(self.script.clone(), opts)
    }

    fn host_decline(&self, _plan: &WorktreePlan) -> Option<GroveSkip> {
        None
    }

    fn source_mount(&self, _source: &Path) -> SourceMount {
        self.mount
    }

    fn dest_still_mounted(&self, _dest: &Path) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

struct Fixture {
    tmp: TempDir,
    creates: Arc<AtomicUsize>,
    endpoint: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo")).unwrap();
        let endpoint = tmp.path().join("mock.sock");
        Self {
            tmp,
            creates: Arc::new(AtomicUsize::new(0)),
            endpoint,
        }
    }

    fn plan(&self, preserve: bool, git_ref: &str) -> WorktreePlan {
        let dest = self.tmp.path().join("dest");
        WorktreePlan {
            source: self.tmp.path().join("repo"),
            dest: dest.clone(),
            git_ref: git_ref.into(),
            parallelism: 1,
            channel_buffer: 8,
            working_tree: if preserve {
                WorkingTreeMode::PreserveWorkingTree
            } else {
                WorkingTreeMode::CleanAll
            },
            ignored_files: IgnoredFilesMode::Skip,
            ignored_parallelism: 1,
            creation_mode: CreationMode::Linked,
            cancellation_token: CancellationToken::new(),
            btrfs_delegate: None,
            worktree_id: "wt-skip".into(),
            nfs: Some(NfsWorktreeOpts {
                enabled: true,
                ping_timeout: std::time::Duration::from_millis(50),
                create_timeout: std::time::Duration::from_millis(50),
                query_timeout: std::time::Duration::from_millis(50),
                query_interval: std::time::Duration::from_millis(5),
                ..NfsWorktreeOpts::default()
            }),
        }
    }

    fn arm(
        &self,
        mount: SourceMount,
        status: serde_json::Value,
        declined: Option<&'static str>,
    ) -> FakeArm {
        FakeArm {
            mount,
            script: Script {
                status,
                declined,
                creates: Arc::clone(&self.creates),
                endpoint: self.endpoint.clone(),
            },
        }
    }

    fn creates(&self) -> usize {
        self.creates.load(Ordering::SeqCst)
    }
}

fn status(capabilities: &[&str], mounts: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "status": {
            "capabilities": capabilities,
            "mounts": mounts
        }
    })
}

fn old_daemon() -> serde_json::Value {
    status(&[], serde_json::json!([{"kind":"store"}]))
}

fn cap_not_forkable() -> serde_json::Value {
    status(
        &[CAP_FORK_FROM_BACKING],
        serde_json::json!([{"kind":"store","forkable":false}]),
    )
}

fn store_forkable() -> serde_json::Value {
    status(
        &[CAP_FORK_FROM_BACKING],
        serde_json::json!([{"kind":"store","forkable":true}]),
    )
}

fn worktree_forkable() -> serde_json::Value {
    status(
        &[CAP_FORK_FROM_BACKING],
        serde_json::json!([{"kind":"worktree","forkable":true}]),
    )
}

fn linked_forkable() -> serde_json::Value {
    status(
        &[CAP_FORK_FROM_BACKING],
        serde_json::json!([{
            "kind":"worktree",
            "source_mode":"local",
            "forkable":true
        }]),
    )
}

fn linked_old_daemon() -> serde_json::Value {
    status(
        &[],
        serde_json::json!([{"kind":"worktree","source_mode":"local"}]),
    )
}

fn skip(arm: &FakeArm, plan: &WorktreePlan) -> GroveSkip {
    match run_grove_arm(arm, plan).unwrap() {
        Some(crate::worktree::GroveTry::Skipped(s)) => s,
        other => panic!("expected skip, got {other:?}"),
    }
}

fn proceeded(arm: &FakeArm, plan: &WorktreePlan) {
    match run_grove_arm(arm, plan).unwrap() {
        Some(crate::worktree::GroveTry::Skipped(
            GroveSkip::DaemonDeclined | GroveSkip::HeadUnreadableAfterAdopt,
        )) => {}
        Some(crate::worktree::GroveTry::Adopted(_)) => {}
        other => panic!("expected CreateWorktree, got {other:?}"),
    }
}

#[test]
fn grove_without_cap_keeps_source_is_grove_mount() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, old_daemon(), None);
    assert_eq!(
        skip(&arm, &f.plan(true, "HEAD")),
        GroveSkip::SourceIsGroveMount
    );
    assert_eq!(f.creates(), 0);
}

#[test]
fn grove_without_cap_linked_preserve_keeps_preserve_on_linked_view() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, linked_old_daemon(), None);
    assert_eq!(
        skip(&arm, &f.plan(true, "HEAD")),
        GroveSkip::PreserveOnLinkedView
    );
    assert_eq!(f.creates(), 0);
}

#[test]
fn grove_with_cap_not_forkable_is_source_is_grove_mount() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, cap_not_forkable(), None);
    assert_eq!(
        skip(&arm, &f.plan(true, "HEAD")),
        GroveSkip::SourceIsGroveMount
    );
    assert_eq!(f.creates(), 0);
}

#[test]
fn grove_store_forkable_preserve_sends_create() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, store_forkable(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn grove_worktree_forkable_preserve_sends_create() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, worktree_forkable(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn grove_linked_forkable_preserve_no_longer_skips() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, linked_forkable(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn grove_forkable_preserve_non_head_still_skips() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, store_forkable(), None);
    assert_eq!(
        skip(&arm, &f.plan(true, "main")),
        GroveSkip::PreserveNonHeadRef
    );
    assert_eq!(f.creates(), 0);
}

#[test]
fn grove_forkable_clean_non_head_sends_create() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Grove, store_forkable(), None);
    proceeded(&arm, &f.plan(false, "main"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn inconclusive_forkable_sends_create() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Inconclusive, store_forkable(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn inconclusive_without_cap_still_skips() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Inconclusive, old_daemon(), None);
    assert_eq!(
        skip(&arm, &f.plan(true, "HEAD")),
        GroveSkip::MountTableInconclusive
    );
    assert_eq!(f.creates(), 0);
}

#[test]
fn plain_source_is_unchanged_first_capture() {
    let f = Fixture::new();
    let arm = f.arm(SourceMount::Plain, old_daemon(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn forkable_skips_jj_probe() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.tmp.path().join("repo/.jj")).unwrap();
    let arm = f.arm(SourceMount::Grove, store_forkable(), None);
    proceeded(&arm, &f.plan(true, "HEAD"));
    assert_eq!(f.creates(), 1);
}

#[test]
fn not_forkable_jj_still_skips() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.tmp.path().join("repo/.jj")).unwrap();
    let arm = f.arm(SourceMount::Plain, old_daemon(), None);
    assert_eq!(skip(&arm, &f.plan(true, "HEAD")), GroveSkip::JjSourceRepo);
    assert_eq!(f.creates(), 0);
}

#[test]
fn fork_parent_busy_is_daemon_declined() {
    let f = Fixture::new();
    let arm = f.arm(
        SourceMount::Grove,
        store_forkable(),
        Some("fork-parent-busy"),
    );
    assert_eq!(skip(&arm, &f.plan(true, "HEAD")), GroveSkip::DaemonDeclined);
    assert_eq!(f.creates(), 1);
}

#[test]
fn fork_not_forkable_is_daemon_declined() {
    let f = Fixture::new();
    let arm = f.arm(
        SourceMount::Grove,
        store_forkable(),
        Some("fork-not-forkable"),
    );
    assert_eq!(skip(&arm, &f.plan(true, "HEAD")), GroveSkip::DaemonDeclined);
    assert_eq!(f.creates(), 1);
}

#[test]
fn grove_source_skip_can_fork_is_none() {
    for mount in [
        SourceMount::Grove,
        SourceMount::Inconclusive,
        SourceMount::Plain,
    ] {
        assert_eq!(grove_source_skip(mount, false, true, true), None);
    }
}

#[test]
fn grove_source_skip_matrix_without_fork() {
    assert_eq!(
        grove_source_skip(SourceMount::Grove, false, false, false),
        Some(GroveSkip::SourceIsGroveMount)
    );
    assert_eq!(
        grove_source_skip(SourceMount::Grove, true, false, true),
        Some(GroveSkip::PreserveOnLinkedView)
    );
    assert_eq!(
        grove_source_skip(SourceMount::Grove, true, false, false),
        None
    );
    assert_eq!(
        grove_source_skip(SourceMount::Inconclusive, false, false, false),
        Some(GroveSkip::MountTableInconclusive)
    );
    assert_eq!(
        grove_source_skip(SourceMount::Inconclusive, true, false, true),
        Some(GroveSkip::PreserveOnInconclusiveLinkedView)
    );
    assert_eq!(
        grove_source_skip(SourceMount::Plain, false, false, true),
        None
    );
}
