// Modified for Distill by Samuel Fajreldines, 2026.
//! The marketplace SOURCE model: loading configured sources (config.toml + settings JSON +
//! managed pins) and filtering them through the managed `marketplace_allowlist`.

use std::path::Path;

use distill_plugin_marketplace::{
    MarketplaceSource, SourceKind, load_extra_sources_from_settings_in, load_sources,
};
use distill_workspace::permission::resolution::PolicySubjectOrigin;

/// Marketplace sources from config.toml + settings JSON + managed
/// `extraKnownMarketplaces` pins. Unfiltered.
pub fn load_marketplace_sources() -> Vec<MarketplaceSource> {
    load_marketplace_sources_with_origin()
        .into_iter()
        .map(|(source, _)| source)
        .collect()
}

/// [`load_marketplace_sources`] with each source origin-classified for advisory scoping:
/// config.toml, admin pins, and `~/.grok` are grok-native; `~/.claude` sources are foreign.
fn load_marketplace_sources_with_origin() -> Vec<(MarketplaceSource, PolicySubjectOrigin)> {
    let config = crate::config::load_effective_config()
        .ok()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    load_marketplace_sources_with_origin_from(
        &config,
        &distill_workspace::permission::resolution::managed_settings().extra_marketplaces,
        &distill_plugin_marketplace::native_settings_roots(),
        &distill_plugin_marketplace::foreign_settings_roots(),
    )
}

/// [`load_marketplace_sources_with_origin`] over injected inputs, so tests
/// can pin the origin tagging with isolated settings roots.
fn load_marketplace_sources_with_origin_from(
    config: &toml::Value,
    managed: &[distill_workspace::permission::resolution::ManagedMarketplace],
    native_roots: &[std::path::PathBuf],
    foreign_roots: &[std::path::PathBuf],
) -> Vec<(MarketplaceSource, PolicySubjectOrigin)> {
    // Dedup helpers read the bare sources; the lists are a handful of entries.
    let plain = |tagged: &[(MarketplaceSource, PolicySubjectOrigin)]| -> Vec<MarketplaceSource> {
        tagged.iter().map(|(source, _)| source.clone()).collect()
    };

    let mut sources: Vec<(MarketplaceSource, PolicySubjectOrigin)> = load_sources(config)
        .into_iter()
        .map(|source| (source, PolicySubjectOrigin::GrokNative))
        .collect();
    sources.extend(
        managed_extra_marketplace_sources(managed, &plain(&sources))
            .into_iter()
            .map(|source| (source, PolicySubjectOrigin::GrokNative)),
    );
    sources.extend(
        load_extra_sources_from_settings_in(&plain(&sources), native_roots)
            .into_iter()
            .map(|source| (source, PolicySubjectOrigin::GrokNative)),
    );
    sources.extend(
        load_extra_sources_from_settings_in(&plain(&sources), foreign_roots)
            .into_iter()
            .map(|source| (source, PolicySubjectOrigin::Foreign)),
    );
    sources
}

