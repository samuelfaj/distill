// Modified for Distill by Samuel Fajreldines, 2026.
//! Which tool ids a session's MCP servers may advertise.
//!
//! Pure planning, kept apart from the bridge lifecycle in `mcp.rs` so the
//! rules are testable without a hub or live transports. Invariants:
//!
//! - a native tool id is never claimed by any MCP server;
//! - a first-party server (an endpoint the bind config designated with
//!   [`BindMcpConfig::with_first_party_servers`]) outranks every third-party
//!   server for an id, exactly as natives outrank MCP — a user-configured
//!   server can therefore never remove an app-defined tool by offering the
//!   same name;
//! - within one tier an id offered by two servers is ambiguous and nobody
//!   claims it, rather than letting start order decide;
//! - the per-session cap is spent first-party first, so a third-party
//!   server's tool count can never starve the app's tools.
//!
//! The first-party rule is the one way an id can change owner while staying
//! claimed: a first-party server that starts after a third-party sibling
//! already advertised the id takes it over. The hub reconciler
//! (`mcp::reconcile_session_tools`) therefore diffs owners, not just the
//! owned set, because the hub's same-life re-register is a no-op that would
//! otherwise keep the sibling's handler.
//!
//! [`BindMcpConfig::with_first_party_servers`]: crate::config::BindMcpConfig::with_first_party_servers

use std::collections::{HashMap, HashSet};

use distill_tool_protocol::ToolId;

/// Where a server's tools rank when ids collide. Ordered: a lower tier claims
/// first and wins collisions with a higher one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum McpServerTier {
    /// A local app endpoint named in the bind config's first-party set. Set
    /// only from that config, never inferred from the server's own
    /// behaviour, so a user-configured server cannot promote itself.
    FirstParty,
    /// Every other server, including every user-configured one.
    ThirdParty,
}

/// One server's offer: its configured name, tier, and the ids it exposes in
/// the server's own order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimOffer {
    pub(crate) name: String,
    pub(crate) tier: McpServerTier,
    pub(crate) tool_ids: Vec<ToolId>,
}

/// Ids each server may advertise, first-party first then name order, plus the counts the caller logs.
/// Every offered server appears in `claims`, empty if it kept nothing, so a caller can reset each server's recorded ownership.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClaimPlan {
    pub(crate) claims: Vec<(String, Vec<ToolId>)>,
    /// Ids refused for a collision (native, or ambiguous within the tier, or
    /// a third-party id a first-party server also offers).
    pub(crate) rejected: usize,
    /// Ids refused only because the session's cap was already spent.
    pub(crate) over_cap: usize,
}

pub(crate) fn plan_claims(
    mut offers: Vec<ClaimOffer>,
    native: &HashSet<ToolId>,
    cap: usize,
) -> ClaimPlan {
    offers.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| left.name.cmp(&right.name))
    });

    let mut first_party_offers: HashMap<&ToolId, usize> = HashMap::new();
    let mut third_party_offers: HashMap<&ToolId, usize> = HashMap::new();
    for offer in &offers {
        let counts = match offer.tier {
            McpServerTier::FirstParty => &mut first_party_offers,
            McpServerTier::ThirdParty => &mut third_party_offers,
        };
        for tool_id in &offer.tool_ids {
            *counts.entry(tool_id).or_default() += 1;
        }
    }
    let is_claimable = |tier: McpServerTier, tool_id: &ToolId| {
        if native.contains(tool_id) {
            return false;
        }
        match tier {
            McpServerTier::FirstParty => first_party_offers.get(tool_id) == Some(&1),
            McpServerTier::ThirdParty => {
                !first_party_offers.contains_key(tool_id)
                    && third_party_offers.get(tool_id) == Some(&1)
            }
        }
    };

    let mut plan = ClaimPlan::default();
    let mut advertised = 0usize;
    for offer in &offers {
        let mut claimed = Vec::new();
        for tool_id in &offer.tool_ids {
            if !is_claimable(offer.tier, tool_id) {
                plan.rejected += 1;
                continue;
            }
            if advertised >= cap {
                plan.over_cap += 1;
                continue;
            }
            advertised += 1;
            claimed.push(tool_id.clone());
        }
        plan.claims.push((offer.name.clone(), claimed));
    }
    plan
}

#[cfg(test)]
#[path = "mcp_claim_tests.rs"]
mod tests;
