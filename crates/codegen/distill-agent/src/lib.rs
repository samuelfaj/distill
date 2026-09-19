// Modified for Distill by Samuel Fajreldines, 2026.
//! Agent builder, definition parsing, and system prompt assembly.
//!
//! An `Agent` bundles tools, system prompt, system-reminder policy, compaction policy, and model configuration into one object any host can consume.

#![deny(clippy::indexing_slicing)]

pub mod agent;
pub mod builder;
pub mod compaction;
pub mod config;
pub mod discovery;
pub mod error;
pub mod plugins;
pub mod prompt;
pub mod repo;
pub mod system_reminder;
pub mod timing;

pub use agent::Agent;
pub use builder::AgentBuilder;
pub use compaction::CompactionPolicy;
pub use config::AgentDefinition;
pub use config::api_backed_tool_ids;
pub use config::preset_names;
pub use config::toolset_for_preset;
pub use config::workspace_distill_toolset;
pub use error::AgentBuildError;
pub use prompt::context::{DEFAULT_SYSTEM_PROMPT_LABEL, PromptContext, RenderedPrompt};
pub use system_reminder::ReminderPolicy;
