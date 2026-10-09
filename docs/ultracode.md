# UltraCode

UltraCode lets a session choose whether to execute directly, investigate an
uncertainty, or delegate smaller tasks that can be verified independently.
Delegation is optional. There is no agent quota, required tree shape, or promise
that a larger tree will be faster or cheaper. The parent must inspect child
changes and evidence before accepting the result.

## Activate the real mode

In the interactive UI, use `/ultracode on`, `/ultracode off`, or `/ultracode`
to toggle. For a headless prompt:

```sh
GROK_SUBAGENTS_MAX_DEPTH=2 distill --ultracode -p 'Implement the requested change and verify it.'
```

`--ultracode` also works with `--prompt-file` and `--prompt-json`. It sends
`x.ai/session/ultracode/set` with the actual session ID and `enabled: true`,
awaits an `enabled: true` acknowledgement, then sends the prompt. A rejected,
malformed, or negative acknowledgement fails the run before the prompt is sent.
The confirmed activation is written to stderr as
`UltraCode enabled for session <id>`.

New sessions default to off. The root session's mode is persisted with the
session; resuming a session can restore an earlier selection. The pager reads
the actual mode from session startup metadata after initializing its models,
so the footer and tier editor reflect the restored selection. An older server
that omits the metadata defaults the UI to off instead of inheriting the prior
session's mode. The flag does not
change the configured model, reasoning effort, permission mode, or profile
configuration. Interactive startup with `--ultracode` is rejected: use the slash
command there. An agent running over `distill agent` uses the ACP endpoint instead.

## Hierarchy and limits

The root is depth zero. A depth ceiling of one allows children but no
grandchildren; two allows a child to delegate one more level. UltraCode defaults
to two child levels and caps its ceiling at two. An explicit lower ceiling wins:
`GROK_SUBAGENTS_MAX_DEPTH=1` produces the flat comparison. The existing depth
environment variable takes precedence over `[subagents] max_depth` and the
remote setting. The runtime clamps values below one to one and ignores invalid
environment values; the comparison runner rejects requested depths below one.
Ordinary sessions retain their existing behavior.

UltraCode task children share the tree's mode and resolved ceiling. Ordinary
sessions do not inherit UltraCode and retain their existing configured depth
behavior. Internal planner/verifier agents, workflow children and children with
an explicit output-token budget remain leaves. Normal UltraCode children can
delegate within shared root concurrency, depth, cancellation and usage accounting.
There is no shared global token grant or new hard global token budget. Capability
and permission ceilings remain in force, alongside explicit model and effort
pins. A nested spawn at full global capacity returns promptly for local
fallback; it does not release the
parent's active slot. A refused delegation should lead to local execution or a
wait for independent work, rather than repeated spawning.

The model still chooses whether to delegate and how to divide work. Enabling
depth two permits a hierarchy; it does not prove that any particular task used
grandchildren. Inspect the saved session, lifecycle events and usage records
to verify the actual execution tree. Mode activation and a depth setting alone
are not evidence of completion, capability gains, or cost savings.

## Paired comparisons

Use the existing `tools.task_cost_eval.compare_parallel` runner. It accepts the
historical two-binary interface and can use the same binary for both variants.
Task prompts, input fixtures and external graders remain the same across the
pair. For off versus a hierarchy:

```sh
python3 -B -m tools.task_cost_eval.compare_parallel \
  --baseline-binary /absolute/path/to/distill \
  --candidate-binary /absolute/path/to/distill \
  --baseline-max-depth 1 --candidate-ultracode --candidate-max-depth 2 \
  --repetitions 2 --work-root /absolute/path/to/off-vs-hierarchy-runs \
  --output /absolute/path/to/off-vs-hierarchy.json
```

For flat versus hierarchical UltraCode, add `--baseline-ultracode` to that
command. For off versus flat, set `--candidate-max-depth 1` instead. Each
comparison needs a separate, empty work root. The default is a dry run;
`--execute` makes real provider calls and spends credits.

Use repeatable `--case` flags for a bounded smoke, for example
`--case par-functions-en --case seq-rename-en --repetitions 1`. A smoke does not
establish a cohort-wide result. Unknown case IDs are rejected before execution.
Pair order alternates by repetition, with the baseline first in odd repetitions
and the candidate first in even repetitions.

Both variants use the same `--model` (default `chatgpt/gpt-6.1-sol`) and optional
`--effort`. Omit effort to preserve configured auto behavior. The isolated
profile configures the worker, session-summary and utility models as
`chatgpt/gpt-6-luna`, with worker and utility effort `auto`. These are configured
tiers, not a guarantee that every call uses them. Optional external decision
services have no model credentials in this runner, so unavailable routes use
the existing native fallback; results describe that setup.

Each run saves its command, settings, stdout, stderr, grader output and session
usage. Settings record requested mode/depth/model/effort, the supplied depth
environment value, configured tiers and binary SHA-256. The runtime may clamp
the requested depth as described above; per-call `call_usage` records actual
models, roles and applied efforts from the existing ledger. Fixture, prompt,
grader and binary changes invalidate a run. UltraCode runs additionally require
the confirmed activation receipt, successful process exit and a passing external
grader. A missing activation receipt cannot pass merely because the edited
fixture passes. Temporary auth copies are removed on exit, including launch
failure and timeout.

Accounting reuses the existing all-call ledger, including parent, children,
utility calls and retries. Missing accounting prevents a passing comparison;
credit estimates are modeled usage, not an invoice. The existing verdict compares
accepted tasks, credits per accepted task and elapsed wall time, and separately
flags sequential-task delegation overhead. It does not grade general intelligence.
No UltraCode speed or cost improvement is claimed without live paired results.

Runner regression checks:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest tools.task_cost_eval.test_compare_parallel
```
