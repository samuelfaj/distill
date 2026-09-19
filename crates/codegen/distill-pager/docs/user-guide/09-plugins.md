<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Plugins

A plugin bundles skills, slash commands, agents, hooks, and MCP servers into one installable unit. You get plugins from a marketplace, install the ones you want, and Distill loads what they add. To build and share your own, see [Create your own marketplace](#create-your-own-marketplace).

---

## How marketplaces work

A marketplace is a catalog of plugins that someone has published and shared. Using one takes two steps, like adding an app store: adding the marketplace lets you browse its plugins, and you then choose which to install.

1. **Add the marketplace** so Distill can show what it offers. Nothing installs yet.
2. **Install the plugins you want**, one at a time.

Plugins stay off until you install and enable them, and a plugin's hooks and MCP servers stay inactive until you [trust](#trust-and-security) it.

---

## Add a marketplace

A marketplace source is a GitHub repository, a git URL on any host, or a local folder. Add one from the command line:

```bash
distill plugin marketplace add my-org/team-plugins                  # GitHub shorthand (owner/repo)
grok plugin marketplace add https://gitlab.com/acme/plugins.git  # any git host, include https:// and .git
grok plugin marketplace add ./my-marketplace                     # a local folder
```

List, refresh, and remove sources with `distill plugin marketplace list`, `distill plugin marketplace update [<name>]`, and `distill plugin marketplace remove <url>`.

You can also declare sources in config so they are always present.

### In config.toml

Each source needs a `name` and either a `git` URL (with an optional `branch`) or a local `path`:

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

### In settings.json

Add sources under `extraKnownMarketplaces`, keyed by name. Each entry's `source` is one of `git` (with `url`), `github` (with `repo`), or `local` (with `path`):

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" }
    }
  }
}
```

Place this file at `~/.grok/settings.json` or `~/.claude/settings.json`.

---

## Install and use a plugin

Once a marketplace is added, install a plugin by name. You can also install straight from a repository or a local path:

```bash
distill plugin install deploy-tools --trust
```

The source you install accepts several forms:

- `owner/repo` (GitHub shorthand), `owner/repo@v1.0` (a ref), `owner/repo@<commit-sha>` (an exact commit, verified after fetch), or `owner/repo#subdir`
- a full git URL (`https://github.com/user/repo.git`) or SSH (`git@github.com:user/repo.git`)
- a local path (`./local-dir` or `/absolute/path`)

Run `distill plugin install <source>` without `--trust` and Grok shows the source, warns that installing activates the plugin's hooks, MCP servers, and skills, then stops. Add `--trust` to go ahead. Only install plugins from sources you trust (see [Trust and security](#trust-and-security)).

A plugin's skills appear in the slash menu. When a skill name is ambiguous, Grok shows the qualified form prefixed by the plugin name, for example `/deploy-tools:release`. To pick up a newly installed plugin, press `r` in the Plugins tab or start a new session.

---

## Manage plugins

### From the command line

```bash
distill plugin list [--json] [--available]   # installed plugins (--available requires --json)
grok plugin uninstall <name> [--confirm] [--keep-data]   # aliases: rm, remove
grok plugin update [<name>]               # omit the name to update every plugin
grok plugin enable <name>
grok plugin disable <name>
grok plugin details <name>                # show the plugin's component inventory
```

### In the terminal UI

Open the plugins modal with `Ctrl+L` (outside the VS Code family) or `/plugins` (any terminal, and required on the VS Code family). It has six tabs, **Hooks**, **Plugins**, **Marketplace**, **Skills**, **Workflows**, and **MCP Servers**; switch with `Tab` / `Shift+Tab`. The `/hooks`, `/marketplace`, `/skills`, `/workflows`, and `/mcps` commands open the modal on the matching tab.

In the **Plugins** tab, press `Enter` to expand a plugin and see its name, version, scope (`cli`, `project`, `user`, `custom path`, or the marketplace source name), skills, agents, hooks, MCP servers (shown as `blocked` when the plugin is not trusted), description, and path. Then:

| Key | Action |
|-----|--------|
| `r` | Reload all plugins |
| `a` | Add a plugin from `owner/repo`, a URL, or a local path |
| `Space` | Enable or disable the selected plugin |
| `x` | Uninstall the selected plugin |
| `f` | Filter by status (all, enabled, or disabled) |
| `/` | Search by name |

In the **Marketplace** tab, browse and install from your sources:

| Key | Action |
|-----|--------|
| `i` | Install the selected plugin |
| `d` | Uninstall the selected plugin |
| `a` | Add a marketplace source |
| `x` | Remove the selected source and its plugins |
| `r` | Refresh sources |
| `u` | Update the selected plugin |

Component summaries in the Marketplace tab appear only for marketplaces that publish a [`plugin-index.json`](#add-a-catalog-optional) catalog. Destructive actions ask for confirmation: press lowercase `y` to confirm, any other key (including `Esc`) to cancel.

In the **Workflows** tab (open it directly with `/workflows`, or `Tab` from the commands above), browse the saved workflows Grok discovered: built-ins, project `.grok/workflows/`, and user `~/.grok/workflows/`. Each row shows the workflow's name, source, and description; press `Enter` to expand its path and when-to-use notes, `r` to reload the list, and `/` to search. Rows are browse-only — run one with `/workflow <name>` or its own slash command.

### Turn plugins on or off in config

Set these in `~/.grok/config.toml`:

```toml
[plugins]
paths = ["~/my-plugins/custom-tools"]        # extra plugin directories
disabled = ["user/a1b2c3d4/noisy-plugin"]    # names or IDs to skip
enabled = ["project/9f8e7d6c/team-tools"]    # names or IDs to force on
```

Plugins are off by default, so list one in `enabled` to turn it on, or in `disabled` to discover it but skip loading it. Each entry is a plain plugin name (from `distill plugin list`) or a full ID (`<scope>/<hash>/<name>`).

To hide the plugins and hooks interface entirely, set `disable_plugins = true` in `~/.grok/pager.toml`.

---

## Trust and security

Plugins run with your privileges, so treat them like any software you install: only add marketplaces and install plugins from sources you trust.

Enabled plugins require trust to load skills, commands, hooks, MCP servers, and LSP servers. Untrusted plugin agents remain listed with frontmatter only. Grok trusts plugins in `~/.grok/plugins/` automatically; project plugins in `.grok/plugins/` require trust. Install with `--trust` to grant it:

```bash
distill plugin install <source> --trust
```

Trusted plugin `.mcp.json` servers attach to the session like other MCP config, and child agents inherit them. Plugin agents (`plugin-name:agent-name`) use the parent session's MCP servers by default, the same as user agents under `~/.grok/agents/`; restrict that with the `mcpInheritance` frontmatter (see [Subagents](16-subagents.md#mcp-inheritance)). For safety, plugin agent frontmatter cannot declare `mcpServers` or hooks, or set `permissionMode: bypassPermissions`.

---

## Create your own marketplace

A marketplace is a git repository (or a local folder) that lists a set of plugins. Adding one works like adding an app store: it lets people browse your plugins, and they choose which to install. Publishing your own is how a team or an organization shares its skills, commands, agents, hooks, and MCP servers from one place.

You need three things: a git repository, one folder per plugin, and a single index file that lists them.

### Set up the repository

1. **Create a git repository.** A private repository is fine; access uses each person's own git credentials.
2. **Add each plugin as a folder.** A plugin folder holds any of `skills/`, `commands/`, `agents/`, `hooks/hooks.json`, `.mcp.json`, and an optional `plugin.json` manifest (see [What a plugin contains](#what-a-plugin-contains)).
3. **List the plugins in `.grok-plugin/marketplace.json`.** This is the index Distill reads.
4. **Push the repository.**

A typical layout:

```
my-org-plugins/
  .grok-plugin/
    marketplace.json      # the index Distill reads (required)
    plugin-index.json     # optional catalog for richer browsing
  plugins/
    gdrive/
      plugin.json         # optional manifest
      skills/gdrive/SKILL.md
      .mcp.json           # MCP servers this plugin adds
```

Distill reads the index from `.grok-plugin/marketplace.json`. It also accepts `.grok-plugin/plugin.json` and the `.claude-plugin/` equivalents.

### Write the index

`marketplace.json` names the marketplace and lists each plugin:

```json
{
  "name": "My Org Plugins",
  "description": "Internal skills and tools",
  "owner": { "name": "Platform Team", "email": "platform@example.com" },
  "plugins": [
    {
      "name": "gdrive",
      "description": "Search and edit Google Drive, Docs, Sheets, and Slides",
      "category": "productivity",
      "source": { "type": "local", "path": "./plugins/gdrive" }
    }
  ]
}
```

Each plugin's `source` points at its files, in one of two ways:

- **In this repository**: `{ "type": "local", "path": "./plugins/gdrive" }`. The plain string `"./plugins/gdrive"` also works.
- **In a separate repository**: `{ "source": "url", "url": "https://github.com/my-org/gdrive.git", "sha": "<full commit sha>" }`. Pin a `sha` so installs are reproducible (required when you [require pinned versions](#require-pinned-versions)).

Optional per-plugin fields: `version`, `author`, `homepage`, `tags`, and `keywords`.

### Add a catalog (optional)

A `plugin-index.json` catalog lets the marketplace browser show each plugin's skills, commands, hooks, and agents before anyone installs it. It is for display only, installs work without it, and teams usually generate it in CI:

```json
{
  "version": 1,
  "plugins": {
    "gdrive": {
      "components": {
        "skills": [{ "name": "gdrive", "description": "Google Drive access" }]
      }
    }
  }
}
```

### Check and share it

Validate a plugin before publishing with `distill plugin validate [<path>]`, and tag a release from the manifest version with `distill plugin tag [<path>] [--push]`. Then point people at the repository. They add it once and install the plugins they want:

```bash
distill plugin marketplace add my-org/my-org-plugins   # GitHub shorthand, a git URL, or a local path
grok plugin install gdrive --trust
```

To install it for everyone automatically instead of person by person, see [Distribute across an organization](#distribute-across-an-organization).

---

## Distribute across an organization

Admins control plugins, marketplaces, and MCP servers through grok's TOML layers plus an optional Claude policy file:

- **`managed_config.toml` / `requirements.toml`** (and macOS MDM) are **native** policy. Put allowlists, denylists, and pins here when grok should enforce them on every server and marketplace, including ones defined in the user's own config or by plugins. `requirements.toml` / MDM is the tamper-resistant tier; user-writable `~/.grok` copies are self-imposed only.
- **Claude `managed-settings.json`** is **advisory**. Its MCP and marketplace restrictions bind **foreign** subjects only — project files (`.grok/config.toml`, `.mcp.json`), imported Claude configs, CLI overrides, and client-injected servers. They never bind grok-native subjects (user/system `config.toml`, plugin-provided definitions, admin pins). **Adding** a marketplace or installing a new source is always treated as foreign, so an advisory strict list still refuses unlisted `marketplace add` / `plugin install` sources.

Layers combine **strictest-wins**: any deny wins, every restricted source must allow, and boolean pins only tighten (`false` sticks; a later `true` cannot unpin). CamelCase Claude keys and snake_case grok keys are both accepted in TOML.

`distill inspect` (and `distill inspect --json`) shows the loaded MCP/marketplace lists, whether `allowManagedMcpServersOnly` is `off` / `advisory` / `enforced`, extra marketplace pins, and tighten-only pins under **Enforced by policy**.

### Roll a marketplace out to everyone

Add the source, and turn on the plugins you want, in `managed_config.toml`:

```toml
[[marketplace.sources]]
name = "My Org Plugins"
git = "https://github.com/my-org/my-org-plugins.git"

# Plugins stay off until enabled. List plugin names (from `distill plugin list`)
# or full IDs (`<scope>/<hash>/<name>`).
[plugins]
enabled = ["gdrive"]
```

For a hands-off install with no per-person step, also place the plugin's files where Grok discovers and trusts them automatically: `~/.grok/plugins/`, or a directory your device-management tool manages that you point to with `[plugins].paths`. Then enable them with `[plugins].enabled`.

A managed workspace can also sync skills to users directly, without a plugin. Synced skills appear with the `server` scope and are administered by the workspace; a user's own skill of the same name shadows the synced one. See [Skills](08-skills.md).

### Restrict which marketplaces can be added

List the only git sources people may add. Any other git URL is refused. Honored entries are `{ "source": "git", "url": "…" }` and `{ "source": "github", "repo": "owner/repo" }` (canonicalized to `https://github.com/owner/repo.git`). An optional `ref` / `branch` is stored on extra pins; it is not part of allowlist identity. `local` entries in the strict list are dropped with a warning; they never allow anything.

The key being **present** is what restricts: an empty list (`strict_known_marketplaces = []`), a list whose every entry is unsupported, or a key with the wrong type is a complete lockdown that refuses every add and install until it is fixed. Leave the key out to leave marketplaces unrestricted.

Adding a **local path** while a binding strict list is present is refused (paths never match a git-URL allowlist; fail closed), unless an **admin** `extraKnownMarketplaces` pin names that exact path. A pin from a user-writable `~/.grok` layer cannot carve that exception. Existing git sources that fail the list are dropped at load (`Marketplace source blocked by allowlist`).

```toml
# /etc/grok/requirements.toml  (native: binds every marketplace)
[[strict_known_marketplaces]]
source = "git"
url = "git@github.enterprise.example:ACME/my-org-plugins.git"

[[strict_known_marketplaces]]
source = "github"
repo = "ACME/more-plugins"
```

The same lists work in Claude `managed-settings.json` (advisory for already-configured grok-native sources). URL comparison folds case on the **scheme and host only**, and strips exactly one trailing `.git` (`repo.git.git` is a different repo). Use `distill inspect` to see the loaded allowlist.

Provision extra sources from policy with `extraKnownMarketplaces` / `extra_known_marketplaces`. First pinning layer wins a name; a configured source already holding that name with a different URL is not overwritten (logged). `autoUpdate = false` on an extra pin turns **global** session-start plugin auto-update off (there is no per-marketplace grok equivalent).

```toml
[extra_known_marketplaces.acme]
source = { source = "git", url = "https://github.com/ACME/my-org-plugins.git", ref = "main" }
```

### Restrict which MCP servers can run

Grok enforces MCP allow/deny lists from every native TOML policy layer and from Claude `managed-settings.json` (advisory; see above). `distill inspect` prints the merged lists.

Each allow or deny entry is one of:

| Field | Matches |
| --- | --- |
| `serverUrl` / `server_url` | HTTP/SSE server URL. Host and path follow the Claude `serverUrl` rules on both lists: `*` wildcards match host and path separately (`https://*.example.com/*` cannot match a lookalike path on another host); a pattern with no path (`https://mcp.example.com`, or with a bare trailing `/`) matches every path on that host; a pattern with a path matches only that path, so use `/mcp/*` to scope a grant. **Allow entries** are stricter than Claude on scheme and port. The scheme is literal or a bare `*` (`*` matches the supported remote schemes, http and https, and nothing else); a scheme-less `*.example.com/*` or a partial scheme glob such as Claude's `http*://` never matches and logs a warning at startup. Ports stay literal (an explicit `:443` on https and no port are the same target); a glob port such as Claude's `http://localhost:*/*` never matches and logs a warning at startup — list each port. **Deny entries** match by host and path across every scheme and port: `mcp.untrusted.example/*` and `http://mcp.untrusted.example:*/*` both block that host on any scheme and port, without a warning. |
| `command` | stdio executable name, exact match on the configured command (not the rest of argv). |
| `serverCommand` / `server_command` | stdio argv, exact match on `[command, args…]`. A partial array (non-string or empty) would match the wrong command: on an allow list it grants nothing; on a deny list it locks the source down (see below). |
| `serverName` / `server_name` | Config name on any transport. Comparison is case-insensitive after spaces become `_`; a `grok_com_` prefix on the runtime name is stripped. |

**Deny wins.** A server that matches `deniedMcpServers` is blocked even if it also matches `allowedMcpServers`. If `allowedMcpServers` is present, every unlisted server is blocked; a present but empty list (`allowed_mcp_servers = []`) blocks every server the file binds, so do not ship it as a scaffold. A deny-only file blocks the listed servers and leaves the rest alone (an empty deny list is harmless). Across layers, a server must pass **every** restricted source.

**Misconfiguration locks down rather than failing open.** A policy key with the wrong type (a table or string where a list belongs), a key written in both spellings with different values, an allow list whose every entry is unsupported, or a deny entry that cannot be enforced (unknown fields, a partial `serverCommand`, a `serverUrl` that can never match) locks that file's MCP policy down: every server it binds is blocked with the reason `locked down by policy (<file>)` until the file is fixed. Startup logs name the file and the offending key. An unusable **allow** entry only grants nothing.

`allowManagedMcpServersOnly = true` (or `allow_managed_mcp_servers_only`) is a lockdown: a positive allow-entry match is required even when the allow list is empty. Native TOML shows as `enforced` in inspect; Claude-only shows as `advisory` (grok-native servers exempt).

`enableAllProjectMcpServers = false` drops project-scoped MCP unless the server also matches an allow entry.

```toml
# /etc/grok/requirements.toml
allow_managed_mcp_servers_only = true
enable_all_project_mcp_servers = false

[[allowed_mcp_servers]]
server_url = "https://*.example.com/*"

[[allowed_mcp_servers]]
command = "npx"

[[allowed_mcp_servers]]
server_command = ["npx", "@corp/mcp"]

[[allowed_mcp_servers]]
server_name = "linear"

[[denied_mcp_servers]]
command = "node"

[[denied_mcp_servers]]
server_url = "https://mcp.untrusted.example/*"
```

The lists apply after Grok merges user, project, plugin, and imported MCP config. A blocked server is dropped from the session (logged as `MCP server blocked by managed settings policy`) with a reason of matching `deniedMcpServers`, not in `allowedMcpServers`, locked down by policy, or the project-MCP pin, plus the policy file path (inspect/JSON/logs keep the full path; user-facing refusals show the file name).

The deployment can also send MCP servers to users directly. Native allowlists still bound what any configuration, managed or personal, is allowed to run.

### Turn off session-start plugin auto-update

`plugin_auto_update = false` / `pluginAutoUpdate = false` is tighten-only. This global pin is Grok's own key with no Claude counterpart. When pinned, session start does not scan marketplaces or fan out per-plugin updates (no toast). Manual `distill plugin update` still works. Claude's per-marketplace `extraKnownMarketplaces.<name>.autoUpdate: false` pins the same global switch.

### Require pinned versions

Refuse any remote plugin install or update that is not pinned to a full commit sha:

```toml
[marketplace]
require_sha = true
```

You can also set `GROK_MARKETPLACE_REQUIRE_SHA=1`. Both only tighten the policy; neither turns it back off. Publish `sha` values in your marketplace's `plugin-index.json` so installs from it satisfy the rule. Plugins vendored directly inside a marketplace repository are copied from that repository's checkout, so pin them the same way, with `sha` values in `plugin-index.json`.

### Turn off the plugins UI

To hide the plugins and hooks interface, set this in `pager.toml`:

```toml
disable_plugins = true
```

### What this does not cover

Marketplaces distribute Grok content: skills, commands, agents, hooks, and MCP server configurations. They do not install a program onto a machine. A skill or MCP server that runs a helper binary (for example a custom sign-in tool) still needs that binary delivered separately, bundled with your deployment or pushed through your device-management tool.

---

## Troubleshooting

**A plugin you installed isn't showing up.** Plugins are off until enabled. Check `distill plugin list`, then add the plugin's name or ID to `[plugins].enabled`, or press `Space` on it in the Plugins tab. Reload with `r` in the Plugins tab or start a new session.

**A plugin's skills, hooks, or MCP servers don't load.** They stay inactive until the plugin is trusted. Reinstall with `--trust`, or place the plugin under `~/.grok/plugins/` (auto-trusted). See [Trust and security](#trust-and-security).

**A skill or MCP server from a marketplace is missing.** Refresh the source with `distill plugin marketplace update`, confirm the plugin is installed and enabled, and, if your organization restricts sources, check that the marketplace is still allowed (see [Distribute across an organization](#distribute-across-an-organization)). Some MCP servers require a sign-in and will not appear until you authenticate.

**An MCP server is configured but never starts.** Org policy may have blocked it. `distill inspect` lists `allowedMcpServers` / `deniedMcpServers`, `mcpManagedServersOnly`, any locked-down policy files, and each server's source. A deny match, an allowlist / lockdown that does not grant the server, a locked-down policy file, or `enableAllProjectMcpServers = false` on a project-scoped server drops it before spawn. See [Restrict which MCP servers can run](#restrict-which-mcp-servers-can-run).

**Adding a marketplace is refused.** A `strictKnownMarketplaces` list is in effect. Only the listed git / GitHub URLs can be added; local-path adds are refused unless an admin `extraKnownMarketplaces` pin names that exact path. If `distill inspect` shows the list as locked down, the key is present but empty, malformed, or names only unsupported sources, and nothing can be added until it is fixed.

**An install is refused as unpinned.** Your deployment requires pinned commits. Install an exact commit (`owner/repo@<sha>`), or use a marketplace whose `plugin-index.json` publishes `sha` values. See [Require pinned versions](#require-pinned-versions).

**See exactly what loaded.** Run `distill inspect` (add `--json` for machine-readable output) to list every discovered plugin and the skills, agents, hooks, and MCP servers it provides, each labeled with its `plugin: <name>` source.

---

## Reference

### What a plugin contains

A plugin is a directory with any combination of:

- **Skills**: a `skills/` directory of SKILL.md files
- **Slash commands**: a `commands/` directory
- **Agents**: an `agents/` directory
- **Hooks**: a `hooks/hooks.json` file
- **MCP servers**: a `.mcp.json` file
- **LSP servers**: a `.lsp.json` file

An optional `plugin.json` manifest can override paths or add metadata; without one, Grok discovers components from these standard directories. For example, a `team-tools` plugin might bundle a deploy skill, a code-review agent, pre-commit hooks, and a Linear MCP server, installed together in one step.

A skill or command may ship a **helper script** next to its SKILL.md (for example a Python file it calls). Put the script in the plugin and have the skill run it by relative path; it is copied to the machine with the plugin. The script's runtime and any packages it imports must already be present, plugins deliver files, not runtimes or native binaries (see [What this does not cover](#what-this-does-not-cover)).

### Where Grok looks for plugins

Grok discovers plugins from these locations, in priority order. The `.claude/plugins/` equivalents also work, and when two plugins share a name the higher-priority one wins:

| Location | Scope | Trust |
|----------|-------|-------|
| `_meta.pluginDirs` (`session/new` / `session/load`) | Session, that session only | Trusted automatically |
| `--plugin-dir` (the `distill agent … stdio` flag) | Process, that agent process only | Trusted automatically |
| `.grok/plugins/` | Project, shared through version control | Requires trust |
| `~/.grok/plugins/` | User, every project | Trusted automatically |
| `[plugins].paths` (config) | Custom directories you add | Depends on location |

The `_meta.pluginDirs` field on the `session/new` and `session/load` requests loads plugins for a single session; because the caller supplies the directory, those plugins are trusted automatically and do not persist after the session. `--plugin-dir` is the process-wide equivalent for a dedicated `distill agent … stdio` process, repeatable (`distill agent --no-leader --plugin-dir A --plugin-dir B stdio`), and ignored in leader mode, where the shared leader discovers its own plugins.

### Environment variables in plugin hooks

Plugin hooks receive two variables beyond the standard hook environment:

| Variable | Description |
|----------|-------------|
| `GROK_PLUGIN_ROOT` | Absolute path to the plugin's installed directory. |
| `GROK_PLUGIN_DATA` | Absolute path to the plugin's writable data directory, for state, caches, and logs. |

Grok sets these and overrides any same-named value in the hook's `env` map (the `CLAUDE_PLUGIN_ROOT` and `CLAUDE_PLUGIN_DATA` aliases are set too). See the [Hooks guide](10-hooks.md) for every variable passed to hooks.

### Keyboard shortcuts

These keys work across every tab in the plugins modal:

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Next / previous tab |
| `j` / `k` or arrow keys | Move the selection |
| `Enter` | Expand or collapse the selected item |
| `/` | Search the current tab by name |
| `Esc` | Clear the search, or close the modal |
