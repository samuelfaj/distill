# distill

Agent command outputs are one of the biggest sources of token waste.

Logs, test results, stack traces… thousands of tokens sent to an LLM just to answer a simple question.

**🔥 `distill` compresses command outputs into only what the LLM actually needs.**

Save **up to 99% of tokens** without losing the signal.

## How to use

```bash
npm i -g @samuelfaj/distill
```

`distill` speaks any OpenAI-compatible API. Point it at whatever endpoint you run locally (LM Studio, Jan, LocalAI, vLLM, SGLang, llama.cpp, MLX, Docker Model Runner, Ollama's `/v1`, DFlash, etc.) or a hosted provider (OpenAI, etc.) by setting `--host` to its base URL.

Add in your global agent instructions file:

```md
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
```

## Usage

```bash
logs | distill "summarize errors"
git diff | distill "what changed?"
terraform plan 2>&1 | distill "is this safe?"
```

Point at any OpenAI-compatible endpoint:

```bash
# LM Studio
distill --host http://127.0.0.1:1234/v1 --model your-loaded-model "what failed?"

# Ollama (via its OpenAI-compatible /v1 endpoint)
distill --host http://127.0.0.1:11434/v1 --model llama3.2 "what failed?"

# OpenAI
distill --host https://api.openai.com/v1 --model gpt-4o-mini --api-key sk-... "summarize"

# Docker Model Runner
distill --host http://127.0.0.1:12434/engines/v1 --model ai/llama3.2 "what failed?"
```

## Configurations

You can persist defaults locally:

```bash
distill config host http://127.0.0.1:1234/v1
distill config model "qwen3.5:2b"
distill config api-key "secret-key-123"
distill config timeout-ms 90000
```

Environment variables override persisted config, and CLI flags override both:

- `DISTILL_HOST`
- `DISTILL_MODEL`
- `DISTILL_API_KEY`
- `DISTILL_TIMEOUT_MS`

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
- GitHub Copilot CLI supports local global instructions from `~/.copilot/copilot-instructions.md`.
- GitHub Copilot CLI also reads repository instructions from .github/copilot-instructions.md, and it can read AGENTS.md files from directories listed in COPILOT_CUSTOM_INSTRUCTIONS_DIRS.

## Example:

```sh 
rg -n "terminal|PERMISSION|permission|Permissions|Plan|full access|default" desktop --glob '!**/node_modules/**' | distill "find where terminal and permission UI are implemented in chat screen"
```

- **Before:** [7648 tokens 30592 characters 10218 words](./examples/1/BEFORE.md)
- **After:** [99 tokens 396 characters 57 words](./examples/1/AFTER.md)

**🔥 Saved ~98.7% tokens**
