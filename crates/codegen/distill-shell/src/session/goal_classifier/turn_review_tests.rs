// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::session::acp_session::turn_facts::{CheckContext, TurnFacts};
use crate::session::goal_classifier::evidence::CapturedChanges;
use crate::session::goal_classifier::{SpawnError, parse_verdict_path_from_prompt};
use crate::session::goal_planner::RoleRenderedPrompt;
use std::sync::Mutex;

// ── contract ──────────────────────────────────────────────────────────

/// The review is on by default, because UltraCode is on its v2 row unless
/// `[modes] v2` leaves it out. With UltraCode left out, only
/// `[modes.ultracode] contract = "v2"` turns the review back on.
#[test]
fn the_review_is_on_unless_modes_v2_leaves_ultracode_out() {
    let parse = |text: &str| toml::from_str::<toml::Value>(text).unwrap();
    assert!(contract_v2_in(None));
    assert!(contract_v2_in(Some(&parse(""))));
    assert!(contract_v2_in(Some(&parse("[cache]\nextended_ttl = \"auto\"\n"))));
    let off = "[modes]\nv2 = []\n";
    assert!(!contract_v2_in(Some(&parse(off))));
    assert!(!contract_v2_in(Some(&parse(&format!("{off}[modes.ultracode]\ncontract = \"v1\"\n")))));
    assert!(!contract_v2_in(Some(&parse(&format!("{off}[modes.ultrafast]\ncontract = \"v2\"\n")))));
    assert!(contract_v2_in(Some(&parse(&format!("{off}[modes.ultracode]\ncontract = \"v2\"\n")))));
    assert!(contract_v2_in(Some(&parse(&format!("{off}[modes.ultracode]\ncontract = \" v2 \"\n")))));
}

/// Listing `ultracode` in `[modes] v2` selects its target row, whose
/// turn-end verification is this review; other listed modes do not.
#[test]
fn listing_ultracode_in_modes_v2_turns_the_review_on() {
    let parse = |text: &str| toml::from_str::<toml::Value>(text).unwrap();
    assert!(contract_v2_in(Some(&parse("[modes]\nv2 = [\"ultracode\"]\n"))));
    assert!(contract_v2_in(Some(&parse("[modes]\nv2 = [\"ultrasave\", \"ultracode\"]\n"))));
    assert!(!contract_v2_in(Some(&parse("[modes]\nv2 = [\"ultrasave\", \"ultrafast\"]\n"))));
    assert!(!contract_v2_in(Some(&parse("[modes]\nv2 = []\n"))));
}

// ── prose ─────────────────────────────────────────────────────────────

/// A turn that changed only documentation is not reviewed; any code or
/// config file makes it a code turn.
#[test]
fn prose_is_documentation_and_plain_text_only() {
    for prose in ["README.md", "docs/guide.rs", "site/docs/a.json", "a.mdx", "NOTES.TXT", "x.rst"] {
        assert!(is_prose_path(prose), "{prose}");
    }
    for code in ["src/lib.rs", "Cargo.toml", "scripts/run.sh", "docs.rs", "src/docsy/a.rs", "md"] {
        assert!(!is_prose_path(code), "{code}");
    }
    assert!(!has_non_prose(&["README.md".into(), "docs/a.png".into()]));
    assert!(has_non_prose(&["README.md".into(), "src/a.rs".into()]));
    assert!(!has_non_prose(&[]));
}

// ── status rule ───────────────────────────────────────────────────────

fn bash(command: &str, exit_code: i32, output: &str) -> distill_tools::types::output::ToolOutput {
    distill_tools::types::output::ToolOutput::Bash(distill_tools::types::output::BashOutput {
        output: Vec::new(),
        output_for_prompt: output.to_owned(),
        exit_code,
        command: command.to_owned(),
        truncated: false,
        signal: None,
        timed_out: false,
        description: None,
        current_dir: "/repo".to_owned(),
        output_file: String::new(),
        total_bytes: 0,
        output_delta: None,
        was_bare_echo: false,
    })
}

fn at(tree: Option<&str>) -> CheckContext {
    CheckContext {
        cwd: "/repo".to_owned(),
        worktree: tree.map(str::to_owned),
    }
}

/// Records checks in order, a moment apart so their finish times differ.
fn facts(runs: &[(&str, i32, &str, Option<&str>)]) -> TurnFacts {
    let mut facts = TurnFacts::default();
    for (index, (command, exit, output, tree)) in runs.iter().enumerate() {
        std::thread::sleep(Duration::from_millis(2));
        facts.note_tool_result_at(&index.to_string(), &bash(command, *exit, output), &at(*tree));
    }
    facts
}

