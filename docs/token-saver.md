# Where the token savings come from

Distill sends less text to your model than a plain agent would. Most of that
happens in deterministic code, on a tool result, before it enters the
conversation. Some of it happens by handing a payload to the utility model
instead.

This page covers every mechanism that reduces what you pay for, in the order a
tool result meets them, with the guard that can stop each one. It also states
what is implemented but not wired, because a technique that never runs is worth
saying out loud.

## The order a tool result meets them

A tool result passes through these steps in this order, in
`crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs`,
which calls one pipeline function so that the tested code is the shipped code:

| Step | What it removes | Runs when | Default |
|---|---|---|---|
| Read reuse | A second copy of bytes already in the conversation | 2000 bytes or more, and hash-identical to a payload already sent | On |
| Exact-output guard | Nothing. This step stops the rest | The call is line-addressed, or the text belongs to a skill | Always |
| Preclean | Terminal noise, and content the payload itself repeats | 2000 bytes or more, and not a document | On |
| Importance extraction | The unreadable middle of a long payload | 4000 bytes or more, and not a document | On |
| Utility task | A payload a small model can digest cheaper than you reading it | 2000 bytes or more, a task registered for the shape, and the lane decision not against it | On |
| Utility compression | The same, for a payload too large to want whole | 24 KiB or more, with the lane decision not against it | On |

The first four save tokens. The utility task and the routing levers save money
by using a cheaper model, which is a different thing, and the last section says
why the distinction matters.

## Read reuse

Every round resends the conversation history, so a file included once is paid
for again on each later round. Sending the same bytes twice costs worse than
twice as much.

When a payload reaches 2000 bytes, Distill hashes it (FNV-1a, which is enough:
a collision only costs a re-read) and checks whether those exact bytes were
already sent in this conversation. If they were, the payload becomes a note
naming where the earlier copy came from and its hash. Only identical bytes
qualify. A file that changed is never reused, because the model would otherwise
be reasoning about a version it never saw.

## The guard that stops everything: exact output

Some payloads are line-addressed: the reader asked for the bytes and intends to
slice, count or match against them. Compressing one of those takes away exactly
what was asked for, so the pipeline leaves before any rewriting stage.

`crushers::is_exact_output` covers the catalogue's own `distill_exact_rg` rule:

- the dumper and matcher family: `rg`, `grep`, `egrep`, `fgrep`, `ag`, `ugrep`,
  `sed`, `awk`, `gawk`, `nawk`, `cut`, `tr`, `paste`, `cat`, `bat`, `head`,
  `tail`, `nl`, `tac`, `diff`, `cmp`, `od`, `hexdump`, `xxd`, `base64`, `jq`,
  `yq`, wherever the program sits in a pipeline;
- Git's own dumpers: `git grep`, `git show`, `git cat-file`, `git blame`;
- the exact-output tools: `grep`, `read_file`, `read`;
- text that belongs to a skill, matched by a `/skills/` path or a `SKILL.md`
  name, because skill bodies stay verbatim whichever tool read them.

A payload that hits this guard passes through byte for byte, and the decision
record says `keep` with the reason.

## Preclean

`crushers::preclean` is a pure function over text. It costs nothing to run, so it
runs before the levers that spend money and does not depend on what they decide.
Two rules hold for every transform in the family, in the module's own words:
nothing unique is invented, and anything removed is either recoverable from the
text itself as a pointer or a count, or reported so the caller can store the
original first.

Two fixed passes run on every payload:

- `strip_ansi` removes colour and cursor escapes. They inflate tokenization and
  carry no information a reader needs.
- `collapse_progress` keeps the last state of each progress bar. npm, pip and
  `docker pull` redraw one bar hundreds of times using carriage returns.

Then the payload's own shape picks a chain, through `classify_payload` and one
table:

| Class | The reader it gets |
|---|---|
| Build log | Repeat collapse: a repeated line becomes a pointer to the line that first carried it, and runs of blank lines go |
| Listing, command output | Repeat collapse, then `compact_padded_table`: alignment whitespace between columns becomes a single tab |
| Test report | `crush_test_output`: failing names, assertion text and the summary stay, the runner's banner lines go |
| Stack trace | `crush_stack`: the frames that say where it broke stay, register and memory dumps go |
| Lockfile | `crush_lockfile` reduces the resolution graph to top-level names and a count; if that would lose a version or a checksum, the chain falls through to the repeat collapse instead |
| Diff | `crush_diff` would keep headers and hunks; in practice a diff is a document, so the guard below stops it first |
| JSON, HTML, notebook | A class transform exists for each; each is a document, so the guard below stops them first |
| Prose | Nothing. There is no shape-based saving in prose |
| Unrecognised | The ANSI pass again, for noise every terminal payload can carry |

Each candidate in a chain is checked against the bytes it would replace before it
is accepted. That is why the lockfile row falls through rather than giving up:
an aggressive reduction that would drop something a reader may need loses to a
conservative one that keeps it.

