// Modified for Distill by Samuel Fajreldines, 2026.
//! Product E2E: ACP `create_from_worktree_sync` through grove.
//!
//! Success cells either start the daemon out of band or rely on
//! `UnixArm::on_unreachable` to spawn it. `install_env` isolates
//! so an auto-started daemon cannot share the host grove data dir.
//! `run_agent_test` isolates `GROK_HOME`. Drop reaps whoever still holds
//! the unix socket (`lsof`/`fuser`); `ensure_daemon` argv has no sock path.

#![cfg(unix)]

#[allow(dead_code)]
mod acp_harness;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use acp_harness::{AutoApproveClient, connect_and_auth_with_remote, ext_method, run_agent_test};
use distill_shell::util::config::{ENV_WORKTREE_TYPE, RemoteSettings};
use serde_json::{Value, json};
use tempfile::TempDir;

const CREATE_SYNC: &str = "x.ai/git/worktree/create_from_worktree_sync";
const DAEMON_READY: Duration = Duration::from_secs(15);

fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1")
            | Some("true")
            | Some("TRUE")
            | Some("yes")
            | Some("YES")
            | Some("on")
            | Some("ON")
    )
}

fn fuse_usable() -> bool {
    if std::env::var_os("GROVE_DISABLE_FUSE").is_some() {
        return false;
    }
    let helper = ["fusermount3", "fusermount"].iter().any(|bin| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
    });
    helper
        && Path::new("/dev/fuse").exists()
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
            .is_ok()
}

fn require_fuse_or_skip() -> bool {
    if cfg!(not(target_os = "linux")) {
        if env_truthy("GROVE_REQUIRE_FUSE") {
            panic!("GROVE_REQUIRE_FUSE=1 requires Linux");
        }
        eprintln!("skip session grove FUSE cell: not Linux");
        return false;
    }
    if fuse_usable() {
        return true;
    }
    if env_truthy("GROVE_REQUIRE_FUSE") {
        panic!("/dev/fuse required (GROVE_REQUIRE_FUSE truthy) but not usable");
    }
    eprintln!(
        "skip session grove FUSE cell: /dev/fuse not usable (set GROVE_REQUIRE_FUSE=1 to fail hard)"
    );
    false
}

fn grove_bin() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("GROVE_BIN")?);
    p.is_file().then_some(p)
}

fn require_grove_bin() -> Option<PathBuf> {
    match grove_bin() {
        Some(p) => Some(p),
        None if env_truthy("GROVE_REQUIRE_FUSE") => {
            panic!("GROVE_BIN required (GROVE_REQUIRE_FUSE truthy) but grove is not on disk")
        }
        None => {
            eprintln!("skip session grove cell: grove binary missing (set GROVE_BIN)");
            None
        }
    }
}

struct EnvRestore {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvRestore {
    fn set(key: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: `run_agent_test` holds the process env lock for the body.
        unsafe {
            std::env::set_var(key, val);
        }
        Self { key, prev }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: inverse of [`EnvRestore::set`].
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn chmod_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("meta").permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms).expect("chmod 0700");
}

struct IsolatedGrove {
    _tmp: TempDir,
    bin: PathBuf,
    runtime_dir: PathBuf,
    sock: PathBuf,
    child: Option<Child>,
    dests: Vec<PathBuf>,
}

impl IsolatedGrove {
    fn dirs() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        chmod_private(tmp.path());
        let runtime_dir = tmp.path().join("run");
        std::fs::create_dir_all(&runtime_dir).expect("runtime");
        chmod_private(&runtime_dir);
        let sock = runtime_dir.join("grove").join("control.sock");
        std::fs::create_dir_all(sock.parent().expect("sock parent")).expect("grove runtime");
        chmod_private(sock.parent().expect("sock parent"));
        Self {
            _tmp: tmp,
            bin: require_grove_bin().expect("grove bin"),
            runtime_dir,
            sock,
            child: None,
            dests: Vec::new(),
        }
    }

    fn private_home_dirs(&self) -> (PathBuf, PathBuf, PathBuf) {
        let home = self._tmp.path().join("home");
        let config = self._tmp.path().join("config");
        let share = self._tmp.path().join("share");
        for p in [&home, &config, &share] {
            std::fs::create_dir_all(p).unwrap();
            chmod_private(p);
        }
        (home, config, share)
    }

