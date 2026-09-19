// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

const YAML_PATH: &str = "falcon_missions_nrol97_trajectory_nrol97.mat_unbreakable";
const YAML_RESPONSE: &str = "```yaml\n\
nrol97:\n\
  trajectory: falcon_missions_nrol97_trajectory_nrol97.mat_unbreakable\n\
  mission_number: 1667\n\
  builds:\n\
    - cgen_swrelease\n\
```";

/// 40 cols so `YAML_PATH` soft-wraps.
const COPY_COLS: u16 = 40;
const COPY_ROWS: u16 = 50;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_native_copy_trims_pads() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL}\n{YAML_RESPONSE}"));

    let mut harness = spawn_minimal_sized(&content, COPY_ROWS, COPY_COLS);
    wait_minimal_ready(&mut harness);

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_full_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("yaml response rendered");
    harness
        .wait_for_turn_idle(Duration::from_secs(30))
        .expect("turn committed");

    let copy = harness.native_copy_text();
    let visual = harness.full_text();

    assert!(
        copy.contains(YAML_PATH),
        "native copy must join the soft-wrapped path\ncopy:\n{copy}\nvisual:\n{visual}"
    );
    assert!(
        !copy.contains("nrol97:\n\n"),
        "trailing pads must not become blank lines between YAML keys\ncopy:\n{copy}\nvisual:\n{visual}"
    );
    assert!(
        copy.contains("nrol97:"),
        "the mapping key stays a semantic line\ncopy:\n{copy}"
    );
    assert!(
        copy.contains("mission_number: 1667"),
        "short keys stay intact\ncopy:\n{copy}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    if let Ok(dir) = std::env::var("TEST_UNDECLARED_OUTPUTS_DIR") {
        let path = std::path::Path::new(&dir).join("gb-6189-native-copy.txt");
        let body = format!(
            "=== native_copy_text ===\n{copy}\n\n=== full_text (visual rows) ===\n{visual}\n"
        );
        let _ = std::fs::write(path, body);
    }

    quit_minimal(&mut harness);
}
