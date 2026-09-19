//! Opaque child address. A session id is not an address.

use std::fmt;

const ADDRESS_PREFIX: &str = "aa1.";

/// Server-minted capability for one child generation. Not a session id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentAddress(String);

impl AgentAddress {
    /// Host supplies entropy. Core does not keep a process-global counter.
    #[must_use]
    pub fn mint(entropy: u128) -> Self {
        Self(format!("{ADDRESS_PREFIX}{entropy:x}"))
    }

    /// Adopt a presented opaque token. Does not mint.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        if !is_presentable(token) {
            return None;
        }
        Some(Self(token.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentAddress {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AgentAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Mint only for a non-workflow child. Host supplies entropy.
#[must_use]
pub fn mint_child_address(workflow: bool, entropy: u128) -> Option<AgentAddress> {
    if workflow {
        None
    } else {
        Some(AgentAddress::mint(entropy))
    }
}

/// Live advertise string. Workflow children are not addressable.
#[must_use]
pub fn advertised_address<'a>(stored: Option<&'a str>, workflow: bool) -> Option<&'a str> {
    if workflow {
        None
    } else {
        stored.filter(|token| AgentAddress::parse(token).is_some())
    }
}

/// Where the host found the candidate. Core does not store actor state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressPresence {
    Active,
    Pending,
    Gone,
}

/// Facts for one presented token against one candidate the host already found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressCandidate<'a> {
    pub stored_address: Option<&'a str>,
    pub session_id: &'a str,
    pub owner_match: bool,
    pub generation_current: bool,
    pub workflow: bool,
    pub presence: AddressPresence,
}

/// Typed resolve result. The host maps this to a send outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressDecision {
    Owned { presence: AddressPresence },
    RawSessionId,
    Foreign,
    Stale,
    WorkflowNotAddressable,
    Unresolved,
}

/// Classify a presented token. Address match wins over a session-id collision.
#[must_use]
pub fn resolve_address(
    presented: &str,
    candidate: Option<&AddressCandidate<'_>>,
) -> AddressDecision {
    let Some(candidate) = candidate else {
        return AddressDecision::Unresolved;
    };
    let presented = presented.trim();
    let address_match = candidate
        .stored_address
        .is_some_and(|stored| stored == presented);
    if candidate.session_id == presented && !address_match {
        return AddressDecision::RawSessionId;
    }
    if !address_match {
        return AddressDecision::Unresolved;
    }
    if candidate.workflow {
        return AddressDecision::WorkflowNotAddressable;
    }
    if !candidate.owner_match {
        return AddressDecision::Foreign;
    }
    if !candidate.generation_current || candidate.presence == AddressPresence::Gone {
        return AddressDecision::Stale;
    }
    AddressDecision::Owned {
        presence: candidate.presence,
    }
}

fn is_presentable(token: &str) -> bool {
    !token.is_empty()
        && !token.eq_ignore_ascii_case("null")
        && !token.eq_ignore_ascii_case("none")
        && !token.eq_ignore_ascii_case("undefined")
}

#[cfg(test)]
#[path = "address_tests.rs"]
mod tests;
