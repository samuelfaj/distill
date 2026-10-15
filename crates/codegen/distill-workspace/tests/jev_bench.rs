// Modified for Distill by Samuel Fajreldines, 2026.
//! Live benchmarks for the Jev changes in PR #34. Every case is labelled; each
//! benchmark sends the same questions to Jev (the shipped client, OpenRouter
//! Decisions) and to chat LLMs on OpenRouter, then reports accuracy, latency
//! and the cost OpenRouter returned.
//!
//! `#[ignore]`d: they make paid calls. Run with
//! `OPENROUTER_API_KEY=... cargo test -p distill-workspace --test jev_bench -- --ignored --nocapture --test-threads=1`.
//! Results also go to `target/jev-bench/<name>.json`.

use std::collections::BTreeMap;
use std::time::Instant;

use distill_workspace::jev::catalog::verify;
use distill_workspace::jev::client::{JevClient, JevClientConfig};
use distill_workspace::jev::types::{Json, Question, QuestionId};

const KEY_ENV: &str = "OPENROUTER_API_KEY";
/// The main model and the worker model the owner runs Distill with.
const LLMS: &[&str] = &["openai/gpt-6.1-sol", "openai/gpt-6-luna"];

fn key() -> Option<String> {
    std::env::var(KEY_ENV).ok().filter(|k| !k.trim().is_empty())
}

fn jev() -> JevClient {
    JevClient::new(JevClientConfig {
        timeout: std::time::Duration::from_secs(60),
        ..JevClientConfig::default()
    })
    .expect("default config is OpenRouter Decisions")
}

/// One measured call.
#[derive(Clone, Debug, serde::Serialize)]
struct Sample {
    correct: bool,
    latency_ms: u128,
    cost_usd: f64,
    input_tokens: u64,
}

#[derive(Default, Debug, serde::Serialize)]
struct Row {
    system: String,
    samples: Vec<Sample>,
}

impl Row {
    fn new(system: &str) -> Self {
        Self { system: system.to_owned(), samples: Vec::new() }
    }
    fn accuracy(&self) -> f64 {
        let n = self.samples.len().max(1) as f64;
        self.samples.iter().filter(|s| s.correct).count() as f64 / n
    }
    fn p50_ms(&self) -> u128 {
        let mut v: Vec<u128> = self.samples.iter().map(|s| s.latency_ms).collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    }
    fn total_cost(&self) -> f64 {
        self.samples.iter().map(|s| s.cost_usd).sum()
    }
}

fn report(name: &str, rows: &[Row]) {
    println!("\n=== {name} ===");
    println!("{:<34} {:>9} {:>9} {:>12} {:>11}", "system", "accuracy", "p50 ms", "cost USD", "calls");
    for row in rows {
        println!(
            "{:<34} {:>8.0}% {:>9} {:>12.6} {:>11}",
            row.system,
            row.accuracy() * 100.0,
            row.p50_ms(),
            row.total_cost(),
            row.samples.len()
        );
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/jev-bench");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(rows).unwrap_or_default(),
    );
}

/// Jev call through the shipped client; cost comes from OpenRouter's response
/// via a raw request, since `JevAnswerSet` does not carry `usage.cost`.
async fn jev_ask(
    state: &Json,
    questions: &BTreeMap<QuestionId, Question>,
    long: bool,
) -> (Option<distill_workspace::jev::types::JevAnswerSet>, Sample) {
    let client = jev();
    let started = Instant::now();
    let result = if long { client.ask_long(state, questions).await } else { client.ask(state, questions).await };
    let latency_ms = started.elapsed().as_millis();
    let input_tokens = result.as_ref().map(|a| a.usage.input()).unwrap_or(0);
    // Published OpenRouter price for Jev: $0.042 per million input tokens, output free.
    let cost_usd = input_tokens as f64 * 0.042 / 1_000_000.0;
    (
        result.ok(),
        Sample { correct: false, latency_ms, cost_usd, input_tokens },
    )
}

