// Modified for Distill by Samuel Fajreldines, 2026.
//! Provides marketplace source configuration and plugin discovery, indexed with a filesystem fallback.
//! Install integration goes through the existing `InstallRegistry` pipeline.

#![deny(clippy::indexing_slicing)]

pub mod catalog;
pub mod config;
pub mod error;
pub mod git;
pub mod index;
pub mod install_resolve;
pub mod installer;
pub mod matcher;
pub mod scanner;
pub mod types;

pub use config::{
    env_require_sha, foreign_settings_roots, load_extra_sources_from_settings,
    load_extra_sources_from_settings_in, load_require_sha, load_sources, native_settings_roots,
};
pub use error::MarketplaceError;
pub use scanner::scan_marketplace;
pub use types::*;

/// Display name of the official Grok marketplace source.
pub const OFFICIAL_SOURCE_NAME: &str = "Grok Official";

/// Git URL of the official Grok marketplace source, auto-registered on first run.
pub const OFFICIAL_SOURCE_GIT_URL: &str = "https://github.com/example/plugin-marketplace.git";

/// Whether `url` is the official Grok marketplace source.
/// Case, a `www.` prefix, a trailing `/` or `.git`, and HTTPS/SSH forms are normalized before comparing.
pub fn is_official_source_url(url: &str) -> bool {
    canonical_github_owner_repo(url).as_deref() == Some("xai-org/plugin-marketplace")
}

/// Normalized lowercase `owner/repo` from a GitHub URL (HTTPS/http/ssh/scp, `www.`, trailing `.git`/`/`), or `None` if not a GitHub URL.
pub(crate) fn canonical_github_owner_repo(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s.strip_suffix('/').unwrap_or(s);
    let s = s.strip_suffix(".git").unwrap_or(s);
    let lower = s.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("ssh://"))
        .unwrap_or(&lower);
    let rest = rest.strip_prefix("git@").unwrap_or(rest);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let owner_repo = rest
        .strip_prefix("github.com/")
        .or_else(|| rest.strip_prefix("github.com:"))?;
    if owner_repo.is_empty() {
        None
    } else {
        Some(owner_repo.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_official_matches_canonical_https() {
        assert!(is_official_source_url(OFFICIAL_SOURCE_GIT_URL));
        assert!(is_official_source_url(
            "https://github.com/example/plugin-marketplace"
        ));
    }

    #[test]
    fn is_official_matches_ssh_form() {
        assert!(is_official_source_url(
            "git@github.com:example/plugin-marketplace.git"
        ));
        assert!(is_official_source_url(
            "git@github.com:example/plugin-marketplace"
        ));
        assert!(is_official_source_url(
            "ssh://git@github.com/example/plugin-marketplace.git"
        ));
        assert!(is_official_source_url(
            "ssh://git@github.com/example/plugin-marketplace"
        ));
    }

    #[test]
    fn is_official_rejects_unrelated_urls() {
        assert!(!is_official_source_url(
            "https://github.com/anthropics/claude-plugins-official.git"
        ));
        assert!(!is_official_source_url(
            "https://github.com/example/some-other-repo.git"
        ));
        assert!(!is_official_source_url(""));
    }

    #[test]
    fn is_official_matches_noncanonical_forms() {
        assert!(is_official_source_url(
            "https://GitHub.com/XAI-org/Plugin-Marketplace"
        ));
        assert!(is_official_source_url(
            "https://github.com/example/plugin-marketplace/"
        ));
        assert!(is_official_source_url(
            "https://github.com/example/plugin-marketplace.git/"
        ));
        assert!(is_official_source_url(
            "http://github.com/example/plugin-marketplace"
        ));
        assert!(is_official_source_url(
            "https://www.github.com/example/plugin-marketplace.git"
        ));
        assert!(is_official_source_url(
            "git@github.com:XAI-org/plugin-marketplace.git"
        ));
    }
}
