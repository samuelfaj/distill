// Modified for Distill by Samuel Fajreldines, 2026.
//! `[paths]` configuration: extra directories for skills and rules.
//!
//! `extra_rule_dirs` supplements the built-in rule scan (`.grok/`, `.agents/`,
//! `~/.grok/rules/`, …). `extra_skill_dirs` is written by `/import-claude` so
//! Claude skill locations survive the runtime `.claude/` cutoff. Skill
//! injection does not read `extra_skill_dirs`. Extra injection dirs belong
//! in `[skills] paths`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Example:
/// ```toml
/// [paths]
/// extra_skill_dirs = ["~/.claude/skills", "/path/to/.claude/skills"]
/// extra_rule_dirs = ["~/.claude/rules"]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Additional directories to scan for skills (each contains `<skill>/SKILL.md`).
    /// `/import-claude` writes this. `list_skills_with_plugins` does not read it.
    /// Extra injection dirs belong in `[skills] paths`. ACP `x.ai/skills/list` may
    /// still show these dirs as source folders.
    pub extra_skill_dirs: Vec<String>,
    /// Additional directories to scan for rules (each contains `*.md`).
    pub extra_rule_dirs: Vec<String>,
}

impl PathsConfig {
    /// `extra_rule_dirs` with `~` expanded against `home`, in config order. Only absolute results are kept: a
    /// relative entry would resolve against the process cwd and enter discovery as a trusted home-scope root.
    pub(crate) fn rule_dirs(&self, home: Option<&Path>) -> Vec<PathBuf> {
        self.extra_rule_dirs
            .iter()
            .map(|dir| expand_tilde_in(dir, home))
            .filter(|dir| dir.is_absolute())
            .collect()
    }
}

/// Expand `~` or a `~/` prefix against the current user's home, like the shell's `expand_home`.
pub(crate) fn expand_tilde(raw: &str) -> PathBuf {
    expand_tilde_in(raw, distill_dirs::home_dir().as_deref())
}

fn expand_tilde_in(raw: &str, home: Option<&Path>) -> PathBuf {
    match (raw, raw.strip_prefix("~/"), home) {
        ("~", _, Some(home)) => home.to_path_buf(),
        (_, Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixtures are POSIX-absolute; on Windows they would be relative and dropped
    #[cfg(unix)]
    #[test]
    fn rule_dirs_expands_tilde_and_drops_relative_entries() {
        let cfg = PathsConfig {
            extra_rule_dirs: [
                "~/.claude/rules",
                "/abs/team-rules",
                "~",
                "relative",
                "~rules",
            ]
            .map(str::to_owned)
            .to_vec(),
            ..Default::default()
        };
        assert_eq!(
            vec![
                PathBuf::from("/home/u/.claude/rules"),
                PathBuf::from("/abs/team-rules"),
                PathBuf::from("/home/u"),
            ],
            cfg.rule_dirs(Some(Path::new("/home/u")))
        );
        // Without a home, `~` entries are relative too
        assert_eq!(vec![PathBuf::from("/abs/team-rules")], cfg.rule_dirs(None));
    }
}
