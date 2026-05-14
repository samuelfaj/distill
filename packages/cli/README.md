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

Default response shape uses Military English + AR-0/AR-1 atoms with fixed prefixes:

```text
Dict: S=state C=context D=action R=risk O=outcome N=no-go P=proof
S glab auth fail gitlab.com
D inspect remotes + MR meta
R merge/update may block w/o token
```

Inline variables use dynamic `<term>=#<letter><digit>` assignments chosen by the model from repeated terms. They stay thread-local unless `distill dsl learn-thread --stdin` sees the explicit variable more than 5 times; learned entries are removed when absent from the next learned thread.

`/distill` also has DSL memory:

```bash
distill dsl show
distill dsl show --candidates
distill dsl learn --dry-run "Dict+: A1=authentication fix"
distill dsl learn-thread --stdin --dry-run < transcript.txt
distill dsl promote --dry-run
distill dsl add alias A1 "authentication bug fix" --scope project
distill dsl prune --dry-run
```

Normal runs load compact active DSL memory into the prompt. Reusable `Dict+` entries from `/distill` output are learned as project candidates using the shortest available key and can later be promoted with `distill dsl promote`.

At thread end, pipe a transcript into `distill dsl learn-thread --stdin`. It learns repeated workflow language as candidates after reviewer approval and sensitive-term filtering.

You can also pipe command output into `distill`:

```bash
bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL, followed by failing test names if any."
git diff | distill "What changed? Return only files changed and one-line summary for each."
command 2>&1 | distill -t 1000 "Return only the first actionable error."
```

Token budget controls:

```bash
distill config max-tokens 1000
DISTILL_MAX_TOKENS=2000 distill "summarize"
command 2>&1 | distill --max-tokens 1500 "summarize"
```