/// Convert managed `extraKnownMarketplaces` pins into marketplace sources, skipping names/URLs
/// already configured (git `ref` carried as the sync branch).
fn managed_extra_marketplace_sources(
    managed: &[distill_workspace::permission::resolution::ManagedMarketplace],
    existing: &[MarketplaceSource],
) -> Vec<MarketplaceSource> {
    use distill_workspace::permission::resolution::{ManagedMarketplaceKind, normalize_git_url};
    let mut out: Vec<MarketplaceSource> = Vec::new();
    for m in managed {
        let kind = match &m.kind {
            ManagedMarketplaceKind::Git { url, git_ref } => SourceKind::Git {
                url: url.clone(),
                branch: git_ref.clone(),
            },
            ManagedMarketplaceKind::Local { path } => SourceKind::Local {
                path: std::path::PathBuf::from(path),
            },
        };
        let same_url = |s: &MarketplaceSource| match (&s.kind, &kind) {
            (SourceKind::Git { url: a, .. }, SourceKind::Git { url: b, .. }) => {
                normalize_git_url(a) == normalize_git_url(b)
            }
            // Local pins dedupe by exact path (same comparison as the strict-list exemption), so a pin of
            // an already-configured directory doesn't register a duplicate row.
            (SourceKind::Local { path: a }, SourceKind::Local { path: b }) => a == b,
            _ => false,
        };
        let collides = existing
            .iter()
            .chain(out.iter())
            .any(|s| s.name == m.name || same_url(s));
        // A configured source holding the pin's NAME with a different URL
        // silently redirects what the admin provisioned — surface it.
        if let Some(squatter) = existing
            .iter()
            .chain(out.iter())
            .find(|s| s.name == m.name && !same_url(s))
        {
            tracing::warn!(
                name = %m.name,
                configured = %super::registered_source_label(squatter),
                "managed extraKnownMarketplaces pin name is already taken by a \
                 configured source with a different URL; the pin is not registered"
            );
        }
        if !collides {
            out.push(MarketplaceSource {
                name: m.name.clone(),
                kind,
            });
        }
    }
    out
}

/// [`load_marketplace_sources`] minus sources blocked by the managed
/// `marketplace_allowlist`. Install, update, and list paths must use this.
pub fn load_filtered_marketplace_sources() -> Vec<MarketplaceSource> {
    let ms = distill_workspace::permission::resolution::managed_settings();
    filter_sources_by_allowlist(
        load_marketplace_sources_with_origin(),
        &ms.marketplace_allowlist,
        &ms.extra_marketplaces,
    )
}

