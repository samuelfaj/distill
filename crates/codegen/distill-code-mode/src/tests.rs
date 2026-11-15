use std::sync::mpsc;
use std::time::Duration;

use super::*;

/// Runs `code` against a host that answers each call with `answer`, one at a time, in arrival order.
fn run_with(
    code: &str,
    limits: Limits,
    answer: impl Fn(&ToolCall) -> Result<String, String> + Send + 'static,
) -> (ScriptOutcome, Vec<ToolCall>) {
    let (call_tx, call_rx) = mpsc::channel::<ToolCall>();
    let (reply_tx, reply_rx) = mpsc::channel::<ToolReply>();
    let host = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(call) = call_rx.recv() {
            let result = answer(&call);
            seen.push(call.clone());
            if reply_tx
                .send(ToolReply {
                    id: call.id,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
        seen
    });
    let outcome = run_script(
        code,
        move |call| call_tx.send(call).is_ok(),
        &reply_rx,
        limits,
    );
    drop(reply_rx);
    (outcome, host.join().expect("host thread"))
}

fn run(code: &str) -> (ScriptOutcome, Vec<ToolCall>) {
    run_with(code, Limits::default(), |call| {
        Ok(format!("{}:{}", call.tool, call.args))
    })
}

#[test]
fn calls_started_together_are_pending_together() {
    // The point of Code Mode: one script, several tool calls in flight at once. This host answers
    // nothing until it holds both calls, so an engine that ran them one by one would never finish.
    let (call_tx, call_rx) = mpsc::channel::<ToolCall>();
    let (reply_tx, reply_rx) = mpsc::channel::<ToolReply>();
    let host = std::thread::spawn(move || {
        let first = call_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("first call");
        let second = call_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("second call arrives before the first is answered");
        // Answered out of order: each result must still reach its own caller.
        for call in [second, first] {
            reply_tx
                .send(ToolReply {
                    id: call.id,
                    result: Ok(call.tool),
                })
                .expect("reply");
        }
    });
    let outcome = run_script(
        "const [a, b] = await Promise.all([tools.read_file({ path: 'a' }), tools.grep({ pattern: 'b' })]);\n\
         return a + '+' + b;",
        move |call| call_tx.send(call).is_ok(),
        &reply_rx,
        Limits::default(),
    );
    host.join().expect("host thread");
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.value.as_deref(), Some("read_file+grep"));
    assert_eq!(outcome.tool_calls, 2);
}

#[test]
fn one_result_feeds_the_next_call() {
    let (outcome, calls) = run_with(
        "const listing = await tools.list_dir({ path: 'src' });\n\
         const first = listing.split('\\n')[0];\n\
         return await tools.read_file({ path: first });",
        Limits::default(),
        |call| match call.tool.as_str() {
            "list_dir" => Ok("src/main.rs\nsrc/lib.rs".to_owned()),
            _ => Ok(format!(
                "contents of {}",
                call.args["path"].as_str().unwrap_or_default()
            )),
        },
    );
    assert_eq!(outcome.value.as_deref(), Some("contents of src/main.rs"));
    assert_eq!(
        calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect::<Vec<_>>(),
        ["list_dir", "read_file"]
    );
    assert_eq!(calls[1].args, serde_json::json!({ "path": "src/main.rs" }));
}

#[test]
fn a_refused_call_throws_where_the_script_can_catch_it() {
    // A denied permission or a failed tool must not look like a result the script can build on.
    let refuse = |call: &ToolCall| match call.tool.as_str() {
        "bash" => Err("permission denied for tool `bash`".to_owned()),
        _ => Ok("ok".to_owned()),
    };
    let (caught, _) = run_with(
        "try { await tools.bash({ command: 'rm -rf build' }); return 'ran'; }\n\
         catch (error) { return 'refused: ' + error.message; }",
        Limits::default(),
        refuse,
    );
    assert_eq!(
        caught.value.as_deref(),
        Some("refused: permission denied for tool `bash`")
    );

    let (uncaught, calls) = run_with(
        "await tools.bash({ command: 'rm -rf build' });\nawait tools.write({ path: 'done' });",
        Limits::default(),
        refuse,
    );
    assert_eq!(uncaught.value, None);
    assert_eq!(
        uncaught.error.as_deref(),
        Some("Error: permission denied for tool `bash`")
    );
    assert_eq!(calls.len(), 1, "nothing after the refused call may run");
}

#[test]
fn the_return_value_is_what_the_model_reads() {
    assert_eq!(
        run("return 'plain text';").0.value.as_deref(),
        Some("plain text")
    );
    assert_eq!(
        run("return { edited: ['a.rs'], count: 1 };")
            .0
            .value
            .as_deref(),
        Some("{\n  \"edited\": [\n    \"a.rs\"\n  ],\n  \"count\": 1\n}")
    );
    let (nothing, _) = run("const unused = 1;");
    assert_eq!((nothing.value, nothing.error), (None, None));
}

