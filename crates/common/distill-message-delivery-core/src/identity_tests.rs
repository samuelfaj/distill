use super::*;

#[test]
fn mint_prefixes_display_and_parse() {
    let agent = AgentId::mint(0xabc);
    assert_eq!(agent.as_str(), "ag1.abc");
    assert_eq!(agent.to_string(), agent.as_str());
    assert_eq!(AgentId::parse(agent.as_str()).as_ref(), Some(&agent));
    assert_eq!(AgentId::parse("  ag1.abc  ").as_ref(), Some(&agent));

    let attempt = AttemptId::mint(0xdef);
    assert_eq!(attempt.as_str(), "at1.def");
    assert_eq!(attempt.to_string(), attempt.as_str());
    assert_eq!(AttemptId::parse(attempt.as_str()).as_ref(), Some(&attempt));
}

#[test]
fn parse_adopts_uuid_v7_agent_ids() {
    let uuid = "019f972a-7c1b-7d92-a896-4f08f91b6864";
    let agent = AgentId::parse(uuid).expect("valid UUIDv7");
    assert_eq!(agent.as_str(), uuid);
    assert_eq!(AgentId::from_uuid_v7(uuid).as_ref(), Some(&agent));
}

#[test]
fn parse_rejects_foreign_and_non_hex() {
    assert!(AgentId::parse("").is_none());
    assert!(AgentId::parse("ag1.").is_none());
    assert!(AgentId::parse("at1.abc").is_none());
    assert!(AgentId::parse("aa1.abc").is_none());
    assert!(AgentId::parse("session-1").is_none());
    assert!(AgentId::parse("ag1.ABC").is_none());
    assert!(AgentId::parse("ag1.12g").is_none());
    assert!(AgentId::parse("019f972a-7c1b-4d92-a896-4f08f91b6864").is_none());
    assert!(AgentId::parse("019f972a-7c1b-7d92-c896-4f08f91b6864").is_none());
    assert!(AgentId::parse("019F972A-7C1B-7D92-A896-4F08F91B6864").is_none());
    assert!(AttemptId::parse("ag1.abc").is_none());
    assert!(AttemptId::parse("at1.").is_none());
}

#[test]
fn start_when_none() {
    let action = next_identity(None, false, 0x11, 0x22);
    match action {
        IdentityAction::Start { agent, attempt } => {
            assert_eq!(agent.as_str(), "ag1.11");
            assert_eq!(attempt.as_str(), "at1.22");
        }
        IdentityAction::Resume { .. } | IdentityAction::Fork { .. } => {
            panic!("expected start");
        }
    }

    let forked_without_previous = next_identity(None, true, 0x11, 0x22);
    assert!(matches!(
        forked_without_previous,
        IdentityAction::Start { .. }
    ));
}

#[test]
fn resume_keeps_agent_and_changes_attempt() {
    let previous = AgentId::mint(0x11);
    let action = next_identity(Some(&previous), false, 0x99, 0x33);
    match action {
        IdentityAction::Resume { agent, attempt } => {
            assert_eq!(agent, previous);
            assert_eq!(attempt.as_str(), "at1.33");
        }
        IdentityAction::Start { .. } | IdentityAction::Fork { .. } => {
            panic!("expected resume");
        }
    }
}

#[test]
fn resume_keeps_uuid_agent_and_changes_attempt() {
    let previous = AgentId::parse("019f972a-7c1b-7d92-a896-4f08f91b6864").unwrap();
    let action = next_identity(Some(&previous), false, 0x99, 0x33);
    match action {
        IdentityAction::Resume { agent, attempt } => {
            assert_eq!(agent, previous);
            assert_eq!(attempt.as_str(), "at1.33");
        }
        IdentityAction::Start { .. } | IdentityAction::Fork { .. } => {
            panic!("expected resume");
        }
    }
}

#[test]
fn fork_changes_agent_and_attempt() {
    let previous = AgentId::mint(0x11);
    let action = next_identity(Some(&previous), true, 0x44, 0x55);
    match action {
        IdentityAction::Fork { agent, attempt } => {
            assert_eq!(agent.as_str(), "ag1.44");
            assert_eq!(attempt.as_str(), "at1.55");
        }
        IdentityAction::Start { .. } | IdentityAction::Resume { .. } => {
            panic!("expected fork");
        }
    }
}
