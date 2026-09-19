// Modified for Distill by Samuel Fajreldines, 2026.
//! Announcement tracking for MCP servers and skills: which of them were already announced via `<system-reminder>` messages.
//! The tracking keeps injections and resumed sessions from duplicating listings.

use distill_tools::implementations::search_tool::ServerFingerprint;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Persisted announcement tracking state.
/// It is restored on session resume so the fresh actor "remembers" what was already announced.
/// The existing delta/fingerprint comparison logic then handles changes (new/removed/updated servers or skills) without creating duplicates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnouncementState {
    /// Maps server_name to `McpServerFingerprint`.
    /// The hash values use FNV-1a (deterministic, portable).
    /// Persisted fingerprints therefore remain valid across Rust versions, build profiles, and CPU architectures.
    pub mcp_server_fingerprints: HashMap<String, McpServerFingerprint>,

    /// An entry is the skill's `dedup_key()`, which is the skill name.
    pub announced_skill_names: HashSet<String>,

    /// Persisted form of [`McpAnnounced::failed`]: only the reason class is stored.
    /// The config identity hash is deliberately NOT persisted.
    /// Restored episodes thus adopt the current config on first sighting instead of spuriously re-announcing after a resume.
    pub announced_failed_servers: HashMap<String, AnnouncedFailure>,
}

/// Reason class a failure episode was announced with.
/// Persisted; keep the variants add-only.
/// Unknown values deserialize as [`Self::Transport`] so newer state files still load in older binaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncedFailure {
    /// OAuth required; recovery needs user action.
    AuthRequired,
    /// Connection or handshake failure; retried automatically.
    #[default]
    #[serde(other)]
    Transport,
}

/// One announced failure episode (the value in [`McpAnnounced::failed`]).
/// Live-only; the persisted form keeps just the class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AnnouncedEpisode {
    pub(crate) class: AnnouncedFailure,
    /// Identity hash of the server config the episode was announced under (transport, url/command/args, header and env names, never values).
    /// An in-place config edit (same name) starts a new episode.
    /// `None` after a restore: the episode adopts the current config on first sighting instead of spuriously re-announcing.
    pub(crate) config_identity: Option<u64>,
}

/// One currently-failed server as gathered from `McpState`, input to [`McpAnnounced::note_failures`].
/// It carries failure facts only; the model-facing reason line is rendered (and sanitized) at the injection site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedServer {
    pub(crate) name: String,
    /// Raw failure detail; `None` falls back to a generic reason.
    pub(crate) detail: Option<String>,
    pub(crate) class: AnnouncedFailure,
    /// A tool call re-handshakes this server (non-auth HTTP/SSE).
    pub(crate) retries_on_use: bool,
    /// Config identity hash (see [`AnnouncedEpisode::config_identity`]).
    pub(crate) config_identity: u64,
}

/// In-memory MCP announcement tracking, the live counterpart of the MCP half of [`AnnouncementState`].
/// It is one value so connected fingerprints and failure episodes restore and persist together.
#[derive(Debug, Clone, Default)]
pub(crate) struct McpAnnounced {
    /// Connected servers already announced, keyed by name; the fingerprint detects tool/description changes that warrant a delta announcement.
    pub(crate) fingerprints: HashMap<String, ServerFingerprint>,
    /// The entry is removed, allowing a new announcement, only when the server connects or leaves the config.
    /// Background retries (and their reason flip-flops) therefore don't re-announce.
    /// One is escalation to [`AnnouncedFailure::AuthRequired`]: it needs user action and invalidates the announced "retries automatically" hint.
    pub(crate) failed: HashMap<String, AnnouncedEpisode>,
}

