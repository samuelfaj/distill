// Modified for Distill by Samuel Fajreldines, 2026.
//! Extracts the pure-logic "resolution" phase of subagent spawning from `distill-shell` into a reusable library.
//! Given a spawn request and a resolution context (roles, personas, parent state), this crate resolves:
//!
//! - Effective runtime config (model, persona, capability mode, isolation) via precedence: explicit override > role > persona > parent.
//! - Persona instruction loading (inline `instructions` and `instructions_file`).
//! - Role prompt file loading.
//! - Resume identity validation (type/persona match checks; model selection is
//!   left to the host's source-pin/explicit-override policy).
//!
//! This crate has no dependency on session, coordinator, or transport types.
//! It serves local hosts (e.g. `distill-shell`) and any future remote spawn path that needs only pure resolution logic.
//!
//! Definition discovery, gating, prompt context, runtime defaults, and capability/depth tool policy are shared here.
//! Choosing the model from the catalog and creating the child workspace stay in the host adapters.

#![deny(clippy::indexing_slicing)]

pub mod config;
pub mod context;
pub mod definition;
pub mod overrides;
pub mod resume;
pub mod types;

pub use config::{PersonaIOField, SubagentPersona, SubagentRole};
pub use definition::{
    DefinitionResolutionContext, DefinitionValidationContext, HarnessToolsetContext,
    apply_child_tool_policy, apply_definition_runtime_defaults, apply_harness_toolset,
    available_agent_names, discover_agent_definition, gate_agent_definition,
    render_subagent_initial_user_message, render_subagent_system_prompt, resolve_agent_definition,
    resolve_child_depth, resolve_runtime_config, select_role,
    subagent_harness_flavor_is_representable,
    validate_agent_name,
};
pub use overrides::{intersect_capability_modes, resolve_effective_overrides};
pub use resume::{ResumeValidationError, validate_resume_identity};
pub use types::{ContextSource, EffectiveRuntimeConfig, ResolutionError, ResumeSourceData};
pub use distill_agent::config::AgentDefinition;
pub use distill_agent::prompt::paths::PathsConfig;
