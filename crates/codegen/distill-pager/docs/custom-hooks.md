<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Custom Hooks Guide

Hooks let you run custom scripts or HTTP requests at key moments during a Grok session, for example before or after a tool runs, when a session starts or ends, or when the agent sends a notification.

Use them for automation, safety checks, logging, notifications, and integrating with your own tools.

## Why Use Hooks?

Common use cases:

- **Safety guards**: Block dangerous commands like `rm -rf /` before they execute.
- **Audit logging**: Record every tool use or session to a file or external service.
- **Notifications**: Send a Slack/Discord message when a long-running task finishes.
- **Auto-formatting**: Run `cargo fmt` or `prettier` automatically after edits.
- **Environment setup**: Export secrets or set variables at session start.
- **Custom workflows**: Trigger builds, tests, or deployments on specific events.

## Quick Start

1. Create the hooks directory:
   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. Create a simple hook file, e.g. `~/.grok/hooks/session-start.json`:
   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
            { "type": "command", "command": "echo \"🚀 Grok session started in $(pwd)\"" }
           ]
         }
       ]
     }
   }
   ```

3. Start (or restart) a Grok session. The hook runs automatically on `SessionStart`.

   To confirm it loaded, open the Hooks tab: press `Ctrl+L` outside the VS Code family, or run `/hooks` anywhere (preferred on VS Code, Cursor, Windsurf, and Zed).

## Hook Locations

Hooks are discovered from several places (all are merged):

| Scope     | Path                              | Trusted?     | Notes |
|-----------|-----------------------------------|--------------|-------|
| Global    | `~/.grok/hooks/*.json`            | Always       | Best for personal hooks |
| Global    | `~/.claude/settings.json`         | Always       | Claude Code compatibility |
| Project   | `<project>/.grok/hooks/*.json`    | Requires trust | Per-repo automation |
| Project   | `<project>/.claude/settings.json` | Requires trust | Claude compatibility |
| Config    | `config.toml`, `managed_config.toml`, `requirements.toml` | Always | Hooks shipped in your (or your organization's) config |
| Plugin    | Bundled inside installed plugins  | Per-plugin   | Shared team hooks |

Config-file hooks use the same schema in TOML form; see the [Hooks user guide](user-guide/10-hooks.md#hooks-in-config-files) for details.

**Trusting a project**: The first time you open a project with hooks, open the hooks modal (`Ctrl+L` outside the VS Code family, or `/hooks` on any terminal) or run `/hooks-trust`. This is the same folder-trust gate as `--trust`, recorded in `~/.grok/trusted_folders.toml`. Trust prevents untrusted repos from running arbitrary code.

## The Hook JSON Format

Each `.json` file can define multiple hooks:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safety-check.sh", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "bin/log-activity.sh" }
        ]
      }
    ]
  }
}
```

Key fields:

- **Event name** (top-level key): `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `SessionEnd`, etc.
- **matcher** (optional): Regex tested against the event's match value: the tool name on tool events, and per-event values elsewhere (see the user guide's Hooks chapter). Empty matches everything.
- **type**: `"command"` (run a script or shell one-liner) or `"http"` (POST the event to a URL).
- **command**: Path to executable (relative to the JSON file) or inline shell command.
- **timeout**: Seconds before killing the hook (default: 5, or 600 for `Stop`/`SubagentStop`/`PostToolUse` gates). Hooks fail open on timeout.

**Tool name aliases**: Claude-style names like `Bash`, `Edit`, `Read` automatically match Grok's internal names (`run_terminal_cmd`, `search_replace`, `read_file`).

## Writing Hook Scripts

### Input
The full event is sent as JSON on **stdin**. Example for a `PreToolUse` hook:

```json
{
  "hookEventName": "pre_tool_use",
  "hook_event_name": "PreToolUse",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "toolName": "run_terminal_cmd",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

The `hook_event_name` (snake_case key) carries Claude's PascalCase value; `hookEventName` (camelCase key) carries grok's snake_case value.

### Output (for blocking hooks like PreToolUse)
Write JSON to **stdout**:

- Allow: `{"decision": "allow"}`
- Deny: `{"decision": "deny", "reason": "Unsafe command detected"}`

**Exit codes** (behavior differs by hook type):
- `0`: success / allow (for blocking hooks).
- `2`: explicit deny (`PreToolUse`), block-stop with stderr as feedback (`Stop`/`SubagentStop`; see Stop Decision Control in the user guide), or feedback to the model (`PostToolUse`, whose stderr reaches the model even though the tool has already run).
- Any other (including timeout, crash, or a missing env var): **fail-open**. The failure is logged and gets one line in the scrollback, but the tool call is not blocked. To block a tool call, return JSON `{"decision":"deny","reason":"..."}` on stdout.

### PostToolUse output
`PostToolUse` runs after the tool finished, so it blocks nothing, but its stdout decides what the model sees next. `{"decision":"block","reason":"..."}` feeds the reason to the model alongside the result. `hookSpecificOutput.additionalContext` adds a note. `hookSpecificOutput.updatedToolOutput` (built-in tools; must match the tool's own output shape) or `hookSpecificOutput.updatedMCPToolOutput` (MCP tools; not shape-checked) replaces the output the model reads, while the scrollback and telemetry keep the original. Every hook's block reason and context are delivered in call order, each naming its hook. Only the replacements are last-writer-wins, and a hook that exits non-zero keeps only its block reason. Output replacement is settings-file only: an SDK-registered `PostToolUse` hook can contribute a `block` reason and `additionalContext` but cannot replace the tool output. See PostToolUse Output in the user guide.

### Passive hooks
For events like `SessionStart` or `Notification`, stdout is ignored. Just exit 0 on success.

### Useful Environment Variables

Grok injects the following variables into every hook process:

- `GROK_HOOK_EVENT`: the event name (e.g. `pre_tool_use`, `session_start`, `post_tool_use`).
- `GROK_HOOK_NAME`: the full configured name of this hook.
- `GROK_SESSION_ID`: the current session identifier.
- `GROK_WORKSPACE_ROOT`: absolute path to the workspace root.

For hooks provided by plugins, the following are also set:

- `GROK_PLUGIN_ROOT`: absolute path to the plugin's installation directory.
- `GROK_PLUGIN_DATA`: absolute path to the plugin's writable data directory.

These runner- and plugin-injected variables always take precedence. Attempts to override the reserved runner keys via the `env` field are stripped at load time (with a warning logged). For plugin hooks, `GROK_PLUGIN_ROOT` and `GROK_PLUGIN_DATA` similarly override any user-supplied values for those keys.

### Custom Environment Variables (`env` field)

Each handler can declare additional env vars to inject into the child process:

```json
{
  "type": "command",
  "command": "bin/check.sh",
  "env": {
    "MY_API_TOKEN": "secret-here",
    "LOG_LEVEL": "debug"
  }
}
```

Values must be **strings**. JSON numbers and bools currently fail to parse; wrap
them in quotes if you need them.

For plugin hooks, the plugin adapter additionally injects
`GROK_PLUGIN_ROOT` and `GROK_PLUGIN_DATA`. These keys override any user-declared
values for the same names (the plugin contract is non-negotiable).

### Variable Substitution

`command` and `url` strings support `$VAR` and `${VAR}` substitution at
config-load time:

```json
{
  "type": "command",
  "command": "${HOME}/.config/grok-hooks/check.sh"
}
```

Lookup order for each reference:
1. The handler's own `env` map.
2. The current process environment (the env Grok itself sees).

If a reference is unset in both, it's **preserved verbatim** (e.g. `${UNSET}`
stays as the literal string). Runner-injected names (`CLAUDE_PROJECT_DIR`,
`GROK_WORKSPACE_ROOT`, `GROK_HOOK_EVENT`, `GROK_HOOK_NAME`,
`GROK_SESSION_ID`) are not taken from the Grok process environment at
load. Unix `sh -c` expands them from the child env; Windows PowerShell
rewrites `$VAR` to `$env:VAR`. HTTP `url` substitutes them at request
time. Remaining unresolved command refs are refused with "required env
var(s) not set".

For HTTP hooks specifically, `url` is also re-expanded **at request time**
(immediately before SSRF validation), so plugin-injected vars like
`${GROK_PLUGIN_ROOT}/check` resolve against the plugin's actual path.

#### Parameter-expansion modifiers

POSIX parameter-expansion forms are **never** expanded at load time. They are
left verbatim for the runtime `sh -c` branch to handle: `${VAR:-default}`,
`${VAR-default}`, `${VAR:=x}`, `${VAR:?msg}`, `${VAR:+x}`, `${VAR%pat}`,
`${VAR#pat}`, `${VAR/pat/repl}`, `${VAR:N:M}`. This avoids subtle divergences
between the load-time expander and POSIX shell semantics (notably, the
empty-string behaviour of `:-`).

If your hook command contains shell metacharacters (spaces, pipes, `&&`,
redirects, `$`, etc.), the runner routes it through `sh -c` and you get full
shell-expansion semantics. If your command is a bare path with no metachars,
the runner spawns it directly. Even then, `$VAR` / `${VAR}` references in the
path are still resolved at load time, so direct-exec paths like
`${HOME}/bin/check.sh` work without being wrapped in `sh -c`.

#### What is NOT expanded

- **`matcher`** is a regex (`$` is the regex anchor for end-of-line). It is
  never env-expanded. Substituting `$VAR` would silently change the regex's
  semantics and likely produce an invalid pattern. If you need a dynamic
  matcher, generate the JSON file at write time.
- **`timeout`** is numeric, so there is nothing to expand.
- **The values of the `env` map itself**: these are stored verbatim and
  passed to the child as-is, so `"BAR": "${HOME}/x"` injects the literal
  string `${HOME}/x` into the child's environment.

## Managing Hooks in the TUI

Press `Ctrl+L` outside the VS Code family (or run `/hooks` anywhere) to open the Hooks & Plugins modal.

In the **Hooks** tab you can:
- `l`: Reload all hooks.
- `a`: Add a custom hook by path (great for testing).
- `e`: Enable or disable.
- `r`: Remove.
- `Space`: Expand groups.

Hooks from `~/.grok/hooks/` appear under **Global**, project ones under **Project**, etc.

## HTTP Hooks

Instead of a local script, call a remote endpoint:

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

The full event envelope is POSTed as JSON. Useful for webhooks, analytics, or serverless functions.

## Best Practices

1. **Keep hooks fast**: long-running hooks block the UI. Use background `&` or async where possible.
2. **Use explicit `deny` to block**: hooks fail-open on any error (timeout, crash, missing env var, etc.), so a hook that crashes will not block the tool call. To enforce policy, your hook must run to completion and emit `{"decision":"deny","reason":"..."}` on stdout.
3. **Use absolute paths or paths relative to the hook file**: scripts in `bin/` next to the JSON are portable.
4. **Test with the Hooks tab**: press `Ctrl+L` outside the VS Code family, or run `/hooks`, to verify loading and matching before relying on them.
5. **Version control project hooks**: commit `.grok/hooks/` (but never secrets).

## Security Notes

- Global hooks (`~/.grok/...`) run with your user permissions. Treat them like shell scripts.
- Project hooks require explicit trust (run `/hooks-trust` or use the modal) to prevent supply-chain attacks from malicious repos.
- HTTP hooks send session data. Only use trusted endpoints.

## Troubleshooting

- **Hook not running?** Press `Ctrl+L` outside the VS Code family (or run `/hooks` anywhere) to see if it's loaded and matched.
- **Project hooks ignored?** Trust the project first.
- **Script not found?** Check the path is relative to the `.json` file and executable (`chmod +x`).
- **`The argument '/.claude/hooks/….ps1' to the -File parameter does not exist`?** PowerShell treated `$CLAUDE_PROJECT_DIR` as empty. Grok rewrites it to `$env:CLAUDE_PROJECT_DIR` unless `GROK_SHELL=cmd`.
- **See errors?** Check the pager logs (usually in the tracing pane or `~/.grok/logs`).

## More Examples

See the built-in examples in the `distill-hooks` crate:

- [Safe Shell Guard](../../../distill-hooks/examples/hooks/safe-shell.json)
- [No Recursive Grep](../../../distill-hooks/examples/hooks/no-recursive-grep.json): hard-blocks `grep -r`/`grep -R`/`rgrep` (OOM guard)
- [Session Audit Log](../../../distill-hooks/examples/hooks/session-log.json)
- [Tool Activity Logger](../../../distill-hooks/examples/hooks/tool-logger.json)

Copy them to `~/.grok/hooks/` and customize.

## Full Reference

For the complete event list, matcher semantics, trust model, and advanced details, see the [Hooks user guide](user-guide/10-hooks.md).

---

*Happy hooking!* If you build something cool, consider sharing it as a plugin.
