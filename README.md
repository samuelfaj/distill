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

## Modes

Each session runs in one execution mode. Auto is the default. Switch with a
slash command in the TUI, or pass a flag to headless prompts.

<table>
<tr>
<td width="25%" valign="top">

### UltraSave

<!-- benchmarks:ultrasave:begin -->
# 94–99% cheaper

UltraSave cost **0.0105x–0.0571x the estimated cost of Normal** with the mode pinned on the requested cohort: Astra (GPT-6 Astra + GPT-6 Luna), Sol (GPT-6.1 Sol + GPT-6 Luna), Opus (Opus 5.5 + Haiku 5.5); 27/27 runs passed against Normal's 27/27. Source: [summary](tools/task_cost_eval/results/requested-20261112-summary.json), 2026-11-12; distill 2.0.9 (39f1c6fe0bf8).
<!-- benchmarks:ultrasave:end -->

It leads on your cheaper worker model when you have one, keeps effort at high
or lower, and compacts a long session early. Required checks still run before
a task counts as done.

`/ultrasave on` or `--ultrasave`

</td>
<td width="25%" valign="top">

### UltraFast

<!-- benchmarks:ultrafast:begin -->
# 19–59% less time

UltraFast took **0.411x–0.814x the wall time of Normal** with the mode pinned on the requested cohort: Astra (GPT-6 Astra + GPT-6 Luna), Sol (GPT-6.1 Sol + GPT-6 Luna), Opus (Opus 5.5 + Haiku 5.5); 27/27 runs passed against Normal's 27/27. Source: [summary](tools/task_cost_eval/results/requested-20261112-summary.json), 2026-11-12; distill 2.0.9 (39f1c6fe0bf8).
<!-- benchmarks:ultrafast:end -->

It goes straight to the work on your own model: no subagent, no waiting on
side decisions, effort set once per turn at medium or lower. Verification
steps stay.

`/ultrafast on` or `--ultrafast`

</td>
<td width="25%" valign="top">

### UltraCode

<!-- benchmarks:ultracode:begin -->
# No quality gain measured

UltraCode passed **27/27 runs against Normal's 27/27** at **2.19x–3.17x the estimated cost** and **1.5x–2.02x the wall time of Normal** with the mode pinned on the requested cohort: Astra (GPT-6 Astra + GPT-6 Luna), Sol (GPT-6.1 Sol + GPT-6 Luna), Opus (Opus 5.5 + Haiku 5.5). Source: [summary](tools/task_cost_eval/results/requested-20261112-summary.json), 2026-11-12; distill 2.0.9 (39f1c6fe0bf8).
<!-- benchmarks:ultracode:end -->

It puts correctness first: effort is never lowered for a routine round, file
reads stay whole, and after a turn that changed code an independent reviewer
checks the patch. The final message ends with `Verification: REVIEWED` or
`UNVERIFIED`. The cohort above is made of tasks every mode already passes, so
it shows what the review costs, not what it catches.

`/ultracode on` or `--ultracode`

</td>
<td width="25%" valign="top">

### Auto

<!-- benchmarks:auto:begin -->
# 51–59% cheaper, 0.833x–1.18x Normal's time

Distill took **0.414x–0.491x the estimated cost** and **0.833x–1.18x the wall time of Normal** under Auto on the requested cohort: Astra (GPT-6 Astra + GPT-6 Luna), Sol (GPT-6.1 Sol + GPT-6 Luna), Opus (Opus 5.5 + Haiku 5.5); 27/27 runs passed against Normal's 27/27. Source: [summary](tools/task_cost_eval/results/requested-20261112-summary.json), 2026-11-12; distill 2.0.9 (39f1c6fe0bf8).
<!-- benchmarks:auto:end -->

It picks a mode for each request and only moves up inside it, when the work
turns out harder. Subagents inherit Auto and adapt independently.

`/mode auto` or `--mode auto`

</td>
</tr>
</table>

### One prompt, five modes

The same prompt in each mode, with DeepSeek V4.1 Flash Ultrafast as the main
model and GLM 5.3 Flash Ultrafast as the worker (both through OpenGateway):

> Create a view of moon surface and the sky, photorealistic. in three.js

| Mode | Result | Cost | Time | Tokens | Model calls |
| --- | --- | ---: | ---: | ---: | --- |
| **UltraSave** | <img src="docs/images/moon/ultrasave.webp" width="320" alt="UltraSave: grey terrain under a starfield"> | **$0.0019** (0.013x) | **2 min 5 s** (0.047x) | 0.39M | 21 GLM |
| **UltraFast** | <img src="docs/images/moon/ultrafast.webp" width="320" alt="UltraFast: cratered terrain with low-sun shadows and Earth"> | $0.0873 (0.62x) | 31 min 9 s (0.71x) | 15.9M | 143 DeepSeek |
| **UltraCode** | <img src="docs/images/moon/ultracode.webp" width="320" alt="UltraCode: boulder field with long shadows, Earth and a faint Milky Way"> | $0.2070 (1.46x) | 63 min 36 s (1.44x) | 41.8M | 200 DeepSeek |
| **Auto** | <img src="docs/images/moon/auto.webp" width="320" alt="Auto: rock-strewn plain with Earth in the sky"> | $0.0620 (0.44x) | 22 min 43 s (0.52x) | 9.2M | 90 DeepSeek |
| **Normal** | <img src="docs/images/moon/normal.webp" width="320" alt="Normal: craters and boulders under the Milky Way with Earth"> | $0.1417 (1.00x) | 44 min 4 s (1.00x) | 30.1M | 175 DeepSeek |

Ratios are against Normal. One run per mode, all five started at the same time
on 2026-11-12 with distill 2.0.9 (39f1c6fe0bf8); each finished with exit code 0.
Cost uses OpenGateway's billed prices that day (DeepSeek $0.09/M input,
$0.36/M output, $0.0018/M cached; GLM $0.015/M, $0.05/M, $0.003/M) and leaves
out the Jev routing calls, which have no published price. UltraCode ended with
`Verification: UNVERIFIED`. Thumbnails are frames captured in headless Chrome
at 1280×720.

Details:
[UltraSave](docs/ultrasave.md), [UltraFast](docs/ultrafast.md) and
[UltraCode](docs/ultracode.md).

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