/// One chat-completions call asking for a single label. Returns the trimmed
/// lowercase answer plus the measured sample (cost from OpenRouter usage).
async fn llm_label(model: &str, system: &str, user: &str) -> (String, Sample) {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": 2000,
        "usage": {"include": true},
    });
    let started = Instant::now();
    let response = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key().unwrap_or_default())
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis();
    let json: Json = match response {
        Ok(r) => r.json().await.unwrap_or(Json::Null),
        Err(_) => Json::Null,
    };
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_matches(|c: char| c == '`' || c == '"' || c == '.' || c == '\'')
        .to_ascii_lowercase();
    let sample = Sample {
        correct: false,
        latency_ms,
        cost_usd: json["usage"]["cost"].as_f64().unwrap_or(0.0),
        input_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
    };
    (text, sample)
}

fn labels_prompt(question: &str, labels: &[&str]) -> String {
    format!(
        "{question}\nAnswer with exactly one of these labels and nothing else: {}.",
        labels.join(", ")
    )
}

// ---------------------------------------------------------------------------
// 1. C2 failure triage (new hook on failed commands)
// ---------------------------------------------------------------------------

fn failure_cases() -> Vec<(&'static str, String)> {
    vec![
        ("compile_error", "error[E0308]: mismatched types\n --> src/lib.rs:42:9\n   |\n42 |     let x: u32 = \"hello\";\n   |            ---   ^^^^^^^ expected `u32`, found `&str`\nerror: could not compile `app` (lib) due to 1 previous error".into()),
        ("compile_error", "src/app.ts(17,5): error TS2339: Property 'nmae' does not exist on type 'User'. Did you mean 'name'?\nFound 1 error in src/app.ts:17".into()),
        ("test_assertion", "---- parser::tests::parses_negative stdout ----\nthread 'parser::tests::parses_negative' panicked at src/parser.rs:88:9:\nassertion `left == right` failed\n  left: 5\n right: -5\ntest result: FAILED. 41 passed; 1 failed".into()),
        ("test_assertion", "FAILED tests/test_cart.py::test_total - AssertionError: assert 19.99 == 21.99\n  where 19.99 = Cart().total()\n1 failed, 37 passed in 0.82s".into()),
        ("environment", "sh: 1: protoc: not found\nerror: failed to run custom build command for `prost-build v0.12.1`\nCould not find `protoc` installation. Install protobuf compiler.".into()),
        ("environment", "npm ERR! code EACCES\nnpm ERR! syscall mkdir\nnpm ERR! path /usr/local/lib/node_modules/pnpm\nnpm ERR! Error: EACCES: permission denied, mkdir '/usr/local/lib/node_modules/pnpm'".into()),
        ("flaky", "test api::tests::fetches_user ... FAILED\nError: reqwest::Error { kind: Request, source: hyper::Error(Connect, ConnectError(\"tcp connect error\", Os { code: 61, kind: ConnectionRefused })) }\n(passed on rerun: test api::tests::fetches_user ... ok)".into()),
        ("timeout", "test integration::full_sync has been running for over 60 seconds\nerror: test timed out after 300s: integration::full_sync\nprocess didn't exit successfully (signal: 9, SIGKILL)".into()),
    ]
}

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn bench_1_failure_triage() {
    if key().is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    let labels = ["compile_error", "test_assertion", "environment", "flaky", "timeout", "other"];
    let mut jev_row = Row::new("jev-1.14 (C2 hook)");
    let mut llm_rows: Vec<Row> = LLMS.iter().map(|m| Row::new(m)).collect();
    for (expected, output) in failure_cases() {
        let state = serde_json::json!({"tool": "run_terminal_command", "result_head": output});
        let questions = verify::failure_triage_questions().expect("catalog");
        let (answers, mut sample) = jev_ask(&state, &questions, false).await;
        let triage = answers.as_ref().map(verify::compose_failure_triage);
        sample.correct = triage.and_then(|t| t.category).as_deref() == Some(expected);
        jev_row.samples.push(sample);
        for (row, model) in llm_rows.iter_mut().zip(LLMS) {
            let (text, mut sample) = llm_label(
                model,
                "You classify failed command output for a coding agent.",
                &format!("{}\n\nOutput:\n{output}", labels_prompt("What kind of failure is this output?", &labels)),
            )
            .await;
            sample.correct = text == expected;
            row.samples.push(sample);
        }
    }
    let mut rows = vec![jev_row];
    rows.extend(llm_rows);
    report("1-failure-triage", &rows);
}

// ---------------------------------------------------------------------------
// 2. C7 change type (new hook on executed edits)
// ---------------------------------------------------------------------------

