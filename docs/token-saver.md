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
| Native command filters | Passing test records, Cargo build progress and Git status boilerplate | Complete terminal results of 512 bytes or more; only when the stored, final replacement is smaller | Always |
| Read reuse | A second copy of bytes already in the conversation | 2000 bytes or more, and hash-identical to a payload already sent | On |
| Exact-output guard | Nothing. This step stops the rest | The call is line-addressed, or the text belongs to a skill | Always |
| Preclean | Terminal noise, and content the payload itself repeats | 2000 bytes or more, and not a document | On |
| Importance extraction | The unreadable middle of a long payload | 4000 bytes or more, and not a document | On |
| Utility selection | Selects source units worth keeping | Source-specific size thresholds; originals remain stored | On |

The deterministic steps save tokens. Utility selection and the routing levers save money
by using a cheaper model, which is a different thing, and the last section says
why the distinction matters.

## Native command filters

The harness includes local output filters inspired by [RTK](https://github.com/rtk-ai/rtk).
They run before Jev's optional reductions, including when Jev is disabled, without
rewriting the command, installing another executable or calling a model.

Supported direct invocations are Cargo `test/build/check/clippy`, Bun/npm/pnpm/yarn
`test` or `run test`, Jest/Vitest (also through `npx`), pytest (also through
`python -m pytest`), `go test`, and human-readable `git status`. Completed single
background terminal results use the same filters after their origin is checked
against the terminal backend. Mixed task results, running/interrupted commands,
truncated output, compound shell commands, structured documents and exact-output
reads stay on the existing path.

Only recognized routine lines are removed. Unknown lines, failure blocks, skip
counts and test summaries stay; command/status/exit-code metadata and the raw
tool output shown to clients are unchanged. The complete original model-visible
result is stored in the existing recovery store before replacement, and the
replacement names that file. Storage failure or no net reduction keeps the
original. These filters skip secret-bearing results.

Each accepted replacement logs its filter, original/final byte counts and
original/final token estimates under `native command output compressed`. These
measure the tool text reduced, not a percentage reduction in the provider bill.

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

## Utility selection

The utility model runs on any catalog transport, including a ChatGPT-subscription
model such as `chatgpt/gpt-6-luna` through Responses and OAuth via the sampler
stack. In `auto` effort mode it uses the lowest level in that model's menu. Up
to four calls run concurrently; localhost endpoints allow one. An
explicit `[jev.local] model` is the only choice. When it is unset, the utility
uses the shipped OpenRouter chain, then the `[models] session_summary` /
`prompt_suggestion` pin when that chain has no key. It never uses the worker, the session's own model, or the
caller's fallback model, and a sampler-backed lane gives up after 20 s (see
`docs/jev-routing.md`).

The harness labels source units `[U12]`; utility returns IDs or ranges, and the
harness copies those original units. It does not use generated replacement
prose. Except for `search_tool`, the harness always retains the first two and
last two units; terminal,
task, grep and subagent output also keep error, failure and summary lines, grep
listings their result headers, and web search its citation paragraphs. The
question names only what that source forces. It also carries the call (tool
name and bounded arguments, such as `target_file`, the grep pattern or the
`search_tool` query) and the last 300 bytes of the assistant text that made the
call, JSON-quoted as data; the session request follows as secondary context,
and the whole stays within the 2 KiB question bound. Input is split into chunks
of at most 24 KiB, with at most eight chunks. A replacement is accepted only
when it is under 70% of the original; a `search_tool` result only needs to drop
one whole tool and come out shorter. The original is always stored, and the footer
names its stored path. A source or question that `utility_secret_presence` flags never
goes to the utility and is not stored: it keeps today's bytes (`keep:secret`).
The screen checks only what is sent, so it does not check a subagent's resume
footer. It flags key prefixes at a word start, private-key and bearer headers,
secret-named keys with a literal value, and dense tokens that mix case and
digits. Paths, UUIDs, git SHAs and checksums do not trigger it. The memory
prepass applies the screen before it spends its chunk budget.
`ask_stored_output` applies the same screen and tells the model to read the
file directly.

