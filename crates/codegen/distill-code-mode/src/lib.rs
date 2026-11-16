//! JavaScript engine behind Code Mode.
//!
//! A script is the body of an async function. It reaches the outside world only through
//! `tools.<name>(args)`, which hands a [`ToolCall`] to the host and resolves with the host's
//! [`ToolReply`]. The engine has no file, network or timer access of its own, so the host decides
//! what a script may do, one call at a time.
#![deny(clippy::indexing_slicing)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::job::{Job, JobExecutor, PromiseJob, SimpleJobExecutor};
use boa_engine::object::builtins::JsPromise;
use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, Source,
    js_string,
};

const PRELUDE: &str = include_str!("prelude.js");

/// One `tools.<name>(args)` call a script is waiting on.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Echoed in the matching [`ToolReply`]; unique within one script run.
    pub id: u32,
    pub tool: String,
    pub args: serde_json::Value,
}

/// The host's answer to a [`ToolCall`]: the tool's text, or why it did not produce one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReply {
    pub id: u32,
    pub result: Result<String, String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Tool calls one script may make; the call past it is rejected inside the script.
    pub max_tool_calls: u32,
    /// Iterations one loop may run, so `while (true) {}` ends instead of pinning a thread.
    pub max_loop_iterations: u64,
    /// Total JavaScript instructions, including nested loops and Promise jobs.
    pub max_instructions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_tool_calls: 200,
            max_loop_iterations: 10_000_000,
            max_instructions: 10_000_000,
        }
    }
}

/// How a script ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutcome {
    /// What the script returned: a string as is, anything else as indented JSON. `None` when it
    /// returned nothing or failed.
    pub value: Option<String>,
    /// Everything the script wrote with `console.*`, one call per line. Kept on failure too.
    pub logs: String,
    /// Why the script failed: a syntax error, an uncaught exception, a limit, or a host that went away.
    pub error: Option<String>,
    /// Tool calls handed to the host.
    pub tool_calls: u32,
}

struct Host {
    send: Box<dyn Fn(ToolCall) -> bool>,
    is_cancelled: Box<dyn Fn() -> bool>,
    in_flight: u32,
    calls: u32,
    max_calls: u32,
}

thread_local! {
    /// The host of the script running on this thread. A native function cannot capture state
    /// without `unsafe`, and a script owns its thread for its whole run, so the thread holds it.
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

struct HostGuard;

impl Drop for HostGuard {
    fn drop(&mut self) {
        HOST.with(|host| host.borrow_mut().take());
    }
}

fn check_cancelled() -> JsResult<()> {
    if HOST.with(|host| {
        host.borrow()
            .as_ref()
            .is_none_or(|host| (host.is_cancelled)())
    }) {
        return Err(JsNativeError::error()
            .with_message("the script was cancelled")
            .into());
    }
    Ok(())
}

/// Retain Boa's FIFO job handling, but observe cancellation even when promises keep scheduling jobs.
#[derive(Default)]
struct ScriptJobs(Rc<SimpleJobExecutor>);

impl JobExecutor for ScriptJobs {
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
        let job = match job {
            Job::PromiseJob(job) => PromiseJob::new(move |context| {
                check_cancelled()?;
                job.call(context)
            })
            .into(),
            job => job,
        };
        self.0.clone().enqueue_job(job, context);
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        check_cancelled()?;
        self.0.clone().run_jobs(context)
    }
}

#[derive(serde::Deserialize)]
struct Settled {
    ok: bool,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Runs `code` to completion on the calling thread, which it blocks while tool calls are pending.
///
/// `send` hands a call to the host and returns `false` when the host is gone. Each call is answered
/// through `replies`, in any order; dropping the sending side ends a waiting script.
/// `is_cancelled` is also checked between Promise jobs. The total instruction budget also bounds
/// JavaScript loops within each job.
pub fn run_script(
    code: &str,
    send: impl Fn(ToolCall) -> bool + 'static,
    replies: &Receiver<ToolReply>,
    limits: Limits,
    is_cancelled: impl Fn() -> bool + 'static,
) -> ScriptOutcome {
    HOST.with(|host| {
        *host.borrow_mut() = Some(Host {
            send: Box::new(send),
            is_cancelled: Box::new(is_cancelled),
            in_flight: 0,
            calls: 0,
            max_calls: limits.max_tool_calls,
        });
    });
    let _guard = HostGuard;

    let mut context = Context::builder()
        .instructions_remaining(limits.max_instructions)
        .job_executor(Rc::new(ScriptJobs::default()))
        .build()
        .expect("default script context with a job executor");
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(limits.max_loop_iterations);

    let mut outcome = ScriptOutcome::default();
    let bridge = match Bridge::install(&mut context) {
        Ok(bridge) => bridge,
        Err(error) => {
            outcome.error = Some(format!("code mode engine failed to start: {error}"));
            return outcome;
        }
    };
    match evaluate(code, &bridge, replies, &mut context) {
        Ok(Settled {
            ok: true, value, ..
        }) => outcome.value = value,
        Ok(settled) => outcome.error = Some(settled.error.unwrap_or_default()),
        Err(error) => outcome.error = Some(error),
    }
    outcome.logs = bridge.logs(&mut context);
    outcome.tool_calls = HOST.with(|host| host.borrow().as_ref().map_or(0, |host| host.calls));
    outcome
}

struct Bridge {
    settle: JsObject,
    logs: JsObject,
    run: JsObject,
}

impl Bridge {
    fn install(context: &mut Context) -> JsResult<Self> {
        context.register_global_callable(
            js_string!("__distill_host_call"),
            3,
            NativeFunction::from_fn_ptr(host_call),
        )?;
        let bridge = context.eval(Source::from_bytes(PRELUDE))?;
        let bridge = bridge
            .as_object()
            .ok_or_else(|| JsNativeError::typ().with_message("prelude returned no bridge"))?;
        let function = |name: JsString, context: &mut Context| -> JsResult<JsObject> {
            bridge
                .get(name, context)?
                .as_object()
                .filter(JsObject::is_callable)
                .ok_or_else(|| {
                    JsNativeError::typ()
                        .with_message("prelude bridge is incomplete")
                        .into()
                })
        };
        Ok(Self {
            settle: function(js_string!("settle"), context)?,
            logs: function(js_string!("logs"), context)?,
            run: function(js_string!("run"), context)?,
        })
    }

