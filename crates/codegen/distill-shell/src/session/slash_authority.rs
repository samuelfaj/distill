//! Untrusted model input may resolve an explicitly eligible canonical built-in or proceed to the child session's focused skill resolver.
//! Human-only command catalogs stay outside this boundary.

use agent_client_protocol as acp;

use super::slash_commands::{BuiltinAction, BuiltinCommand, ModelAuthoredEligibility};

/// Model-authored slash parse only. Human catalogs use `resolve_human_intent`;
/// inert slash never calls this.
#[derive(Debug)]
pub(super) enum AuthorityResolution<'a> {
    NotSlash,
    ModelAuthoredSkillCandidate {
        command_name: &'a str,
        args: &'a str,
    },
    StaticBuiltin(BuiltinAction),
}

pub(super) fn resolve<'a>(
    prompt_blocks: &'a [acp::ContentBlock],
    builtins: &[BuiltinCommand],
) -> AuthorityResolution<'a> {
    let Some((command_name, args)) = parse_slash_prefix(prompt_blocks) else {
        return AuthorityResolution::NotSlash;
    };

    builtins
        .iter()
        .find(|command| {
            command.model_authored_eligibility == ModelAuthoredEligibility::ExactCanonical
                && command.name == command_name
                && command.gate == super::slash_commands::BuiltinGate::AlwaysOn
        })
        .map_or(
            AuthorityResolution::ModelAuthoredSkillCandidate { command_name, args },
            |command| AuthorityResolution::StaticBuiltin((command.resolve)(args)),
        )
}

/// Extract `(name, args)` if the first text block starts with a slash-command token.
pub(super) fn parse_slash_prefix(prompt_blocks: &[acp::ContentBlock]) -> Option<(&str, &str)> {
    let text = prompt_blocks.iter().find_map(|block| match block {
        acp::ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    })?;
    let without_slash = text.trim().strip_prefix('/')?;
    let (name, args) = match without_slash.find(char::is_whitespace) {
        Some(index) => (
            without_slash.get(..index).unwrap_or(""),
            without_slash.get(index..).unwrap_or("").trim(),
        ),
        None => (without_slash, ""),
    };
    (!name.is_empty() && !name.contains('/') && !name.contains('\\')).then_some((name, args))
}

// Regression tests read these test-only counters around real `SessionActor::handle_turn_input` calls
// An authority-gate reorder therefore cannot silently reintroduce host catalog I/O
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DynamicResolutionCalls {
    pub(crate) skill_catalog: usize,
    pub(crate) command_availability: usize,
    pub(crate) workflow_discovery: usize,
}

#[cfg(test)]
std::thread_local! {
    static DYNAMIC_RESOLUTION_CALLS: std::cell::Cell<DynamicResolutionCalls> = const {
        std::cell::Cell::new(DynamicResolutionCalls {
            skill_catalog: 0,
            command_availability: 0,
            workflow_discovery: 0,
        })
    };
}

#[cfg(test)]
pub(crate) fn record_skill_catalog_call() {
    DYNAMIC_RESOLUTION_CALLS.set(DynamicResolutionCalls {
        skill_catalog: DYNAMIC_RESOLUTION_CALLS.get().skill_catalog + 1,
        ..DYNAMIC_RESOLUTION_CALLS.get()
    });
}

#[cfg(test)]
pub(crate) fn record_command_availability_call() {
    DYNAMIC_RESOLUTION_CALLS.set(DynamicResolutionCalls {
        command_availability: DYNAMIC_RESOLUTION_CALLS.get().command_availability + 1,
        ..DYNAMIC_RESOLUTION_CALLS.get()
    });
}

#[cfg(test)]
pub(crate) fn record_workflow_discovery_call() {
    DYNAMIC_RESOLUTION_CALLS.set(DynamicResolutionCalls {
        workflow_discovery: DYNAMIC_RESOLUTION_CALLS.get().workflow_discovery + 1,
        ..DYNAMIC_RESOLUTION_CALLS.get()
    });
}

#[cfg(test)]
pub(crate) fn dynamic_resolution_calls() -> DynamicResolutionCalls {
    DYNAMIC_RESOLUTION_CALLS.get()
}

#[cfg(test)]
#[path = "slash_authority_tests.rs"]
mod tests;
