//! Jev (TypeSafe System One) decision layer for the harness.
//!
//! Scope and invariants come from `plan/plan.md`:
//! - §12.5 is the wire contract implemented by [`client`] and [`types`];
//! - §1.6/I-1 keeps every lever default OFF (see [`flags`]);
//! - §1.6/I-3 bounds Jev's authority to "≤ incumbent": it may block, escalate or
//!   allow only where the local heuristic would allow anyway;
//! - §1.3.1 holds the token-saving ladder (P1…P6) in [`ladder`];
//! - [`questions`] is the single reviewable catalog of questions and thresholds.
//!
//! Nothing here performs I/O at construction time, and the module never logs
//! request/response bodies or the credential.

pub mod catalog;
pub mod cheap;
pub mod client;
pub mod crushers;
pub mod error;
pub mod flags;
pub mod ladder;
pub mod permission;
pub mod policy;
pub mod provider;
pub mod questions;
pub mod reduce;
pub mod tasks;
pub mod types;

pub use client::{JevClient, JevClientConfig};
pub use error::{JevError, JevErrorKind};
pub use flags::{JevFlags, JevLever};
pub use policy::{DecisionRecord, DecisionSink, JevDecision, TracingSink};
pub use provider::{JevProvider, ReasoningShape};
pub use types::{Answer, JevAnswerSet, Question, QuestionId, Usage};

/// A configured Jev client plus the flags that allowed it to exist.
///
/// The only supported way to obtain one is [`JevRuntime::from_flags`] (or the
/// resolver-injecting variant), which returns `None` while the master switch is
/// off — that is what makes "flag OFF ⇒ zero connections" a property of the
/// type system rather than a promise.
pub struct JevRuntime {
    pub client: JevClient,
    pub flags: JevFlags,
}

impl JevRuntime {
    /// Builds a runtime only when the master switch is on.
    pub fn from_flags(flags: JevFlags, config: JevClientConfig) -> Result<Option<Self>, JevError> {
        if !flags.enabled {
            return Ok(None);
        }
        let client = JevClient::new(config)?;
        Ok(Some(Self { client, flags }))
    }

    /// Test seam: same gate, injected credential resolver (no env mutation).
    pub fn from_flags_with_resolver(
        flags: JevFlags,
        config: JevClientConfig,
        resolver: client::ApiKeyResolver,
    ) -> Result<Option<Self>, JevError> {
        if !flags.enabled {
            return Ok(None);
        }
        let client = JevClient::with_key_resolver(config, resolver)?;
        Ok(Some(Self { client, flags }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn flags_off_builds_no_client() {
        let runtime = JevRuntime::from_flags(JevFlags::default(), JevClientConfig::default())
            .expect("no I/O happens when disabled");
        assert!(
            runtime.is_none(),
            "master switch off must not produce a client"
        );
    }

    #[test]
    fn flags_on_builds_a_client_without_network_io() {
        let resolver: client::ApiKeyResolver = Arc::new(|_| None);
        let runtime = JevRuntime::from_flags_with_resolver(
            JevFlags::default().with_enabled(true),
            JevClientConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                ..JevClientConfig::default()
            },
            resolver,
        )
        .expect("client construction performs no I/O");
        let runtime = runtime.expect("enabled runtime exists");
        assert!(!runtime.client.credential_present());
        assert!(runtime.flags.enabled);
    }
}
