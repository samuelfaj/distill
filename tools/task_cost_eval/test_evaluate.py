#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path

try:
    from .evaluate import (
        _sha256_file,
        _sha256_tree,
        build_report,
        load_cohort,
        load_runs,
    )
except ImportError:  # Direct invocation: python3 tools/task_cost_eval/test_evaluate.py
    from evaluate import (
        _sha256_file,
        _sha256_tree,
        build_report,
        load_cohort,
        load_runs,
    )


class TaskCostEvaluationTest(unittest.TestCase):
    def make_cohort(self, root: Path, *, cases=("case-a",), repetitions=1):
        prompt_dir = root / "prompts"
        fixture_root = root / "fixtures"
        grader_dir = root / "graders"
        prompt_dir.mkdir()
        fixture_root.mkdir()
        grader_dir.mkdir()
        grader_script = grader_dir / "test_grader.py"
        grader_script.write_text("#!/usr/bin/env python3\nraise SystemExit(0)\n", encoding="utf-8")
        grader_sha256 = _sha256_file(grader_script)
        self._case_hashes = {}
        for case_id in cases:
            prompt = prompt_dir / f"{case_id}.txt"
            prompt.write_text(
                f"Implement the requested behavior for {case_id}.\n", encoding="utf-8"
            )
            fixture_dir = fixture_root / case_id
            fixture_dir.mkdir()
            (fixture_dir / "fixture.txt").write_text("fixture\n", encoding="utf-8")
            self._case_hashes[case_id] = {
                "prompt_sha256": _sha256_file(prompt),
                "fixture_sha256": _sha256_tree(fixture_dir),
                "grader_sha256": grader_sha256,
            }
        baseline_runtime = {
            "status": "pinned",
            "executable": "distill",
            "version": "test-version",
            "build_id": "baseline-build",
            "executable_sha256": "a" * 64,
            "client_version": "test-version",
            "provider": "test-provider",
            "model": "test-model",
            "endpoint": "test-endpoint",
            "effort": "medium",
            "config_sha256": "b" * 64,
        }
        candidate_runtime = {**baseline_runtime, "build_id": "candidate-build", "config_sha256": "c" * 64}
        cohort = {
            "schema_version": 1,
            "cohort_id": "test-cohort",
            "task_base_sha": "base-sha",
            "repetitions": repetitions,
            "required_run_metadata": [
                "executable",
                "version",
                "build_id",
                "executable_sha256",
                "client_version",
                "provider",
                "model",
                "endpoint",
                "effort",
                "config_sha256",
            ],
            "variants": [
                {
                    "id": "baseline",
                    "role": "baseline",
                    "runner": "distill",
                    "runtime_pin": baseline_runtime,
                },
                {
                    "id": "candidate",
                    "role": "candidate",
                    "runner": "distill",
                    "runtime_pin": candidate_runtime,
                },
            ],
            "comparisons": [
                {
                    "baseline": "baseline",
                    "candidate": "candidate",
                    "require_same_task_revision": True,
                    "match_runtime_fields": ["provider", "model", "endpoint", "effort"],
                }
            ],
            "cases": [
                {
                    "id": case_id,
                    "class": "test",
                    "language": "en",
                    "prompt_ref": f"prompts/{case_id}.txt",
                    "fixture_ref": f"fixtures/{case_id}",
                    "prompt_sha256": self._case_hashes[case_id]["prompt_sha256"],
                    "fixture_sha256": self._case_hashes[case_id]["fixture_sha256"],
                    "grader": {
                        "id": "test-grader",
                        "script_ref": "graders/test_grader.py",
                        "sha256": self._case_hashes[case_id]["grader_sha256"],
                        "command": ["python3", "graders/test_grader.py", "{worktree}"],
                    },
                    "isolation": {
                        "fresh_checkout": True,
                        "isolated_session": True,
                        "network_policy": "off",
                    },
                }
                for case_id in cases
            ],
        }
        path = root / "cohort.json"
        path.write_text(json.dumps(cohort), encoding="utf-8")
        loaded = load_cohort(path)
        self._cohort_digest = loaded["_cohort_sha256"]
        return loaded

    def write_session(
        self,
        root: Path,
        name: str,
        *,
        cost="present",
        cost_ticks=None,
        partial=False,
        incomplete=False,
        stop="end_turn",
        cost_basis=None,
        cost_is_estimate=False,
    ):
        session = root / name
        session.mkdir()
        usage = {
            "sessionId": name,
            "session": {
                "inputTokens": 10,
                "outputTokens": 3,
                "cachedReadTokens": 2,
                "cacheCreationTokens": 1,
                "reasoningTokens": 4,
                "totalTokens": 13,
                "modelCalls": 1,
                "costIsPartial": partial,
                "usageIsIncomplete": incomplete,
            },
            "turns": [{"turnNumber": 1}],
        }
        if cost_ticks is not None:
            usage["session"]["costUsdTicks"] = cost_ticks
        elif cost == "present":
            usage["session"]["costUsdTicks"] = 100
        elif cost == "other":
            usage["session"]["costUsdTicks"] = 250
        elif cost != "missing":
            raise AssertionError(cost)
        if cost_basis is not None:
            usage["session"]["costBasis"] = cost_basis
        if cost_is_estimate:
            usage["session"]["costIsEstimate"] = True
        (session / "usage.json").write_text(json.dumps(usage), encoding="utf-8")
        summary = {
            "info": {"id": name, "cwd": "/isolated/worktree"},
            "currentModelId": "test-model",
            "reasoningEffort": "medium",
            "headCommit": "base-sha",
            "headBranch": "benchmark",
        }
        (session / "summary.json").write_text(json.dumps(summary), encoding="utf-8")
        update = {
            "timestamp": 1,
            "method": "_x.ai/session/update",
            "params": {
                "sessionId": name,
                "update": {"sessionUpdate": "turn_completed", "stopReason": stop},
            },
        }
        (session / "updates.jsonl").write_text(json.dumps(update) + "\n", encoding="utf-8")
        return session

    def record(
        self,
        case_id,
        variant,
        session,
        *,
        repetition=1,
        status="completed",
        accepted=True,
        revision="base-sha",
        runtime_overrides=None,
        task_overrides=None,
    ):
        runtime = {
            "executable": "distill",
            "version": "test-version",
            "build_id": "baseline-build" if variant == "baseline" else "candidate-build",
            "executable_sha256": "a" * 64,
            "client_version": "test-version",
            "provider": "test-provider",
            "model": "test-model",
            "endpoint": "test-endpoint",
            "effort": "medium",
            "config_sha256": "b" * 64 if variant == "baseline" else "c" * 64,
        }
        runtime.update(runtime_overrides or {})
        task = {
            "revision": revision,
            **self._case_hashes[case_id],
            "cohort_sha256": self._cohort_digest,
        }
        task.update(task_overrides or {})
        return {
            "run_id": f"{variant}-{case_id}-{repetition}",
            "case_id": case_id,
            "variant": variant,
            "repetition": repetition,
            "execution_status": status,
            "accepted": accepted,
            "grader_id": "test-grader",
            "grader_exit_code": (
                None
                if accepted is None
                else (0 if accepted is True and status == "completed" else 1)
            ),
            "session_dir": str(session),
            "task": task,
            "runtime": runtime,
        }

    def test_current_session_formats_are_read_without_response_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline = self.write_session(root, "baseline-session", cost="present")
            candidate = self.write_session(root, "candidate-session", cost="other")
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline),
                    self.record("case-a", "candidate", candidate),
                ],
                manifest_dir=root,
            )
            run = report["runs"][0]
            self.assertEqual(run["usage"]["cost_usd_ticks"], 100)
            self.assertEqual(run["terminal"]["turn_completed_count"], 1)
            self.assertEqual(run["terminal"]["stop_reasons"], ["end_turn"])
            self.assertNotIn("agent_result", run)

    def test_failed_attempt_cost_stays_in_all_attempt_totals(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            failed = self.write_session(root, "failed-session", cost="other", stop="error")
            accepted = self.write_session(root, "accepted-session", cost="present")
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", failed, status="agent_failed", accepted=False),
                    self.record("case-a", "candidate", accepted),
                ],
                manifest_dir=root,
            )
            self.assertEqual(report["variants"]["baseline"]["agent_failed"], 1)
            self.assertEqual(report["variants"]["baseline"]["cost_complete_ticks_all_runs"], 250)
            self.assertEqual(report["variants"]["baseline"]["cost_ticks_for_accepted_runs"], None)
            self.assertFalse(report["headline"]["savings_claim_allowed"])

    def test_complete_matched_pair_calculates_exact_savings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline = self.write_session(root, "baseline-session", cost="other")
            candidate = self.write_session(root, "candidate-session", cost="present")
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline),
                    self.record("case-a", "candidate", candidate),
                ],
                manifest_dir=root,
            )
            comparison = report["comparisons"][0]
            self.assertTrue(comparison["savings_claim_allowed"])
            self.assertEqual(comparison["baseline_total_attempt_ticks"], 250)
            self.assertEqual(comparison["candidate_total_attempt_ticks"], 100)
            self.assertEqual(comparison["secondary_matched_accepted_baseline_ticks"], 250)
            self.assertEqual(comparison["secondary_matched_accepted_candidate_ticks"], 100)
            self.assertEqual(comparison["savings_ticks"], 150)

    def test_cohort_digest_controls_pairing_without_dropping_costs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline_session = self.write_session(root, "baseline-session", cost="other")
            candidate_session = self.write_session(root, "candidate-session", cost="present")
            baseline = self.record("case-a", "baseline", baseline_session)

            valid_report = build_report(
                cohort,
                [baseline, self.record("case-a", "candidate", candidate_session)],
                manifest_dir=root,
            )
            valid_comparison = valid_report["comparisons"][0]
            self.assertEqual(valid_comparison["matched_accepted_pairs"], 1)
            self.assertTrue(valid_comparison["savings_claim_allowed"])

            for index, (digest, reason) in enumerate(
                (
                    (None, "cohort_digest_missing"),
                    ("f" * 64, "cohort_digest_mismatch"),
                ),
                1,
            ):
                candidate = self.record("case-a", "candidate", candidate_session)
                if digest is None:
                    candidate["task"].pop("cohort_sha256")
                else:
                    candidate["task"]["cohort_sha256"] = digest
                manifest = root / f"runs-{index}.jsonl"
                manifest.write_text(
                    "\n".join(json.dumps(record) for record in (baseline, candidate)) + "\n",
                    encoding="utf-8",
                )
                report = build_report(cohort, load_runs(manifest), manifest_dir=root)
                comparison = report["comparisons"][0]
                self.assertEqual(comparison["matched_accepted_pairs"], 0)
                self.assertFalse(comparison["savings_claim_allowed"])
                self.assertIn(reason, comparison["blocking_reasons"])
                self.assertEqual(
                    report["variants"]["candidate"]["cost_complete_ticks_all_runs"],
                    100,
                )
                self.assertEqual(report["variants"]["candidate"]["accepted"], 1)

    def test_cost_basis_is_preserved_and_mixed_bases_block_pooling(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root, repetitions=2)
            baseline_one = self.write_session(root, "baseline-one", cost_ticks=100)
            baseline_two = self.write_session(root, "baseline-two", cost_ticks=100)
            candidate_one = self.write_session(
                root,
                "candidate-one",
                cost_ticks=50,
                cost_basis="pi_usage_cost_estimate",
                cost_is_estimate=True,
            )
            candidate_two = self.write_session(
                root,
                "candidate-two",
                cost_ticks=50,
                cost_basis="pi_usage_cost_estimate",
                cost_is_estimate=True,
            )
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline_one, repetition=1),
                    self.record("case-a", "baseline", baseline_two, repetition=2),
                    self.record("case-a", "candidate", candidate_one, repetition=1),
                    self.record("case-a", "candidate", candidate_two, repetition=2),
                ],
                manifest_dir=root,
            )
            runs = {
                (run["variant"], run["repetition"]): run for run in report["runs"]
            }
            self.assertEqual(runs[("baseline", 1)]["usage"]["cost_kind"], "actual")
            self.assertEqual(
                runs[("candidate", 1)]["usage"]["cost_kind"], "estimated"
            )
            self.assertEqual(
                runs[("candidate", 1)]["usage"]["cost_basis"],
                "pi_usage_cost_estimate",
            )
            self.assertIn("mixed_cost_basis", report["comparisons"][0]["blocking_reasons"])
            self.assertIsNone(report["comparisons"][0]["baseline_total_attempt_ticks"])
            self.assertFalse(report["comparisons"][0]["savings_claim_allowed"])
            self.assertTrue(report["policy"]["estimated_cost_is_not_provider_statement"])

    def test_missing_or_partial_cost_blocks_headline_and_is_not_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root, repetitions=2)
            baseline_accepted = self.write_session(root, "baseline-accepted", cost="present")
            candidate_accepted = self.write_session(root, "candidate-accepted", cost="present")
            baseline_failed = self.write_session(root, "baseline-failed", cost="other", stop="error")
            candidate_failed = self.write_session(
                root, "candidate-failed", cost="missing", partial=True, stop="error"
            )
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline_accepted, repetition=1),
                    self.record("case-a", "candidate", candidate_accepted, repetition=1),
                    self.record(
                        "case-a",
                        "baseline",
                        baseline_failed,
                        repetition=2,
                        status="agent_failed",
                        accepted=False,
                    ),
                    self.record(
                        "case-a",
                        "candidate",
                        candidate_failed,
                        repetition=2,
                        status="agent_failed",
                        accepted=False,
                    ),
                ],
                manifest_dir=root,
            )
            self.assertEqual(report["variants"]["candidate"]["cost_missing_runs"], 1)
            self.assertEqual(report["variants"]["candidate"]["cost_partial_or_incomplete_runs"], 1)
            self.assertEqual(report["variants"]["candidate"]["cost_per_accepted_task_ticks"], None)
            self.assertIn("incomplete_cost", report["comparisons"][0]["blocking_reasons"])
            self.assertIsNone(report["comparisons"][0]["candidate_total_attempt_ticks"])
            self.assertFalse(report["comparisons"][0]["savings_claim_allowed"])

    def test_expensive_failed_attempt_cannot_create_savings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root, repetitions=2)
            baseline_accepted = self.write_session(root, "baseline-accepted", cost_ticks=100)
            baseline_failed = self.write_session(
                root, "baseline-failed", cost_ticks=10, stop="error"
            )
            candidate_accepted = self.write_session(root, "candidate-accepted", cost_ticks=50)
            candidate_failed = self.write_session(
                root, "candidate-failed", cost_ticks=1000, stop="error"
            )
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline_accepted, repetition=1),
                    self.record(
                        "case-a",
                        "baseline",
                        baseline_failed,
                        repetition=2,
                        status="agent_failed",
                        accepted=False,
                    ),
                    self.record("case-a", "candidate", candidate_accepted, repetition=1),
                    self.record(
                        "case-a",
                        "candidate",
                        candidate_failed,
                        repetition=2,
                        status="agent_failed",
                        accepted=False,
                    ),
                ],
                manifest_dir=root,
            )
            baseline = report["variants"]["baseline"]
            candidate = report["variants"]["candidate"]
            comparison = report["comparisons"][0]
            self.assertEqual(baseline["cost_complete_ticks_all_runs"], 110)
            self.assertEqual(candidate["cost_complete_ticks_all_runs"], 1050)
            self.assertEqual(baseline["cost_per_accepted_task_ticks"], 110)
            self.assertEqual(candidate["cost_per_accepted_task_ticks"], 1050)
            self.assertEqual(comparison["baseline_total_attempt_ticks"], 110)
            self.assertEqual(comparison["candidate_total_attempt_ticks"], 1050)
            self.assertEqual(comparison["baseline_cost_per_accepted_task_ticks"], 110)
            self.assertEqual(comparison["candidate_cost_per_accepted_task_ticks"], 1050)
            self.assertEqual(comparison["secondary_matched_accepted_baseline_ticks"], 100)
            self.assertEqual(comparison["secondary_matched_accepted_candidate_ticks"], 50)
            self.assertEqual(comparison["savings_ticks"], -940)
            self.assertIn("candidate_not_cheaper", comparison["blocking_reasons"])
            self.assertFalse(comparison["savings_claim_allowed"])

    def test_frozen_runtime_and_protocol_hashes_block_claims(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline = self.write_session(root, "baseline-session")
            candidate = self.write_session(root, "candidate-session")
            report = build_report(
                cohort,
                [
                    self.record(
                        "case-a",
                        "baseline",
                        baseline,
                        runtime_overrides={"config_sha256": "d" * 64},
                        task_overrides={"prompt_sha256": "e" * 64},
                    ),
                    self.record("case-a", "candidate", candidate),
                ],
                manifest_dir=root,
            )
            reasons = report["comparisons"][0]["blocking_reasons"]
            self.assertIn("runtime_metadata_mismatch", reasons)
            self.assertIn("protocol_hash_mismatch", reasons)
            self.assertFalse(report["headline"]["savings_claim_allowed"])

    def test_grader_exit_code_controls_acceptance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = {
                "revision": "base-sha",
                "prompt_sha256": "a" * 64,
                "fixture_sha256": "b" * 64,
                "grader_sha256": "c" * 64,
            }
            runtime = {
                "executable": "distill",
                "version": "test-version",
                "build_id": "test-build",
                "executable_sha256": "d" * 64,
                "client_version": "test-version",
                "provider": "test-provider",
                "model": "test-model",
                "endpoint": "test-endpoint",
                "effort": "medium",
                "config_sha256": "e" * 64,
            }
            failed_with_success = {
                "case_id": "case-a",
                "variant": "baseline",
                "repetition": 1,
                "execution_status": "agent_failed",
                "accepted": False,
                "grader_id": "test-grader",
                "grader_exit_code": 0,
                "task": task,
                "runtime": runtime,
            }
            path = root / "runs.jsonl"
            path.write_text(json.dumps(failed_with_success) + "\n", encoding="utf-8")
            with self.assertRaises(Exception) as context:
                load_runs(path)
            self.assertIn("cannot have grader_exit_code=0", str(context.exception))

    def test_distill_revision_must_match_frozen_base(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline = self.write_session(root, "baseline-session")
            candidate = self.write_session(root, "candidate-session")
            report = build_report(
                cohort,
                [
                    self.record("case-a", "baseline", baseline, revision="wrong-sha"),
                    self.record("case-a", "candidate", candidate),
                ],
                manifest_dir=root,
            )
            self.assertIn("task_revision_not_frozen", report["headline"]["blocking_reasons"])
            self.assertFalse(report["headline"]["savings_claim_allowed"])

    def test_missing_planned_cell_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cohort = self.make_cohort(root)
            baseline = self.write_session(root, "baseline-session")
            report = build_report(
                cohort,
                [self.record("case-a", "baseline", baseline)],
                manifest_dir=root,
            )
            self.assertEqual(report["coverage"]["planned_cells"], 2)
            self.assertEqual(report["coverage"]["recorded_cells"], 1)
            self.assertEqual(report["coverage"]["missing_cells"], 1)
            self.assertIn("missing_execution", report["headline"]["blocking_reasons"])

    def test_load_runs_rejects_secret_bearing_manifest_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "runs.jsonl"
            path.write_text(json.dumps({"api_key": "should-not-be-here"}) + "\n", encoding="utf-8")
            with self.assertRaises(Exception) as context:
                load_runs(path)
            self.assertIn("secret-bearing key", str(context.exception))


if __name__ == "__main__":
    unittest.main()