Two more properties keep this honest. The classifier is deliberately blunt: a
wrong guess costs a missed reduction and nothing else, because every transform is
safe on any text. And a transform with no gain returns nothing, so the payload
keeps its exact bytes. `a_payload_without_redundancy_is_left_alone` fixes that in
a test: a listing where every line is unique passes through untouched, and a
caller can read "no result" as "today's bytes" without a second thought.

## The document guard

A document is the answer the reader asked for. Cutting a hole in one leaves
something that still looks complete, which is worse than leaving it whole, so the
rewriting stages stay out of it. `retention::looks_structured` decides, once, and
both the crushers and the importance stage read the same verdict.

A payload counts as a document when it parses as JSON, starts with an XML or HTML
declaration, contains a `diff --git` or `@@ ` line, or came from a document
producer: `cat`, `bat`, `jq`, `yq`, `diff`, `base64`, `openssl`, `xxd`,
`hexdump`, `pdfinfo`, and Git's `diff`, `show` and `cat-file`. The check looks
through a pipeline, so `find . | head` and `cmd && cat x` count.

## Importance extraction

Not everything can be shredded by shape. For a payload of 4000 bytes or more,
`reduce::extract_important` scores each line for whether a reader would act on
it: a failure, a `file:line` location, a number that changed. Progress noise
scores zero. The payload keeps its head, its tail and the lines that scored, and
the middle is elided.

This step is lossy, so it carries two rules:

- **The original is stored before the body is replaced.** `store_payload` writes
  the full output to a file under the harness home and returns its path. The
  elided text is sent with a line naming that file, so the model can read the raw
  bytes when it needs them. A store that refuses means the stage does not run: no
  byte is ever dropped without somewhere to get it back.
- **A reduction that would drop a literal is refused.** `preserves_literals`
  compares the result against the original. If a path, a `file:line`, a number or
  an error word would disappear, the reduction is thrown away, the original bytes
  are kept, and the refusal is recorded with the count of lost literals.

A live session shows both halves working. On `ls -lR crates/codegen`, the crusher
lane reported `18522 bytes -> 18279 bytes via log_crusher+padded_table_compact`,
and the importance lane answered with a refusal: `a reduction would have dropped
235 literal(s); keeping today's bytes`.

That refusal is why this layer is worth trusting. A summarising model can quietly
drop the one detail that mattered; a rule that keeps the bytes when a literal
would go cannot.

## The utility task

For a payload that a small model can read more cheaply than the session model
should, Distill hands the work over. `tasks::task_for_payload` picks the task
from the payload's command first and its shape second, because the same shape
means different work depending on what produced it:

- the command names the work: `git log` and `git status` go to a git theme
  summary, `kubectl` to a kubectl digest, `terraform plan` to a plan digest,
  `gh pr` to a review-thread digest, `lldb` to a debugger digest, `cargo test` to
  a test verdict, `cargo build` to a compiler-diagnostics list, `vite build` to a
  bundler digest, `docker build` to an error tail, and so on;
- with nothing named in the command, the shape decides: a test report to a test
  verdict, a stack trace to a likely-files map, a lockfile to a dependency graph,
  a listing to a tree digest, a diff to a patch explanation.

Every id the table can return is checked against the registry by a test, so the
table cannot drift away from the tasks that exist. The registry holds 93 tasks;
each one carries a guard, and the guard is the point: a small model's answer is
used only where it can be checked against the payload it was given.

- A compression must keep every literal and come out shorter.
- A digest must keep every literal and come out shorter.
- An extraction must quote spans that really appear in the payload.
- A classification must land on one of the closed labels.
- A pick must name ids that were offered.
- A schema fill must parse as a JSON object.

An answer that fails its guard is discarded and the bytes stay as they were. That
is why a listing often yields nothing: almost every token in one is a literal (a
path, a mode, a date, a size), so a digest that shortens it loses one, and the
guard refuses. A real turn shows the whole sequence: the lane mapped the listing
to `tree_listing_digest`, called it, and recorded `task tree_listing_digest
refused or failed; keeping today's bytes`.

The id that runs comes from the mapping, not from a constant, so a test report
and a crash report each get their own reader instead of both getting the same one
hard-coded in the caller.

A second utility lane handles the payloads too large to want whole:
`e_cheap_compress` sends anything from 24 KiB to the extractive compression task,
and the original is stored first, so the body that replaces it names the file it
came from.

## The reversible store

Everything above is lossy in what it shows and exact in what it keeps. The
original lives in a file, and two functions read from it without sending it to a
model:

- `retrieve_range` returns a line range of the original.
- `grep_handle` runs an exact match inside handles and returns verbatim lines
  with their line numbers.

So summarising is optional. A model that needs the raw bytes asks for a range; a
model that does not never pays for them.

## Context pruning

Six levers affect what reaches the model, five of them by default:

- `p1_tool_family` narrows the tool set announced for a turn to the families that
  turn needs. Tool schemas are resent every round, which makes them a recurring
  cost rather than a one-off.
