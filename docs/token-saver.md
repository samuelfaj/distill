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
| Read reuse | A second copy of bytes already in the conversation | 2000 bytes or more, and hash-identical to a payload already sent, or a whole `read_file` identical to a copy still in history | On |
| Exact-output guard | Nothing. This step stops the rest | The call is line-addressed, or the text belongs to a skill | Always |
| Preclean | Terminal noise, and content the payload itself repeats | 2000 bytes or more, and not a document | On |
| Stored crushers | Stack register dumps, test runner banners, HTML chrome, embedded base64 and SVG data | 2000 bytes or more, not a document, and only with the original stored | On |
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
against the terminal backend. The usual wrapping counts as a direct invocation:
one leading `cd <dir> &&`, a trailing `2>&1` and one trailing `| tail -N` or
`| head -N` are set aside before matching. Mixed task results, running/interrupted
commands, truncated output, other compound shell commands (chains, real
pipelines, substitutions), structured documents and exact-output reads stay on
the existing path.

A rerun of the same Cargo test command folds the failures that did not change.
Each `---- name stdout ----` block is kept with a hash of its text, per session
and per command (directory plus invocation). On the next filtered run, a block
whose name and text match the previous run becomes one line, `[still failing
with the same output as the previous run of this command: <names>]`. A new
failure, a changed message, the `failures:` list and the summary stay verbatim,
and the whole run is stored as usual. A compaction or a history eviction forgets
the baseline, so the next run shows every failure again. Folds are counted in
usage.json under `test_rerun`.

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

A whole-file `read_file` (no offset or limit) is checked against the history
itself instead, before the exact-output guard: when the latest earlier whole
read of the same file is still in history byte for byte (not narrowed, evicted
or compacted), and no edit, write or shell call naming the file came after it,
the new result is one `[unchanged since call …]` line naming that call. An
offset/limit read repeats the lines. Instruction and skill files, and a read
carrying a reminder, stay whole. The note's target is pinned against history
eviction.

## The guard that stops everything: exact output

Some payloads are line-addressed: the reader asked for the bytes and intends to
slice, count or match against them. Compressing one of those takes away exactly
what was asked for, so the pipeline leaves before any rewriting stage.

`crushers::is_exact_output` covers the catalogue's own `distill_exact_rg` rule:

- the dumper and matcher family: `rg`, `grep`, `egrep`, `fgrep`, `ag`, `ugrep`,
  `sed`, `awk`, `gawk`, `nawk`, `cut`, `tr`, `paste`, `cat`, `bat`, `head`,
  `tail`, `nl`, `tac`, `diff`, `cmp`, `od`, `hexdump`, `xxd`, `base64`, `jq`,
  `yq`, in command position: the first word of a pipeline stage after env
  assignments and `sudo`/`time`/`xargs`/`parallel`/`bash -c` wrappers. A
  runner counts as what it runs: `ssh HOST …`, `watch …`, `docker|podman
  exec`, `kubectl exec … --`, `find -exec` and `fd -x`. Words in quotes,
  substitutions and heredoc bodies are not programs, a quoted `>` is not a
  redirection, and a stage whose stdout goes to a file does not count. A
  command that does not parse, or holds a `case`, falls back to matching every
  word;
- Git's own dumpers: `git grep`, `git show`, `git cat-file`, `git blame`
  (`git diff` is git, not `diff`: a document);
- the exact-output tools: `grep`, `read_file`, `read`;
- text that belongs to a skill, matched by a `/skills/` path or a `SKILL.md`
  name, because skill bodies stay verbatim whichever tool read them.

A payload that hits this guard passes through byte for byte, and the decision
record says `keep` with the reason. `exact_output_kind` grades it by the stage
that produced the bytes: a file dump (`cat`, `sed -n`, `git show`, a transform
run on a file) is `Exact`; a grep-like producer or a positive `| grep` filter
is `Matches`; `head`/`tail`, or any filter over another command's own output
(`| grep -v`, `| jq`, `| sed`), is `Window`. A compound command takes the most
exact of its parts, so `cd x && git diff | head -120` is a `Window` and
`cmd; cat f` stays `Exact`.

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
| JSON, notebook | A class transform exists for each; each is a document, so the guard below stops them first |
| HTML | `crush_html` keeps the text. Output that opens with a tag is a document, so only markup further in (after `curl -i` headers, say) reaches it |
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

A payload counts as a document when it parses as JSON, starts with `<?xml`,
`<!DOCTYPE` or a tag, contains a `diff --git` or `@@ ` line, or came from a document
producer: `cat`, `bat`, `jq`, `yq`, `diff`, `base64`, `openssl`, `xxd`,
`hexdump`, `pdfinfo`, and Git's `diff`, `show` and `cat-file`. The check looks
through a pipeline, so `find . | head` and `cmd && cat x` count.

## Stored crushers

Preclean only accepts a transform that keeps every literal. A second crusher
stage, in `jev_lanes::reduce_payload`, may lose lines, so it stores first, like
the importance pass below:

