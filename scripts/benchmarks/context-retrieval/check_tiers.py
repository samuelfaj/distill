#!/usr/bin/env python3
"""Offline checks for paired tier isolation, literal provenance, and paid-call stops."""

from copy import deepcopy
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import urllib.error
from unittest.mock import patch

from generate import generate
import tiers


class TierChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.fixtures = self.root / "fixtures"
        generate(self.fixtures)
        self.adapter = self.root / "adapter"
        self.adapter.write_text("mock adapter")
        self.args = tiers.parser().parse_args(["--fixtures", str(self.fixtures), "--adapter", str(self.adapter),
                                             "--output", str(self.root / "result.json"), "--allow-network"])
        self.manifest, self.sources = tiers.load_fixtures(self.fixtures)
        self.requests = []
        self.adapter_calls = []
        self.helper_content = None
        self.unknown_helper = False
        self.adapter_unknown = False
        self.secret_gate = True

    def response(self, query):
        excerpts = []
        for group in query["evidence"]:
            item = group["alternatives"][0]
            source = self.sources[item["source_id"]]
            excerpts.append({"source_id": source["source_id"], "session_id": source["session_id"],
                             "sha256": source["sha256"], "start_line": item["line"], "end_line": item["line"],
                             "text": item["text"] + "\n"})
        return {"schema_version": 1, "query_id": query["id"], "status": "ok", "excerpts": excerpts,
                "rendered_text": "".join(item["text"] for item in excerpts), "retrieval_latency_ms": 1,
                "production": {"implementation": "rust-production", "network_used": True,
                               "selection": "Jev explicit include/exclude" if self.secret_gate else "lexical fallback: secret-bearing question"},
                "jev_attempts": [{"billing": {"cost_usd_ticks": 10_000_000 if not self.adapter_unknown else None}}]}

    def adapter_mock(self, command, **kwargs):
        request = json.loads(kwargs["input"])
        self.adapter_calls.append(request)
        query = next(item for item in self.manifest["queries"] if item["id"] == request["query_id"])
        return subprocess.CompletedProcess(command, 0, json.dumps(self.response(query)))

    def http_mock(self, path, key=None, payload=None):
        if path.endswith("/endpoints"):
            return {"data": {"endpoints": [{"tag": "deepinfra/fp16" if "ling-3.0" in path else "openai", "supported_parameters": [], "pricing": {
                "prompt": "0.0000001", "completion": "0.0000002"}, "supports_implicit_caching": True}]}}
        self.assertEqual(path, "chat/completions")
        self.requests.append(deepcopy(payload))
        helper = payload["model"] != self.args.main_model
        if helper:
            lines = json.loads(payload["messages"][-1]["content"])["lines"]
            content = self.helper_content if self.helper_content is not None else json.dumps([item["id"] for item in lines])
        else:
            content = payload["messages"][-1]["content"].split("\nEvidence:\n", 1)[1]
        return {"id": "gen-test", "model": payload["model"], "provider": "test/provider",
                "usage": {"cost": None if helper and self.unknown_helper else 0.001, "prompt_tokens": 100,
                          "prompt_tokens_details": {"cached_tokens": 50}, "completion_tokens": 20},
                "choices": [{"finish_reason": "stop", "message": {"content": content}}]}

    def run_mock(self):
        with patch.dict("os.environ", {"OPENROUTER_API_KEY": "test-only"}), \
             patch.object(tiers.live, "http", side_effect=self.http_mock), \
             patch.object(tiers.live, "generation", return_value={"data": None}), \
             patch.object(tiers.subprocess, "run", side_effect=self.adapter_mock), patch("builtins.print"):
            tiers.run(self.args)
        return json.loads(self.args.output.read_text())

    def test_shared_jev_billing_quality_and_arm_isolated_warm_history(self):
        report = self.run_mock()
        self.assertEqual(report["status"], "completed")
        self.assertEqual(len(self.adapter_calls), 12)
        self.assertEqual(len(report["rows"]), 36)
        self.assertEqual(len(report["physical_calls"]), 72)
        self.assertAlmostEqual(report["physical_cost_usd"], 0.072)
        self.assertAlmostEqual(sum(item["counterfactual_cost_usd"] for item in report["summary"].values()), 0.096)
        for pair_index in range(12):
            rows = [row for row in report["rows"] if row["pair_index"] == pair_index]
            self.assertEqual(len(rows), 3)
            self.assertEqual(len({json.dumps(row["before"], sort_keys=True) for row in rows}), 1)
        for row in report["rows"]:
            self.assertEqual(row["before"]["hit_groups"], row["after"]["hit_groups"])
            self.assertTrue(row["quality"]["all_evidence"])
        main_requests = [item for item in self.requests if item["model"] == self.args.main_model]
        prefixes = {}
        for request in main_requests:
            messages = request["messages"]
            prefix = messages[0]["content"]
            prefixes.setdefault(prefix, []).append(messages)
            self.assertNotIn("tools", request)
        self.assertEqual(len(prefixes), 18)
        for histories in prefixes.values():
            self.assertEqual([len(history) for history in histories], [2, 4])
            self.assertEqual(histories[0], histories[1][:2])
        self.assertIsNone(report["summary"]["jev"]["main_reasoning_tokens"])

    def test_invalid_selection_keeps_original_and_known_charge(self):
        self.helper_content = '["invented:999"]'
        report = self.run_mock()
        for row in report["rows"]:
            original = report["pairs"][row["pair_index"]]["common_rendered_baseline"]
            self.assertEqual(row["evidence"], original)
            if row["arm"] != "jev":
                self.assertEqual(row["extra_status"], "fallback_invalid_or_failed")
                self.assertEqual(row["extra_cost_usd"], 0.001)

    def test_empty_selection_cannot_restore_missing_evidence(self):
        self.helper_content = "[]"
        report = self.run_mock()
        for row in report["rows"]:
            if row["arm"] != "jev":
                self.assertEqual(row["after"]["hit_groups"], 0)
                self.assertEqual(row["evidence"], "")
                self.assertFalse(row["quality"]["all_evidence"])

    def test_unknown_helper_cost_pauses_before_another_paid_call(self):
        self.unknown_helper = True
        with self.assertRaisesRegex(ValueError, "unknown charge"):
            self.run_mock()
        report = json.loads(self.args.output.read_text())
        self.assertEqual(report["status"], "failed")
        self.assertIsNone(report["physical_cost_usd"])
        self.assertEqual(len(self.adapter_calls), 1)
        self.assertEqual(len(self.requests), 2)
        self.assertIsNone(report["rows"][-1]["extra_cost_usd"])
        self.assertIn("evidence", report["rows"][-1])

    def test_unknown_jev_cost_stops_before_main_or_helper(self):
        self.adapter_unknown = True
        with self.assertRaisesRegex(ValueError, "unknown charge"):
            self.run_mock()
        self.assertEqual(len(self.adapter_calls), 1)
        self.assertEqual(self.requests, [])
        self.assertIsNone(json.loads(self.args.output.read_text())["physical_cost_usd"])

    def test_production_secret_gate_blocks_extra_dispatch(self):
        self.secret_gate = False
        report = self.run_mock()
        self.assertEqual(len(self.requests), 36)
        for row in report["rows"]:
            if row["arm"] != "jev":
                self.assertEqual(row["extra_status"], "fallback_production_secret_gate_not_confirmed")

    def test_no_network_without_opt_in_or_when_output_exists(self):
        self.args.allow_network = False
        with patch.object(tiers.live, "http") as network:
            with self.assertRaisesRegex(ValueError, "allow-network"):
                tiers.run(self.args)
            self.args.allow_network = True
            self.args.output.write_text("preserve")
            with self.assertRaisesRegex(ValueError, "refusing overwrite"):
                tiers.run(self.args)
            network.assert_not_called()
        self.assertEqual(self.args.output.read_text(), "preserve")

    def test_budget_preflight_persists_and_blocks_all_paid_calls(self):
        self.args.budget_usd = 0.001
        with self.assertRaisesRegex(ValueError, "preflight exceeds"):
            self.run_mock()
        self.assertEqual(self.requests, [])
        self.assertEqual(self.adapter_calls, [])
        self.assertEqual(json.loads(self.args.output.read_text())["status"], "failed")

    def test_separate_spend_limit_stops_even_when_preflight_cap_is_large(self):
        self.args.budget_usd = 40
        self.args.spend_limit_usd = 0.03
        with self.assertRaisesRegex(ValueError, "remaining budget insufficient"):
            self.run_mock()
        report = json.loads(self.args.output.read_text())
        self.assertEqual(report["status"], "failed")
        self.assertLessEqual(report["known_physical_cost_usd"], 0.03)
        self.assertLess(len(self.requests), 60)

    def test_extract_rejects_wrong_duplicate_and_oversized_ids(self):
        query = next(item for item in self.manifest["queries"] if item["id"] == "unicode-literal")
        response = self.response(query)
        identity = next(iter(tiers.lines_from(response)))
        for content in ("not JSON", '["wrong:1"]', json.dumps([identity, identity]), '{}', '[1]'):
            with self.subTest(content=content), self.assertRaises(ValueError):
                tiers.extract(response, content)
        selected = tiers.extract(response, json.dumps([identity]))
        self.assertIn("São Paulo — peça №4", selected["rendered_text"])
        response["excerpts"][0]["text"] = "x" * tiers.LIMIT + "\n"
        with self.assertRaisesRegex(ValueError, "exceeds ceiling"):
            tiers.extract(response, json.dumps([identity]))

    def test_missing_jev_evidence_cannot_be_recovered_by_selection(self):
        query = next(item for item in self.manifest["queries"] if item["id"] == "failure-vs-keyword-decoys")
        response = self.response(query)
        removed = response["excerpts"].pop()
        response["rendered_text"] = "".join(item["text"] for item in response["excerpts"])
        before = tiers.score(query, self.sources, response)
        selected = tiers.extract(response, json.dumps(list(tiers.lines_from(response))))
        self.assertLess(before["hit_groups"], before["gold_groups"])
        self.assertEqual(tiers.score(query, self.sources, selected)["hit_groups"], before["hit_groups"])
        with self.assertRaisesRegex(ValueError, "invalid source IDs"):
            tiers.extract(response, json.dumps([removed["source_id"] + ":" + str(removed["start_line"])]))

    def test_realistic_pricing_tiers_and_unused_web_search_fee(self):
        rates = {"prompt": "0.00001", "completion": "0.00005", "web_search": "0.01",
                 "input_cache_write": "0.0000125", "discount": 0,
                 "overrides": [{"min_prompt_tokens": 272000, "prompt": "0.00002",
                                "completion": "0.000075", "input_cache_write": "0.000025"}]}
        endpoints = [{"tag": "openai/flex", "pricing": rates}, {"tag": "openai", "pricing": rates}]
        with patch.object(tiers.live, "http", return_value={"data": {"endpoints": endpoints}}):
            snapshot = tiers.endpoint(self.args.main_model, self.args.main_provider)
        self.assertEqual(snapshot["tag"], "openai")
        self.assertAlmostEqual(tiers.ceiling(snapshot, 100000, 512), 1.25 + 0.0256)
        self.assertAlmostEqual(tiers.ceiling(snapshot, 272000, 512), 6.8 + 0.0384)
        for invalid in ({**rates, "unknown_fee": "0.01"},
                        {**rates, "overrides": [{"min_prompt_tokens": -1, "prompt": "0.1"}]},
                        {**rates, "overrides": [{"prompt": "0.1"}]},
                        {**rates, "overrides": [{"min_prompt_tokens": 0, "prompt": "nan"}]}):
            with self.assertRaises(ValueError):
                tiers.validate_pricing(invalid)

    def test_temperature_only_when_supported_and_no_search_dispatch(self):
        for supported in ([], ["temperature"]):
            result = tiers.payload("model", {"tag": "openai", "supported_parameters": supported}, [], 512)
            self.assertEqual("temperature" in result, bool(supported))
            self.assertEqual(result["reasoning"], {"enabled": False})
            self.assertFalse(any(key in result for key in ("tools", "plugins", "web_search_options")))

    def test_identical_selection_has_identical_main_evidence_and_provenance(self):
        report = self.run_mock()
        for pair_index, pair in enumerate(report["pairs"]):
            rows = [row for row in report["rows"] if row["pair_index"] == pair_index]
            self.assertEqual({row["evidence"] for row in rows}, {pair["common_rendered_baseline"]})
            self.assertEqual(pair["raw_before"], rows[0]["before"])
            self.assertNotEqual(pair["retrieval"]["rendered_text"], pair["common_rendered_baseline"])
            for identity, item in tiers.lines_from(pair["retrieval"]).items():
                self.assertIn(identity + " " + item["text"], pair["common_rendered_baseline"])

    def test_task_fact_score_rejects_missing_declaration(self):
        rows = [{"arm": "jev", "before": {"hit_groups": 1, "gold_groups": 1},
                 "quality": {"all_evidence": True, "declares_missing": True},
                 "task_quality": {"all_facts": True}}]
        summary = tiers.summarize(rows)["jev"]
        self.assertEqual(summary["main_complete_answers"], 0)
        self.assertEqual(summary["main_task_complete_answers"], 0)

    def test_main_mandatory_reasoning_low_and_helpers_disabled(self):
        report = self.run_mock()
        self.assertEqual(self.args.main_max_tokens, 2048)
        for request in self.requests:
            if request["model"] == self.args.main_model:
                self.assertEqual(request["reasoning"], {"effort": "low"})
                self.assertEqual(request["max_tokens"], 2048)
            else:
                self.assertEqual(request["reasoning"], {"enabled": False})
                self.assertEqual(request["max_tokens"], tiers.HELPER_OUTPUT)
        self.assertEqual(report["requested_reasoning"]["main"], {"effort": "low"})
        self.assertEqual(report["requested_reasoning"]["worker"], {"enabled": False})
        self.assertIn("runner_sha256", report)
        self.assertTrue(all("selection_response" in row["extra"] for row in report["rows"] if "extra" in row))

    def test_main_output_limit_bounds_history_and_full_output_cost(self):
        self.args.main_max_tokens = 4096
        self.args.main_reasoning_effort = "medium"
        report = self.run_mock()
        expected = len(tiers.live.HISTORY.encode()) + 8192 + tiers.live.TURNS * (tiers.LIMIT + 4096 * 8 + 8192)
        self.assertEqual(report["preflight"]["main_input_byte_bound"], expected)
        self.assertEqual(report["main_max_tokens"], 4096)
        snapshot = report["endpoint_snapshots"]
        per_pair = 3 * tiers.ceiling(snapshot["main"], expected, 4096)
        per_pair += sum(tiers.ceiling(snapshot[tier], report["preflight"]["helper_input_byte_bound"], tiers.HELPER_OUTPUT)
                        for tier in ("worker", "utility"))
        self.assertAlmostEqual(report["preflight"]["conservative_bound_usd"], 12 * per_pair + self.args.jev_reserve_usd)
        for request in self.requests:
            if request["model"] == self.args.main_model:
                self.assertEqual(request["max_tokens"], 4096)
                self.assertEqual(request["reasoning"], {"effort": "medium"})
                self.assertLessEqual(tiers.live.prompt_bytes(request["messages"]), expected)

    def test_http_failure_retains_cost_not_error_body(self):
        error = urllib.error.HTTPError("https://test", 502, "secret", {}, io.BytesIO(json.dumps(
            {"usage": {"cost": 0.02}, "error": {"code": 400, "message": "secret"}}).encode()))
        record = {}
        with patch.object(tiers.live, "http", side_effect=error):
            self.assertIsNone(tiers.completion({}, "test-only", record))
        self.assertEqual(record["cost_usd"], 0.02)
        self.assertEqual(record["provider_error_code"], 400)
        self.assertNotIn("secret", json.dumps(record))

    def test_timeout_cost_unknown_not_fake_zero(self):
        record = {}
        with patch.object(tiers.live, "http", side_effect=TimeoutError):
            self.assertIsNone(tiers.completion({}, "test-only", record))
        self.assertIsNone(record["cost_usd"])
        self.assertEqual(record["error"], "TimeoutError")

    def test_invalid_content_retains_cost_and_length_is_invalid(self):
        for message, finish in (({}, "stop"), ({"content": "[]"}, "length")):
            record = {}
            with patch.object(tiers.live, "http", return_value={"usage": {"cost": 0.02},
                      "choices": [{"message": message, "finish_reason": finish}]}):
                self.assertIsNone(tiers.completion({}, "test-only", record))
            self.assertEqual(record["cost_usd"], 0.02)