- `p2_read_shortlist` picks line and segment windows instead of whole files.
- `p3_compaction_recorte` decides which segments the compactor must see.
- `d2_big_output_retention` drops a large tool result once it is no longer
  needed.
- `d3_post_compaction` re-injects only the chunks still relevant after a
  compaction.
- `e_retention` keeps only the payload chunks a task still needs, asking one
  question per chunk. It is off by default because its own cost is unmeasured.

Two commands line up with this. `/compact` reclaims window space on demand.
`/context` shows where the window is going, including what the tool definitions,
the skills listing and the MCP announcements cost in estimated tokens.

## Routing saves money, not tokens

The `b2_*` levers route a call to a weaker or local model:

- `b2_reasoning_model` lets the main model consult the reasoning model for a
  step it cannot do alone, so the step is right the first time.
- `b2_local_model` prefers your configured local model for calls it can finish.
- `b2_micro_effort` picks the effort level per call, when you set `/effort auto`.

Routing changes which model answers the turn, not how many tokens the turn needs.
It lowers your bill. If you are trying to fit a context window, these levers do
not help you and the layers above do.

## The decision layer on top

Jev owns the choice, and it can veto. A payload reaches a cheap lane only when
the lane decision favours it: `e_lane_choice` asks one question per eligible
micro-action, the answer can name the main model, and a subagent-shaped answer
defers because that lane is not wired. With the decision against it, the payload
stays with the session model.

Every step records what it did, so a session can be read afterwards:

```sh
GROK_LOG_JEV=1 distill
```

This writes `logs/jev.jsonl` inside the active profile, one entry per decision,
each with the lever, the verdict, the reason and, where there is one, a
confidence. The reduction steps use the labels `reuse`, `crush`, `extract` and
`keep`, the utility task uses `used` or `defer`, and the routing levers use
`local` or `cloud`. That is how you find out which layer is doing the work in a
real session.

## Turning levers off

`[jev] enabled = false` or `GROK_JEV=0` disables everything at once. Each lever
also has its own key under `[jev.ladder]`, and with a lever off the payloads pass
through as the bytes they were. The authoritative list of switches and their
defaults is `JevFlags::harness_default()` in
`crates/codegen/distill-workspace/src/jev/flags.rs`. Do not confuse it with
`JevFlags::default()`, an inert all-off value used by tests.

Five levers stay off by default, each for a stated reason:

| Lever | Why it is off |
|---|---|
| `e_retention` | One question per chunk; the cost of those calls is unmeasured |
| `e_cheap_agent` | The cheap-subagent lane is not wired, so a decision that asks for one defers instead of pretending |
| `e_prompt_blocks` | Cutting the standing prompt needs a whitelist of blocks that must always travel |
| `b2_model_tier` | A money lever waiting for its own cost gate |
| `c6_injection_screen` | Waiting for a measurement of what the screen itself costs |

## What is coded but not wired

Honesty about the rest. These transforms exist, have tests and a class each, and
nothing on the live path invokes them:

- `source_skeleton` (signatures without bodies), `crush_svg`,
  `generated_asset_notice` and `crush_embedded_blobs` are genuinely lossy: they
  need the original stored before they run, and the only stage that stores today
  is the importance pass. They belong to a future stage that stores first, then
  applies.
- `crush_json`, `crush_html` and `crush_notebook` are behind the document guard,
  because each of those payloads *is* the document.
- `crush_diff` is behind the same guard: a unified diff is a document.
- `secret_redacted_view`, `secret_presence`, `pii_presence` and
  `injection_presence` are safety transforms, not savings: they mask or flag a
  value by decision rather than by gain.
- `store_stats`, `search_store`, `validate_json`, `estimate_tokens`,
  `repo_map_budget`, `write_ack`, `error_site_refs`, `test_baseline_diff`,
  `alias_identifiers`, `volatile_tokens` and `compact_span` are pure functions
  waiting for a caller.
- Of the 93 registered utility tasks, the mapping reaches the families that
  describe a payload's shape. The draft families that produce work rather than
  digest it (commit messages, PR bodies, release notes, translations, issue
  triage) stay unwired, because nothing selects them from a tool result.

`TODO.md` at the repository root is the full inventory, with a status and a
reason per entry, cross-referenced to `list.md`.

## What never happens

These are rules, not features, and they come from the catalogue that defines this
work:

- A tool result on an exact-output call is never rewritten.
- A document is never rewritten.
- A user-authored message is never compressed or rewritten. The pipeline only
  ever receives a tool result.
- Skill bodies and permission text are never rewritten.
- A secret's value is never returned by a transform; a presence flag masks it.
- A payload is never changed without the literal gate agreeing, and a lossy stage
  never runs without the original stored.
- No savings number is reported, because nothing here measures one. A cap is not
  a saving, an estimate is not a measurement, and a skipped payload is not a win.

For the routing side, see [How Jev routes work](jev-routing.md). Provider setup
and local model configuration are in [Accounts and local models](local-models.md).
