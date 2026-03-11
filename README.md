# distill

Agent command outputs are one of the biggest sources of token waste.

Logs, test results, stack traces… thousands of tokens sent to an LLM just to answer a simple question.

**🔥 `distill` compresses command outputs into only what the LLM actually needs.**

Save **up to 99% of tokens** without losing the signal.

## How to use

```bash
# Apple Silicon
python -m pip install mlx-lm

# Linux with CUDA or ROCm
python -m pip install transformers torch

# distill
npm i -g @samuelfaj/distill
```

Then just use it:

```bash
echo "3123213 test failed" | distill "o teste passou?"
git diff | distill "o que mudou?"
bun test 2>&1 | distill "os testes passaram?"
```

For `bitnet`, `distill` now starts and reuses its own local daemon automatically. You do not need to run `distill daemon` manually for normal use.

`distill` defaults to the local `bitnet` provider:
- Apple Silicon: `mlx-lm`
- Linux NVIDIA/AMD: `transformers` + `torch` with CUDA or ROCm

If you want the old Ollama flow instead, select it explicitly:

```bash
ollama pull qwen3.5:2b
distill config provider ollama
distill config model qwen3.5:2b
```

## Quick check

Use this once to confirm the local runtime is working:

```bash
distill test
```

Example successful output on Apple Silicon:

```text
Original prompt:
You are validating a local summarization model. Reply with exactly this single word and nothing else: ok

Final response:
ok

Saved 96% tokens.

token/s: 3.0

provider: bitnet
model: mlx-community/bitnet-b1.58-2B-4T
python: ok (/path/to/python3)
runtime: mlx
backend: mlx
model load: ok
generate: ok
```

## Agent setup

Add this to your global agent instructions file:

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
echo "3123213 test failed" | distill "o teste passou?"
logs | distill "summarize errors"
terraform plan 2>&1 | distill "is this safe?"
distill test
distill --provider ollama "summarize errors"
```

## Configurations

You can persist defaults locally:

```bash
distill config provider bitnet
distill config model mlx-community/bitnet-b1.58-2B-4T
distill config model "qwen3.5:2b"
distill config timeout-ms 90000
distill config thinking false
```

`provider` and `model` are persisted in the same local config file. You can always override them per command:

```bash
distill --provider bitnet --model mlx-community/bitnet-b1.58-2B-4T "summarize errors"
distill --provider ollama --model qwen3.5:2b "summarize errors"
distill test --provider bitnet --model mlx-community/bitnet-b1.58-2B-4T
```

## Runtime check

Use `distill test` to verify the configured provider, model, and runtime end to end:

```bash
distill test
distill test --provider ollama --model qwen3.5:2b
distill test --provider bitnet --model mlx-community/bitnet-b1.58-2B-4T
```

The command exits with code `0` only when the provider can run a real short generation.

## Daemon

Normal `distill` usage autostarts the daemon when needed.

You only need `distill daemon` if you want to run it manually yourself:

```bash
distill daemon
```

When the daemon is already running, `distill` and `distill test` reuse it automatically.

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