- a stack trace (`crush_stack`), a test report (`crush_test_output`) or HTML
  (`crush_html`) is cut when the cut keeps under 70% of the bytes;
- otherwise, base64 and data-URI islands (`crush_embedded_blobs`) and SVG path
  data (`crush_svg`) become typed placeholders when that saves at least 1 KiB,
  unless the payload looks secret-bearing.

The cut is accepted only after `store_payload` returns a path, and the footer
names it. A store that refuses keeps the bytes.

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
  bytes when it needs them; it names `ask_stored_output` only when the model has
  that tool. A store that refuses means the stage does not run: no
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
last two units; terminal
and task output also keep diagnostic lines (failure words such as
`error`/`failed`/`panicked`, `TypeError:`-style labels, pytest `E` lines, TAP
`not ok`, timeout, exit and not-found phrases, test summaries), grep listings
only their result headers, and web search its citation paragraphs. Common words
such as `not`, `run`, `out`, `expected` or `todo` are not markers. The
question names only what that source forces. It also carries the call (tool
name and bounded arguments, such as `target_file`, the grep pattern or the
`search_tool` query) and the last 300 bytes of the assistant text that made the
call, JSON-quoted as data; the session request follows as secondary context,
and the whole stays within the 2 KiB question bound. Input is split into chunks
of at most 24 KiB, with at most eight chunks; past eight, the head is selected
and the rest kept whole when that tail is under half the bytes
(`partial:verbatim-tail`). A replacement is accepted only when its kept body,
without the metadata block and the recovery pointer, is under 70% of the
original and the whole replacement is shorter; a `search_tool` result only needs
to drop one whole tool and come out shorter. A compressed shell result repeats
no command and only non-default metadata (a non-zero exit, a signal, a timeout,
truncation with the terminal log); a task result keeps its task id, command,
status, exit code and output file. A chunk answer is remembered per process by
endpoint, model, source kind and the hashes of the chunk and question, so the
same selection is paid for once (`memo`); a failed chunk is not remembered.
On the closed client with reasoning off, a unit-id answer is capped at 256
completion tokens, so an answer that turns into prose stops early. The original
is always stored, and the footer names its stored path; when the model has `ask_stored_output`, the footer names
that tool too, unless the original has a line over 4 KiB (a single-line JSON
original points at jq instead). A source or question that `utility_secret_presence` flags never
goes to the utility and is not stored: it keeps today's bytes (`keep:secret`).
The screen checks only what is sent, so it does not check a subagent's resume
footer. It flags key prefixes at a word start, private-key and bearer headers,
secret-named keys with a literal value, and dense tokens that mix case and
digits. Paths, UUIDs, git SHAs and checksums do not trigger it. The memory
prepass applies the screen before it spends its chunk budget.
`ask_stored_output` applies the same screen and tells the model to read the
file directly.

`ask_stored_output` takes a stored path (a store file or a session terminal log)
and a question. The utility picks `[U#]` line ids with `select_units` over at
most eight 24 KiB chunks, and the harness copies the picked lines verbatim with
their line numbers, so there is no quoting contract to fail. An answer stops
at 16 KiB of picked lines and says where the rest starts. `NONE` is reported
as "no line answers". A file too large for eight chunks is narrowed to the lines
holding the question's literal terms (quoted spans, paths, identifiers). With no
lane, a failed call or an unusable answer, the model gets the old "read the file
directly" message, plus the lines matching those terms (`grep_handle`, at most 40
lines). A read of a stored original by `read_file`, a shell command or a grep
path is never compressed again.

The utility handles these sources:

- Terminal output and task output at 4,000 bytes or more, documents (JSON, a
  diff) included. Windows and filters over a command's own output count at
  4,000 bytes, matches at 12,000, and file dumps such as `sed -n`, `cat` and
  `git show` at 8,000.
- Grep listings at 12,000 bytes or more. Their footer says `kept K of M match
  lines`.
- In a top-level session, which replays a result on far more later calls than
  a subagent, generic output (shell, MCP, web, task output, and windows over a
  command's own output) counts from 3,000 bytes and match listings (the grep
  tool, shell `rg`/`grep`) from 6,000; file dumps and reads keep their floors.
  A result only these floors admit is skipped before the call when its forced
  units and the footer reach half of it (`defer:small-cannot-pay`).
- `list_dir` listings at 6,000 bytes or more, as tree lines: every kept entry
  keeps its parent directory lines, and the footer says `kept K of M listing
  lines`.
- Web search, and web fetch of any content type, using the full source.
- MCP results at 4,000 bytes or more; file readers are excluded.
- `search_tool`, where selected tool schemas remain JSON.
- JSON results (MCP, or a non-exact shell or task body) with an array of 8 or
  more elements: each element of the largest array is a unit, the other fields
  and focused or input elements always stay, and the kept elements come back as
  valid JSON with a `kept K of N … omitted` footer. An MCP result cut at 20 KB
  is selected from the full output `mcp_truncate` saved for that call, and that
  full output is what gets stored. JSON that does not parse, has a number that
  would not survive re-serialising, or whose plan defers (an element over a
  chunk, forced bytes over 60%, or what every answer keeps already over 70% of
  the result as it stands) keeps line units, before anything is stored or sent.
