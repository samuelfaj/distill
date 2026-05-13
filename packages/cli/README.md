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

You can also pipe command output into `distill`:

```bash
bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL, followed by failing test names if any."
git diff | distill "What changed? Return only files changed and one-line summary for each."
```
