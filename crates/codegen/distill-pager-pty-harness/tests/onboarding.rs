//! Opt-in real-PTY coverage for the four-step first-run onboarding flow.
//!
//! The controller must provide PAGER_BINARY. The tests intentionally do not
//! resolve a missing binary through Cargo, so an accidental pre-handback run
//! cannot start a second build lane.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use distill_pager_pty_harness::{ContentController, EnvOp, MockModel, PtyHarness, keys};

const ROWS: u16 = 50;
const COLS: u16 = 120;
const NARROW_ROWS: u16 = 24;
const NARROW_COLS: u16 = 80;
const SHORT_ROWS: u16 = 8;
const SHORT_COLS: u16 = 80;
const WAIT: Duration = Duration::from_secs(30);
// Match the existing leader_pty_e2e LEADER_TIMEOUT for a cold leader/client
// bring-up; all onboarding transitions retain the shorter WAIT deadline.
const LEADER_STARTUP_WAIT: Duration = Duration::from_secs(240);
const STEP4: &str = "Step 4 of 4";
const SHORT_GUARD: &str = "Onboarding needs a slightly larger terminal. Resize, then press Enter.";
const X_URL: &str = "https://x.com/samfajreldines/";
const LEFT: &[u8] = b"\x1b[D";
const END: &[u8] = b"\x1b[F";
const ARTIFACT_ENV: &str = "DISTILL_ONBOARDING_ARTIFACT_DIR";

fn required_binary() -> PathBuf {
    let binary = env::var_os("PAGER_BINARY")
        .map(PathBuf::from)
        .expect("PAGER_BINARY must point to the controller-supplied new distill binary");
    assert!(
        binary.is_file(),
        "PAGER_BINARY does not name a file: {}",
        binary.display()
    );
    binary
}

fn seed_onboarding_config(content: &ContentController, default_model: &str) {
    let config =
        format!("[ui]\nonboarding_completed = false\n\n[models]\ndefault = \"{default_model}\"\n");
    fs::write(content.sandbox().distill_home().join("config.toml"), config)
        .expect("seed incomplete onboarding config in the isolated sandbox");
}

fn seed_compatible_onboarding_config(content: &ContentController, default_model: &str) {
    let model_base_url = content.url();
    let config = format!(
        "[ui]\nonboarding_completed = false\n\n[models]\ndefault = \"{default_model}\"\n\n[model.default-model]\nmodel = \"default-model\"\nbase_url = \"{model_base_url}\"\napi_backend = \"chat_completions\"\ncontext_window = 131072\n\n[model.test-model]\nmodel = \"test-model\"\nbase_url = \"{model_base_url}\"\napi_backend = \"chat_completions\"\ncontext_window = 131072\n"
    );
    fs::write(content.sandbox().distill_home().join("config.toml"), config)
        .expect("seed compatible onboarding model catalog in the isolated sandbox");
}

fn config_value(content: &ContentController) -> toml::Value {
    let path = content.sandbox().distill_home().join("config.toml");
    let body = fs::read_to_string(&path).expect("read isolated onboarding config");
    toml::from_str(&body).expect("parse isolated onboarding config")
}

fn assert_completion(content: &ContentController) {
    let config = config_value(content);
    assert_eq!(
        config
            .get("ui")
            .and_then(|ui| ui.get("onboarding_completed"))
            .and_then(toml::Value::as_bool),
        Some(true),
        "onboarding completion must be persisted in the isolated config"
    );
}

fn assert_not_completed(content: &ContentController) {
    let config = config_value(content);
    assert_eq!(
        config
            .get("ui")
            .and_then(|ui| ui.get("onboarding_completed"))
            .and_then(toml::Value::as_bool),
        Some(false),
        "short-terminal guard must not persist onboarding completion"
    );
}

fn assert_model_saved(content: &ContentController) {
    let config = config_value(content);
    assert_eq!(
        config
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(toml::Value::as_str),
        Some("test-model"),
        "model selection must persist the selected catalog id"
    );
}

fn assert_reasoning_saved(content: &ContentController, expected: &str) {
    let config = config_value(content);
    assert_eq!(
        config
            .get("models")
            .and_then(|models| models.get("reasoning"))
            .and_then(toml::Value::as_str),
        Some(expected),
        "reasoning selection must persist the selected catalog id"
    );
}

