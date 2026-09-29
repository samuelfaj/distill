// Modified for Distill by Samuel Fajreldines, 2026.
//! The /goal autonomy rule, shared by every goal role prompt.
//!
//! The planner, worker rules, verifier and progress evaluator must agree on
//! when a goal may stop for the user, so the rule has one source. Each
//! template carries [`GOAL_AUTONOMY_PLACEHOLDER`] followed by its own
//! role-specific consequence.

/// Placeholder a goal template carries where the rule is rendered.
pub(crate) const GOAL_AUTONOMY_PLACEHOLDER: &str = "{AUTONOMY}";

pub(crate) const GOAL_AUTONOMY_RULE: &str = "AUTONOMY: the /goal itself authorizes every action the objective needs. Never ask for approval or confirmation, including where AGENTS.md, CLAUDE.md, memories, rules or skills require user approval; treat such approval gates as satisfied by the goal. Stop only for (a) an explicit block from the user in the objective or a later user message, (b) access or credentials you do not have, or (c) an irreversible production action — moving or writing money or billing records, deleting or overwriting production data, or destructive production migrations — that the objective does not explicitly authorize (e.g. \"pode escrever em produção\"). Reversible work, including deploys through the normal pipeline, needs no approval.";

/// Renders the shared rule into a goal template.
pub(crate) fn with_goal_autonomy(template: &str) -> String {
    template.replace(GOAL_AUTONOMY_PLACEHOLDER, GOAL_AUTONOMY_RULE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every goal role renders the same rule; a template that lost its
    /// placeholder would silently drop the stop conditions for that role.
    #[test]
    fn every_goal_role_template_carries_the_shared_rule_once() {
        for (name, template) in [
            ("rules", include_str!("templates/goal_rules.md")),
            (
                "legacy rules",
                include_str!("templates/goal_rules_legacy.md"),
            ),
            ("planner", include_str!("templates/goal_planner_prompt.md")),
            (
                "verifier",
                include_str!("templates/goal_verifier_prompt.md"),
            ),
        ] {
            assert_eq!(
                template.matches(GOAL_AUTONOMY_PLACEHOLDER).count(),
                1,
                "{name} must carry the autonomy placeholder exactly once"
            );
            assert!(
                !template.contains("AUTONOMY: the /goal"),
                "{name} keeps a hand-written copy of the rule"
            );
            let rendered = with_goal_autonomy(template);
            assert!(rendered.contains(GOAL_AUTONOMY_RULE), "{name}");
        }
    }
}