/// Each source is checked at its own origin: an advisory strict list drops only foreign-defined
/// sources, a native one binds everything; local sources drop unless an exact-path pin exempts.
fn filter_sources_by_allowlist(
    mut sources: Vec<(MarketplaceSource, PolicySubjectOrigin)>,
    allowlist: &distill_workspace::permission::resolution::MarketplacePolicy,
    managed_pins: &[distill_workspace::permission::resolution::ManagedMarketplace],
) -> Vec<MarketplaceSource> {
    use distill_workspace::permission::resolution::ManagedMarketplaceKind;
    if allowlist.is_restricted() {
        sources.retain(|(source, origin)| match &source.kind {
            SourceKind::Git { url, .. } => {
                let allowed = allowlist.is_url_allowed(url, *origin);
                if !allowed {
                    tracing::warn!(
                        name = %source.name,
                        url,
                        reason = %allowlist.block_reason(url, *origin),
                        "Marketplace source blocked by allowlist"
                    );
                }
                allowed
            }
            SourceKind::Local { path } => {
                // Extras accumulate from every layer: a pin in user-writable ~/.grok files must not carve an
                // exception out of a root-owned lockdown.
                let admin_pinned = managed_pins.iter().any(|pin| {
                    pin.ownership
                        == distill_workspace::permission::resolution::PolicyLayerOwnership::Admin
                        && matches!(&pin.kind, ManagedMarketplaceKind::Local { path: pinned }
                            if Path::new(pinned) == path)
                });
                let allowed = admin_pinned || !allowlist.is_restricted_for(*origin);
                if !allowed {
                    tracing::warn!(
                        name = %source.name,
                        path = %path.display(),
                        "Local marketplace source blocked: a strict marketplace \
                         allowlist is active and local sources cannot be allowlisted"
                    );
                }
                allowed
            }
        });
    }
    sources.into_iter().map(|(source, _)| source).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_fixtures::{git_source, local_source, marketplace_allowlist};

    /// A binding strict list drops non-allowlisted git AND local sources; only the exact-path admin pin exempts.
    #[test]
    fn filter_sources_by_allowlist_drops_blocked_git_and_unpinned_local() {
        use distill_workspace::permission::resolution::{
            ManagedMarketplace, ManagedMarketplaceKind, PolicyLayerOwnership,
        };

        let allowlist = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let pins = vec![
            ManagedMarketplace {
                name: "Pinned Local".into(),
                kind: ManagedMarketplaceKind::Local {
                    path: "/opt/marketplace".into(),
                },
                ownership: PolicyLayerOwnership::Admin,
            },
            // A pin from a user-writable layer must not carve an exception
            // out of the lockdown.
            ManagedMarketplace {
                name: "User Pin".into(),
                kind: ManagedMarketplaceKind::Local {
                    path: "/tmp/user-pin".into(),
                },
                ownership: PolicyLayerOwnership::User,
            },
        ];
        let sources = || {
            vec![
                (
                    git_source("Allowed", "https://github.com/ok/repo.git"),
                    PolicySubjectOrigin::Foreign,
                ),
                (
                    git_source("Blocked", "https://github.com/bad/repo.git"),
                    PolicySubjectOrigin::Foreign,
                ),
                (
                    local_source("Local", "/tmp/p"),
                    PolicySubjectOrigin::GrokNative,
                ),
                (
                    local_source("Pinned Local", "/opt/marketplace"),
                    PolicySubjectOrigin::GrokNative,
                ),
                // Squats the pin's name with a different path — not exempt.
                (
                    local_source("Pinned Local", "/tmp/squat"),
                    PolicySubjectOrigin::GrokNative,
                ),
                (
                    local_source("User Pin", "/tmp/user-pin"),
                    PolicySubjectOrigin::GrokNative,
                ),
            ]
        };
        let filtered = filter_sources_by_allowlist(sources(), &allowlist, &pins);
        let labels: Vec<String> = filtered
            .iter()
            .map(|s| match &s.kind {
                SourceKind::Git { url, .. } => format!("{}:{url}", s.name),
                SourceKind::Local { path } => format!("{}:{}", s.name, path.display()),
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "Allowed:https://github.com/ok/repo.git",
                "Pinned Local:/opt/marketplace",
            ]
        );

        // No policy, no pins: everything survives, blocked or local alike.
        let unrestricted = distill_workspace::permission::resolution::MarketplacePolicy::default();
        assert_eq!(
            filter_sources_by_allowlist(sources(), &unrestricted, &[]).len(),
            sources().len()
        );
    }

    /// Managed pins register as sources (git `ref` carried as the sync
    /// branch), deduped against configured names/URLs.
    #[test]
    fn managed_extra_marketplaces_register_with_ref_and_dedup() {
        use distill_workspace::permission::resolution::{
            ManagedMarketplace, ManagedMarketplaceKind, PolicyLayerOwnership,
        };

        let existing = vec![
            git_source("Configured", "https://github.com/cfg/repo.git"),
            local_source("My Local", "/opt/mp"),
        ];
        let managed = vec![
            ManagedMarketplace {
                name: "approved-plugins".into(),
                kind: ManagedMarketplaceKind::Git {
                    url: "https://github.com/example-corp/approved-plugins.git".into(),
                    git_ref: Some("main".into()),
                },
                ownership: PolicyLayerOwnership::Admin,
            },
            // Same URL as a configured source — not duplicated.
            ManagedMarketplace {
                name: "dup".into(),
                kind: ManagedMarketplaceKind::Git {
                    url: "https://github.com/cfg/repo.git".into(),
                    git_ref: None,
                },
                ownership: PolicyLayerOwnership::Admin,
            },
            // Same path as a configured Local source — not duplicated.
            ManagedMarketplace {
                name: "dup-local".into(),
                kind: ManagedMarketplaceKind::Local {
                    path: "/opt/mp".into(),
                },
                ownership: PolicyLayerOwnership::Admin,
            },
            ManagedMarketplace {
                name: "local-pin".into(),
                kind: ManagedMarketplaceKind::Local {
                    path: "/opt/marketplace".into(),
                },
                ownership: PolicyLayerOwnership::Admin,
            },
        ];

        let added = managed_extra_marketplace_sources(&managed, &existing);
        let names: Vec<&str> = added.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["approved-plugins", "local-pin"]);
        let Some(first) = added.first() else {
            panic!("expected managed extra sources: {added:?}");
        };
        assert!(matches!(
            &first.kind,
            SourceKind::Git { url, branch }
                if url == "https://github.com/example-corp/approved-plugins.git"
                    && branch.as_deref() == Some("main")
        ));
    }

    /// Origin tagging: config.toml and `~/.grok` sources are grok-native, `~/.claude` foreign; both-home dupes dedup native.
    #[test]
    fn load_sources_with_origin_tags_native_and_foreign_roots() {
        fn write_known(root: &std::path::Path, entries: &[(&str, &str)]) {
            let plugins = root.join("plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            let map: serde_json::Map<String, serde_json::Value> = entries
                .iter()
                .map(|(name, url)| {
                    (
                        name.to_string(),
                        serde_json::json!({"source": {"source": "git", "url": url}}),
                    )
                })
                .collect();
            std::fs::write(
                plugins.join("known_marketplaces.json"),
                serde_json::to_string(&serde_json::Value::Object(map)).unwrap(),
            )
            .unwrap();
        }

        let native_home = tempfile::tempdir().unwrap();
        let foreign_home = tempfile::tempdir().unwrap();
        write_known(
            native_home.path(),
            &[("shared", "https://example.com/shared.git")],
        );
        write_known(
            foreign_home.path(),
            &[
                // Same URL as the native root: dedups grok-native-first.
                ("shared", "https://example.com/shared.git"),
                ("claude-only", "https://example.com/claude-only.git"),
            ],
        );
        let config: toml::Value = toml::from_str(
            "[[marketplace.sources]]\nname = \"cfg\"\ngit = \"https://example.com/cfg.git\"\n",
        )
        .unwrap();

        let tagged = load_marketplace_sources_with_origin_from(
            &config,
            &[],
            &[native_home.path().to_path_buf()],
            &[foreign_home.path().to_path_buf()],
        );
        let origin_of = |url: &str| -> PolicySubjectOrigin {
            let hits: Vec<_> = tagged
                .iter()
                .filter(|(s, _)| matches!(&s.kind, SourceKind::Git { url: u, .. } if u == url))
                .collect();
            let [hit] = hits.as_slice() else {
                panic!("{url} must resolve to one entry: {tagged:?}");
            };
            hit.1
        };
        assert_eq!(
            origin_of("https://example.com/cfg.git"),
            PolicySubjectOrigin::GrokNative,
            "config.toml sources are grok-native"
        );
        assert_eq!(
            origin_of("https://example.com/shared.git"),
            PolicySubjectOrigin::GrokNative,
            "a URL in both homes must dedup to the grok-native entry"
        );
        assert_eq!(
            origin_of("https://example.com/claude-only.git"),
            PolicySubjectOrigin::Foreign,
            "~/.claude settings sources are foreign"
        );
    }

    /// An advisory (Claude-file) strict list drops only foreign-defined
    /// sources; grok-native config sources survive it.
    #[test]
    fn filter_sources_by_allowlist_advisory_exempts_native_sources() {
        let allowlist = distill_workspace::permission::resolution::MarketplacePolicy::single(
            distill_workspace::permission::resolution::MarketplaceAllowlist {
                allowed_urls: vec!["https://github.com/ok/repo.git".into()],
                source_path: None,
                authority:
                    distill_workspace::permission::resolution::PolicySourceAuthority::Advisory,
            },
        );
        let sources = vec![
            (
                git_source("Native Unlisted", "https://github.com/native/repo.git"),
                PolicySubjectOrigin::GrokNative,
            ),
            (
                git_source("Foreign Unlisted", "https://github.com/foreign/repo.git"),
                PolicySubjectOrigin::Foreign,
            ),
            // Local sources drop only when the strict list binds their origin.
            (
                local_source("Native Local", "/tmp/native"),
                PolicySubjectOrigin::GrokNative,
            ),
            (
                local_source("Foreign Local", "/tmp/foreign"),
                PolicySubjectOrigin::Foreign,
            ),
        ];
        let filtered = filter_sources_by_allowlist(sources, &allowlist, &[]);
        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Native Unlisted", "Native Local"]);
    }
}
