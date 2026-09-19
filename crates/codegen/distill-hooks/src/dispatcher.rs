use crate::config::HookSpec;
use crate::discovery::HookRegistry;
use crate::event::{HookEventEnvelope, HookEventName};
use crate::result::{HookDecision, HookRunResult, PromptDecision};
use crate::runner::{self, GateKind, HookRunnerResult, RunContext};
use crate::trust::DisabledHooks;

fn dispatch_span(event: HookEventName, hook_count: usize) -> tracing::Span {
    tracing::info_span!(
        "hooks.dispatch",
        hook_event = %event,
        hook_count = hook_count as i64,
        num_success = tracing::field::Empty,
        num_failed = tracing::field::Empty,
        num_blocking = tracing::field::Empty,
        num_skipped = tracing::field::Empty,
        total_duration_ms = tracing::field::Empty,
    )
}

/// The one per-spec disable rule: a user-disabled spec is skipped unless it is managed policy, which cannot be disabled.
pub(crate) fn is_disabled(spec: &HookSpec, disabled: &DisabledHooks) -> bool {
    (!spec.enabled || disabled.contains(&spec.name)) && !spec.is_managed_policy()
}

fn eligible_or_record_skip(
    spec: &HookSpec,
    match_value: Option<&str>,
    results: &mut Vec<HookRunResult>,
    disabled: &DisabledHooks,
) -> bool {
    if is_disabled(spec, disabled) {
        tracing::info!(hook_name = %spec.name, "hook skipped (disabled)");
        results.push(HookRunResult::Skipped {
            hook_name: spec.name.clone(),
        });
        return false;
    }
    crate::matcher::matcher_allows(spec.matcher.as_ref(), match_value)
}

/// How many hooks dispatching `envelope` would run, so the caller can announce the batch before awaiting it.
/// Filters exactly like the dispatch loops (payload match value, disabled snapshot), so the count never over-announces.
pub fn runnable_count(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> usize {
    let match_value = envelope.payload.match_value();
    registry
        .hooks_for_canonical(envelope.hook_event_name)
        .into_iter()
        .filter(|spec| !is_disabled(spec, ctx.disabled()))
        .filter(|spec| crate::matcher::matcher_allows(spec.matcher.as_ref(), match_value))
        .count()
}

pub struct InputRewrite {
    pub hook_name: String,
    pub input: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdditionalContext {
    pub hook_name: String,
    pub text: String,
}

pub struct PreToolUseResult {
    pub decision: HookDecision,
    pub updated_input: Option<InputRewrite>,
    pub additional_context: Vec<AdditionalContext>,
    pub results: Vec<HookRunResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateBlock {
    hook_name: String,
    reason: String,
}

struct SequentialGateOutcome {
    block: Option<GateBlock>,
    pending_ask: Option<PendingAsk>,
    deferring_hook: Option<String>,
    updated_input: Option<InputRewrite>,
    additional_context: Vec<AdditionalContext>,
    results: Vec<HookRunResult>,
}

// SECURITY: a hook that errors fails open (contributes nothing); only a healthy deny blocks.
async fn dispatch_sequential_gate(
    registry: &HookRegistry,
    event: HookEventName,
    gate: GateKind,
    block_verb: &str,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> SequentialGateOutcome {
    let hooks = registry.hooks_for(event);
    if hooks.is_empty() {
        return SequentialGateOutcome {
            block: None,
            pending_ask: None,
            deferring_hook: None,
            updated_input: None,
            additional_context: Vec::new(),
            results: Vec::new(),
        };
    }

    let span = dispatch_span(event, hooks.len());
    let _enter = span.enter();

    let match_value = envelope.payload.match_value().map(str::to_string);
    let mut run_results = Vec::new();
    let mut updated_input: Option<InputRewrite> = None;
    let mut additional_context: Vec<AdditionalContext> = Vec::new();
    let mut pending_ask: Option<PendingAsk> = None;
    let mut deferring_hook: Option<String> = None;

    for spec in hooks {
        if !eligible_or_record_skip(
            spec,
            match_value.as_deref(),
            &mut run_results,
            ctx.disabled(),
        ) {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info, system_message) =
            runner::run_hook(spec, envelope, ctx, gate).await;

        match result {
            HookRunnerResult::Deny { reason, .. } | HookRunnerResult::Block { reason, .. } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    reason = %reason,
                    "gate hook blocked"
                );
                run_results.push(HookRunResult::Blocked {
                    hook_name: spec.name.clone(),
                    detail: format!("{block_verb}: {reason}"),
                    elapsed,
                    http_info,
                    system_message,
                });
                record_dispatch_counts(&span, &run_results);
                return SequentialGateOutcome {
                    block: Some(GateBlock {
                        hook_name: spec.name.clone(),
                        reason,
                    }),
                    pending_ask: None,
                    deferring_hook: None,
                    updated_input: None,
                    additional_context: Vec::new(),
                    results: run_results,
                };
            }
            HookRunnerResult::Allow {
                updated_input: hook_updated_input,
                additional_context: hook_additional_context,
            } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    updated_input = hook_updated_input.is_some(),
                    additional_context = hook_additional_context.is_some(),
                    "hook allowed"
                );
                if let Some(rewrite) = hook_updated_input {
                    record_rewrite(&mut updated_input, &spec.name, rewrite);
                }
                if let Some(text) = hook_additional_context {
                    record_additional_context(&mut additional_context, &spec.name, text);
                }
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Ask {
                reason,
                updated_input: hook_updated_input,
                additional_context: hook_additional_context,
            } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    updated_input = hook_updated_input.is_some(),
                    additional_context = hook_additional_context.is_some(),
                    "hook asked"
                );
                if let Some(rewrite) = hook_updated_input {
                    record_rewrite(&mut updated_input, &spec.name, rewrite);
                }
                if let Some(text) = hook_additional_context {
                    record_additional_context(&mut additional_context, &spec.name, text);
                }
                record_ask(&mut pending_ask, &spec.name, reason);
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Defer => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook deferred"
                );
                deferring_hook = Some(spec.name.clone());
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hook_failure = %err,
                    "gate hook failed; ignoring (fail-open)"
                );
                run_results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Success
            | HookRunnerResult::Stop(_)
            | HookRunnerResult::PostToolUse { .. } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
        }
    }

    record_dispatch_counts(&span, &run_results);
    SequentialGateOutcome {
        block: None,
        pending_ask,
        deferring_hook,
        updated_input,
        additional_context,
        results: run_results,
    }
}

struct PendingAsk {
    hook_name: String,
    reason: Option<String>,
}

fn record_ask(pending: &mut Option<PendingAsk>, hook_name: &str, reason: Option<String>) {
    if let Some(replaced) = pending.replace(PendingAsk {
        hook_name: hook_name.to_string(),
        reason,
    }) {
        tracing::warn!(
            hook_name,
            replaced_hook = %replaced.hook_name,
            "a later ask replaced an earlier one"
        );
    }
}

fn record_additional_context(context: &mut Vec<AdditionalContext>, hook_name: &str, text: String) {
    context.push(AdditionalContext {
        hook_name: hook_name.to_string(),
        text,
    });
}

