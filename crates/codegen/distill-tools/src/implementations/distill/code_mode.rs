//! Code Mode: the `code` tool runs model-written JavaScript whose `tools.<name>(args)` calls are the session's own tools.
//!
//! The script engine lives in `distill-code-mode` and has no access of its own. Every tool call a script makes is sent to
//! the session as a [`ScriptToolCall`]; the session runs it as it would a call from the model and replies with the text.
use educe::Educe;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot};

use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_io::ToolInput;

pub const CODE_TOOL_NAME: &str = "code";

/// Bytes of script output the model is given; a script that returns more is told so and can return less.
pub const MAX_CODE_OUTPUT_BYTES: usize = 64 * 1024;

/// The engine recurses on the native stack for nested calls and deep literals, and an overflow there would take the
/// whole process down rather than fail one script. The pages are only committed if a script actually uses them.
const SCRIPT_THREAD_STACK_BYTES: usize = 64 * 1024 * 1024;

const DESCRIPTION: &str = r#"Run JavaScript that calls your other tools, so one step does the work of many tool calls.

Use it when a step needs several tool calls (start them together with `Promise.all`, or feed one result into the next), or when a tool's output should be searched, counted or cut down before you read it. For a single call whose whole output you need, call that tool directly.

- `code` is the body of an async function. Use `await`, and `return` what you want to read back: a string comes back as is, anything else as JSON. `console.log` output comes back too.
- Each of your other tools is `await tools.<tool_name>(args)`, where `args` is the same arguments object you would pass calling that tool directly. It resolves to the text that tool would have returned to you, and throws if the call fails or is refused, so `try`/`catch` works.
- A call made by the script passes the same permission checks as a direct call.
- Plain JavaScript only: no `import`, `require`, `fetch`, timers, file or process access. Reach files, commands and the network through `tools`.
- Tools that talk to the user or steer the session (questions, plan mode, subagents, background tasks, todos) are not available inside a script; call them directly.

Example, where the `...Args` values are the arguments those tools take:
```
const [usages, tests] = await Promise.all([
  tools.${{ tools.by_kind.search | default("grep") }}(usageArgs),
  tools.${{ tools.by_kind.search | default("grep") }}(testArgs),
]);
const files = [...new Set(usages.split("\n").map((line) => line.split(":")[0]))];
return { files, untested: files.filter((file) => !tests.includes(file)) };
```"#;

