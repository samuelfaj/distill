//! Code Mode on the session side: runs the tool calls a `code` script makes.
//!
//! A script's call runs exactly as if the model had made it: `execute_tool_calls_batch` applies the permission rules
//! and prompts, the hooks, the tool and the session's bookkeeping. The one difference is where the result goes. The
//! conversation holds no tool call a script's result could answer, so the result is handed to the script instead.
use super::*;
use distill_tools::implementations::distill::code_mode::ScriptToolCall;
use distill_tools::types::tool::ToolKind;
use futures::stream::FuturesUnordered;

/// What a script may call: tools that read, search, edit, run commands or reach an integration.
///
/// Tools that talk to the user or steer the session (questions, plan mode, subagents, background tasks, todos, goals)
/// stay with the model: they end or redirect a turn, which a call buried in a script cannot do. `Code` is left out
/// too, so a script cannot start another script.
fn script_callable_kind(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Read
            | ToolKind::Edit
            | ToolKind::Delete
            | ToolKind::ListDir
            | ToolKind::Write
            | ToolKind::Move
            | ToolKind::Search
            | ToolKind::Lsp
            | ToolKind::Execute
            | ToolKind::WebSearch
            | ToolKind::WebFetch
            | ToolKind::List
            | ToolKind::MemorySearch
            | ToolKind::MemoryGet
            | ToolKind::SearchTool
            | ToolKind::UseTool
    )
}

/// The card title of a `code` call: `Code`, then the model's description of the script, else the script's first line.
pub(crate) fn script_call_title(input: &serde_json::Value) -> String {
    const SHOWN_CHARS: usize = 72;
    let first_line_of = |field: &str| {
        input
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    };
    let first_line = first_line_of("description")
        .or_else(|| first_line_of("code"))
        .unwrap_or_default();
    let shown: String = first_line.chars().take(SHOWN_CHARS).collect();
    match (shown.is_empty(), shown.len() < first_line.len()) {
        (true, _) => "Code".to_owned(),
        (false, true) => format!("Code: {shown}…"),
        (false, false) => format!("Code: {shown}"),
    }
}

#[derive(Default)]
struct ScriptCallResult {
    text: Option<String>,
    failed: bool,
}

/// The session's record of script tool calls: the ones in flight, and what they leave for the turn that ran them.
#[derive(Default)]
pub(crate) struct ScriptCalls {
    /// Calls sent by running scripts; `None` when Code Mode is off. `execute_tool_calls` serves them while the calls
    /// that started those scripts run, and holds the lock for as long as it does.
    receiver: Option<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ScriptToolCall>>>,
    next: std::sync::atomic::AtomicU64,
    in_flight: parking_lot::Mutex<std::collections::HashMap<String, ScriptCallResult>>,
    /// Hook notes, harness notices and extracted images of finished script calls. The model reads them after the
    /// script's own result, where a direct call's follow-ups would have gone.
    followups: parking_lot::Mutex<Vec<ConversationItem>>,
    /// The user's decision at a prompt raised by a script call (a rejection, a cancel, a typed follow-up). It ends
    /// the tool loop as it would for a direct call.
    stop: parking_lot::Mutex<Option<ToolLoop>>,
}

/// Forgets a script call when its future ends, including when the turn is cancelled under it.
struct InFlight<'a> {
    calls: &'a ScriptCalls,
    call_id: String,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.calls.in_flight.lock().remove(&self.call_id);
    }
}

impl ScriptCalls {
    pub(crate) fn new(
        receiver: Option<tokio::sync::mpsc::UnboundedReceiver<ScriptToolCall>>,
    ) -> Self {
        Self {
            receiver: receiver.map(tokio::sync::Mutex::new),
            ..Self::default()
        }
    }

    fn begin(&self, parent_call_id: &str) -> InFlight<'_> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let call_id = format!("{parent_call_id}.script-{n}");
        self.in_flight
            .lock()
            .insert(call_id.clone(), ScriptCallResult::default());
        InFlight {
            calls: self,
            call_id,
        }
    }

    pub(super) fn is_script_call(&self, call_id: &str) -> bool {
        self.in_flight.lock().contains_key(call_id)
    }

    /// Takes a tool result that belongs to a script call. Any other result is handed back for the conversation.
    fn divert(&self, item: ConversationItem, failed: bool) -> Option<ConversationItem> {
        let ConversationItem::ToolResult(result) = &item else {
            return Some(item);
        };
        let mut in_flight = self.in_flight.lock();
        let Some(slot) = in_flight.get_mut(&result.tool_call_id) else {
            return Some(item);
        };
        slot.text = Some(result.content.to_string());
        slot.failed |= failed;
        if !result.images.is_empty() {
            let mut message = ConversationItem::system_reminder(format!(
                "[Images from script tool call {}]\n{}",
                result.tool_call_id, result.content,
            ));
            if let ConversationItem::User(user) = &mut message {
                user.content.extend(result.images.clone());
            }
            self.followups.lock().push(message);
        }
        None
    }

    /// Harness notices appended to a script call's result. They are instructions for the model, so they go to the
    /// model verbatim instead of into a script that would filter or parse them away.
    pub(super) fn keep_notices(&self, notices: String) {
        if !notices.trim().is_empty() {
            self.followups
                .lock()
                .push(ConversationItem::system_reminder(notices));
        }
    }

    /// Drops what an interrupted turn left behind, so it cannot stop or annotate the next one.
    fn reset(&self) {
        self.followups.lock().clear();
        self.stop.lock().take();
    }

    /// Moves what the turn's script calls left into the turn's own follow-ups and outcome.
    pub(super) fn drain_into(
        &self,
        followups: &mut Vec<ConversationItem>,
        final_result: &mut Option<ToolLoop>,
    ) {
        followups.append(&mut self.followups.lock());
        if let Some(stop) = self.stop.lock().take()
            && final_result.is_none()
        {
            *final_result = Some(stop);
        }
    }
}