fn record_rewrite(
    updated_input: &mut Option<InputRewrite>,
    hook_name: &str,
    input: serde_json::Map<String, serde_json::Value>,
) {
    if let Some(replaced) = updated_input.replace(InputRewrite {
        hook_name: hook_name.to_string(),
        input,
    }) {
        tracing::warn!(
            hook_name,
            replaced_hook = %replaced.hook_name,
            "a later rewrite replaced an earlier one"
        );
    }
}

pub async fn dispatch_pre_tool_use(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> PreToolUseResult {
    let outcome = dispatch_sequential_gate(
        registry,
        HookEventName::PreToolUse,
        GateKind::Tool,
        "denied",
        envelope,
        ctx,
    )
    .await;
    let decision = match outcome.block {
        Some(GateBlock { hook_name, reason }) => HookDecision::Deny { reason, hook_name },
        None => match (outcome.pending_ask, outcome.deferring_hook) {
            (Some(ask), _) => HookDecision::Ask {
                hook_name: ask.hook_name,
                reason: ask.reason,
            },
            (None, Some(hook_name)) => HookDecision::Defer { hook_name },
            (None, None) => HookDecision::Allow,
        },
    };
    PreToolUseResult {
        decision,
        updated_input: outcome.updated_input,
        additional_context: outcome.additional_context,
        results: outcome.results,
    }
}

pub struct PromptGateResult {
    pub decision: PromptDecision,
    pub results: Vec<HookRunResult>,
}

pub async fn dispatch_prompt_gate(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> PromptGateResult {
    let outcome = dispatch_sequential_gate(
        registry,
        HookEventName::UserPromptSubmit,
        GateKind::Prompt,
        "blocked",
        envelope,
        ctx,
    )
    .await;
    PromptGateResult {
        decision: match outcome.block {
            Some(GateBlock { hook_name, reason }) => PromptDecision::Block { reason, hook_name },
            None => PromptDecision::Allow,
        },
        results: outcome.results,
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopBlock {
    pub hook_name: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct StopDispatchResult {
    pub blocks: Vec<StopBlock>,
    pub additional_context: Vec<String>,
    pub prevent_continuation: Option<StopBlock>,
    pub results: Vec<HookRunResult>,
}

impl StopDispatchResult {
    pub fn wants_continuation(&self) -> bool {
        self.prevent_continuation.is_none()
            && (!self.blocks.is_empty() || !self.additional_context.is_empty())
    }

    pub fn absorb(&mut self, hook_name: &str, signals: StopSignals) {
        if let Some(reason) = signals.stop_reason
            && self.prevent_continuation.is_none()
        {
            self.prevent_continuation = Some(StopBlock {
                hook_name: hook_name.to_string(),
                reason,
            });
        }
        if let Some(reason) = signals.block_reason {
            self.blocks.push(StopBlock {
                hook_name: hook_name.to_string(),
                reason,
            });
        }
        if let Some(context) = signals.additional_context {
            self.additional_context.push(context);
        }
    }
}

#[derive(Debug, Default)]
pub struct StopSignals {
    pub block_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub additional_context: Option<String>,
}

pub fn stop_detail(
    prevented: bool,
    prevent_reason: Option<&str>,
    block_reason: Option<&str>,
) -> Option<String> {
    if prevented {
        return Some(match prevent_reason {
            Some(reason) => format!("prevented continuation: {reason}"),
            None => "prevented continuation".to_string(),
        });
    }
    block_reason.map(|reason| format!("blocked stop: {reason}"))
}

fn stop_outcome_detail(outcome: &crate::result::StopHookOutcome) -> Option<String> {
    stop_detail(
        outcome.force_stop.is_some(),
        outcome
            .force_stop
            .as_ref()
            .and_then(|f| f.reason.as_deref()),
        outcome.block_reason.as_deref(),
    )
}

pub async fn dispatch_stop(
    registry: &HookRegistry,
    event: HookEventName,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> StopDispatchResult {
    if event.traits().gate != GateKind::Stop {
        debug_assert!(false, "dispatch_stop called with non-stop event {event:?}");
        tracing::error!(%event, "dispatch_stop called with a non-stop event; ignoring");
        return StopDispatchResult::default();
    }
    let event = event.canonical();
    let hooks = registry.hooks_for_canonical(event);
    if hooks.is_empty() {
        return StopDispatchResult::default();
    }

    let span = dispatch_span(event, hooks.len());
    let _enter = span.enter();

    let mut out = StopDispatchResult::default();
    let match_value = envelope.payload.match_value().map(str::to_string);

    for spec in hooks {
        if !eligible_or_record_skip(
            spec,
            match_value.as_deref(),
            &mut out.results,
            ctx.disabled(),
        ) {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info, system_message) =
            runner::run_hook(spec, envelope, ctx, GateKind::Stop).await;

        match result {
            HookRunnerResult::Stop(outcome) => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    block = outcome.block_reason.is_some(),
                    additional_context = outcome.additional_context.is_some(),
                    prevent_continuation = outcome.force_stop.is_some(),
                    "stop hook completed"
                );
                match stop_outcome_detail(&outcome) {
                    Some(detail) => {
                        out.results.push(HookRunResult::Blocked {
                            hook_name: spec.name.clone(),
                            detail,
                            elapsed,
                            http_info,
                            system_message,
                        });
                    }
                    None => out.results.push(HookRunResult::Success {
                        hook_name: spec.name.clone(),
                        elapsed,
                        http_info,
                        system_message,
                    }),
                }
                out.absorb(
                    &spec.name,
                    StopSignals {
                        block_reason: outcome.block_reason,
                        stop_reason: outcome.force_stop.map(|force| {
                            force
                                .reason
                                .unwrap_or_else(|| "stopped by hook".to_string())
                        }),
                        additional_context: outcome.additional_context,
                    },
                );
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hook_failure = %err,
                    "stop hook failed; ignoring (fail-open)"
                );
                out.results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Success
            | HookRunnerResult::Allow { .. }
            | HookRunnerResult::Ask { .. }
            | HookRunnerResult::Defer
            | HookRunnerResult::Deny { .. }
            | HookRunnerResult::Block { .. }
            | HookRunnerResult::PostToolUse { .. } => {
                out.results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
        }
    }

    record_dispatch_counts(&span, &out.results);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostToolUseBlock {
    pub hook_name: String,
    pub reason: String,
}

pub use crate::result::{OutputReplacement, ReplacementKind};

#[derive(Debug, Clone, PartialEq)]
pub struct SelectedReplacement {
    pub replacement: OutputReplacement,
    pub run_index: usize,
}

#[derive(Debug, Default)]
pub struct PostToolUseResult {
    pub blocks: Vec<PostToolUseBlock>,
    pub additional_context: Vec<AdditionalContext>,
    pub builtin_replacement: Option<SelectedReplacement>,
    pub mcp_replacement: Option<SelectedReplacement>,
    pub results: Vec<HookRunResult>,
}

impl PostToolUseResult {
    fn absorb(
        &mut self,
        hook_name: &str,
        run_index: usize,
        outcome: crate::result::PostToolUseHookOutcome,
    ) {
        let crate::result::PostToolUseHookOutcome {
            block_reason,
            additional_context,
            output_replacement,
        } = outcome;
        if let Some(reason) = block_reason {
            self.blocks.push(PostToolUseBlock {
                hook_name: hook_name.to_string(),
                reason,
            });
        }
        if let Some(text) = additional_context {
            self.additional_context.push(AdditionalContext {
                hook_name: hook_name.to_string(),
                text,
            });
        }
        if let Some(replacement) = output_replacement {
            self.set_replacement(replacement, run_index);
        }
    }

    pub fn merge(&mut self, other: PostToolUseResult) {
        let PostToolUseResult {
            blocks,
            additional_context,
            builtin_replacement,
            mcp_replacement,
            results,
        } = other;
        debug_assert!(
            builtin_replacement.is_none() && mcp_replacement.is_none(),
            "client results carry no output replacement"
        );
        self.results.extend(results);
        self.blocks.extend(blocks);
        self.additional_context.extend(additional_context);
    }

    fn set_replacement(&mut self, replacement: OutputReplacement, run_index: usize) {
        let slot = match replacement.kind {
            ReplacementKind::Builtin => &mut self.builtin_replacement,
            ReplacementKind::Mcp => &mut self.mcp_replacement,
        };
        if let Some(replaced) = slot.as_ref() {
            tracing::warn!(
                hook_name = replacement.hook_name.as_str(),
                wire_field = replacement.wire_field(),
                replaced_hook = replaced.replacement.hook_name.as_str(),
                "a later output replacement replaced an earlier one of the same kind"
            );
        }
        *slot = Some(SelectedReplacement {
            replacement,
            run_index,
        });
    }
}

pub async fn dispatch_post_tool_use(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> PostToolUseResult {
    let event = HookEventName::PostToolUse;
    let gate = event.traits().gate;
    debug_assert!(
        gate == GateKind::PostTool,
        "dispatch_post_tool_use gate regressed to {gate:?}"
    );
    let hooks = registry.hooks_for_canonical(event);
    if hooks.is_empty() {
        return PostToolUseResult::default();
    }

    let span = dispatch_span(event, hooks.len());
    let _enter = span.enter();

    let mut out = PostToolUseResult::default();
    let match_value = envelope.payload.match_value().map(str::to_string);

    for spec in hooks {
        if !eligible_or_record_skip(
            spec,
            match_value.as_deref(),
            &mut out.results,
            ctx.disabled(),
        ) {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info, system_message) =
            runner::run_hook(spec, envelope, ctx, gate).await;

        match result {
            HookRunnerResult::PostToolUse { outcome, failure } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    block = outcome.block_reason.is_some(),
                    additional_context = outcome.additional_context.is_some(),
                    output_replacement = outcome.output_replacement.is_some(),
                    "post_tool_use hook completed"
                );
                out.results.push(match failure {
                    Some(error) => HookRunResult::Failed {
                        hook_name: spec.name.clone(),
                        error,
                        elapsed,
                        http_info,
                        system_message,
                    },
                    None => HookRunResult::Success {
                        hook_name: spec.name.clone(),
                        elapsed,
                        http_info,
                        system_message,
                    },
                });
                let run_index = out.results.len() - 1;
                out.absorb(&spec.name, run_index, outcome);
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hook_failure = %err,
                    "post_tool_use hook failed; ignoring (fail-open)"
                );
                out.results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Success
            | HookRunnerResult::Allow { .. }
            | HookRunnerResult::Ask { .. }
            | HookRunnerResult::Defer
            | HookRunnerResult::Deny { .. }
            | HookRunnerResult::Block { .. }
            | HookRunnerResult::Stop(_) => {
                out.results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
        }
    }

    record_dispatch_counts(&span, &out.results);
    out
}

#[derive(Debug, Default)]
pub struct PostToolUseFailureResult {
    pub additional_context: Vec<AdditionalContext>,
    pub results: Vec<HookRunResult>,
}

// CC's PostToolUseFailure is context-only: it reuses the PostToolUse stdout
// parse but honors only `additionalContext` — block and output replacement are
// dropped.
pub async fn dispatch_post_tool_use_failure(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> PostToolUseFailureResult {
    let event = HookEventName::PostToolUseFailure;
    let hooks = registry.hooks_for_canonical(event);
    if hooks.is_empty() {
        return PostToolUseFailureResult::default();
    }

    let span = dispatch_span(event, hooks.len());
    let _enter = span.enter();

    let mut out = PostToolUseFailureResult::default();
    let match_value = envelope.payload.match_value().map(str::to_string);

    for spec in hooks {
        if !eligible_or_record_skip(
            spec,
            match_value.as_deref(),
            &mut out.results,
            ctx.disabled(),
        ) {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info, system_message) =
            runner::run_hook(spec, envelope, ctx, GateKind::PostTool).await;

        match result {
            HookRunnerResult::PostToolUse { outcome, failure } => {
                if let Some(text) = outcome.additional_context {
                    out.additional_context.push(AdditionalContext {
                        hook_name: spec.name.clone(),
                        text,
                    });
                }
                out.results.push(match failure {
                    Some(error) => HookRunResult::Failed {
                        hook_name: spec.name.clone(),
                        error,
                        elapsed,
                        http_info,
                        system_message,
                    },
                    None => HookRunResult::Success {
                        hook_name: spec.name.clone(),
                        elapsed,
                        http_info,
                        system_message,
                    },
                });
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hook_failure = %err,
                    "post_tool_use_failure hook failed; ignoring (fail-open)"
                );
                out.results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Success
            | HookRunnerResult::Allow { .. }
            | HookRunnerResult::Ask { .. }
            | HookRunnerResult::Defer
            | HookRunnerResult::Deny { .. }
            | HookRunnerResult::Block { .. }
            | HookRunnerResult::Stop(_) => {
                out.results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
        }
    }

    record_dispatch_counts(&span, &out.results);
    out
}

pub async fn dispatch_non_blocking(
    registry: &HookRegistry,
    event: HookEventName,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> Vec<HookRunResult> {
    debug_assert!(
        event.traits().gate == GateKind::Observe,
        "dispatch_non_blocking called with gate event {event:?}"
    );
    let hooks = registry.hooks_for_canonical(event);
    if hooks.is_empty() {
        return Vec::new();
    }

    let span = dispatch_span(event, hooks.len());
    let _enter = span.enter();

    let match_value = envelope.payload.match_value().map(str::to_string);
    let mut results = Vec::with_capacity(hooks.len());

    for spec in hooks {
        if !eligible_or_record_skip(spec, match_value.as_deref(), &mut results, ctx.disabled()) {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info, system_message) =
            runner::run_hook(spec, envelope, ctx, GateKind::Observe).await;

        match result {
            HookRunnerResult::Success => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    hook_failure = %err,
                    "hook failed"
                );
                results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Allow { .. }
            | HookRunnerResult::Ask { .. }
            | HookRunnerResult::Defer
            | HookRunnerResult::Deny { .. }
            | HookRunnerResult::Block { .. } => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
            HookRunnerResult::Stop(_) | HookRunnerResult::PostToolUse { .. } => {
                results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: "a gate hook result routed to the observe dispatch".to_string(),
                    elapsed,
                    http_info,
                    system_message,
                });
            }
        }
    }

    record_dispatch_counts(&span, &results);

    results
}

fn record_dispatch_counts(span: &tracing::Span, results: &[HookRunResult]) {
    let mut num_success = 0i64;
    let mut num_failed = 0i64;
    let mut num_skipped = 0i64;
    let mut total_duration_ms = 0i64;
    let mut num_blocked = 0i64;
    for r in results {
        match r {
            HookRunResult::Success { elapsed, .. } => {
                num_success += 1;
                total_duration_ms += elapsed.as_millis() as i64;
            }
            HookRunResult::Blocked { elapsed, .. } => {
                num_blocked += 1;
                total_duration_ms += elapsed.as_millis() as i64;
            }
            HookRunResult::Failed { elapsed, .. } => {
                num_failed += 1;
                total_duration_ms += elapsed.as_millis() as i64;
            }
            HookRunResult::Skipped { .. } => num_skipped += 1,
        }
    }
    span.record("num_success", num_success);
    span.record("num_failed", num_failed);
    span.record("num_blocking", num_blocked);
    span.record("num_skipped", num_skipped);
    span.record("total_duration_ms", total_duration_ms);
}

pub fn hub_hook_kind(event: HookEventName) -> Option<String> {
    event.traits().hub_forward.then(|| format!("hook.{event}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookSpec;
    use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
    use crate::matcher::HookMatcher;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn pre_tool_use_envelope(tool_name: &str) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::PreToolUse {
                tool_name: tool_name.into(),
                tool_use_id: "tu-1".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                tool_input_truncated: false,
                subagent_type: None,
            },
        }
    }

    fn session_start_envelope() -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::SessionStart,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::SessionStart {
                source: "new".into(),
                model_id: None,
                agent_type: None,
            },
        }
    }

    fn run_ctx() -> RunContext<'static> {
        RunContext {
            session_id: "test-session",
            workspace_root: "/tmp",
            process_scope: None,
            disabled: Default::default(),
        }
    }

    fn make_command_spec(
        name: &str,
        matcher: Option<&str>,
        enabled: bool,
        script: &str,
    ) -> HookSpec {
        HookSpec {
            name: name.into(),
            event: HookEventName::PreToolUse,
            handler_type: crate::config::HandlerType::Command,
            configured_matcher: matcher.map(|s| s.to_string()),
            matcher: matcher.map(|s| HookMatcher::new(s).unwrap()),
            enabled,
            command: Some(PathBuf::from(script)),
            command_raw: Some(script.to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: PathBuf::from("/tmp"),
            extra_env: HashMap::new(),
            layer: crate::config::HookProvenance::File,
        }
    }

    fn registry_from_specs(specs: Vec<HookSpec>) -> HookRegistry {
        let (mut registry, _) = crate::discovery::load_hooks(None, None);
        registry.append_specs(specs);
        registry
    }

    #[test]
    fn match_value_extracts_per_payload_field() {
        assert_eq!(
            pre_tool_use_envelope("run_terminal_cmd")
                .payload
                .match_value(),
            Some("run_terminal_cmd")
        );
        assert_eq!(session_start_envelope().payload.match_value(), Some("new"));

        let notification = HookPayload::Notification {
            notification_type: "permission_prompt".into(),
            message: None,
            title: None,
            level: None,
        };
        assert_eq!(notification.match_value(), Some("permission_prompt"));
    }

    #[test]
    fn subagent_match_value_is_none_when_type_empty() {
        let mut envelope = stop_envelope();
        envelope.hook_event_name = HookEventName::SubagentStop;
        let payload = |subagent_type: &str| HookPayload::SubagentStop {
            phase: crate::event::SubagentStopPhase::Observe,
            subagent_id: "sub-1".into(),
            subagent_type: subagent_type.into(),
            stop_hook_active: None,
            last_assistant_message: None,
        };
        envelope.payload = payload("explore");
        assert_eq!(envelope.payload.match_value(), Some("explore"));
        envelope.payload = payload("");
        assert_eq!(envelope.payload.match_value(), None);
    }

    #[tokio::test]
    async fn empty_registry_allows() {
        let registry = registry_from_specs(vec![]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn pre_tool_use_carries_last_updated_input() {
        let first = make_command_spec(
            "first",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"one\"}}}'",
        );
        let second = make_command_spec(
            "second",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"two\"}}}'",
        );
        let registry = registry_from_specs(vec![first, second]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert_eq!(result.decision, HookDecision::Allow);
        let rewrite = result.updated_input.expect("updatedInput carried");
        assert_eq!(
            rewrite.input.get("command").and_then(|v| v.as_str()),
            Some("two")
        );
        assert_eq!(rewrite.hook_name, "second");
    }

    #[tokio::test]
    async fn rewrite_is_invisible_to_later_hooks() {
        let rewriter = make_command_spec(
            "rewriter",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"rewritten\"}}}'",
        );
        let gate = make_command_spec(
            "gate",
            Some("run_terminal_cmd"),
            true,
            "if grep -q '\"command\":\"ls\"'; \
             then echo '{\"decision\":\"deny\",\"reason\":\"saw the original\"}'; \
             else echo '{\"decision\":\"deny\",\"reason\":\"saw the rewrite\"}'; fi",
        );
        let registry = registry_from_specs(vec![rewriter, gate]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        match result.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "saw the original"),
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_drops_earlier_rewrite() {
        let rewriter = make_command_spec(
            "rewriter",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"one\"}}}'",
        );
        let denier = make_command_spec(
            "denier",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"blocked\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![rewriter, denier]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(matches!(result.decision, HookDecision::Deny { .. }));
        assert!(
            result.updated_input.is_none(),
            "a deny must drop any earlier rewrite"
        );
    }

    #[tokio::test]
    async fn failing_hook_keeps_an_earlier_rewrite() {
        let rewriter = make_command_spec(
            "rewriter",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"one\"}}}'",
        );
        let failing = make_command_spec("failing", Some("run_terminal_cmd"), true, "exit 1");
        let registry = registry_from_specs(vec![rewriter, failing]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert_eq!(result.decision, HookDecision::Allow);
        let rewrite = result
            .updated_input
            .expect("the earlier rewrite must survive a later failure");
        assert_eq!(
            rewrite.input.get("command").and_then(|v| v.as_str()),
            Some("one")
        );
        assert_eq!(rewrite.hook_name, "rewriter");
    }

    #[tokio::test]
    async fn ask_then_deny_denies() {
        let ask = make_command_spec(
            "asker",
            None,
            true,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm"}}'"#,
        );
        let deny = make_command_spec(
            "denier",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"nope\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![ask, deny]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        match result.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "nope"),
            ref other => panic!("expected Deny to win over ask, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_wins_over_allow_in_either_order() {
        let allow = || make_command_spec("allower", None, true, "echo '{\"decision\":\"allow\"}'");
        let ask = || {
            make_command_spec(
                "asker",
                None,
                true,
                r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask"}}'"#,
            )
        };
        for specs in [vec![allow(), ask()], vec![ask(), allow()]] {
            let order: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
            let registry = registry_from_specs(specs);
            let result = dispatch_pre_tool_use(
                &registry,
                &pre_tool_use_envelope("run_terminal_cmd"),
                &run_ctx(),
            )
            .await;
            assert!(
                matches!(result.decision, HookDecision::Ask { ref hook_name, .. } if hook_name == "asker"),
                "ask must win over allow in order {order:?}, got {:?}",
                result.decision
            );
        }
    }

    #[tokio::test]
    async fn ask_carries_updated_input() {
        let spec = make_command_spec(
            "ask-rewrite",
            Some("run_terminal_cmd"),
            true,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"command":"safe"}}}'"#,
        );
        let registry = registry_from_specs(vec![spec]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(matches!(result.decision, HookDecision::Ask { .. }));
        let rewrite = result.updated_input.expect("ask carries updatedInput");
        assert_eq!(
            rewrite.input.get("command").and_then(|v| v.as_str()),
            Some("safe")
        );
    }

    #[tokio::test]
    async fn ask_wins_over_defer() {
        let defer_spec = || {
            make_command_spec(
                "deferrer",
                None,
                true,
                r#"echo '{"hookSpecificOutput":{"permissionDecision":"defer"}}'"#,
            )
        };
        let registry = registry_from_specs(vec![defer_spec()]);
        let deferred = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(matches!(deferred.decision, HookDecision::Defer { .. }));

        let ask = make_command_spec(
            "asker",
            None,
            true,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask"}}'"#,
        );
        let registry = registry_from_specs(vec![defer_spec(), ask]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(
            matches!(result.decision, HookDecision::Ask { ref hook_name, .. } if hook_name == "asker"),
            "an ask must outrank a defer, got {:?}",
            result.decision
        );
    }

    #[tokio::test]
    async fn defer_wins_over_allow() {
        let allow = make_command_spec(
            "allower",
            None,
            true,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#,
        );
        let defer = make_command_spec(
            "deferrer",
            None,
            true,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"defer"}}'"#,
        );
        let registry = registry_from_specs(vec![allow, defer]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(
            matches!(result.decision, HookDecision::Defer { ref hook_name } if hook_name == "deferrer"),
            "a defer must outrank an allow, got {:?}",
            result.decision
        );
    }

    #[tokio::test]
    async fn additional_context_accumulates_in_call_order_and_a_deny_drops_it() {
        let ctx_spec = |name: &str, text: &str| {
            make_command_spec(
                name,
                None,
                true,
                &format!(
                    r#"echo '{{"hookSpecificOutput":{{"permissionDecision":"allow","additionalContext":"{text}"}}}}'"#
                ),
            )
        };
        let registry = registry_from_specs(vec![
            ctx_spec("first", "heads up"),
            ctx_spec("second", "and also"),
        ]);
        let allowed = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert_eq!(allowed.decision, HookDecision::Allow);
        let carried: Vec<(&str, &str)> = allowed
            .additional_context
            .iter()
            .map(|context| (context.hook_name.as_str(), context.text.as_str()))
            .collect();
        assert_eq!(
            carried,
            [("first", "heads up"), ("second", "and also")],
            "every hook's context must survive, in call order"
        );

        let deny = make_command_spec(
            "denier",
            None,
            true,
            r#"echo '{"decision":"deny","reason":"nope"}'; exit 2"#,
        );
        let registry = registry_from_specs(vec![ctx_spec("first", "heads up"), deny]);
        let denied = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert!(matches!(denied.decision, HookDecision::Deny { .. }));
        assert!(
            denied.additional_context.is_empty(),
            "a deny must drop additionalContext"
        );
    }

    #[tokio::test]
    async fn pre_tool_use_surfaces_system_message() {
        let with_msg = make_command_spec(
            "with-msg",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"systemMessage\":\"heads up\"}'",
        );
        let without = make_command_spec("without", Some("run_terminal_cmd"), true, "exit 0");
        let registry = registry_from_specs(vec![with_msg, without]);
        let result = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        assert_eq!(result.decision, HookDecision::Allow);
        assert!(matches!(
            result.results.first().unwrap_or_else(|| panic!("expected results item 0")),
            HookRunResult::Success { system_message: Some(msg), .. } if msg == "heads up"
        ));
        assert!(matches!(
            result
                .results
                .get(1)
                .unwrap_or_else(|| panic!("expected results item 1")),
            HookRunResult::Success {
                system_message: None,
                ..
            }
        ));
    }

    fn prompt_submit_envelope() -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::UserPromptSubmit,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: Some("p-1".into()),
            permission_mode: None,
            payload: HookPayload::UserPromptSubmit {
                prompt: Some("deploy to prod".into()),
                subagent_type: None,
            },
        }
    }

    fn make_prompt_spec(name: &str, script: &str) -> HookSpec {
        let mut spec = make_command_spec(name, None, true, script);
        spec.event = HookEventName::UserPromptSubmit;
        spec
    }

    #[tokio::test]
    async fn prompt_gate_blocks_on_exit_2_with_stderr_reason() {
        let spec = make_prompt_spec("prompt-gate", "echo 'no prod deploys' >&2; exit 2");
        let registry = registry_from_specs(vec![spec]);
        let result = dispatch_prompt_gate(&registry, &prompt_submit_envelope(), &run_ctx()).await;
        match result.decision {
            PromptDecision::Block {
                ref reason,
                ref hook_name,
            } => {
                assert_eq!(reason, "no prod deploys");
                assert_eq!(hook_name, "prompt-gate");
            }
            ref other => panic!("expected Block, got {other:?}"),
        }
        assert!(
            matches!(result.results.first(), Some(HookRunResult::Blocked { .. })),
            "a block must record HookRunResult::Blocked for telemetry"
        );
    }

    #[tokio::test]
    async fn prompt_gate_json_block_short_circuits_later_hooks() {
        let first = make_prompt_spec(
            "first",
            "echo '{\"decision\":\"block\",\"reason\":\"stop right there\"}'",
        );
        let second = make_prompt_spec("second", "exit 0");
        let registry = registry_from_specs(vec![first, second]);
        let result = dispatch_prompt_gate(&registry, &prompt_submit_envelope(), &run_ctx()).await;
        match result.decision {
            PromptDecision::Block {
                ref reason,
                ref hook_name,
            } => {
                assert_eq!(reason, "stop right there");
                assert_eq!(hook_name, "first");
            }
            ref other => panic!("expected Block, got {other:?}"),
        }
        assert_eq!(result.results.len(), 1, "the second hook must not run");
    }

    #[tokio::test]
    async fn prompt_gate_failure_fails_open() {
        let spec = make_prompt_spec("broken", "echo 'oops' >&2; exit 1");
        let registry = registry_from_specs(vec![spec]);
        let result = dispatch_prompt_gate(&registry, &prompt_submit_envelope(), &run_ctx()).await;
        assert_eq!(result.decision, PromptDecision::Allow);
        assert!(matches!(
            result.results.first(),
            Some(HookRunResult::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn prompt_gate_allows_and_discards_stdout() {
        let spec = make_prompt_spec("context-only", "echo 'extra context the model never sees'");
        let registry = registry_from_specs(vec![spec]);
        let result = dispatch_prompt_gate(&registry, &prompt_submit_envelope(), &run_ctx()).await;
        assert_eq!(result.decision, PromptDecision::Allow);
        assert!(matches!(
            result.results.first(),
            Some(HookRunResult::Success { .. })
        ));
    }

    #[tokio::test]
    async fn disabled_hook_is_skipped_allows() {
        let spec = make_command_spec(
            "disabled-deny",
            None,
            false,
            "echo '{\"decision\":\"deny\",\"reason\":\"should not run\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn matcher_filters_by_tool() {
        let spec = make_command_spec(
            "bash-deny",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"bash blocked\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);

        let fired = dispatch_pre_tool_use(
            &registry,
            &pre_tool_use_envelope("run_terminal_cmd"),
            &run_ctx(),
        )
        .await;
        match fired.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "bash blocked"),
            ref other => panic!("expected Deny, got {other:?}"),
        }

        let skipped =
            dispatch_pre_tool_use(&registry, &pre_tool_use_envelope("read_file"), &run_ctx()).await;
        assert_eq!(skipped.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn first_deny_wins_short_circuits() {
        let deny_spec = make_command_spec(
            "first-deny",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"first says no\"}'; exit 2",
        );
        let allow_spec = make_command_spec(
            "second-allow",
            None,
            true,
            "echo '{\"decision\":\"allow\"}'",
        );
        let registry = registry_from_specs(vec![deny_spec, allow_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref reason,
                ref hook_name,
            } => {
                assert_eq!(reason, "first says no");
                assert_eq!(hook_name, "first-deny");
            }
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_then_deny_denies() {
        let allow_spec =
            make_command_spec("broad-allow", None, true, "echo '{\"decision\":\"allow\"}'");
        let deny_spec = make_command_spec(
            "strict-deny",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"strict policy\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![allow_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref reason,
                ref hook_name,
            } => {
                assert_eq!(reason, "strict policy");
                assert_eq!(hook_name, "strict-deny");
            }
            ref other => panic!("expected Deny from strict filter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fail_open_on_hook_crash() {
        let spec = make_command_spec("crasher", None, true, "exit 1");
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(
            result.decision,
            HookDecision::Allow,
            "fail-open: a crashing hook must not block the tool call"
        );
        assert_eq!(result.results.len(), 1);
        assert!(
            matches!(result.results.first(), Some(HookRunResult::Failed { hook_name, .. }) if hook_name == "crasher"),
            "the failure must still appear in run_results for UI scrollback, got {:?}",
            result.results
        );
    }

    #[tokio::test]
    async fn fail_open_then_deny_lets_deny_win() {
        let crash_spec = make_command_spec("crasher", None, true, "exit 1");
        let deny_spec = make_command_spec(
            "denier",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"nope\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![crash_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref hook_name,
                ref reason,
            } => {
                assert_eq!(hook_name, "denier");
                assert_eq!(reason, "nope");
            }
            ref other => panic!("expected Deny from explicit denier, got {other:?}"),
        }
        assert_eq!(result.results.len(), 2);
        assert!(
            matches!(result.results.get(1), Some(HookRunResult::Blocked { detail, .. })
                if detail == "denied: nope"),
            "a deny is the hook's decision, not a failure: {:?}",
            result.results.get(1)
        );
    }

    fn stop_envelope() -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::Stop,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::Stop {
                reason: "end_turn".into(),
                stop_hook_active: false,
                last_assistant_message: Some("done".into()),
                background_tasks: None,
                session_crons: None,
            },
        }
    }

    fn stop_spec(name: &str, script: &str) -> HookSpec {
        let mut spec = make_command_spec(name, None, true, script);
        spec.event = HookEventName::Stop;
        spec
    }

    #[test]
    fn absorb_folds_signals_with_first_force_stop_winning() {
        let mut out = StopDispatchResult::default();
        out.absorb(
            "b1",
            StopSignals {
                block_reason: Some("first block".into()),
                ..Default::default()
            },
        );
        out.absorb(
            "s1",
            StopSignals {
                stop_reason: Some("stop now".into()),
                additional_context: Some("ctx".into()),
                ..Default::default()
            },
        );
        out.absorb(
            "s2",
            StopSignals {
                stop_reason: Some("too late".into()),
                block_reason: Some("second block".into()),
                ..Default::default()
            },
        );

        assert!(!out.wants_continuation(), "a force-stop overrides blocks");
        assert_eq!(
            out.blocks
                .iter()
                .map(|b| b.reason.as_str())
                .collect::<Vec<_>>(),
            ["first block", "second block"]
        );
        assert_eq!(out.additional_context, ["ctx"]);
        let prevent = out
            .prevent_continuation
            .as_ref()
            .expect("force-stop captured");
        assert_eq!(prevent.hook_name, "s1");
        assert_eq!(prevent.reason, "stop now");
    }

    #[tokio::test]
    async fn stop_collects_all_blocks() {
        let registry = registry_from_specs(vec![
            stop_spec("b1", "echo '{\"decision\":\"block\",\"reason\":\"first\"}'"),
            stop_spec("allow", "echo ok"),
            stop_spec(
                "b2",
                "echo '{\"decision\":\"block\",\"reason\":\"second\"}'",
            ),
        ]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(result.wants_continuation());
        assert_eq!(
            result
                .blocks
                .iter()
                .map(|b| b.reason.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(result.results.len(), 3, "all hooks must have run");
    }

    #[tokio::test]
    async fn stop_prevent_continuation_overrides_blocks() {
        let registry = registry_from_specs(vec![
            stop_spec(
                "blocker",
                "echo '{\"decision\":\"block\",\"reason\":\"keep going\"}'",
            ),
            stop_spec(
                "stopper",
                "echo '{\"continue\":false,\"stopReason\":\"enough\"}'",
            ),
        ]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(!result.wants_continuation());
        let prevent = result
            .prevent_continuation
            .expect("continue:false captured");
        assert_eq!(prevent.hook_name, "stopper");
        assert_eq!(prevent.reason, "enough");
        assert_eq!(result.blocks.len(), 1);
    }

    #[tokio::test]
    async fn stop_exit2_fail_open_and_context() {
        let registry = registry_from_specs(vec![
            stop_spec("exit2", "echo 'fix the build' >&2; exit 2"),
            stop_spec("crasher", "exit 1"),
            stop_spec(
                "ctx",
                "echo '{\"hookSpecificOutput\":{\"additionalContext\":\"note\"}}'",
            ),
        ]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(result.wants_continuation());
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(
            result.blocks.first().map(|b| b.reason.as_str()),
            Some("fix the build")
        );
        assert_eq!(result.additional_context, ["note"]);
    }

    #[tokio::test]
    async fn stop_additional_context_only_keeps_working() {
        let registry = registry_from_specs(vec![stop_spec(
            "ctx",
            "echo '{\"hookSpecificOutput\":{\"additionalContext\":\"run the tests\"}}'",
        )]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(
            result.wants_continuation(),
            "context alone must keep working"
        );
        assert!(result.blocks.is_empty());
        assert!(result.prevent_continuation.is_none());
        assert_eq!(result.additional_context, ["run the tests"]);
    }

    #[tokio::test]
    async fn stop_timeout_fails_open() {
        let mut spec = stop_spec(
            "slow",
            "echo '{\"decision\":\"block\",\"reason\":\"late\"}'; sleep 5",
        );
        spec.timeout_ms = 200;
        let registry = registry_from_specs(vec![spec]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(
            !result.wants_continuation(),
            "timeout must not block the stop"
        );
        assert!(
            matches!(result.results.first(), Some(HookRunResult::Failed { .. })),
            "the timeout is recorded as a failure, got {:?}",
            result.results.first()
        );
    }

    #[tokio::test]
    async fn stop_empty_and_allowing_registries_allow_stop() {
        let registry = registry_from_specs(vec![]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(!result.wants_continuation());
        assert!(result.results.is_empty());

        let registry = registry_from_specs(vec![stop_spec("ok", "echo done")]);
        let result =
            dispatch_stop(&registry, HookEventName::Stop, &stop_envelope(), &run_ctx()).await;
        assert!(!result.wants_continuation());
    }

    #[tokio::test]
    async fn subagent_stop_consults_alias_specs() {
        let mut canonical = make_command_spec(
            "canonical",
            None,
            true,
            "echo '{\"decision\":\"block\",\"reason\":\"from canonical\"}'",
        );
        canonical.event = HookEventName::SubagentStop;
        let mut alias = make_command_spec(
            "alias",
            None,
            true,
            "echo '{\"decision\":\"block\",\"reason\":\"from alias\"}'",
        );
        alias.event = HookEventName::SubagentEnd;
        let registry = registry_from_specs(vec![canonical, alias]);

        let mut envelope = stop_envelope();
        envelope.hook_event_name = HookEventName::SubagentStop;
        envelope.payload = HookPayload::SubagentStop {
            phase: crate::event::SubagentStopPhase::Gate,
            subagent_id: "sub-1".into(),
            subagent_type: "explore".into(),
            stop_hook_active: Some(false),
            last_assistant_message: None,
        };
        let result = dispatch_stop(
            &registry,
            HookEventName::SubagentStop,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(result.blocks.len(), 2);
    }

    #[tokio::test]
    async fn subagent_stop_matcher_filters_by_agent_type() {
        let mut reviewer = make_command_spec(
            "reviewer",
            Some("code-reviewer"),
            true,
            "echo '{\"decision\":\"block\",\"reason\":\"from reviewer\"}'",
        );
        reviewer.event = HookEventName::SubagentStop;
        let mut explorer = make_command_spec(
            "explorer",
            Some("explore"),
            true,
            "echo '{\"decision\":\"block\",\"reason\":\"from explorer\"}'",
        );
        explorer.event = HookEventName::SubagentStop;
        let registry = registry_from_specs(vec![reviewer, explorer]);

        let mut envelope = stop_envelope();
        envelope.hook_event_name = HookEventName::SubagentStop;
        envelope.payload = HookPayload::SubagentStop {
            phase: crate::event::SubagentStopPhase::Gate,
            subagent_id: "sub-1".into(),
            subagent_type: "explore".into(),
            stop_hook_active: Some(false),
            last_assistant_message: None,
        };
        let result = dispatch_stop(
            &registry,
            HookEventName::SubagentStop,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(result.blocks.len(), 1, "only the matching spec runs");
        assert_eq!(
            result.blocks.first().map(|b| b.reason.as_str()),
            Some("from explorer")
        );
    }

    #[tokio::test]
    async fn non_blocking_failure_does_not_stop_chain() {
        let mut spec1 = make_command_spec("crasher", None, true, "exit 1");
        spec1.event = HookEventName::SessionStart;
        let mut spec2 = make_command_spec("ok", None, true, "echo ok");
        spec2.event = HookEventName::SessionStart;
        let registry = registry_from_specs(vec![spec1, spec2]);
        let envelope = session_start_envelope();
        let results = dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(matches!(
            results
                .first()
                .unwrap_or_else(|| panic!("expected results item 0: {results:?}")),
            HookRunResult::Failed { .. }
        ));
        assert!(matches!(
            results
                .get(1)
                .unwrap_or_else(|| panic!("expected results item 1: {results:?}")),
            HookRunResult::Success { .. }
        ));
    }

    #[test]
    fn hub_hook_kind_maps_all_hub_forwarded_events() {
        assert_eq!(hub_hook_kind(HookEventName::PreToolUse), None);

        let cases: &[(HookEventName, &str)] = &[
            (HookEventName::SessionStart, "hook.session_start"),
            (HookEventName::SessionEnd, "hook.session_end"),
            (HookEventName::Stop, "hook.stop"),
            (HookEventName::StopFailure, "hook.stop_failure"),
            (HookEventName::StopCancelled, "hook.stop_cancelled"),
            (HookEventName::PostToolUse, "hook.post_tool_use"),
            (
                HookEventName::PostToolUseFailure,
                "hook.post_tool_use_failure",
            ),
            (HookEventName::PermissionDenied, "hook.permission_denied"),
            (HookEventName::UserPromptSubmit, "hook.user_prompt_submit"),
            (HookEventName::Notification, "hook.notification"),
            (HookEventName::SubagentStart, "hook.subagent_start"),
            (HookEventName::SubagentStop, "hook.subagent_stop"),
            (HookEventName::SubagentEnd, "hook.subagent_stop"),
            (HookEventName::PreCompact, "hook.pre_compact"),
            (HookEventName::PostCompact, "hook.post_compact"),
        ];

        let total_variants = |e: HookEventName| -> usize {
            match e {
                HookEventName::SessionStart
                | HookEventName::SessionEnd
                | HookEventName::Stop
                | HookEventName::StopFailure
                | HookEventName::StopCancelled
                | HookEventName::PreToolUse
                | HookEventName::PostToolUse
                | HookEventName::PostToolUseFailure
                | HookEventName::PermissionDenied
                | HookEventName::UserPromptSubmit
                | HookEventName::Notification
                | HookEventName::SubagentStart
                | HookEventName::SubagentStop
                | HookEventName::SubagentEnd
                | HookEventName::PreCompact
                | HookEventName::PostCompact => 16,
            }
        };
        assert_eq!(
            cases.len() + 1,
            total_variants(HookEventName::SessionStart),
            "update hub_hook_kind test when new HookEventName variants are added"
        );

        for (event, expected) in cases {
            let kind = hub_hook_kind(*event);
            assert_eq!(
                kind.as_deref(),
                Some(*expected),
                "hub_hook_kind wrong for {event:?}"
            );
        }
    }

    fn post_tool_use_envelope(tool_name: &str) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::PostToolUse,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::PostToolUse {
                tool_name: tool_name.into(),
                tool_use_id: "tu-1".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                tool_result: serde_json::json!({"type": "Bash"}),
                tool_input_truncated: false,
                tool_result_truncated: false,
                duration_ms: None,
                is_backgrounded: false,
                subagent_type: None,
            },
        }
    }

    fn post_tool_use_spec(name: &str, script: &str) -> HookSpec {
        let mut spec = make_command_spec(name, None, true, script);
        spec.event = HookEventName::PostToolUse;
        spec
    }

    #[tokio::test]
    async fn post_tool_use_wrong_kind_write_cannot_displace_correct_kind() {
        let builtin = post_tool_use_spec(
            "builtin",
            r#"echo '{"hookSpecificOutput":{"updatedToolOutput":{"type":"Bash","output":[],"exit_code":0,"command":"correct","truncated":false}}}'"#,
        );
        let mcp = post_tool_use_spec(
            "mcp",
            r#"echo '{"hookSpecificOutput":{"updatedMCPToolOutput":"wrong-kind"}}'"#,
        );
        let registry = registry_from_specs(vec![builtin, mcp]);
        let result = dispatch_post_tool_use(
            &registry,
            &post_tool_use_envelope("run_terminal_command"),
            &run_ctx(),
        )
        .await;

        let builtin = result
            .builtin_replacement
            .as_ref()
            .expect("the built-in replacement survives the later wrong-kind write");
        assert_eq!(builtin.replacement.hook_name, "builtin");
        assert_eq!(builtin.run_index, 0);
        let mcp = result.mcp_replacement.as_ref().expect("the MCP slot");
        assert_eq!(mcp.replacement.hook_name, "mcp");
        assert_eq!(mcp.run_index, 1);
    }

    #[tokio::test]
    async fn post_tool_use_same_kind_write_takes_the_last_writer() {
        let first = post_tool_use_spec(
            "first",
            r#"echo '{"hookSpecificOutput":{"updatedToolOutput":{"type":"Bash","output":[],"exit_code":0,"command":"first","truncated":false}}}'"#,
        );
        let second = post_tool_use_spec(
            "second",
            r#"echo '{"hookSpecificOutput":{"updatedToolOutput":{"type":"Bash","output":[],"exit_code":0,"command":"second","truncated":false}}}'"#,
        );
        let registry = registry_from_specs(vec![first, second]);
        let result = dispatch_post_tool_use(
            &registry,
            &post_tool_use_envelope("run_terminal_command"),
            &run_ctx(),
        )
        .await;

        let selected = result
            .builtin_replacement
            .as_ref()
            .expect("the later same-kind write wins the built-in slot");
        assert_eq!(
            selected
                .replacement
                .value
                .get("command")
                .and_then(|v| v.as_str()),
            Some("second")
        );
        assert_eq!(selected.run_index, 1);
        assert!(result.mcp_replacement.is_none());
    }

    #[test]
    fn merge_appends_results_blocks_and_context() {
        let success = |name: &str| HookRunResult::Success {
            hook_name: name.to_string(),
            elapsed: std::time::Duration::ZERO,
            http_info: None,
            system_message: None,
        };
        let block = |name: &str| PostToolUseBlock {
            hook_name: name.to_string(),
            reason: format!("{name}-reason"),
        };
        let context = |name: &str| AdditionalContext {
            hook_name: name.to_string(),
            text: format!("{name}-text"),
        };
        let mut base = PostToolUseResult {
            results: vec![success("a")],
            blocks: vec![block("a")],
            additional_context: vec![context("a")],
            ..Default::default()
        };
        base.merge(PostToolUseResult {
            results: vec![success("b"), success("c")],
            blocks: vec![block("b")],
            additional_context: vec![context("b"), context("c")],
            ..Default::default()
        });

        let result_names: Vec<&str> = base
            .results
            .iter()
            .map(|r| match r {
                HookRunResult::Success { hook_name, .. } => hook_name.as_str(),
                other => panic!("unexpected result variant: {other:?}"),
            })
            .collect();
        assert_eq!(result_names, ["a", "b", "c"]);
        let block_names: Vec<&str> = base.blocks.iter().map(|b| b.hook_name.as_str()).collect();
        assert_eq!(block_names, ["a", "b"]);
        let context_names: Vec<&str> = base
            .additional_context
            .iter()
            .map(|c| c.hook_name.as_str())
            .collect();
        assert_eq!(context_names, ["a", "b", "c"]);
        assert!(base.builtin_replacement.is_none());
        assert!(base.mcp_replacement.is_none());
    }

    #[test]
    fn disabled_hooks_file_cannot_skip_managed_policy_hook() {
        let disabled = crate::trust::DisabledHooks::from_names([
            "requirements/system:pre_tool_use[0].hooks[0]".to_string(),
            "global/user-hook".to_string(),
        ]);

        let mut results = Vec::new();
        let mut managed = make_command_spec(
            "requirements/system:pre_tool_use[0].hooks[0]",
            None,
            true,
            "echo ok",
        );
        managed.layer = crate::config::HookProvenance::Requirements;
        assert!(
            eligible_or_record_skip(&managed, None, &mut results, &disabled),
            "managed-policy hook must remain eligible despite a disabled-hooks entry"
        );
        assert!(results.is_empty());

        let user = make_command_spec("global/user-hook", None, true, "echo ok");
        assert!(
            !eligible_or_record_skip(&user, None, &mut results, &disabled),
            "a user hook with the same disabled-hooks treatment must be filtered"
        );
        assert!(matches!(
            results
                .first()
                .unwrap_or_else(|| panic!("expected results item 0: {results:?}")),
            HookRunResult::Skipped { .. }
        ));

        assert!(
            !crate::trust::hook_disabled_for_display_with(&managed, &disabled),
            "managed-policy hooks must never display as disabled"
        );
        assert!(crate::trust::hook_disabled_for_display_with(
            &user, &disabled
        ));
    }

    #[tokio::test]
    async fn managed_policy_hook_runs_via_dispatch_even_when_flagged_disabled() {
        let mut spec = make_command_spec(
            "requirements/system:pre_tool_use[0].hooks[0]",
            None,
            false,
            "echo '{\"decision\":\"deny\",\"reason\":\"managed policy\"}'; exit 2",
        );
        spec.layer = crate::config::HookProvenance::Requirements;
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "managed policy"),
            ref other => panic!("managed hook must have run and denied, got {other:?}"),
        }
    }

    fn ran(results: &[HookRunResult]) -> usize {
        results
            .iter()
            .filter(|r| !matches!(r, HookRunResult::Skipped { .. }))
            .count()
    }

    /// The announced count matches the dispatch on every eligibility axis: matcher, enabled, disabled snapshot, managed exemption.
    #[tokio::test]
    async fn runnable_count_matches_pre_tool_use_dispatch() {
        let mut managed = make_command_spec(
            "requirements/system:pre_tool_use[0].hooks[0]",
            None,
            false,
            "true",
        );
        managed.layer = crate::config::HookProvenance::Requirements;
        let registry = registry_from_specs(vec![
            make_command_spec("a", Some("read_file"), true, "true"),
            make_command_spec("b", None, true, "true"),
            make_command_spec("c", Some("write"), true, "true"),
            make_command_spec("d", Some("read_file"), false, "true"),
            make_command_spec("e", None, true, "true"),
            managed,
        ]);
        let ctx = RunContext {
            disabled: std::sync::Arc::new(DisabledHooks::from_names(["e".to_string()])),
            ..run_ctx()
        };
        // read_file: a, b, managed (flagged disabled but exempt); d is disabled, e is in the snapshot, c misses the matcher
        for (tool, expected) in [("read_file", 3), ("grep", 2)] {
            let envelope = pre_tool_use_envelope(tool);
            let count = runnable_count(&registry, &envelope, &ctx);
            assert_eq!(count, expected, "{tool}");
            let result = dispatch_pre_tool_use(&registry, &envelope, &ctx).await;
            assert_eq!(count, ran(&result.results), "{tool}");
        }
        assert_eq!(runnable_count(&registry, &stop_envelope(), &ctx), 0);
    }

    /// Non-tool events match on their own payload field, so a count that ignored the payload would over-announce.
    #[tokio::test]
    async fn runnable_count_matches_dispatch_for_a_non_tool_matcher() {
        let mut spec = make_command_spec("manual-only", Some("manual"), true, "true");
        spec.event = HookEventName::PreCompact;
        let registry = registry_from_specs(vec![spec]);
        for (source, expected) in [("auto", 0), ("manual", 1)] {
            let mut envelope = session_start_envelope();
            envelope.hook_event_name = HookEventName::PreCompact;
            envelope.payload = HookPayload::PreCompact {
                source: source.into(),
            };
            let count = runnable_count(&registry, &envelope, &run_ctx());
            assert_eq!(count, expected, "{source}");
            let results =
                dispatch_non_blocking(&registry, HookEventName::PreCompact, &envelope, &run_ctx())
                    .await;
            assert_eq!(count, ran(&results), "{source}");
        }
    }
}
