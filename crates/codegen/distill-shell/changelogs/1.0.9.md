# 1.0.9 — 2026-08-24

## Features

- **New sessions in the interactive TUI** now start in auto mode by default for fewer approval prompts.
- **/feedback now supports attaching images** (screenshots etc.).
- **The / slash menu** is now ordered by recency and groups skills below commands for easier navigation.
- **/workflow** now accepts --agent-budget N or an agent_budget JSON field to set child-agent limits.
- **/workflow** now accepts --effort LEVEL or an effort JSON field to set child reasoning effort.
- **MCP servers** now receive a grok-cli User-Agent header so providers can attribute traffic.
- **Dashboard** now lets you start new agents in Auto mode via Shift+Tab.
- **Workflows** now appear alongside skills in the model prompt so the agent can launch them by name.
- **`/edit-prompt`** now opens an external editor from the full TUI, not only minimal mode.
- **Up arrow** on an empty prompt now selects queued follow-ups first instead of opening history.
- **/plugin** now works as an alias for **/plugins** in the TUI.
- **Plugin-provided agents** now appear in the **/agents** modal and can be toggled.
- **/minimal** and **/fullscreen** now switch instantly inside the running session without restarting or losing the current turn.
- The configurable status line row now appears at the bottom of minimal mode as well as fullscreen.
- **grok clone** now fetches only the selected branch tip by default, with --full-history for legacy full clones.
- **grok clone** can now reuse a matching local checkout as a linked worktree instead of always fetching from the network.

## Bug Fixes

- **Subagent tasks** are more resilient to temporary rate limits and will retry instead of failing immediately.
- **Narrow allow rules for Bash commands** now correctly permit file writes without extra prompts.
- **Rules** containing markdown headings now render correctly in the prompt.
- **/new** and **/clear** now preserve the last-chosen reasoning effort instead of resetting to the model default.
- **Prompt dropdowns** (slash, history, file search) now have consistent left alignment with the composer.
- **History** now shows every command, slash command, and memory note in the order you typed them.
- **Collapsed tool-call rows** no longer show a vertical accent line beside the status diamond.
- **Hint text** above the prompt now lines up with the text you type.
- **Pressing Esc** while peeking at a conversation from the dashboard now closes open modals instead of leaving the view.
- **`/copy`** now preserves markdown formatting such as headings and code fences.
- **Sending a new message** while a shell command is running now moves the command to the background instead of cancelling it.
- **grok mcp add** now correctly treats bare http(s):// URLs as HTTP servers instead of stdio commands.
- Plan mode no longer incorrectly blocks subagent spawns as file edits outside plan.md.
- MCP servers that share a URL but have different names are now kept as separate entries instead of being collapsed.
- Subagents and workflow-spawned agents no longer receive the workflow tool that only top-level sessions can use.
- Memory results are now presented as historical context that should be verified against live sources before use.
- **Interactive grok sessions** now start in ask mode by default again instead of auto.
- **Minimal mode** no longer truncates subagent wake-turn replies after the first token.

## Performance

- **Concurrent subagents** no longer trigger rate-limit errors during bursts.
- **MCP server connections** no longer delay session startup when many servers are configured.

