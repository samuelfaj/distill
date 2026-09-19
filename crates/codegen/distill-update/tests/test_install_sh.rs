// Modified for Distill by Samuel Fajreldines, 2026.
//! Blitz harness for the bash installer (`install.sh`), the second client that can brick a machine.
//! Runs the REAL shipped `install.sh` against a fake `curl` that can serve the good artifact, truncate it, or serve a right-length garbage body.
//! Asserts the same invariant as the Rust blitz:
//!
//! > After any install attempt, `$BIN_DIR/grok` resolves to a binary that runs, OR is still the previous-good binary, never a partial/garbage binary.
//!
//! Also covers shell-rc rewrite: stowed/symlinked `~/.bashrc` etc. must survive reinstall without being replaced by a plain file.
//!
//! The installer lives in the sibling `distill-pager` crate. Under Bazel it is a declared `data`
//! dependency resolved through runfiles; under `cargo nextest` it is resolved relative to this crate.
//! If it cannot be found the test skips rather than fail.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve a workspace-relative path: Bazel runfiles when present, else relative to this crate.
fn workspace_file(rel: &str) -> Option<PathBuf> {
    let candidate = if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
        let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into());
        PathBuf::from(srcdir).join(workspace).join(rel)
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(rel)
    };
    dunce::canonicalize(candidate).ok().filter(|p| p.exists())
}

fn script_path(name: &str) -> Option<PathBuf> {
    workspace_file(&format!("crates/codegen/distill-pager/scripts/{name}"))
}

fn install_sh_path() -> Option<PathBuf> {
    script_path("install.sh")
}

fn desktop_install_sh_path() -> Option<PathBuf> {
    workspace_file("frontend/apps/grok-desktop/scripts/install.sh")
}

fn host_platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "aarch64"
    };
    format!("{os}-{arch}")
}

const GOOD_SCRIPT: &str = "#!/bin/sh\nexit 0\n";
const INSTALLER_BLOCK_START: &str = "# >>> grok installer >>>";

/// Write a fake `curl` that intercepts every download `install.sh` performs.
/// `$FAKE_MODE` (full|truncate|garbage) selects the corruption.
fn write_fake_curl(dir: &Path) {
    let body = format!(
        r#"#!/bin/bash
mode="${{FAKE_MODE:-full}}"
fullsize={fullsize}
head=0; out=""; want_code=0; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --head) head=1 ;;
    -o) shift; out="$1" ;;
    -w) shift; [ "$1" = '%{{http_code}}' ] && want_code=1 ;;
    # Do not let --proto '=https' or -H value steal the URL argv.
    --proto=*) ;;
    --proto) shift ;;
    -H) shift ;;
    --connect-timeout|--retry|-m) shift ;;
    -*) : ;;
    *) url="$1" ;;
  esac
  shift
done
if [ -n "${{FAKE_URL_LOG:-}}" ] && [ -n "$url" ]; then echo "$url" >> "$FAKE_URL_LOG"; fi
if [ "$head" = 1 ]; then
  if [ "$want_code" = 1 ]; then printf '200'; else printf 'HTTP/1.1 200 OK\r\nContent-Length: %s\r\n\r\n' "$fullsize"; fi
  exit 0
fi
if [ -n "$out" ]; then
  case "$mode" in
    full)     printf '%s' '{good}' > "$out" ;;
    truncate) printf '\0\0\0\0' > "$out" ;;
    garbage)  head -c "$fullsize" /dev/zero | tr '\0' 'X' > "$out" ;;
  esac
  exit 0
fi
printf '0.1.181'
exit 0
"#,
        fullsize = GOOD_SCRIPT.len(),
        good = GOOD_SCRIPT,
    );
    let path = dir.join("curl");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Seed a valid previous-good binary and symlink in the isolated home.
fn seed_previous_good(home: &Path, platform: &str) -> PathBuf {
    let downloads = home.join(".grok").join("downloads");
    let bin = home.join(".grok").join("bin");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let prev = downloads.join(format!("grok-{platform}"));
    std::fs::write(&prev, GOOD_SCRIPT).unwrap();
    std::fs::set_permissions(&prev, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = bin.join("grok");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(format!("../downloads/grok-{platform}"), &link).unwrap();
    dunce::canonicalize(&prev).unwrap()
}

/// Re-resolve `$BIN_DIR/grok` from disk and re-run it: the active grok must always execute, and never be a `.tmp`/partial file.
fn assert_active_grok_runs(home: &Path) {
    let link = home.join(".grok").join("bin").join("grok");
    assert!(link.is_symlink(), "grok must remain a symlink");
    let resolved =
        dunce::canonicalize(&link).unwrap_or_else(|e| panic!("grok symlink dangles: {e}"));
    let name = resolved.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        !name.contains(".tmp"),
        "active grok must not be a temp file: {name}"
    );
    let ok = Command::new(&resolved)
        .arg("--version")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "active grok must run: {}", resolved.display());
}

