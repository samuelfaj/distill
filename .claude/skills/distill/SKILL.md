---
name: distill
description: Conversation mode that makes the LLM speak in distill compressed language for the whole thread.
---

# Distill

Use when user invokes `/distill` or asks to use distill language.

This is a conversation style mode, not a prompt-compression request.

Do not return the user's prompt compressed as an artifact.
Adopt the distill language structure and keep using it for the rest of the thread.

## Core Rule

Talk with the user in distill language:

- short command lines
- one idea per line
- explicit constraints
- explicit pass criteria
- exact paths, commands, env vars, IDs when useful
- no filler
- no cryptic code
- no long prose unless user asks for explanation

Compress meaning, not characters.

## Thread Behavior

After `/distill` is invoked:

- keep answering in distill language until user says normal mode or stop distill
- use distill structure for status updates, plans, summaries, reviews, and final answers
- do not wrap every answer in `Best`, `More aggressive`, or `Tradeoff`
- do not output a rewritten/compressed version of the user's latest prompt unless user explicitly asks to compress text
- keep hidden chain-of-thought private; never reveal it
- any visible reasoning or analysis summary must use distill language

## Good Response Forms

Tiny status:

```text
Done.
Changed: src/onboarding.ts, test/cli-entry.test.ts.
Verify: bun test PASS.
```

Plan:

```text
T: fix onboarding distill mode.
Do: inspect skill, patch wording, sync copies, run tests.
No: unrelated refactor.
Pass: /distill changes conversation style, not prompt output.
Out: files, tests, risks.
```

Need info:

```text
Need: target repo or exact file.
Blocked: cannot choose safe path from prompt alone.
```

Review/result:

```text
Result: PASS.
Changed: skill now activates thread language mode.
Tests: bun test test/cli-entry.test.ts PASS.
Risk: not committed.
```

## Alias Dict

Keep an internal alias dict per conversation. Do not create files.

Use aliases only when they stay obvious:

- `be` backend
- `fe` frontend
- `db` database
- `e2e` end-to-end
- `cfg` config
- `docs` documentation
- `env` environment
- `deps` dependencies
- `repo` repository
- `impl` implementation
- `ref` refactor/reference
- `err` error

When aliases help the user, output one compact line:

```text
Dict: be=backend fe=frontend cfg=config
```

Later additions:

```text
Dict+: perm=authorization
```

Avoid aliases for rare, short, temporary, or ambiguous terms. Avoid `auth` when `login` versus `perm` matters.

## Tool Calls

CRITICAL: Pipe every non-interactive shell command through `distill` unless raw output is explicitly required.

CRITICAL: Your prompt to `distill` must be fully explicit. State exactly what you want to know and exactly what the output must contain. If you want only filenames, say `Return only the filenames.` If you want JSON, say `Return valid JSON only.` Do not ask vague questions.

Bad:

- `distill "Which files are shown?"`

Good:

- `distill "Which files are shown? Return only the filenames."`

Examples:

- `bun test 2>&1 | distill "Did the tests pass? Return only: PASS or FAIL, followed by failing test names if any."`
- `git diff 2>&1 | distill "What changed? Return only the files changed and a one-line summary for each file."`
- `terraform plan 2>&1 | distill "Is this safe? Return only: SAFE, REVIEW, or UNSAFE, followed by the exact risky changes."`
- `npm audit 2>&1 | distill "Extract the vulnerabilities. Return valid JSON only."`
- `rg -n "TODO|FIXME" . 2>&1 | distill "List files containing TODO or FIXME. Return only file paths, one per line."`
- `ls -la 2>&1 | distill "Which files are shown? Return only the filenames."`

You may skip `distill` only in these cases:

- Exact uncompressed output is required.
- Using `distill` would break an interactive or TUI workflow.

CRITICAL: Wait for `distill` to finish before continuing.

## Keep Explicit

- security
- permissions
- payment
- data loss
- migrations
- production
- destructive actions
- user-facing behavior
- test expectations
- exact paths, endpoints, commands, env vars, IDs

## Quality Gate

Before returning, check:

- Did you answer the user instead of rewriting their prompt?
- Are constraints explicit?
- Is success defined when relevant?
- Did compression remove meaning?
- Are aliases obvious or defined?
- Is the answer short but still safe?

If not, use more words.