fn spawn_onboarding(
    binary: &Path,
    content: &ContentController,
    rows: u16,
    cols: u16,
    socket_name: &str,
    env_ops: &[EnvOp<'_>],
) -> PtyHarness {
    let socket = content
        .sandbox()
        .distill_home()
        .join(socket_name)
        .to_str()
        .expect("isolated leader socket path is UTF-8")
        .to_owned();
    let cwd = content
        .sandbox()
        .workspace()
        .to_str()
        .expect("isolated PTY cwd is UTF-8")
        .to_owned();
    let args = [
        "--leader",
        "--leader-socket",
        socket.as_str(),
        "--cwd",
        cwd.as_str(),
    ];

    PtyHarness::spawn_with_content_env_ops_in_dir(
        binary,
        rows,
        cols,
        content,
        &args,
        env_ops,
        Some(content.sandbox().workspace()),
    )
    .expect("spawn onboarding pager in the isolated PTY sandbox")
}

fn press(harness: &mut PtyHarness, bytes: &[u8]) {
    harness.inject_keys(bytes).expect("inject onboarding key");
    harness.update(Duration::from_millis(150));
}

fn wait_for_step_with_timeout(harness: &mut PtyHarness, step: usize, timeout: Duration) {
    let marker = format!("Step {step} of 4");
    harness
        .wait_for_text(&marker, timeout)
        .unwrap_or_else(|error| panic!("{error}\nscreen:\n{}", harness.screen_contents()));
}

fn wait_for_step(harness: &mut PtyHarness, step: usize) {
    wait_for_step_with_timeout(harness, step, WAIT);
}

fn wait_for_initial_step(harness: &mut PtyHarness) {
    wait_for_step_with_timeout(harness, 1, LEADER_STARTUP_WAIT);
}

fn save_artifacts(content: &ContentController, harness: &PtyHarness, label: &str) {
    let Some(root) = env::var_os(ARTIFACT_ENV).map(PathBuf::from) else {
        return;
    };
    fs::create_dir_all(&root).expect("create optional onboarding artifact directory");
    fs::write(
        root.join(format!("{label}.screen.txt")),
        harness.screen_contents(),
    )
    .expect("write onboarding screen artifact");
    fs::write(root.join(format!("{label}.raw")), harness.raw_output())
        .expect("write onboarding raw PTY artifact");
    fs::write(root.join(format!("{label}.html")), harness.screen_html())
        .expect("write onboarding HTML screen artifact");
    harness
        .write_cast(&root.join(format!("{label}.cast")))
        .expect("write onboarding asciinema cast artifact");
    let config = content.sandbox().distill_home().join("config.toml");
    if config.is_file() {
        fs::copy(&config, root.join(format!("{label}.config.toml")))
            .expect("copy onboarding config artifact");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "E3 onboarding PTY; controller runs with PAGER_BINARY and --ignored"]
async fn onboarding_four_steps_persist_and_restart_normal_and_narrow() {
    let binary = required_binary();
    let models = vec![
        MockModel::new("default-model"),
        MockModel::new("test-model"),
    ];

    let content = ContentController::start_with_models(models.clone())
        .await
        .expect("start onboarding mock content");
    seed_compatible_onboarding_config(&content, "default-model");
    let opened_url = content.sandbox().temp_dir().join("opened-urls.txt");
    let opened_url_text = opened_url
        .to_str()
        .expect("opened URL path is UTF-8")
        .to_owned();
    let opener_env = [EnvOp::set(
        "GROK_TEST_OPEN_URL_FILE",
        opened_url_text.as_str(),
    )];

    let mut harness = spawn_onboarding(&binary, &content, ROWS, COLS, "normal.sock", &opener_env);
    wait_for_initial_step(&mut harness);
    save_artifacts(&content, &harness, "normal-step1");

    // Budget -> Connect, then prove back/forward navigation before continuing.
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 2);
    save_artifacts(&content, &harness, "normal-step2");
    press(&mut harness, LEFT);
    wait_for_step(&mut harness, 1);
    save_artifacts(&content, &harness, "normal-step1-back");
    press(&mut harness, keys::RIGHT);
    wait_for_step(&mut harness, 2);
    save_artifacts(&content, &harness, "normal-step2-forward");

    // Close from Connect and reopen through the existing slash command; the
    // resumed state must remain on Connect rather than restarting at Budget.
    press(&mut harness, keys::ESC);
    harness
        .wait_for_text_absent("Step 2 of 4", WAIT)
        .expect("onboarding closes back to Welcome");
    for byte in b"/onboarding" {
        press(&mut harness, std::slice::from_ref(byte));
    }
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 2);
    save_artifacts(&content, &harness, "normal-step2-reopened");

    // Select the second mock catalog model (four Down presses from provider
    // row 0), then wait for the real config acknowledgement.
    for _ in 0..4 {
        press(&mut harness, keys::DOWN);
    }
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text("Main model saved", WAIT)
        .expect("main model persistence acknowledgement");
    assert_model_saved(&content);
    save_artifacts(&content, &harness, "normal-step2-model-selected");

    // Connect -> Reasoning: select the other catalog model as the reasoning
    // model and wait for the persistence acknowledgement before Community.
    press(&mut harness, END);
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 3);
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text("Reasoning model saved", WAIT)
        .expect("reasoning model persistence acknowledgement");
    assert_reasoning_saved(&content, "default-model");
    save_artifacts(&content, &harness, "normal-step3");

    // Continue from the reasoning picker, record the safe URL opener seam, then
    // select Finish without following or sending anything.
    press(&mut harness, END);
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 4);
    save_artifacts(&content, &harness, "normal-step4");
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text("Browser opener requested", WAIT)
        .expect("community opener status");
    let opened = fs::read_to_string(&opened_url).expect("read existing opener hook output");
    assert!(
        opened.lines().any(|line| line == X_URL),
        "the existing opener hook must record the profile URL, got {opened:?}"
    );
    press(&mut harness, keys::DOWN);
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text_absent(STEP4, WAIT)
        .expect("Finish closes completed onboarding");
    assert_completion(&content);
    assert_reasoning_saved(&content, "default-model");
    save_artifacts(&content, &harness, "normal-complete");
    harness
        .quit()
        .expect("gracefully quit normal onboarding child");

    // Same isolated GROK_HOME, new leader socket: completion must suppress the
    // first-run overlay after restart.
    let mut restart = spawn_onboarding(&binary, &content, ROWS, COLS, "restart.sock", &[]);
    restart
        .wait_for_text("Quit", LEADER_STARTUP_WAIT)
        .expect("restart reaches Welcome");
    restart.update(Duration::from_secs(2));
    assert!(
        !restart.contains_text("Step 1 of 4"),
        "persisted completion must prevent automatic onboarding after restart\nscreen:\n{}",
        restart.screen_contents()
    );
    assert_reasoning_saved(&content, "default-model");
    save_artifacts(&content, &restart, "restart-completed");
    restart
        .quit()
        .expect("gracefully quit restarted onboarding child");

    // Narrow geometry is a separate fresh sandbox so it exercises the same
    // four-step flow without reusing the completed marker.
    let narrow = ContentController::start_with_models(models)
        .await
        .expect("start narrow onboarding mock content");
    seed_onboarding_config(&narrow, "default-model");
    let mut narrow_harness = spawn_onboarding(
        &binary,
        &narrow,
        NARROW_ROWS,
        NARROW_COLS,
        "narrow.sock",
        &[],
    );
    wait_for_initial_step(&mut narrow_harness);
    save_artifacts(&narrow, &narrow_harness, "narrow-step1");
    press(&mut narrow_harness, keys::ENTER);
    wait_for_step(&mut narrow_harness, 2);
    save_artifacts(&narrow, &narrow_harness, "narrow-step2");
    press(&mut narrow_harness, END);
    press(&mut narrow_harness, keys::ENTER);
    wait_for_step(&mut narrow_harness, 3);
    save_artifacts(&narrow, &narrow_harness, "narrow-step3");
    press(&mut narrow_harness, b"s");
    wait_for_step(&mut narrow_harness, 4);
    save_artifacts(&narrow, &narrow_harness, "narrow-step4");
    press(&mut narrow_harness, keys::DOWN);
    press(&mut narrow_harness, keys::ENTER);
    narrow_harness
        .wait_for_text_absent(STEP4, WAIT)
        .expect("narrow Finish closes completed onboarding");
    assert_completion(&narrow);
    save_artifacts(&narrow, &narrow_harness, "narrow-complete");
    narrow_harness
        .quit()
        .expect("gracefully quit narrow onboarding child");

    // The short terminal renders the resize guard instead of onboarding steps.
    // Resize the same child through the existing PTY API, then prove the flow
    // becomes usable without completing while the guard was visible.
    let short = ContentController::start_with_models(vec![MockModel::new("default-model")])
        .await
        .expect("start short-terminal onboarding mock content");
    seed_onboarding_config(&short, "default-model");
    let mut short_harness =
        spawn_onboarding(&binary, &short, SHORT_ROWS, SHORT_COLS, "short.sock", &[]);
    short_harness
        .wait_for_text(SHORT_GUARD, LEADER_STARTUP_WAIT)
        .expect("short PTY reaches onboarding resize guard");
    assert_not_completed(&short);
    save_artifacts(&short, &short_harness, "short-resize-guard");

    short_harness
        .resize(NARROW_ROWS, NARROW_COLS)
        .expect("resize short onboarding PTY to usable geometry");
    wait_for_step(&mut short_harness, 1);
    assert_not_completed(&short);
    save_artifacts(&short, &short_harness, "short-resized-step1");
    press(&mut short_harness, keys::ENTER);
    wait_for_step(&mut short_harness, 2);
    save_artifacts(&short, &short_harness, "short-resized-step2");
    press(&mut short_harness, keys::ESC);
    short_harness
        .wait_for_text_absent("Step 2 of 4", WAIT)
        .expect("short onboarding closes after resize recovery");
    assert_not_completed(&short);
    short_harness
        .quit()
        .expect("gracefully quit short onboarding child");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "E3 onboarding PTY; controller runs with PAGER_BINARY and --ignored"]