impl SessionActor {
    /// Where a tool call's result goes: to the script that made the call, else into the conversation.
    /// `failed` is whether the call did not do what it was asked, which a script sees as a thrown error.
    pub(super) fn push_tool_result(&self, item: ConversationItem, failed: bool) {
        if let Some(item) = self.script_calls.divert(item, failed) {
            self.chat_state_handle.push_tool_result(item);
        }
    }

    /// Runs `turn_calls` (the model's tool calls) while serving the tool calls their Code Mode scripts make.
    ///
    /// `turn_calls` is boxed so this future's layout does not contain the whole tool-call pipeline a second time:
    /// the turn future that awaits it is already close to the compiler's type-depth limit.
    pub(super) async fn serve_script_tool_calls<T>(
        &self,
        mut turn_calls: std::pin::Pin<Box<dyn std::future::Future<Output = T> + '_>>,
    ) -> T {
        let Some(receiver) = self.script_calls.receiver.as_ref() else {
            return turn_calls.await;
        };
        // Held by whichever `execute_tool_calls` got here first; a second one has nothing to serve.
        let Ok(mut receiver) = receiver.try_lock() else {
            return turn_calls.await;
        };
        self.script_calls.reset();
        let mut running = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                done = &mut turn_calls => return done,
                Some(call) = receiver.recv() => running.push(self.run_script_tool_call(call)),
                Some(()) = running.next(), if !running.is_empty() => {
                    if self.script_calls.stop.lock().is_some() {
                        // Drop pending inner calls, including any waiting on another permission prompt.
                        running.clear();
                    }
                }
            }
        }
    }

    async fn run_script_tool_call(&self, call: ScriptToolCall) {
        let ScriptToolCall {
            parent_call_id,
            tool,
            args,
            reply,
        } = call;
        // The script that made this call is gone (its turn was cancelled), so nobody asked for it any more.
        if reply.is_closed() || self.script_calls.stop.lock().is_some() {
            return;
        }
        let result = match self.script_callable(&tool) {
            Ok(()) => {
                self.execute_script_tool_call(&parent_call_id, tool, args)
                    .await
            }
            Err(refusal) => Err(refusal),
        };
        // Closing the reply ends the script outside JavaScript, so try/catch cannot ignore a user stop.
        if self.script_calls.stop.lock().is_none() {
            let _ = reply.send(result);
        }
    }

    fn script_callable(&self, tool: &str) -> Result<(), String> {
        let kinds = self.agent.borrow().tool_bridge().toolset().tool_kind_map();
        let is_mcp = |name: &str| crate::session::mcp_servers::parse_mcp_tool_name(name).is_some();
        if kinds
            .get(tool)
            .is_some_and(|kind| script_callable_kind(*kind))
            || is_mcp(tool)
        {
            return Ok(());
        }
        let mut callable: Vec<&str> = kinds
            .iter()
            .filter(|(_, kind)| script_callable_kind(**kind))
            .map(|(name, _)| name.as_str())
            .collect();
        callable.sort_unstable();
        let reason = if kinds.contains_key(tool) {
            format!("`{tool}` cannot be called from a script; call it directly")
        } else {
            format!("there is no tool named `{tool}`")
        };
        Err(format!(
            "{reason}. A script can call: {}.",
            callable.join(", ")
        ))
    }

    async fn execute_script_tool_call(
        &self,
        parent_call_id: &str,
        tool: String,
        args: serde_json::Value,
    ) -> Result<String, String> {
        let in_flight = self.script_calls.begin(parent_call_id);
        let call = crate::sampling::types::ToolCallResponse {
            id: in_flight.call_id.clone(),
            kind: "function".to_owned(),
            function: crate::sampling::types::ToolCallFunction::new(tool, args.to_string()),
        };
        let mut followups = Vec::new();
        let mut stop = None;
        let ran = async {
            // A deferred tool is refused here as it is for the model, with the reason as the result.
            let calls = self.reject_deferred_tool_calls(vec![call]).await?;
            if calls.is_empty() {
                return Ok(());
            }
            Box::pin(self.execute_tool_calls_batch(calls, &mut followups, &mut stop)).await
        }
        .await;
        let ScriptCallResult { text, failed } = self
            .script_calls
            .in_flight
            .lock()
            .remove(&in_flight.call_id)
            .unwrap_or_default();
        self.script_calls.followups.lock().append(&mut followups);
        let stopped = stop.is_some();
        if let Some(stop) = stop {
            self.script_calls.stop.lock().get_or_insert(stop);
        }
        if let Err(error) = ran {
            return Err(format!("the session could not run this call: {error}"));
        }
        match text {
            Some(text) if failed || stopped => Err(text),
            Some(text) => Ok(text),
            None => Err("the call ended without a result".to_owned()),
        }
    }
}
