# @samuelfaj/distill

Install:

```bash
npm i -g @samuelfaj/distill
```

Run onboarding:

```bash
distill
```

After onboarding, use `/distill` in Claude/Codex to make the agent keep talking in distill language for the whole thread. It should adopt the language style, not return your prompt rewritten.

`/distill` also has DSL memory:

```bash
distill dsl show
distill dsl show --candidates
distill dsl learn --dry-run "Dict+: A1=authentication fix"
distill dsl promote --dry-run
distill dsl add alias A1 "authentication bug fix" --scope project
distill dsl prune --dry-run
```

Normal runs load compact active DSL memory into the prompt. Reusable `Dict+` entries from `/distill` output are learned as project candidates using the shortest available key and can later be promoted with `distill dsl promote`.

You can also pipe command output into `distill`:

```bash
bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL, followed by failing test names if any."
git diff | distill "What changed? Return only files changed and one-line summary for each."
```