- In non-exact line sources, a line over 1 KiB is cut into byte-exact pieces, so
  part of a minified line can be kept; the gap is marked `[… N bytes omitted …]`.
- `read_file` from line 1 (the whole file, or the first 1,000-line window of a
  longer one) at 16,000 bytes or more; line numbers remain intact, and a window
  says where the file continues. AGENTS.md, SKILL.md, CLAUDE.md and files in a
  `skills` directory are never narrowed. A selection that is `NONE` or under 10%
  of the bytes gains the file's outline (Markdown headings, or declaration lines
  by `is_signature_line`) verbatim with line numbers (`compress:outline`); a
  file with no outline keeps the original (`keep:thin-selection`).
- `read_file` offset/limit windows of 16,000 bytes or more with no limit or a
  limit over 300 lines (`read_range`): the window's first and last 20 lines and
  its outline are always kept, numbered from the window's start. Shorter or
  smaller windows, and tail reads by negative offset, stay whole.
- Subagent reports: a foreground result, a subagent item of a multi-wait,
  and a background completion in a wake digest or between-turn reminder, at
  4,000 bytes or more. A single finished subagent's `get_task_output` is where
  the completion notice's pointer leads, so it stays whole. The question quotes the spawn
  call's description and prompt (a background child's prompt is found by the
  spawn result that names it). The opening paragraph up to 1 KiB, status and
  verdict lines and the first and last two lines are kept; the prose gets no
  failure-marker lines. Worker evidence, the repository review, the meta line
  and the resume footer stay verbatim. A background report must end within
  the 16,000-byte inline cap (the last picks drop to fit), and the poll
  pointer stays. A worker report `cap_task_report` cut to 3 KB is selected
  from the full stored report within the bytes the cut took, and one cut to
  12 KB within 70% of them, the bar its head's own selection had to clear
  (the last picks drop to fit). When the full report is too big for the
  utility, the head is selected on its own; any other failure keeps the cut.
- Multi-wait envelopes whose items add up to 4,000 bytes: each finished bash
  or subagent item of 2,000 bytes or more is selected on its own.
- Polls of a still-running bash task (one task or a multi-wait item). First,
  without a model (`e_read_reuse`), the leading lines an earlier poll of the
  same task put into history become one `[… first N lines of this output
  already shown in call … …]` line in the new result only. Only the leading
  run in stream order counts, each named call must still hold its lines
  verbatim in history, and the last six lines always stay. What is left, at
  4,000 bytes or more, is selected with a progress question (`task_poll`),
  keeping error lines and the last six lines over a stored copy of the whole
  window. The poll that finds the task finished takes the terminal path.
- Truncated shell and task results whose session terminal log fits eight
  chunks: selected from the whole log instead of the head and tail window,
  within 70% of the bytes the window took (the bar the window's own selection
  must clear), with the footer pointing at the log (`full_log`). Any failure
  leaves the window to the usual selection.
- PostToolUse hook replacements of 4,000 bytes or more (`hook`), except a
  re-read of a stored original, a read `read_file` would keep whole, an
  edit's result and a subagent's result.
- Reminders the harness appends to a result (a finished background task, LSP
  diagnostics, skill discovery, a concatenated-call notice) are split off
  before the Jev pass and re-appended verbatim. A result whose notices cannot
  be told apart from its text keeps the old whole-result bypass; a reminder tag
  inside the tool's own text (a read of a file that quotes one) does not.
- Idle workflow-completion reminders over 8 KB: each run result the reminder
  would cut at 4 KiB is selected from the whole result within 4 KiB and the
  original stored; otherwise the cut stays. Background completions reported
  inside a tool result mid-turn are not covered yet.
- Memory capture: tool results, assistant text and string tool-call
  arguments of 4,000 bytes or more in the finished turn (the user's words never;
  arguments stay valid JSON), at most 8 chunks per capture, skipped when the
  session model is the utility model. The extraction itself stays on the main
  model. The legacy memory flush pre-digests its window the same way.

Selection has no Jev pre-approval and never falls back to the main model: its
fallback is today's bytes, as the deterministic stages left them. A tool-result or
memory-capture selection keeps verbatim units over a stored original, so it is
used without a Jev review (`review:skip-verbatim`) unless it keeps under 10% of
the chunk; then Jev post-review reads the reconstructed text. Other
`select_units` answers (`ask_stored_output`, display picks) keep the review. The review is skipped for `NONE` and over-size
state, and runs after the lane permit is released. If Jev returns no answer,
the verified candidate remains. Only a `reject` at
confidence 0.70 or higher discards the candidate; `defer` or a less confident
`reject` keeps it.