fn run_installer(install_sh: &Path, home: &Path, fakebin: &Path, mode: &str, shell: &str) -> bool {
    let path_env = format!("{}:/usr/bin:/bin", fakebin.display());
    let status = Command::new("/bin/bash")
        .arg(install_sh)
        .arg("0.1.181")
        .env_clear()
        .env("HOME", home)
        .env("PATH", path_env)
        .env("SHELL", shell)
        .env("GROK_BIN_DIR", home.join(".grok").join("bin"))
        .env("GROK_CHANNEL", "stable")
        .env("FAKE_MODE", mode)
        .status()
        .expect("spawn bash install.sh");
    status.success()
}

fn installer_block_count(body: &str) -> usize {
    body.matches(INSTALLER_BLOCK_START).count()
}

fn assert_single_installer_block(path: &Path, preserved: Option<&str>) {
    let body = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("read {}: {e}", path.display());
    });
    let n = installer_block_count(&body);
    assert_eq!(
        n,
        1,
        "{} must contain exactly one grok installer block, got {n}:\n{body}",
        path.display()
    );
    if let Some(marker) = preserved {
        assert!(
            body.contains(marker),
            "{} must keep pre-existing content ({marker:?}):\n{body}",
            path.display()
        );
    }
}

#[derive(Clone, Copy)]
enum RcLayout {
    Missing,
    Plain,
    StowAbsolute,
    StowRelative,
    /// `$root/user/.bashrc` is a symlink to `../packages/bash/bashrc`; the relative target leaves `$HOME`.
    StowRelativeDotDot,
}

struct ShellRcCase {
    name: &'static str,
    script: &'static str,
    shell: &'static str,
    rc_name: &'static str,
    stow_name: &'static str,
    layout: RcLayout,
    reinstall: bool,
}

/// Returns `(installer_home, rc_path, stow_target, expected_link_value)`.
fn setup_rc(
    root: &Path,
    case: &ShellRcCase,
) -> (PathBuf, PathBuf, Option<PathBuf>, Option<PathBuf>) {
    let marker = "# user shell rc\n";
    match case.layout {
        RcLayout::Missing => {
            let home = root.to_path_buf();
            (home.clone(), home.join(case.rc_name), None, None)
        }
        RcLayout::Plain => {
            let home = root.to_path_buf();
            let rc_link = home.join(case.rc_name);
            std::fs::write(&rc_link, marker).unwrap();
            (home, rc_link, None, None)
        }
        RcLayout::StowAbsolute | RcLayout::StowRelative => {
            let home = root.to_path_buf();
            let stow_dir = home.join("dotfiles");
            std::fs::create_dir_all(&stow_dir).unwrap();
            let target = stow_dir.join(case.stow_name);
            std::fs::write(&target, marker).unwrap();
            let link_value = if matches!(case.layout, RcLayout::StowAbsolute) {
                target.clone()
            } else {
                PathBuf::from(format!("dotfiles/{}", case.stow_name))
            };
            let rc_link = home.join(case.rc_name);
            std::os::unix::fs::symlink(&link_value, &rc_link).unwrap();
            (home, rc_link, Some(target), Some(link_value))
        }
        RcLayout::StowRelativeDotDot => {
            // $HOME is root/user; the package is a sibling of user, so the relative link needs `..`
            let home = root.join("user");
            std::fs::create_dir_all(&home).unwrap();
            let target = root.join("packages/bash/bashrc");
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, marker).unwrap();
            let link_value = PathBuf::from("../packages/bash/bashrc");
            let rc_link = home.join(case.rc_name);
            std::os::unix::fs::symlink(&link_value, &rc_link).unwrap();
            (home, rc_link, Some(target), Some(link_value))
        }
    }
}