#[test]
fn console_output_is_kept_even_when_the_script_fails() {
    let (outcome, _) = run("console.log('step', 1, { ok: true });\nthrow new TypeError('boom');");
    assert_eq!(outcome.logs, "step 1 {\"ok\":true}");
    assert_eq!(outcome.error.as_deref(), Some("TypeError: boom"));
}

#[test]
fn a_runaway_loop_ends_instead_of_pinning_the_thread() {
    let limits = Limits {
        max_loop_iterations: 1_000,
        ..Limits::default()
    };
    let (outcome, _) = run_with("while (true) {}", limits, |_| Ok(String::new()));
    let error = outcome.error.expect("the loop limit is an error");
    assert!(error.to_lowercase().contains("loop iteration"), "{error}");
}

#[test]
fn a_promise_nothing_can_settle_ends_the_script() {
    // No timers exist, so with no tool call in flight nothing can ever wake this script.
    let (outcome, _) = run("await new Promise(() => {});");
    assert_eq!(
        outcome.error.as_deref(),
        Some("the script is waiting on a promise that no tool call can settle")
    );
}

#[test]
fn a_syntax_error_points_at_the_scripts_own_line() {
    let (outcome, calls) = run("const a = 1;\nconst b = ;\nawait tools.bash({ command: 'true' });");
    let error = outcome.error.expect("syntax error");
    assert!(
        error.contains("SyntaxError") && error.contains("line 2"),
        "{error}"
    );
    assert!(
        calls.is_empty(),
        "a script that does not parse runs nothing"
    );
}

#[test]
fn the_tool_call_budget_stops_a_script_that_keeps_calling() {
    let limits = Limits {
        max_tool_calls: 3,
        ..Limits::default()
    };
    let (outcome, calls) = run_with(
        "for (let i = 0; i < 10; i++) await tools.bash({ command: 'echo ' + i });",
        limits,
        |_| Ok(String::new()),
    );
    assert_eq!(calls.len(), 3);
    assert_eq!(outcome.tool_calls, 3);
    assert_eq!(
        outcome.error.as_deref(),
        Some("Error: this script already made 3 tool calls, the most one script may make")
    );
}

#[test]
fn a_host_that_goes_away_cancels_the_script() {
    let (call_tx, call_rx) = mpsc::channel::<ToolCall>();
    let (reply_tx, reply_rx) = mpsc::channel::<ToolReply>();
    let host = std::thread::spawn(move || {
        let _call = call_rx.recv_timeout(Duration::from_secs(10)).expect("call");
        drop(reply_tx);
    });
    let outcome = run_script(
        "console.log('before');\nawait tools.bash({ command: 'sleep 600' });\nreturn 'after';",
        move |call| call_tx.send(call).is_ok(),
        &reply_rx,
        Limits::default(),
    );
    host.join().expect("host thread");
    assert_eq!(outcome.error.as_deref(), Some("the script was cancelled"));
    assert_eq!(outcome.logs, "before");
    assert_eq!(outcome.value, None);
}

#[test]
fn arguments_must_be_one_object() {
    let (outcome, calls) = run("return await tools.read_file('src/main.rs');");
    let error = outcome.error.expect("positional arguments are rejected");
    assert!(
        error.contains("tools.read_file takes one object"),
        "{error}"
    );
    assert!(calls.is_empty());
    assert_eq!(
        run("return await tools.list_dir();").1[0].args,
        serde_json::json!({})
    );
}

#[test]
fn a_script_has_no_way_out_except_tools() {
    // Permission checks sit on tool calls, so anything else that reaches a file, a process or the
    // network would be a way around them.
    let (outcome, _) = run(
        "return ['require', 'process', 'fetch', 'Deno', 'Bun', 'setTimeout', 'XMLHttpRequest', '__distill_host_call']\n\
           .filter((name) => typeof globalThis[name] !== 'undefined');",
    );
    assert_eq!(outcome.value.as_deref(), Some("[]"));
    let (import, _) = run("const fs = await import('fs');\nreturn Object.keys(fs);");
    assert!(import.error.is_some(), "{import:?}");
}

#[test]
fn each_run_starts_clean() {
    // Scripts share a thread pool; one must not see another's state or keep its host.
    let (first, _) = run("globalThis.leak = 'x'; await tools.bash({ command: 'true' }); return 1;");
    assert_eq!(first.tool_calls, 1);
    let (second, calls) = run("return typeof globalThis.leak;");
    assert_eq!(second.value.as_deref(), Some("undefined"));
    assert_eq!((second.tool_calls, calls.len()), (0, 0));
}
