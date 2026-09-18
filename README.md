<div align="center">

<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://media.x.ai/v1/website/spacexai-symbol-white-transparent-0c31957f.png">
    <source media="(prefers-color-scheme: light)" srcset="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png">
    <img alt="SpaceXAI logo" src="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png" width="96">
  </picture>
  <br>
  Grok Build (<code>grok</code>)
</h1>

**Grok Build** is SpaceXAI's terminal-based AI coding agent. It runs as a
full-screen TUI that understands your codebase, edits files, executes shell
commands, searches the web, and manages long-running tasks — interactively,
headlessly for scripting/CI, or embedded in editors via the Agent Client
Protocol (ACP).

[Installing the released binary](#installing-the-released-binary) ·
[Building from source](#building-from-source) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Contributing](#contributing) ·
[License](#license)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

**Learn more about Grok Build at [x.ai/cli](https://x.ai/cli)**

This repository contains the Rust source for the `grok` CLI/TUI and its agent
runtime. It is synced periodically from the SpaceXAI monorepo.

A small `SOURCE_REV` file at the root records the full monorepo commit SHA
for the version of the code present in this tree.

</div>

---

## Installing the released binary

Prebuilt binaries are published for macOS, Linux, and Windows:

```sh
curl -fsSL https://x.ai/cli/install.sh | bash   # macOS / Linux / Git Bash
irm https://x.ai/cli/install.ps1 | iex          # Windows PowerShell
grok --version
```

See the [changelog](https://x.ai/build/changelog) for the latest fixes,
features, and improvements in each release.

## Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/xai-grok-pager
cargo check -p xai-grok-pager-bin            # fast validation
```

The binary artifact is named `xai-grok-pager`; official installs ship it as
`grok`. On first launch it opens your browser to authenticate — see the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

Full online documentation is available at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview).

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more.

## Jev — the local decision layer

This fork routes the harness's **structured decisions** to
[Jev](https://docs.typesafe.ai) (TypeSafe System One) instead of paying a model
to make them. Jev answers typed `choice` / `score` / `noul` questions over a
bounded `state` with probabilities and confidence, at roughly $0.042 per
million input tokens with output free (measured ~400 ms per battery). Jev only
answers; the harness composes the decision in code, and everything Jev cannot do
(text generation, arithmetic, embeddings, vision) stays with the model.

**What is wired.** The full catalogue of 23 decision points — where it lives and
which test covers it — is [`todo.md`](todo.md). In short:

| Area | Items | Effect |
|------|-------|--------|
| Permission (live today) | classifier, YOLO brake | route routine actions to *allow*, refuse a confident catastrophe in always-approve mode |
| A — content selection | which file to edit, which lines matter, which search results to read, which memories to inject, which test to run | narrows what the model re-reads; never opens more than the code already offered |
| B — effort routing | turn intent, tool-family pruning, model/effort tier, subagent type, skill suggestion, delegation hint | prunes tools per turn, names the relevant announced skill, resolves an unknown subagent type, hints at delegation |
| C — verification | premature stop, failure triage, completion check, diff risk, error priority, injection screen, change type | adds hints and refuses to call unfinished work complete |
| D — context and cost | compaction recorte, big-output retention, post-compaction retrieval, call validation | keeps the summary and context small; holds a call that looks out of scope |

**Authority is tighten-only.** No item can widen what the harness already
allows; every item may only narrow, reorder, annotate or refuse. Any failure,
timeout, missing answer or wrong-typed answer keeps today's path, and items that
drop content (compaction recorte, big-output retention, read narrowing) carry
their own flag.

**Per-item switches.** Every item is behind its own key in `~/.grok/config.toml`:

```toml
[jev]
enabled = true                    # master switch (env: GROK_JEV)

[jev.ladder]
p1_tool_family = true             # B4: prune tool families for the turn
a1_file_to_edit = true            # A1: rank candidate files
p3_compaction_recorte = true      # D1: which segments the summarizer must see
b2_model_tier = false             # B2: the money lever — off until its gate passes
c6_injection_screen = false       # C6: off until its per-output cost is measured
# …every item has the same shape; see `JevLadderConfig` in
# crates/codegen/xai-grok-shell/src/agent/config.rs
```

**Kill switch.** `GROK_JEV=0` (or `[jev] enabled = false`) makes the harness
behave exactly as before: no Jev client is built and **no connection is opened**.
The prompt footer shows the state at a glance: `jev` (active), `jev·shadow`
(records only), `jev·veto` (always-approve brake), `jev:off` (disabled).

**Where decisions are visible.** Set `GROK_LOG_JEV=1` to append one JSON line per
decision to `~/.grok/logs/jev.jsonl` (lever, decision, confidence, model,
tokens, request id). The user-facing reference is
[`crates/codegen/xai-grok-pager/docs/user-guide/28-jev-decisions.md`](crates/codegen/xai-grok-pager/docs/user-guide/28-jev-decisions.md).

**How it is tested.** Pure pack logic and thresholds are unit-tested in
`xai-grok-workspace` (`cargo test -p xai-grok-workspace --lib jev::`); the wire
contract, the permission gate and the selection/retention batteries run against
the real API in `crates/codegen/xai-grok-workspace/tests/jev_live.rs`
(`--ignored`, needs `JEV_API_KEY`).

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `xai-grok-pager` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

## Contributing

> [!NOTE]
> External contributions are not accepted. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

First-party code in this repository is licensed under the **Apache License,
Version 2.0** — see [`LICENSE`](LICENSE).

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
