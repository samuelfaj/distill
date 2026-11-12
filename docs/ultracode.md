# UltraCode

UltraCode means extra care: higher effort and verification discipline. With
automatic effort it leans one level higher than Normal, and its directive asks
for the invariants and root cause before an edit and a check of the changed
behavior and edge cases after it. The session still chooses whether to execute
directly, investigate an uncertainty, or delegate smaller tasks that can be
verified independently. Delegation is optional. There is no agent quota, required tree shape, or promise
that a larger tree will be faster or cheaper. The parent must inspect child
changes and evidence before accepting the result.

## Activate the real mode

In the interactive UI, use `/ultracode on`, `/ultracode off`, or `/ultracode`
to toggle. Normal, UltraCode, UltraFast and UltraSave are mutually exclusive session modes;
activating one replaces the previous mode, and disabling the active mode returns to Normal.
See also [UltraFast](ultrafast.md) and [UltraSave](ultrasave.md). For a headless prompt:

```sh
GROK_SUBAGENTS_MAX_DEPTH=2 distill --ultracode -p 'Implement the requested change and verify it.'
```

`--ultracode` also works with `--prompt-file` and `--prompt-json`. It sends
`x.ai/session/execution_mode/set` with the actual session ID and
`mode: "ultracode"`, awaits a matching mode acknowledgement, then sends the prompt.
The older `x.ai/session/ultracode/set` endpoint remains available to existing clients. A rejected,
malformed, or negative acknowledgement fails the run before the prompt is sent.
The confirmed activation is written to stderr as
`UltraCode enabled for session <id>`.

New sessions default to off. The root session's mode is persisted with the
session; resuming a session can restore an earlier selection. The pager reads
the actual mode from session startup metadata after initializing its models,
so the footer and tier editor reflect the restored selection. An older server
that omits the metadata defaults the UI to off instead of inheriting the prior
session's mode. The flag does not
change the configured model, permission mode, or profile configuration. An
explicit reasoning effort stays as configured; with automatic effort, UltraCode
leans one level higher. Interactive startup with `--ultracode` is rejected: use the slash
command there. An agent running over `distill agent` uses the ACP endpoint instead.

## Hierarchy and limits

The root is depth zero. A depth ceiling of one allows children but no
grandchildren; two allows a child to delegate one more level. UltraCode defaults
to one level, like Normal, and caps its ceiling at two:
`GROK_SUBAGENTS_MAX_DEPTH=2` allows the second level. The existing depth
environment variable takes precedence over `[subagents] max_depth` and the
remote setting. The runtime clamps values below one to one and ignores invalid
environment values; the comparison runner rejects requested depths below one.
Ordinary shared-workspace sessions retain their existing behavior.

Task children share the root's exclusive mode, including when launched in
Normal, so later mode changes apply to the existing tree. Normal retains its
configured depth; UltraFast and UltraSave advertise no delegation tool and UltraCode
uses its resolved ceiling. Internal planner/verifier agents, workflow children
and children with an explicit output-token budget remain leaves. With a ceiling of
two, UltraCode children can delegate within shared root concurrency, depth,
cancellation and usage accounting. A child that cannot delegate (no spawn tool,
or already at the ceiling) gets the UltraCode directive without its delegation
sentences.
Subagents running in isolated worktrees are leaves and cannot delegate, with
UltraCode on or off; shared-workspace UltraCode hierarchy remains available
within the existing limits.
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

## Exploratory measured candidate

The restored product sources match candidate
`ca31afd6fa1bacec8bcfaf06435d9cff2586d735`, measured against baseline
`ac7d11b7a4a97c9a1ffadbe6c288ed9783259414`. The paired development study
covered four tasks with three repetitions per arm:

| Measure | Baseline | Candidate |
| --- | --- | --- |
| Approved runs | 8/12 | 9/12 |
| Raw elapsed time (seconds) | 2297.742882 | 1618.027047 |
| Modeled credits | 39.7077685 | 33.53982 |
| Modeled credits per approved run | 4.9634710625 | 3.7266466667 |

Totals include recorded runtime calls and failed runs. Separate study preparation,
orchestration and external review costs are excluded. Credits are modeled usage,
not invoice charges. Both versions failed all three
D2 ingestion runs. The overall quality verdict remains `NO_GO`; independent
confirmation remains unopened. These exploratory sample results do not establish
AGI or general superiority. Retained evaluator-only changes do not alter the
restored shipped runtime.

Evidence: `development-quality-verdict.json` and `binary-provenance-ca31afd6.json`.
