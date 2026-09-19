<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# Bot-relay wire fixtures

Handwritten, language-neutral JSON. These are the replay corpus for
generated clients — not produced by serializing the Rust types.

## Consumers

| Surface | Harness |
|---------|---------|
| Rust | `crates/common/distill-tool-protocol/tests/bot_relay_conformance.rs` |
| TypeScript | `frontend/packages/bot-relay-protocol/` (replay not in this PR) |
| Swift | `generated/swift/BotRelayProtocol.swift` (replay not in this PR) |
| Kotlin | `generated/kotlin/BotRelay.kt` (replay not in this PR) |

A later PR adds the TS / Swift / Kotlin replay harnesses against this
directory. Until then the Rust test is the executable consumer.

`event_hub_turn_finished.json` carries a non-empty `preview` because the
**wire schema** is a required string with no emptiness constraint.
The computer-hub emitter always ships `""` (clients read the transcript
tail). A non-empty value remains legal on the wire and must decode.

## Sequence fixtures

Each sequence file is `{ must, expectedResyncCount, expectedDistinctEvents, frames }`.
`must` is commentary. The two integers are the observations a conforming
receive path must report after ingesting `frames` in order:

- `expectedResyncCount` — resyncs observed. Only an explicit
  `hub:resync_required` frame is a resync; a seq gap is not.
- `expectedDistinctEvents` — events observed. `seq` is not a dedupe
  key; a redelivered payload with a new `seq` is a new event, and two
  frames that share a `seq` are also two events (`sequence_same_seq_two_payloads`).

The Rust `ReferenceConsumer` in
`tests/bot_relay_conformance.rs` is the behavior a TS / Swift / Kotlin
harness must reproduce: feed it `frames`, then assert its
`observed_resyncs` / `observed_events` equal those integers. Do not
re-derive the integers from the input list (counting channel tags or
`frames.length`).
