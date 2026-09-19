// Modified for Distill by Samuel Fajreldines, 2026.
//! Replicates a scenario an enterprise customer reported:
//! 1. Install a local plugin (full copy into `installed-plugins/`).
//! 2. Add a new agent only on the **live** source tree.
//! 3. Start a headless session: startup must re-copy trusted and user-home local installs.
//! 4. After exit, check the session JSON under `$GROK_HOME/sessions/` parses.
//!
//! Requires a built `grok` binary (`GROK_BINARY` or cargo-built pager) for the ignored headless test.
//!
//! ```bash
//! cargo test -p distill-shell --test test_trusted_local_plugin_refresh_e2e
//! cargo test -p distill-shell --test test_trusted_local_plugin_refresh_e2e -- --ignored
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use distill_agent::plugins::SharedPluginRegistryHandle;
use distill_agent::plugins::discovery::DiscoveryConfig;
use distill_agent::plugins::git_install::{InstallSource, install_from_source};
use distill_agent::plugins::install_registry::{
    InstallKind, InstallRegistry, InstalledRepo, RepoPlugin,
};
use distill_test_support::*;
use serial_test::serial;
use tempfile::TempDir;

fn write_minimal_plugin(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
}

fn write_agent(dir: &Path, file_stem: &str, name: &str, description: &str) {
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    let body = format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n");
    std::fs::write(dir.join("agents").join(format!("{file_stem}.md")), body).unwrap();
}

fn register_local_install(registry: &mut InstallRegistry, source: &Path) -> InstalledRepo {
    let installed = install_from_source(
        &InstallSource::Local {
            path: source.to_path_buf(),
            subdir: None,
        },
        registry,
        false,
    )
    .expect("install local plugin");
    let plugins = installed
        .plugins
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                RepoPlugin {
                    subdir: p.subdir.clone(),
                    version: p.version.clone(),
                },
            )
        })
        .collect();
    let now = chrono::Utc::now().to_rfc3339();
    let repo = InstalledRepo {
        kind: InstallKind::Local {
            source_path: source.to_path_buf(),
            subdir: None,
        },
        installed_at: now.clone(),
        updated_at: now,
        path: installed.repo_path.clone(),
        plugins,
        marketplace: None,
    };
    registry.insert(installed.repo_key.clone(), repo.clone());
    repo
}

/// Library-level e2e in a sandboxed tmp dir (always runs, no external binary). Proves the reported symptom is fixed: an agent added to the live source after install shows up in discovery without a reinstall.
/// The test drives the real session-spawn path, `refresh_and_build_for_cwd`, which refreshes before building. The `/agents` dashboard reads the same `all_subagents_with_plugins` list.
/// RAII: set an env var, restore the prior value (or unset) on drop, so a test never leaves process-global env pointing at a dropped tempdir. This is a local copy: each test holds two guards at once, and the canonical lock-holding guard deadlocks when nested.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[test]
#[serial]
fn trusted_local_refresh_surfaces_new_agent_via_discovery() {
    // Canonicalize so auto-trust for installs under the home dir holds when the temp root is a symlink (on macOS `/var` links to `/private/var`)
    let home_tmp = TempDir::new().unwrap();
    let home = dunce::canonicalize(home_tmp.path()).unwrap();
    let distill_home = home.join(".grok");
    let _home_guard = EnvVarGuard::set("HOME", &home);
    let _grok_guard = EnvVarGuard::set("GROK_HOME", &distill_home);

    // Live source: a user-home local plugin (mirrors a `~/.claude` local tree).
    let source = home
        .join(".claude")
        .join("local-marketplace")
        .join("demo-plugin");
    write_minimal_plugin(&source, "demo-plugin");
    write_agent(&source, "old", "old-agent", "exists at install");

    // The install copies a full snapshot into installed-plugins, not a live symlink
    let mut registry = InstallRegistry::empty(distill_home.join("installed-plugins"));
    let installed = register_local_install(&mut registry, &source);
    registry.save().expect("save registry");

    // New agent added to the live source only, not yet in the snapshot
    write_agent(&source, "new", "new-agent", "added after install");
    assert!(!installed.path.join("agents/new.md").exists());

    // Session spawn: refresh_and_build_for_cwd re-copies trusted local installs, then rediscovers
    // Enabling the plugin here mirrors the install command's auto-enable
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let handle = SharedPluginRegistryHandle::new(None, Vec::new());
    let config = DiscoveryConfig {
        cli_plugin_dirs: Vec::new(),
        config_paths: Vec::new(),
        disabled: Vec::new(),
        enabled: vec!["demo-plugin".to_string()],
    };
    let plugin_registry = handle
        .refresh_and_build_for_cwd(&cwd, &config, &[], true)
        .expect("registry built with installed plugin");

    assert!(
        installed.path.join("agents/new.md").exists(),
        "session-spawn refresh must re-copy the new agent into the snapshot"
    );

    // The new agent must show up in discovery (the reported symptom)
    let agents = distill_agent::discovery::all_subagents_with_plugins(
        &cwd,
        &HashMap::new(),
        Some(plugin_registry.as_ref()),
    );
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    assert!(
        names.contains(&"demo-plugin:new-agent"),
        "new agent must surface in /agents after session-start refresh; got {names:?}"
    );

    // Session `_meta.pluginDirs` load
    // This runs in the same test because distill_home() caches the first GROK_HOME per process
    // A separate test could seed the cache first and break the assertions above
    let plugin_dir = home.join("session-plugin");
    write_minimal_plugin(&plugin_dir, "session-plugin");
    write_agent(&plugin_dir, "helper", "helper-agent", "session-scoped");

    let session_handle = SharedPluginRegistryHandle::new(None, Vec::new());
    let session_config = DiscoveryConfig {
        cli_plugin_dirs: Vec::new(),
        config_paths: Vec::new(),
        disabled: Vec::new(),
        enabled: Vec::new(),
    };
    let session_dirs = vec![plugin_dir.clone()];
    let registry = session_handle
        .build_for_cwd(&cwd, &session_config, &session_dirs, true)
        .expect("registry built with session plugin dir");

    let plugin = registry
        .get("session-plugin")
        .expect("session plugin discovered");
    assert_eq!(
        plugin.scope,
        distill_agent::plugins::PluginScope::CliOverride
    );
    assert!(plugin.trusted && plugin.enabled);
    assert_eq!(registry.session_plugin_dirs(), session_dirs.as_slice());

    // Rebuilding without the dirs, as the shared code path does, must not carry them over
    let shared = session_handle.build_for_cwd(&cwd, &session_config, &[], true);
    assert!(shared.is_none_or(|r| r.session_plugin_dirs().is_empty()));
}