fn run_shell_rc_case(case: &ShellRcCase) {
    let Some(script) = script_path(case.script) else {
        eprintln!(
            "skipping {}: {} not found relative to crate",
            case.name, case.script
        );
        return;
    };
    let platform = host_platform();
    let fakedir = tempfile::tempdir().unwrap();
    write_fake_curl(fakedir.path());

    let root = tempfile::tempdir().unwrap();
    let (home_path, rc_path, stow_target, expected_link) = setup_rc(root.path(), case);
    seed_previous_good(&home_path, &platform);

    assert!(
        run_installer(&script, &home_path, fakedir.path(), "full", case.shell),
        "{}: first install should succeed",
        case.name
    );

    if case.reinstall {
        assert!(
            run_installer(&script, &home_path, fakedir.path(), "full", case.shell),
            "{}: reinstall should succeed",
            case.name
        );
    }

    match case.layout {
        RcLayout::Missing | RcLayout::Plain => {
            assert!(
                rc_path.is_file() && !rc_path.is_symlink(),
                "{}: {} must be a regular file",
                case.name,
                case.rc_name
            );
            let preserved = match case.layout {
                RcLayout::Plain => Some("# user shell rc"),
                _ => None,
            };
            assert_single_installer_block(&rc_path, preserved);
        }
        RcLayout::StowAbsolute | RcLayout::StowRelative | RcLayout::StowRelativeDotDot => {
            assert!(
                rc_path.is_symlink(),
                "{}: {} must remain a symlink after install",
                case.name,
                case.rc_name
            );
            let link = std::fs::read_link(&rc_path).unwrap();
            assert_eq!(
                link,
                *expected_link.as_ref().unwrap(),
                "{}: symlink target must be unchanged",
                case.name
            );
            let target = stow_target.as_ref().unwrap();
            assert_single_installer_block(target, Some("# user shell rc"));
        }
    }

    assert_active_grok_runs(&home_path);
}

#[test]
fn install_sh_blitz_keeps_grok_runnable_under_corruption() {
    let Some(install_sh) = install_sh_path() else {
        eprintln!("skipping: install.sh not found relative to crate; run under cargo");
        return;
    };
    let platform = host_platform();
    let fakedir = tempfile::tempdir().unwrap();
    write_fake_curl(fakedir.path());

    // Each entry: (mode, should the installer succeed?)
    // Loop a few rounds so a re-install over an existing good install is also exercised
    let cases = [
        ("full", true),
        ("truncate", false),
        ("garbage", false),
        ("full", true),
        ("truncate", false),
        ("garbage", false),
        ("full", true),
    ];

    for (mode, expect_ok) in cases {
        let home = tempfile::tempdir().unwrap();
        seed_previous_good(home.path(), &platform);

        let ok = run_installer(&install_sh, home.path(), fakedir.path(), mode, "/bin/bash");
        assert_eq!(
            ok, expect_ok,
            "install.sh mode={mode} exit success mismatch"
        );

        // The invariant holds on every path: the active grok always runs (new good binary on success, previous-good on rejection)
        assert_active_grok_runs(home.path());
    }
}

/// The two macOS/x86_64 host shapes the Rosetta probe distinguishes.
#[derive(Clone, Copy)]
enum FakeHost {
    /// Rosetta shell on Apple Silicon: `sysctl hw.optional.arm64` prints 1.
    AppleSiliconRosetta,
    /// Genuine Intel Mac: the sysctl key is missing.
    IntelMac,
}

