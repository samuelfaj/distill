// Modified for Distill by Samuel Fajreldines, 2026.
#![deny(clippy::indexing_slicing)]

pub mod engine;
pub mod host;
pub mod journal;
pub mod meta;
pub mod run;
pub mod validate;

pub const MAX_WORKFLOW_NAME_LEN: usize = 64;
pub const MAX_WORKFLOW_DESCRIPTION_LEN: usize = 1_024;
pub const MAX_WORKFLOW_WHEN_TO_USE_LEN: usize = 2_048;
pub const MAX_WORKFLOW_PHASES: usize = 64;
pub const MAX_PHASE_TITLE_LEN: usize = 128;
pub const MAX_PHASE_DETAIL_LEN: usize = 1_024;
pub const MAX_PARALLEL: usize = 1_024;
pub const DEFAULT_AGENT_BUDGET: u64 = 128;
pub const MAX_AGENT_BUDGET: u64 = 1_024;
pub const MAX_HOST_CALLS: u64 = 10_000;

pub(crate) fn with_rhai_hint(msg: String) -> String {
    let hint = if msg.contains("Expression exceeds maximum complexity") {
        "a single expression nests too deep — usually one long chained `+` string \
         concatenation. Split it into multiple `+=` statements."
    } else if msg.contains("reserved keyword") {
        "Rhai reserves identifiers it doesn't use — `shared`, `sync`, `async`, `await`, \
         `spawn`, `go`, `thread`, `new`, `match`, `case`, `default`, `void`, `null`, \
         `nil`, `exit`, `static`, `var` — rename the variable (`shared` → `has_shared`). \
         To start an agent without waiting for it, the function is `spawn_agent(opts)`."
    } else if msg.contains("getter is not registered for type 'char'") {
        "indexing a string yields a `char`, so field access on it fails — you likely \
         indexed a string you expected to be an array (e.g. unparsed JSON in an agent \
         output). Check with `type_of(x)`; slice strings with `s.sub_string(start, len)`."
    } else {
        return msg;
    };
    format!("{msg}\nhint: {hint}")
}

pub use engine::{WorkflowClock, WorkflowRunParams, run_workflow};
pub use host::{
    AgentOpts, AgentResult, BudgetState, DEFAULT_SH_TIMEOUT_MS, HostError, MAX_SH_OUTPUT_BYTES,
    ShOpts, ShResult, TicketHandle, WaitTimeout, WorkflowHostRequest,
};
pub use journal::{Journal, JournalEntry, JournalError};
pub use meta::{MetaError, PhaseMeta, WorkflowMeta, extract_meta};
pub use run::{PauseKind, WorkflowOutcome};
pub use validate::{
    ValidationError, ValidationReport, validate_script, validate_script_with_agent_budget,
    validate_script_with_cancel,
};
