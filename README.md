# distill

Agent command outputs are one of the biggest sources of token waste.

Logs, test results, stack traces… thousands of tokens sent to an LLM just to answer a simple question.

**🔥 `distill` compresses command outputs into only what the LLM actually needs.**

Save **up to 99% of tokens** without losing the signal.

## How to use

```bash
ollama pull phi3:mini
npm i -g @samuelfaj/distill
```

Add in your global agent instructions file:

```md
CRITICAL: Always when you invoke a any shell command pipe it through `distill` and tell exactly what you want to know.
`distill` will summarize the response to save tokens, which is critical for us.

Examples:
- `bun test 2>&1 | distill "did the tests pass?"`
- `git diff 2>&1 | distill "what changed?"`
- `terraform plan 2>&1 | distill "is this safe?"`
- `npm audit 2>&1 | distill "extract vulnerabilities as JSON"`

THERE ARE ONLY TWO SCENARIOS WHEN YOU ARE ALLOWED TO NOT USE `distill`:
- Do not use `distill` if exact uncompressed output is required.
- Do not use `distill` if it would break an interactive/TUI workflow.
```

## Usage

```bash
logs | distill "summarize errors"
git diff | distill "what changed?"
terraform plan 2>&1 | distill "is this safe?"
```

## Configurations

You can persist defaults locally:

```bash
distill config model "phi3:mini"
distill config timeout-ms 90000
distill config thinking false
```

For pipeline exit mirroring, use `pipefail` in your shell:

```bash
set -o pipefail
```

Interactive prompts are passed through when `distill` detects simple prompt patterns like `[y/N]` or `password:`.

## Global agent instructions

If you want Codex, Claude Code, or OpenCode to prefer `distill` whenever they run a command whose output will be sent to a paid LLM, add a global instruction telling the agent to pipe command output through `distill`.

- Codex reads global agent instructions from `~/.codex/AGENTS.md`.
- Claude Code supports global settings in `~/.claude/settings.json`, and its official mechanism for custom behavior is global instructions via `CLAUDE.md`.
- OpenCode supports global instruction files through `~/.config/opencode/opencode.json`. Point its `instructions` field at a markdown file with the same rule.

## Example:

```sh 
rg -n "terminal|PERMISSION|permission|Permissions|Plan|full access|default" desktop --glob '!**/node_modules/**' | distill "find where terminal and permission UI are implemented in chat screen"
```

- **Before:** [7648 tokens 30592 characters 10218 words](./examples/1/BEFORE.md)
- **After:** [99 tokens 396 characters 57 words](./examples/1/AFTER.md)

**🔥 Saved ~98.7% tokens**
