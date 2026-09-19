// Modified for Distill by Samuel Fajreldines, 2026.
//! Project config-file discovery: locating repo-local `.mcp.json` and `.grok/config.toml` files by walking from `cwd` up to the git root.
//!
//! These pure `git2` and filesystem walks are shared by the shell's config loaders and the folder-trust gate's `repo_configs_present`.

use std::path::{Path, PathBuf};

use distill_agent::repo::RepoDirChain;

/// Filename of the project-local MCP server config.
pub const MCP_JSON_FILENAME: &str = ".mcp.json";

/// Candidate `.mcp.json` paths from repo root to `cwd`, whether or not they exist.
/// Useful for file watching so newly created files are detected after startup.
pub fn mcp_json_candidate_paths(cwd: &Path) -> Vec<PathBuf> {
    mcp_json_candidate_paths_in(&RepoDirChain::resolve(cwd).dirs)
}

/// [`mcp_json_candidate_paths`] over a precomputed cwd-to-git-root dir chain ([`RepoDirChain`]), repo-root-first.
fn mcp_json_candidate_paths_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    chain_dirs
        .iter()
        .rev()
        .map(|dir| dir.join(MCP_JSON_FILENAME))
        .collect()
}

/// Find existing `.mcp.json` files from `cwd` up to the git root (repo-root-first order).
pub fn find_mcp_json_files(cwd: &Path) -> Vec<PathBuf> {
    find_mcp_json_files_in(&RepoDirChain::resolve(cwd).dirs)
}

/// [`find_mcp_json_files`] over a precomputed dir chain. See [`RepoDirChain`].
/// `pub(crate)` so the folder-trust gate's `repo_configs_present` can call it.
pub(crate) fn find_mcp_json_files_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    mcp_json_candidate_paths_in(chain_dirs)
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

/// True when `config_path` is `$GROK_HOME/config.toml` (user tier, not project).
fn is_user_grok_config_file(config_path: &Path) -> bool {
    let Some(user_home) = distill_config::user_distill_home() else {
        return false;
    };
    let user_config = user_home.join("config.toml");
    if config_path == user_config.as_path() {
        return true;
    }
    let Ok(canonical_config) = dunce::canonicalize(config_path) else {
        return false;
    };
    let canonical_user = dunce::canonicalize(&user_config).unwrap_or(user_config);
    canonical_config == canonical_user
}

/// Find `.grok/config.toml` from `cwd` up to the git repo root, repo-root (lowest) to cwd (highest), matching skills and AGENTS.md discovery.
/// No repo: only `cwd/.grok/config.toml`. Excludes user-global config so `cwd == $HOME` is not a project overlay.
pub fn find_project_configs(cwd: &Path) -> Vec<PathBuf> {
    find_project_configs_in(&RepoDirChain::resolve(cwd).dirs)
}

/// [`find_project_configs`] over a precomputed [`RepoDirChain`], repo-root-first.
/// Excludes user-global config so `cwd == $HOME` is not a project overlay. `pub(crate)` for the folder-trust gate.
pub(crate) fn find_project_configs_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    // `dirs` is cwd-first; reverse so repo root comes first (lowest priority) and cwd last (highest)
    chain_dirs
        .iter()
        .rev()
        .map(|dir| dir.join(".grok").join("config.toml"))
        .filter(|config_path| config_path.is_file() && !is_user_grok_config_file(config_path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_project_configs_excludes_user_grok_config_file() {
        let Some(user_home) = distill_config::user_distill_home() else {
            return;
        };
        let user_config = user_home.join("config.toml");
        if user_config.is_file() {
            let home = distill_dirs::home_dir().expect("home dir");
            let from_home = find_project_configs(&home);
            assert!(
                !from_home.iter().any(|p| is_user_grok_config_file(p)),
                "user config leaked into project configs: {from_home:?}"
            );
            assert!(is_user_grok_config_file(&user_config));
        }

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join(".grok")).unwrap();
        std::fs::write(project.join(".grok/config.toml"), "# project\n").unwrap();
        let found = find_project_configs(&project);
        assert_eq!(found.len(), 1);
        let Some(first) = found.first() else {
            panic!("expected one project config: {found:?}");
        };
        assert!(!is_user_grok_config_file(first));
    }
}
