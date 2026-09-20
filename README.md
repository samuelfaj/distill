<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Distill

![distill in action](https://raw.githubusercontent.com/samuelfaj/distill/main/screenshot.png)

Distill is a lightweight coding agent harness and TUI, built to **get far more done
with far fewer tokens.**

It works with Grok and Codex subscriptions, and with any model that speaks the
OpenAI-compatible protocol.

**Steps:**

1 - Download distill.
2 - Login with openrouter.
3 - Login with your subscription.
4 - Save money.

## Install

### Mac / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/samuelfaj/distill/main/install.sh | sh   # macOS / Linux / Git Bash
export PATH="$HOME/.local/share/distill/bin:$PATH"                                    # macOS / Linux
distill --version
```

### Windows

```sh
irm https://raw.githubusercontent.com/samuelfaj/distill/main/install.ps1 | iex        # Windows PowerShell
$bin = "$env:LOCALAPPDATA\distill\bin"; [Environment]::SetEnvironmentVariable('Path', "$bin;" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')   # Windows PowerShell, also read by Git Bash
distill --version
```

## Upgrade

For an installation made with the release installer:

```sh
distill update
```

## Choose your models

Open **Model tiers** on the home screen, click **change** beside the current
model, or enter `/tiers`. You can edit each tier in that screen.

| Tier | Purpose | Picker |
|---|---|---|
| Reasoning model | Handles the main conversation, difficult reasoning, and code changes. | `/model` or `/tiers reasoning` |
| Worker model | Takes suitable calls in the same conversation when a lighter model can handle them. Optional. | `/worker-model` or `/tiers worker` |
| Utility model | Handles bounded tasks such as extraction, summaries, and compression. | `/utility-model` or `/tiers utility` |

Each picker helps you choose a model and its effort. `auto` is available and is
the default. You can also supply the selection directly:

```text
/model gpt-6-astra auto
/worker-model gpt-5.6-luna auto
/utility-model openrouter-qwen37 auto
```

### Read More:

- [Accounts and local models](docs/local-models.md)
- [How Jev routes work](docs/jev-routing.md)
- [Where the token savings come from](docs/token-saver.md)
