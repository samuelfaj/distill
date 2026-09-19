use super::*;

fn candidate<'a>(
    stored_address: Option<&'a str>,
    session_id: &'a str,
    owner_match: bool,
    generation_current: bool,
    workflow: bool,
    presence: AddressPresence,
) -> AddressCandidate<'a> {
    AddressCandidate {
        stored_address,
        session_id,
        owner_match,
        generation_current,
        workflow,
        presence,
    }
}

#[test]
fn mint_displays_and_parses_as_opaque_string() {
    let address = AgentAddress::mint(0xabc);
    assert_eq!(address.as_str(), "aa1.abc");
    assert_eq!(address.to_string(), address.as_str());
    assert_eq!(
        AgentAddress::parse(address.as_str()).as_ref(),
        Some(&address)
    );
}

#[test]
fn parse_rejects_empty_and_sentinels() {
    assert!(AgentAddress::parse("").is_none());
    assert!(AgentAddress::parse("   ").is_none());
    assert!(AgentAddress::parse("null").is_none());
    assert!(AgentAddress::parse("None").is_none());
    assert!(AgentAddress::parse("undefined").is_none());
    assert_eq!(
        AgentAddress::parse("  aa1.1.2.3  ")
            .as_ref()
            .map(AgentAddress::as_str),
        Some("aa1.1.2.3")
    );
}

#[test]
fn mint_child_address_skips_workflow() {
    assert!(mint_child_address(true, 1).is_none());
    assert_eq!(
        mint_child_address(false, 1)
            .as_ref()
            .map(AgentAddress::as_str),
        Some("aa1.1")
    );
}

#[test]
fn advertised_address_is_live_non_workflow_only() {
    assert_eq!(
        advertised_address(Some("aa1.1.2.3"), false),
        Some("aa1.1.2.3")
    );
    assert_eq!(advertised_address(Some("aa1.1.2.3"), true), None);
    assert_eq!(advertised_address(None, false), None);
    assert_eq!(advertised_address(Some("null"), false), None);
}

#[test]
fn resolve_owned_active_and_pending() {
    let owned = candidate(
        Some("aa1.1"),
        "child",
        true,
        true,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&owned)),
        AddressDecision::Owned {
            presence: AddressPresence::Active
        }
    );
    let pending = candidate(
        Some("aa1.1"),
        "child",
        true,
        true,
        false,
        AddressPresence::Pending,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&pending)),
        AddressDecision::Owned {
            presence: AddressPresence::Pending
        }
    );
}

#[test]
fn resolve_raw_session_id_foreign_stale_and_workflow() {
    let raw = candidate(
        Some("aa1.1"),
        "child-session",
        true,
        true,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("child-session", Some(&raw)),
        AddressDecision::RawSessionId
    );

    let foreign = candidate(
        Some("aa1.1"),
        "child",
        false,
        true,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&foreign)),
        AddressDecision::Foreign
    );

    let stale_gen = candidate(
        Some("aa1.1"),
        "child",
        true,
        false,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&stale_gen)),
        AddressDecision::Stale
    );
    let gone = candidate(
        Some("aa1.1"),
        "child",
        true,
        true,
        false,
        AddressPresence::Gone,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&gone)),
        AddressDecision::Stale
    );

    let workflow = candidate(
        Some("aa1.1"),
        "child",
        true,
        true,
        true,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&workflow)),
        AddressDecision::WorkflowNotAddressable
    );
}

#[test]
fn resolve_unresolved_without_candidate_or_address_match() {
    assert_eq!(resolve_address("aa1.1", None), AddressDecision::Unresolved);
    let other = candidate(
        Some("aa1.9"),
        "child",
        true,
        true,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("aa1.1", Some(&other)),
        AddressDecision::Unresolved
    );
}

#[test]
fn address_match_wins_over_session_id_collision() {
    let collided = candidate(
        Some("same"),
        "same",
        true,
        true,
        false,
        AddressPresence::Active,
    );
    assert_eq!(
        resolve_address("same", Some(&collided)),
        AddressDecision::Owned {
            presence: AddressPresence::Active
        }
    );
}