The utility can also write display text: initial title, shell autocomplete,
prompt suggestion and recap. Each falls back to the old path when the utility
fails. The recap reads the newest 20 KB of the transcript, joins a multi-line
answer into one paragraph and cuts it at a whole sentence, and retries an
unusable answer once with a stricter instruction before the main-model recap;
a lane that gave no answer is not retried. A utility `NONE` for a
prompt suggestion is final: nothing is shown and no paid model is asked. The
paid suggestion model keeps its own provider. When the utility title fails, the
request's first words are the title, and the title model (at its lowest effort)
runs only when those make no title. A subagent's title is its spawn
description, with no model call.

When enabled, the laziness classifier asks the utility first with its own
prompt; an answer that is not a valid verdict, a secret in the transcript, a
failure or 45 s without an answer classify on the session model as before,
inside the same 120 s budget and abort on user input or a model switch. After
two utility failures in a row the session stops asking the utility. Its events
name the model that gave the verdict. The
permission auto-classifier and the memory dream stay on their models.

Goal roles stay on the main model (round-end evaluation, planner, verifiers,
strategist), with three exceptions that fall back to the old path:

- The mid-turn progress checkpoint (every 24 tool rounds) makes no model call
  when the harness sees new work (a new worktree state or check outcome) that
  no evaluation counted and no blocker is pending; that work counts as progress
  once and still reaches the next main evaluation (the goal's
  `evaluations.jsonl` logs it with `"evaluator": "skipped"`; usage.json does
  not count it). Otherwise the checkpoint runs on main: a pending blocker is
  main's to confirm, and without new harness work only main can tell progress
  from a stall (a utility observation would reset the stall count unchecked).
- The closing summary is one utility `display_text` call over the objective,
  the verified criteria, the plan and the verifier notes; the read-only
  subagent stays the fallback.
- Each skill the objective pins is cut once per goal by `select_units` to its
  gates, outputs, steps and prohibitions (headings and lines saying must,
  never, always, required or before kept) and stored in goal state; the
  evaluator gets that excerpt, marked as unchecked where lines were omitted,
  while the file is unchanged, and the full body otherwise or when the utility
  picked nothing beyond those always-kept lines.

On main, the evaluator's effort is capped at medium (low when the model offers
no medium; a model offering neither keeps the session's), its output at 16K
tokens when that cap applied (a retained high or max effort keeps the model's
limit), and the stable goal message carries its own cache breakpoint.

One-shot side calls (memory capture, the title model, compaction pass 2, goal
evaluation, the laziness classifier, the dream and flush calls, utility tasks)
mark no conversation breakpoint on the Messages API, so they no longer pay the
cache-write premium on a prompt nobody resends. Their system prompt, and the
goal message, keep theirs.

Utility usage rows carry `reason`, `bytes_in` and `bytes_out`. The end-of-turn
report shows `Utility - Nx`.

### Every utility use at a glance

Each use needs a resolved utility model and `e_cheap_compress` (on by default;
the `explore` children need `e_cheap_agent` instead), plus the lever of its own
feature where it has one. Without them, and on any
failure, timeout, secret or unusable answer, it takes its fallback, which is
the behavior from before the utility was added. A valid `NONE` is an answer,
not a failure.

| Use | Runs when | Fallback |
|---|---|---|
| Tool-result selection (terminal, task, checks, MCP, web, grep, `list_dir`, `read_file`, `read_range`, `search_tool`, JSON arrays, subagent reports, multi-wait items, running-task polls, full logs, hook replacements, workflow results) | At the per-source floors above, 3,000 to 16,000 bytes | Today's bytes |
| `ask_stored_output` | The model calls it | "Read the file directly", plus up to 40 lines matching the question's terms |
| Memory capture pre-digest, and the legacy flush | Items of 4,000 bytes or more, 8 chunks per capture | The item as it was; extraction stays on main |
| Skills listing (`p6_skill_suggestion`) | Jev gives no answer at a prefix build or a discovery of more than eight skills | The lexical selection |
| Tool families (`p1_tool_family`) | Jev gives no answer | Every pending family joins |
| Compaction input | Only a cold input that must be fitted; results of 4 KiB or more, 4 chunks, 30 s | First and last lines |
| Working-set excerpts | After a compaction, up to three edited files, 30 s | The reminder without them |
| Initial title | First request | The request's first words, then the title model |
| Shell autocomplete | No model pinned | The previous completion call |
| Prompt suggestion | After a turn | The paid suggestion model |
| Recap | Auto or manual recap | One stricter retry, then the main-model recap |
| Laziness classifier | The classifier is enabled, until two utility failures in a row | The session model, after 45 s at most |
| Goal closing summary | The goal completes | The read-only summarizer subagent |
| Goal skill excerpts | Once per pinned skill per goal | The full skill body |
| `explore` children (`e_cheap_agent`, opt-in) | A fresh `explore` child with no model of its own | The child's own model |

Round-end goal evaluation, the planner, verifiers and skeptics, the strategist,
memory extraction and the compaction summary stay on main; the utility only
pre-digests some of their inputs.

## The reversible store

