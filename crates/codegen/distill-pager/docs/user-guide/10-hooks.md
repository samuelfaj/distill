# Hooks

Hooks let you run a script or send an HTTP request at key moments in a Grok session. Use them to automate tasks, enforce safety checks, log activity, send notifications, and integrate your own tools.

---

## What Are Hooks?

A hook is a shell command or HTTP endpoint that Grok calls when a specific lifecycle event occurs. Hooks can:

- **Block actions**: A `PreToolUse` hook can deny a dangerous command before it runs.
- **Keep the agent working**: A `Stop` hook can block the agent from finishing its turn until a condition holds (e.g. the test suite passes) and feed the reason back to the model.
- **React to events**: A `PostToolUse` hook can log every tool execution to a file.
- **Correct a call after it ran**: A `PostToolUse` hook can tell the model what a tool result means, or replace the output the model reads — redact a secret, trim a wall of log lines — while the real result stays on the record.
- **Set up context**: A `SessionStart` hook can export environment variables or run setup scripts.

---

## Common Use Cases

- **Safety guards**: Block commands such as `rm -rf /` before they run.
- **Audit logging**: Record tool use and sessions to a file or external service.
- **Notifications**: Send a message when a task finishes.
- **Auto-formatting**: Run `cargo fmt` or `prettier` after edits.
- **Environment setup**: Export variables at session start.
- **Custom workflows**: Trigger builds, tests, or deployments on specific events.

---

## Quick Start

