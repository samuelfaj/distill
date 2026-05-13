# distill

Agent command outputs are one of the biggest sources of token waste.

Logs, test results, stack traces… thousands of tokens sent to an LLM just to answer a simple question.

**🔥 `distill` compresses command outputs into only what the LLM actually needs.**

Save **up to 99% of tokens** without losing the signal.

## How to use

Install:

```bash
npm i -g @samuelfaj/distill
```

Run onboarding:

```bash
distill
```

After onboarding you can use `/distill` in Claude/Codex to make the agent keep talking in distill language for the whole thread.

It should not return your prompt rewritten. It should adopt the language structure and keep using it.

You can also pipe command output into `distill`:

```bash
bun test 2>&1 | distill "Did tests pass? Return PASS or FAIL, followed by failing test names if any."
git diff | distill "What changed? Return only files changed and one-line summary for each."
terraform plan 2>&1 | distill "Is this safe? Return SAFE, REVIEW, or UNSAFE, followed by risky changes."
command 2>&1 | distill -t 1000 "Return only the first actionable error."
```

Tune output limits globally or per run: let distill adapt to the verbosity your agent wants.

```bash
distill config max-tokens 1000
DISTILL_MAX_TOKENS=2000 distill "summarize"
command 2>&1 | distill --max-tokens 1500 "summarize"
```

**Recommended LLM: qwen3.5-4b**

## Example

```sh
rg -n "terminal|PERMISSION|permission|Permissions|Plan|full access|default" desktop --glob '!**/node_modules/**' | distill "find where terminal and permission UI are implemented in chat screen"
```

- **Before:** [7648 tokens 30592 characters 10218 words](./examples/1/BEFORE.md)
- **After:** [99 tokens 396 characters 57 words](./examples/1/AFTER.md)
- **🔥 Saved ~98.7% tokens**
