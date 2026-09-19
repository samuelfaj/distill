// Modified for Distill by Samuel Fajreldines, 2026.
//! # distill-hooks
//!
//! Runtime hook system for Grok: file-based discovery, command execution, and policy enforcement.
//!
//! ## Overview
//!
//! This crate provides a minimal hooks system for Grok. Hooks are discovered
//! from dedicated directories (`~/.grok/hooks/` and `<git-worktree-root>/.grok/hooks/`),
//! defined in JSON files (compatible settings format), and executed as child processes.
//!
//! ## Scope
//!
//! - Event types: `session_start`, `pre_tool_use`, `post_tool_use`, `user_prompt_submit`, `stop`/`subagent_stop`, `notification`, `session_end`
//! - Both command-backed and HTTP hooks
//! - `pre_tool_use` hooks can allow/ask/deny and rewrite tool input via `updatedInput`
//! - Prompt and stop gates can block
//! - Fail-open by default: a hook error never blocks
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use std::path::Path;
//! use distill_hooks::discovery::load_hooks;
//! use distill_hooks::event::HookEventName;
//!
//! let (registry, errors) = load_hooks(
//!     Some(Path::new("/home/user/.grok/hooks")),
//!     Some(Path::new("/project/.grok/hooks")),
//! );
//!
//! for err in &errors {
//!     eprintln!("hook load warning: {err}");
//! }
//!
//! let pre_hooks = registry.hooks_for(HookEventName::PreToolUse);
//! println!("loaded {} pre_tool_use hooks", pre_hooks.len());
//! ```

#![deny(clippy::indexing_slicing)]

pub mod config;
pub mod discovery;
pub mod dispatcher;
mod env_expand;
pub mod error;
pub mod event;
pub mod matcher;
pub mod result;
pub mod runner;
#[cfg(test)]
mod test_support;
pub mod trust;
