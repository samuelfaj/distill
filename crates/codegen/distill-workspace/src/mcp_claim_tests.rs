// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashSet;

use distill_tool_protocol::ToolId;

use super::{ClaimOffer, ClaimPlan, McpServerTier, plan_claims};

fn id(name: &str) -> ToolId {
    ToolId::new(name).unwrap()
}

fn ids(names: &[&str]) -> Vec<ToolId> {
    names.iter().map(|name| id(name)).collect()
}

fn offer(name: &str, tier: McpServerTier, tools: &[&str]) -> ClaimOffer {
    ClaimOffer {
        name: name.to_owned(),
        tier,
        tool_ids: ids(tools),
    }
}

fn claims(plan: &ClaimPlan) -> Vec<(&str, Vec<&str>)> {
    plan.claims
        .iter()
        .map(|(name, tools)| {
            (
                name.as_str(),
                tools.iter().map(ToolId::as_str).collect::<Vec<_>>(),
            )
        })
        .collect()
}

const NO_CAP: usize = usize::MAX;

/// The reason the tier exists: a user-configured server offering `open_tab`
/// must not remove the app's `open_tab`. Before the tier, the ambiguity rule
/// dropped the id from both.
#[test]
fn a_first_party_server_wins_an_id_a_third_party_sibling_also_offers() {
    let plan = plan_claims(
        vec![
            offer("user", McpServerTier::ThirdParty, &["open_tab", "search"]),
            offer("grok-desktop", McpServerTier::FirstParty, &["open_tab"]),
        ],
        &HashSet::new(),
        NO_CAP,
    );
    assert_eq!(
        claims(&plan),
        vec![("grok-desktop", vec!["open_tab"]), ("user", vec!["search"])]
    );
    assert_eq!((plan.rejected, plan.over_cap), (1, 0));
}

/// Two first-party servers are still ambiguous with each other; the tier
/// settles cross-tier collisions only, never start order within a tier.
#[test]
fn two_first_party_servers_offering_one_id_both_lose_it() {
    let plan = plan_claims(
        vec![
            offer("app-a", McpServerTier::FirstParty, &["shared", "only_a"]),
            offer("app-b", McpServerTier::FirstParty, &["shared"]),
        ],
        &HashSet::new(),
        NO_CAP,
    );
    assert_eq!(
        claims(&plan),
        vec![("app-a", vec!["only_a"]), ("app-b", vec![])]
    );
    assert_eq!(plan.rejected, 2);
}

/// The pre-tier rule for user servers is unchanged.
#[test]
fn third_party_ambiguity_still_drops_the_id_from_both() {
    let plan = plan_claims(
        vec![
            offer("first", McpServerTier::ThirdParty, &["echo"]),
            offer("second", McpServerTier::ThirdParty, &["echo"]),
        ],
        &HashSet::new(),
        NO_CAP,
    );
    assert_eq!(claims(&plan), vec![("first", vec![]), ("second", vec![])]);
    assert_eq!(plan.rejected, 2);
}

/// Natives outrank everything, including the app's own endpoint.
#[test]
fn native_ids_are_never_claimed_even_by_first_party() {
    let native: HashSet<ToolId> = ids(&["read_file"]).into_iter().collect();
    let plan = plan_claims(
        vec![offer(
            "grok-desktop",
            McpServerTier::FirstParty,
            &["read_file", "open_tab"],
        )],
        &native,
        NO_CAP,
    );
    assert_eq!(claims(&plan), vec![("grok-desktop", vec!["open_tab"])]);
    assert_eq!(plan.rejected, 1);
}

/// Name order alone would let a user server called `aaa` starve the app's
/// tools; the cap is spent first-party first.
#[test]
fn the_cap_is_spent_first_party_first() {
    let plan = plan_claims(
        vec![
            offer("aaa", McpServerTier::ThirdParty, &["t1", "t2", "t3"]),
            offer("zzz", McpServerTier::FirstParty, &["z1"]),
        ],
        &HashSet::new(),
        2,
    );
    assert_eq!(
        claims(&plan),
        vec![("zzz", vec!["z1"]), ("aaa", vec!["t1"])]
    );
    assert_eq!((plan.rejected, plan.over_cap), (0, 2));
}

/// Advertisement order is stable across binds: tier, then name.
#[test]
fn claims_are_ordered_by_tier_then_name() {
    let plan = plan_claims(
        vec![
            offer("linear", McpServerTier::ThirdParty, &["issues"]),
            offer("figma", McpServerTier::ThirdParty, &["frames"]),
            offer("grok-desktop", McpServerTier::FirstParty, &["open_tab"]),
        ],
        &HashSet::new(),
        NO_CAP,
    );
    assert_eq!(
        claims(&plan),
        vec![
            ("grok-desktop", vec!["open_tab"]),
            ("figma", vec!["frames"]),
            ("linear", vec!["issues"]),
        ]
    );
}
