<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Project Rules (AGENTS.md)

Project rules let you configure Grok per project or directory. By placing an AGENTS.md file in your repository, you can set coding conventions, build instructions, style guides, and any other instructions that Grok should follow when working in that codebase. Startup loading requires folder trust (`--trust` or an interactive grant).

---

## What Are Project Rules?

Project rules are Markdown files that Distill reads and adds to its context. Grok follows their content for every interaction in that tree.

This is the primary mechanism for teaching Grok about your project's conventions, so you need not restate them each session.

---

## Supported File Names

Grok checks for these filenames (in this order) within each directory:

- `Agents.md`
- `Claude.md`
- `CLAUDE.md`
- `CLAUDE.local.md`
- `AGENT.md`
- `AGENTS.md`

Distill loads every matching file in a directory, so a folder that contains both `AGENTS.md` and `CLAUDE.md` contributes both. On case-insensitive filesystems, names that resolve to the same file (such as `Agents.md` and `AGENTS.md`) are deduplicated and counted once. `Claude.md`, `CLAUDE.md`, and `CLAUDE.local.md` are supported for compatibility with Claude Code workflows. When Claude compatibility is enabled (the default), Grok also scans your home-level `~/.claude/` directory for these filenames and, at each directory level, checks `.claude/CLAUDE.md` and `.claude/CLAUDE.local.md` -- the locations Claude Code uses for project memory. With Cursor compatibility enabled, the home-level `~/.cursor/` directory is scanned the same way.

### Rules Directories

In addition to AGENTS.md files, Grok scans for `*.md` files in rules directories at each level (`<dir>`) from the repo root to the current working directory:

| Location | Notes |
|----------|-------|
| `<dir>/.grok/rules/` | Always scanned |
| `<dir>/.claude/rules/` | Claude compatibility (configurable) |
| `<dir>/.cursor/rules/` | Cursor compatibility (configurable) |

Grok also scans home-level rules, regardless of where it starts. These roots are already vendor-specific, so rules live directly under `rules/`:

| Location | Notes |
|----------|-------|
| `$GROK_HOME/rules/` (default `~/.grok/rules/`) | Always scanned; applies to all projects |
| `~/.claude/rules/` | Controlled by `compat.claude.rules` |
| `~/.cursor/rules/` | Controlled by `compat.cursor.rules` |
| Each entry of `[paths] extra_rule_dirs` | Any absolute directory you list in `config.toml`; `~` is expanded |

Home rules load first, in the table order, followed by project files from repo root to the current directory. Files are alphabetical within each rules directory. To load rules from a directory that is not one of the built-in locations, list it under `[paths]`:

```toml
[paths]
extra_rule_dirs = ["~/team-rules", "/opt/company/grok-rules"]
```