fn change_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("fix", "Fix the off-by-one in pagination", "-    for i in 0..=len {\n+    for i in 0..len {"),
        ("fix", "Handle a missing config file without panicking", "-    let cfg = std::fs::read_to_string(path).unwrap();\n+    let cfg = std::fs::read_to_string(path).unwrap_or_default();"),
        ("feat", "Add a --json flag to the status command", "+    #[arg(long)]\n+    json: bool,\n...\n+    if args.json { println!(\"{}\", serde_json::to_string(&status)?); return Ok(()); }"),
        ("docs", "Document the retry setting", "+## Retries\n+\n+Set `retries = 3` under `[http]` to retry failed requests."),
        ("test", "Cover the empty-input case", "+    #[test]\n+    fn empty_input_returns_none() {\n+        assert_eq!(parse(\"\"), None);\n+    }"),
        ("refactor", "Extract the price calculation into a helper", "-    let total = items.iter().map(|i| i.price * i.qty).sum::<u64>();\n+    let total = cart_total(&items);\n+fn cart_total(items: &[Item]) -> u64 { items.iter().map(|i| i.price * i.qty).sum() }"),
        ("chore", "Bump serde to 1.0.228", "-serde = \"1.0.219\"\n+serde = \"1.0.228\""),
        ("feat", "Support exporting the report as CSV", "+pub fn to_csv(report: &Report) -> String {\n+    let mut out = String::from(\"name,total\\n\");\n+    for row in &report.rows { out.push_str(&format!(\"{},{}\\n\", row.name, row.total)); }\n+    out\n+}"),
    ]
}

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn bench_2_change_type() {
    if key().is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    let labels = verify::CHANGE_TYPES;
    let mut jev_row = Row::new("jev-1.14 (C7 hook)");
    let mut llm_rows: Vec<Row> = LLMS.iter().map(|m| Row::new(m)).collect();
    for (expected, intent, diff) in change_cases() {
        let state = serde_json::json!({"intent": intent, "change": diff});
        let questions = verify::change_type_questions().expect("catalog");
        let (answers, mut sample) = jev_ask(&state, &questions, false).await;
        let change = answers.as_ref().map(verify::compose_change_type);
        sample.correct = change.and_then(|c| c.label).as_deref() == Some(expected);
        jev_row.samples.push(sample);
        for (row, model) in llm_rows.iter_mut().zip(LLMS) {
            let (text, mut sample) = llm_label(
                model,
                "You label code changes for release notes.",
                &format!(
                    "{}\n\nIntent: {intent}\nChange:\n{diff}",
                    labels_prompt("How should this change be labelled for release notes?", labels)
                ),
            )
            .await;
            sample.correct = text == expected;
            row.samples.push(sample);
        }
    }
    let mut rows = vec![jev_row];
    rows.extend(llm_rows);
    report("2-change-type", &rows);
}

// ---------------------------------------------------------------------------
// 3. C1/C3 end-of-turn completeness (new hook before a main turn ends)
// ---------------------------------------------------------------------------

