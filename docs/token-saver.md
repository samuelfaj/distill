# Where the token savings come from

Distill sends less text to your model than a plain agent would. Most of that
happens in deterministic code, on tool results, before they enter the
conversation. This page describes what runs, in what order, and which parts are
on by default.

There is no single switch for this. It is a stack of layers, each removing a
different kind of waste.

## The order things run in

A tool result passes through these steps in this order, in
`crates/codegen/distill-shell/src/session/acp_session_impl/jev_tool_result.rs`:

| Step | What it removes | Runs when | Default |
|---|---|---|---|
| Read reuse | A second copy of bytes already in the conversation | 2000 bytes or more, and hash-identical to a payload already sent | On |
| Preclean | Terminal noise, and content the payload itself repeats | 2000 bytes or more, and not a document | On |
| Importance extraction | The unreadable middle of a long payload | 4000 bytes or more | On |
| Cheap lanes | Model calls a weaker or local model can finish | Depends on the lane | Partly |

The first three save tokens. The last one saves money, which is a different
thing, and the sections below say why.

## Read reuse

Every round resends the conversation history, so a file included once is paid
for again on each later round. Sending the same bytes twice costs worse than
twice as much.

When a payload reaches 2000 bytes, Distill hashes it and checks whether those
exact bytes were already sent in this conversation. If they were, the payload is
replaced by a note naming where the earlier copy came from and its hash. Only
identical bytes qualify. A file that changed is never reused, because the model
would otherwise be reasoning about a version it never saw.

## Preclean

`crushers::preclean` is a pure function over text. It costs nothing to run, so it
runs before the levers that spend money and does not depend on what they decide.
Two rules hold for every transform in the family, in the module's own words:
nothing unique is invented, and anything removed is either recoverable from the
text itself as a pointer or a count, or reported so the caller can store the
original first.

It applies two steps:

1. A fixed pair first, on every payload: `strip_ansi`, then `collapse_progress`.
   Colour and cursor escapes inflate tokenization and carry no information. npm,
   pip and `docker pull` redraw one progress bar hundreds of times using
   carriage returns.
2. One transform chosen from the payload's own shape, by `classify_payload` and
   `crusher_for_class`:
   - Build logs, listings and command output go through `reduce_redundancy`,
     which replaces a repeated line with a pointer to the line that first carried
     it and collapses runs of blank lines.
   - Diffs go through `crush_diff`, which keeps file headers, hunks and changed
     lines and drops long runs of unchanged context.
   - Prose gets nothing. There is no shape-based saving in it.
   - An unclassified payload is sent through the ANSI transform again.

Two guards keep this honest. The classifier is deliberately blunt: a wrong guess
costs a missed reduction and nothing else, because every transform is safe on
any text. And a payload that looks like a document is never crushed at all,
because there the command's output *is* the answer, and cutting a hole in it
leaves something that still looks complete. Distill reuses the same
`retention::looks_structured` check the retention lane uses, so the two cannot
drift apart.

When a transform has no gain, it returns nothing and the payload keeps its exact
bytes. `a_payload_without_redundancy_is_left_alone` fixes that rule in a test: a
listing where every line is unique passes through untouched, and the caller can
read "no result" as "today's bytes" without a second thought.

The module also implements transforms that no product path calls today:
`crush_stack`, `crush_test_output`, `crush_html`, `crush_json`, `crush_notebook`,
`crush_lockfile`, `crush_svg`, `compact_padded_table` and `source_skeleton`. They
have tests and a classifier class each, but nothing outside the module invokes
them, so a stack trace or a JSON blob shrinks today only through the two steps
above.

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
  bytes when it needs them.
- **A reduction that would drop a literal is refused.** `preserves_literals`
  compares the result against the original. If a path, a `file:line`, a number or
  an error word would disappear, the reduction is thrown away and the original
  bytes are kept. The refusal is recorded with the count of lost literals, so you
  can see when it happened.

That refusal is why this layer is worth trusting. A summarising model can quietly
drop the one detail that mattered; a rule that keeps the bytes when a literal
would go cannot.

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

Five levers decide what reaches the model at all:

- `p1_tool_family` narrows the tool set announced for a turn to the families that
  turn needs. Tool schemas are resent every round, which makes them a recurring
  cost rather than a one-off.
- `p2_read_shortlist` picks line and segment windows instead of whole files.
- `p3_compaction_recorte` decides which segments the compactor must see.
- `d2_big_output_retention` drops a large tool result once it is no longer
  needed.
- `d3_post_compaction` re-injects only the chunks still relevant after a
  compaction.

Two commands line up with this. `/compact` reclaims window space on demand.
`/context` shows where the window is going, including what the tool definitions,
the skills listing and the MCP announcements cost in estimated tokens.

## Cheap lanes save money, not tokens

The `b2_*` levers route a call to a weaker or local model:

- `b2_light_model` lets the session model's lighter sibling take a call it can
  fully do, when the conversation fits that model's window.
- `b2_local_model` prefers your configured local model for calls it can finish.
- `b2_micro_effort` picks the effort level per call, when you set `/effort auto`.

Routing changes which model answers the turn, not how many tokens the turn
needs. It lowers your bill. If you are trying to fit a context window, these
levers do not help you and the layers above do.

## Turning levers off

`[jev] enabled = false` or `GROK_JEV=0` disables everything. Each lever also has
its own key under `[jev.ladder]`, and with a lever off the payloads pass through
as the bytes they were. The authoritative list of switches and their defaults is
`JevFlags::harness_default()` in
`crates/codegen/distill-workspace/src/jev/flags.rs`. Do not confuse it with
`JevFlags::default()`, an inert all-off value used by tests.

Eight levers are off by default, either because their own cost has not been
measured or because they are switchable but not yet proven: `e_retention`,
`e_cheap_compress`, `e_cheap_task`, `e_cheap_agent`, `e_lane_choice`,
`e_prompt_blocks`, `b2_model_tier` and `c6_injection_screen`.

## Inspecting what happens

```sh
GROK_LOG_JEV=1 distill
```

This writes `logs/jev.jsonl` inside the active profile, one entry per decision.
The reduction steps record entries labelled `reuse`, `crush`, `extract` and
`keep`, each with the byte counts it moved. That is how you find out which layer
is doing the work in a real session.

For the routing side, see [How Jev routes work](jev-routing.md). The capability
inventory, with the test that covers each entry, is in `list.md`.
