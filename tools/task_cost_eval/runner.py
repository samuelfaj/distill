#!/usr/bin/env python3
"""Run one frozen task cell and record only observed artifacts.

This is intentionally a one-cell launcher, not a benchmark scheduler.  It
refuses variants whose runtime pin is incomplete and never accepts an API key
as a command-line argument.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import signal
import subprocess
import sys
import uuid
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

from .evaluate import (
    TICKS_PER_USD,
    EvaluationError,
    _sha256_file,
    _sha256_tree,
    load_cohort,
)


DEFAULT_TIMEOUT_SECONDS = 300.0
TERMINATE_GRACE_SECONDS = 5.0
SUCCESSFUL_STOP_REASONS = {"end_turn", "stop_sequence"}
UNACCOUNTED_PI_EVENTS = {
    "branch-summary",
    "branch_summary",
    "branchSummary",
    "compaction",
    "error",
}


def _required_path(value: str | Path, field: str) -> Path:
    path = Path(value).expanduser().resolve()
    if not path.exists():
        raise EvaluationError(f"{field} does not exist: {path}")
    return path


def _required_file(value: str | Path, field: str) -> Path:
    path = _required_path(value, field)
    if not path.is_file():
        raise EvaluationError(f"{field} is not a file: {path}")
    return path


def _find_session_dir(grok_home: Path, session_id: str) -> Path | None:
    sessions = grok_home / "sessions"
    if not sessions.is_dir():
        return None
    matches = sorted(path for path in sessions.rglob(session_id) if path.is_dir())
    return matches[0] if len(matches) == 1 else None


def _decimal_ticks(value: Any) -> int | None:
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        return None
    try:
        ticks = Decimal(str(value)) * Decimal(TICKS_PER_USD)
    except (InvalidOperation, ValueError):
        return None
    if not ticks.is_finite() or ticks < 0 or ticks != ticks.to_integral_value():
        return None
    return int(ticks)


def _nonnegative_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _normalize_pi_session(session_file: Path, output_dir: Path) -> Path:
    entries: list[dict[str, Any]] = []
    malformed = 0
    for line in session_file.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            malformed += 1
            continue
        if isinstance(value, dict):
            entries.append(value)
        else:
            malformed += 1

    assistants = [
        entry["message"]
        for entry in entries
        if entry.get("type") == "message"
        and isinstance(entry.get("message"), dict)
        and entry["message"].get("role") == "assistant"
    ]
    input_tokens = output_tokens = cache_read = cache_write = 0
    cost_ticks = 0
    cost_observed = False
    cost_complete = True
    usage_complete = bool(assistants) and malformed == 0
    model = effort = session_id = None
    final_stop_reason: str | None = None

    for entry in entries:
        event_type = entry.get("type")
        if event_type in UNACCOUNTED_PI_EVENTS:
            usage_complete = False
        if event_type != "message" and ("usage" in entry or "cost" in entry):
            usage_complete = False
        if event_type == "message":
            message = entry.get("message")
            if isinstance(message, dict) and message.get("role") != "assistant":
                if "usage" in message or "cost" in message:
                    usage_complete = False
        if event_type == "session" and isinstance(entry.get("id"), str):
            session_id = entry["id"]
        if event_type == "model_change":
            model = entry.get("modelId") if isinstance(entry.get("modelId"), str) else model
        if event_type == "thinking_level_change":
            effort = (
                entry.get("thinkingLevel")
                if isinstance(entry.get("thinkingLevel"), str)
                else effort
            )

    for message in assistants:
        usage = message.get("usage")
        if not isinstance(usage, dict):
            usage_complete = False
            cost_complete = False
            continue

        token_values = {
            "input": usage.get("input"),
            "output": usage.get("output"),
            "cacheRead": usage.get("cacheRead"),
            "cacheWrite": usage.get("cacheWrite"),
        }
        if not all(_nonnegative_int(value) for value in token_values.values()):
            usage_complete = False
        input_value = token_values["input"] if _nonnegative_int(token_values["input"]) else 0
        output_value = token_values["output"] if _nonnegative_int(token_values["output"]) else 0
        cache_read_value = (
            token_values["cacheRead"] if _nonnegative_int(token_values["cacheRead"]) else 0
        )
        cache_write_value = (
            token_values["cacheWrite"] if _nonnegative_int(token_values["cacheWrite"]) else 0
        )
        input_tokens += input_value + cache_read_value + cache_write_value
        output_tokens += output_value
        cache_read += cache_read_value
        cache_write += cache_write_value

        cost = usage.get("cost")
        total = cost.get("total") if isinstance(cost, dict) else None
        ticks = _decimal_ticks(total)
        if ticks is None:
            usage_complete = False
            cost_complete = False
        else:
            cost_observed = True
            cost_ticks += ticks
        stop_reason = message.get("stopReason")
        if isinstance(stop_reason, str) and stop_reason:
            final_stop_reason = stop_reason
            if stop_reason == "error":
                usage_complete = False
        else:
            usage_complete = False

        if isinstance(message.get("provider"), str) and model is None:
            model = message.get("model") if isinstance(message.get("model"), str) else model

    if final_stop_reason == "stop":
        terminal_stop_reason = "end_turn"
    elif final_stop_reason in SUCCESSFUL_STOP_REASONS:
        terminal_stop_reason = final_stop_reason
    elif final_stop_reason:
        terminal_stop_reason = final_stop_reason
    else:
        terminal_stop_reason = "error"

    output_dir.mkdir(parents=True, exist_ok=True)
    session = {
        "inputTokens": input_tokens,
        "outputTokens": output_tokens,
        "cachedReadTokens": cache_read,
        "cacheCreationTokens": cache_write,
        "reasoningTokens": 0,
        "totalTokens": input_tokens + output_tokens,
        "modelCalls": len(assistants),
        "costIsPartial": not usage_complete or not cost_complete,
        "usageIsIncomplete": not usage_complete,
        "costBasis": "pi_usage_cost_estimate",
        "costIsEstimate": True,
    }
    if cost_observed:
        session["costUsdTicks"] = cost_ticks
    usage = {
        "sessionId": session_id,
        "session": session,
        "turns": [{"turnNumber": index + 1} for index in range(len(assistants))],
    }
    (output_dir / "usage.json").write_text(json.dumps(usage), encoding="utf-8")
    (output_dir / "summary.json").write_text(
        json.dumps(
            {
                "info": {"id": session_id} if session_id else {},
                "currentModelId": model,
                "reasoningEffort": effort,
            }
        ),
        encoding="utf-8",
    )
    update = {
        "params": {
            "update": {"sessionUpdate": "turn_completed", "stopReason": terminal_stop_reason}
        }
    }
    (output_dir / "updates.jsonl").write_text(json.dumps(update) + "\n", encoding="utf-8")
    return output_dir


def _protocol_root(case: dict[str, Any]) -> Path:
    script_ref = Path(case["grader"]["script_ref"])
    script_parts = script_ref.parts
    for item in case["grader"]["command"]:
        item_path = Path(item)
        if (
            not item_path.is_absolute()
            and len(item_path.parts) >= len(script_parts)
            and item_path.parts[-len(script_parts) :] == script_parts
        ):
            prefix = item_path.parts[: -len(script_parts)]
            return Path(*prefix) if prefix else Path(".")
    return Path("tools/task_cost_eval")


def _copy_protocol_files(
    cohort: dict[str, Any], case: dict[str, Any], worktree: Path, prompt: Path
) -> Path:
    source = Path(cohort["_source"]).resolve().parent
    protocol_root = _protocol_root(case)
    fixture = source / case["fixture_ref"]
    fixture_target = worktree / protocol_root / case["fixture_ref"]
    fixture_target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(fixture, fixture_target)
    prompt_target = worktree / protocol_root / case["prompt_ref"]
    prompt_target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(prompt, prompt_target)
    return fixture_target


def _grader_source(cohort: dict[str, Any], case: dict[str, Any]) -> Path:
    source = Path(cohort["_source"]).resolve().parent
    return (source / case["grader"]["script_ref"]).resolve()


def _verify_digest(path: Path, expected: str, field: str) -> str:
    actual = _sha256_file(path)
    if actual != expected:
        raise EvaluationError(f"{field} digest does not match frozen pin: {path}")
    return actual


def _grader_command(
    cohort: dict[str, Any], case: dict[str, Any], worktree: Path
) -> tuple[list[str], Path]:
    grader_source = _grader_source(cohort, case)
    if not grader_source.is_file():
        raise EvaluationError(f"grader does not exist: {grader_source}")
    try:
        grader_source.relative_to(worktree)
    except ValueError:
        pass
    else:
        raise EvaluationError(f"grader must remain outside mutable worktree: {grader_source}")

    script_ref = Path(case["grader"]["script_ref"]).as_posix()
    command: list[str] = []
    replaced_script = False
    for item in case["grader"]["command"]:
        item_posix = Path(item).as_posix()
        if item_posix == script_ref or item_posix.endswith(f"/{script_ref}"):
            command.append(str(grader_source))
            replaced_script = True
        elif item == "{worktree}":
            command.append(str(worktree))
        else:
            command.append(item)
    if not replaced_script:
        raise EvaluationError(f"grader command does not reference {case['grader']['script_ref']}")
    return command, grader_source


def _command_for(
    variant: dict[str, Any],
    runtime: dict[str, Any],
    *,
    worktree: Path,
    prompt: Path,
    session_id: str,
    pi_session_dir: Path,
) -> list[str]:
    if variant["runner"] == "distill":
        executable = runtime["executable_path"]
        return [
            str(executable),
            "--no-leader",
            "--no-subagents",
            "--disable-web-search",
            "--cwd",
            str(worktree),
            "--session-id",
            session_id,
            "--prompt-file",
            str(prompt),
            "--output-format",
            "streaming-messages-json",
            "--model",
            runtime["model"],
            "--reasoning-effort",
            runtime["effort"],
        ]
    if variant["runner"] == "pi":
        return [
            str(runtime["executable_path"]),
            "--provider",
            runtime["provider"],
            "--model",
            runtime["model"],
            "--thinking",
            runtime["effort"],
            "--mode",
            "json",
            "--session-dir",
            str(pi_session_dir),
            "--no-context-files",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--print",
            prompt.read_text(encoding="utf-8"),
        ]
    raise EvaluationError(f"unsupported runner: {variant['runner']}")


def _terminate_owned_group(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        process.wait()
        return
    try:
        process.wait(timeout=TERMINATE_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait()


def _run_owned_process(
    command: list[str],
    *,
    cwd: Path,
    environment: dict[str, str],
    stdout: Any,
    stderr: Any,
    timeout_seconds: float,
) -> tuple[int, bool]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        stdout=stdout,
        stderr=stderr,
        start_new_session=True,
    )
    try:
        return process.wait(timeout=timeout_seconds), False
    except subprocess.TimeoutExpired:
        _terminate_owned_group(process)
        return process.returncode, True


def _timeout_seconds(args: argparse.Namespace) -> float:
    value = getattr(args, "timeout_seconds", DEFAULT_TIMEOUT_SECONDS)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise EvaluationError("timeout_seconds must be a finite number greater than zero")
    if not math.isfinite(value) or value <= 0:
        raise EvaluationError("timeout_seconds must be a finite number greater than zero")
    return float(value)


def _config_digest(args: argparse.Namespace, pin: dict[str, Any]) -> tuple[Path, str]:
    expected = pin.get("config_sha256")
    if not isinstance(expected, str):
        raise EvaluationError("pinned runtime is missing config_sha256")
    config_file = getattr(args, "config_file", None)
    if config_file is None:
        raise EvaluationError("--config-file is required for a pinned runtime")
    path = _required_file(config_file, "config_file")
    return path, _verify_digest(path, expected, "configured file")


def _apply_runtime_config(
    variant: dict[str, Any],
    config_file: Path,
    config_digest: str,
    grok_home: Path,
    pi_agent_dir: Path,
) -> Path:
    if variant["runner"] == "distill":
        target = grok_home / "config.toml"
    elif variant["runner"] == "pi":
        target = pi_agent_dir / "models.json"
    else:
        raise EvaluationError(f"unsupported runner: {variant['runner']}")
    shutil.copyfile(config_file, target)
    _verify_digest(target, config_digest, "applied runtime config")
    return target


def _effective_cli_overrides(variant: dict[str, Any], runtime: dict[str, Any]) -> dict[str, str]:
    if variant["runner"] == "distill":
        return {
            "model": runtime["model"],
            "reasoning_effort": runtime["effort"],
        }
    if variant["runner"] == "pi":
        return {
            "provider": runtime["provider"],
            "model": runtime["model"],
            "thinking": runtime["effort"],
        }
    raise EvaluationError(f"unsupported runner: {variant['runner']}")


def _post_dispatch_failure_reason(stage: str) -> str:
    return f"post_dispatch_{stage}_error"


def _last_terminal_stop(session_dir: Path) -> str | None:
    updates = session_dir / "updates.jsonl"
    if not updates.is_file():
        return None
    last: str | None = None
    for line in updates.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict):
            continue
        params = value.get("params", value)
        update = params.get("update", params) if isinstance(params, dict) else None
        if not isinstance(update, dict) or update.get("sessionUpdate") != "turn_completed":
            continue
        stop = update.get("stopReason")
        if isinstance(stop, str) and stop:
            last = stop
    return last


def _terminal_success(session_dir: Path) -> bool | None:
    stop = _last_terminal_stop(session_dir)
    if stop is None:
        return None
    return stop in SUCCESSFUL_STOP_REASONS


def _collect_artifacts(
    variant: dict[str, Any],
    environment: dict[str, str],
    session_id: str,
    pi_session_dir: Path,
    cell_dir: Path,
) -> Path:
    artifact_dir = cell_dir / "session"
    if variant["runner"] == "distill":
        session_dir = _find_session_dir(Path(environment["GROK_HOME"]), session_id)
        if session_dir is not None:
            artifact_dir = session_dir
    else:
        pi_files = sorted(pi_session_dir.rglob("*.jsonl"))
        if pi_files:
            _normalize_pi_session(pi_files[-1], artifact_dir)
    return artifact_dir


def run_one(args: argparse.Namespace) -> int:
    cohort = load_cohort(args.cohort)
    case = next((item for item in cohort["cases"] if item["id"] == args.case_id), None)
    variant = next((item for item in cohort["variants"] if item["id"] == args.variant), None)
    if case is None or variant is None:
        raise EvaluationError("case and variant must be present in the cohort")
    if args.repetition < 1 or args.repetition > cohort["_repetitions"]:
        raise EvaluationError("repetition is outside the frozen cohort")
    pin = cohort["_variant_runtime_pins"][args.variant]
    if pin.get("status") != "pinned":
        raise EvaluationError(f"variant runtime is not pinned: {args.variant}")

    run_root = _required_path(args.work_root, "work_root")
    cell_id = f"{args.case_id}--{args.variant}--{args.repetition}"
    cell_dir = run_root / cell_id
    if cell_dir.exists():
        raise EvaluationError(f"run cell already exists: {cell_dir}")

    executable_name = "pi" if variant["runner"] == "pi" else "distill"
    if args.executable:
        executable_path = Path(args.executable).expanduser().resolve()
    else:
        found = shutil.which(executable_name)
        executable_path = Path(found).resolve() if found else None
    if executable_path is None or not executable_path.is_file():
        raise EvaluationError(f"executable not found: {executable_name}")
    executable_digest = _verify_digest(
        executable_path, pin["executable_sha256"], "executable"
    )
    config_file, config_digest = _config_digest(args, pin)
    prompt_source = (Path(cohort["_source"]).resolve().parent / case["prompt_ref"]).resolve()
    planned_prompt = cell_dir / "prompt.txt"
    planned_pi_session_dir = cell_dir / "pi-sessions"
    dry_run_prompt = prompt_source if variant["runner"] == "pi" else planned_prompt
    planned_command = _command_for(
        variant,
        {**pin, "executable_path": executable_path},
        worktree=cell_dir / "worktree",
        prompt=dry_run_prompt,
        session_id="<generated-at-execution>",
        pi_session_dir=planned_pi_session_dir,
    )
    if not args.execute:
        print(
            json.dumps(
                {
                    "cell": cell_id,
                    "command": planned_command,
                    "worktree": str(cell_dir / "worktree"),
                }
            )
        )
        return 0

    timeout_seconds = _timeout_seconds(args)
    grok_home_parent = None
    if variant["runner"] == "distill":
        if not args.grok_home:
            raise EvaluationError("--grok-home is required for an isolated Distill run")
        grok_home_parent = _required_path(args.grok_home, "grok_home")
        if not grok_home_parent.is_dir():
            raise EvaluationError(f"grok_home is not a directory: {grok_home_parent}")

    cell_dir.mkdir()
    worktree = cell_dir / "worktree"
    worktree.mkdir()
    prompt = cell_dir / "prompt.txt"
    shutil.copy2(prompt_source, prompt)
    _copy_protocol_files(cohort, case, worktree, prompt_source)

    session_id = str(uuid.uuid4())
    pi_session_dir = cell_dir / "pi-sessions"
    pi_agent_dir = cell_dir / "pi-agent"
    grok_home = (grok_home_parent / cell_id) if grok_home_parent else (cell_dir / "grok-home")
    for directory in (pi_agent_dir, pi_session_dir):
        directory.mkdir(parents=True, exist_ok=False)
    if variant["runner"] == "distill":
        grok_home.mkdir(parents=True, exist_ok=False)

    runtime = {**pin, "executable_path": executable_path}
    command = _command_for(
        variant,
        runtime,
        worktree=worktree,
        prompt=prompt,
        session_id=session_id,
        pi_session_dir=pi_session_dir,
    )
    environment = os.environ.copy()
    environment["GROK_HOME"] = str(grok_home)
    environment["PI_CODING_AGENT_DIR"] = str(pi_agent_dir)
    if variant["runner"] == "distill":
        environment["GROK_JEV"] = "0" if args.variant == "distill-jev-off" else "1"

    applied_config_file = _apply_runtime_config(
        variant, config_file, config_digest, grok_home, pi_agent_dir
    )
    fixture_source = Path(cohort["_source"]).resolve().parent / case["fixture_ref"]
    fixture_digest_before = _sha256_tree(fixture_source)
    if fixture_digest_before != case["fixture_sha256"]:
        raise EvaluationError(f"fixture digest does not match frozen pin: {fixture_source}")
    grader_source = _grader_source(cohort, case)
    grader_digest_before = _verify_digest(
        grader_source, case["grader"]["sha256"], "grader"
    )

    raw_session_dir = pi_session_dir if variant["runner"] == "pi" else grok_home
    artifact_dir = cell_dir / "session"
    artifacts_collected = False
    agent_exit = None
    agent_timed_out = False
    grader_exit = None
    grader_timed_out = False
    failure_reason = None
    failure_type = None
    execution_status = "inconclusive"
    accepted: bool | None = None
    terminal_success = None
    agent_dispatched = False
    stage = "agent_dispatch"
    try:
        agent_dispatched = True
        stdout_file = cell_dir / "stdout.ndjson"
        stderr_file = cell_dir / "stderr.log"
        with stdout_file.open("w", encoding="utf-8") as stdout, stderr_file.open(
            "w", encoding="utf-8"
        ) as stderr:
            agent_exit, agent_timed_out = _run_owned_process(
                command,
                cwd=worktree,
                environment=environment,
                stdout=stdout,
                stderr=stderr,
                timeout_seconds=timeout_seconds,
            )

        stage = "agent_verification"
        _verify_digest(config_file, config_digest, "configured file")
        _verify_digest(applied_config_file, config_digest, "applied runtime config")
        if _sha256_tree(fixture_source) != fixture_digest_before:
            raise EvaluationError(f"fixture changed outside mutable worktree: {fixture_source}")
        _verify_digest(grader_source, grader_digest_before, "grader")

        stage = "artifact_collection"
        artifact_dir = _collect_artifacts(
            variant, environment, session_id, pi_session_dir, cell_dir
        )
        artifacts_collected = True
        terminal_success = _terminal_success(artifact_dir)

        if agent_timed_out:
            execution_status = "agent_failed"
            accepted = False
            failure_reason = "agent_timeout"
        elif agent_exit != 0:
            execution_status = "agent_failed"
            accepted = False
            failure_reason = "agent_process_failed"
        elif terminal_success is False:
            execution_status = "agent_failed"
            accepted = False
            failure_reason = "agent_did_not_end_turn"
        else:
            stage = "grader"
            grader_command, grader_source = _grader_command(cohort, case, worktree)
            _verify_digest(grader_source, case["grader"]["sha256"], "grader")
            grader_stdout_file = cell_dir / "grader.stdout.log"
            grader_stderr_file = cell_dir / "grader.stderr.log"
            try:
                with grader_stdout_file.open(
                    "w", encoding="utf-8"
                ) as grader_stdout, grader_stderr_file.open(
                    "w", encoding="utf-8"
                ) as grader_stderr:
                    grader_exit, grader_timed_out = _run_owned_process(
                        grader_command,
                        cwd=grader_source.parent,
                        environment=environment,
                        stdout=grader_stdout,
                        stderr=grader_stderr,
                        timeout_seconds=timeout_seconds,
                    )
            finally:
                _verify_digest(grader_source, case["grader"]["sha256"], "grader")
            if grader_timed_out:
                execution_status = "infra_failed"
                accepted = None
                grader_exit = None
                failure_reason = "grader_timeout"
            else:
                execution_status = "completed"
                accepted = grader_exit == 0
                if not accepted:
                    failure_reason = "grader_rejected"

        stage = "post_run_verification"
        _verify_digest(config_file, config_digest, "configured file")
        _verify_digest(applied_config_file, config_digest, "applied runtime config")
        if _sha256_tree(fixture_source) != fixture_digest_before:
            raise EvaluationError(f"fixture changed outside mutable worktree: {fixture_source}")
    except Exception as error:
        if not agent_dispatched:
            raise
        execution_status = "inconclusive"
        accepted = None
        grader_exit = None
        grader_timed_out = False
        failure_reason = _post_dispatch_failure_reason(stage)
        failure_type = type(error).__name__
    finally:
        if agent_dispatched and not artifacts_collected:
            try:
                artifact_dir = _collect_artifacts(
                    variant, environment, session_id, pi_session_dir, cell_dir
                )
                artifacts_collected = True
            except Exception as error:
                execution_status = "inconclusive"
                accepted = None
                grader_exit = None
                grader_timed_out = False
                if failure_reason is None:
                    failure_reason = _post_dispatch_failure_reason("artifact_collection")
                if failure_type is None:
                    failure_type = type(error).__name__

    record = {
        "run_id": cell_id,
        "case_id": args.case_id,
        "variant": args.variant,
        "repetition": args.repetition,
        "execution_status": execution_status,
        "accepted": accepted,
        "grader_id": case["grader"]["id"],
        "grader_exit_code": grader_exit,
        "agent_exit_code": agent_exit,
        "agent_timed_out": agent_timed_out,
        "grader_timed_out": grader_timed_out,
        "session_dir": str(artifact_dir),
        "raw_session_dir": str(raw_session_dir),
        "task": {
            "revision": cohort["task_base_sha"],
            "prompt_sha256": case["prompt_sha256"],
            "fixture_sha256": case["fixture_sha256"],
            "grader_sha256": case["grader"]["sha256"],
        },
        "runtime": {
            **{field: pin[field] for field in pin if field != "status"},
            "executable_sha256": executable_digest,
            "config_sha256": config_digest,
            "cli_overrides": _effective_cli_overrides(variant, runtime),
        },
    }
    if failure_reason is not None:
        record["failure_reason"] = failure_reason
    if failure_type is not None:
        record["failure_type"] = failure_type
    manifest = (
        Path(args.manifest).expanduser().resolve()
        if args.manifest
        else cell_dir / "runs.jsonl"
    )
    with manifest.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                "cell": cell_id,
                "process_exit": agent_exit,
                "grader_exit": grader_exit,
                "timed_out": agent_timed_out or grader_timed_out,
            }
        )
    )
    return 0 if execution_status == "completed" and accepted is True else 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="task-cost-runner")
    run = parser.add_subparsers(dest="command", required=True).add_parser("run-one")
    run.add_argument("--cohort", required=True, type=Path)
    run.add_argument("--case", dest="case_id", required=True)
    run.add_argument("--variant", required=True)
    run.add_argument("--repetition", required=True, type=int)
    run.add_argument("--work-root", required=True, type=Path)
    run.add_argument("--executable", type=Path)
    run.add_argument("--grok-home", type=Path)
    run.add_argument("--config-file", type=Path)
    run.add_argument("--manifest", type=Path)
    run.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    run.add_argument("--execute", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        return run_one(args)
    except (EvaluationError, OSError, subprocess.SubprocessError) as exc:
        print(f"task-cost-runner: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