/// REVIEWED needs the latest check to be a pass on the current tree; a pass
/// that came after an earlier failure is enough, and no check at all does not
/// block.
#[test]
fn a_current_passing_latest_check_does_not_block() {
    assert_eq!(latest_check_blocker(&[], Some("t")), None);
    let passed = facts(&[("cargo test", 0, "test result: ok", Some("t"))]);
    assert_eq!(latest_check_blocker(passed.recorded_checks(), Some("t")), None);
    let fixed = facts(&[
        ("cargo test a", 101, "test result: FAILED", Some("t0")),
        ("cargo test", 0, "test result: ok", Some("t1")),
    ]);
    assert_eq!(latest_check_blocker(fixed.recorded_checks(), Some("t1")), None);
}

/// A failed, unknown or stale latest check blocks REVIEWED even when the
/// reviewer passed the patch; so does a pass whose tree marker is missing.
#[test]
fn a_failed_unknown_or_stale_latest_check_blocks() {
    let failed = facts(&[
        ("cargo test", 0, "test result: ok", Some("t")),
        ("cargo test b", 101, "test result: FAILED. 1 failed", Some("t")),
    ]);
    let reason = latest_check_blocker(failed.recorded_checks(), Some("t")).unwrap();
    assert!(reason.contains("`cargo test b` failed"), "{reason}");

    let unknown = facts(&[("cargo build 2>&1 | grep -E '^error'", 1, "", Some("t"))]);
    let reason = latest_check_blocker(unknown.recorded_checks(), Some("t")).unwrap();
    assert!(reason.contains("unknown outcome"), "{reason}");

    let stale = facts(&[("cargo test", 0, "test result: ok", Some("before-edit"))]);
    let reason = latest_check_blocker(stale.recorded_checks(), Some("after-edit")).unwrap();
    assert!(reason.contains("ran before the last change"), "{reason}");

    let no_marker = facts(&[("cargo test", 0, "test result: ok", None)]);
    assert!(latest_check_blocker(no_marker.recorded_checks(), Some("t")).is_some());
    assert!(latest_check_blocker(passed_on("t").recorded_checks(), None).is_some());
}

fn passed_on(tree: &str) -> TurnFacts {
    facts(&[("cargo test", 0, "test result: ok", Some(tree))])
}

// ── gate state machine ────────────────────────────────────────────────

fn state() -> TurnReviewState {
    TurnReviewState::begin(
        "p1",
        TurnSnapshot::Tree {
            tree: "t".into(),
            untracked: Default::default(),
        },
    )
}

fn refuted(fingerprint: &str) -> TurnReviewVerdict {
    TurnReviewVerdict::Refuted {
        gaps: format!("- gap {fingerprint}"),
        fingerprint: fingerprint.into(),
    }
}

fn finish(status: TurnStatus, gaps: Option<&str>) -> NextStep {
    NextStep::Finish {
        status,
        gaps: gaps.map(str::to_owned),
    }
}

#[test]
fn a_passing_review_is_reviewed_unless_a_check_blocks() {
    let mut s = state();
    assert_eq!(s.before_review(), None);
    assert_eq!(s.after_review(TurnReviewVerdict::NotRefuted, None), finish(TurnStatus::Reviewed, None));
    assert_eq!(s.status_for("p1"), Some(&TurnStatus::Reviewed));
    assert_eq!(s.status_for("p2"), None, "another prompt has no status");
    assert!(s.open_snapshot("p1").is_none(), "a finished review never runs again");

    let mut s = state();
    s.before_review();
    assert_eq!(
        s.after_review(TurnReviewVerdict::NotRefuted, Some("latest check `x` failed".into())),
        finish(TurnStatus::Unverified("latest check `x` failed".into()), None)
    );
}

/// A review that could not judge the patch (spawn failure, timeout, no
/// parsable verdict) never passes.
#[test]
fn an_unavailable_review_is_unverified() {
    let mut s = state();
    s.before_review();
    let step = s.after_review(
        TurnReviewVerdict::Unavailable {
            reason: "the review could not run: sampler_error".into(),
        },
        None,
    );
    assert_eq!(
        step,
        finish(TurnStatus::Unverified("the review could not run: sampler_error".into()), None)
    );
}

/// Fresh gaps each round get at most two rework rounds, then the turn ends
/// UNVERIFIED with the last gaps; no panel is spent on gaps that keep moving.
#[test]
fn rework_is_bounded_to_two_rounds() {
    let mut s = state();
    for (round, fingerprint) in ["a", "b"].iter().enumerate() {
        s.before_review();
        assert_eq!(
            s.after_review(refuted(fingerprint), None),
            NextStep::Rework {
                gaps: format!("- gap {fingerprint}")
            }
        );
        assert_eq!(s.rework_rounds(), round as u32 + 1);
    }
    s.before_review();
    assert_eq!(
        s.after_review(refuted("c"), None),
        finish(
            TurnStatus::Unverified("still refuted after 2 rework rounds".into()),
            Some("- gap c")
        )
    );
    assert!(!s.panel_used());
    assert_eq!(s.reviews(), 3);
}

