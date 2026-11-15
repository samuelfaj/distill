use std::time::Duration;

use distill_tool_runtime::Tool as _;

use super::*;
use crate::types::resources::Resources;
use crate::types::tool_metadata::test_ctx_with_call_id;

/// A session stand-in: answers every script tool call with `answer` and records what it was asked.
fn session(
    answer: impl Fn(&str, &serde_json::Value) -> Result<String, String> + Send + 'static,
) -> (
    distill_tool_runtime::ToolCallContext,
    tokio::task::JoinHandle<Vec<(String, String, serde_json::Value)>>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ScriptToolCall>();
    let mut resources = Resources::new();
    resources.insert(ScriptToolCallSender(tx));
    let ctx = test_ctx_with_call_id(resources.into_shared(), "call_outer");
    let seen = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(call) = rx.recv().await {
            let _ = call.reply.send(answer(&call.tool, &call.args));
            seen.push((call.parent_call_id, call.tool, call.args));
        }
        seen
    });
    (ctx, seen)
}

fn script(code: &str) -> CodeInput {
    CodeInput {
        code: code.to_owned(),
        description: None,
    }
}

#[tokio::test]
async fn a_scripts_tool_calls_go_to_the_session_and_only_the_return_value_comes_back() {
    // The saving Code Mode exists for: the session runs three calls, the model reads one short line.
    let (ctx, seen) = session(|tool, args| match tool {
        "grep" => Ok("src/a.rs:1:TODO one\nsrc/b.rs:9:TODO two".to_owned()),
        _ => Ok(format!(
            "{} lines",
            args["path"].as_str().unwrap_or_default().len()
        )),
    });
    let output = CodeTool
        .run(
            ctx,
            script(
                "const hits = await tools.grep({ pattern: 'TODO' });\n\
                 const files = hits.split('\\n').map((line) => line.split(':')[0]);\n\
                 const sizes = await Promise.all(files.map((path) => tools.read_file({ path })));\n\
                 return files.length + ' files: ' + sizes.join(', ');",
            ),
        )
        .await
        .expect("script succeeds");
    assert_eq!(output.text, "2 files: 8 lines, 8 lines");

    let seen = seen.await.expect("session task");
    assert_eq!(
        seen,
        vec![
            (
                "call_outer".to_owned(),
                "grep".to_owned(),
                serde_json::json!({ "pattern": "TODO" })
            ),
            (
                "call_outer".to_owned(),
                "read_file".to_owned(),
                serde_json::json!({ "path": "src/a.rs" })
            ),
            (
                "call_outer".to_owned(),
                "read_file".to_owned(),
                serde_json::json!({ "path": "src/b.rs" })
            ),
        ]
    );
}

#[tokio::test]
async fn without_a_session_to_run_its_calls_the_tool_refuses_to_start() {
    // A script whose tool calls nobody checks must not run at all.
    let ctx = test_ctx_with_call_id(Resources::new().into_shared(), "call_outer");
    let error = CodeTool
        .run(ctx, script("return 1;"))
        .await
        .expect_err("no host, no script");
    assert!(
        error.to_string().contains("Code Mode is not available"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_script_reports_what_already_ran() {
    // Nothing is rolled back, so the model has to learn that the first edit happened.
    let (ctx, seen) = session(|tool, _| match tool {
        "edit" => Ok("edited".to_owned()),
        _ => Err("permission denied for tool `bash`".to_owned()),
    });
    let error = CodeTool
        .run(
            ctx,
            script(
                "console.log('editing');\n\
                 await tools.edit({ path: 'a.rs' });\n\
                 await tools.bash({ command: 'cargo test' });\n\
                 await tools.edit({ path: 'b.rs' });",
            ),
        )
        .await
        .expect_err("the refused call fails the script");
    let text = error.to_string();
    for expected in [
        "Script failed: Error: permission denied for tool `bash`",
        "2 tool calls were made before it stopped; their effects remain.",
        "Console output:\nediting",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }
    let tools: Vec<_> = seen
        .await
        .expect("session task")
        .into_iter()
        .map(|(_, tool, _)| tool)
        .collect();
    assert_eq!(tools, ["edit", "bash"], "nothing runs after the failure");
}

#[tokio::test]
async fn cancelling_the_call_stops_the_script() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ScriptToolCall>();
    let mut resources = Resources::new();
    resources.insert(ScriptToolCallSender(tx));
    let ctx = test_ctx_with_call_id(resources.into_shared(), "call_outer");
    let run = tokio::spawn(async move {
        CodeTool
            .run(
                ctx,
                script("await tools.bash({ command: 'sleep 600' });\nawait tools.write({ path: 'late' });"),
            )
            .await
    });
    let pending = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the first call arrives")
        .expect("sender alive");
    assert_eq!(pending.tool, "bash");

    run.abort();
    let _ = run.await;
    // The cancelled call is not waited on any more, and answering it late must not resume the script.
    assert!(pending.reply.send(Ok("done".to_owned())).is_err());
    let next = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        !matches!(next, Ok(Some(_))),
        "a cancelled script made another tool call"
    );
}

#[test]
fn output_is_capped_and_says_so() {
    let big = "x".repeat(MAX_CODE_OUTPUT_BYTES * 2);
    let text = render_outcome(distill_code_mode::ScriptOutcome {
        value: Some(big.clone()),
        ..Default::default()
    })
    .expect("a finished script");
    assert!(text.len() < MAX_CODE_OUTPUT_BYTES + 200);
    assert!(
        text.ends_with(&format!(
            "[Output cut at {MAX_CODE_OUTPUT_BYTES} of {} bytes. Return less: filter or summarise in the script.]",
            big.len()
        )),
        "{}",
        &text[text.len() - 120..]
    );
}

#[test]
fn a_script_that_returns_nothing_is_told_how_to_return() {
    let text = render_outcome(distill_code_mode::ScriptOutcome::default()).expect("finished");
    assert!(text.contains("Use `return`"), "{text}");
}

#[test]
fn the_input_is_the_script_alone() {
    // Permission classification reads this input's fields; a stray `command` or `path` would make the
    // script itself look like a shell command or a file access.
    assert!(
        CodeInput::try_from(ToolInput::Dynamic(
            serde_json::json!({ "code": "return 1;" })
        ))
        .is_ok()
    );
    // Models used to shell tools send a description with the call; refusing it costs them a round.
    assert!(
        CodeInput::try_from(ToolInput::Dynamic(
            serde_json::json!({ "code": "return 1;", "description": "count the TODOs" })
        ))
        .is_ok()
    );
    assert!(
        CodeInput::try_from(ToolInput::Dynamic(
            serde_json::json!({ "code": "return 1;", "command": "rm -rf /" })
        ))
        .is_err()
    );
}
