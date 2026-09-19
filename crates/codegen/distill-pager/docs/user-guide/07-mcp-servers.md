<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# MCP Servers

MCP (Model Context Protocol) servers extend Grok with external tool integrations. They let Grok interact with any service that implements the MCP standard.

---

## What Are MCP Servers?

An MCP server is a process that exposes tools to Grok over a standardized protocol. When you configure an MCP server, its tools become available to the model alongside Grok's built-in tools. The model can discover and call these tools during a session.

For example, a GitHub MCP server might expose tools like `create_issue`, `list_pull_requests`, and `search_code`. A database server might expose `query`, `list_tables`, and `describe_schema`.

See the [MCP specification](https://modelcontextprotocol.io) for protocol details.

---

## Configuration

MCP servers are configured in `~/.grok/config.toml` under `[mcp_servers.<name>]` sections.

To distribute MCP servers to a team, or to restrict which servers users may run (`allowedMcpServers` / `deniedMcpServers` in `requirements.toml` / `managed_config.toml`, with Claude `managed-settings.json` advisory for foreign-defined servers), see [Distribute across an organization](09-plugins.md#distribute-across-an-organization) in the Plugins guide.

### stdio Transport (Local Process)

Grok spawns a local process and communicates over stdin/stdout:

```toml
[mcp_servers.my-server]
command = "/path/to/server"           # Server executable
args = ["--flag", "value"]            # Command arguments
env = { API_KEY = "sk-..." }          # Environment variables
enabled = true                        # Enable or disable the server (default: true)
startup_timeout_sec = 30              # Server startup timeout, seconds (default: 30)
tool_timeout_sec = 6000               # Per-tool-call timeout fallback, seconds (default: 6000)
tool_timeouts = { slow_op = 120 }     # Per-tool timeout overrides, seconds
```

> **Global startup-timeout override:** instead of setting `startup_timeout_sec`
> per server, you can change the default for all servers via the `MCP_TIMEOUT`
> environment variable (milliseconds, compatible with Claude Code) or
> `GROK_MCP_STARTUP_TIMEOUT_SECS` (seconds). A per-server `startup_timeout_sec`
> still takes precedence over both. Cold-start `npx`/`uvx` servers that download
> packages on first launch often need this; the default is 30s.
>
> **MCP tool-result size cap:** large MCP / `use_tool` results are truncated
> inline (full payload spilled under the session `mcp/` folder). Default is
> **20_000 bytes**. Override via:
>
> - env `GROK_MAX_MCP_OUTPUT_BYTES` or `MAX_MCP_OUTPUT_BYTES` (bytes; Grok-native
>   wins if both set; Claude-style name, but we bound by **bytes** not tokens)
> - `config.toml` — user-level (`~/.grok/config.toml`) **or repo-level**
>   (`.grok/config.toml` anywhere on the cwd → git-root chain; the deepest
>   file wins, and the repo value applies only once the folder is trusted):
>
> ```toml
> [mcp]
> max_output_bytes = 40000
> ```
>
> Precedence: requirements.toml > env > repo `.grok/config.toml` >
> user/managed config > default. Repo edits apply to running sessions in that
> directory via config hot-reload.

### HTTP/SSE Transport (Remote Server)

For remote MCP servers accessible over HTTP:

```toml
[mcp_servers.remote-api]
url = "https://mcp.example.com/api"
headers = { "Authorization" = "Bearer token" }
```

MCP data-plane requests (JSON-RPC and SSE) and the anonymous-access probe carry a
default `User-Agent: grok-cli/<version>` header, where `<version>` is the Grok binary
version. OAuth discovery, client registration, and token requests are issued by the
rmcp OAuth client and keep its own behavior (no default `User-Agent`). A valid
`User-Agent` entry in the server's `headers` overrides the default; an invalid
configured `User-Agent` value is dropped by header parsing (with a warning), so such a
server still receives the default. Exception: Figma MCP servers (server name `figma`,
legacy managed name `grok_com_figma`, or a `figma.com` host — all case-insensitive)
send the bare token `grok-cli` with no version unless the config supplies its own
`User-Agent`.

### Streamable HTTP with Session ID

```toml
[mcp_servers.my-streamable-server]
url = "https://mcp.example.com/api/mcp"
headers = { "x-mcp-session-id" = "{{session_id}}" }
```

---

## CLI Management

Manage MCP servers from the command line without editing config files:

```bash
# List configured MCP servers
grok mcp list
grok mcp list --json          # Machine-readable output

# Add a stdio server. Everything after -- is the server command, so flags
# like -y reach the server instead of being parsed by grok.
grok mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path/to/dir

# Add a stdio server with environment variables (-e is repeatable)
grok mcp add postgres -e DATABASE_URL=postgres://localhost/mydb -- npx -y @modelcontextprotocol/server-postgres

# Add a remote HTTP server
grok mcp add --transport http sentry https://mcp.sentry.dev/mcp

# Add a remote server with an authentication header (--header is repeatable)
grok mcp add --transport http api https://mcp.example.com/mcp --header "Authorization: Bearer YOUR_TOKEN"

# Add a remote SSE server
grok mcp add --transport sse linear https://mcp.linear.app/sse

# Remove a server
grok mcp remove github

# Enable or disable a local/TOML (or compat-sourced) server
grok mcp enable github
grok mcp disable github

# Diagnose a server's configuration and connectivity
grok mcp doctor               # Check every configured server
grok mcp doctor github        # Check one server
grok mcp doctor --json        # Machine-readable output
```

The transport defaults to `stdio`; pass `--transport http` or `--transport sse` for remote servers.

By default `distill mcp add` writes to `~/.grok/config.toml` (`--scope user`). Use `--scope project` to write to `.grok/config.toml` in the current directory instead, which can be committed and shared with your team (see [Project-Scoped MCP Servers](#project-scoped-mcp-servers)). Header and environment variable values are stored verbatim, so reference secrets as `${VAR}` instead of pasting them into a committed project config (see [Example Configurations](#example-configurations)). `distill mcp list` shows servers from both scopes, marking project-scoped ones with `(project)` and disabled ones with `(disabled)`.

`distill mcp remove` searches both scopes and exits 0 after removing the server. It exits 1 when the name is not found, or when the name is defined in both user and project scope — pass `--scope` to say which one to remove.

`distill mcp enable` / `disable` persist the personal on/off state to user `~/.grok/config.toml` (`disabled_mcp_servers`, and `[mcp_servers.<name>].enabled` when that entry exists). Scope:

- **Known names:** user/project Grok TOML, names already on the disabled list, compat sources (`.mcp.json`, Claude, Cursor), and **plugin** MCP servers (same discovery as doctor/`/mcps`).
- **Enable only:** if the cwd-nearest project definition has sticky `enabled = false`, that single key is cleared (comments preserved); disable never rewrites project configs.
- **Not full `/mcps` parity:** gateway connectors (`managed_gateway:…`, stored under `disabled_mcp_tools.__managed_gateway_connectors`) stay Space-only in the TUI. Idempotent; unknown names exit 1.

Breaking changes from earlier releases: `--env` now takes one `KEY=value` per flag (use `-e A=1 -e B=2`, not `--env A=1 B=2`), and server names may only contain letters, numbers, hyphens, and underscores.

---

## Project-Scoped MCP Servers

MCP servers can be configured per-project by placing a `.grok/config.toml` in your repository:

```
my-project/
  .grok/
    config.toml
  src/
  ...
```

```toml
# .grok/config.toml
[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
enabled = true
```

When a server exposes a native HTTP/SSE endpoint, prefer the `url` form over wrapping it in a stdio proxy such as `npx mcp-remote <url>`. Grok handles HTTP/SSE and OAuth directly, so the native form avoids an extra subprocess per session. It also registers Grok's own OAuth client with the provider.

Grok walks from the current directory up to the git repo root, loading `.grok/config.toml` at each level:

| Location | Scope | Priority |
|----------|-------|----------|
| `~/.grok/config.toml` | All projects | Lowest |
| `<repo-root>/.grok/config.toml` | This repository | Medium |
| `<cwd>/.grok/config.toml` | Current directory | Highest |

If a project defines a server with the same name as a global one, the project version replaces it entirely (fields are not merged).

Project-scoped files contribute `[mcp_servers]`, `[plugins]`, and `[permission]` entries. Distill reads most other config sections only from `~/.grok/config.toml`.

---

## Tool Naming

MCP tools are namespaced with the server name to avoid collisions. The catalog key is `server__tool` (two underscores):

- Server `filesystem` with tool `read_file` becomes `filesystem__read_file`
- Server `github` with tool `create_issue` becomes `github__create_issue`
- A tool segment may start with a digit: server `auth` with tool `2fa_enable` becomes `auth__2fa_enable`

### What Grok admits

Grok admits a listed tool into the session catalog when all of these hold (`distill-mcp` `qualify_mcp_tool_name`):

| Part | Rule |
| --- | --- |
| Server name | Starts with a letter or underscore. Then ASCII letters, digits, underscores, and hyphens only. |
| Tool name | Non-empty. ASCII letters, digits, underscores, and hyphens only. May start with a digit. |
| Delimiter | Exactly one `__`. Names with a second `__`, or with `___`, are skipped. |
| Catalog key | `server` + `__` + `tool` is at most **256** characters. |

A rejected tool is skipped. The log line is `Skipping MCP tool` with the reason. The rest of that server's tools still load.

The **64-character** cap is a provider **function-name** budget. It applies to the meta-tools `search_tool` and `use_tool` themselves. It does **not** apply to catalog keys. A `server__tool` name longer than 64 characters stays in the catalog. The model still calls it through `use_tool` with that full name. Grok used to drop those tools at 64 characters. It no longer does.

The server name in `[mcp_servers.<name>]` / `distill mcp add` is the catalog prefix. A name that starts with a digit is a valid TOML key. Catalog admission still rejects it (`InvalidServerName`). Rename the server so it starts with a letter or underscore.

A server name that ends with `_` makes `server__tool` contain `___`. Admission skips that key (`InvalidOrAmbiguousQualifiedName`).

`search_tool` / `use_tool` take the qualified catalog key, not the raw MCP tool name. Example: `github__create_issue`, not `create_issue`.

---

## Toggle Servers at Runtime

You can enable or disable MCP servers without restarting Grok (TUI `/mcps` or CLI — see [CLI Management](#cli-management)).

### The /mcps Modal

Open the MCP servers modal in the TUI:

- Run `/mcps` as a slash command
- Or press `Ctrl+L` (non–VS Code family) and navigate to the MCP Servers tab; on VS Code family use `/plugins` or `/mcp` and open the MCP Servers tab

From the modal you can:

- See each server's source, enabled state, and tool count
- Enable or disable a server with `Space`
- Expand a server to view the tools it provides
- Refresh the list with `r` after you edit `config.toml`
- Authenticate an OAuth server with `i`
- Add a server with `a`, or remove a local server with `x` (the modal asks for confirmation; press lowercase `y` to remove, or any other key to cancel)

### Tool Discovery

The model has access to two built-in tools for working with MCP servers:

- `search_tool` — Discover available integration tools across all enabled MCP servers. Use this to find tools by name or description.
- `use_tool` — Call an integration tool discovered via `search_tool`. Specify the fully-qualified tool name (e.g., `github__create_issue`).

---

## Compatibility

Distill loads MCP server configurations from multiple sources for compatibility:

| Source | Format | Location | Configurable |
|--------|--------|----------|-------------|
| `config.toml` | Native Grok config | `~/.grok/config.toml`, `.grok/config.toml` | Always on |
| `.claude.json` | Claude Code format | `~/.claude.json` | `[compat.claude] mcps` |
| `.cursor/mcp.json` | Cursor format | `~/.cursor/mcp.json`, `<project>/.cursor/mcp.json` | `[compat.cursor] mcps` |
| `.mcp.json` | MCP standard format | Project root (cwd to git root) | Loaded unless you have imported or dismissed the Claude import prompt (the import marker is set) |

All sources are merged in priority order: config.toml > Claude > Cursor > `.mcp.json`. Servers from higher-priority sources take precedence when names conflict.

The Claude and Cursor MCP sources are scanned by default. To disable scanning for a specific vendor, set `[compat.<vendor>] mcps = false` in `~/.grok/config.toml` or the corresponding environment variable (`GROK_CURSOR_MCPS_ENABLED`, `GROK_CLAUDE_MCPS_ENABLED`). See [Configuration](05-configuration.md#harness-compatibility) for details. Use `distill inspect` to see which MCP servers were loaded and their vendor origin (`[cursor]`, `[claude]`).

---

## MCP OAuth

For MCP servers that require OAuth authentication, Grok handles the credential flow automatically. When an MCP server requests OAuth credentials, Distill opens a browser-based authorization flow and stores the resulting tokens for future use.

---

## Example Configurations

Use the `url` form for hosted MCP servers and the `command` / `args` form for local stdio tools.

### Native HTTP (hosted services)

You must authenticate OAuth-based MCP servers before you can use them. Distill stores the resulting tokens under `~/.grok/mcp_credentials.json` as local plaintext with owner-only file permissions (`0600` on Unix). Prefer full-disk encryption on the host. After you edit `config.toml`, press `r` in the `/mcps` modal to refresh the server list.

```toml
[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
enabled = true

[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"
enabled = true

[mcp_servers.mixpanel]
url = "https://mcp.mixpanel.com/mcp"
enabled = true
```

For internal or self-hosted servers that authenticate with a static bearer token rather than OAuth, set the `Authorization` header explicitly:

```toml
[mcp_servers.internal-tools]
url = "https://mcp.internal.example.com/mcp"
enabled = true

[mcp_servers.internal-tools.headers]
Authorization = "Bearer <token>"
```

To avoid putting secrets in the config file, reference an environment variable with `${VAR}` (or `${VAR:-default}`). Grok expands string fields in `[mcp_servers.*]` — `url`, `command`, `args`, and the values in `env` and `headers` — at load time:

```toml
[mcp_servers.internal-tools]
url = "https://mcp.internal.example.com/mcp"
enabled = true
headers = { "Authorization" = "Bearer ${INTERNAL_MCP_TOKEN}" }
```

### Local stdio

Use stdio for tools that must run locally (filesystem access, local databases, in-house servers).

```toml
# Filesystem access scoped to a directory
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/directory"]

# Local Postgres
[mcp_servers.postgres]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-postgres", "postgresql://user:pass@localhost/db"]

# Custom server with a longer startup timeout and tuned per-tool timeouts
[mcp_servers.my-tools]
command = "/usr/local/bin/my-mcp-server"
args = ["--config", "/etc/my-mcp.json"]
startup_timeout_sec = 30
tool_timeout_sec = 120
tool_timeouts = { slow_analysis = 300, quick_lookup = 10 }
```

On Windows, npm installs launchers like `npx`, `npm`, `pnpm`, and `yarn` as `.cmd` batch shims (there is no `npx.exe`). Grok resolves a bare `command` such as `npx` to its real launcher path on `PATH` (honoring `PATHEXT`) before spawning, so these work without manually wrapping them in `cmd /c`. A `command` given as an absolute path or one containing a path separator is used as-is.

---

## Available MCP Servers

A partial list of MCP servers you can configure with the `url` or `command` forms shown above. Confirm the current endpoint or package name with each provider before use:

| Server | Transport | Endpoint / Package |
|--------|-----------|--------------------|
| Linear | HTTP (OAuth) | `https://mcp.linear.app/mcp` |
| Sentry | HTTP (OAuth) | `https://mcp.sentry.dev/mcp` |
| Mixpanel | HTTP (OAuth) | `https://mcp.mixpanel.com/mcp` |
| Filesystem | stdio | `@modelcontextprotocol/server-filesystem` |
| Git | stdio | `@modelcontextprotocol/server-git` |
| GitHub | stdio | `@modelcontextprotocol/server-github` |
| GitLab | stdio | `@modelcontextprotocol/server-gitlab` |
| PostgreSQL | stdio | `@modelcontextprotocol/server-postgres` |
| SQLite | stdio | `@modelcontextprotocol/server-sqlite` |
| Puppeteer | stdio | `@modelcontextprotocol/server-puppeteer` |

See the [MCP Server Registry](https://github.com/modelcontextprotocol/servers) for the full list of community servers and the [MCP specification](https://modelcontextprotocol.io) for protocol details.

---

## Subagents and MCP

Subagents inherit the parent session’s connected MCP servers by default, including plugin-sourced agents. Use agent frontmatter `mcpInheritance` to restrict that set (`all`, `none`, `named`, or `except`). Details are in [Subagents — MCP inheritance](16-subagents.md#mcp-inheritance).

If a child lists `search_tool` / `use_tool` but returns an empty catalog, check that:

1. The parent session actually connected the server (see Extensions / `distill inspect`)
2. The agent’s `mcpInheritance` is not `none` or a filter that excludes the server
3. Plugin agents cannot declare their own `mcpServers` in frontmatter — they only see parent-connected servers

---

## Troubleshooting

### Server Not Starting

```bash
# Test the server command manually
npx -y @modelcontextprotocol/server-filesystem /path

# Increase startup timeout
# In config.toml:
[mcp_servers.filesystem]
startup_timeout_sec = 30
```

For stdio servers, Grok captures the process's standard error to `~/.grok/logs/mcp/<server>.stderr.log`, truncated on each launch. Check this file when a server starts but fails to handshake:

```bash
tail -f ~/.grok/logs/mcp/filesystem.stderr.log
```

### Blocked by organization policy

If native TOML policy or Claude `managed-settings.json` sets `deniedMcpServers`, a nonempty `allowedMcpServers`, or `allowManagedMcpServersOnly`, Grok drops non-matching servers at merge time and logs `MCP server blocked by managed settings policy`. Native grok layers bind every server; the Claude file binds foreign-defined servers only. `distill inspect` shows the lists, lockdown scope, and each remaining server. Details and examples: [Restrict which MCP servers can run](09-plugins.md#restrict-which-mcp-servers-can-run).

### A listed tool never appears

The server starts and `tools/list` returns the tool, but `/mcps` and `search_tool` omit it.

1. Check `Skipping MCP tool` in `GROK_LOG_FILE` / `--debug`. The reason names the rule that failed (invalid server name, invalid tool name, ambiguous `__`, or catalog key longer than 256 characters).
2. Confirm the server config key starts with a letter or underscore. A digit-leading key never enters the catalog. A key that ends with `_` is skipped as an ambiguous `___` name.
3. Confirm the tool name uses only `[A-Za-z0-9_-]`. Dots and colons in the raw MCP name are skipped.
4. Do not shorten a `server__tool` key to 64 characters. Catalog keys may be up to 256. The 64-character cap is only for `search_tool` / `use_tool` as function names. See [Tool Naming](#tool-naming).

This is separate from a tool that is missing on the **first** prompt because the handshake is still running. Send a second prompt after the server is up, or run `distill mcp doctor`.

### Viewing Server Status

Use `distill inspect` to see all loaded MCP servers and their sources:

```bash
distill inspect          # Human-readable
grok inspect --json   # Machine-readable
```

### Debug Logging

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok
tail -f /tmp/grok.log
```

Look for log entries containing `mcp` to trace server startup, tool discovery, and tool call execution.
