<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Code Mode (experimental)

With Code Mode on, the model gets one extra tool, `code`. Instead of calling its tools one by one, it can write a short JavaScript script that calls them: several at once, one feeding the next, with the output filtered before it reaches the model. A step that took five round trips to the model can take one, and the model reads the three lines it needed instead of the three hundred a tool printed.

Code Mode works with every model and every provider Distill supports, and with subagents. It is on by default.

---

## Turn it off

In `config.toml`:

```toml
[features]
code_mode = false
```

or for one run:

```bash
GROK_CODE_MODE=0 distill
```

In the TUI, a session that starts with Code Mode on shows `Code Mode (experimental) is on` at its top. The switch is read when a session starts, so a running session keeps the setting it started with. Set `code_mode = false` (or `GROK_CODE_MODE=0`) to turn it off; a `requirements.toml` pin wins over both.

The model keeps all its usual tools. It decides, call by call, whether a script is worth it.

---

## What a script looks like

The script is the body of an async function. Every tool the model has is a function under `tools`, taking the same arguments as a direct call and returning the text the model would have read:

```js
const [usages, tests] = await Promise.all([
  tools.grep({ pattern: "retryPolicy", path: "src" }),
  tools.grep({ pattern: "retryPolicy", path: "tests" }),
]);
const files = [...new Set(usages.split("\n").map((line) => line.split(":")[0]))];
return { files, untested: files.filter((file) => !tests.includes(file)) };
```

- `return` sends a value back to the model: a string as is, anything else as JSON. `console.log` output is returned too.
- A tool call that fails or is refused throws, so `try`/`catch` works. A command that exits non-zero counts as failed.
- A user rejection, cancellation or follow-up stops the script outside JavaScript; `try`/`catch` cannot override it.
- Images and rendered PDF pages reach the model after the script's result, while the script receives the tool's text.
- The script is plain JavaScript with nothing else attached: no `import` or `require`, no `fetch`, no timers, no file or process access. Files, commands and the network are reachable only through `tools`.

---

## Permissions

The script itself never prompts: on its own it can only compute. Each tool call it makes is handled exactly like a call the model makes directly:

- your `allow`, `ask` and `deny` rules apply to it;
- in ask mode you are prompted for it, with the same card and the same choices, and answering "No" ends the turn as usual;
- auto mode classifies it, and always-approve mode skips the prompt;
- `PreToolUse`, `PostToolUse` and `PermissionDenied` hooks run for it, under the tool's own name;
- plan mode still refuses edits outside the plan file.

Each call also appears in the session as its own tool card, so you see the edits and commands a script made, not just the script.

---

## What a script cannot call

A script can call tools that read, search, edit, run commands, fetch from the web, read memory, or reach an MCP integration. Tools that talk to you or steer the session stay with the model: questions, plan mode, subagents, background tasks, todos, goals, workflows. A script also cannot start another script.

---

## Limits

| Limit | Value |
| --- | --- |
| Tool calls per script | 200 |
| Iterations of one loop | 10,000,000 |
| Total JavaScript instructions, including Promise jobs and nested loops | 10,000,000 |
| Output returned to the model | 64 KiB, then cut with a note |

A script that fails is not rolled back. The model is told how many tool calls ran before the failure, and their effects remain.

Cancellation is checked at tool calls and between Promise jobs. Computation within a job is bounded by the total instruction limit. Queued calls from a cancelled script are dropped rather than run later.

---

## Not covered yet

- Remote (hub) workspaces do not offer the `code` tool.
- There is no benchmark evidence yet for how much Code Mode saves, or for how well weaker models write scripts. Treat it as an experiment and compare your own runs.
