//! Visibility policy for `session_kind`, shared by the local session list and search.

use std::collections::HashMap;

use crate::agent::session_registry_client::SessionRecord;
use crate::session::persistence::Summary;

pub const SESSION_KIND_HEADLESS: &str = "headless";

/// Listing/search policy for `session_kind=headless` rows.
/// Applied before truncation; headless remains distinct from `Summary::is_hidden()`.
/// The Rust default is the first-party picker policy; omitted wire values are handled separately by [`Self::from_wire`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeadlessPolicy {
    #[default]
    Exclude,
    Only,
    Include,
}

impl HeadlessPolicy {
    /// Missing values keep the wire behavior from before this policy existed: include everything.
    /// Unknown explicit values fail closed to [`Self::Exclude`].
    pub fn from_wire(value: Option<&str>) -> Self {
        match value {
            None | Some("include") => Self::Include,
            Some("exclude") => Self::Exclude,
            Some("only") => Self::Only,
            Some(_) => Self::Exclude,
        }
    }

    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Exclude => "exclude",
            Self::Only => "only",
            Self::Include => "include",
        }
    }

    pub const fn admits(self, is_headless: bool) -> bool {
        match self {
            Self::Exclude => !is_headless,
            Self::Only => is_headless,
            Self::Include => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassifiedSessionKind {
    Interactive,
    Headless,
    Unknown,
}

pub(crate) fn policy_admits(policy: HeadlessPolicy, kind: ClassifiedSessionKind) -> bool {
    match (policy, kind) {
        (HeadlessPolicy::Include, _)
        | (HeadlessPolicy::Exclude, ClassifiedSessionKind::Interactive)
        | (HeadlessPolicy::Only, ClassifiedSessionKind::Headless) => true,
        (HeadlessPolicy::Exclude | HeadlessPolicy::Only, ClassifiedSessionKind::Unknown)
        | (HeadlessPolicy::Exclude, ClassifiedSessionKind::Headless)
        | (HeadlessPolicy::Only, ClassifiedSessionKind::Interactive) => false,
    }
}

/// Filters local rows and the remote rows that share their session ids; returns whether a local row was removed.
/// Remote rows with no local twin have no known kind: Exclude and Include keep them, and Only drops them without reporting a removal.
pub(crate) fn retain_session_lanes(
    local: &mut Vec<Summary>,
    remote: &mut Vec<SessionRecord>,
    policy: HeadlessPolicy,
) -> bool {
    let local_kind_by_id: HashMap<&str, bool> = local
        .iter()
        .map(|summary| (summary.info.id.0.as_ref(), summary.is_headless()))
        .collect();
    remote.retain(|row| {
        local_kind_by_id
            .get(row.session_id.as_str())
            .map_or(policy != HeadlessPolicy::Only, |is_headless| {
                policy.admits(*is_headless)
            })
    });
    let local_before = local.len();
    local.retain(|summary| policy.admits(summary.is_headless()));
    local.len() < local_before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_is_excluded_from_classified_views_but_included_in_inventory() {
        assert!(!policy_admits(
            HeadlessPolicy::Exclude,
            ClassifiedSessionKind::Unknown,
        ));
        assert!(!policy_admits(
            HeadlessPolicy::Only,
            ClassifiedSessionKind::Unknown,
        ));
        assert!(policy_admits(
            HeadlessPolicy::Include,
            ClassifiedSessionKind::Unknown,
        ));
    }

    #[test]
    fn headless_policy_wire_preserves_omitted_include() {
        assert_eq!(HeadlessPolicy::from_wire(None), HeadlessPolicy::Include);
        assert_eq!(
            HeadlessPolicy::from_wire(Some("exclude")),
            HeadlessPolicy::Exclude,
        );
        assert_eq!(
            HeadlessPolicy::from_wire(Some("only")),
            HeadlessPolicy::Only,
        );
        assert_eq!(
            HeadlessPolicy::from_wire(Some("include")),
            HeadlessPolicy::Include,
        );
        assert_eq!(
            HeadlessPolicy::from_wire(Some("bogus")),
            HeadlessPolicy::Exclude,
        );
    }
}
