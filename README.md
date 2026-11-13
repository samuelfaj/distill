<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Distill

![distill in action](https://raw.githubusercontent.com/samuelfaj/distill/main/screenshot.png)

Distill is a lightweight coding agent harness and TUI, built to **get far more done
with far fewer tokens.**

It works with Grok, Claude and Codex subscriptions, and with any model that speaks the
OpenAI-compatible protocol.

**Steps:**

1. Download distill.
2. Login with openrouter.
3. Login with your subscription.
4. Save money.

----

## One prompt, five modes

The same prompt in each mode, with Claude Opus 5.5 as the main model and
DeepSeek V4.1 Flash Ultrafast as the worker:

> Create a view of moon surface and the sky, photorealistic. in three.js

Open any scene from the table to run it in your browser, or find all five on
the [Distill page](https://www.samuelfaj.com/en/distill/#benchmarks).

| Mode | Result | Cost | Time | Tokens | Model calls |
| --- | --- | ---: | ---: | ---: | --- |
| [**UltraSave**](https://www.samuelfaj.com/files/distill/moon/ultrasave/) | <a href="https://www.samuelfaj.com/files/distill/moon/ultrasave/"><img src="docs/images/moon/ultrasave.webp" width="320" alt="UltraSave: cratered plain with boulders, the Sun and a crescent Earth"></a> | **$0.058** (0.40x) | 16 min 18 s (3.57x) | 6.2M | 86 DeepSeek |
| [**UltraFast**](https://www.samuelfaj.com/files/distill/moon/ultrafast/) | <a href="https://www.samuelfaj.com/files/distill/moon/ultrafast/"><img src="docs/images/moon/ultrafast.webp" width="320" alt="UltraFast: large boulders casting shadows under a starfield with Earth"></a> | $0.577 (4.00x) | 8 min 7 s (1.77x) | 0.5M | 18 Opus |
| [**UltraCode**](https://www.samuelfaj.com/files/distill/moon/ultracode/) | <a href="https://www.samuelfaj.com/files/distill/moon/ultracode/"><img src="docs/images/moon/ultracode.webp" width="320" alt="UltraCode: rugged craters in low sunlight with the Sun and a crescent Earth"></a> | $1.011 (7.00x) | 15 min 29 s (3.39x) | 7.8M | 19 Opus, 73 DeepSeek |
| [**Auto**](https://www.samuelfaj.com/files/distill/moon/auto/) | <a href="https://www.samuelfaj.com/files/distill/moon/auto/"><img src="docs/images/moon/auto.webp" width="320" alt="Auto: bright regolith plain with distant craters and Earth"></a> | $0.508 (3.52x) | 15 min 29 s (3.39x) | 6.0M | 8 Opus, 77 DeepSeek |
| [**Normal**](https://www.samuelfaj.com/files/distill/moon/normal/) | <a href="https://www.samuelfaj.com/files/distill/moon/normal/"><img src="docs/images/moon/normal.webp" width="320" alt="Normal: rolling terrain with rocks and Earth above the horizon"></a> | $0.145 (1.00x) | **4 min 34 s** (1.00x) | 1.9M | 5 Opus, 32 DeepSeek |

Ratios are against Normal. One run per mode, all five started at the same time
on 2026-11-12 with distill 2.0.9 (9bbf0aafec73), the commit of v2.0.72; each
finished with exit code 0. A single run per mode carries run-to-run variation:
here Normal handed the build to the worker once and accepted the first result,
so it finished first, while UltraFast kept all 18 rounds on Opus. UltraSave led
on the worker, and UltraCode ended with `Verification: REVIEWED`. Cost is an
estimate: Opus at list API prices ($4/M input, $0.20/M cached, $5/M cache
write, $20/M output; the runs used a Claude subscription) and DeepSeek at
OpenGateway's billed prices ($0.09/M input, $0.36/M output, $0.0018/M cached),
without the Jev routing calls, which have no published price. Thumbnails are
frames captured in headless Chrome at 1280×720.

## Benchmarks

[![Distill benchmarks of 2026-10-16 (Distill 2.0.46): same quality at up to 87% lower cost and up to 76% less time versus native GPT-6 Astra, GPT-6.1 Sol, Claude Opus 5.5 and DeepSeek V4.1 max baselines](docs/images/benchmarks.webp)](BENCHMARKS.md)

The image shows the 2026-10-16 campaign on Distill 2.0.46. See
[BENCHMARKS.md](BENCHMARKS.md) for the current tables, including one row per
execution mode, with definitions, methodology, cost bounds, reproduction
commands, and source JSON.

## Install

### Mac / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/samuelfaj/distill/main/install.sh | sh  
```

The installer adds `~/.local/share/distill/bin` to your PATH by writing to your shell's rc file (`~/.zshrc`, `~/.bashrc` or `~/.bash_profile`, `~/.config/fish/config.fish`, or `~/.profile`), so open a new terminal afterwards. Set `DISTILL_NO_MODIFY_PATH=1` to skip this and add the directory yourself. In the new terminal, run `distill --version` to check.

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
| Main model | Required. Owns every session: it plans, specifies, delegates, and reviews the work. | `/model` or `/tiers main` |
| Worker model | Optional. Runs every delegated assignment the main model hands it as a precise spec. Without it, the main model does all the work. | `/worker-model` or `/tiers worker` |
| Utility model | Handles bounded tasks such as extraction, summaries, and compression. | `/utility-model` or `/tiers utility` |

Pick your strongest model as the main model and a cheaper one as the worker:
the main model gives the worker as much of the work as it can do well. Each
tier takes an effort level or `auto`, which lets Jev pick the effort for every
call; `auto` is the default. In the Model tiers screen, enter a tier as
`model effort` (for example `gpt-6-luna auto`). You can supply each selection
directly:

```text
/model gpt-6-sol auto
/worker-model gpt-6-luna auto
/utility-model openrouter-qwen37 auto
```

## Always-on behavior

Two rule sets are built into every system prompt, for the main model and for
its subagents, and neither has an off switch:

- **Caveman**: the agent writes terse prose and keeps code, paths and error
  strings exact. `/caveman lite|full|ultra` changes how hard it compresses
  (default `full`); a stored `off` is ignored.
- **Ponytail**: the agent works like a lazy senior developer. Before writing
  code it asks whether the code needs to exist, then reuses what the codebase
  has, then the standard library, then an installed dependency, and only then
  writes the minimum that works. Validation, error handling, security and
  anything you asked for are never cut. Adapted from
  [ponytail](https://github.com/DietrichGebert/ponytail) (MIT).

### Read More:

- [Accounts and local models](docs/local-models.md)
- [How Jev routes work](docs/jev-routing.md)
- [UltraCode activation, hierarchy limits and paired comparisons](docs/ultracode.md)
- [UltraFast execution mode and paired speed/quality evaluation](docs/ultrafast.md)
- [UltraSave execution mode and paired token/quality evaluation](docs/ultrasave.md)
- [Where the token savings come from](docs/token-saver.md)
