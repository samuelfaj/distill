// Modified for Distill by Samuel Fajreldines, 2026.
//! Replays a set of recorded Jev requests against Jev and writes the answers
//! as JSON lines to `target/jev-replay/<mode>.jsonl`. Any change to a Jev
//! question or threshold is judged here first, then in shadow, then in a
//! paired benchmark (docs/jev-routing.md).
//!
//! `#[ignore]`d: it makes paid calls. Run with
//! `OPENROUTER_API_KEY=... cargo test -p distill-workspace --test jev_replay -- --ignored --nocapture --test-threads=1 replay`.
//!
//! Environment:
//! - `JEV_REPLAY_SET`: the set (default: the committed fixture set).
//! - `JEV_REPLAY_MODE`: `twice` (same body twice: the noise floor, run it
//!   first), `solo` (each batched pack alone against the batch), `models`
//!   (`JEV_REPLAY_PREVIOUS_MODEL` against `JEV_REPLAY_CURRENT_MODEL` or the
//!   client default), `swap` (live wording against the swap).
//! - A swap: `JEV_REPLAY_SWAP` (a JSON file `{lever, question, instructions?,
//!   criteria?}`) or `JEV_REPLAY_SWAP_LEVER`, `JEV_REPLAY_SWAP_QUESTION`,
//!   `JEV_REPLAY_SWAP_INSTRUCTIONS`, `JEV_REPLAY_SWAP_CRITERIA` (JSON). In
//!   modes other than `swap` it applies to every ask.

use std::collections::BTreeMap;

use distill_workspace::jev::client::{JevClient, JevClientConfig};
use distill_workspace::jev::replay::{self, ReplayCase, Swap, Tally};
use distill_workspace::jev::types::{JevAnswerSet, Question, QuestionId};

const KEY_ENV: &str = "OPENROUTER_API_KEY";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn client(model: Option<&str>) -> JevClient {
    let mut config = JevClientConfig {
        timeout: std::time::Duration::from_secs(60),
        ..JevClientConfig::default()
    };
    if let Some(model) = model {
        config.model = model.to_owned();
    }
    JevClient::new(config).expect("default config is OpenRouter Decisions")
}

fn swap_from_env() -> Option<Swap> {
    let file = env("JEV_REPLAY_SWAP").map(|path| {
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    });
    Swap::from_parts(
        file.as_deref(),
        env("JEV_REPLAY_SWAP_LEVER").as_deref(),
        env("JEV_REPLAY_SWAP_QUESTION").as_deref(),
        env("JEV_REPLAY_SWAP_INSTRUCTIONS").as_deref(),
        env("JEV_REPLAY_SWAP_CRITERIA").as_deref(),
    )
    .expect("swap parses")
}

async fn ask(
    client: &JevClient,
    state: &serde_json::Value,
    questions: &BTreeMap<QuestionId, Question>,
) -> Option<JevAnswerSet> {
    match client.ask(state, questions).await {
        Ok(answers) => Some(answers),
        Err(error) => {
            eprintln!("ask failed: {error}");
            None
        }
    }
}

fn prefixed(prefix: &str, answers: JevAnswerSet) -> JevAnswerSet {
    if prefix.is_empty() {
        return answers;
    }
    JevAnswerSet {
        answers: answers
            .answers
            .into_iter()
            .map(|(id, answer)| (format!("{prefix}:{id}"), answer))
            .collect(),
        ..answers
    }
}

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn replay_recorded_states() {
    if env(KEY_ENV).is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    let set_path = env("JEV_REPLAY_SET").unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jev_replay/fixtures-v1.jsonl"
        )
        .to_owned()
    });
    let text =
        std::fs::read_to_string(&set_path).unwrap_or_else(|error| panic!("{set_path}: {error}"));
    let mut cases: Vec<ReplayCase> = replay::load_set(&text).expect("set parses");
    let mode = env("JEV_REPLAY_MODE").unwrap_or_else(|| "twice".to_owned());
    let swap = swap_from_env();
    if mode == "swap" {
        assert!(swap.is_some(), "swap mode needs a swap");
    }
    let current = client(env("JEV_REPLAY_CURRENT_MODEL").as_deref());
    let previous = (mode == "models").then(|| {
        client(Some(
            &env("JEV_REPLAY_PREVIOUS_MODEL").expect("models mode needs JEV_REPLAY_PREVIOUS_MODEL"),
        ))
    });

    let mut totals: BTreeMap<String, Tally> = BTreeMap::new();
    let mut lines = Vec::new();
    for case in &mut cases {
        let live = replay::questions_for(case).expect("questions build");
        let mut swapped = live.clone();
        if let Some(swap) = &swap {
            replay::apply_swap(case, &mut swapped, swap).expect("swap applies");
        }
        // Arm A and arm B of this mode, each one answer set over the same ids.
        let (first, second) = match mode.as_str() {
            "twice" => (
                ask(&current, &case.state, &swapped).await,
                ask(&current, &case.state, &swapped).await,
            ),
            "solo" => {
                let batched = ask(&current, &case.state, &swapped).await;
                let mut solo = JevAnswerSet {
                    model: String::new(),
                    answers: BTreeMap::new(),
                    usage: Default::default(),
                    request_id: None,
                    latency_ms: 0,
                };
                for (prefix, pack) in replay::split_packs(&swapped) {
                    if let Some(answers) = ask(&current, &case.state, &pack).await {
                        solo.model = answers.model.clone();
                        solo.answers.extend(prefixed(&prefix, answers).answers);
                    }
                }
                (batched, Some(solo))
            }
            "models" => (
                ask(
                    previous.as_ref().expect("previous client"),
                    &case.state,
                    &swapped,
                )
                .await,
                ask(&current, &case.state, &swapped).await,
            ),
            "swap" => (
                ask(&current, &case.state, &live).await,
                ask(&current, &case.state, &swapped).await,
            ),
            other => panic!("unknown JEV_REPLAY_MODE {other}"),
        };
        for (arm, answers) in [("a", &first), ("b", &second)] {
            lines.push(serde_json::json!({
                "case": case.id,
                "lever": case.lever,
                "mode": mode,
                "arm": arm,
                "model": answers.as_ref().map(|a| a.model.clone()),
                "answers": answers.as_ref().map(|a| a.answers.clone()),
            }));
        }
        if let (Some(first), Some(second)) = (&first, &second) {
            replay::tally(case, first, second, &mut totals);
        }
    }

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/jev-replay");
    std::fs::create_dir_all(&dir).expect("output dir");
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(dir.join(format!("{mode}.jsonl")), body).expect("answers written");
    println!(
        "{}",
        replay::format_table(&format!("{mode}: disagreement per lever"), &totals)
    );
}
