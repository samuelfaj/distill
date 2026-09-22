#!/usr/bin/env python3
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from . import runner
    from .evaluate import _sha256_file, _sha256_tree, load_cohort
except ImportError:  # Direct invocation: python3 tools/task_cost_eval/test_runner.py
    import runner
    from evaluate import _sha256_file, _sha256_tree, load_cohort


class TaskCostRunnerTest(unittest.TestCase):
    def test_fake_cli_stages_canonical_fixture_and_grades_original_fixture(self):
        cohort_path = Path(__file__).with_name("cohort-v1.json").resolve()
        fixture_source = cohort_path.parent / "fixtures/tax_bug"
        grader_source = cohort_path.parent / "graders/grade_tax_bug.py"
        fixture_digest = _sha256_tree(fixture_source)
        grader_digest = _sha256_file(grader_source)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_cli = root / "fake-distill"
            fake_cli.write_text(
                """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

def arg(name):
    return sys.argv[sys.argv.index(name) + 1]

worktree = Path(arg('--cwd'))
if (arg('--model') != 'fake-model' or arg('--reasoning-effort') != 'low'
        or arg('--max-turns') != '12' or '--always-approve' not in sys.argv):
    raise SystemExit(10)
expected_grok_home = Path(os.environ['EXPECTED_GROK_PARENT']).resolve() / worktree.parent.name
if Path(os.environ['GROK_HOME']) != expected_grok_home:
    raise SystemExit(11)
runtime_config = Path(os.environ['GROK_HOME']) / 'config.toml'
if runtime_config.read_text(encoding='utf-8') != os.environ['EXPECTED_CONFIG']:
    raise SystemExit(9)
if os.environ.get('FAKE_FIX') == '1':
    (worktree / 'tools/task_cost_eval/fixtures/tax_bug/tax.py').write_text(
        'def total_with_tax(amount_cents, rate_bps):\\n'
        '    return amount_cents + (amount_cents * rate_bps // 10000)\\n',
        encoding='utf-8',
    )

session = Path(os.environ['GROK_HOME']) / 'sessions' / arg('--session-id')
session.mkdir(parents=True)
(session / 'usage.json').write_text(json.dumps({
    'sessionId': arg('--session-id'),
    'session': {
        'inputTokens': 1, 'outputTokens': 1, 'cachedReadTokens': 0,
        'cacheCreationTokens': 0, 'reasoningTokens': 0, 'totalTokens': 2,
        'modelCalls': 1, 'costUsdTicks': 1,
    },
    'turns': [{'turnNumber': 1}],
}), encoding='utf-8')
(session / 'summary.json').write_text(json.dumps({
    'info': {'id': arg('--session-id')},
    'currentModelId': 'fake-model',
    'reasoningEffort': 'low',
}), encoding='utf-8')
(session / 'updates.jsonl').write_text(json.dumps({
    'params': {'update': {'sessionUpdate': 'turn_completed', 'stopReason': 'end_turn'}}
}) + '\\n', encoding='utf-8')
if os.environ.get('FAKE_BREAK_CONFIG') == '1':
    runtime_config.write_text('changed by fake agent', encoding='utf-8')
if os.environ.get('FAKE_BREAK_COHORT') == '1':
    cohort_file = Path(os.environ['COHORT_FILE'])
    cohort_file.write_text(cohort_file.read_text(encoding='utf-8') + '\\n', encoding='utf-8')
""",
                encoding="utf-8",
            )
            fake_cli.chmod(0o755)
            config_file = root / "config.json"
            config_file.write_text("{\"runtime\":\"fake\"}\n", encoding="utf-8")
            work_root = root / "runs"
            work_root.mkdir()
            grok_root = root / "grok-root"
            grok_root.mkdir()

            pin = {
                "status": "pinned",
                "executable": "distill",
                "version": "fake-version",
                "build_id": "fake-build",
                "executable_sha256": _sha256_file(fake_cli),
                "client_version": "fake-version",
                "provider": "local",
                "model": "fake-model",
                "endpoint": "local",
                "effort": "low",
                "max_turns": 12,
                "config_sha256": _sha256_file(config_file),
            }
            protocol_root = root / "protocol"
            (protocol_root / "tools").mkdir(parents=True)
            (protocol_root / "tools/task_cost_eval").symlink_to(
                cohort_path.parent, target_is_directory=True
            )
            for directory_name in ("fixtures", "prompts", "graders"):
                (protocol_root / directory_name).symlink_to(
                    cohort_path.parent / directory_name, target_is_directory=True
                )
            cohort_file = protocol_root / "cohort.json"
            cohort_data = json.loads(cohort_path.read_text(encoding="utf-8"))
            cohort_data["repetitions"] = 4
            for variant_data in cohort_data["variants"]:
                if variant_data["id"] == "distill-current":
                    variant_data["runtime_pin"] = pin
            tax_case = next(case for case in cohort_data["cases"] if case["id"] == "tax-bug-en")
            tax_case["grader"]["script_ref"] = "tools/task_cost_eval/graders/grade_tax_bug.py"
            tax_case["grader"]["command"][1] = "tools/task_cost_eval/graders/grade_tax_bug.py"
            cohort_file.write_text(json.dumps(cohort_data, indent=2) + "\n", encoding="utf-8")
            cohort = load_cohort(cohort_file)
            cohort_digest = _sha256_file(cohort_file)
            manifest = root / "runs.jsonl"
            common = [
                "run-one",
                "--cohort",
                str(cohort_file),
                "--case",
                "tax-bug-en",
                "--variant",
                "distill-current",
                "--repetition",
                "1",
                "--work-root",
                str(work_root),
                "--grok-home",
                str(grok_root),
                "--config-file",
                str(config_file),
                "--manifest",
                str(manifest),
                "--timeout-seconds",
                "5",
            ]

            def with_repetition(repetition):
                args = common.copy()
                args[args.index("--repetition") + 1] = str(repetition)
                return args

            with mock.patch.object(runner, "load_cohort", return_value=cohort), mock.patch.object(
                runner.shutil, "which", return_value=str(fake_cli)
            ), mock.patch.dict(
                os.environ,
                {
                    "EXPECTED_CONFIG": config_file.read_text(encoding="utf-8"),
                    "EXPECTED_GROK_PARENT": str(grok_root),
                    "COHORT_FILE": str(cohort_file),
                },
                clear=False,
            ):
                self.assertEqual(runner.main(common), 0)
                self.assertFalse((work_root / "tax-bug-en--distill-current--1").exists())
                with mock.patch.dict(os.environ, {"FAKE_FIX": "1"}, clear=False):
                    self.assertEqual(runner.main(common + ["--execute"]), 0)

                with mock.patch.dict(os.environ, {"FAKE_FIX": "0"}, clear=False):
                    self.assertEqual(runner.main(with_repetition(2) + ["--execute"]), 1)

                with mock.patch.dict(
                    os.environ, {"FAKE_FIX": "1", "FAKE_BREAK_CONFIG": "1"}, clear=False
                ):
                    self.assertEqual(runner.main(with_repetition(3) + ["--execute"]), 1)

                with mock.patch.dict(
                    os.environ, {"FAKE_FIX": "1", "FAKE_BREAK_COHORT": "1"}, clear=False
                ):
                    self.assertEqual(runner.main(with_repetition(4) + ["--execute"]), 1)

            first_cell = work_root / "tax-bug-en--distill-current--1"
            second_cell = work_root / "tax-bug-en--distill-current--2"
            self.assertEqual(
                (grok_root / first_cell.name / "config.toml").read_text(encoding="utf-8"),
                config_file.read_text(encoding="utf-8"),
            )
            first_fixture = first_cell / "worktree/tools/task_cost_eval/fixtures/tax_bug/tax.py"
            second_fixture = second_cell / "worktree/tools/task_cost_eval/fixtures/tax_bug/tax.py"
            self.assertNotEqual(_sha256_file(first_fixture), _sha256_file(fixture_source / "tax.py"))
            self.assertEqual(_sha256_file(second_fixture), _sha256_file(fixture_source / "tax.py"))
            self.assertEqual(_sha256_tree(fixture_source), fixture_digest)
            self.assertEqual(_sha256_file(grader_source), grader_digest)
            self.assertNotEqual(_sha256_file(cohort_file), cohort_digest)
            self.assertFalse(
                (first_cell / "worktree/tools/task_cost_eval/graders/grade_tax_bug.py").exists()
            )
            records = [json.loads(line) for line in manifest.read_text().splitlines()]
            self.assertEqual([record["grader_exit_code"] for record in records], [0, 1, None, None])
            self.assertEqual(records[2]["execution_status"], "inconclusive")
            self.assertIsNone(records[2]["accepted"])
            self.assertEqual(records[2]["failure_reason"], "post_dispatch_agent_verification_error")
            self.assertTrue((Path(records[2]["session_dir"]) / "usage.json").is_file())
            self.assertEqual(records[3]["execution_status"], "inconclusive")
            self.assertIsNone(records[3]["accepted"])
            self.assertEqual(records[3]["failure_reason"], "post_dispatch_agent_verification_error")
            self.assertTrue((Path(records[3]["session_dir"]) / "usage.json").is_file())
            for record in records:
                self.assertEqual(record["task"]["cohort_sha256"], cohort_digest)
                self.assertEqual(
                    _sha256_file(Path(record["task"]["cohort_snapshot"])), cohort_digest
                )
            self.assertEqual(records[0]["runtime"]["config_sha256"], pin["config_sha256"])
            self.assertEqual(
                records[0]["runtime"]["cli_overrides"],
                {
                    "model": "fake-model",
                    "reasoning_effort": "low",
                    "always_approve": "true",
                    "max_turns": "12",
                },
            )

    def test_all_fixture_cases_use_canonical_protocol_root(self):
        cohort_path = Path(__file__).with_name("cohort-v1.json").resolve()
        cohort = load_cohort(cohort_path)
        source = cohort_path.parent
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worktree = root / "worktree"
            worktree.mkdir()
            for case in cohort["cases"]:
                prompt = root / f"{case['id']}.txt"
                prompt.write_bytes((source / case["prompt_ref"]).read_bytes())
                fixture_target = runner._copy_protocol_files(cohort, case, worktree, prompt)
                expected_fixture = worktree / "tools/task_cost_eval" / case["fixture_ref"]
                expected_prompt = worktree / "tools/task_cost_eval" / case["prompt_ref"]
                self.assertEqual(fixture_target, expected_fixture)
                self.assertTrue(expected_fixture.is_dir())
                self.assertTrue(expected_prompt.is_file())

    def test_decimal_ticks_rounds_provider_float_noise_without_free_zero(self):
        self.assertEqual(runner._decimal_ticks(5.4399999999999994e-05), 544000)
        self.assertIsNone(runner._decimal_ticks(-1e-12))
        self.assertIsNone(runner._decimal_ticks(float("nan")))

    def test_pi_normalization_keeps_unknown_cost_and_tool_use_incomplete(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            session_file = root / "session.jsonl"
            session_file.write_text(
                "\n".join(
                    [
                        json.dumps({"type": "session", "id": "pi-session"}),
                        json.dumps({"type": "compaction"}),
                        json.dumps(
                            {
                                "type": "message",
                                "message": {
                                    "role": "assistant",
                                    "usage": {
                                        "input": 2,
                                        "output": 3,
                                        "cacheRead": 4,
                                        "cacheWrite": 5,
                                        "cost": {"total": "unknown"},
                                    },
                                    "stopReason": "toolUse",
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            output_dir = runner._normalize_pi_session(session_file, root / "normalized")
            usage = json.loads((output_dir / "usage.json").read_text())
            update = json.loads((output_dir / "updates.jsonl").read_text())
            session = usage["session"]
            self.assertEqual(session["inputTokens"], 11)
            self.assertEqual(session["totalTokens"], 14)
            self.assertTrue(session["usageIsIncomplete"])
            self.assertTrue(session["costIsPartial"])
            self.assertNotIn("costUsdTicks", session)
            self.assertEqual(session["costBasis"], "pi_usage_cost_estimate")
            self.assertTrue(session["costIsEstimate"])
            self.assertEqual(update["params"]["update"]["stopReason"], "toolUse")


if __name__ == "__main__":
    unittest.main()