Everything above is lossy in what it shows and exact in what it keeps. The
original lives in a file, and the model reads it back with its ordinary
`read_file` (an offset and limit for a range) or a shell command, without a
model in between. `ask_stored_output` asks the utility which lines answer a
question; when it cannot, `grep_handle` returns the lines matching the
question's terms, verbatim with their line numbers. (`retrieve_range`, the same
slice as a pure function, has no caller.)

So summarising is optional. A model that needs the raw bytes asks for a range; a
model that does not never pays for them.

## Context pruning

Eight levers affect what reaches the model:

- `p1_tool_family` keeps rarely used tool families (image and video generation,
  scheduling, feedback, and MCP tools when `search_tool` can find them again) out
  of the tools array until a human request needs them. Tool schemas are resent
  every round, which makes them a recurring cost rather than a one-off. A family
  that joins never leaves: the tools array opens the cached prompt prefix, so a
  family that came and went would re-bill the whole conversation. A subagent
  starts from the families its parent offers and never adds one the parent left
  out. When Jev gives no answer the utility model answers the same questions for
  media, scheduling and feedback (verified `select_units`, NONE is an answer);
  without either answer every pending family joins, as before. When its parent
  offers no MCP family (so no MCP tool sits in its own tools array), a
  subagent's MCP announcement names servers and tool counts only; each server's
  instructions come with its `search_tool` results instead. A child that may
  call an MCP tool directly keeps the instructions. With P1 on, the spawn
  roster of a child type leaves out media, scheduling and feedback, which a
  child gets only when its parent's request needs them.
- `p6_skill_suggestion` has Jev rank the whole skill catalog against the request.
  The listing keeps full descriptors only for the skills that request needs,
  names every other skill next to a full-catalog index file, and is rebuilt only
  when the prefix is (the first prompt and each compaction). When Jev gives no
  answer the utility model picks instead (verified `select_units`, one line per
  skill); without either answer the lexical selection stays. A mid-session
  discovery of more than eight skills is narrowed the same way, as its own new
  item. Each human turn also gets one `<skill_relevance>` line that names the
  skill to read, or says none applies, without touching earlier messages. Only
  `/name`, `$name` or a backticked name pins a skill (a manual-only skill only
  as `/name`); a bare word like "review" and a subagent's assignment go to the
  ranking.
- `p2_read_shortlist` picks line and segment windows instead of whole files.
- `p3_compaction_recorte` decides which segments the compactor must see. It
  runs only on a compaction input that is cold anyway (fitted or lossy) and
  not when a two-pass summary is used: dropping a middle segment of the warm,
  cache-aligned input re-bills every cached token after it.
- `d2_big_output_retention` asks whether a large tool result is inert enough
  to drop. It is off by default: it asked at ingest, judging a 4 KB output by
  its first 1,200 characters before the model had read it, and dropped none.
- `d6_history_eviction` evicts old large tool output from the history, below.
- `d3_post_compaction` re-injects only the chunks still relevant after a
  compaction.
- `d4_compaction_timing` asks, between 50% and the threshold, whether to
  compact early. It reads the request, the fill level, the end of the last
  assistant message (left out when it looks secret-bearing) and the todo
  statuses; `usage.json` counts each answer under `compaction_timing`.
- `e_retention` keeps only the payload chunks a task still needs, asking one
  question per chunk. It is off by default, and when on runs only on an output
  utility selection left as it was (no lane, a failed or unpaying selection): its
  questions never carried the chunk text (60 logged runs, no trim), and
  utility selection already keeps a result's edges and failure lines.

### History eviction

Every main call resends the history, so an output read 40 rounds ago is paid
for on each of those calls. `d6_history_eviction` rewrites old output in the
retained history (and `chat_history.jsonl`; `updates.jsonl` keeps the
original), deterministically, with no model call:

- A tool result of 4 KiB or more, 20 or more tool rounds old, keeps its first
  and last 8 lines (at most 300 bytes each end, lines cut at 160 bytes) under a
  line naming the stored original.
- A result a later identical call superseded (the same path, offset and limit,
  or the same shell command in the same directory) becomes that one line after
  10 rounds, but only when the later result holds the output itself: not a
  reuse note, a digest or a utility selection, and at least half the size.
- In call arguments 20 or more rounds old, a `write` body of 4 KiB or more
  becomes a stub naming the file and the stored body, and a shell command of 4
  KiB or more keeps its first 600 bytes. The arguments stay valid JSON.

The original is stored first and the line names it (and `ask_stored_output`
when the model has it). A store that refuses keeps the bytes: a user's answer,
the plan, the goal, the todo list, a skill or instruction file read by any
tool, and anything `utility_secret_presence` flags. A digest is never
rewritten, so each item breaks the prompt cache once.

