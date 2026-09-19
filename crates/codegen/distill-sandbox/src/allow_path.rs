//! Normalization of `read_only` / `read_write` sandbox config entries.
//!
//! This security-sensitive parser is kept separate from the profile resolver so the policy it implements stays small and easy to audit.
//! Allow paths are literal directory grants.
//! The only rewriting ever performed is stripping one trailing recursive glob down to the directory the user plainly meant.
//! Everything else either passes through byte-for-byte or is rejected, never widened.

use std::path::PathBuf;

use crate::deny::is_glob;

/// Allow paths are literal directory grants. One trailing `/**`, `/**/`, `/**/*`, or `/*` is stripped to the parent
/// (root forms grant `/`); a literal `**` directory used to hide the intended tree. Whitespace is not trimmed, and any
/// entry still glob-shaped after that strip is skipped rather than widened. `deny` globs are unchanged.
pub(crate) fn normalize_allow_path(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    if raw.trim() != raw {
        tracing::warn!(
            path = %raw,
            "sandbox allow path has surrounding whitespace; whitespace is significant in \
             literal paths, so fix the entry; skipping"
        );
        return None;
    }

    let mut s = raw;
    if let Some(parent) = s
        .strip_suffix("/**/*")
        .or_else(|| s.strip_suffix("/**/"))
        .or_else(|| s.strip_suffix("/**"))
        .or_else(|| s.strip_suffix("/*"))
    {
        s = parent.trim_end_matches('/');
        if s.is_empty() {
            // The whole path was a root glob (`/**`, `/**/`, `/**/*`, `/*`): the parent directory is the filesystem root
            s = "/";
        }
    }

    if is_glob(s) {
        tracing::warn!(
            path = %raw,
            "sandbox read_only/read_write paths are literal directory grants, not globs; \
             name the directory itself (or put globs under `deny`); skipping"
        );
        return None;
    }

    if s != raw {
        tracing::info!(
            original = %raw,
            normalized = %s,
            "stripped trailing glob from sandbox allow path"
        );
    }
    Some(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_allow_path_cases() {
        for (raw, expected) in [
            // One trailing glob group strips to the parent directory.
            ("/home/u/.cargo/cache/**", Some("/home/u/.cargo/cache")),
            ("/home/u/.cargo/cache/**/", Some("/home/u/.cargo/cache")),
            ("/home/u/.cargo/cache/**/*", Some("/home/u/.cargo/cache")),
            ("/tmp/scratch/*", Some("/tmp/scratch")),
            // Root globs grant the filesystem root, their parent directory.
            ("/**", Some("/")),
            ("/**/", Some("/")),
            ("/**/*", Some("/")),
            ("/*", Some("/")),
            // Literal directory paths pass through unchanged.
            ("/tmp/scratch", Some("/tmp/scratch")),
            ("/tmp/scratch/", Some("/tmp/scratch")),
            ("/", Some("/")),
            // Still glob-shaped after one strip: skip, never widen further (stacked wildcards must not collapse to a higher parent)
            ("/a/**/**", None),
            ("/a/*/*", None),
            ("/home/**/cache", None),
            ("/tmp/foo*", None),
            // Surrounding whitespace is rejected, never trimmed
            // Trimming would widen `/tmp/* ` into a grant of /tmp and rewrite `/srv/cache ` into a different directory than configured
            ("/tmp/* ", None),
            ("/srv/cache ", None),
            (" /srv/cache", None),
            ("   ", None),
            // Nothing configured, nothing to grant.
            ("", None),
        ] {
            assert_eq!(
                normalize_allow_path(raw),
                expected.map(PathBuf::from),
                "normalize_allow_path({raw:?})"
            );
        }
    }
}