1. Create the hooks directory:

   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. Create a hook file, e.g. `~/.grok/hooks/session-start.json`:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
             { "type": "command", "command": "echo 'Grok session started in '$(pwd)" }
           ]
         }
       ]
     }
   }
   ```

3. Start (or restart) a Grok session. The hook runs automatically on `SessionStart`.

4. Press `Ctrl+L` on non–VS Code family terminals (or run `/hooks` anywhere — preferred on VS Code family) and check the Hooks tab to confirm it loaded.

---

## Hook Locations

Hooks are discovered from several places (all are merged):

| Scope | Path | Trusted? | Notes |
|-------|------|----------|-------|
| Global | `~/.grok/hooks/*.json` | Always | Personal hooks |
| Global | `~/.claude/settings.json` (and `settings.local.json`) | Always | Claude Code compatibility (configurable) |
| Global | `~/.cursor/hooks.json` | Always | Cursor compatibility (configurable) |
| Project | `<project>/.grok/hooks/*.json` | Requires trust | Per-repo automation |
| Project | `<project>/.claude/settings.json` (and `settings.local.json`) | Requires trust | Claude compatibility (configurable) |
| Project | `<project>/.cursor/hooks.json` | Requires trust | Cursor compatibility (configurable) |
| Config | `~/.grok/config.toml` | Always | Your hooks alongside the rest of your config |
| Config | `managed_config.toml` (`$GROK_HOME` and `/etc/grok`) | Always | Organization-distributed hooks (server-synced and on-device) |
| Config | `requirements.toml` (user and system) | Always | Organization-distributed hooks in the requirements layer |
| Plugin | Bundled inside installed plugins | Per-plugin | Shared team hooks |

Config-file hooks live in the same TOML your organization already controls; see [Hooks in Config Files](#hooks-in-config-files) for the format. The compatible vendor hook sources are scanned by default. To disable scanning for a specific vendor, set `[compat.<vendor>] hooks = false` in `~/.grok/config.toml` or the corresponding environment variable. See [Configuration](05-configuration.md#harness-compatibility) for details.

**Trusting a project**: The first time you open a project with hooks, you must trust it before its project hooks will run; until then they are silently skipped. Grant trust by running `/hooks-trust` (or launching with `--trust`); the decision is recorded in the unified folder-trust store (`~/.grok/trusted_folders.toml`), the same gate that governs repo-local MCP/LSP servers. Global hooks in `~/.grok/hooks/` are always trusted and need no entry. This prevents untrusted repos from running arbitrary code.

Because hooks are unified under folder-trust, a `--trust` / `/hooks-trust` grant trusts the whole folder for **MCP, LSP, hooks, project instructions, and project skills** together, and covers subdirectories of the same repository. A nested git checkout under that folder is a separate workspace and is not covered. Conversely, disabling folder-trust (`GROK_FOLDER_TRUST=0` or `[folder_trust] enabled = false`) ungates those surfaces together.

---

## Hook Events

Events fire at three cadences: once per session (`SessionStart`, `SessionEnd`), once per turn (`UserPromptSubmit`, `Stop`, `StopFailure`), and on every tool call inside the turn (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`).

| Event | When it fires | Blocking? |
|-------|---------------|-----------|
| `SessionStart` | A session starts. Does not fire for a subagent's own session. | No |
| `UserPromptSubmit` | You submit a prompt. | Yes: can block the prompt |
| `PreToolUse` | A tool is about to run. | Yes: can deny |
| `PostToolUse` | A tool finishes running (including a built-in logical error such as a non-zero `run_terminal_command` exit; a dispatch failure or an MCP error result fires `PostToolUseFailure` instead). | No, but it can feed the model feedback and replace the output the model sees |
| `PostToolUseFailure` | A tool fails to dispatch, or an MCP tool returns an error result. | No, but it can feed the model `additionalContext` |
| `PermissionDenied` | The permission system denies a tool call. | No |
| `Stop` | An agent turn ends on a genuine completion (an interrupt fires `StopCancelled` instead). | Yes: can block the stop |
| `StopFailure` | A turn ends because of an API error. | No |
| `StopCancelled` | Runs instead of `Stop` when a turn ends without completing: a user interrupt (Ctrl+C / a client stop), a declined permission prompt, the `--max-turns` limit, or a no-progress bail-out. | No |
| `Notification` | User-attention events (`idle_prompt`, `permission_prompt`, `task_complete`, …). | No |
| `SubagentStart` | A subagent starts. | No |
| `SubagentStop` | A subagent's turn ends (fires once, in the subagent, with stop decision control). | Yes: can block the stop |
| `PreCompact` | Conversation compaction is about to run. | No |
| `PostCompact` | Conversation compaction completes. | No |
| `SessionEnd` | The session ends. Carries `subagentType` for a child session, so a host can tell a child's teardown from its own. | No |

`SubagentEnd` is accepted as an alias for `SubagentStop`. `PreToolUse` can block a tool call, `UserPromptSubmit` can block a prompt (see below), and `Stop`/`SubagentStop` can block the agent from stopping (see [Stop Decision Control](#stop-decision-control)). `PostToolUse` runs too late to block anything, but its stdout is read: it can feed the model feedback and replace the tool output the model sees (see [PostToolUse Output](#posttooluse-output)). Every other event is passive.

### UserPromptSubmit Decision Control

A `UserPromptSubmit` hook can reject a prompt: exit 2 blocks (stderr becomes the message), and so does JSON `{"decision": "block", "reason": "..."}` on stdout, on any exit code. The reason is shown to you and is never added to the model's context. Only a prompt you typed can be blocked: auto-wake turns (task and subagent completions, scheduler fires) and subagent sessions run the hook observe-only. The default timeout for this event is 30 seconds; a timed-out or crashed hook fails open and the prompt proceeds.

After a block, prompts already queued behind the blocked one do not auto-run: the queue holds until you act (send a prompt, or edit / remove / reorder / force-run a queued row). A blocked prompt is not recorded: it never enters the conversation history the model sees on later turns, the on-disk session record, or the session summary. It stays visible in the live scrollback and is held at the front of the queue for you to edit, resend, or discard — but after a session restart the blocked bubble is gone from the scrollback, exactly because nothing was stored. One deliberate exception: your client's local prompt history (the up-arrow recall) keeps the text, recorded at submit time before the hook runs, so a discarded prompt is still recoverable. One current limit: stdout of an allowing hook is discarded (no `additionalContext`).

### Cursor Hook Compatibility

Grok accepts Cursor's camelCase hook event names, so `~/.cursor/hooks.json` loads unchanged:

| Cursor event | Maps to |
|---|---|
| `sessionStart`, `sessionEnd` | `SessionStart`, `SessionEnd` |
| `preToolUse`, `postToolUse`, `postToolUseFailure` | `PreToolUse`, `PostToolUse`, `PostToolUseFailure` |
| `beforeShellExecution`, `beforeMCPExecution`, `beforeReadFile` | `PreToolUse` |
| `afterShellExecution`, `afterMCPExecution`, `afterFileEdit` | `PostToolUse` |
| `afterAgentResponse`, `afterAgentThought` | `PostToolUse` |
| `beforeSubmitPrompt` | `UserPromptSubmit` |
| `subagentStart`, `subagentStop` | `SubagentStart`, `SubagentStop` |
| `preCompact`, `stop` | `PreCompact`, `Stop` |

Cursor's per-operation hooks (`beforeShellExecution`, `afterFileEdit`, etc.) map to the generic `PreToolUse`/`PostToolUse` events. The hook script receives the tool name in the JSON input and can filter accordingly, or use the `matcher` field.

---

## The Hook JSON Format

Each `.json` file can define hooks for multiple events:

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

### Key Fields

- **Event name** (top-level key): any event listed in [Hook Events](#hook-events). Grok skips unrecognized event names so a shared Claude or Cursor settings file still loads.
- **matcher** (optional): A regular expression that selects which invocations trigger the hook. What it tests depends on the event: the tool name on tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`), the notification type on `Notification`, the subagent type on `SubagentStart`/`SubagentStop` (e.g. `explore`), the start source on `SessionStart` (`startup`, `resume`, …), the end reason on `SessionEnd`, the compaction trigger on `PreCompact`/`PostCompact` (`manual` or `auto`), the error type on `StopFailure` (`rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, or `unknown`), and the reason on `StopCancelled` (`user_interrupt`, `permission_rejected`, `permission_cancelled`, `max_turns`, `no_progress`, or `unknown`). A matcher on `Stop` or `UserPromptSubmit` is ignored with a warning (those events always fire). An empty or omitted matcher matches everything. A finish-thinking chime should set `matcher` to `idle_prompt` on `Notification` (any turn end, then sustained idle); `permission_prompt` fires only when a permission UI is actually waiting. The matcher tests the real tool name; MCP calls routed through the internal `use_tool` dispatcher appear as the qualified `server__tool` name (e.g. `linear__save_issue`), so match on that, not the dispatcher name.
- **type**: `"command"` (run a script or shell one-liner) or `"http"` (POST the event to a URL).
- **command**: Path to executable (relative to the JSON file) or inline shell command.
- **timeout**: Seconds before killing the hook (default: 5, or 600 for `Stop`/`SubagentStop`/`PostToolUse` gates). All hook failures (timeouts, crashes, malformed output, missing required env vars) are fail-open: the failure is recorded for the UI scrollback but the tool call is not blocked. Only an explicit `deny` decision returned by the hook blocks a tool call.

### Tool Name Aliases

In a `matcher`, Grok maps Claude-style tool names to its own so hooks migrated from Claude fire correctly. Common aliases include:

- `Bash` → `run_terminal_command`
- `Read` → `read_file`
- `Edit`, `Write`, and `MultiEdit` → `search_replace`
- `Grep` → `grep`
- `Glob` and `ListDir` → `list_dir`
- `WebSearch` → `web_search`
- `Task` → `spawn_subagent`

A matcher keeps its original name too, so `Bash` matches both `Bash` and `run_terminal_command`.

---

## How a Hook Resolves

When an event fires, Grok resolves it in four steps:

1. **Select matching groups.** For that event, each matcher group whose `matcher` matches the event's field runs. The matcher tests the tool name on tool events, the notification type on `Notification`, and so on (see [Key Fields](#key-fields)). An empty or omitted matcher matches everything.
2. **Run the handlers in order.** Handlers in the selected groups run in config order, each receiving the event as JSON on stdin, until one returns `deny` (which stops the chain). Handlers from different sources (global, project, plugin, config) are merged, and identical handlers are deduplicated. Every handler sees the model's original tool input; a `PreToolUse` `updatedInput` is applied only after all handlers finish, so one handler cannot see another's rewrite (the last rewrite wins).
3. **Apply the decision.** For a `PreToolUse` gate, the first `deny` blocks the call and its reason is shown to the model, an `updatedInput` rewrites the tool input, and otherwise the call proceeds. For `Stop` and `SubagentStop`, a `block` keeps the agent working. For `PostToolUse` the tool has already run, so nothing is blocked and every hook runs: a `block` reason and any `additionalContext` are delivered to the model with the tool result, and an output replacement rewrites the model's copy of that result. Every other event is passive: its output is recorded but does not change control flow.
4. **Fail open.** A handler that times out, crashes, or emits malformed output is recorded in the scrollback but never blocks the action. The one exception is a `PreToolUse` `updatedInput` that fails the tool's schema: the rewrite cannot run safely, so the call is blocked and reported as an invalid-input error. Otherwise only an explicit `deny` blocks a tool call.

---

## Hooks in Config Files

Hooks can also live directly in your Grok config, so a team can distribute them with the rest of their configuration instead of shipping separate JSON files. The same `hooks` object is read from three TOML files:

| File | Tier | Who sets it |
|------|------|-------------|
| `~/.grok/config.toml` | User | You |
| `managed_config.toml` (`$GROK_HOME`, `/etc/grok`) | Managed / system | Your organization |
| `requirements.toml` (user and system) | Requirements | Your organization |

The TOML is structurally identical to the JSON hook object, so an existing hook transliterates directly:

```toml
[[hooks.PreToolUse]]
matcher = "Bash|Write|Edit"
hooks = [
  { type = "command", command = "/opt/guard/pretooluse.sh", timeout = 10 },
]
```

Each matcher group is a `[[hooks.<Event>]]` entry with an optional `matcher` and an inner `hooks` array of handlers. The handler fields (`type`, `command`, `url`, `timeout`, `env`) and event names are exactly the same as the [JSON format](#the-hook-json-format).

TOML offers two equivalent notations for the inner handlers, and both parse to the identical structure. The inline-table array shown above is recommended: it reads best for the common single-handler case. The nested array-of-tables form is also accepted:

```toml
[[hooks.PreToolUse]]
matcher = "Bash|Write|Edit"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "/opt/guard/pretooluse.sh"
timeout = 10
```

Prefer the inline form to avoid repeating the `[[hooks.<Event>.hooks]]` header for each handler.

- **Additive across layers.** Every layer's hooks run; a lower-priority layer adds hooks but never replaces another layer's block. A hook defined identically in more than one layer is deduplicated, keeping the highest-authority copy.
- **Provenance labels.** Config hooks appear in `/hooks` tagged by origin (`managed:`, `requirements/user:`, `user:`, and so on) so you can see which layer contributed each one.
- **No read-time expansion.** A literal `${VAR}` in a `command` or `url` reaches the hook runner unchanged, matching JSON hook-file semantics; the runner performs the single expansion.

---

## Writing Hook Scripts

### Input

The event is sent as JSON on **stdin** (for example, a `PreToolUse` event; the payload also always includes `toolUseId` and `toolInputTruncated`):

```json
{
  "hookEventName": "pre_tool_use",
  "hook_event_name": "PreToolUse",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "permissionMode": "default",
  "toolName": "run_terminal_command",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

Every event carries the same common fields: `hookEventName`, `sessionId`, `cwd`, `workspaceRoot`, `timestamp`, `permissionMode` (`default`, `auto`, `plan`, or `bypassPermissions`), and `promptId` (the turn the event belongs to; absent for session-scoped events), plus event-specific fields like `toolName` above. The `hook_event_name` (snake_case key) carries Claude's PascalCase value; `hookEventName` (camelCase key) carries grok's snake_case value.

### Output (Blocking Hooks)

For `PreToolUse` hooks, write JSON to **stdout**:

- **Allow**: `{"decision": "allow"}`
- **Deny**: `{"decision": "deny", "reason": "Unsafe command detected"}`
- **Ask the user**: `{"decision": "ask", "reason": "Confirm this deploy"}`
- **State no opinion**: `{"decision": "defer"}`
- **Rewrite the tool input**: `{"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": {"command": "npm test"}}}`
- **Tell the model something**: `{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "This repo builds with xb, not cargo"}}`

The decision can be written as top-level `decision` or as `hookSpecificOutput.permissionDecision`. Both take `allow`, `deny`, `ask`, or `defer` (the legacy `approve` and `block` spellings also work), each with its own reason field — `reason` and `permissionDecisionReason`. The canonical `permissionDecision` decides when present; the top-level `decision` applies only when it is absent. The deny or ask message is `permissionDecisionReason` if present, otherwise `reason`. An `allow` means only "not blocked" — it does not auto-approve a call the user would otherwise be asked about. A decision value outside that set is a hook failure, which fails open unless the hook also exits 2, in which case the deny stands and carries the mistake in its reason.

An `ask` makes the call reach the permission prompt: nothing that would otherwise approve it without asking — always-approve mode, auto mode, a saved "always allow" grant, a safe command — applies, and the prompt names your hook and shows your reason. There is never a second prompt: where you would have been asked anyway, the ask only re-labels that one. Approving runs it; rejecting blocks it as an ordinary permission rejection. A client running in full always-approve/YOLO mode (auto-answering every prompt) will still auto-approve the call, matching Claude Code's `bypassPermissions`: the ask overrides the manager's always-approve, auto, saved-grant, and safe-command paths, not a client that blanket-approves every prompt.

An `ask` cannot widen anything, so a permission policy deny, an auto-mode block, or plan mode still decides the call. In auto mode a hook `ask` still runs the classifier before the prompt appears: the classifier may deny the call, but it can never silently approve one the hook asked about. `dontAsk` mode denies whatever it would have to prompt for, so there an ask turns an otherwise-approved call into a denial.

Only hooks configured in a settings file (command and HTTP hooks) can ask, defer, or send `additionalContext`: a `PreToolUse` hook registered through the grok-agent-sdk can allow or deny, and the rest is dropped — an `ask` or a `defer` there leaves the call to the normal permission flow and is logged as an unrecognized decision, and `additionalContext` never reaches the model.

`updatedInput` replaces the tool's input before it runs, silently: the model is not told and nothing is written to the scrollback, so the only sign of a rewrite is the rewritten arguments themselves, which the user sees if the call reaches a permission prompt. The plan-mode gate, the permission prompt, the tool itself, and the later `PostToolUse` payload all see the rewritten input, so a hook can normalize or harden a call rather than only allow or deny it. Hooks run before the plan-mode gate, so a hook with side effects fires even when plan mode later rejects the call.

The value must be a JSON object; a non-object fails the hook. If the rewritten input fails the tool's schema, the call is blocked as a hook denial — the scrollback annotation names the hook — rather than falling back to the original. A rewrite may change a call's arguments but not which tool runs, so one that retargets a `use_tool` call is blocked too. A hook that exits non-zero keeps its `deny` but loses its `updatedInput` and its `additionalContext`.

A `deny` discards any `updatedInput`; when several hooks return one, the last wins. Omitting `decision` while returning `updatedInput` allows the call and applies the rewrite.

A `defer` neither blocks the call nor approves it: the call takes the normal permission flow, exactly as if your hook had not answered, and a warning naming the hook goes to the log. It also acts on nothing else you sent — an `updatedInput` or `additionalContext` next to a `defer` is ignored and named in the log. Across hooks `defer` ranks below `ask`, so where one of your hooks defers and another asks, grok prompts.

`additionalContext` is a note for the model. It arrives after the call has run — never before — with the results of the batch the call belongs to, wrapped in your harness's reminder tag (`<system-reminder>` by default) and naming the hook that wrote it, so the model can tell your text from the user's. Every hook that sends one is delivered, in the order the hooks ran (unlike `updatedInput`, where the last writer wins). A `deny` drops all of it, since the call never runs, and names the drop in the log. Text over 10,000 characters is clipped, the same ceiling `Stop` feedback carries.

### PostToolUse Output

`PostToolUse` runs after the tool finished, so it blocks nothing. Its stdout is still read, because it decides what the model sees next. Write JSON to **stdout**:

```json
{
  "decision": "block",
  "reason": "The diff still contains a debug print",
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "This file is generated; edit the template instead",
    "updatedToolOutput": { "type": "Bash", "command": "…", "exit_code": 0, "output_for_prompt": "[redacted]" }
  }
}
```

| Field | Effect |
|-------|--------|
| `decision: "block"` + `reason` | Delivers `reason` to the model next to the tool result. The tool's own output still arrives; "block" means "tell the model something went wrong", not "stop the call". |
| `additionalContext` | Adds a note for the model next to the tool result. |
| `updatedToolOutput` | Replaces the model's copy of the result. Universal key; works for every tool. |
| `updatedMCPToolOutput` | MCP-only alias for `updatedToolOutput`. Ignored on a built-in tool. |

- **Delivery.** The block reason and `additionalContext` arrive after the tool result, wrapped in your harness's reminder tag and naming the hook that wrote them, so the model can act in the same turn. Every hook's block reason and `additionalContext` are delivered in the order the hooks ran, so one hook's finding cannot drop another's. Only replacements are last-writer-wins: when two hooks return one, the last survives and the drop is named in the log.
- **Building `updatedToolOutput`.** On a built-in tool it must carry grok's own output shape for the tool that ran, a tagged object such as `{"type": "Bash", …}`. Take the `toolResult` the event handed you, edit it, and send it back — that is exactly the shape it is validated against. A replacement that fails to parse or parses as another tool's output is ignored and the original stands, but the hook's run is recorded `Failed` with the reason, so an exit-0 hook that shows "failed" is reporting a dropped replacement, not that it never ran. A mistyped `decision` (only `"block"` is honored) is reported the same way. Check `toolResultTruncated` first: an oversized payload reaches the hook as a plain string and cannot be echoed back.
- **MCP tools.** There is no shape to enforce, so both `updatedToolOutput` and `updatedMCPToolOutput` pass through un-checked — a JSON string becomes the model-facing text verbatim, any other value is serialized — and the last hook to write wins across both keys.
- **Caps.** The block reason and `additionalContext` are clipped at 10,000 characters, the ceiling `Stop` feedback and `PreToolUse` context share. A replacement gets 64 K characters. Caps are measured on the rendered model-facing text and applied once the replacement has rendered, so a long `updatedToolOutput` is clipped like a string, not dropped for its size. A structured replacement is dropped only when it does not match the tool's own output shape.
- **Broken hook.** A non-zero exit — exit 2 included — keeps the block reason and drops everything else: `additionalContext` and the replacement are dropped and the drop is named in the log, the same rule `PreToolUse` applies to `updatedInput`. The block is the fail-safe direction.
- **Record vs. model.** A replacement rewrites only the model's copy. The scrollback, transcript, and telemetry keep the original, so redacting a secret hides it from the model, not from you, and a failure rewritten as success stays real on the record. Images are not delivered under a replacement, so a replaced screenshot or PDF read reaches the model as your text alone. Everything a hook sends (note, block reason, replacement) is escaped so it cannot close the reminder tag and pose as harness or user authored instruction.
- **Output replacement is settings-file only.** Command and HTTP hooks can do all of this. A `PostToolUse` hook registered through the grok-agent-sdk can contribute a `block` reason and `additionalContext`, but cannot replace the tool output.
- **When it fires.** `PostToolUse` fires for every tool that actually ran, including one whose result is a built-in logical error such as a non-zero `run_terminal_command` exit. A tool that failed to dispatch, or an MCP tool that returned an error result, fires `PostToolUseFailure` instead — context-only: it can feed the model `additionalContext` but cannot block or replace the output. The hook inherits the 600-second gate default (it commonly runs a linter or test); set `timeout` explicitly only when the check needs longer or shorter. A timed-out hook is recorded as a failure and contributes nothing.

### Exit Codes

| Exit Code | Meaning |
|-----------|---------|
| `0` | Success / allow (for blocking hooks) |
| `2` | Explicit deny (`PreToolUse`), block-stop with stderr as feedback (`Stop`/`SubagentStop`), or feedback to the model (`PostToolUse`). For `PreToolUse`, the first stderr line (capped) becomes the deny reason when the JSON carries none; `Stop`/`SubagentStop` and `PostToolUse` feed the full stderr to the model, and a JSON `reason` wins over it. |
| Other | Fail-open — the failure is recorded (as `exit code N: <first stderr line>`) but nothing is blocked. For `PreToolUse`, a `deny` decision in stdout JSON is honored regardless of exit code. For `Stop`/`SubagentStop`, a valid decision JSON on stdout wins over the exit code; the exit code decides only when stdout has no usable JSON, in which case exit 2 blocks with stderr as the feedback. For `PostToolUse`, the tool has already run so nothing is blocked either way; the failure is still recorded, and the hook keeps its block reason but loses its `additionalContext` and its output replacement. |

**`PostToolUse` exit 2 is a behavior change.** It used to be an ordinary recorded failure that changed nothing; it now feeds the hook's stderr to the model. A logging hook written as `run_checker; exit $?` therefore hands the model whatever the checker printed whenever the checker exits 2 — `mypy`, `grep`, `pytest` and `argparse` all use exit 2 for "no match" or "bad usage". End such a hook with an explicit `exit 0` to keep it silent.

Write human-readable diagnostics to **stderr**: it is the hook's feedback channel. On failures the first stderr line appears in the scrollback entry and logs instead of a bare exit code.

### Stop Decision Control

`Stop` and `SubagentStop` hooks run when the agent is about to finish its turn and can keep it working (Claude Code-compatible). Write JSON to **stdout**:

- **Block the stop**: `{"decision": "block", "reason": "The test suite hasn't been run yet"}`. The reason is fed back to the model as a user message and the agent runs another round in the same turn.
- **Non-error feedback**: `{"hookSpecificOutput": {"hookEventName": "Stop", "additionalContext": "Run the linter before finishing"}}`. Also keeps the agent working, but is surfaced as hook feedback rather than a hook error.
- **Force stop**: `{"continue": false, "stopReason": "Budget exhausted"}`. Ends the turn, overriding any blocks.
- **Allow the stop**: exit 0 with no output (or any non-JSON output).

Exiting with code `2` also blocks the stop, with **stderr** as the feedback.

The hook input includes `stopHookActive` and `lastAssistantMessage`. `stopHookActive` is true when the agent is already continuing due to a previous stop-hook block this turn; check it, or the transcript, to avoid blocking on a condition that will never resolve. `lastAssistantMessage` carries the text of the agent's final response this turn, so hooks can act on it without parsing the transcript. Every event carrying this field clips it at 32,768 characters, with the same `… [+N chars]` marker as the other free-text fields. It is far looser than the 1,000 applied to `errorDetails` and friends because it carries a whole answer rather than a label, and it is sized to the same scale as the tool payload cap. After **8 continuations** (blocks or non-error feedback) in one turn the gate is overridden and the turn ends; hooks are not consulted for that final, forced stop. The counter is per turn: the next user prompt starts fresh, so a long-running goal can span turns. Hook failures fail open: the agent stops normally.

`Stop`, `SubagentStop`, and `PostToolUse` hooks default to a 600-second timeout because these gates commonly run builds or test suites, and a timed-out hook fails open, so the check does not block anyway. Every other event keeps the 5-second default. Set `timeout` explicitly when a gate needs more: `{ "type": "command", "command": "bin/verify.sh", "timeout": 1200 }`.

The gate runs only for genuine completions. A turn that was interrupted (Ctrl+C), refused, or cut off at the turn limit skips the Stop gate, though a Ctrl+C that lands while a Stop hook is already running kills it mid-flight (see below); API-error turns fire `StopFailure`, and cancelled turns fire `StopCancelled`. `Esc` never cancels a running turn. A separate Stop also fires at session end (`reason: "channel_closed"` or `"shutdown"`); its decision output is parsed but ignored, since there is no turn left to continue. A script that counts or gates on Stop fires should check `reason == "end_turn"` so the session-end fire doesn't skew it.

`StopFailure` is observation-only (use it to log failures or send alerts; output and exit code are ignored). Its input carries `error` (the classified type the matcher tests: `rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, or `unknown` for anything the runtime cannot distinguish; capacity errors classify as `rate_limit`), `errorDetails` (the raw error detail, when available, clipped at 1000 characters; absent for a refusal, whose explanation rides `lastAssistantMessage` alone), `lastAssistantMessage` (the rendered error text shown in the conversation; for this event it is the error string, not assistant output), and `subagentType` (the subagent's type when the turn ran inside one).

`StopCancelled` is observation-only too. **It runs instead of `Stop` when the turn ends without completing**, the same way `StopFailure` runs instead of `Stop` on an API error.

A turn reports **at most one** of the three, with one exception noted below: a `Stop` hook that ran to completion can still be followed by `StopCancelled` if the user interrupts during the gate, because by then the hook has already been told the turn ended. Every turn that runs the model and then ends, errors, or is cancelled reports one, except for the cases listed below.

If your host must never miss an idle transition, listen for the `idle_prompt` `Notification` as well. It covers every exception in which the session is still alive, with one gap: a session whose only activity was a bash-mode command that ran to completion earns neither the report nor the ping, though interrupting one earns both. `SessionEnd` covers teardown. The `idle_prompt` ping fires about a minute after the session settles, needs at least one turn to have ended, and is cancelled if you send another message first.

A cancelled turn's report is dispatched off the session's command loop, so an interrupt is never delayed by your hook. The report can therefore arrive **after** the next turn's `UserPromptSubmit`, and turn-end reports are not ordered against each other across paths.

A script that tracks busy and idle should key on `promptId`, following the rules below. grok mints one per turn, but a client that supplies its own in `_meta` owns its uniqueness, so treat the id as opaque and scope it to the session.

Every turn-end report goes through one worker, so a slow hook delays the next report but never the turn it belongs to. Keep observe-hook timeouts short.

An interrupt while a `Stop` hook is running kills that hook mid-flight, and the turn then reports `StopCancelled`: a `Stop` hook that started is not a promise the turn completed. A `StopFailure` hook runs off the turn, so an interrupt cannot kill it, and that turn has already reported, so no `StopCancelled` follows.

`Stop` is a gate, so when a stop hook blocks it fires again for each continuation round; only the fire that lets the turn end is the report, and a turn that ends cancelled or failed after a blocked `Stop` reports that instead. A passive observer cannot tell a continuation fire from the final one (`stopHookActive` is true for both), so a UI gated on `Stop` alone shows a false idle from the first continuation fire until the user's next prompt, since no `UserPromptSubmit` marks a continuation round. Leave `Stop` out of the state script when you also run a blocking gate, and settle on the `idle_prompt` `Notification` instead.

Some turns report none of the three:

- bash mode (`!`) and builtin slash commands that run to completion. Interrupting one still reports `user_interrupt`, without a preceding `UserPromptSubmit`.
- a cancel-and-send, a rewind, or a queued prompt removed before it ran.
- session teardown, reported by the session-end `Stop` and `SessionEnd`.
- a turn whose stop hooks kept the agent working until the per-turn continuation limit forced the stop.
- a turn where no stop hook ran to completion, because they were all disabled, untrusted, or failed, or, in a subagent, because their matchers all missed. Leaving the turn unreported is deliberate: a later cancel or failure can then report it.
- a turn superseded by the next one while its report was still being built.
- a report still queued when the session exits: teardown waits half a second for queued turn-end hooks, then drops what is left and aborts any hook still running.
- a turn that completed, ran its `Stop`, and only then failed to write to disk: it reported `Stop`, so no `StopFailure` follows. The failure still surfaces in the conversation.

`StopCancelled`'s input carries:

- `reason`: the classified cause, and the value the matcher tests. `user_interrupt` (Ctrl+C, a client stop button, or a client `session/cancel`), `permission_rejected` (you declined a tool call), `permission_cancelled` (you dismissed the prompt), `max_turns`, `no_progress` (the agent bailed out after repeated no-op rounds), or `unknown` (a cancel the runtime could not classify, and the forward-compatible fallback). The matcher tests this field only, so a hook that wants every user-initiated stop matches the reasons it cares about and reads `cancelledBy` from the payload. New reasons may be added over time, so treat an unrecognized value the way you treat `unknown`.
- `cancelledBy`: `user` for an interrupt, a declined tool call, or a dismissed prompt; `runtime` for everything the agent decided itself, such as `max_turns` and `no_progress`; `unknown` when `reason` is `unknown`, because a cancel the runtime could not classify cannot claim the user was uninvolved. Derived from `reason`, so a new reason classifies automatically. Values may be added here too: treat one you do not recognize the way you treat `unknown`, rather than assuming anything that is not `user` was the runtime.
- `cancelTrigger`: the gesture, when the client named one, clipped at 64 characters, since a gesture name is a token. The bundled pager sends one of three: `ctrl_c`, `mouse` (the on-screen stop button), or `dashboard_stop`. It never sends `esc` (Esc does not cancel a turn). Another client may send any string, including `esc`, and it is passed through verbatim. Every value here classifies as `user_interrupt`, including one that happens to spell an internal name such as `shutdown`, because a client asking to cancel is the user asking; read `cancelledBy` from the payload rather than parsing this string. Omitted for a bare `session/cancel` and for every runtime-initiated reason.
- `reasonDetails`: the same kind of detail `StopFailure` puts in `errorDetails`, when the runtime has one. For a declined tool call it is `<tool>: <why>`. Clipped at 1000 characters, like `StopFailure`'s `errorDetails`.
- `lastAssistantMessage`: whatever the turn had committed to the conversation at the interrupt, if any. A Ctrl+C during the final answer leaves the last committed text, or nothing if the turn never committed any. Clipped like the same field on `Stop` and `StopFailure`.
- `subagentType`: the subagent's type when the turn ran inside one, so a hook can tell a nested agent's stop from the session's. Absent in the main session.

The envelope's `timestamp` is stamped when the hook dispatches, not when the turn ended. The three turn-end events queue behind one worker, so a report waiting on a slow hook ahead of it carries a later timestamp than the moment it describes. Use `promptId` to correlate, not the clock.

`StopCancelled` cannot block: the turn is already over, and letting a hook reopen a turn the user deliberately stopped would fight the user. Use `Stop` when you want to keep the agent working.

A "cancel-and-send" (typing a new message while a turn runs) does **not** fire `StopCancelled`, because the turn is being replaced, not stopped, and the agent stays busy. Inside a subagent, a `user_interrupt` does not fire either: it follows the parent's cancel, and the session-level signal is the useful one. A subagent's own `max_turns`, `no_progress`, or declined permission does fire. The matcher tests `reason` only, so a script that reports whether the session is idle should exit early when `subagentType` is present.

A complete busy and idle indicator takes five registrations. `UserPromptSubmit` marks the session busy;
`Stop`, `StopFailure`, and `StopCancelled` settle it however the turn ended; the `idle_prompt`
`Notification` is the backstop for the turns that report none of the three. Registering only
`StopCancelled` leaves the host busy after every normal turn.

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "bin/turn-started.sh" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "bin/turn-ended.sh", "timeout": 10 }] }],
    "StopFailure": [{ "hooks": [{ "type": "command", "command": "bin/turn-ended.sh" }] }],
    "StopCancelled": [{ "hooks": [{ "type": "command", "command": "bin/turn-ended.sh" }] }],
    "Notification": [
      { "matcher": "idle_prompt", "hooks": [{ "type": "command", "command": "bin/turn-ended.sh" }] }
    ]
  }
}
```

What the two scripts have to get right:

- **Track the newest `promptId` and ignore reports for older turns.** A cancelled turn's report is
  dispatched off the command loop, so it can arrive after the next turn's `UserPromptSubmit`.
- **Settle unconditionally when there is no `promptId`.** That is grok reporting on the session
  rather than a turn: the `idle_prompt` ping and the session-end `Stop`. It is what makes the
  backstop work for a rewind or a superseded turn, which report nothing.
- **Treat a `promptId` you never saw start as idle.** An interrupted bash-mode turn reports without
  a preceding `UserPromptSubmit`.
- **Exit early when `subagentType` is present.** A subagent's stop is not the session's.
- **Settle the host before you record the turn as handled,** so a hook killed mid-flight leaves the
  turn correctable. Re-read that record first, so you only clear a turn you recorded yourself.
- **Keep it to a local write.** Teardown gives the whole queue of turn-end reports half a second,
  and each `SessionEnd` hook is then bounded by its own timeout (default 1.5s; set
  `GROK_SESSION_END_HOOKS_TIMEOUT_MS`, in milliseconds, to change that default, capped at 60s).

`Stop` is a gate, so that entry runs on the turn's critical path: keep it fast, give it a `timeout`,
and exit 0, because exit 2 blocks the stop and keeps the agent working. Leave `Stop` out if you also
run a blocking `Stop` gate, since a continuation fire would settle the host while the agent is still
going; register `SessionEnd` instead, which is the only thing that settles a session that exits
before the ping.

Both scripts also run inside a subagent's own session. Every event either script reads carries
`subagentType` there and omits it in the main session, so `[ -n "$subagentType" ] && exit 0`
filters a child out of both halves. This matters most for a background subagent, which outlives
the parent turn and would otherwise hold the host busy after the parent went idle.

`Stop` input also carries `backgroundTasks` and `sessionCrons`, so a hook can distinguish "session is done" from "session is paused waiting for background work to wake it back up". Both arrays are empty when nothing is in flight or scheduled. Each `backgroundTasks` entry describes one in-flight task: `id`, `type` (`shell`, `monitor`, or `subagent`), `status`, and (depending on the type) `command` (shell tasks only), `description` (a monitor's watched command line, or a subagent's task description), and `agentType` (subagents). Each `sessionCrons` entry describes one scheduled wakeup (`scheduler_create` or `/loop`): `id`, `schedule`, `recurring`, and `prompt`. The `schedule` value is a human-readable interval such as `every 5 minutes`; grok schedules are intervals, not cron expressions. Free-text entry fields are capped at 1000 characters with an in-string `… [+N chars]` marker.

Inside a subagent, the gate fires as `SubagentStop` (agent-frontmatter `Stop` hooks are automatically remapped). A `Stop` hook only gates the main agent.

`SubagentStop` fires once per subagent, at the subagent's own turn end, matching Claude Code. Its input carries a `phase` field (currently always `"gate"`) reserved for forward compatibility.

**Porting Claude Code stop hooks**: the output vocabulary (`decision`, `reason`, `continue`, `stopReason`, `additionalContext`) works unchanged. Check this list for what does not match Claude:

- **camelCase input**: grok's stdin envelope uses camelCase keys throughout where Claude uses snake_case. A script reading `.stop_hook_active` or `.background_tasks[].agent_type` must switch to `.stopHookActive` and `.backgroundTasks[].agentType` (the `hook_event_name` snake_case key carries Claude's PascalCase value, e.g. `"Stop"`; the `hookEventName` camelCase key carries grok's snake_case value, e.g. `"stop"`). Hooks registered through the grok-agent-sdk convert both the top-level keys and the `backgroundTasks`/`sessionCrons` entry keys to snake_case, so the wire's `.backgroundTasks[].agentType` reads as `.background_tasks[].agent_type` in the SDK.
- **`toolResult` field**: the `PostToolUse` tool output is `toolResult` (SDK: `tool_result`); grok also emits a `tool_response` snake alias that copies `toolResult`, so a hook reading Claude's `.tool_response` works unchanged.
- **`updatedToolOutput` carries grok's own output shape on built-in tools**: a `PostToolUse` replacement for a built-in tool is validated against the tool's output as grok serializes it — the tagged object in that event's `toolResult` — so one written against another runtime's field names parses as the wrong shape and is ignored. On an MCP tool there is no shape to enforce, so `updatedToolOutput` passes through like its `updatedMCPToolOutput` alias. See [PostToolUse Output](#posttooluse-output).
- **Session-end fire**: an extra observe-only Stop fires at session end; filter on `reason == "end_turn"` (see above).
- **Interval schedules**: `sessionCrons[].schedule` is a human-readable interval, never a cron expression.
- **Task types**: `backgroundTasks[].type` is only `shell`, `monitor`, or `subagent`; Claude's other labels (`workflow`, `teammate`, …) are not emitted.
- **StopFailure classes**: grok emits six (`rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, `unknown`). Capacity errors (503/529) classify as `rate_limit`. A matcher on an error class grok does not emit never fires.
- **Default timeout**: grok defaults observe hooks to 5 seconds, which is shorter than most. Set `timeout` explicitly on an imported hook that does real work.
- **`UserPromptSubmit` blocks, with one gap**: exit 2 and `decision: "block"` reject the prompt like Claude, and a blocked prompt never enters the conversation history — but an allowing hook's stdout / `additionalContext` is discarded rather than added as context.
- **`StopCancelled` is grok-specific**: a config that uses it is not portable to a runtime with no interrupt hook.
- **`idle_prompt` fires on any turn end**: grok fires it after an interrupted or errored turn too, not only a completed one, because it reports a state rather than an outcome. Its `message` is display text and can change between releases, so match on `notificationType` instead.
- **Subagent identity is `subagentType`, not `agent_type`**: grok puts it in the payload of the events that can fire inside a subagent, matching its own `SubagentStart`/`SubagentStop`, rather than in the common fields.
- **permission_mode values**: grok emits `default`, `auto`, `plan`, or `bypassPermissions`. Claude's `acceptEdits`/`dontAsk` have no grok equivalent (grok's `auto` is the nearest), so a check like `permission_mode === "acceptEdits"` never matches.
- **Client (SDK) gate timeouts**: SDK `Stop`/`SubagentStop` gates default to 600 seconds like file hooks; `PreToolUse` client gates default to 30 seconds (the interactive hot path). Either can be overridden per matcher group via `timeoutS`, capped at 600.
- **`/goal`**: grok's goal loop is a separate feature that runs before the stop gate; it is not a prompt-type Stop hook.

A complete keep-working policy in one script:

```bash
#!/bin/bash
input=$(cat)
# Gate only genuine turn ends, not the session-end observe fire.
if [ "$(echo "$input" | jq -r '.reason')" != "end_turn" ]; then exit 0; fi
if ! bin/verify.sh >/dev/null 2>&1; then
  echo '{"decision": "block", "reason": "verify.sh failed; fix the failures before finishing"}'
fi
```

registered as `{ "type": "command", "command": "bin/stop-gate.sh", "timeout": 300 }` with `timeout` sized for the verify step. The hook fires again after each continuation, and the built-in cap ends the turn after 8; check `stopHookActive` to give up earlier on feedback the agent evidently cannot act on.

### Passive Hooks

For events like `SessionStart` or `Notification`, stdout is ignored. Just exit 0 on success. The exceptions are `PreToolUse` (see [Output (Blocking Hooks)](#output-blocking-hooks)), `Stop`/`SubagentStop` (see [Stop Decision Control](#stop-decision-control)), and `PostToolUse`, whose stdout is read even though it blocks nothing (see [PostToolUse Output](#posttooluse-output)).

### Environment Variables

Grok sets several environment variables on every hook process. These are useful when writing context-aware or plugin-aware hook scripts.

#### Runner-injected variables (always available)

These variables are set by the hook runner for **every** hook:

| Variable              | Description |
|-----------------------|-------------|
| `GROK_HOOK_EVENT`     | The name of the event that triggered the hook (e.g. `pre_tool_use`, `session_start`, `post_tool_use`, `session_end`, `stop`, `notification`). |
| `GROK_HOOK_NAME`      | The configured name of this specific hook (includes the plugin prefix for plugin-provided hooks). |
| `GROK_SESSION_ID`     | The unique identifier of the current Grok session. |
| `GROK_WORKSPACE_ROOT` | Absolute path to the root of the current workspace. |
| `CLAUDE_PROJECT_DIR`  | Absolute path to the workspace root. A Claude Code-compatible alias for `GROK_WORKSPACE_ROOT`, set for every hook. |

These variables are **reserved**. Any values you attempt to set for them via the `env` field in your hook JSON are stripped at load time (a warning is logged), and the runner always injects the real values at spawn time.

#### Plugin hook variables

When a hook originates from a plugin, Grok additionally injects the following variables:

| Variable             | Description |
|----------------------|-------------|
| `GROK_PLUGIN_ROOT`   | Absolute path to the plugin's installed directory. |
| `GROK_PLUGIN_DATA`   | Absolute path to the plugin's writable data directory (for storing plugin state, caches, etc.). |

These values are provided by the plugin system. For the four plugin-related keys (`GROK_PLUGIN_ROOT`, `GROK_PLUGIN_DATA`, and their Claude aliases), the plugin adapter ensures the official plugin values always win over any user-declared values in the hook's `env` map.

#### User-defined environment variables

You can supply additional environment variables for an individual hook handler using the `env` field:

```json
{
  "type": "command",
  "command": "bin/my-hook.sh",
  "env": {
    "MY_SECRET": "value",
    "LOG_LEVEL": "debug"
  }
}
```

These variables are passed through to the hook process, but they cannot override the reserved runner or plugin variables listed above.

#### Using variables in `command` and `url` fields

Both `command` and `url` support `${VAR}` and `$VAR` expansion. On Windows PowerShell, known `$VAR` refs are rewritten to `$env:VAR` so they read the child environment. See the custom-hooks reference for load-time vs runtime expansion, the `env` map lookup order, and parameter-expansion modifiers (e.g. `${VAR:-default}`).

---

## HTTP Hooks

Instead of a local script, call a remote endpoint:

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

The full event envelope is POSTed as JSON.

---

## Managing Hooks in the TUI

### The Hooks Tab

Press `Ctrl+L` on non–VS Code family terminals to open the Extensions modal (Plugins tab), or run `/hooks` (any terminal; required on VS Code family where `Ctrl+L` is interject) to open it on the Hooks tab. In the **Hooks** tab:

| Key | Action |
|-----|--------|
| `r` | Reload all hooks from disk |
| `a` | Add a custom hook by path |
| `x` | Remove the selected hook source (asks for confirmation; press lowercase `y` to confirm) |
| `Space` | Enable or disable the selected hook |
| `f` | Cycle the status filter (All / Enabled / Disabled) |

Hooks are grouped by source: **Global**, **Project**, **Plugin**, and **Custom**.

Each hook shows:
- **Event** it triggers on
- **Command** or **URL** that runs
- **Timeout** duration
- **Status**: enabled or `[disabled]`

### Slash Commands

```
/hooks-list           # Show hooks loaded in this session
/hooks-trust          # Trust this project for hook execution
/hooks-add <path>     # Add a custom hook file or directory
/hooks-remove <path>  # Remove a custom hook
/hooks-untrust        # Revoke trust for this project
```

In the TUI pager, the individual `/hooks-*` commands do not appear in the slash-command list. The `/hooks` modal covers listing, adding, removing, and enabling or disabling hooks; project trust is managed via `/hooks-trust` (or the modal's Trust action), which writes the unified folder-trust store described above.

### Per-Hook Enable/Disable

Enable or disable an individual hook at runtime by pressing `Space` in the Hooks tab. The change takes effect immediately, without restarting the session.

### Mid-Session Reload

Press `r` in the Hooks tab to reload all hooks from disk. Grok re-reads every hook source, so this picks up changes you made to hook files during the session.

---

## Hooks in the Status Row and Scrollback

Hooks are quiet unless they hold the turn up or change its course:

- While the turn is blocked on a hook batch (a `PreToolUse` gate before a tool, the `UserPromptSubmit` gate, a `Stop` gate), the status row reads `Running pre_tool_use hook…` (or `Running 3 stop hooks…`) once the batch has run for about 300 ms. The timer counts from when the batch started, so a slow hook shows its full wait; a fast one never shows at all.
- A hook that ran and allowed leaves no trace. Its stdout is not shown.
- A hook that denies a tool call, blocks a prompt, or stops or continues the agent gets one annotation line with the reason. Hooks from `~/.grok`, project, and plugin files are named; hooks from managed configuration read as "a managed policy hook".
- A hook that fails (non-zero exit, timeout, crash, malformed output) gets one line: `<event> hook (<name>) failed, ignored: <reason>`, where the reason is the exit code with the first stderr line, or the timeout. "Ignored" is literal: failures are fail-open, so the tool call or turn proceeds as if the hook had allowed it.

Deny and failure lines carry the same bullet as the tool rows, so they read as part of the tool call above them.

These lines appear only when the plugins UI is enabled (the default).

---

## Example: Safe Shell Guard

Block dangerous shell commands:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safe-shell.sh", "timeout": 5 }
        ]
      }
    ]
  }
}
```

Where `bin/safe-shell.sh`:

```bash
#!/bin/sh
INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

# Block destructive patterns
if echo "$CMD" | grep -qE '(rm -rf /|mkfs|dd if=|:(){ :|& };:)'; then
  echo '{"decision": "deny", "reason": "Blocked potentially destructive command"}' 
  exit 2
fi

echo '{"decision": "allow"}'
```

---

## Security Notes

- Global hooks (`~/.grok/hooks/`) run with your user permissions; treat them like shell scripts.
- Project hooks require folder trust (`/hooks-trust` or `--trust`, the same gate as repo-local MCP/LSP) to prevent supply-chain attacks from malicious repos.
- HTTP hooks send session data; only use trusted endpoints.
- A `PostToolUse` hook decides what the model reads for that tool call — it can add instructions or replace the output outright — so trust one the way you trust a `PreToolUse` gate. The scrollback and the transcript keep the real output, so a replacement is always visible to you.

---

## Best Practices

1. **Keep hooks fast**: long-running hooks block the UI. Use background processes (`&`) or async where possible.
2. **Use explicit `deny` to block**: hooks fail-open on any error, so a hook that crashes will not block the tool. To enforce policy, your hook must run to completion and emit `{"decision":"deny","reason":"..."}` on stdout. Always handle errors inside your script so it can return an explicit decision.
3. **Use absolute paths or relative to hook file**: scripts in `bin/` next to the JSON file are portable.
4. **Test with the modal**: press `Ctrl+L` (non–VS Code family) or run `/hooks` to verify hooks are loaded and matching before relying on them.
5. **Version control project hooks**: commit `.grok/hooks/` (but never secrets).

---

## Troubleshooting

- **Hook not running?** Press `Ctrl+L` on non–VS Code family (or run `/hooks` anywhere) to see if it is loaded and matched.
- **Project hooks ignored?** The folder may be untrusted. Run `/hooks-trust` (or relaunch with `--trust`).
- **Script not found?** Check the path is relative to the `.json` file and executable (`chmod +x`).
- **See errors?** Capture logs by launching with `RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok`, then check `/tmp/grok.log`.