fn write_fake_macos_x86_host(dir: &Path, host: FakeHost) {
    let sysctl = if matches!(host, FakeHost::AppleSiliconRosetta) {
        "#!/bin/bash\n[ \"$1\" = -n ] && [ \"$2\" = hw.optional.arm64 ] && { echo 1; exit 0; }\nexit 1\n"
    } else {
        "#!/bin/bash\nexit 1\n"
    };
    for (name, body) in [
        (
            "uname",
            "#!/bin/bash\ncase \"$1\" in -s) echo Darwin ;; -m) echo x86_64 ;; *) echo Darwin ;; esac\n",
        ),
        ("sysctl", sysctl),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Run an install script against a fake macOS/x86_64 host and return the artifact URLs it requested.
/// The enterprise script requires auth, provided via a dummy `GROK_DEPLOYMENT_KEY`.
fn install_urls_on_fake_host(script: &str, host: FakeHost) -> Option<String> {
    let script_file = script_path(script)?;
    let fakedir = tempfile::tempdir().unwrap();
    write_fake_curl(fakedir.path());
    write_fake_macos_x86_host(fakedir.path(), host);
    let url_log = fakedir.path().join("urls.log");

    let home = tempfile::tempdir().unwrap();
    let path_env = format!("{}:/usr/bin:/bin", fakedir.path().display());
    let status = Command::new("/bin/bash")
        .arg(&script_file)
        .arg("0.1.181")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", path_env)
        .env("SHELL", "/bin/bash")
        .env("GROK_BIN_DIR", home.path().join(".grok").join("bin"))
        .env("GROK_CHANNEL", "stable")
        .env("GROK_DEPLOYMENT_KEY", "test-deployment-key")
        .env("FAKE_MODE", "full")
        .env("FAKE_URL_LOG", &url_log)
        .status()
        .expect("spawn bash install script");
    assert!(status.success(), "{script} must succeed");
    Some(std::fs::read_to_string(&url_log).unwrap_or_default())
}

const INSTALL_SCRIPTS: [&str; 2] = ["install.sh", "install-enterprise.sh"];

const BAD_PROXY_URLS: &[&str] = &[
    "http://127.0.0.1:9/v1",
    "HTTP://evil.example/v1",
    "https://",
    "https:///v1",
    "https://user:pass@evil.example/v1",
    "https://user:pass@[::1]/v1",
    "ftp://evil.example/v1",
];

const GOOD_PROXY_URLS: &[&str] = &[
    "https://proxy.example.com/v1",
    "https://proxy.example.com:443/v1",
    "https://[::1]/v1",
];

fn run_with_proxy_url(script: &Path, proxy_url: &str) -> (bool, String, bool) {
    let fakedir = tempfile::tempdir().unwrap();
    write_fake_curl(fakedir.path());
    let url_log = fakedir.path().join("urls.log");
    let home = tempfile::tempdir().unwrap();
    let path_env = format!("{}:/usr/bin:/bin", fakedir.path().display());
    let status = Command::new("/bin/bash")
        .arg(script)
        .arg("0.1.181")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", &path_env)
        .env("SHELL", "/bin/bash")
        .env("GROK_BIN_DIR", home.path().join(".grok").join("bin"))
        .env("GROK_CHANNEL", "stable")
        .env("GROK_DEPLOYMENT_KEY", "test-deployment-key-must-not-leak")
        .env("GROK_PROXY_URL", proxy_url)
        .env("FAKE_MODE", "full")
        .env("FAKE_URL_LOG", &url_log)
        .status()
        .expect("spawn bash install script");
    let urls = std::fs::read_to_string(&url_log).unwrap_or_default();
    let managed = home.path().join(".grok/managed_config.toml").exists();
    (status.success(), urls, managed)
}

fn assert_no_credentialed_proxy_request(label: &str, proxy_url: &str, urls: &str) {
    assert!(
        !urls.contains("/deployment/config")
            && !urls.contains("/grok-cli/update")
            && !urls.contains("127.0.0.1")
            && !urls.contains("evil.example"),
        "{label}: must not issue credentialed proxy request for {proxy_url:?}, urls:\n{urls}"
    );
}

#[test]
fn install_scripts_refuse_bad_proxy_url_for_deployment_key() {
    let Some(pager_install) = script_path("install.sh") else {
        eprintln!("skipping: install.sh not found relative to crate; run under cargo");
        return;
    };
    let desktop = desktop_install_sh_path()
        .expect("desktop install.sh must resolve when pager install.sh is present");

    let mut scripts: Vec<(&str, PathBuf)> = vec![
        ("install.sh", pager_install),
        ("desktop install.sh", desktop),
    ];
    if let Some(enterprise) = script_path("install-enterprise.sh") {
        scripts.insert(1, ("install-enterprise.sh", enterprise));
    }

    for (label, script_file) in &scripts {
        for proxy_url in BAD_PROXY_URLS {
            let (ok, urls, managed) = run_with_proxy_url(script_file, proxy_url);
            assert!(
                !ok,
                "{label}: GROK_PROXY_URL={proxy_url:?} must fail closed"
            );
            assert_no_credentialed_proxy_request(label, proxy_url, &urls);
            assert!(
                !managed,
                "{label}: must not write managed_config.toml for {proxy_url:?}"
            );
        }
    }
}

#[test]
fn install_sh_rejects_hostile_grok_channel() {
    let Some(install_sh) = install_sh_path() else {
        eprintln!("skipping: install.sh not found relative to crate; run under cargo");
        return;
    };
    let fakedir = tempfile::tempdir().unwrap();
    write_fake_curl(fakedir.path());
    let url_log = fakedir.path().join("urls.log");
    let home = tempfile::tempdir().unwrap();
    let path_env = format!("{}:/usr/bin:/bin", fakedir.path().display());
    let hostile = "stable\"\n\n[[hooks.SessionStart]]\nhooks = [ { type = \"command\", command = 'true' } ]\nignored = \"";
    let output = Command::new("/bin/bash")
        .arg(&install_sh)
        .arg("0.1.181")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", path_env)
        .env("SHELL", "/bin/bash")
        .env("GROK_BIN_DIR", home.path().join(".grok").join("bin"))
        .env("GROK_CHANNEL", hostile)
        .env("FAKE_MODE", "full")
        .env("FAKE_URL_LOG", &url_log)
        .output()
        .expect("spawn bash install.sh");
    assert!(
        !output.status.success(),
        "unlisted GROK_CHANNEL must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("GROK_CHANNEL"),
        "must name GROK_CHANNEL in the error, stderr:\n{stderr}"
    );
    let config = home.path().join(".grok/config.toml");
    if config.exists() {
        let body = std::fs::read_to_string(&config).unwrap();
        assert!(
            !body.contains("hooks"),
            "must not write extra tables into config.toml:\n{body}"
        );
    }
    let urls = std::fs::read_to_string(&url_log).unwrap_or_default();
    assert!(
        urls.is_empty(),
        "must not probe a URL with an unlisted channel, urls:\n{urls}"
    );
}

#[test]
fn install_scripts_allow_custom_https_proxy_url() {
    let Some(pager_install) = script_path("install.sh") else {
        eprintln!("skipping: install.sh not found relative to crate; run under cargo");
        return;
    };
    let desktop = desktop_install_sh_path()
        .expect("desktop install.sh must resolve when pager install.sh is present");

    let mut scripts: Vec<(&str, PathBuf)> = vec![
        ("install.sh", pager_install),
        ("desktop install.sh", desktop),
    ];
    if let Some(enterprise) = script_path("install-enterprise.sh") {
        scripts.insert(1, ("install-enterprise.sh", enterprise));
    }
    for (label, script_file) in &scripts {
        for proxy_url in GOOD_PROXY_URLS {
            let (ok, urls, _managed) = run_with_proxy_url(script_file, proxy_url);
            assert!(
                ok,
                "{label}: custom https GROK_PROXY_URL={proxy_url:?} must succeed"
            );
            assert!(
                urls.contains("proxy.example.com") || urls.contains("[::1]"),
                "{label}: must request custom https proxy {proxy_url:?}, urls:\n{urls}"
            );
        }
    }
}

/// A Rosetta shell (uname says macos/x86_64, sysctl says Apple Silicon) must download the native arm64 artifact.
/// A genuine Intel Mac (sysctl key missing) must keep x86_64.
/// Both installer scripts carry the probe.
#[test]
fn install_scripts_rosetta_shell_installs_arm64() {
    for script in INSTALL_SCRIPTS {
        let Some(urls) = install_urls_on_fake_host(script, FakeHost::AppleSiliconRosetta) else {
            eprintln!("skipping: {script} not found relative to crate; run under cargo");
            return;
        };
        assert!(
            urls.contains("grok-0.1.181-macos-aarch64"),
            "{script}: Rosetta shell must request the arm64 artifact, urls:\n{urls}"
        );
        assert!(
            !urls.contains("macos-x86_64"),
            "{script}: Rosetta shell must not request the x86_64 artifact, urls:\n{urls}"
        );
    }
}

#[test]
fn install_scripts_intel_mac_keeps_x86_64() {
    for script in INSTALL_SCRIPTS {
        let Some(urls) = install_urls_on_fake_host(script, FakeHost::IntelMac) else {
            eprintln!("skipping: {script} not found relative to crate; run under cargo");
            return;
        };
        assert!(
            urls.contains("grok-0.1.181-macos-x86_64"),
            "{script}: Intel Mac must keep the x86_64 artifact, urls:\n{urls}"
        );
    }
}

#[test]
fn install_sh_shell_rc_rewrite_matrix() {
    let cases = [
        ShellRcCase {
            name: "stow absolute bashrc reinstall",
            script: "install.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::StowAbsolute,
            reinstall: true,
        },
        ShellRcCase {
            name: "stow relative bashrc reinstall",
            script: "install.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::StowRelative,
            reinstall: true,
        },
        ShellRcCase {
            name: "stow relative ../ bashrc reinstall",
            script: "install.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::StowRelativeDotDot,
            reinstall: true,
        },
        ShellRcCase {
            name: "plain bashrc reinstall",
            script: "install.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::Plain,
            reinstall: true,
        },
        ShellRcCase {
            name: "missing bashrc first install",
            script: "install.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::Missing,
            reinstall: false,
        },
        ShellRcCase {
            name: "enterprise stow absolute bashrc reinstall",
            script: "install-enterprise.sh",
            shell: "/bin/bash",
            rc_name: ".bashrc",
            stow_name: "bashrc",
            layout: RcLayout::StowAbsolute,
            reinstall: true,
        },
    ];

    for case in &cases {
        run_shell_rc_case(case);
    }
}
