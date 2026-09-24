<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Distill

![distill in action](https://raw.githubusercontent.com/samuelfaj/distill/main/screenshot.png)

Distill is a lightweight coding agent harness and TUI, built to **get far more done
with far fewer tokens.**

It works with Grok and Codex subscriptions, and with any model that speaks the
OpenAI-compatible protocol.

**Steps:**

1. Download distill.
2. Login with openrouter.
3. Login with your subscription.
4. Save money.

----

## Install

### Mac / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/samuelfaj/distill/main/install.sh | sh  
export PATH="$HOME/.local/share/distill/bin:$PATH"                                  
distill --version
```

### Windows

```sh
irm https://raw.githubusercontent.com/samuelfaj/distill/main/install.ps1 | iex        
$bin = "$env:LOCALAPPDATA\distill\bin"; [Environment]::SetEnvironmentVariable('Path', "$bin;" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User') 
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
| Main model | Required. Runs every session and every step. | `/model` or `/tiers main` |
| Reasoning model | Optional. Plans and reviews the steps the main model cannot do alone. | `/reasoning-model` or `/tiers reasoning` |
| Utility model | Handles bounded tasks such as extraction, summaries, and compression. | `/utility-model` or `/tiers utility` |

The main model picker also lets you set effort; `auto` is the default. You can
supply each selection directly:

```text
/model gpt-6-luna auto
/reasoning-model gpt-6-sol
/utility-model openrouter-qwen37 auto
```

### Read More:

- [Accounts and local models](docs/local-models.md)
- [How Jev routes work](docs/jev-routing.md)
- [Where the token savings come from](docs/token-saver.md)
