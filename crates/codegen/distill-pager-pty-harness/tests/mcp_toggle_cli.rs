// Modified for Distill by Samuel Fajreldines, 2026.
//! CLI-seam tests for `grok mcp enable`/`disable`/`add`/`list` against the real pager binary: no-op
//! toggles leave config.toml alone, policy refusals fire before any write, list reports verdicts.

use std::process::{Command, Stdio};

use distill_pager_pty_harness::pager_binary;

struct ToggleEnv {
    _temp: tempfile::TempDir,
    home: std::path::PathBuf,
    distill_home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    config: std::path::PathBuf,
    extra_env: Vec<(&'static str, &'static str)>,
}

fn toggle_env(user_config: &str) -> ToggleEnv {
    let temp = tempfile::tempdir().expect("tempdir");
    // Canonical so paths under $HOME compare equal to their canonicalized form (plugin auto-trust).
    let root = dunce::canonicalize(temp.path()).expect("canonicalize tempdir");
    let home = root.join("home");
    let distill_home = root.join("grok-home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&home).expect("create HOME");
    std::fs::create_dir_all(&distill_home).expect("create GROK_HOME");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    // Bound the project-config walk to the temp dir.
    git2::Repository::init(&cwd).expect("git init");
    let config = distill_home.join("config.toml");
    std::fs::write(&config, user_config).expect("write config.toml");
    ToggleEnv {
        home,
        distill_home,
        cwd,
        config,
        extra_env: Vec::new(),
        _temp: temp,
    }
}

const BLOCKED_URL: &str = "https://blocked.example.test/sse";

/// `toggle_env` plus a managed deny on [`BLOCKED_URL`].
fn blocked_env(user_config: &str) -> ToggleEnv {
    let env = toggle_env(user_config);
    std::fs::write(
        env.distill_home.join("managed_config.toml"),
        format!(r#"denied_mcp_servers = [{{ server_url = "{BLOCKED_URL}" }}]"#),
    )
    .expect("write managed_config.toml");
    env
}

fn run_mcp(env: &ToggleEnv, args: &[&str]) -> std::process::Output {
    let binary = pager_binary().expect("real pager binary is required when this test is selected");
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", &env.home)
        .env("GROK_HOME", &env.distill_home)
        .env("SHELL", "/bin/sh")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .envs(env.extra_env.iter().copied())
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(distill_tty_utils::pager_env());
    distill_tty_utils::detach_std_command(&mut command);
    command.arg("mcp").args(args);
    command.output().expect("run isolated pager binary")
}

/// Exit 1, the org-policy refusal on stderr naming the policy file by name only, no success
/// or write report on stdout, and a byte-identical `path`.
fn assert_refused_without_write(
    env: &ToggleEnv,
    output: &std::process::Output,
    path: &std::path::Path,
    before: &[u8],
) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("is blocked by an organization policy")
            && stderr.contains("(managed_config.toml)"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains(&*env.distill_home.to_string_lossy()),
        "the refusal must name the policy file, not its absolute path: {stderr}"
    );
    assert!(
        !stdout.contains("already") && !stdout.contains("File modified"),
        "refusal must precede the no-op report and any write: {stdout}"
    );
    assert_eq!(
        std::fs::read(path).ok().as_deref(),
        Some(before),
        "a refused command must not write {}",
        path.display()
    );
}

const ENABLED_SERVER: &str = r#"
[mcp_servers.svc]
url = "https://svc.example.test/sse"
"#;

const DISABLED_SERVER: &str = r#"
disabled_mcp_servers = ["svc"]

[mcp_servers.svc]
url = "https://svc.example.test/sse"
"#;

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn noop_enable_reports_already_enabled_without_config_write() {
    let env = toggle_env(ENABLED_SERVER);
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["enable", "svc"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("MCP server 'svc' is already enabled."),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("File modified"),
        "a no-op enable must not claim a write: {stdout}"
    );
    assert_eq!(
        std::fs::read(&env.config).expect("re-read config"),
        before,
        "a no-op enable must not rewrite config.toml"
    );
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn noop_disable_reports_already_disabled_without_config_write() {
    let env = toggle_env(DISABLED_SERVER);
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["disable", "svc"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("MCP server 'svc' is already disabled."),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("File modified"),
        "a no-op disable must not claim a write: {stdout}"
    );
    assert_eq!(
        std::fs::read(&env.config).expect("re-read config"),
        before,
        "a no-op disable must not rewrite config.toml"
    );
}

/// The refusal must win over both a real enable (disabled server) and the no-op path (already
/// enabled), so it fires before the no-op check and before any write.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn blocked_enable_refuses_without_config_write() {
    for user_config in [DISABLED_SERVER, ENABLED_SERVER] {
        let env = blocked_env(&user_config.replace("https://svc.example.test/sse", BLOCKED_URL));
        let before = std::fs::read(&env.config).expect("read config");

        let output = run_mcp(&env, &["enable", "svc"]);

        assert_refused_without_write(&env, &output, &env.config, &before);
    }
}

/// A setup-required server cannot be materialized, but its configured name and transport are
/// still judged: the gate must not read "no subject" as allowed.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn blocked_enable_refuses_setup_required_server() {
    let user_config = format!(
        r#"
disabled_mcp_servers = ["svc"]

[mcp_servers.svc]
url = "{BLOCKED_URL}"

[[mcp_servers.svc.setup.fields]]
id = "site"
label = "Site"
type = "select"
options = [{{ label = "US", value = "us1" }}]
"#
    );
    let env = blocked_env(&user_config);
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["enable", "svc"]);

    assert_refused_without_write(&env, &output, &env.config, &before);
    let list = run_mcp(&env, &["list", "--json"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("blocked_reason"),
        "list must annotate the setup-required blocked server: {stdout}"
    );
}

