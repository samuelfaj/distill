// Modified for Distill by Samuel Fajreldines, 2026.
//! What a Jev question or threshold change is judged on before it ships, in
//! the order the change must pass: replay (recorded states asked again, see
//! `distill_workspace::jev::replay`), shadow (candidate questions riding live
//! requests), then a paired benchmark.
//!
//! Both switches are resolved once per process:
//! - `[jev] shadow_packs` (on by default, not benchmarked; `false` turns it
//!   off): shadow packs join requests a live pack already makes.
//! - `[jev] capture_states` (off by default): every Jev request's state and questions are
//!   appended to [`capture_dir`]. The state holds user content (the request,
//!   diffs, the final message), so the capture stays on this machine, in a
//!   directory beside the payload store and never inside a session folder:
//!   session folders are uploaded whole with a trace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use distill_workspace::jev::flags::JevLever;
use distill_workspace::jev::types::{Question, QuestionId};

/// Whether shadow packs may join live requests (`[jev] shadow_packs`).
pub fn packs_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_SWITCHES.with(|switches| switches.get().0) {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| packs_on_with(crate::jev::resolve_config_from_disk().shadow_packs))
}

/// `[jev] shadow_packs` resolved: on unless the key says `false`. A shadow
/// pack only joins a request a live pack already makes and nothing acts on
/// its answers, so it writes nothing and changes no decision.
const fn packs_on_with(key: Option<bool>) -> bool {
    match key {
        Some(on) => on,
        None => true,
    }
}

/// Whether Jev requests are captured for replay (`[jev] capture_states`).
pub fn capture_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_SWITCHES.with(|switches| switches.get().1) {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::jev::resolve_config_from_disk()
            .capture_states
            .unwrap_or(false)
    })
}

/// `<distill home>/jev/states`: beside the payload store, outside `sessions/`.
pub fn capture_dir() -> PathBuf {
    capture_dir_in(&crate::util::distill_home::distill_home())
}

fn capture_dir_in(home: &Path) -> PathBuf {
    home.join("jev").join("states")
}

/// Appends one request to today's capture file when capture is on. Failures
/// are logged and dropped: a capture never delays or changes a decision.
pub(crate) fn capture(
    lever: JevLever,
    model: &str,
    state: &serde_json::Value,
    questions: &BTreeMap<QuestionId, Question>,
) {
    capture_to(&capture_dir(), lever, model, state, questions);
}

fn capture_to(
    dir: &Path,
    lever: JevLever,
    model: &str,
    state: &serde_json::Value,
    questions: &BTreeMap<QuestionId, Question>,
) {
    if !capture_on() {
        return;
    }
    if let Err(error) = capture_in(dir, lever, model, state, questions) {
        tracing::debug!(%error, lever = lever.as_str(), "jev state capture failed");
    }
}

/// One replay-set line (see `distill_workspace::jev::replay::ReplayCase`)
/// appended to `<dir>/<date>-<pid>.jsonl`, readable by the owner only.
fn capture_in(
    dir: &Path,
    lever: JevLever,
    model: &str,
    state: &serde_json::Value,
    questions: &BTreeMap<QuestionId, Question>,
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let now = chrono::Utc::now();
    let path = dir.join(format!(
        "{}-{}.jsonl",
        now.format("%Y-%m-%d"),
        std::process::id()
    ));
    let line = serde_json::json!({
        "lever": lever.as_str(),
        "captured_at": now.to_rfc3339(),
        "model": model,
        "question_hash": distill_workspace::jev::catalog::thresholds::question_hash(questions),
        "state": state,
        "questions": questions,
    });
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    file.write_all(format!("{line}\n").as_bytes())?;
    Ok(path)
}

#[cfg(test)]
thread_local! {
    /// Per-thread overrides of (`shadow_packs`, `capture_states`), so tests
    /// never write `[jev]` config, which is cached per process.
    static TEST_SWITCHES: std::cell::Cell<(Option<bool>, Option<bool>)> =
        const { std::cell::Cell::new((None, None)) };
}

#[cfg(test)]
pub(crate) fn set_test_switches(shadow_packs: Option<bool>, capture_states: Option<bool>) {
    TEST_SWITCHES.with(|switches| switches.set((shadow_packs, capture_states)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shadow packs default on and `false` turns them off. Capture stays
    /// off by default: an unset `[jev]` never writes user content to disk.
    #[test]
    fn shadow_packs_default_on_and_capture_defaults_off() {
        let config = crate::agent::config::JevConfig::default();
        assert_eq!(config.shadow_packs, None);
        assert_eq!(config.capture_states, None);
        assert!(packs_on_with(config.shadow_packs), "unset: shadow packs on");
        assert!(!packs_on_with(Some(false)), "false is the off switch");
        assert!(packs_on_with(Some(true)));
        let parsed: crate::agent::config::JevConfig =
            toml::from_str("shadow_packs = true\ncapture_states = true").expect("keys parse");
        assert_eq!(
            (parsed.shadow_packs, parsed.capture_states),
            (Some(true), Some(true))
        );
        let dir = tempfile::tempdir().expect("dir");
        let states = capture_dir_in(dir.path());
        let state = serde_json::json!({"request": "synthetic"});
        set_test_switches(None, Some(false));
        capture_to(
            &states,
            JevLever::C4DiffRisk,
            "jev",
            &state,
            &BTreeMap::new(),
        );
        assert!(!states.exists(), "off: nothing touches the disk");
        set_test_switches(None, Some(true));
        capture_to(
            &states,
            JevLever::C4DiffRisk,
            "jev",
            &state,
            &BTreeMap::new(),
        );
        assert_eq!(std::fs::read_dir(&states).expect("on: written").count(), 1);
        set_test_switches(None, None);
    }

    /// Session folders are uploaded with a trace; the capture holds user
    /// content, so it must never land under one.
    #[test]
    fn the_capture_directory_is_outside_the_session_folders() {
        let home = Path::new("/home/someone/.distill");
        let dir = capture_dir_in(home);
        assert!(dir.starts_with(home));
        assert!(!dir.starts_with(home.join("sessions")));
        assert!(
            !capture_dir().starts_with(crate::util::distill_home::distill_home().join("sessions"))
        );
    }

    /// A captured line is a replay-set line, and the file is the owner's only.
    #[test]
    fn a_capture_is_a_replay_case_readable_by_the_owner_only() {
        let dir = tempfile::tempdir().expect("dir");
        let questions: BTreeMap<_, _> = [("q".to_owned(), Question::noul("Is it?"))]
            .into_iter()
            .collect();
        let state = serde_json::json!({"request": "synthetic"});
        let path = capture_in(
            dir.path(),
            JevLever::C4DiffRisk,
            "jev-x",
            &state,
            &questions,
        )
        .expect("written");
        capture_in(
            dir.path(),
            JevLever::C4DiffRisk,
            "jev-x",
            &state,
            &questions,
        )
        .expect("appended");
        let text = std::fs::read_to_string(&path).expect("read");
        let cases = distill_workspace::jev::replay::load_set(&text).expect("replay set");
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].lever, "c4_diff_risk");
        assert_eq!(cases[0].state, state);
        assert_eq!(cases[0].questions.as_ref(), Some(&questions));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o077, 0, "no group or other access");
        }
    }
}