class RetainedTierChecks(unittest.TestCase):
    def test_completed_run_preserves_pairing_billing_and_literal_evidence(self):
        report = json.loads(Path(__file__).with_name("tiers-qwen-results-2026-10-16.json").read_text())
        self.assertEqual(report["status"], "completed")
        self.assertEqual(len(report["pairs"]), 12)
        self.assertEqual(len(report["physical_calls"]), 72)
        self.assertEqual(report["summary"], tiers.summarize(report["rows"]))
        actual = sum(call["cost_usd"] for call in report["physical_calls"])
        shared = sum(call["cost_usd"] for call in report["physical_calls"] if call["tier"] == "jev")
        self.assertAlmostEqual(actual, report["physical_cost_usd"])
        self.assertAlmostEqual(sum(row["total_cost_usd"] for row in report["rows"]), actual + 2 * shared)
        for call in report["physical_calls"]:
            if call["tier"] != "jev":
                self.assertEqual(call["response_model"], report["models"][call["tier"]])
        for arm in tiers.ARMS:
            rows = [row for row in report["rows"] if row["arm"] == arm]
            self.assertEqual(len(rows), 12)
            self.assertEqual(len({(row["trial"], row["task"], row["turn"]) for row in rows}), 12)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fixtures"
            generate(root)
            manifest, sources = tiers.load_fixtures(root)
            queries = {query["id"]: query for query in manifest["queries"]}
            for index, pair in enumerate(report["pairs"]):
                original = pair["retrieval"]
                query = queries[pair["task"]]
                self.assertEqual(pair["raw_before"], tiers.score(query, sources, original))
                normalized = tiers.normalize(original)
                self.assertEqual(pair["common_rendered_baseline"], normalized["rendered_text"])
                rows = [row for row in report["rows"] if row["pair_index"] == index]
                self.assertEqual({row["arm"] for row in rows}, set(tiers.ARMS))
                for row in rows:
                    selected = normalized if row["arm"] == "jev" else tiers.extract(
                        normalized, row["extra"]["selection_response"])
                    self.assertEqual(row["extra_status"], "not_applicable" if row["arm"] == "jev" else "selected")
                    self.assertEqual(row["before"], pair["raw_before"])
                    self.assertEqual(row["evidence"], selected["rendered_text"])
                    self.assertEqual(row["after"], tiers.score(query, sources, selected))
                    self.assertEqual(row["quality"], tiers.live.quality(query, row["answer"]))
                    self.assertEqual(row["task_quality"], tiers.live.task_quality(query["id"], row["answer"]))
                    self.assertEqual(row["main"]["finish_reason"], "stop")
                    self.assertAlmostEqual(row["total_cost_usd"], row["jev_cost_usd"] +
                                           row["extra_cost_usd"] + row["main"]["cost_usd"])


if __name__ == "__main__":
    unittest.main()