impl McpAnnounced {
    pub(crate) fn from_persisted(persisted: AnnouncementState) -> Self {
        Self {
            fingerprints: from_persisted_fingerprints(&persisted.mcp_server_fingerprints),
            failed: persisted
                .announced_failed_servers
                .into_iter()
                .map(|(name, class)| {
                    (
                        name,
                        AnnouncedEpisode {
                            class,
                            config_identity: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Persisted form of [`Self::failed`] (classes only; see [`AnnouncementState::announced_failed_servers`]).
    pub(crate) fn persisted_failed(&self) -> HashMap<String, AnnouncedFailure> {
        self.failed
            .iter()
            .map(|(name, ep)| (name.clone(), ep.class))
            .collect()
    }

    /// End every failure episode and let the next injection re-announce still-down servers (used when reminders were dropped from context).
    pub(crate) fn rearm_failed(&mut self) {
        self.failed.clear();
    }

    /// One reconcile pass of the failure-episode state machine (the rules live on [`Self::failed`]).
    /// A handshaking server is absent from `currently_failed` but present in `unconnected_configured`, so its episode survives the retry attempt.
    pub(crate) fn note_failures(
        &mut self,
        mut currently_failed: Vec<FailedServer>,
        unconnected_configured: &HashSet<String>,
    ) -> (Vec<FailedServer>, bool) {
        let before = self.failed.len();
        self.failed
            .retain(|name, _| unconnected_configured.contains(name));
        let recovered = self.failed.len() != before;
        // Restored episodes carry no config identity; adopt the current one rather than treating the restore itself as a config edit
        for f in &currently_failed {
            if let Some(ep) = self.failed.get_mut(&f.name)
                && ep.config_identity.is_none()
            {
                ep.config_identity = Some(f.config_identity);
            }
        }
        currently_failed.retain(|f| match self.failed.get(&f.name) {
            None => true,
            Some(ep) if ep.config_identity != Some(f.config_identity) => true,
            Some(ep) => match ep.class {
                AnnouncedFailure::Transport => f.class == AnnouncedFailure::AuthRequired,
                AnnouncedFailure::AuthRequired => false,
            },
        });
        for f in &currently_failed {
            self.failed.insert(
                f.name.clone(),
                AnnouncedEpisode {
                    class: f.class,
                    config_identity: Some(f.config_identity),
                },
            );
        }
        let changed = recovered || !currently_failed.is_empty();
        (currently_failed, changed)
    }
}

/// The serializable counterpart of the in-memory `ServerFingerprint` type alias `(usize, u64, u64)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerFingerprint {
    pub tool_count: usize,
    pub description_hash: u64,
    pub tool_names_hash: u64,
}

pub(crate) fn to_persisted_fingerprints(
    in_memory: &HashMap<String, ServerFingerprint>,
) -> HashMap<String, McpServerFingerprint> {
    in_memory
        .iter()
        .map(|(name, &(tc, dh, tnh))| {
            (
                name.clone(),
                McpServerFingerprint {
                    tool_count: tc,
                    description_hash: dh,
                    tool_names_hash: tnh,
                },
            )
        })
        .collect()
}

pub(crate) fn from_persisted_fingerprints(
    persisted: &HashMap<String, McpServerFingerprint>,
) -> HashMap<String, ServerFingerprint> {
    persisted
        .iter()
        .map(|(name, fp)| {
            (
                name.clone(),
                (fp.tool_count, fp.description_hash, fp.tool_names_hash),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(name: &str, class: AnnouncedFailure) -> FailedServer {
        FailedServer {
            name: name.to_string(),
            detail: None,
            class,
            retries_on_use: false,
            config_identity: 0,
        }
    }

    #[test]
    fn serde_round_trip() {
        let state = AnnouncementState {
            mcp_server_fingerprints: HashMap::from([(
                "github".to_string(),
                McpServerFingerprint {
                    tool_count: 5,
                    description_hash: 12345678,
                    tool_names_hash: 87654321,
                },
            )]),
            announced_skill_names: HashSet::from(["commit".to_string(), "review".to_string()]),
            announced_failed_servers: HashMap::from([(
                "sentry".to_string(),
                AnnouncedFailure::AuthRequired,
            )]),
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: AnnouncementState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.mcp_server_fingerprints.len(), 1);
        assert_eq!(loaded.announced_skill_names.len(), 2);
        assert_eq!(
            loaded.announced_failed_servers.get("sentry"),
            Some(&AnnouncedFailure::AuthRequired)
        );
        let Some(fp) = loaded.mcp_server_fingerprints.get("github") else {
            panic!(
                "expected github fingerprint: {:?}",
                loaded.mcp_server_fingerprints
            );
        };
        assert_eq!(fp.tool_count, 5);
        assert_eq!(fp.description_hash, 12345678);
        assert_eq!(fp.tool_names_hash, 87654321);
    }

    #[test]
    fn backward_compat_empty_json() {
        let loaded: AnnouncementState = serde_json::from_str("{}").unwrap();
        assert!(loaded.mcp_server_fingerprints.is_empty());
        assert!(loaded.announced_skill_names.is_empty());
        assert!(loaded.announced_failed_servers.is_empty());
    }

    #[test]
    fn unknown_failure_class_loads_as_transport() {
        let loaded: AnnouncementState =
            serde_json::from_str(r#"{"announced_failed_servers": {"srv": "some_future_class"}}"#)
                .unwrap();
        assert_eq!(
            loaded.announced_failed_servers.get("srv"),
            Some(&AnnouncedFailure::Transport)
        );
    }

    #[test]
    fn fingerprint_conversion_round_trip() {
        let in_memory: HashMap<String, ServerFingerprint> =
            HashMap::from([("srv".to_string(), (3, 111, 222))]);
        let persisted = to_persisted_fingerprints(&in_memory);
        let back = from_persisted_fingerprints(&persisted);
        assert_eq!(in_memory, back);
    }

    #[test]
    fn mcp_announced_from_persisted_restores_both_halves() {
        let state = AnnouncementState {
            mcp_server_fingerprints: to_persisted_fingerprints(&HashMap::from([(
                "srv".to_string(),
                (3, 111, 222),
            )])),
            announced_skill_names: HashSet::new(),
            announced_failed_servers: HashMap::from([(
                "dead".to_string(),
                AnnouncedFailure::Transport,
            )]),
        };
        let announced = McpAnnounced::from_persisted(state);
        assert_eq!(announced.fingerprints.get("srv"), Some(&(3, 111, 222)));
        assert_eq!(
            announced.failed.get("dead"),
            Some(&AnnouncedEpisode {
                class: AnnouncedFailure::Transport,
                config_identity: None,
            })
        );
    }

    #[test]
    fn note_failures_announces_once_per_episode() {
        let mut announced = McpAnnounced::default();
        let unconnected = HashSet::from(["dead".to_string()]);

        let (to_announce, changed) = announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );
        assert_eq!(to_announce.len(), 1);
        assert!(changed);

        // Same failure again (a background retry): silent, unchanged.
        let (to_announce, changed) = announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );
        assert!(to_announce.is_empty());
        assert!(!changed);
    }

    #[test]
    fn note_failures_escalates_to_auth_once_and_stays_sticky() {
        let mut announced = McpAnnounced::default();
        let unconnected = HashSet::from(["dead".to_string()]);
        announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );

        let (to_announce, changed) = announced.note_failures(
            vec![failed("dead", AnnouncedFailure::AuthRequired)],
            &unconnected,
        );
        assert_eq!(to_announce.len(), 1, "escalation announces once");
        assert!(changed);

        // Flips back and forth stay silent: once escalated to `AuthRequired` the episode stays there
        for class in [AnnouncedFailure::Transport, AnnouncedFailure::AuthRequired] {
            let (to_announce, changed) =
                announced.note_failures(vec![failed("dead", class)], &unconnected);
            assert!(to_announce.is_empty());
            assert!(!changed);
        }
    }

    #[test]
    fn restored_episode_adopts_current_config_without_reannouncing() {
        let unconnected = HashSet::from(["dead".to_string()]);
        let mut announced = McpAnnounced::from_persisted(AnnouncementState {
            announced_failed_servers: HashMap::from([(
                "dead".to_string(),
                AnnouncedFailure::Transport,
            )]),
            ..Default::default()
        });

        // First sighting after a resume: the restored episode adopts the current config instead of treating the restore as a config edit
        let (to_announce, changed) = announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );
        assert!(to_announce.is_empty(), "{to_announce:?}");
        assert!(!changed);

        // A real config edit after adoption still starts a new episode.
        let mut edited = failed("dead", AnnouncedFailure::Transport);
        edited.config_identity = 1;
        let (to_announce, _) = announced.note_failures(vec![edited], &unconnected);
        assert_eq!(to_announce.len(), 1);
    }

    #[test]
    fn note_failures_config_edit_starts_a_new_episode() {
        let mut announced = McpAnnounced::default();
        let unconnected = HashSet::from(["dead".to_string()]);
        announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );

        // Same name, changed config identity: the edited server failing is a fresh episode and announces again
        let mut edited = failed("dead", AnnouncedFailure::Transport);
        edited.config_identity = 1;
        let (to_announce, changed) = announced.note_failures(vec![edited.clone()], &unconnected);
        assert_eq!(to_announce.len(), 1, "config edit re-announces");
        assert!(changed);

        // The new episode then dedupes as usual.
        let (to_announce, changed) = announced.note_failures(vec![edited], &unconnected);
        assert!(to_announce.is_empty());
        assert!(!changed);
    }

    #[test]
    fn note_failures_rearms_on_recovery_and_keeps_handshaking_episodes() {
        let mut announced = McpAnnounced::default();
        let unconnected = HashSet::from(["dead".to_string()]);
        announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );

        // Mid-retry: absent from currently_failed but still configured and unconnected; the episode survives
        let (to_announce, changed) = announced.note_failures(vec![], &unconnected);
        assert!(to_announce.is_empty());
        assert!(!changed);
        assert!(announced.failed.contains_key("dead"));

        // Connected (or removed from config): the episode ends and a later failure announces again
        let (_, changed) = announced.note_failures(vec![], &HashSet::new());
        assert!(changed, "recovery removal must trigger a persist");
        let (to_announce, _) = announced.note_failures(
            vec![failed("dead", AnnouncedFailure::Transport)],
            &unconnected,
        );
        assert_eq!(to_announce.len(), 1, "new episode after recovery");
    }
}
