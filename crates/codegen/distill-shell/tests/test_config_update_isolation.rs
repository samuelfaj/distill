// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression test: `update_config` must not leak values from `managed_config.toml` or `requirements.toml` into the user's `config.toml`.
//!
//! Bug: `update_config` used `load_effective_config()` (which merges all config layers) to populate the `Config` struct.
//! `save_config` then wrote that merged result back to the user's `config.toml`.
//! With `auto_update = false` in `requirements.toml`, any unrelated config write (even a theme change) would permanently poison the user's config.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use serial_test::serial;

/// Shared temp directory that lives for the entire test binary.
/// All tests share this as GROK_HOME (the `OnceLock` in distill-config only allows one value per process).
fn test_home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::TempDir::new().unwrap();
        // Keep so the directory survives the entire test process.
        let path = dir.keep();
        // SAFETY: called once at init before other threads touch this var.
        unsafe { std::env::set_var("GROK_HOME", &path) };
        path
    })
}

fn reset_config_files(home: &std::path::Path) {
    for name in ["config.toml", "requirements.toml", "managed_config.toml"] {
        let path = home.join(name);
        // `remove_file` cannot clear a directory squat left by
        // `update_config_refuses_unreadable_config_toml`; later `fs::write`
        // then fails with EISDIR.
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => {
                let _ = fs::remove_dir_all(&path);
            }
            Ok(_) => {
                let _ = fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
}

#[tokio::test]
#[serial]
async fn update_config_does_not_leak_requirements_into_user_config() {
    let home = test_home();
    reset_config_files(home);

    // --- Arrange ---

    fs::write(
        home.join("config.toml"),
        "[cli]\nauto_update = true\ninstaller = \"internal\"\n",
    )
    .unwrap();

    // Enterprise requirements.toml overrides auto_update to false
    fs::write(
        home.join("requirements.toml"),
        "[cli]\nauto_update = false\n",
    )
    .unwrap();

    // Sanity-check: effective config should show auto_update = false (requirements wins over user config)
    let effective = distill_shell::config::load_effective_config().unwrap();
    let effective_cfg = distill_shell::util::config::load_config_from_toml(&effective);
    assert_eq!(
        effective_cfg.cli.auto_update,
        Some(false),
        "precondition: effective config should merge requirements (auto_update=false)"
    );

    // --- Act ---
    // Simulate an unrelated config write (e.g. persisting a model preference).
    distill_shell::util::config::update_config(|cfg| {
        cfg.models.default = Some("grok-3".to_string());
    })
    .await
    .expect("update_config should succeed");

    // --- Assert ---
    // Read the user's config.toml back from disk (raw, no merge).
    let raw = fs::read_to_string(home.join("config.toml")).unwrap();
    let user_toml: toml::Value = toml::from_str(&raw).unwrap();
    let user_cfg = distill_shell::util::config::load_config_from_toml(&user_toml);

    assert_eq!(
        user_cfg.cli.auto_update,
        Some(true),
        "BUG REPRODUCED: auto_update in user config.toml was overwritten by \
         requirements.toml value. The raw file contents:\n{raw}"
    );

    // Also verify the unrelated write succeeded.
    assert_eq!(user_cfg.models.default.as_deref(), Some("grok-3"));
}

#[tokio::test]
#[serial]
async fn update_config_preserves_none_when_only_requirements_sets_value() {
    let home = test_home();
    reset_config_files(home);

    // User config has no auto_update field
    fs::write(
        home.join("config.toml"),
        "[cli]\ninstaller = \"internal\"\n",
    )
    .unwrap();

    fs::write(
        home.join("requirements.toml"),
        "[cli]\nauto_update = false\n",
    )
    .unwrap();

    // Write an unrelated field
    distill_shell::util::config::update_config(|cfg| {
        cfg.ui.yolo = true;
    })
    .await
    .expect("update_config should succeed");

    // Read back
    let raw = fs::read_to_string(home.join("config.toml")).unwrap();
    let user_toml: toml::Value = toml::from_str(&raw).unwrap();
    let user_cfg = distill_shell::util::config::load_config_from_toml(&user_toml);

    assert_eq!(
        user_cfg.cli.auto_update, None,
        "auto_update should remain absent in user config — requirements.toml \
         value must not leak. Raw file:\n{raw}"
    );
}

#[tokio::test]
#[serial]
async fn update_config_does_not_leak_managed_config_values() {
    let home = test_home();
    reset_config_files(home);

    // User config has no auto_update, only installer
    fs::write(
        home.join("config.toml"),
        "[cli]\ninstaller = \"internal\"\n",
    )
    .unwrap();

    fs::write(
        home.join("managed_config.toml"),
        "[cli]\nauto_update = false\nchannel = \"stable\"\n",
    )
    .unwrap();

    distill_shell::util::config::update_config(|cfg| {
        cfg.models.default = Some("test-model".to_string());
    })
    .await
    .expect("update_config should succeed");

    let raw = fs::read_to_string(home.join("config.toml")).unwrap();
    let user_toml: toml::Value = toml::from_str(&raw).unwrap();
    let user_cfg = distill_shell::util::config::load_config_from_toml(&user_toml);

    assert_eq!(
        user_cfg.cli.auto_update, None,
        "auto_update from managed_config.toml leaked into user config. Raw:\n{raw}"
    );
    assert_eq!(
        user_cfg.cli.channel, None,
        "channel from managed_config.toml leaked into user config. Raw:\n{raw}"
    );
}

/// Settings save must not replace a user `config.toml` symlink with a regular file.
#[tokio::test]
#[serial]
#[cfg(unix)]
async fn update_config_preserves_config_toml_symlink() {
    let home = test_home();
    reset_config_files(home);

    let repo = home.join("dotfiles");
    fs::create_dir_all(&repo).unwrap();
    let target = repo.join("config.toml");
    fs::write(&target, "[ui]\nsimple_mode = false\n").unwrap();
    let link = home.join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    distill_shell::util::config::update_config(|cfg| {
        cfg.ui.simple_mode = Some(true);
    })
    .await
    .expect("update_config should succeed");

    let meta = fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "config.toml must remain a symlink after save_config; got {:?}",
        meta.file_type()
    );
    assert_eq!(target, fs::read_link(&link).unwrap());
    let raw = fs::read_to_string(&target).unwrap();
    assert!(
        raw.contains("simple_mode = true"),
        "symlink target must receive the settings save:\n{raw}"
    );
}

/// A present-but-unreadable `config.toml` (directory squat) must not be treated
/// as empty and rewritten — same class as an `ELOOP` symlink chain the kernel
/// cannot `open`.
#[tokio::test]
#[serial]
#[cfg(unix)]
async fn update_config_refuses_unreadable_config_toml() {
    let home = test_home();
    reset_config_files(home);

    let squat = home.join("config.toml");
    fs::create_dir(&squat).unwrap();

    let err = distill_shell::util::config::update_config(|cfg| {
        cfg.ui.simple_mode = Some(true);
    })
    .await
    .expect_err("unreadable config.toml must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("unreadable") || msg.contains("Is a directory") || msg.contains("directory"),
        "expected unreadable refusal, got {msg}"
    );
    assert!(
        squat.is_dir(),
        "directory squat must not be replaced by a regular file"
    );
}

/// Bind dest before the typed load. A retarget inside `f` (after load, before
/// save) must refuse — not merge A's modeled fields onto B.
#[tokio::test]
#[serial]
#[cfg(unix)]
async fn update_config_refuses_retarget_between_load_and_save() {
    let home = test_home();
    reset_config_files(home);

    let a = home.join("a.toml");
    let b = home.join("b.toml");
    fs::write(&a, "[ui]\nsimple_mode = false\nfrom_a = true\n").unwrap();
    fs::write(&b, "[ui]\nsimple_mode = false\nfrom_b = true\n").unwrap();
    let link = home.join("config.toml");
    std::os::unix::fs::symlink(&a, &link).unwrap();

    let err = distill_shell::util::config::update_config(|cfg| {
        cfg.ui.simple_mode = Some(true);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
    })
    .await
    .expect_err("retarget between load and save must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("changed") || msg.contains("follow destination"),
        "expected retarget refusal, got {msg}"
    );

    let raw_a = fs::read_to_string(&a).unwrap();
    let raw_b = fs::read_to_string(&b).unwrap();
    assert!(
        raw_a.contains("from_a = true"),
        "referent A must stay unmerged:\n{raw_a}"
    );
    assert!(
        !raw_a.contains("simple_mode = true"),
        "must not publish onto A after retarget:\n{raw_a}"
    );
    assert!(
        raw_b.contains("from_b = true"),
        "referent B must stay unmerged:\n{raw_b}"
    );
    assert!(
        !raw_b.contains("simple_mode = true"),
        "must not merge A's snapshot onto B:\n{raw_b}"
    );
    assert_eq!(b, fs::read_link(&link).unwrap());
}
