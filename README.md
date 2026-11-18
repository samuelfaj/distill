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

The same complete prompt, run in all five modes. Final Paris, 1888 results compare
`main` (2ff8b021) with PR #75 at `1e10a3cb`. See the [benchmark page](https://www.samuelfaj.com/en/distill/#benchmarks), [full report](docs/d1.2-decision-results.md#paris-1888-the-task-benchmark), and [final PR #75 comment](https://github.com/samuelfaj/distill/pull/75#issuecomment-6985680352). Each final montage below shows `main` above the PR result; it is not one of the earlier single-run scenes.

### Paris, 1888

The task: build a 14-second deterministic voxel animation of the Eiffel Tower
under construction. The final comparison below shows still frames from the
animations.

<details>
<summary>Prompt</summary>

```text
Build a single self-contained three.js HTML page that renders a 14-second animation (1280x720, 30fps, deterministic: expose window.renderAt(t) so every frame can be captured).

Scene: a floating voxel diorama island - Paris, 1888, the Eiffel Tower under construction. Everything is already moving on frame 1.

Island:
- floating chunk with layered rock underside, the Seine curving across the front with stone quays and an arched bridge
- Champ de Mars: dirt work yard, stacks of iron girders, wooden sheds, a steam engine with a smoking chimney
- around it: rows of Haussmann buildings with cream walls, grey zinc roofs, chimneys, tiny trees along the boulevards

The tower (the hero):
- dark reddish-brown wrought iron lattice made of thin voxel beams with real X-bracing, not solid blocks
- four curved legs and the first platform finished, the second level half built, the top section only a skeleton
- wooden scaffolding towers under the legs, small creeper cranes on the structure lifting girders on chains
- glowing orange rivet sparks where workers hammer, small puffs of steam

Life (hundreds of tiny voxel people, ~3 voxels tall):
- riveting teams on every level with hammers swinging, workers walking along beams
- girders hoisted by cranes swinging slowly into place
- horse-drawn carts carrying iron across the yard, crowds of onlookers in 1880s clothes with top hats and parasols
- a riverboat with a smoking funnel moving along the Seine, carriages crossing the bridge, pigeons circling the tower

Camera and look:
- wide view of the whole island, slow orbit (~25 degrees) with a gentle push-in toward the tower
- warm late-afternoon sun, soft shadows, ambient occlusion, light haze, rounded voxels, ACES tone mapping, soft bloom on sparks
```

</details>

Final benchmark: Opus 5.5 main model, Sonnet 5.5 worker, list-price cost. Run counts vary by mode; quality is blind-judged against `main`. Lower cost/time does not imply higher quality.

| Mode | Runs main / PR | Mean cost main → PR | Median cost main → PR | Mean time main → PR | Quality vs `main` (items / overall, of 32) |
|---|---:|---:|---:|---:|---|
| Normal | 4 / 4 | $7.71 → $3.73 (−52%) | $8.69 → $3.89 | 95 → 26 min | +1.42 / +0.72* |
| Auto | 4 / 4 | $5.72 → $4.41 (−23%) | $5.85 → $4.27 | 77 → 27 min | +1.42 / +0.72* |
| UltraSave | 3 / 4 | $7.16 → $2.37 (−67%) | $6.36 → $2.41 | 95 → 25 min | −0.92 / +0.67 |
| UltraFast | 3 / 6 | $14.65 → $2.14 (−85%) | $13.60 → $1.75 | 128 → 13 min | −0.83 / −0.67 |
| UltraCode | 3 / 4 | $14.39 → $7.83 (−46%) | $9.70 → $8.05 | 123 → 45 min | +3.50 / +1.08 |
| Mean over modes | | $9.93 → $4.10 (−59%) | | | +1.06 items (p = 0.055), +0.55 overall (p = 0.016), 40 pages |

*Normal and Auto quality pooled across 26 pages; same +1.42 / +0.72 result. Gains concentrate in Normal/Auto and UltraCode. UltraSave scored fewer items but better overall; UltraFast scored below `main` on both measures. Sample counts are small and single-page scores ranged 15–32, so mode-level quality differences are uncertain.

**Method.** For each page, `window.renderAt(t)` supplied frames at 0, 7 and 13.9 seconds. Blind judges, shown random IDs and `main` anchors, scored 16 prompt elements (0–2 each) plus an overall mark. A PR score is its mark minus the mean `main` mark in the same panel and mode; p-values use a one-sided permutation test against `main`. Costs use Anthropic list prices (Opus: $4/M input, $0.20/M cached, $5/M cache write, $20/M output; Sonnet: $2/M, $0.10/M, $2.50/M, $10/M); runs used a Claude subscription. Jev and d1.2 calls (up to $0.0085/run) are excluded. The measurements mix builds: Normal/Auto quality pools builds from cc2f032b onward, but costs reflect 1e10a3cb; UltraCode uses the reminder-hold build; UltraFast and UltraSave use 1e10a3cb. Each montage shows `main` on top and PR below, at 7 seconds; tiles identify run, blind-judge score and list-price cost.

![Normal final comparison montage](https://raw.githubusercontent.com/samuelfaj/distill/9fc868d9f8cd59a97884cb23f50df79c4be71977/paris-1888-rerun/1e10a3cb/normal.jpg)

![Auto final comparison montage](https://raw.githubusercontent.com/samuelfaj/distill/9fc868d9f8cd59a97884cb23f50df79c4be71977/paris-1888-rerun/1e10a3cb/auto.jpg)

![UltraSave final comparison montage](https://raw.githubusercontent.com/samuelfaj/distill/9fc868d9f8cd59a97884cb23f50df79c4be71977/paris-1888-rerun/1e10a3cb/ultrasave.jpg)

![UltraFast final comparison montage](https://raw.githubusercontent.com/samuelfaj/distill/9fc868d9f8cd59a97884cb23f50df79c4be71977/paris-1888-rerun/1e10a3cb/ultrafast.jpg)

![UltraCode final comparison montage](https://raw.githubusercontent.com/samuelfaj/distill/9fc868d9f8cd59a97884cb23f50df79c4be71977/paris-1888-rerun/1e10a3cb/ultracode.jpg)

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
- [Code Mode (experimental): the model scripts several tool calls in one step](crates/codegen/distill-pager/docs/user-guide/29-code-mode.md)
- [How Jev routes work](docs/jev-routing.md)
- [UltraCode activation, hierarchy limits and paired comparisons](docs/ultracode.md)
- [UltraFast execution mode and paired speed/quality evaluation](docs/ultrafast.md)
- [UltraSave execution mode and paired token/quality evaluation](docs/ultrasave.md)
- [Where the token savings come from](docs/token-saver.md)