    fn logs(&self, context: &mut Context) -> String {
        self.logs
            .call(&JsValue::undefined(), &[], context)
            .ok()
            .and_then(|value| value.as_string().map(|text| text.to_std_string_lossy()))
            .unwrap_or_default()
    }
}

fn evaluate(
    code: &str,
    bridge: &Bridge,
    replies: &Receiver<ToolReply>,
    context: &mut Context,
) -> Result<Settled, String> {
    check_cancelled().map_err(|error| error.to_string())?;
    // The prefix shares the script's first line, so reported line numbers are the script's own.
    let wrapped = format!("(async () => {{{code}\n}})");
    let main = context
        .eval(Source::from_bytes(&wrapped))
        .map_err(|error| error.to_string())?;
    let promise = bridge
        .run
        .call(&JsValue::undefined(), &[main], context)
        .map_err(|error| error.to_string())?;
    let promise = promise
        .as_object()
        .and_then(|object| JsPromise::from_object(object).ok())
        .ok_or_else(|| "code mode engine returned no promise".to_owned())?;
    loop {
        context.run_jobs().map_err(|error| error.to_string())?;
        match promise.state() {
            PromiseState::Fulfilled(value) => {
                let text = value
                    .as_string()
                    .map(|text| text.to_std_string_lossy())
                    .unwrap_or_default();
                return serde_json::from_str(&text).map_err(|error| {
                    format!("code mode engine returned a malformed result: {error}")
                });
            }
            PromiseState::Rejected(error) => return Err(error.display().to_string()),
            PromiseState::Pending => {}
        }
        if HOST.with(|host| {
            host.borrow()
                .as_ref()
                .is_none_or(|host| host.in_flight == 0)
        }) {
            return Err(
                "the script is waiting on a promise that no tool call can settle".to_owned(),
            );
        }
        let Ok(reply) = replies.recv() else {
            return Err("the script was cancelled".to_owned());
        };
        HOST.with(|host| {
            if let Some(host) = host.borrow_mut().as_mut() {
                host.in_flight = host.in_flight.saturating_sub(1);
            }
        });
        let (ok, payload) = match reply.result {
            Ok(text) => (true, text),
            Err(message) => (false, message),
        };
        bridge
            .settle
            .call(
                &JsValue::undefined(),
                &[
                    JsValue::from(reply.id),
                    JsValue::from(ok),
                    JsValue::from(JsString::from(payload.as_str())),
                ],
                context,
            )
            .map_err(|error| error.to_string())?;
    }
}

/// `__distill_host_call(id, tool, argsJson)`: hands one call to the host. A thrown error rejects the
/// promise the prelude made for this call.
fn host_call(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    check_cancelled()?;
    let arg = |index: usize| args.get(index).cloned().unwrap_or_default();
    let id = arg(0).to_u32(context)?;
    let tool = arg(1).to_string(context)?.to_std_string_lossy();
    let json = arg(2).to_string(context)?.to_std_string_lossy();
    let args = match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(args @ serde_json::Value::Object(_)) => args,
        _ => {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "tools.{tool} takes one object of named arguments, e.g. tools.{tool}({{ name: value }})"
                ))
                .into());
        }
    };
    let failure = HOST.with(|host| {
        let mut host = host.borrow_mut();
        let Some(host) = host.as_mut() else {
            return Some("the script was cancelled".to_owned());
        };
        if host.calls >= host.max_calls {
            return Some(format!(
                "this script already made {} tool calls, the most one script may make",
                host.max_calls
            ));
        }
        if !(host.send)(ToolCall { id, tool, args }) {
            return Some("the script was cancelled".to_owned());
        }
        host.calls += 1;
        host.in_flight += 1;
        None
    });
    match failure {
        Some(message) => Err(JsNativeError::error().with_message(message).into()),
        None => Ok(JsValue::undefined()),
    }
}

#[cfg(test)]
mod tests;