The utility handles these sources:

- Terminal output and task output at 4,000 bytes or more. `head` and `tail`
  windows count; exact-output commands such as `sed`, `cat`, `jq` and `git show`
  stay untouched.
- Grep listings at 12,000 bytes or more. Their footer says `kept K of M match
  lines`.
- Web search, and web fetch of any content type, using the full source.
- MCP results at 4,000 bytes or more; file readers are excluded.
- `search_tool`, where selected tool schemas remain JSON.
- Whole-file `read_file` in read-only sessions at 16,000 bytes or more; line
  numbers remain intact.
- Memory capture: tool results of 4,000 bytes or more in the finished turn,
  at most 8 chunks per capture, skipped when the session model is the utility
  model. The extraction itself stays on the main model.

There is no Jev pre-approval and no main-model fallback. Jev post-review reads
the reconstructed text. It is skipped for `NONE` and over-size state. If Jev
returns no answer, the verified candidate remains. Only a `reject` at
confidence 0.70 or higher discards the candidate; `defer` or a less confident
`reject` keeps it.

The utility can also write display text: initial title, shell autocomplete,
prompt suggestion and recap. Each falls back to the old path.

Utility usage rows carry `reason`, `bytes_in` and `bytes_out`. The end-of-turn
report shows `Utility - Nx`.

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

Seven levers affect what reaches the model:

- `p1_tool_family` keeps rarely used tool families (image and video generation,
  scheduling, feedback, and MCP tools when `search_tool` can find them again) out
  of the tools array until a human request needs them. Tool schemas are resent
  every round, which makes them a recurring cost rather than a one-off. A family
  that joins never leaves: the tools array opens the cached prompt prefix, so a
  family that came and went would re-bill the whole conversation.
- `p6_skill_suggestion` has Jev rank the whole skill catalog against the request.
  The listing keeps full descriptors only for the skills that request needs,
  names every other skill next to a full-catalog index file, and is rebuilt only
  when the prefix is (the first prompt and each compaction). Each human turn also
  gets one `<skill_relevance>` line that names the skill to read, or says none
  applies, without touching earlier messages.
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

- `b2_local_model` prefers your configured local model for calls it can finish.
- `b2_micro_effort` picks the effort level per call, when you set `/effort auto`.

The worker model (`/worker-model`) is the other routing saving: the main model
delegates implementation to it, so the expensive model plans and reviews while
the cheaper one does the long edit-and-test work in its own context.

Routing changes which model answers the turn, not how many tokens the turn needs.
It lowers your bill. If you are trying to fit a context window, these levers do
not help you and the layers above do.

## The decision layer on top

Routing levers influence model and effort choices. Utility selection itself uses the bounded source-unit protocol above; it has no Jev pre-approval.

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

Utility outcomes are kept without that variable. Each session's `usage.json`
has `utilityOutcomes` per source kind (`shell`, `mcp`, `recap`, …): the final
decision per eligible result (`compress`, `not_shorter`,
`defer:required-dominates`, `keep:lane-unavailable`, …), requests refused before
dispatch (`request:defer:failure-bound`, …), chunks, and bytes in and out. Every
utility attempt row also carries `source_kind` and `final_decision`. Sizes and
labels only, never content.

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
- `secret_redacted_view`, `pii_presence` and `injection_presence` are safety
  transforms, not savings: they mask or flag a value by decision rather than by
  gain. (`secret_presence` is live: retention and D2 use it. The utility screen uses
  the narrower `utility_secret_presence`.)
- `store_stats`, `search_store`, `validate_json`, `estimate_tokens`,
  `repo_map_budget`, `write_ack`, `error_site_refs`, `test_baseline_diff`,
  `alias_identifiers`, `volatile_tokens` and `compact_span` are pure functions
  waiting for a caller.
- The task registry still contains other utility tasks, but tool-result compression now selects source units instead of mapping payloads through `task_for_payload`.

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
