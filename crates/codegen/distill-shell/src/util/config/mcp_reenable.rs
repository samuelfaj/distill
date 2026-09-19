// Modified for Distill by Samuel Fajreldines, 2026.
//! Whether a personally disabled MCP name should appear as a re-enableable stub in `/mcps`.
//!
//! A row shows only when a definition still exists (ignoring personal disable) and org policy would not block enable.
//! Orphans that only linger in `disabled_mcp_servers` stay hidden.
//!
//! Discovery is shared with session merge ([`crate::session::managed_mcp::discover_mcp_definitions_ignoring_disable`]).

use std::collections::{BTreeSet, HashMap, HashSet};

use agent_client_protocol as acp;
use distill_workspace::permission::resolution::{ManagedSettings, McpSubject, McpVerdict};

use crate::session::managed_mcp::{McpDiscoveryInputs, discover_mcp_definitions_ignoring_disable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisabledStubVerdict {
    /// Show a disabled row the user can Space-enable.
    Show,
    /// Already represented by a live catalog row (no extra stub).
    HideAlreadyInCatalog,
    /// No remaining definition (orphan in `disabled_mcp_servers` only).
    HideNoDefinition,
    /// Org allowlist/denylist would reject enable.
    HidePolicyBlocked,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct McpDefinitionIndex {
    entries: HashMap<String, (acp::McpServer, McpSubject)>,
}

impl McpDefinitionIndex {
    pub(crate) fn build(inputs: &McpDiscoveryInputs<'_>) -> Self {
        Self {
            entries: discover_mcp_definitions_ignoring_disable(inputs),
        }
    }

    pub(crate) fn verdict(
        &self,
        name: &str,
        in_catalog: bool,
        ms: &ManagedSettings,
    ) -> DisabledStubVerdict {
        if in_catalog {
            return DisabledStubVerdict::HideAlreadyInCatalog;
        }
        let Some((server, subject)) = self.entries.get(name) else {
            return DisabledStubVerdict::HideNoDefinition;
        };
        if let McpVerdict::Blocked(_) = ms.mcp_verdict(server, *subject) {
            return DisabledStubVerdict::HidePolicyBlocked;
        }
        DisabledStubVerdict::Show
    }

    /// Sorted stub names for a stable list order.
    pub(crate) fn reenableable_for_list(
        &self,
        disabled_names: &HashSet<String>,
        catalog_names: &HashSet<String>,
        ms: &ManagedSettings,
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for name in disabled_names {
            let verdict = self.verdict(name, catalog_names.contains(name), ms);
            if matches!(verdict, DisabledStubVerdict::Show) {
                out.insert(name.clone());
            } else {
                tracing::debug!(
                    server = %name,
                    ?verdict,
                    "disabled MCP omitted from reenableable stub list"
                );
            }
        }
        out
    }

    /// The discovered definitions with their policy subjects — the one discovery pass shared between
    /// stub filtering and the list's blocked-reason verdicts.
    pub(crate) fn definitions(&self) -> impl Iterator<Item = (&str, &acp::McpServer, McpSubject)> {
        self.entries
            .iter()
            .map(|(name, (server, subject))| (name.as_str(), server, *subject))
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: HashMap<String, acp::McpServer>) -> Self {
        use distill_workspace::permission::resolution::PolicySubjectOrigin;
        // Test entries default to foreign: every policy source binds.
        Self {
            entries: entries
                .into_iter()
                .map(|(name, server)| {
                    (
                        name,
                        (
                            server,
                            McpSubject {
                                origin: PolicySubjectOrigin::Foreign,
                                project_scoped: false,
                            },
                        ),
                    )
                })
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    #[cfg(test)]
    pub(crate) fn transport(&self, name: &str) -> Option<&acp::McpServer> {
        self.entries.get(name).map(|(server, _)| server)
    }
}

pub(crate) fn needs_definition_scan(
    disabled_names: &HashSet<String>,
    catalog_names: &HashSet<String>,
) -> bool {
    disabled_names.iter().any(|n| !catalog_names.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::managed_mcp::mcp_server_name;
    use distill_tools::types::compat::CompatConfig;
    use distill_workspace::permission::resolution::{
        AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
    };

    fn settings_with_policy(policy: McpServerPolicy) -> ManagedSettings {
        let mut ms = ManagedSettings::default();
        ms.mcp_allowlist = policy;
        ms
    }

    fn unrestricted() -> ManagedSettings {
        settings_with_policy(McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![],
            None,
        )))
    }

    fn deny_name(name: &str) -> ManagedSettings {
        settings_with_policy(McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Name {
                name: name.to_string(),
            }],
            None,
        )))
    }

    fn http(name: &str, url: &str) -> acp::McpServer {
        acp::McpServer::Http(acp::McpServerHttp::new(name, url.to_string()).headers(vec![]))
    }

    fn inputs<'a>(cwd: &'a std::path::Path, compat: &'a CompatConfig) -> McpDiscoveryInputs<'a> {
        McpDiscoveryInputs {
            cwd,
            plugin_registry: None,
            compat,
        }
    }

    /// Git repo with project `.grok/config.toml` so discovery is bounded to cwd.
    fn project_repo(toml: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        // Pin trust so ambient GROK_CLI_VERSION cannot drop project MCP names.
        crate::agent::folder_trust::record_for_test(tmp.path(), true);
        std::fs::create_dir_all(tmp.path().join(".grok")).unwrap();
        std::fs::write(tmp.path().join(".grok").join("config.toml"), toml).unwrap();
        tmp
    }

    /// Isolate HOME/GROK_HOME so ambient user MCP config cannot pad discovery.
    fn isolated_home() -> (
        tempfile::TempDir,
        distill_test_support::EnvGuard,
        distill_test_support::EnvGuard,
    ) {
        let home = tempfile::tempdir().unwrap();
        let distill_home = home.path().join(".grok");
        std::fs::create_dir_all(&distill_home).unwrap();
        std::fs::write(distill_home.join("config.toml"), "").unwrap();
        let home_guard = distill_test_support::EnvGuard::set("HOME", home.path());
        let grok_guard = distill_test_support::EnvGuard::set("GROK_HOME", &distill_home);
        (home, home_guard, grok_guard)
    }

    #[test]
    fn verdict_table() {
        let index = McpDefinitionIndex::from_entries(HashMap::from([
            ("local".into(), http("local", "https://example.com/local")),
            (
                "blocked".into(),
                http("blocked", "https://blocked.example.com/mcp"),
            ),
            ("ok".into(), http("ok", "https://ok.example.com/mcp")),
        ]));
        let unrestricted = unrestricted();
        let deny_blocked = deny_name("blocked");
        let cases: &[(&str, bool, &ManagedSettings, DisabledStubVerdict)] = &[
            (
                "local",
                true,
                &unrestricted,
                DisabledStubVerdict::HideAlreadyInCatalog,
            ),
            (
                "grok_com_notion",
                false,
                &unrestricted,
                DisabledStubVerdict::HideNoDefinition,
            ),
            ("local", false, &unrestricted, DisabledStubVerdict::Show),
            (
                "blocked",
                false,
                &deny_blocked,
                DisabledStubVerdict::HidePolicyBlocked,
            ),
            ("ok", false, &deny_blocked, DisabledStubVerdict::Show),
        ];
        for &(name, in_catalog, allowlist, expected) in cases {
            assert_eq!(
                index.verdict(name, in_catalog, allowlist),
                expected,
                "name={name} in_catalog={in_catalog}"
            );
        }
    }

    #[test]
    fn reenableable_for_list_filters_catalog_and_orphans() {
        let index = McpDefinitionIndex::from_entries(HashMap::from([
            ("keep".into(), http("keep", "https://example.com/keep")),
            (
                "already".into(),
                http("already", "https://example.com/already"),
            ),
        ]));
        let disabled = HashSet::from(["keep".into(), "already".into(), "orphan".into()]);
        let catalog = HashSet::from(["already".into()]);
        let stubs = index.reenableable_for_list(&disabled, &catalog, &unrestricted());
        assert_eq!(stubs, BTreeSet::from(["keep".into()]));
    }

    #[test]
    #[serial_test::serial]
    fn orphan_grok_com_name_is_not_a_definition() {
        let (_home, _hg, _gg) = isolated_home();
        let cwd = tempfile::tempdir().unwrap();
        let compat = CompatConfig::default();
        let index = McpDefinitionIndex::build(&inputs(cwd.path(), &compat));
        assert!(!index.contains("grok_com_slack"));
        let disabled = HashSet::from(["grok_com_slack".into(), "totally_orphan_xyz".into()]);
        let stubs = index.reenableable_for_list(&disabled, &HashSet::new(), &unrestricted());
        assert!(!stubs.contains("grok_com_slack"));
        assert!(!stubs.contains("totally_orphan_xyz"));
    }

    #[test]
    #[serial_test::serial]
    fn build_indexes_toml_enabled_false() {
        let (_home, _hg, _gg) = isolated_home();
        let repo = project_repo(
            r#"
[mcp_servers.reenable_disabled_local]
command = "true"
enabled = false
"#,
        );
        let compat = CompatConfig::default();
        let index = McpDefinitionIndex::build(&inputs(repo.path(), &compat));
        assert!(
            index.contains("reenable_disabled_local"),
            "enabled=false TOML definition must still be discoverable"
        );
        let server = index
            .transport("reenable_disabled_local")
            .expect("transport after force-enable");
        assert_eq!(mcp_server_name(server), "reenable_disabled_local");

        let disabled = HashSet::from(["reenable_disabled_local".into()]);
        let stubs = index.reenableable_for_list(&disabled, &HashSet::new(), &unrestricted());
        assert!(stubs.contains("reenable_disabled_local"));
    }

    #[test]
    #[serial_test::serial]
    fn discover_contains_merge_when_nothing_disabled() {
        let (_home, _hg, _gg) = isolated_home();

        let repo = project_repo(
            r#"
[mcp_servers.drift_check_server]
command = "echo"
args = ["ok"]
"#,
        );
        let mut compat = CompatConfig::default();
        compat.claude.mcps = false;
        compat.cursor.mcps = false;
        let discovered = discover_mcp_definitions_ignoring_disable(&inputs(repo.path(), &compat));
        let merged = crate::session::managed_mcp::merge_managed_mcp_servers(
            vec![],
            repo.path(),
            None,
            &compat,
        );
        let discovered_names: HashSet<_> = discovered.keys().cloned().collect();
        let merged_names: HashSet<_> = merged
            .iter()
            .map(|s| mcp_server_name(s).to_string())
            .collect();
        assert!(
            merged_names.is_subset(&discovered_names),
            "merge survivors must be reenable-discoverable: merge={merged_names:?} discovered={discovered_names:?}"
        );
        assert!(discovered_names.contains("drift_check_server"));
        assert!(merged_names.contains("drift_check_server"));
    }

    #[test]
    #[serial_test::serial]
    fn toml_duplicate_url_both_kept_matches_merge() {
        let (_home, _hg, _gg) = isolated_home();
        let repo = project_repo(
            r#"
[mcp_servers.first]
url = "https://dup.example.com/mcp"

[mcp_servers.second]
url = "https://dup.example.com/mcp"
"#,
        );
        let mut compat = CompatConfig::default();
        compat.claude.mcps = false;
        compat.cursor.mcps = false;
        let discovered = discover_mcp_definitions_ignoring_disable(&inputs(repo.path(), &compat));
        let merged = crate::session::managed_mcp::merge_managed_mcp_servers(
            vec![],
            repo.path(),
            None,
            &compat,
        );
        let merged_names: HashSet<_> = merged
            .iter()
            .map(|s| mcp_server_name(s).to_string())
            .collect();
        // Name is the identity: a shared URL never collapses entries
        assert!(discovered.contains_key("first"));
        assert!(discovered.contains_key("second"));
        assert!(merged_names.contains("first"));
        assert!(merged_names.contains("second"));
    }
}