/// The same gaps twice in a row go to one three-reviewer panel, whose verdict
/// is final: a refutation ends UNVERIFIED, a pass ends REVIEWED.
#[test]
fn a_repeated_refutation_gets_one_panel_and_the_panel_decides() {
    let mut s = state();
    s.before_review();
    assert!(matches!(s.after_review(refuted("a"), None), NextStep::Rework { .. }));
    s.before_review();
    assert_eq!(s.after_review(refuted("a"), None), NextStep::Panel);
    s.before_review();
    assert_eq!(
        s.after_panel(refuted("a"), None),
        finish(
            TurnStatus::Unverified("refuted again by a three-reviewer panel".into()),
            Some("- gap a")
        )
    );
    assert!(s.panel_used());

    let mut s = state();
    s.before_review();
    s.after_review(refuted("a"), None);
    s.before_review();
    assert_eq!(s.after_review(refuted("a"), None), NextStep::Panel);
    s.before_review();
    assert_eq!(s.after_panel(TurnReviewVerdict::NotRefuted, None), finish(TurnStatus::Reviewed, None));
}

/// After two rework rounds a repeated refutation still gets its one panel;
/// a refutation repeated after the panel was used cannot get another.
#[test]
fn the_panel_runs_at_most_once() {
    let mut s = state();
    s.before_review();
    s.after_review(refuted("a"), None);
    s.before_review();
    s.after_review(refuted("b"), None);
    s.before_review();
    assert_eq!(s.after_review(refuted("b"), None), NextStep::Panel);
    s.before_review();
    s.after_panel(refuted("b"), None);
    assert!(s.panel_used());
    // The turn has ended; a later review of the same prompt never opens.
    assert!(s.open_snapshot("p1").is_none());
}

/// Fifteen minutes of gate time end the turn UNVERIFIED with the gaps known
/// so far, before another review or rework round starts.
#[test]
fn the_gate_time_budget_ends_the_turn() {
    let mut s = state();
    s.before_review();
    s.after_review(refuted("a"), None);
    s.backdate_gate_for_tests(GATE_BUDGET + Duration::from_secs(1));
    assert_eq!(
        s.before_review(),
        Some(finish(
            TurnStatus::Unverified("review time budget used".into()),
            Some("- gap a")
        ))
    );
}

/// Gaps that need the user's decision end the turn instead of a rework.
#[test]
fn blocked_gaps_end_the_turn_unverified() {
    let mut s = state();
    s.before_review();
    let step = s.after_review(TurnReviewVerdict::Blocked { gaps: "- needs prod access".into() }, None);
    assert_eq!(
        step,
        finish(
            TurnStatus::Unverified("the review found gaps that need your decision".into()),
            Some("- needs prod access")
        )
    );
}

#[test]
fn the_status_is_the_last_line_of_the_appended_text() {
    let reviewed = final_text(&TurnStatus::Reviewed, None);
    assert_eq!(reviewed, "\n\nVerification: REVIEWED");
    let unverified = final_text(
        &TurnStatus::Unverified("refuted again by a three-reviewer panel".into()),
        Some("- src/a.rs:3 off by one\n"),
    );
    assert!(unverified.contains("- src/a.rs:3 off by one"));
    assert!(unverified.contains("/goal"));
    assert_eq!(
        unverified.lines().last(),
        Some("Verification: UNVERIFIED (refuted again by a three-reviewer panel)")
    );
    let no_gaps = final_text(&TurnStatus::Unverified("no repository".into()), None);
    assert!(!no_gaps.contains("/goal"));
}

// ── adapter over the verification stage ───────────────────────────────

/// A reviewer's terminal answer and the verdict JSON it writes, if any.
type ScriptedResponse = (Result<String, SpawnError>, Option<String>);