/// Runs the real binary end to end: session start refreshes the snapshot, then writes session JSON.
#[tokio::test]
#[ignore = "requires pre-built grok binary; run with --ignored"]
#[serial]
async fn headless_session_refreshes_trusted_local_plugin_and_writes_session_json() {
    let server = MockInferenceServer::start()
        .await
        .expect("start mock server");

    // Canonicalize so auto-trust for installs under the home dir holds when the temp root is a symlink (on macOS `/var` links to `/private/var`)
    let home_tmp = TempDir::new().unwrap();
    let home = dunce::canonicalize(home_tmp.path()).unwrap();
    let distill_home = home.join(".grok");
    std::fs::create_dir_all(&distill_home).unwrap();

    let source = home
        .join(".claude")
        .join("local-marketplace")
        .join("demo-plugin");
    write_minimal_plugin(&source, "demo-plugin");
    write_agent(&source, "old", "old-agent", "exists at install");

    // The spawned binary gets HOME and GROK_HOME via `cmd.env` below
    // This global env only serves the discovery assertion run in-process after the binary exits; it resolves the registry via distill_home()
    // `#[serial]` keeps it from racing other tests
    let _home_guard = EnvVarGuard::set("HOME", &home);
    let _grok_guard = EnvVarGuard::set("GROK_HOME", &distill_home);

    let mut registry = InstallRegistry::empty(distill_home.join("installed-plugins"));
    let installed = register_local_install(&mut registry, &source);
    registry.save().expect("save registry");

    write_agent(&source, "new", "new-agent", "added after install");
    assert!(!installed.path.join("agents/new.md").exists());

    let workdir = git_workdir();
    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "say hello",
        "--yolo",
        "--output-format",
        "json",
        "--cwd",
    ])
    .arg(workdir.workspace())
    .current_dir(workdir.workspace())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    let mut sandbox = TestSandbox::builder().mock_url(server.url()).build();
    sandbox
        .set_env("HOME", &home)
        .set_env("USERPROFILE", &home)
        .set_env("GROK_HOME", &distill_home);

    let result = run_headless_in_sandbox(cmd, sandbox).await;
    assert_headless_success(
        &result,
        "headless session with trusted local plugin refresh",
        Some(&server),
    );
    assert_no_crashes(&result.stderr);

    assert!(
        installed.path.join("agents/new.md").exists(),
        "session startup should have re-copied new agent into installed-plugins; stderr=\n{}",
        result.stderr
    );

    // The real binary's session start refreshed the on-disk snapshot
    // Rebuild the registry for the workdir and assert the new agent shows up in `/agents`, the same proof as the library test
    let config = DiscoveryConfig {
        cli_plugin_dirs: Vec::new(),
        config_paths: Vec::new(),
        disabled: Vec::new(),
        enabled: vec!["demo-plugin".to_string()],
    };
    let plugin_registry = SharedPluginRegistryHandle::new(None, Vec::new())
        .build_for_cwd(workdir.workspace(), &config, &[], true)
        .expect("registry built from refreshed snapshot");
    let agents = distill_agent::discovery::all_subagents_with_plugins(
        workdir.workspace(),
        &HashMap::new(),
        Some(plugin_registry.as_ref()),
    );
    assert!(
        agents.iter().any(|a| a.name == "demo-plugin:new-agent"),
        "new agent must surface in /agents after the binary's session-start refresh"
    );

    // Session storage under GROK_HOME/sessions must hold JSON files that parse
    let sessions_root = distill_home.join("sessions");
    assert!(
        sessions_root.is_dir(),
        "expected sessions dir at {}",
        sessions_root.display()
    );
    let mut json_files: Vec<PathBuf> = Vec::new();
    collect_json_files(&sessions_root, &mut json_files);
    assert!(
        !json_files.is_empty(),
        "expected session JSON under {}",
        sessions_root.display()
    );
    for path in &json_files {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if path.extension().is_some_and(|e| e == "jsonl") {
            for line in trimmed.lines().filter(|l| !l.trim().is_empty()) {
                serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
                    panic!("invalid JSONL line in {}: {e}\n{line}", path.display())
                });
            }
        } else {
            serde_json::from_str::<serde_json::Value>(trimmed)
                .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()));
        }
    }
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "json" || e == "jsonl")
        {
            out.push(path);
        }
    }
}
