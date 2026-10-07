// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode concern for `SessionActor`: per-turn orchestration reminder while the session flag is on.
use super::*;

pub(super) const ULTRACODE_DIRECTIVE: &str = "Ultracode mode is ON. Treat this request as a dynamic workflow, independent of the effort level: 1) Plan briefly: split the task into independent parts and note which files each part owns. 2) Fan out: launch every ready part as parallel subagents in one message (spawn_subagent, background when long; use the workflow tool for large fan-outs), giving each a precise spec, acceptance criteria and the files it owns. Size the fan-out to the task: small tasks 1-3 agents, medium 3-8, large 8+. 3) Integrate: collect all results, review the actual diffs, resolve conflicts, and run the verification yourself. Do trivial single-step requests directly without subagents.";

impl SessionActor {
    /// Called once per user turn from `handle_prompt()`; pushes nothing while Ultracode is off.
    pub(super) fn inject_ultracode_reminder(&self) {
        if self.ultracode.load(std::sync::atomic::Ordering::Relaxed) {
            self.push_system_reminder_with_tag(ULTRACODE_DIRECTIVE, self.reminder_wrapper_tag());
        }
    }
}