/// A denied server shipped by a trusted project's `[plugins].paths` plugin is known to the CLI,
/// so the verdict map must be built over the same registry or the gate passes and prints success.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn enable_refuses_denied_server_from_project_path_plugin() {
    let mut env = blocked_env(r#"disabled_mcp_servers = ["corp-tool"]"#);
    // Under $HOME so the config-path plugin is auto-trusted (folder trust is inert in dev builds).
    let project = env.home.join("project");
    let plugin = project.join("tools").join("plugin");
    std::fs::create_dir_all(project.join(".grok")).expect("create project .grok");
    std::fs::create_dir_all(&plugin).expect("create plugin dir");
    git2::Repository::init(&project).expect("git init");
    std::fs::write(plugin.join("plugin.json"), r#"{"name": "corp-plugin"}"#).expect("manifest");
    std::fs::write(
        plugin.join(".mcp.json"),
        format!(r#"{{"mcpServers": {{"corp-tool": {{"type": "http", "url": "{BLOCKED_URL}"}}}}}}"#),
    )
    .expect("plugin .mcp.json");
    std::fs::write(
        project.join(".grok").join("config.toml"),
        format!("[plugins]\npaths = [\"{}\"]\n", plugin.display()),
    )
    .expect("project config.toml");
    env.cwd = project;
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["enable", "corp-tool"]);

    assert_refused_without_write(&env, &output, &env.config, &before);
}

/// Release-stamped build, untrusted clone: folder trust hides the repo's project definitions, but
/// the enable gate still judges them for both disable encodings, sticky `enabled = false` included.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn enable_refuses_denied_project_server_in_untrusted_folder() {
    for (user_config, project_extra) in [
        (r#"disabled_mcp_servers = ["corp"]"#, ""),
        ("", "enabled = false\n"),
    ] {
        let mut env = blocked_env(user_config);
        env.extra_env.push(("GROK_TEST_VERSION", "1.0.0"));
        let project_config = env.cwd.join(".grok").join("config.toml");
        std::fs::create_dir_all(env.cwd.join(".grok")).expect("create project .grok");
        std::fs::write(
            &project_config,
            format!("[mcp_servers.corp]\nurl = \"{BLOCKED_URL}\"\n{project_extra}"),
        )
        .expect("project config.toml");
        let before = std::fs::read(&env.config).expect("read config");
        let project_before = std::fs::read(&project_config).expect("read project config");

        let output = run_mcp(&env, &["enable", "corp"]);

        assert_refused_without_write(&env, &output, &env.config, &before);
        assert_eq!(
            std::fs::read(&project_config).expect("re-read project config"),
            project_before,
            "a refused enable must not unstick the repo's config.toml"
        );

        let list = run_mcp(&env, &["list"]);
        let stdout = String::from_utf8_lossy(&list.stdout);
        assert!(
            stdout.contains("blocked by organization policy"),
            "list must carry the verdict doctor reports: {stdout}"
        );
    }
}

/// `grok mcp disable` writes the personal (user-tier) disable, so "already disabled" is judged
/// against that tier: a project-tier `enabled = false` must not swallow the write.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn disable_persists_personal_disable_over_project_tier_disable() {
    let env = toggle_env("");
    std::fs::create_dir_all(env.cwd.join(".grok")).expect("create project .grok");
    std::fs::write(
        env.cwd.join(".grok").join("config.toml"),
        "[mcp_servers.x]\nurl = \"https://x.example.test/sse\"\nenabled = false\n",
    )
    .expect("project config.toml");

    let output = run_mcp(&env, &["disable", "x"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Disabled MCP server 'x'.")
            && stdout.contains("File modified: $GROK_HOME/config.toml"),
        "stdout: {stdout}"
    );
    let parsed: toml::Value =
        toml::from_str(&std::fs::read_to_string(&env.config).expect("re-read config"))
            .expect("config.toml stays parseable");
    assert_eq!(
        parsed
            .get("disabled_mcp_servers")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
        Some(vec!["x"]),
        "the personal disable must be persisted: {parsed}"
    );

    // Now a user-tier no-op.
    let output = run_mcp(&env, &["disable", "x"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("MCP server 'x' is already disabled.") && !stdout.contains("File modified"),
        "stdout: {stdout}"
    );
}

/// `grok mcp list` co-reports the policy verdict with the personal disable, in the text note and
/// as the additive `blocked_reason` JSON key.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn list_reports_policy_block_in_text_and_json() {
    let env = blocked_env(&DISABLED_SERVER.replace("https://svc.example.test/sse", BLOCKED_URL));

    let output = run_mcp(&env, &["list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "svc: {BLOCKED_URL} (blocked by organization policy, disabled)"
        )),
        "stdout: {stdout}"
    );

    let output = run_mcp(&env, &["list", "--json"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("list --json emits JSON");
    let svc = &payload[0];
    assert_eq!(svc["name"], "svc");
    assert_eq!(svc["enabled"], false);
    assert_eq!(
        svc["blocked_reason"]
            .as_str()
            .map(|r| r.starts_with("matches deniedMcpServers (")),
        Some(true),
        "payload: {payload}"
    );
}

/// `grok mcp add` gates on policy before persisting: a URL deny refuses a user-scope add and the
/// project-MCP pin refuses a fresh `--scope project` add, with no config file written either way.
#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn add_refuses_denied_server_before_any_write() {
    let env = toggle_env("");
    std::fs::write(
        env.distill_home.join("managed_config.toml"),
        format!(
            "denied_mcp_servers = [{{ server_url = \"{BLOCKED_URL}\" }}]\nenable_all_project_mcp_servers = false\n"
        ),
    )
    .expect("write managed_config.toml");
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["add", "--transport", "http", "svc", BLOCKED_URL]);
    assert_refused_without_write(&env, &output, &env.config, &before);

    let project_config = env.cwd.join(".grok").join("config.toml");
    let output = run_mcp(
        &env,
        &[
            "add",
            "--scope",
            "project",
            "--transport",
            "http",
            "fresh",
            "https://fresh.example.test/mcp",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("is blocked by an organization policy"),
        "stderr: {stderr}"
    );
    assert!(
        !project_config.exists(),
        "a refused project-scope add must not create {}",
        project_config.display()
    );
    assert_eq!(std::fs::read(&env.config).expect("re-read config"), before);
}

#[test]
#[ignore = "spawns the real pager binary; CI/Bazel provides PAGER_BINARY"]
fn real_toggle_writes_config_and_reports_file_modified() {
    let env = toggle_env(DISABLED_SERVER);
    let before = std::fs::read(&env.config).expect("read config");

    let output = run_mcp(&env, &["enable", "svc"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Enabled MCP server 'svc'."),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("File modified: $GROK_HOME/config.toml"),
        "stdout: {stdout}"
    );
    let enabled_body = std::fs::read(&env.config).expect("re-read config");
    assert_ne!(enabled_body, before, "a real enable must write config.toml");
    let parsed: toml::Value = toml::from_str(std::str::from_utf8(&enabled_body).unwrap())
        .expect("config.toml stays parseable");
    assert!(
        parsed.get("disabled_mcp_servers").is_none(),
        "enable must clear the disabled list: {parsed}"
    );

    // Symmetric: disabling the now-enabled server writes and reports again.
    let output = run_mcp(&env, &["disable", "svc"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Disabled MCP server 'svc'."),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("File modified: $GROK_HOME/config.toml"),
        "stdout: {stdout}"
    );
    let disabled_body = std::fs::read(&env.config).expect("re-read config");
    let parsed: toml::Value = toml::from_str(std::str::from_utf8(&disabled_body).unwrap())
        .expect("config.toml stays parseable");
    assert_eq!(
        parsed
            .get("disabled_mcp_servers")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
        Some(vec!["svc"]),
        "disable must re-add the disabled list entry: {parsed}"
    );
}