/// One tool call made by a running script.
///
/// The session that owns the `code` call runs it through the same path as a call from the model (permission rules and
/// prompts, hooks, the tool itself) and sends back the text the model would have read, or why the call produced none.
#[derive(Debug)]
pub struct ScriptToolCall {
    /// The `code` tool call this request belongs to.
    pub parent_call_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// Resource: where a script's tool calls go. Present only in a session that has Code Mode on.
#[derive(Clone, Educe)]
#[educe(Debug)]
pub struct ScriptToolCallSender(#[educe(Debug(ignore))] pub mpsc::UnboundedSender<ScriptToolCall>);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeInput {
    /// JavaScript to run: the body of an async function. Call tools with `await tools.<tool_name>(args)` and `return` the result.
    pub code: String,
    /// One short line saying what the script does, shown to the user while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl TryFrom<ToolInput> for CodeInput {
    type Error = String;
    fn try_from(value: ToolInput) -> Result<Self, Self::Error> {
        match value {
            ToolInput::Dynamic(value) => {
                serde_json::from_value(value).map_err(|error| format!("CodeInput: {error}"))
            }
            _ => Err("expected Dynamic variant for CodeInput".into()),
        }
    }
}

impl From<CodeInput> for ToolInput {
    fn from(value: CodeInput) -> Self {
        ToolInput::Dynamic(serde_json::to_value(value).expect("CodeInput serializes to JSON"))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeOutput {
    pub text: String,
}

impl distill_tool_runtime::ToolOutput for CodeOutput {}

impl From<CodeOutput> for ToolOutput {
    fn from(output: CodeOutput) -> Self {
        ToolOutput::Text(output.text.into())
    }
}

#[derive(Debug, Default)]
pub struct CodeTool;

impl crate::types::tool_metadata::ToolMetadata for CodeTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Code
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }
    fn description_template(&self) -> &str {
        DESCRIPTION
    }
}

impl distill_tool_runtime::Tool for CodeTool {
    type Args = CodeInput;
    type Output = CodeOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(CODE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            CODE_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> distill_tool_protocol::ToolCapabilities {
        distill_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(distill_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.code", skip_all)]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: CodeInput,
    ) -> Result<CodeOutput, distill_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let host = resources
            .lock()
            .await
            .get::<ScriptToolCallSender>()
            .cloned()
            .ok_or_else(|| {
                distill_tool_runtime::ToolError::custom(
                    "code_mode_unavailable",
                    "Code Mode is not available in this session; call your tools directly",
                )
            })?;
        let outcome = run_script(ctx.call_id.as_ref(), input.code, &host).await?;
        render_outcome(outcome)
            .map(|text| CodeOutput { text })
            .map_err(|text| distill_tool_runtime::ToolError::custom("script_failed", text))
    }
}

/// Runs the script on its own thread and carries its tool calls to the session and back.
///
/// Dropping the returned future cancels the script: the engine sees its reply channel close at its next tool call.
async fn run_script(
    parent_call_id: &str,
    code: String,
    host: &ScriptToolCallSender,
) -> Result<distill_code_mode::ScriptOutcome, distill_tool_runtime::ToolError> {
    let (call_tx, mut call_rx) = mpsc::unbounded_channel::<distill_code_mode::ToolCall>();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<distill_code_mode::ToolReply>();
    let (done_tx, mut done_rx) = oneshot::channel::<distill_code_mode::ScriptOutcome>();
    std::thread::Builder::new()
        .name("code-mode".to_owned())
        .stack_size(SCRIPT_THREAD_STACK_BYTES)
        .spawn(move || {
            let outcome = distill_code_mode::run_script(
                &code,
                move |call| call_tx.send(call).is_ok(),
                &reply_rx,
                distill_code_mode::Limits::default(),
            );
            let _ = done_tx.send(outcome);
        })
        .map_err(|error| {
            distill_tool_runtime::ToolError::custom(
                "code_mode_unavailable",
                format!("could not start the script thread: {error}"),
            )
        })?;

    let mut in_flight = FuturesUnordered::new();
    loop {
        tokio::select! {
            // A finished script's last calls are already answered, so its outcome wins.
            biased;
            outcome = &mut done_rx => {
                return outcome.map_err(|_| {
                    distill_tool_runtime::ToolError::custom("script_failed", "the script thread ended without a result")
                });
            }
            Some(call) = call_rx.recv() => {
                let (reply, answer) = oneshot::channel();
                let sent = host.0.send(ScriptToolCall {
                    parent_call_id: parent_call_id.to_owned(),
                    tool: call.tool,
                    args: call.args,
                    reply,
                });
                in_flight.push(async move {
                    let result = match sent {
                        Ok(()) => answer.await.unwrap_or_else(|_| Err("the session ended before this call finished".to_owned())),
                        Err(_) => Err("the session is no longer running tool calls".to_owned()),
                    };
                    distill_code_mode::ToolReply { id: call.id, result }
                });
            }
            Some(reply) = in_flight.next(), if !in_flight.is_empty() => {
                let _ = reply_tx.send(reply);
            }
        }
    }
}

/// The text the model reads: `Ok` for a script that finished, `Err` for one that failed.
fn render_outcome(outcome: distill_code_mode::ScriptOutcome) -> Result<String, String> {
    let distill_code_mode::ScriptOutcome {
        value,
        logs,
        error,
        tool_calls,
    } = outcome;
    let console = if logs.is_empty() {
        String::new()
    } else {
        format!("\n\nConsole output:\n{logs}")
    };
    match error {
        Some(error) => {
            // A failed script is not rolled back, and the model must know what already happened.
            let ran = match tool_calls {
                0 => "No tool call was made.".to_owned(),
                1 => "1 tool call was made before it stopped; its effects remain.".to_owned(),
                n => format!("{n} tool calls were made before it stopped; their effects remain."),
            };
            Err(clip(format!("Script failed: {error}\n{ran}{console}")))
        }
        None => Ok(clip(match value {
            Some(value) => format!("{value}{console}"),
            None if logs.is_empty() => {
                "The script finished and returned nothing. Use `return` to send a value back."
                    .to_owned()
            }
            None => format!("The script finished and returned nothing.{console}"),
        })),
    }
}

fn clip(text: String) -> String {
    if text.len() <= MAX_CODE_OUTPUT_BYTES {
        return text;
    }
    let kept = crate::util::truncate_str(&text, MAX_CODE_OUTPUT_BYTES);
    format!(
        "{kept}\n[Output cut at {MAX_CODE_OUTPUT_BYTES} of {} bytes. Return less: filter or summarise in the script.]",
        text.len()
    )
}

#[cfg(test)]
mod tests;
