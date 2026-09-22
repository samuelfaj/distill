#!/usr/bin/env python3
"""Parse Distill session artifacts and compare matched task recordings.

This module is deliberately an evaluator, not a paid benchmark launcher.  It
reads the current on-disk ``usage.json``/``summary.json``/``updates.jsonl``
formats and a small JSONL manifest that supplies the task and grader identity.
Missing or partial billing data stays visible and blocks savings claims.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


SCHEMA_VERSION = 1
TICKS_PER_USD = 10_000_000_000
STATUSES = {"completed", "agent_failed", "infra_failed", "inconclusive"}
SENSITIVE_KEYS = {
    "api_key",
    "apikey",
    "authorization",
    "access_token",
    "bearer",
    "password",
    "secret",
    "token_value",
}
TOKEN_FIELDS = (
    "input_tokens",
    "output_tokens",
    "cached_read_tokens",
    "cache_creation_tokens",
    "reasoning_tokens",
    "total_tokens",
    "model_calls",
)
RUNTIME_FIELDS = (
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
)
TASK_HASH_FIELDS = ("prompt_sha256", "fixture_sha256", "grader_sha256")
SHA256_LENGTH = 64


class EvaluationError(ValueError):
    """Invalid input that must not produce a comparison report."""


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise EvaluationError(f"missing JSON file: {path}") from None
    except json.JSONDecodeError as exc:
        raise EvaluationError(f"invalid JSON in {path} at line {exc.lineno}") from None
    if not isinstance(value, dict):
        raise EvaluationError(f"expected a JSON object: {path}")
    return value


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        raise EvaluationError(f"missing JSONL file: {path}") from None
    records: list[dict[str, Any]] = []
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as exc:
            raise EvaluationError(
                f"invalid JSONL in {path} at line {line_number} (column {exc.colno})"
            ) from None
        if not isinstance(value, dict):
            raise EvaluationError(f"JSONL record {line_number} is not an object: {path}")
        records.append(value)
    return records


def _reject_secret_keys(value: Any, location: str = "input") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            if key.lower() in SENSITIVE_KEYS:
                raise EvaluationError(f"secret-bearing key is not accepted: {location}.{key}")
            _reject_secret_keys(child, f"{location}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _reject_secret_keys(child, f"{location}[{index}]")


def _required_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise EvaluationError(f"{field} must be a non-empty string")
    return value


def _required_int(value: Any, field: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise EvaluationError(f"{field} must be an integer >= {minimum}")
    return value


def _optional_int(value: Any, field: str, *, minimum: int = 0) -> int | None:
    if value is None:
        return None
    return _required_int(value, field, minimum=minimum)


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as exc:
        raise EvaluationError(f"cannot hash protocol file {path}: {exc.strerror}") from None
    return digest.hexdigest()


def _sha256_tree(path: Path) -> str:
    digest = hashlib.sha256()
    for child in sorted(item for item in path.rglob("*") if item.is_file()):
        relative = child.relative_to(path).as_posix().encode("utf-8")
        digest.update(relative)
        digest.update(b"\0")
        try:
            with child.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
        except OSError as exc:
            raise EvaluationError(f"cannot hash protocol fixture {child}: {exc.strerror}") from None
        digest.update(b"\0")
    return digest.hexdigest()


def _required_sha256(value: Any, field: str) -> str:
    result = _required_string(value, field).lower()
    if len(result) != SHA256_LENGTH or any(char not in "0123456789abcdef" for char in result):
        raise EvaluationError(f"{field} must be a lowercase SHA-256 hex digest")
    return result


def _protocol_path(source: Path, raw: Any, field: str, *, directory: bool) -> Path:
    reference = _required_string(raw, field)
    path = Path(reference)
    if path.is_absolute() or ".." in path.parts:
        raise EvaluationError(f"{field} must be a repository-relative path")
    resolved = source.parent / path
    if (resolved.is_dir() if directory else resolved.is_file()) is False:
        kind = "directory" if directory else "file"
        raise EvaluationError(f"{field} does not resolve to a {kind}: {reference}")
    if directory:
        if not any(child.is_file() for child in resolved.rglob("*")):
            raise EvaluationError(f"{field} resolves to an empty fixture directory: {reference}")
    else:
        try:
            content = resolved.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            raise EvaluationError(f"{field} is not readable UTF-8 content: {reference}") from None
        if not content.strip():
            raise EvaluationError(f"{field} resolves to empty content: {reference}")
    return resolved


def load_cohort(path: str | Path) -> dict[str, Any]:
    """Load and validate a frozen cohort/protocol."""

    source = Path(path)
    cohort = _read_json(source)
    _reject_secret_keys(cohort, str(source))
    if cohort.get("schema_version") != SCHEMA_VERSION:
        raise EvaluationError(f"unsupported cohort schema_version in {source}")
    _required_string(cohort.get("cohort_id"), "cohort_id")
    _required_string(cohort.get("task_base_sha"), "task_base_sha")
    repetitions = _required_int(cohort.get("repetitions"), "repetitions", minimum=1)
    required_metadata = cohort.get("required_run_metadata")
    if not isinstance(required_metadata, list) or not required_metadata:
        raise EvaluationError("required_run_metadata must be a non-empty list")
    if any(not isinstance(field, str) or not field.strip() for field in required_metadata):
        raise EvaluationError("required_run_metadata entries must be non-empty strings")
    if len(set(required_metadata)) != len(required_metadata):
        raise EvaluationError("required_run_metadata must not contain duplicates")

    variants = cohort.get("variants")
    if not isinstance(variants, list) or not variants:
        raise EvaluationError("cohort variants must be a non-empty list")
    variant_ids: set[str] = set()
    variant_runtime_pins: dict[str, dict[str, Any]] = {}
    for index, variant in enumerate(variants):
        if not isinstance(variant, dict):
            raise EvaluationError(f"variants[{index}] must be an object")
        variant_id = _required_string(variant.get("id"), f"variants[{index}].id")
        if variant_id in variant_ids:
            raise EvaluationError(f"duplicate variant id: {variant_id}")
        variant_ids.add(variant_id)
        runtime_pin = variant.get("runtime_pin")
        if not isinstance(runtime_pin, dict):
            raise EvaluationError(f"variants[{index}].runtime_pin must be an object")
        runtime_status = _required_string(
            runtime_pin.get("status"), f"variants[{index}].runtime_pin.status"
        )
        if runtime_status == "pinned":
            for field in RUNTIME_FIELDS:
                if field.endswith("_sha256"):
                    _required_sha256(
                        runtime_pin.get(field), f"variants[{index}].runtime_pin.{field}"
                    )
                else:
                    _required_string(
                        runtime_pin.get(field), f"variants[{index}].runtime_pin.{field}"
                    )
        else:
            for field in RUNTIME_FIELDS:
                value = runtime_pin.get(field)
                if value is not None and not isinstance(value, str):
                    raise EvaluationError(
                        f"variants[{index}].runtime_pin.{field} must be a string or null"
                    )
                if value is not None and field.endswith("_sha256"):
                    _required_sha256(value, f"variants[{index}].runtime_pin.{field}")
        variant_runtime_pins[variant_id] = runtime_pin

    cases = cohort.get("cases")
    if not isinstance(cases, list) or not cases:
        raise EvaluationError("cohort cases must be a non-empty list")
    case_ids: set[str] = set()
    for index, case in enumerate(cases):
        if not isinstance(case, dict):
            raise EvaluationError(f"cases[{index}] must be an object")
        case_id = _required_string(case.get("id"), f"cases[{index}].id")
        if case_id in case_ids:
            raise EvaluationError(f"duplicate case id: {case_id}")
        case_ids.add(case_id)
        _required_string(case.get("class"), f"cases[{index}].class")
        _required_string(case.get("language"), f"cases[{index}].language")
        prompt_path = _protocol_path(
            source, case.get("prompt_ref"), f"cases[{index}].prompt_ref", directory=False
        )
        fixture_path = _protocol_path(
            source, case.get("fixture_ref"), f"cases[{index}].fixture_ref", directory=True
        )
        prompt_sha256 = _required_sha256(case.get("prompt_sha256"), f"cases[{index}].prompt_sha256")
        fixture_sha256 = _required_sha256(
            case.get("fixture_sha256"), f"cases[{index}].fixture_sha256"
        )
        if _sha256_file(prompt_path) != prompt_sha256:
            raise EvaluationError(f"cases[{index}].prompt_sha256 does not match prompt_ref")
        if _sha256_tree(fixture_path) != fixture_sha256:
            raise EvaluationError(f"cases[{index}].fixture_sha256 does not match fixture_ref")
        grader = case.get("grader")
        if not isinstance(grader, dict):
            raise EvaluationError(f"cases[{index}].grader must be an object")
        _required_string(grader.get("id"), f"cases[{index}].grader.id")
        command = grader.get("command")
        if not isinstance(command, list) or not command or not all(
            isinstance(item, str) and item for item in command
        ):
            raise EvaluationError(f"cases[{index}].grader.command must be a non-empty list")
        grader_path = _protocol_path(
            source,
            grader.get("script_ref"),
            f"cases[{index}].grader.script_ref",
            directory=False,
        )
        grader_sha256 = _required_sha256(
            grader.get("sha256"), f"cases[{index}].grader.sha256"
        )
        if _sha256_file(grader_path) != grader_sha256:
            raise EvaluationError(f"cases[{index}].grader.sha256 does not match script_ref")
        isolation = case.get("isolation")
        if not isinstance(isolation, dict):
            raise EvaluationError(f"cases[{index}].isolation must be an object")
        for key in ("fresh_checkout", "isolated_session", "network_policy"):
            if key not in isolation:
                raise EvaluationError(f"cases[{index}].isolation missing {key}")

    comparisons = cohort.get("comparisons")
    if not isinstance(comparisons, list) or not comparisons:
        raise EvaluationError("cohort comparisons must be a non-empty list")
    for index, comparison in enumerate(comparisons):
        if not isinstance(comparison, dict):
            raise EvaluationError(f"comparisons[{index}] must be an object")
        baseline = _required_string(comparison.get("baseline"), f"comparisons[{index}].baseline")
        candidate = _required_string(comparison.get("candidate"), f"comparisons[{index}].candidate")
        if baseline not in variant_ids or candidate not in variant_ids:
            raise EvaluationError(f"comparison {index} references an unknown variant")
        if baseline == candidate:
            raise EvaluationError(f"comparison {index} compares a variant with itself")
        if not isinstance(comparison.get("require_same_task_revision"), bool):
            raise EvaluationError(
                f"comparisons[{index}].require_same_task_revision must be boolean"
            )
        match_runtime_fields = comparison.get("match_runtime_fields", [])
        if not isinstance(match_runtime_fields, list) or any(
            field not in RUNTIME_FIELDS for field in match_runtime_fields
        ):
            raise EvaluationError(
                f"comparisons[{index}].match_runtime_fields must list runtime fields"
            )

    cohort["_source"] = str(source)
    cohort["_variant_ids"] = variant_ids
    cohort["_case_ids"] = case_ids
    cohort["_repetitions"] = repetitions
    cohort["_required_run_metadata"] = tuple(required_metadata)
    cohort["_variant_runtime_pins"] = variant_runtime_pins
    cohort["_case_pins"] = {
        case["id"]: {
            "prompt_sha256": case["prompt_sha256"],
            "fixture_sha256": case["fixture_sha256"],
            "grader_sha256": case["grader"]["sha256"],
            "grader_id": case["grader"]["id"],
        }
        for case in cases
    }
    cohort["_base_revision_variants"] = {
        variant["id"] for variant in variants if variant.get("runner") == "distill"
    }
    return cohort


def load_runs(path: str | Path) -> list[dict[str, Any]]:
    """Load the JSONL run manifest without reading any session artifacts yet."""

    source = Path(path)
    records = _read_jsonl(source)
    for index, record in enumerate(records, 1):
        _reject_secret_keys(record, f"{source}:{index}")
        _required_string(record.get("case_id"), f"run {index}.case_id")
        _required_string(record.get("variant"), f"run {index}.variant")
        _required_int(record.get("repetition"), f"run {index}.repetition", minimum=1)
        status = _required_string(record.get("execution_status"), f"run {index}.execution_status")
        if status not in STATUSES:
            raise EvaluationError(f"run {index} has unsupported execution_status: {status}")
        if "accepted" not in record:
            raise EvaluationError(f"run {index} must include accepted (use null when inconclusive)")
        accepted = record["accepted"]
        if accepted is not None and not isinstance(accepted, bool):
            raise EvaluationError(f"run {index}.accepted must be boolean or null")
        if status == "completed" and accepted is None:
            raise EvaluationError(f"completed run {index} needs a boolean accepted outcome")
        if status == "agent_failed" and accepted is not False:
            raise EvaluationError(f"agent_failed run {index} must have accepted=false")
        if status in {"infra_failed", "inconclusive"} and accepted is not None:
            raise EvaluationError(f"{status} run {index} must have accepted=null")
        _required_string(record.get("grader_id"), f"run {index}.grader_id")
        grader_exit = record.get("grader_exit_code")
        if grader_exit is not None:
            _required_int(grader_exit, f"run {index}.grader_exit_code")
        if status == "completed":
            if grader_exit is None:
                raise EvaluationError(f"completed run {index} needs grader_exit_code")
            if accepted != (grader_exit == 0):
                raise EvaluationError(
                    f"run {index}.accepted must match the behavioral grader exit code"
                )
        elif status == "agent_failed" and grader_exit == 0:
            raise EvaluationError(f"agent_failed run {index} cannot have grader_exit_code=0")
        elif status in {"infra_failed", "inconclusive"} and grader_exit is not None:
            raise EvaluationError(f"{status} run {index} cannot have a grader exit code")

        task = record.get("task")
        if not isinstance(task, dict):
            raise EvaluationError(f"run {index}.task must be an object")
        _required_string(task.get("revision"), f"run {index}.task.revision")
        for field in TASK_HASH_FIELDS:
            _required_sha256(task.get(field), f"run {index}.task.{field}")

        runtime = record.get("runtime")
        if not isinstance(runtime, dict):
            raise EvaluationError(f"run {index}.runtime must be an object")
        for field in RUNTIME_FIELDS:
            if field.endswith("_sha256"):
                _required_sha256(runtime.get(field), f"run {index}.runtime.{field}")
            else:
                _required_string(runtime.get(field), f"run {index}.runtime.{field}")
    return records


@dataclass(frozen=True)
class UsageSnapshot:
    present: bool
    cost_usd_ticks: int | None
    cost_status: str
    usage_is_incomplete: bool
    cost_is_partial: bool
    fields: dict[str, int]
    session_id: str | None
    turn_count: int
    cost_basis: str | None
    cost_is_estimate: bool

    @property
    def cost_kind(self) -> str:
        if self.cost_status != "complete":
            return "incomplete"
        return "estimated" if self.cost_is_estimate else "actual"

    @property
    def cost_basis_key(self) -> str:
        return f"{self.cost_kind}:{self.cost_basis or self.cost_kind}"


@dataclass(frozen=True)
class TerminalEvidence:
    present: bool
    turn_completed_count: int
    stop_reasons: tuple[str, ...]
    parse_errors: int

    @property
    def observed_status(self) -> str:
        if self.parse_errors:
            return "incomplete"
        if not self.present:
            return "missing"
        if self.turn_completed_count == 0:
            return "missing"
        stop = self.stop_reasons[-1]
        return "completed" if stop in {"end_turn", "stop_sequence"} else "failed"


@dataclass(frozen=True)
class JevEvidence:
    present: bool
    decision_count: int
    escalated_count: int
    input_tokens: int
    output_tokens: int
    parse_errors: int


def _usage_from_path(path: Path | None) -> UsageSnapshot:
    if path is None or not path.is_file():
        return UsageSnapshot(False, None, "missing", False, False, {}, None, 0, None, False)
    value = _read_json(path)
    session = value.get("session")
    if not isinstance(session, dict):
        raise EvaluationError(f"usage session must be an object: {path}")
    fields: dict[str, int] = {}
    for field in TOKEN_FIELDS:
        fields[field] = _required_int(session.get(_camel(field), 0), f"{path}:{field}")
    cost = _optional_int(session.get("costUsdTicks"), f"{path}:costUsdTicks")
    partial = session.get("costIsPartial", False)
    incomplete = session.get("usageIsIncomplete", False)
    basis = session.get("costBasis")
    estimate = session.get("costIsEstimate", False)
    if basis is not None and (not isinstance(basis, str) or not basis.strip()):
        raise EvaluationError(f"usage costBasis must be a non-empty string or null: {path}")
    if not isinstance(partial, bool) or not isinstance(incomplete, bool) or not isinstance(estimate, bool):
        raise EvaluationError(f"usage cost flags must be boolean: {path}")
    if incomplete:
        cost_status = "incomplete"
    elif partial:
        cost_status = "partial"
    elif cost is None:
        cost_status = "missing"
    else:
        cost_status = "complete"
    session_id = value.get("sessionId")
    if session_id is not None and not isinstance(session_id, str):
        raise EvaluationError(f"usage sessionId must be a string: {path}")
    turns = value.get("turns", [])
    if not isinstance(turns, list):
        raise EvaluationError(f"usage turns must be a list: {path}")
    return UsageSnapshot(
        True,
        cost,
        cost_status,
        incomplete,
        partial,
        fields,
        session_id,
        len(turns),
        basis or ("provider_reported_exact" if cost_status == "complete" else None),
        estimate,
    )


def _camel(snake: str) -> str:
    head, *tail = snake.split("_")
    return head + "".join(part.capitalize() for part in tail)


def _terminal_from_line(value: dict[str, Any]) -> str | None:
    params = value.get("params", value)
    if not isinstance(params, dict):
        return None
    update = params.get("update", params)
    if not isinstance(update, dict) or update.get("sessionUpdate") != "turn_completed":
        return None
    stop = update.get("stopReason")
    return stop if isinstance(stop, str) and stop else "unknown"


def _terminal_evidence(path: Path | None) -> TerminalEvidence:
    if path is None or not path.is_file():
        return TerminalEvidence(False, 0, (), 0)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        raise EvaluationError(f"cannot read session updates: {path} ({exc.strerror})") from None
    stops: list[str] = []
    errors = 0
    for line in lines:
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            errors += 1
            continue
        if isinstance(value, dict):
            stop = _terminal_from_line(value)
            if stop is not None:
                stops.append(stop)
    return TerminalEvidence(True, len(stops), tuple(stops), errors)


def _jev_evidence(path: Path | None) -> JevEvidence:
    if path is None or not path.is_file():
        return JevEvidence(False, 0, 0, 0, 0, 0)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        raise EvaluationError(f"cannot read Jev log: {path} ({exc.strerror})") from None
    decisions = escalated = input_tokens = output_tokens = errors = 0
    for line in lines:
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            errors += 1
            continue
        if not isinstance(value, dict):
            errors += 1
            continue
        fields = value.get("fields")
        if not isinstance(fields, dict) or "decision" not in fields:
            continue
        decisions += 1
        escalated += int(fields.get("escalated") is True)
        input_tokens += _log_int(fields.get("input_tokens"))
        output_tokens += _log_int(fields.get("output_tokens"))
    return JevEvidence(True, decisions, escalated, input_tokens, output_tokens, errors)


def _log_int(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else 0


def _summary_metadata(path: Path | None) -> dict[str, Any]:
    if path is None or not path.is_file():
        return {"present": False}
    value = _read_json(path)
    info = value.get("info")
    info_id = info.get("id") if isinstance(info, dict) else None
    if info_id is not None and not isinstance(info_id, str):
        raise EvaluationError(f"summary info.id must be a string: {path}")
    return {
        "present": True,
        "session_id": info_id,
        "model": value.get("currentModelId") if isinstance(value.get("currentModelId"), str) else None,
        "effort": value.get("reasoningEffort") if isinstance(value.get("reasoningEffort"), str) else None,
        "revision": value.get("headCommit") if isinstance(value.get("headCommit"), str) else None,
        "branch": value.get("headBranch") if isinstance(value.get("headBranch"), str) else None,
    }


def _artifact_path(record: dict[str, Any], manifest_dir: Path, key: str) -> Path | None:
    raw = record.get(key)
    if raw is None:
        return None
    if not isinstance(raw, str) or not raw:
        raise EvaluationError(f"run {record.get('run_id', '<unnamed>')}.{key} must be a path string")
    path = Path(raw)
    return path if path.is_absolute() else manifest_dir / path


def _parse_run(record: dict[str, Any], manifest_dir: Path) -> dict[str, Any]:
    session_dir = _artifact_path(record, manifest_dir, "session_dir")
    usage_path = _artifact_path(record, manifest_dir, "usage_file")
    summary_path = _artifact_path(record, manifest_dir, "summary_file")
    updates_path = _artifact_path(record, manifest_dir, "updates_file")
    jev_path = _artifact_path(record, manifest_dir, "jev_log")
    if session_dir is not None:
        usage_path = usage_path or session_dir / "usage.json"
        summary_path = summary_path or session_dir / "summary.json"
        updates_path = updates_path or session_dir / "updates.jsonl"
    usage = _usage_from_path(usage_path)
    summary = _summary_metadata(summary_path)
    if usage.session_id and summary.get("session_id") and usage.session_id != summary["session_id"]:
        raise EvaluationError(f"usage/summary session id mismatch for run {record.get('run_id', '<unnamed>')}")
    terminal = _terminal_evidence(updates_path)
    jev = _jev_evidence(jev_path)
    task = record["task"]
    runtime = record["runtime"]
    session_id = usage.session_id or summary.get("session_id")
    return {
        "run_id": record.get("run_id"),
        "case_id": record["case_id"],
        "variant": record["variant"],
        "repetition": record["repetition"],
        "execution_status": record["execution_status"],
        "accepted": record["accepted"],
        "grader_id": record["grader_id"],
        "grader_exit_code": record.get("grader_exit_code"),
        "task": {
            **task,
            "summary_revision": summary.get("revision"),
        },
        "runtime": {
            **runtime,
            "summary_model": summary.get("model"),
            "summary_effort": summary.get("effort"),
        },
        "session_id": session_id,
        "usage": {
            "present": usage.present,
            "cost_usd_ticks": usage.cost_usd_ticks,
            "cost_status": usage.cost_status,
            "cost_is_partial": usage.cost_is_partial,
            "usage_is_incomplete": usage.usage_is_incomplete,
            "cost_basis": usage.cost_basis,
            "cost_basis_key": usage.cost_basis_key,
            "cost_kind": usage.cost_kind,
            "cost_is_estimate": usage.cost_is_estimate,
            "turn_count": usage.turn_count,
            "fields": usage.fields,
        },
        "terminal": {
            "required": session_dir is not None,
            "present": terminal.present,
            "turn_completed_count": terminal.turn_completed_count,
            "stop_reasons": list(terminal.stop_reasons),
            "parse_errors": terminal.parse_errors,
            "observed_status": terminal.observed_status,
        },
        "jev": {
            "present": jev.present,
            "decision_count": jev.decision_count,
            "escalated_count": jev.escalated_count,
            "input_tokens": jev.input_tokens,
            "output_tokens": jev.output_tokens,
            "parse_errors": jev.parse_errors,
        },
    }


def _expected_keys(cohort: dict[str, Any]) -> set[tuple[str, str, int]]:
    return {
        (case["id"], variant["id"], repetition)
        for case in cohort["cases"]
        for variant in cohort["variants"]
        for repetition in range(1, cohort["_repetitions"] + 1)
    }


def _sum_known(rows: Iterable[dict[str, Any]], field: str) -> int:
    return sum(row["usage"]["fields"].get(field, 0) for row in rows if row["usage"]["present"])


def _variant_summary(variant_id: str, rows: list[dict[str, Any]], planned: int) -> dict[str, Any]:
    actual = [row for row in rows if row["variant"] == variant_id]
    complete_cost = [
        row for row in actual if row["usage"]["cost_status"] == "complete" and row["usage"]["cost_usd_ticks"] is not None
    ]
    reported_incomplete = [
        row for row in actual if row["usage"]["cost_status"] in {"partial", "incomplete"}
    ]
    missing_cost = [row for row in actual if row["usage"]["cost_usd_ticks"] is None]
    complete_cost_by_basis: dict[str, int] = {}
    for row in complete_cost:
        basis = row["usage"]["cost_basis_key"]
        complete_cost_by_basis[basis] = complete_cost_by_basis.get(basis, 0) + row["usage"]["cost_usd_ticks"]
    complete_cost_bases = sorted(complete_cost_by_basis)
    mixed_cost_basis = len(complete_cost_bases) > 1
    complete_cost_basis_names = sorted(
        {row["usage"]["cost_basis"] for row in complete_cost}
    )
    accepted = [row for row in actual if row["accepted"] is True]
    accepted_without_complete_cost = [
        row for row in accepted if row["usage"]["cost_status"] != "complete"
    ]
    accepted_cost_bases = {row["usage"]["cost_basis_key"] for row in accepted if row["usage"]["cost_status"] == "complete"}
    accepted_cost = None
    if accepted and not accepted_without_complete_cost and len(accepted_cost_bases) == 1:
        accepted_cost = sum(row["usage"]["cost_usd_ticks"] or 0 for row in accepted)
    all_planned_cost_complete = len(actual) == planned and len(complete_cost) == planned
    average_ticks = None
    average_fraction = None
    if all_planned_cost_complete and accepted and not mixed_cost_basis:
        total_attempt_cost = sum(row["usage"]["cost_usd_ticks"] or 0 for row in complete_cost)
        count = len(accepted)
        if total_attempt_cost % count == 0:
            average_ticks = total_attempt_cost // count
        else:
            average_fraction = {"numerator": total_attempt_cost, "denominator": count}
    return {
        "planned": planned,
        "executed": len(actual),
        "missing_execution": planned - len(actual),
        "completed": sum(row["execution_status"] == "completed" for row in actual),
        "agent_failed": sum(row["execution_status"] == "agent_failed" for row in actual),
        "infra_failed": sum(row["execution_status"] == "infra_failed" for row in actual),
        "inconclusive": sum(row["execution_status"] == "inconclusive" for row in actual),
        "accepted": len(accepted),
        "rejected": sum(row["accepted"] is False for row in actual),
        "usage_missing": sum(not row["usage"]["present"] for row in actual),
        "cost_complete_runs": len(complete_cost),
        "cost_partial_or_incomplete_runs": len(reported_incomplete),
        "cost_missing_runs": len(missing_cost),
        "cost_complete_for_all_planned": all_planned_cost_complete,
        "cost_basis": (
            "mixed"
            if mixed_cost_basis
            else (
                complete_cost_basis_names[0]
                if complete_cost_basis_names
                else None
            )
        ),
        "cost_basis_mixed": mixed_cost_basis,
        "cost_complete_ticks_all_runs_by_basis": complete_cost_by_basis,
        "cost_complete_ticks_all_runs": (
            None
            if mixed_cost_basis
            else sum(row["usage"]["cost_usd_ticks"] or 0 for row in complete_cost)
        ),
        "cost_reported_on_partial_runs_ticks": sum(
            row["usage"]["cost_usd_ticks"] or 0 for row in reported_incomplete
        ),
        "cost_ticks_for_accepted_runs": accepted_cost,
        "cost_per_accepted_task_ticks": average_ticks,
        "cost_per_accepted_task_ticks_fraction": average_fraction,
        "observed_tokens": {field: _sum_known(actual, field) for field in TOKEN_FIELDS},
    }


def _pair_comparison(
    comparison: dict[str, Any],
    rows_by_key: dict[tuple[str, str, int], dict[str, Any]],
    cohort: dict[str, Any],
) -> dict[str, Any]:
    baseline_id = comparison["baseline"]
    candidate_id = comparison["candidate"]
    keys = [
        (case["id"], repetition)
        for case in cohort["cases"]
        for repetition in range(1, cohort["_repetitions"] + 1)
    ]
    reasons: set[str] = set()
    missing: list[dict[str, Any]] = []
    quality_gaps: list[dict[str, Any]] = []
    all_pairs: list[tuple[dict[str, Any], dict[str, Any]]] = []
    intersection: list[tuple[dict[str, Any], dict[str, Any]]] = []
    baseline_task_revisions: set[str] = set()
    candidate_task_revisions: set[str] = set()
    for case_id, repetition in keys:
        baseline = rows_by_key.get((case_id, baseline_id, repetition))
        candidate = rows_by_key.get((case_id, candidate_id, repetition))
        if baseline is None or candidate is None:
            missing.append({"case_id": case_id, "repetition": repetition})
            reasons.add("missing_execution")
            continue
        for row, revisions in ((baseline, baseline_task_revisions), (candidate, candidate_task_revisions)):
            revision = row["task"].get("revision")
            if isinstance(revision, str) and revision:
                revisions.add(revision)
            else:
                reasons.add("missing_task_revision")
            if (
                row["variant"] in cohort["_base_revision_variants"]
                and revision != cohort["task_base_sha"]
            ):
                reasons.add("task_revision_not_frozen")
            summary_revision = row["task"].get("summary_revision")
            if summary_revision is not None and summary_revision != revision:
                reasons.add("summary_task_revision_mismatch")
            case_pin = cohort["_case_pins"][case_id]
            if any(row["task"].get(field) != case_pin[field] for field in TASK_HASH_FIELDS):
                reasons.add("protocol_hash_mismatch")
            runtime_pin = cohort["_variant_runtime_pins"][row["variant"]]
            if runtime_pin.get("status") != "pinned":
                reasons.add("runtime_not_frozen")
            else:
                if any(row["runtime"].get(field) != runtime_pin[field] for field in RUNTIME_FIELDS):
                    reasons.add("runtime_metadata_mismatch")
            if (
                row["runtime"].get("summary_model") is not None
                and row["runtime"].get("summary_model") != row["runtime"].get("model")
            ) or (
                row["runtime"].get("summary_effort") is not None
                and row["runtime"].get("summary_effort") != row["runtime"].get("effort")
            ):
                reasons.add("runtime_observation_mismatch")
        all_pairs.append((baseline, candidate))
        if baseline["accepted"] is True and candidate["accepted"] is not True:
            quality_gaps.append({"case_id": case_id, "repetition": repetition})
        if baseline["accepted"] is True and candidate["accepted"] is True:
            intersection.append((baseline, candidate))
        for row in (baseline, candidate):
            if row["execution_status"] in {"infra_failed", "inconclusive"}:
                reasons.add("inconclusive_execution")
            if row["accepted"] is None:
                reasons.add("missing_acceptance")
            if row["usage"]["cost_status"] != "complete":
                reasons.add("incomplete_cost")
            if row["terminal"]["parse_errors"]:
                reasons.add("malformed_updates")
            if row["terminal"]["required"] and not row["terminal"]["present"]:
                reasons.add("missing_terminal_evidence")
            observed_status = row["terminal"]["observed_status"]
            if row["terminal"]["present"] and observed_status == "failed" and row["execution_status"] == "completed":
                reasons.add("execution_evidence_mismatch")
            if row["terminal"]["present"] and observed_status == "completed" and row["execution_status"] == "agent_failed":
                reasons.add("execution_evidence_mismatch")
    complete_cost_bases = {
        row["usage"]["cost_basis_key"]
        for pair in all_pairs
        for row in pair
        if row["usage"]["cost_status"] == "complete"
        and row["usage"]["cost_usd_ticks"] is not None
    }
    if len(complete_cost_bases) > 1:
        reasons.add("mixed_cost_basis")
    if missing:
        reasons.add("missing_execution")
    if quality_gaps:
        reasons.add("quality_not_preserved")
    if comparison["require_same_task_revision"]:
        if len(baseline_task_revisions) != 1 or len(candidate_task_revisions) != 1:
            reasons.add("task_revision_not_frozen")
        elif baseline_task_revisions != candidate_task_revisions:
            reasons.add("task_revision_mismatch")
    for baseline, candidate in all_pairs:
        for field in comparison["match_runtime_fields"]:
            if baseline["runtime"].get(field) != candidate["runtime"].get(field):
                reasons.add("paired_runtime_mismatch")
    if not intersection:
        reasons.add("no_matched_accepted_tasks")

    secondary_baseline_ticks = None
    secondary_candidate_ticks = None
    if intersection and all(
        row["usage"]["cost_status"] == "complete"
        for pair in intersection
        for row in pair
    ):
        secondary_baseline_ticks = sum(pair[0]["usage"]["cost_usd_ticks"] or 0 for pair in intersection)
        secondary_candidate_ticks = sum(pair[1]["usage"]["cost_usd_ticks"] or 0 for pair in intersection)

    baseline_ticks = None
    candidate_ticks = None
    baseline_average = None
    candidate_average = None
    savings = None
    savings_fraction = None
    data_eligible = not reasons
    baseline_accepted_count = sum(pair[0]["accepted"] is True for pair in all_pairs)
    candidate_accepted_count = sum(pair[1]["accepted"] is True for pair in all_pairs)
    if data_eligible and all_pairs and baseline_accepted_count and candidate_accepted_count:
        baseline_ticks = sum(pair[0]["usage"]["cost_usd_ticks"] or 0 for pair in all_pairs)
        candidate_ticks = sum(pair[1]["usage"]["cost_usd_ticks"] or 0 for pair in all_pairs)
        baseline_average = {"numerator": baseline_ticks, "denominator": baseline_accepted_count}
        candidate_average = {"numerator": candidate_ticks, "denominator": candidate_accepted_count}
        savings_numerator = (
            baseline_ticks * candidate_accepted_count
            - candidate_ticks * baseline_accepted_count
        )
        savings_denominator = baseline_accepted_count * candidate_accepted_count
        if savings_numerator % savings_denominator == 0:
            savings = savings_numerator // savings_denominator
        else:
            savings_fraction = {
                "numerator": savings_numerator,
                "denominator": savings_denominator,
            }
        if savings_numerator <= 0:
            reasons.add("candidate_not_cheaper")
    else:
        reasons.add("no_accepted_tasks")
    eligible = data_eligible and not reasons.intersection({"candidate_not_cheaper", "no_accepted_tasks"})
    return {
        "baseline": baseline_id,
        "candidate": candidate_id,
        "require_same_task_revision": comparison["require_same_task_revision"],
        "match_runtime_fields": comparison["match_runtime_fields"],
        "planned_pairs": len(keys),
        "matched_accepted_pairs": len(intersection),
        "quality_gaps": quality_gaps,
        "missing_pairs": missing,
        "baseline_task_revisions": sorted(baseline_task_revisions),
        "candidate_task_revisions": sorted(candidate_task_revisions),
        "baseline_accepted_count": baseline_accepted_count,
        "candidate_accepted_count": candidate_accepted_count,
        "baseline_total_attempt_ticks": baseline_ticks,
        "candidate_total_attempt_ticks": candidate_ticks,
        "baseline_cost_per_accepted_task_ticks": (
            baseline_average["numerator"] // baseline_average["denominator"]
            if baseline_average and baseline_average["numerator"] % baseline_average["denominator"] == 0
            else None
        ),
        "candidate_cost_per_accepted_task_ticks": (
            candidate_average["numerator"] // candidate_average["denominator"]
            if candidate_average and candidate_average["numerator"] % candidate_average["denominator"] == 0
            else None
        ),
        "baseline_cost_per_accepted_task_ticks_fraction": baseline_average,
        "candidate_cost_per_accepted_task_ticks_fraction": candidate_average,
        "secondary_matched_accepted_baseline_ticks": secondary_baseline_ticks,
        "secondary_matched_accepted_candidate_ticks": secondary_candidate_ticks,
        "savings_ticks": savings,
        "savings_fraction": savings_fraction,
        "savings_claim_allowed": eligible,
        "blocking_reasons": sorted(reasons),
    }


def build_report(
    cohort: dict[str, Any],
    records: list[dict[str, Any]],
    *,
    manifest_dir: str | Path = ".",
) -> dict[str, Any]:
    """Build a report from a validated cohort and JSONL records."""

    variant_ids = cohort["_variant_ids"]
    case_ids = cohort["_case_ids"]
    expected = _expected_keys(cohort)
    parsed: list[dict[str, Any]] = []
    seen: set[tuple[str, str, int]] = set()
    unexpected: list[dict[str, Any]] = []
    duplicates: list[dict[str, Any]] = []
    root = Path(manifest_dir)
    for record in records:
        key = (record["case_id"], record["variant"], record["repetition"])
        if key not in expected:
            unexpected.append({"case_id": key[0], "variant": key[1], "repetition": key[2]})
            continue
        if key in seen:
            duplicates.append({"case_id": key[0], "variant": key[1], "repetition": key[2]})
            continue
        expected_grader = next(case["grader"]["id"] for case in cohort["cases"] if case["id"] == key[0])
        if record["grader_id"] != expected_grader:
            raise EvaluationError(
                f"grader mismatch for {key[0]}: expected {expected_grader}, got {record['grader_id']}"
            )
        for field in cohort["_required_run_metadata"]:
            _required_string(
                record["runtime"].get(field),
                f"{key[0]}/{key[1]}/{key[2]}.runtime.{field}",
            )
        seen.add(key)
        parsed.append(_parse_run(record, root))
    if unexpected:
        raise EvaluationError(f"run manifest contains {len(unexpected)} unplanned records")
    if duplicates:
        raise EvaluationError(f"run manifest contains {len(duplicates)} duplicate task cells")

    rows_by_key = {(row["case_id"], row["variant"], row["repetition"]): row for row in parsed}
    missing_rows = [
        {"case_id": case_id, "variant": variant_id, "repetition": repetition}
        for case_id, variant_id, repetition in sorted(expected - seen)
    ]
    planned_per_variant = len(case_ids) * cohort["_repetitions"]
    variants = {
        variant_id: _variant_summary(variant_id, parsed, planned_per_variant)
        for variant_id in sorted(variant_ids)
    }
    comparisons = [
        _pair_comparison(comparison, rows_by_key, cohort)
        for comparison in cohort["comparisons"]
    ]
    headline_reasons = sorted({reason for comparison in comparisons for reason in comparison["blocking_reasons"]})
    headline_allowed = bool(comparisons) and all(
        comparison["savings_claim_allowed"] for comparison in comparisons
    )
    return {
        "schema_version": SCHEMA_VERSION,
        "cohort_id": cohort["cohort_id"],
        "task_base_sha": cohort["task_base_sha"],
        "measurement_status": "recorded_artifacts_only",
        "headline": {
            "status": "eligible" if headline_allowed else "blocked",
            "savings_claim_allowed": headline_allowed,
            "blocking_reasons": headline_reasons,
        },
        "coverage": {
            "planned_cells": len(expected),
            "recorded_cells": len(parsed),
            "missing_cells": len(missing_rows),
            "missing": missing_rows,
        },
        "variants": variants,
        "comparisons": comparisons,
        "runs": sorted(parsed, key=lambda row: (row["case_id"], row["variant"], row["repetition"])),
        "policy": {
            "missing_cost_is_not_zero": True,
            "failed_attempts_are_charged_when_recorded": True,
            "partial_or_incomplete_cost_blocks_headline": True,
            "router_is_not_acceptance_grader": True,
            "task_identity_is_separate_from_runtime_identity": True,
            "frozen_protocol_hashes_required": True,
            "runtime_pin_required_per_variant": True,
            "grader_exit_controls_acceptance": True,
            "cost_basis_preserved": True,
            "estimated_cost_is_not_provider_statement": True,
            "mixed_cost_basis_blocks_claim": True,
        },
    }


def _usd_string(ticks: int | None) -> str | None:
    if ticks is None:
        return None
    sign = "-" if ticks < 0 else ""
    whole, fraction = divmod(abs(ticks), TICKS_PER_USD)
    return f"{sign}{whole}.{fraction:010d}"


def _add_usd_strings(report: dict[str, Any]) -> None:
    for summary in report["variants"].values():
        summary["cost_complete_usd"] = _usd_string(summary["cost_complete_ticks_all_runs"])
        summary["cost_per_accepted_task_usd"] = _usd_string(summary["cost_per_accepted_task_ticks"])
    for comparison in report["comparisons"]:
        comparison["baseline_usd_on_total_attempts"] = _usd_string(
            comparison["baseline_total_attempt_ticks"]
        )
        comparison["candidate_usd_on_total_attempts"] = _usd_string(
            comparison["candidate_total_attempt_ticks"]
        )
        comparison["secondary_matched_accepted_baseline_usd"] = _usd_string(
            comparison["secondary_matched_accepted_baseline_ticks"]
        )
        comparison["secondary_matched_accepted_candidate_usd"] = _usd_string(
            comparison["secondary_matched_accepted_candidate_ticks"]
        )
        comparison["savings_usd"] = _usd_string(comparison["savings_ticks"])


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="task-cost-eval",
        description="Compare matched task recordings without treating missing cost as zero.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    report = subparsers.add_parser("report", help="build a JSON report from recorded artifacts")
    report.add_argument("--cohort", required=True, type=Path)
    report.add_argument("--runs", required=True, type=Path, help="JSONL run manifest")
    report.add_argument("--output", type=Path, help="write the report here instead of stdout")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.command != "report":
        return 2
    try:
        cohort = load_cohort(args.cohort)
        records = load_runs(args.runs)
        report = build_report(cohort, records, manifest_dir=args.runs.parent)
        _add_usd_strings(report)
        rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        else:
            sys.stdout.write(rendered)
        return 0
    except EvaluationError as exc:
        print(f"task-cost-eval: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
