// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::ToolSpec;
use distill_tools::types::tool::ToolKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildToolProjection {
    Rebuilt,
    VerbatimMirror,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildMessagingGrant {
    Granted,
    Ungranted,
}

pub(super) fn child_safe_tool_specs(
    specs: Vec<ToolSpec>,
    projection: ChildToolProjection,
    messaging_grant: ChildMessagingGrant,
    kind_for_name: impl Fn(&str) -> Option<ToolKind>,
) -> Vec<ToolSpec> {
    // Unknown names are parent-only capabilities; granted messaging exempts only the active-message kind.
    match projection {
        ChildToolProjection::Rebuilt | ChildToolProjection::VerbatimMirror => specs
            .into_iter()
            .filter(|spec| match kind_for_name(&spec.name) {
                Some(ToolKind::ActiveAgentMessage) => {
                    messaging_grant == ChildMessagingGrant::Granted
                }
                Some(_) => true,
                None => false,
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "child_tool_projection_tests.rs"]
mod tests;