Every `*.md` directly inside a listed directory is loaded as a rule (subdirectories are not scanned), in every project and regardless of folder trust, the repository's `.gitignore`, or the compatibility cells; the model receives them as user rules and `distill inspect` lists them as `global`. Entries must be absolute or start with `~/`; a relative or missing entry loads nothing. `/import-claude` writes your existing `~/.claude/rules/` here so it keeps loading after the Claude compatibility scan is turned off. The vendor `rules` cells control both home and project rules independently of the corresponding `agents` cells. Claude's `agents` cell controls named files under `~/.claude/` and project `<dir>/.claude/CLAUDE*.md`; generic top-level names such as `Claude.md`, `CLAUDE.md`, and `CLAUDE.local.md` remain recognized. See [Configuration](05-configuration.md#harness-compatibility).

---

## How Discovery Works

Grok scans for project rules in this order:

1. **Home rules**: `$GROK_HOME`, then enabled `~/.claude/` and `~/.cursor/` sources, then `[paths] extra_rule_dirs`
2. **Repo rules**: If inside a git repo, every directory from the repo root down to the current working directory (inclusive)
3. **CWD-only**: If not inside a git repo, only the current working directory

### Example

Given this project structure:

```
~/projects/my-app/
  AGENTS.md              # "Use TypeScript. Follow ESLint rules."
  src/
    AGENTS.md            # "Prefer functional components."
    components/
      AGENTS.md          # "Use CSS modules for styling."
```

When Distill runs in `~/projects/my-app/src/components/`, it loads all three files. The instructions accumulate, so Grok sees all of them.

### Deeper Files Take Precedence

Grok orders the files from the repo root to the current working directory, so files in deeper directories appear later in its context and take precedence when instructions conflict. In the example above, if the root says "Use styled-components" but `components/AGENTS.md` says "Use CSS modules", the CSS modules instruction wins because it appears later.

### Auto-Loading Behavior

- Distill loads the files from the repo root to the current working directory automatically at session start.
- When Distill reads, lists, or edits files in directories outside that initial set, it detects any project instruction files there, notes their paths, and reads them when they apply to the task.

---

## What to Put in Project Rules

### Coding Conventions

```markdown
# Coding Standards

- Use TypeScript for all new code
- Prefer functional components with hooks over class components
- Use `const` by default; only use `let` when reassignment is needed
- Maximum line length: 100 characters
```

### Build and Test Instructions

```markdown
# Build & Test

- Run `npm test` before committing
- Use `npm run lint` to check code style
- Build with `npm run build` -- ensure no TypeScript errors
- Integration tests: `npm run test:e2e` (requires Docker)
```

### Style Guides

```markdown
# Style Guide

- Follow the Airbnb JavaScript Style Guide
- Use 2-space indentation
- Always use trailing commas in multi-line arrays/objects
- Prefer template literals over string concatenation
```

### PR and Commit Requirements

```markdown
# Version Control

- Write commit messages in conventional commits format
- Prefix branch names with `feature/`, `fix/`, or `chore/`
- All PRs require at least one approval before merge
- Squash-merge feature branches
```

### Architecture Notes

```markdown
# Architecture

- API routes go in `src/routes/` with one file per resource
- Business logic goes in `src/services/`
- Database queries go in `src/repositories/`
- Never import from `src/routes/` in `src/services/`
```

---

## Scoping Rules to Subdirectories

AGENTS.md files scope to the entire directory tree rooted at their folder. Use this to provide different instructions for different parts of your codebase:

```
my-monorepo/
  AGENTS.md                    # Monorepo-wide rules
  packages/
    frontend/
      AGENTS.md                # "Use React. Prefer CSS modules."
    backend/
      AGENTS.md                # "Use Express. Follow REST conventions."
    shared/
      AGENTS.md                # "No framework-specific code in this package."
```

---

## Session Rules Flags

To add rules for a single session without editing files, pass `--rules` (alias `--append-system-prompt`):

```bash
distill --rules "Always use TypeScript. Prefer functional components."
```

Grok appends this text to the session's system prompt. Use it for session-specific customization.

To replace the system prompt entirely, pass `--system-prompt-override` (alias `--system-prompt`). Distill uses the text verbatim and skips both the default system prompt and `--rules`. (Text passed with `--rules`, by contrast, is wrapped in a `<human_rules>` block and appended to the default prompt.)

---

## File Size

Distill loads each project instruction file in full; there is no character cap and no truncation. Even so, keep instructions concise and focused. Shorter, specific rules are easier for Grok to follow than long ones, and every file you load consumes context.

---

## Gitignore Filtering

Files ignored by `.gitignore` are skipped during discovery. To keep personal overrides out of the shared repository, gitignore a recognized filename such as `CLAUDE.local.md`:

```gitignore
# .gitignore
CLAUDE.local.md
```

As top-level instruction files, Grok discovers only the recognized filenames listed under [Supported File Names](#supported-file-names) — not custom names such as `AGENTS.local.md` or `notes.md`. (Inside a rules directory such as `.grok/rules/`, every `*.md` file is loaded regardless of name.)

---

## The .grok/ Project Directory

Beyond AGENTS.md files, the `.grok/` directory in your project root can contain additional project-level configuration:

| Path | Purpose |
|------|---------|
| `.grok/config.toml` | Project-scoped MCP servers, plugins, and permission rules (other settings load only from `~/.grok/config.toml`) |
| `.grok/skills/` | Project-scoped skill definitions |
| `.grok/plugins/` | Project-scoped plugins |
| `.grok/agents/` | Project-scoped agent definitions |
| `.grok/hooks/` | Project-scoped lifecycle hooks |
| `.grok/lsp.json` | LSP server configuration |

These are all optional. See the respective guides for details on each.

---

## Inspecting Loaded Rules

Use `distill inspect` to see all loaded project instructions:

```bash
distill inspect
```

This shows each project instruction file it finds, with its path and approximate token count. Use it to confirm Grok picks up your rules.

---

## Best Practices

1. **Start with the root.** Put the most important, project-wide rules in the repo root AGENTS.md.

2. **Be specific.** "Use TypeScript" is better than "Use modern JavaScript". "Run `cargo fmt` before committing" is better than "Format your code".

3. **Keep it short.** Concise instructions are more likely to be followed than lengthy ones.

4. **Use subdirectory scoping for large repos.** Different parts of a monorepo may have different conventions. Use per-directory AGENTS.md to scope rules appropriately.

5. **Version control your rules.** Commit AGENTS.md to the repository so the whole team benefits. User-specific overrides belong in `~/.grok/` (global rules).

6. **Do not duplicate documentation.** AGENTS.md should contain actionable instructions, not a copy of your project's README. Link to external docs if needed.

7. **Review periodically.** As your project evolves, update your rules to match current conventions.