/// (work is complete?, user request, agent's final message, todos)
fn completion_cases() -> Vec<(bool, &'static str, &'static str, Json)> {
    vec![
        (true, "Rename `fetch_user` to `load_user` everywhere and run the tests.", "Renamed `fetch_user` to `load_user` in 4 files (api.rs, cache.rs, cli.rs, tests/api.rs). `cargo test`: 52 passed, 0 failed.", serde_json::json!([{"content":"rename","status":"Completed"},{"content":"run tests","status":"Completed"}])),
        (false, "Rename `fetch_user` to `load_user` everywhere and run the tests.", "Renamed `fetch_user` to `load_user` in api.rs and cache.rs. I will update cli.rs next.", serde_json::json!([{"content":"rename","status":"InProgress"},{"content":"run tests","status":"Pending"}])),
        (true, "Add a `--verbose` flag and document it in README.", "Added `--verbose` (src/cli.rs:31) and a README section 'Verbose output'. Build passes.", serde_json::json!([])),
        (false, "Add a `--verbose` flag and document it in README.", "Added `--verbose` in src/cli.rs:31. Build passes.", serde_json::json!([])),
        (false, "Fix the failing login test and open a PR.", "Fixed the token expiry check in auth.rs; `cargo test login` passes now.", serde_json::json!([{"content":"fix test","status":"Completed"},{"content":"open PR","status":"Pending"}])),
        (true, "Fix the failing login test and open a PR.", "Fixed the token expiry check in auth.rs; `cargo test login` passes. Opened PR #212: https://github.com/acme/app/pull/212", serde_json::json!([{"content":"fix test","status":"Completed"},{"content":"open PR","status":"Completed"}])),
        (false, "Translate the error messages to Portuguese and add tests for them.", "Translated all 14 error messages to Portuguese in i18n/pt.json.", serde_json::json!([])),
        (true, "Explain why the build is slow.", "The build is slow because `syn` with full features is compiled 3 times (proc-macro, build script, lib); unify the feature set in Cargo.toml to compile it once.", serde_json::json!([])),
    ]
}

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn bench_3_end_of_turn_check() {
    if key().is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    let items = vec!["user request".to_owned()];
    let mut jev_row = Row::new("jev-1.14 (C1+C3 hook)");
    let mut llm_rows: Vec<Row> = LLMS.iter().map(|m| Row::new(m)).collect();
    let mut caught = (0usize, 0usize); // (incomplete flagged by Jev, incomplete total)
    for (complete, request, final_message, todos) in completion_cases() {
        let state = serde_json::json!({
            "user_request": request, "final_message": final_message, "todos": todos,
        });
        // The shipped hook merges both packs into one request with index prefixes.
        let mut questions: BTreeMap<QuestionId, Question> = BTreeMap::new();
        for (index, pack) in [verify::premature_stop_questions(&items), verify::completion_questions(&items)]
            .into_iter()
            .enumerate()
        {
            for (id, q) in pack.expect("catalog") {
                questions.insert(format!("{index}:{id}"), q);
            }
        }
        let (answers, mut sample) = jev_ask(&state, &questions, false).await;
        let flagged = answers.as_ref().is_some_and(|a| {
            let split = |index: usize| distill_workspace::jev::types::JevAnswerSet {
                answers: a
                    .answers
                    .iter()
                    .filter_map(|(k, v)| k.strip_prefix(&format!("{index}:")).map(|k| (k.to_owned(), v.clone())))
                    .collect(),
                ..a.clone()
            };
            let c1 = verify::compose_premature_stop(&split(0), &items);
            let c3 = verify::compose_completion(&split(1), &items);
            let c1_incomplete = !c1.deferred
                && c1.continue_working
                && c1.confidence.is_some_and(|c| c >= verify::ITEM_DONE_FLOOR);
            c1_incomplete || (!c3.deferred && !c3.complete)
        });
        sample.correct = flagged == !complete;
        if !complete {
            caught.1 += 1;
            caught.0 += usize::from(flagged);
        }
        jev_row.samples.push(sample);
        for (row, model) in llm_rows.iter_mut().zip(LLMS) {
            let (text, mut sample) = llm_label(
                model,
                "You check whether a coding agent finished what the user asked before it stops.",
                &format!(
                    "{}\n\nUser request: {request}\nAgent's final message: {final_message}\nTodos: {todos}",
                    labels_prompt("Is everything the user asked for done and shown?", &["complete", "incomplete"])
                ),
            )
            .await;
            sample.correct = (text == "complete") == complete;
            row.samples.push(sample);
        }
    }
    let mut rows = vec![jev_row];
    rows.extend(llm_rows);
    report("3-end-of-turn-check", &rows);
    println!("Jev flagged {}/{} unfinished turns (each becomes one advisory reminder).", caught.0, caught.1);
}