Rewriting an old item breaks the cache from that item on, so the pass runs in
batches, never per request, and by default only when the next call is cold
anyway: a model switch, a compaction, or no model output for an hour. A
compaction that kept a cached prefix with tool rounds in it (a fork re-pins
its parent's) is not cold. Warm batches wait for `d6_warm_batches` (off): at
most once per 25 tool rounds, and only when the bytes removed, replayed for
25 rounds, are at least ten times the suffix re-billed; those thresholds are
estimates until `history_batch` and `history_reread` show they pay. Inherited
history (a fork, a resume) counts as already batched. A resumed session gets
no idle check, and there is no observed-hit-rate trigger: on a provider whose
hits alternate, a rewrite would destroy the hits that remain. A history where
a tool call id repeats is left alone. After a batch the read-reuse index of
that session is cleared. The old user-turn hard clear
(`[Tool result omitted — too old]`) leaves eviction digests, and the copy a
whole-read reuse note names, alone. A cached two-pass pass 1 survives a batch:
its fingerprint reads a tool result by its call id, since a result is only
ever shortened in place.

`usage.json` counts it under `utilityOutcomes`: `history_batch` per batch
(`batch:warm`, `batch:cold-model-switch`, …; `bytes_in` is the suffix
re-billed), `history_evict` per item (`evict:head-tail`, `evict:superseded`,
`evict:write-content`, `evict:command`, `keep:unstored`; original and new
bytes), and `history_reread` for a later call that reads an evicted path or
runs an evicted command again (`reread:stored`, `reread:source`). Whether a
utility `select_units` digest beats head and tail here is not measured, so
there is none.

### Compaction input

Compaction still writes its summary on the session model; the utility never
writes summary text. When the input has to be fitted (the verbatim estimate
leaves no room for the tools and the summary, the provider rejected it as too
large, a two-pass pass 1 would not fit, or a fallback model has a smaller
window), it is cold anyway, so before the oldest turns are dropped each tool
result of 4 KiB or more before the newest human turn is stored and replaced:
by a verbatim utility selection (`select_units`, at most four chunks per
compaction, run at once under one 30 s deadline, used when it keeps under
70%), or by its first and last lines. A cached two-pass summary skips this:
it never reads the input. Pass 1's own fit keeps its trailing compaction
prompt out of the digest, so the turn in progress stays verbatim. Both name the stored copy. A store that refuses keeps the bytes, and a
missing, failed or slow selection keeps head and tail. The warm verbatim input
and the lossy stage are untouched. `usage.json` counts it under
`compaction_input` (`digest:selected`, `digest:head-tail`, `keep:unstored`).
Whether the selection reduces rejected or failed compactions is not measured.

A compaction request reserves at most the output the window leaves after its
input (and a margin of a tenth of the input): a 943K catalogue ceiling beside
a 600K summary input overflowed muse-spark's 1M window, so every two-pass pass
1 failed with a tokenless 400. A pass 1 that fails twice in a row is not
resent each round (one failure, a 429 or a timeout, is retried); the next
compaction or a model switch lets it try again. A summary the session
model rejects (degenerate, empty or failing after its retries) gets one try on
the policy's compaction model or the worker before the failure stands.

After a compaction the reminder can carry `## Working Set Excerpts`: for up to
three files this session edited, the lines of their latest read in the current
turn that the utility says the next edit needs, verbatim, naming the stored
read, at most 4 KiB per file and 8 KiB in all. A read with a later call naming
the file is stale and skipped; the selections run at once under one 30 s
deadline, and no lane, NONE or a failure leaves the reminder as it was. `usage.json` counts it under `post_compaction_excerpt`. Whether it
cuts re-reads is not measured: an edit still needs a prior read by default.

Two commands line up with this. `/compact` reclaims window space on demand.
`/context` shows where the window is going, including what the tool definitions,
the skills listing and the MCP announcements cost in estimated tokens.

## Routing saves money, not tokens

Two levers change which model or effort answers a call:

- `b2_micro_effort` picks the effort level per call, when you set `/effort auto`.
- `e_cheap_agent` (opt-in) runs a fresh `explore` child on the utility model,
  with the child's usual model as its fallback (see below).

`b2_local_model` no longer routes anything: a whole main round on another model
replays the history uncached, so that route was removed and the lever is off.

The worker model (`/worker-model`) is the other routing saving: the main model
delegates implementation to it, so the expensive model plans and reviews while
the cheaper one does the long edit-and-test work in its own context.

Routing changes which model answers the turn, not how many tokens the turn needs.
It lowers your bill. If you are trying to fit a context window, these levers do
not help you and the layers above do.

### Utility explore children (`e_cheap_agent`, opt-in)

```toml
[jev.ladder]
e_cheap_agent = true

[jev.local]
model = "openrouter-qwen37"   # a [model.<id>] catalog entry with tool calling
```

A fresh `explore` child the main model delegates, with no model chosen by the
caller, a role, `[subagents.models]` or its definition, runs each round on the
utility model. Its own model (the worker, or main without one) is resolved as
before and stays its fallback. These keep that model instead:

- the utility is a raw OpenRouter chain, missing, the child's own model, or
  listed by OpenRouter without tool calling;
- the conversation plus the reserve no longer fits the utility window (capped by
  `[jev.local] max_context_tokens`);
- the conversation carries something `utility_secret_presence` flags;
- a utility request fails for any reason; that request is resent on the child's
  model at once and the child stays there (a round the user cancelled or
  rewound is not resent);
- the child belongs to a workflow or has an output budget: its failed request
  must fail closed, not be resent.

After a utility child fails, its parent's later children skip the utility. A
fresh child shares no prefix with main, so no main cache is lost; the saving is
the price gap. Not measured yet: report quality against the worker's, and how
often a utility child answers that it cannot do the task (that answer is not
retried; it reaches the main model as it is). Watch `e_cheap_agent` rows in
`jev.jsonl` (`utility`, `own-model`, `fallback`).

A config-only way to try the same thing, with no fallback at all, is a pin:

```toml
[subagents.models]
explore = "openrouter-qwen37"
```

The pin also covers explore children that skills or goal roles spawn (unless a
role names its own model) and every resume, and the lever leaves a pinned type
alone. Remove the line to revert.

## The decision layer on top

Routing levers influence model and effort choices. Utility selection itself uses the bounded source-unit protocol above; it has no Jev pre-approval.

Every step records what it did, so a session can be read afterwards:

```sh
GROK_LOG_JEV=1 distill
```

This writes `logs/jev.jsonl` inside the active profile, one entry per decision,
each with the lever, the verdict, the reason and, where there is one, a
confidence. The reduction steps use the labels `reuse`, `crush`,
`crush_stored`, `extract` and `keep`, the utility task uses `used` or `defer`,
and `e_cheap_agent` uses `utility`, `own-model` or `fallback`. That is how you
find out which layer is doing the work in a real session.

Utility outcomes are kept without that variable. Each session's `usage.json`
has `utilityOutcomes`, one row per source kind. A row holds `decisions` (a
count per label) and, summed over every decision, `chunks` (utility requests
planned), `bytes_in` and `bytes_out`. Sizes and labels only, never content.
Every utility attempt row also carries `source_kind` and `final_decision`.

The source kinds:

- tool results: `shell`, `checks`, `task_output`, `mcp`, `grep`, `list_dir`,
  `read_file`, `read_range`, `web_search`, `web_fetch`, `search_tool`,
  `subagent`, `task_poll`, `full_log`, `hook`, `workflow_result`;
- side calls: `stored_output`, `memory_capture`, `skill_listing`,
  `tool_families`, `compaction_input`, `post_compaction_excerpt`,
  `initial_title`, `ai_suggest`, `prompt_suggest`, `recap`, `goal_summary`,
  `goal_skill`, `laziness_classifier`;
- no utility call, counted in the same table: `history_batch`,
  `history_evict`, `history_reread` (above), `test_rerun`
  (`collapse:unchanged`; `chunks` is the failures folded), `error_site`
  (`cited`, `read-after-cite`) and `compaction_timing` (`compact:jev_early`,
  `defer`).

The labels:

- `compress` (and `compress:outline`, `partial:verbatim-tail`): the selection
  was used;
- `not_shorter`, `defer:…` (`defer:required-dominates`,
  `defer:small-cannot-pay`, …): the utility was asked, or the plan showed it
  could not pay, and today's bytes stayed;
- `keep:…`: today's bytes stayed for the named reason, mostly a guard before
  any call (`keep:lane-unavailable` when no utility resolved, `keep:secret`,
  `keep:store-unavailable`, `keep:exact-floor` under an exact-output floor,
  `keep:read-window` for an offset/limit read that is not a large window,
  `keep:read-floor` for a whole read under 16,000 bytes), sometimes after one
  (`keep:thin-selection`, `keep:rebuild-failed`);
- `request:defer:…`: one utility request refused before dispatch
  (`request:defer:failure-bound` after repeated failures in the turn,
  `request:defer:input-bound`, `request:defer:task-bound`);
- for the classifier: `used`, `fallback:main` (an answer the
  harness would not apply, so main ran too) and `fallback:transport` (no answer
  in time).

For a tool-result kind, `bytes_in` minus `bytes_out` is what that source kept
out of the context on first entry; each byte saved there is saved again on
every later call that replays the history. For `laziness_classifier` the two
numbers are the request and the answer, not a
saving: the saving is the main call that `used` replaced. A rising
`fallback:main` share means those calls are paying twice. A source whose
`decisions` are mostly `not_shorter` or `defer:…` spends utility calls for
nothing and is a candidate for a higher floor.

## Turning levers off

`[jev] enabled = false` or `GROK_JEV=0` disables everything at once. Each lever
also has its own key under `[jev.ladder]`, and with a lever off the payloads pass
through as the bytes they were. The authoritative list of switches and their
defaults is `JevFlags::harness_default()` in
`crates/codegen/distill-workspace/src/jev/flags.rs`. Do not confuse it with
`JevFlags::default()`, an inert all-off value used by tests.
`e_cheap_compress = false` turns off every utility use in the table above
except the `explore` children; each takes its fallback.

These levers stay off by default, each for a stated reason:

| Lever | Why it is off |
|---|---|
| `e_retention` | One question per chunk, and the questions never carried the chunk text; it runs only on a result utility selection left as it was |
| `d6_warm_batches` | Rewriting sent history on a warm cache waits until `history_batch` and `history_reread` show it pays |
| `d2_big_output_retention` | Asked at ingest from a 1,200-character head, before the model read the output; it kept every output it judged |
| `e_cheap_agent` | Utility `explore` reports are not yet measured against the worker's; a missed file costs a main follow-up read |
| `b2_local_model` | Retired: nothing routes a main round to the utility any more, so it switches nothing |
| `e_prompt_blocks` | The skills part is already cut per request by `p6_skill_suggestion`; cutting AGENTS.md and user rules needs a whitelist of lines that must always travel, and no safe one exists yet, so it would need opt-in and a rule-adherence measurement first |
| `b2_model_tier` | A money lever waiting for its own cost gate |
| `c6_injection_screen` | Waiting for a measurement of what the screen itself costs |

`b6_delegation_hint`, `c1_premature_stop` and `c3_completion_check` are off as
well; they are quality checks, not savings (B6 is still asked and logged).
Retired lever keys (`e_cheap_task`, `e_lane_choice`, `e_breaker`, …) are
ignored with a warning.

## What is coded but not wired

Honesty about the rest. These transforms exist, have tests and a class each, and
nothing on the live path invokes them:

- `source_skeleton` (signatures without bodies) and `generated_asset_notice`
  are genuinely lossy. (Only `source_skeleton`'s line test, `is_signature_line`,
  is live: it builds the outline of a thin `read_file` selection.) `generated_asset_notice` replaces the whole output, its
  `minified` test matches every one-line JSON or HTML result, and its `binary`
  test counts any non-ASCII character, so it stays unwired. (`crush_embedded_blobs`
  and `crush_svg` run in the crusher stage on non-exact, non-document output at
  2,000 bytes or more, with the original stored first, when they save at least
  1 KiB; a secret-looking payload keeps its bytes.)
- `crush_json` and `crush_notebook` are behind the document guard, because each
  of those payloads *is* the document. (`crush_html` runs only on markup that
  does not open the output; see stored crushers.)
- `crush_diff` is behind the same guard: a unified diff is a document.
  Large documents still reach the utility's line selection where that lane
  admits them, with the original stored; what is missing there is unit shape
  (hunks, JSON elements), not a crusher pre-pass.
- `secret_redacted_view`, `pii_presence` and `injection_presence` are safety
  transforms, not savings: they mask or flag a value by decision rather than by
  gain. (`secret_presence` is live: retention and D2 use it. The utility screen uses
  the narrower `utility_secret_presence`.)
- `store_stats`, `search_store`, `validate_json`, `estimate_tokens`,
  `repo_map_budget`, `write_ack`, `test_baseline_diff`, `alias_identifiers`,
  `volatile_tokens` and `compact_span` are pure functions waiting for a caller.
  None has a measured saving for the main model: `write_ack` targets results
  that are already small, `repo_map_budget` and `alias_identifiers` would change
  what the model sees in the prefix or history, `volatile_tokens` is a cache
  diagnostic, and `test_baseline_diff` compares names only (the native Cargo
  rerun fold compares each failure's text instead).
- `error_site_refs` runs only as a measurement. For a failing terminal result it
  notes the first three cited `file:line` sites, and usage.json counts
  `error_site` `cited` and `read-after-cite` (a later `read_file` covering one of
  them). Quoting the lines up front would add bytes to every failing run, so the
  autoquote waits for that rate.
- Apart from `select_units` and `display_text`, the task registry
  (`jev/tasks.rs`: 93 catalogue tasks such as `test_verdict`, `cite_spans` and
  `commit_message_draft`) has no live caller: tool-result
  compression selects source units instead of mapping payloads through
  `task_for_payload`, and `run_best_of` runs only in tests. The utility
  allowlist admits only `select_units` and `display_text`; a registry row with
  the `Literals` guard is converted to a pick over source units or a closed label
  set before it is wired, because where the literals are the content no real
  reduction passes that guard.
- The image-describe pipeline (`session/image_describe.rs`, `transcribe_user_images`)
  runs only under `is_cursor_harness()`, which is always false in this build. User
  images and tool screenshots reach the main model as images. They are not
  captioned by the utility model, even though the shipped utility has vision: a
  caption at entry loses the pixels UI work needs. Swapping an image already in
  history would bust the cache, and compaction already removes every image.

`list.md` at the repository root is the full inventory, with a status and a
reason per entry; `todo.md` is the cost plan it serves.

## What never happens

These are rules, not features, and they come from the catalogue that defines this
work:

- A tool result on an exact-output call is never rewritten when it enters the
  conversation. Twenty rounds later, history eviction may replace it with its
  edges and a pointer to the stored original.
- A document is never rewritten by a deterministic stage. A large one may reach
  utility selection, which keeps verbatim units over a stored original.
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