    fn install_env(&self) -> Vec<EnvRestore> {
        let mut paths = vec![self.bin.parent().expect("grove bin dir").to_path_buf()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let path = std::env::join_paths(paths).expect("PATH");
        let (home, config, share) = self.private_home_dirs();
        vec![
            EnvRestore::set("GROVE_CONTROL_SOCK", &self.sock),
            EnvRestore::set("XDG_RUNTIME_DIR", &self.runtime_dir),
            EnvRestore::set(ENV_WORKTREE_TYPE, "grove"),
            EnvRestore::set("PATH", path),
            EnvRestore::set("HOME", home),
            EnvRestore::set("XDG_CONFIG_HOME", config),
            EnvRestore::set("XDG_DATA_HOME", share),
        ]
    }

    fn start_daemon(&mut self) {
        let (home, config, share) = self.private_home_dirs();
        let mut cmd = Command::new(&self.bin);
        cmd.env("GROVE_CONTROL_SOCK", &self.sock)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", config)
            .env("XDG_DATA_HOME", share)
            .args(["daemon", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[allow(clippy::disallowed_methods)] // Drop SIGKILLs this child
        let child = cmd.spawn().expect("spawn grove daemon");
        self.child = Some(child);
        self.wait_alive();
    }

    fn wait_alive(&mut self) {
        let start = Instant::now();
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait().expect("try_wait daemon")
            {
                panic!("grove daemon exited early ({status})");
            }
            let mut cmd = Command::new(&self.bin);
            cmd.env("GROVE_CONTROL_SOCK", &self.sock)
                .env("XDG_RUNTIME_DIR", &self.runtime_dir)
                .args(["status", "--json"])
                .stdin(Stdio::null());
            let out = cmd.output().expect("grove status");
            let alive = out.status.success()
                && serde_json::from_slice::<serde_json::Value>(&out.stdout)
                    .ok()
                    .and_then(|v| v.get("daemon_alive")?.as_bool())
                    == Some(true);
            if alive {
                return;
            }
            if start.elapsed() > DAEMON_READY {
                panic!(
                    "grove daemon did not become alive within {DAEMON_READY:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn note_mount(&mut self, dest: &Path) {
        self.dests.push(dest.to_path_buf());
    }

    fn unmount(&self, dest: &Path) {
        let dest_s = dest.display().to_string();
        let mut cmd = Command::new(&self.bin);
        cmd.env("GROVE_CONTROL_SOCK", &self.sock)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .args(["unmount", dest_s.as_str(), "--forget"])
            .stdin(Stdio::null());
        if cmd.output().map(|o| !o.status.success()).unwrap_or(true) {
            let _ = Command::new("fusermount3")
                .args(["-uz", dest_s.as_str()])
                .output();
        }
    }

    fn forget(&self, dest: &Path) -> std::process::Output {
        let dest_s = dest.display().to_string();
        let mut cmd = Command::new(&self.bin);
        cmd.env("GROVE_CONTROL_SOCK", &self.sock)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .args(["unmount", dest_s.as_str(), "--forget"])
            .stdin(Stdio::null());
        cmd.output().expect("grove unmount --forget")
    }
}

impl Drop for IsolatedGrove {
    fn drop(&mut self) {
        let dests = self.dests.clone();
        for dest in dests {
            self.unmount(&dest);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // cannot see it. Reap whoever still holds the unix socket.
        reap_unix_socket_holders(&self.sock);
    }
}

/// PIDs that have `sock` open (`lsof -t`, then `fuser`). Never kill ourselves.
fn reap_unix_socket_holders(sock: &Path) {
    let me = std::process::id();
    let mut pids = pids_from_cmd(&["lsof", "-t", "--"], sock);
    if pids.is_empty() {
        pids = pids_from_cmd(&["fuser", "--"], sock);
    }
    for pid in pids {
        if pid == me {
            continue;
        }
        let pid_s = pid.to_string();
        let _ = Command::new("kill").args(["-TERM", &pid_s]).status();
        std::thread::sleep(Duration::from_millis(50));
        let _ = Command::new("kill").args(["-KILL", &pid_s]).status();
    }
}

fn pids_from_cmd(argv: &[&str], sock: &Path) -> Vec<u32> {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]).arg(sock).stdin(Stdio::null());
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text.split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn init_repo(cwd: &Path) {
    distill_test_utils::git::init_git_repo(cwd);
    std::fs::write(cwd.join("tracked.txt"), "hello\n").expect("write tracked");
    distill_test_utils::git::git_commit_all(cwd, "initial");
    std::fs::write(cwd.join("dirty.txt"), "uncommitted\n").expect("write dirty");
}

fn create_params(cwd: &Path, label: &str) -> Value {
    json!({
        "sourceWorktreePath": cwd.to_string_lossy(),
        "newSessionId": label,
        "copyMode": "dirty",
        "label": label,
        "groveWorktree": true,
    })
}

fn strategy_of(resp: &Value) -> &Value {
    resp.get("result")
        .unwrap_or(resp)
        .get("strategy")
        .unwrap_or_else(|| panic!("response carries no strategy report: {resp}"))
}

fn worktree_path(resp: &Value) -> PathBuf {
    PathBuf::from(
        resp.get("result")
            .unwrap_or(resp)
            .get("worktreePath")
            .and_then(Value::as_str)
            .expect("worktreePath"),
    )
}

fn dest_is_live_mount(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !path.exists() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    std::fs::metadata(parent)
        .map(|p| p.dev() != meta.dev())
        .unwrap_or(false)
}

fn gate_remote() -> RemoteSettings {
    RemoteSettings {
        grove_worktree: Some(true),
        ..Default::default()
    }
}

#[test]
fn worktree_create_grove_success_reports_fuse() {
    if !require_fuse_or_skip() {
        return;
    }
    let Some(_) = require_grove_bin() else {
        return;
    };
    run_agent_test(|cwd, _mock| async move {
        let mut grove = IsolatedGrove::dirs();
        grove.start_daemon();
        let _env = grove.install_env();
        init_repo(&cwd);
        let (conn, _init) =
            connect_and_auth_with_remote(AutoApproveClient, "test", Some(gate_remote())).await;
        let resp = ext_method(&conn, CREATE_SYNC, create_params(&cwd, "grove-ok")).await;
        if let Some(dest) = resp
            .get("result")
            .unwrap_or(&resp)
            .get("worktreePath")
            .and_then(Value::as_str)
        {
            grove.note_mount(Path::new(dest));
        }
        let strategy = strategy_of(&resp);
        assert_eq!(strategy["requestedStrategy"], json!("grove"));
        assert_eq!(
            strategy["resolvedStrategy"],
            json!("grove-fuse"),
            "live daemon + open gate must resolve grove-fuse: {strategy}"
        );
        assert_eq!(
            strategy["transport"],
            json!("fuse"),
            "Linux must never label FUSE as NFS: {strategy}"
        );
        assert!(
            strategy.get("fallbackReason").is_none(),
            "grove success carries no fallback: {strategy}"
        );
        let dest = resp
            .get("result")
            .unwrap_or(&resp)
            .get("worktreePath")
            .and_then(Value::as_str)
            .expect("worktreePath");
        let dest = PathBuf::from(dest);
        assert!(
            dest_is_live_mount(&dest),
            "create dest must be a live mount"
        );
        grove.unmount(&dest);
    });
}

#[test]
fn worktree_create_daemon_down_starts_daemon_and_reports_fuse() {
    if !require_fuse_or_skip() {
        return;
    }
    if require_grove_bin().is_none() {
        return;
    }
    run_agent_test(|cwd, _mock| async move {
        let mut grove = IsolatedGrove::dirs();
        let _env = grove.install_env();
        init_repo(&cwd);
        let (conn, _init) =
            connect_and_auth_with_remote(AutoApproveClient, "test", Some(gate_remote())).await;
        let resp = ext_method(&conn, CREATE_SYNC, create_params(&cwd, "grove-down")).await;
        if let Some(dest) = resp
            .get("result")
            .unwrap_or(&resp)
            .get("worktreePath")
            .and_then(Value::as_str)
        {
            grove.note_mount(Path::new(dest));
        }
        let strategy = strategy_of(&resp);
        assert_eq!(strategy["requestedStrategy"], json!("grove"));
        assert_eq!(
            strategy["resolvedStrategy"],
            json!("grove-fuse"),
            "ping-fail must spawn the daemon and resolve grove-fuse: {strategy}"
        );
        assert!(
            strategy.get("fallbackReason").is_none(),
            "auto-started daemon is not a fallback: {strategy}"
        );
        let dest = resp
            .get("result")
            .unwrap_or(&resp)
            .get("worktreePath")
            .and_then(Value::as_str)
            .expect("worktreePath");
        let dest = PathBuf::from(dest);
        assert!(
            dest_is_live_mount(&dest),
            "create dest must be a live mount"
        );
        grove.unmount(&dest);
    });
}

/// Second attach: `create_from_worktree_sync` from a live Grove dest (grok -w
/// from a Grove cwd). Must stay grove-fuse, preserve dirty, isolate the child.
#[test]
fn worktree_create_from_grove_dest_forks_fuse() {
    if !require_fuse_or_skip() {
        return;
    }
    if require_grove_bin().is_none() {
        return;
    }
    run_agent_test(|cwd, _mock| async move {
        let mut grove = IsolatedGrove::dirs();
        grove.start_daemon();
        let _env = grove.install_env();
        init_repo(&cwd);
        let (conn, _init) =
            connect_and_auth_with_remote(AutoApproveClient, "test", Some(gate_remote())).await;
        let first = ext_method(&conn, CREATE_SYNC, create_params(&cwd, "grove-parent")).await;
        let dest1 = worktree_path(&first);
        grove.note_mount(&dest1);
        assert_eq!(
            strategy_of(&first)["resolvedStrategy"],
            json!("grove-fuse"),
            "first attach must be grove-fuse: {}",
            strategy_of(&first)
        );
        assert!(
            dest_is_live_mount(&dest1),
            "parent dest must be a live mount"
        );
        std::fs::write(dest1.join("from-parent.txt"), b"p").expect("dirty parent");

        let second = ext_method(&conn, CREATE_SYNC, create_params(&dest1, "grove-child")).await;
        let dest2 = worktree_path(&second);
        grove.note_mount(&dest2);
        let strategy = strategy_of(&second);
        assert_eq!(strategy["requestedStrategy"], json!("grove"));
        assert_eq!(
            strategy["resolvedStrategy"],
            json!("grove-fuse"),
            "second attach from a Grove dest must fork grove-fuse, not copy: {strategy}"
        );
        assert_eq!(
            strategy["transport"],
            json!("fuse"),
            "Linux must never label FUSE as NFS: {strategy}"
        );
        assert!(
            strategy.get("fallbackReason").is_none(),
            "Grove-parent fork must not fall back: {strategy}"
        );
        assert!(
            dest_is_live_mount(&dest2),
            "child dest must be a live mount"
        );
        assert_ne!(dest1, dest2);
        assert_eq!(
            std::fs::read_to_string(dest2.join("from-parent.txt")).expect("child dirty"),
            "p",
            "dirty preserve must copy the parent file into the child"
        );
        std::fs::write(dest2.join("only-child.txt"), b"c").expect("child write");
        assert!(
            !dest1.join("only-child.txt").exists(),
            "child write must not appear on the parent mount"
        );
        assert!(
            dest_is_live_mount(&dest1),
            "parent must stay mounted after the child forks"
        );

        let forget = grove.forget(&dest1);
        let mut err = String::from_utf8_lossy(&forget.stdout).into_owned();
        err.push_str(&String::from_utf8_lossy(&forget.stderr));
        assert!(
            !forget.status.success(),
            "forget parent with live child must fail: {err}"
        );
        assert!(
            err.contains("live fork children"),
            "forget must name live fork children: {err}"
        );

        grove.unmount(&dest2);
        grove.unmount(&dest1);
    });
}

/// Isolated subagent spawn from a live Grove dest must not share the parent.
#[test]
fn isolated_subagent_from_grove_dest_is_isolated() {
    if !require_fuse_or_skip() {
        return;
    }
    if require_grove_bin().is_none() {
        return;
    }
    run_agent_test(|cwd, mock| async move {
        mock.set_response("ordinary output");
        let mut grove = IsolatedGrove::dirs();
        grove.start_daemon();
        let _env = grove.install_env();
        init_repo(&cwd);
        let (conn, _init) =
            connect_and_auth_with_remote(AutoApproveClient, "test", Some(gate_remote())).await;
        let first = ext_method(&conn, CREATE_SYNC, create_params(&cwd, "grove-sa-parent")).await;
        let dest1 = worktree_path(&first);
        grove.note_mount(&dest1);
        assert_eq!(
            strategy_of(&first)["resolvedStrategy"],
            json!("grove-fuse"),
            "parent attach must be grove-fuse: {}",
            strategy_of(&first)
        );
        assert!(dest_is_live_mount(&dest1));
        std::fs::write(dest1.join("from-parent.txt"), b"p").expect("dirty parent");

        let spawned = distill_shell::agent::testkit::spawn_isolated_subagent_for_e2e(
            &dest1,
            RemoteSettings {
                grove_worktree: Some(true),
                ..Default::default()
            },
            &mock.url(),
        )
        .await;
        assert!(
            spawned.success,
            "isolated spawn must succeed: {:?}",
            spawned.error
        );
        let dest2 = spawned
            .worktree_path
            .expect("isolated spawn must create a worktree, not share the parent");
        grove.note_mount(&dest2);
        assert_ne!(dest1, dest2);
        assert!(
            dest_is_live_mount(&dest2),
            "child dest must be a live Grove mount"
        );
        assert_eq!(
            std::fs::read_to_string(dest2.join("from-parent.txt")).expect("child dirty"),
            "p"
        );
        std::fs::write(dest2.join("only-child.txt"), b"c").expect("child write");
        assert!(
            !dest1.join("only-child.txt").exists(),
            "child write must not appear on the parent"
        );
        assert!(dest_is_live_mount(&dest1));

        grove.unmount(&dest2);
        grove.unmount(&dest1);
    });
}