struct Scripted {
    responses: Mutex<std::collections::VecDeque<ScriptedResponse>>,
    prompts: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(responses: Vec<(Result<String, SpawnError>, Option<&str>)>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(
                responses
                    .into_iter()
                    .map(|(terminal, json)| (terminal, json.map(str::to_owned)))
                    .collect(),
            ),
            prompts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl GoalClassifierSpawner for Scripted {
    async fn spawn_classifier(
        &self,
        _id: &str,
        _skeptic_idx: u32,
        prompt: RoleRenderedPrompt,
        _details_path: &Path,
        _resume_from: Option<&str>,
    ) -> Result<String, SpawnError> {
        let (terminal, json) = self.responses.lock().unwrap().pop_front().expect("scripted response");
        if let (Some(path), Some(json)) = (parse_verdict_path_from_prompt(&prompt.primary), json) {
            tokio::fs::write(path, json).await.unwrap();
        }
        self.prompts.lock().unwrap().push(prompt.primary);
        terminal
    }
}

fn turn_patch() -> TurnPatch {
    TurnPatch {
        changes: CapturedChanges {
            diff: "diff --git a/src/a.rs b/src/a.rs\n+turn change\n".into(),
            changed_files: vec!["src/a.rs".into()],
        },
        may_include_earlier: false,
    }
}

async fn review(spawner: Arc<Scripted>) -> (TurnReviewVerdict, Vec<String>) {
    let workspace = tempfile::tempdir().unwrap();
    let verifier_id = format!("turn-{}", uuid::Uuid::new_v4().simple());
    let patch = turn_patch();
    let events = Mutex::new(Vec::new());
    let verdict = run_turn_review(
        spawner,
        TurnReviewInputs {
            request: "fix the off-by-one in a.rs",
            final_response: "fixed",
            patch: &patch,
            workspace_root: workspace.path(),
            verifier_id: &verifier_id,
            review: 1,
            model_id: "test",
            scratch_dir: workspace.path(),
            scratch_dir_ready: true,
            skeptic_count: 1,
            prior_gaps: None,
            tool_names: &[],
            inherit_tool_names: &RoleToolNames::inherit_defaults(),
            harness_checks: &[],
        },
        &|event| events.lock().unwrap().push(format!("{event:?}")),
    )
    .await;
    let _ = std::fs::remove_dir_all(crate::session::goal_tracker::goal_scratch_root(&verifier_id));
    (verdict, events.into_inner().unwrap())
}

/// The reviewer reads the turn lens and the caller's patch (not a HEAD diff),
/// and the run reports one turn-scoped event and no goal events.
#[tokio::test]
async fn a_passing_review_uses_the_turn_lens_and_the_turn_patch() {
    let spawner = Scripted::new(vec![(
        Ok("Not Refuted".into()),
        Some(r#"{"refuted":false,"evidence":"src/a.rs:1 correct","confidence":"high"}"#),
    )]);
    let (verdict, events) = review(spawner.clone()).await;
    assert_eq!(verdict, TurnReviewVerdict::NotRefuted);
    let prompts = spawner.prompts.lock().unwrap();
    let prompt = &prompts[0];
    assert!(prompt.contains("independent code reviewer"));
    assert!(!prompt.contains("adversarial verifier"), "not the goal verifier prompt");
    assert!(prompt.contains("OBJECTIVE:\nfix the off-by-one in a.rs"));
    assert!(prompt.contains("- src/a.rs"));
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].starts_with("UltracodeTurnReview"), "{events:?}");
    assert!(events[0].contains("not_refuted"));
}

#[tokio::test]
async fn a_refutation_carries_its_gaps_and_fingerprint() {
    let spawner = Scripted::new(vec![(
        Ok("Refuted".into()),
        Some(
            r#"{"refuted":true,"evidence":"src/a.rs:3 off by one","confidence":"high","findings":[{"kind":"bug","location":"src/a.rs:3","detail":"loop skips the last item"}]}"#,
        ),
    )]);
    let (verdict, _) = review(spawner).await;
    let TurnReviewVerdict::Refuted { gaps, fingerprint } = verdict else {
        panic!("{verdict:?}");
    };
    assert!(gaps.contains("loop skips the last item"), "{gaps}");
    assert_eq!(fingerprint, "src/a.rs:3");
}

/// The stage's fail-open outcomes never become a pass: a spawn failure, a
/// cancelled reviewer and a reviewer that left no verdict are all unavailable.
#[tokio::test]
async fn fail_open_outcomes_are_unavailable() {
    for response in [
        (Err(SpawnError::Transport("channel closed".into())), None),
        (
            Err(SpawnError::Runtime {
                message: "crashed".into(),
                cancelled: false,
            }),
            None,
        ),
        (Ok("I think it is fine".into()), None),
        (Ok("Refuted".into()), None),
    ] {
        let label = format!("{:?}", response.0);
        let (verdict, events) = review(Scripted::new(vec![response])).await;
        assert!(
            matches!(verdict, TurnReviewVerdict::Unavailable { .. }),
            "{label}: {verdict:?}"
        );
        assert!(events[0].contains("unavailable"), "{events:?}");
    }
}
