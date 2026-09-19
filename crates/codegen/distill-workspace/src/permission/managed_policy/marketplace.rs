//! Marketplace policy: per-source allowlists, the cross-source
//! [`MarketplacePolicy`] (strictest wins), managed marketplace pins, and the
//! canonical git-URL identity.

use super::layer::{PolicyLayerOwnership, PolicySourceAuthority};
use super::mcp::PolicySubjectOrigin;
use super::verdict::user_facing_policy_source;

/// Marketplace allowlist from ONE source; exists only when its strict key was
/// present, so empty `allowed_urls` is a lockdown (see `McpServerAllowlist::lockdown`; no `Default`).
#[derive(Debug, Clone)]
pub struct MarketplaceAllowlist {
    pub allowed_urls: Vec<String>,
    pub source_path: Option<std::path::PathBuf>,
    /// Whether this source's restrictions bind grok-native marketplaces.
    pub authority: PolicySourceAuthority,
}

impl MarketplaceAllowlist {
    /// See [`McpServerAllowlist::binds`].
    fn binds(&self, origin: PolicySubjectOrigin) -> bool {
        self.authority == PolicySourceAuthority::Native || origin == PolicySubjectOrigin::Foreign
    }

    /// Full lockdown (see the type docs): reporting must not render it unrestricted.
    pub fn is_lockdown(&self) -> bool {
        self.allowed_urls.is_empty()
    }

    /// Membership check; an empty list allows nothing (see the type docs).
    pub fn is_url_allowed(&self, url: &str) -> bool {
        let normalized = normalize_git_url(url);
        self.allowed_urls
            .iter()
            .any(|allowed| normalize_git_url(allowed) == normalized)
    }

    pub fn block_reason(&self) -> String {
        match &self.source_path {
            Some(p) => format!("source not in strictKnownMarketplaces ({})", p.display()),
            None => "source not in strictKnownMarketplaces".to_string(),
        }
    }

    /// [`Self::block_reason`] with the policy file reduced to its name — the
    /// user-facing refusal form; tracing logs keep the full-path form.
    fn user_facing_block_reason(&self) -> String {
        match &self.source_path {
            Some(p) => format!(
                "source not in strictKnownMarketplaces ({})",
                user_facing_policy_source(p)
            ),
            None => "source not in strictKnownMarketplaces".to_string(),
        }
    }
}

/// Marketplace policy across all sources: a URL must pass every restricted
/// source (strictest wins).
#[derive(Debug, Clone, Default)]
pub struct MarketplacePolicy {
    pub sources: Vec<MarketplaceAllowlist>,
}

impl MarketplacePolicy {
    /// A single-source policy (test construction across crates).
    pub fn single(allowlist: MarketplaceAllowlist) -> Self {
        Self {
            sources: vec![allowlist],
        }
    }

    pub fn is_restricted(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Restriction active for a subject of `origin` (advisory strict lists
    /// don't bind grok-native marketplaces).
    pub fn is_restricted_for(&self, origin: PolicySubjectOrigin) -> bool {
        self.sources.iter().any(|s| s.binds(origin))
    }

    pub fn is_url_allowed(&self, url: &str, origin: PolicySubjectOrigin) -> bool {
        self.sources
            .iter()
            .filter(|s| s.binds(origin))
            .all(|s| s.is_url_allowed(url))
    }

    /// The binding source that actually rejects `url`, falling back to the first binding source
    /// when every one allows (callers only ask after a block).
    fn blocking_source(
        &self,
        url: &str,
        origin: PolicySubjectOrigin,
    ) -> Option<&MarketplaceAllowlist> {
        self.sources
            .iter()
            .find(|s| s.binds(origin) && !s.is_url_allowed(url))
            .or_else(|| self.sources.iter().find(|s| s.binds(origin)))
    }

    /// Reason `url` is blocked, attributed to the blocking source. Full-path
    /// form (tracing logs); user surfaces get [`Self::add_block_reason`].
    pub fn block_reason(&self, url: &str, origin: PolicySubjectOrigin) -> String {
        self.blocking_source(url, origin)
            .map(MarketplaceAllowlist::block_reason)
            .unwrap_or_else(|| "source not in strictKnownMarketplaces".to_string())
    }

    /// Fail-closed add/install gate: `Some(reason)` when restricted and `identity` isn't allowed (local paths never match).
    /// An add/install is not yet grok-native, so every policy source binds, including advisory ones; the carve-out never covers acquiring new sources.
    /// The refusal names the blocking policy file only (logs use [`Self::block_reason`]).
    pub fn add_block_reason(&self, identity: &str) -> Option<String> {
        let origin = PolicySubjectOrigin::Foreign;
        (self.is_restricted_for(origin) && !self.is_url_allowed(identity, origin)).then(|| {
            self.blocking_source(identity, origin)
                .map(MarketplaceAllowlist::user_facing_block_reason)
                .unwrap_or_else(|| "source not in strictKnownMarketplaces".to_string())
        })
    }

    /// Union across sources — display only (matching intersects).
    pub fn allowed_urls(&self) -> Vec<String> {
        let mut out = Vec::new();
        for source in &self.sources {
            for url in &source.allowed_urls {
                if !out.contains(url) {
                    out.push(url.clone());
                }
            }
        }
        out
    }
}

/// A marketplace pinned by managed policy via `extraKnownMarketplaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedMarketplace {
    pub name: String,
    pub kind: ManagedMarketplaceKind,
    /// Ownership of the layer that provisioned the pin.
    pub ownership: PolicyLayerOwnership,
}

/// How to reach a managed marketplace. `github`+`repo` sources are
/// canonicalized to their clone URL at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedMarketplaceKind {
    Git {
        url: String,
        /// Optional branch/tag (`ref` in the Claude JSON).
        git_ref: Option<String>,
    },
    Local {
        path: String,
    },
}

/// Canonical git-URL identity for marketplace allowlist/dedup.
/// Only scheme and authority fold case — lowercasing the path would widen an entry to sibling repos. Exactly one `.git` suffix is stripped.
pub fn normalize_git_url(url: &str) -> String {
    let url = url.strip_suffix(".git").unwrap_or(url);
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        return format!(
            "{}://{}{path}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase()
        );
    }
    // scp-style `git@host:org/repo` — the part before `:` is user@host.
    if let Some((user_host, path)) = url.split_once(':')
        && user_host.contains('@')
    {
        return format!("{}:{path}", user_host.to_ascii_lowercase());
    }
    url.to_string()
}