// ---------------------------------------------------------------------------
// 4. Long context: ask_long (new) vs the old 32 KiB state cap
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn bench_4_long_context() {
    if key().is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    // A long build log with the one real error at a varying depth.
    let positions = [0.10, 0.35, 0.60, 0.85, 0.99];
    let mut long_row = Row::new("jev ask_long (PR, 768 KiB)");
    let mut capped_row = Row::new("jev ask, old 32 KiB cap");
    let mut llm_rows: Vec<Row> = LLMS.iter().map(|m| Row::new(m)).collect();
    for (case, position) in positions.iter().enumerate() {
        let filler_lines = 14_000usize;
        let error_at = (filler_lines as f64 * position) as usize;
        let module = ["billing", "auth", "search", "export", "sync"][case];
        let mut log = String::new();
        for i in 0..filler_lines {
            if i == error_at {
                log.push_str(&format!("error[E0425]: cannot find value `rate` in module `{module}` at src/{module}.rs:{}\n", 100 + i % 50));
            } else {
                log.push_str(&format!("   Compiling crate-{i} v0.1.{} (registry)\n", i % 9));
            }
        }
        let mut criteria: BTreeMap<String, Json> = BTreeMap::new();
        for m in ["billing", "auth", "search", "export", "sync", "none"] {
            criteria.insert(m.to_owned(), Json::from(if m == "none" { "No error in the log".to_owned() } else { format!("src/{m}.rs") }));
        }
        let mut questions = BTreeMap::new();
        questions.insert(
            "failing_module".to_owned(),
            Question::choice("Which source file has the compile error in this build log?", criteria).expect("choice"),
        );
        let state = serde_json::json!({"build_log": log});
        let (answers, mut sample) = jev_ask(&state, &questions, true).await;
        sample.correct = answers.as_ref().and_then(|a| a.choice("failing_module")) == Some(module);
        long_row.samples.push(sample);

        // Old behaviour: state over 32 KiB was cut to fit (head kept).
        let head: String = log.chars().take(30 * 1024).collect();
        let (answers, mut sample) = jev_ask(&serde_json::json!({"build_log": head}), &questions, false).await;
        sample.correct = answers.as_ref().and_then(|a| a.choice("failing_module")) == Some(module);
        capped_row.samples.push(sample);

        for (row, model) in llm_rows.iter_mut().zip(LLMS) {
            let (text, mut sample) = llm_label(
                model,
                "You read build logs for a coding agent.",
                &format!(
                    "{}\n\nBuild log:\n{log}",
                    labels_prompt("Which module's source file has the compile error?", &["billing", "auth", "search", "export", "sync", "none"])
                ),
            )
            .await;
            sample.correct = text == module;
            row.samples.push(sample);
        }
    }
    let mut rows = vec![long_row, capped_row];
    rows.extend(llm_rows);
    report("4-long-context", &rows);
}

// ---------------------------------------------------------------------------
// 5. Batching: C5 priority + C2 triage in one request (PR) vs two requests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "network and paid: needs OPENROUTER_API_KEY"]
async fn bench_5_batched_failure_pass() {
    if key().is_none() {
        return eprintln!("skipping: {KEY_ENV} not set");
    }
    let output = "error[E0433]: failed to resolve: use of undeclared crate `tokio_util`\n --> src/net.rs:3:5\nerror[E0308]: mismatched types\n --> src/net.rs:41:20\nwarning: unused import: `std::fmt`\n --> src/lib.rs:1:5\nerror: could not compile `net` due to 2 previous errors";
    let errors = vec!["E0433".to_owned(), "E0308".to_owned(), "unused_import".to_owned()];
    let state = serde_json::json!({"tool": "run_terminal_command", "result_head": output});
    let mut batched = Row::new("1 request: C5+C2 (PR)");
    let mut separate = Row::new("2 requests: C5 then C2");
    for _ in 0..6 {
        let priority = verify::error_priority_questions(&errors).expect("catalog");
        let triage = verify::failure_triage_questions().expect("catalog");
        let mut merged = BTreeMap::new();
        for (index, pack) in [&priority, &triage].into_iter().enumerate() {
            for (id, q) in pack {
                merged.insert(format!("{index}:{id}"), q.clone());
            }
        }
        let (answers, mut sample) = jev_ask(&state, &merged, false).await;
        sample.correct = answers.is_some_and(|a| a.choice("1:failure_category") == Some("compile_error"));
        batched.samples.push(sample);

        let started = Instant::now();
        let (first, a) = jev_ask(&state, &priority, false).await;
        let (second, b) = jev_ask(&state, &triage, false).await;
        separate.samples.push(Sample {
            correct: first.is_some() && second.is_some_and(|s| s.choice("failure_category") == Some("compile_error")),
            latency_ms: started.elapsed().as_millis(),
            cost_usd: a.cost_usd + b.cost_usd,
            input_tokens: a.input_tokens + b.input_tokens,
        });
    }
    report("5-batched-failure-pass", &[batched, separate]);
}