async fn onboarding_browser_write_failure_keeps_finish_available() {
    let binary = required_binary();
    let content = ContentController::start_with_models(vec![
        MockModel::new("default-model"),
        MockModel::new("test-model"),
    ])
    .await
    .expect("start onboarding failure mock content");
    seed_onboarding_config(&content, "default-model");

    let missing_parent = content.sandbox().temp_dir().join("missing-open-parent");
    let bad_url_file = missing_parent.join("opened-urls.txt");
    assert!(!missing_parent.exists(), "failure parent must start absent");
    let bad_url_file_text = bad_url_file
        .to_str()
        .expect("failure URL path is UTF-8")
        .to_owned();
    let opener_env = [EnvOp::set(
        "GROK_TEST_OPEN_URL_FILE",
        bad_url_file_text.as_str(),
    )];

    let mut harness = spawn_onboarding(
        &binary,
        &content,
        ROWS,
        COLS,
        "browser-fail.sock",
        &opener_env,
    );
    wait_for_initial_step(&mut harness);
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 2);
    press(&mut harness, END);
    press(&mut harness, keys::ENTER);
    wait_for_step(&mut harness, 3);
    press(&mut harness, b"s");
    wait_for_step(&mut harness, 4);

    assert!(
        harness.contains_text(X_URL),
        "Community must show the manual profile URL\nscreen:\n{}",
        harness.screen_contents()
    );
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text("Could not open a browser", WAIT)
        .expect("browser failure feedback remains visible");
    harness
        .wait_for_text("Finish", WAIT)
        .expect("Finish remains available after browser failure");
    assert!(
        !bad_url_file.exists(),
        "failed opener hook must not create a URL record through a missing parent"
    );
    save_artifacts(&content, &harness, "browser-failure-feedback");

    press(&mut harness, keys::DOWN);
    press(&mut harness, keys::ENTER);
    harness
        .wait_for_text_absent(STEP4, WAIT)
        .expect("Finish completes after browser failure");
    assert_completion(&content);
    save_artifacts(&content, &harness, "browser-failure-finish");
    harness
        .quit()
        .expect("gracefully quit browser-failure onboarding child");
}
