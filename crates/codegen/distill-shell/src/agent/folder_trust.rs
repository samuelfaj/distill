// Modified for Distill by Samuel Fajreldines, 2026.
//! Folder-trust gate ("do you trust this folder?").
//!
//! Repo-local MCP / LSP servers, permission policy, and project instructions/skills are configured by files an attacker can ship inside a cloned repository.
//! Those files include `.mcp.json`, project `.grok/config.toml` (`[permission]` / `[mcp_servers]` / `[plugins].paths`), `.grok/lsp.json`, `AGENTS.md` / `CLAUDE.md`, and `.grok/skills`.
//! `~/.claude.json` `projects.<cwd>` is another such source.
//! Those configs contain commands, auto-approve rules, or agent instructions the CLI would otherwise honor automatically, a 1-click RCE / policy bypass.
//! This module resolves a VS-Code-style trust decision ONCE per workspace, BEFORE any repo-local server is spawned.
//! It exposes a cheap [`project_scope_allowed`] check that the MCP/LSP/permission, instruction, and skill loaders consult.
//!
//! Resolution lives here (not in `acp_session`) so the session core stays free of feature logic.
//! The loaders only call [`project_scope_allowed`].
//!
//! The DECISION side lives in `distill-workspace` (client-side).
//! It holds the workspace scan, the pure [`decide`] precedence, and the interactive prompt.
//! It also owns the durable [`distill_workspace::trust::TrustStore`] reads/writes.
//! This module keeps the CONSUME/gating side: the `DECISIONS` cache, [`resolve_and_record`], and the loader filters.
//! The ordered trust precedence is documented canonically on [`distill_workspace::folder_trust::decide`].
//! The consume-side nuance is that two allows are PROVISIONAL (NOT cached).
//! The first is the "no repo configs" allow.
//! Configs appearing after the first resolve (git pull / agent write) are re-checked on the next resolve rather than riding a stale grant.
//! The second is the unrecordable-key allow (cwd is $HOME / fs-root), which can never be persisted (see [`resolve_and_record_inner`] / [`compute`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use agent_client_protocol as acp;
use distill_workspace::trust::{is_unsafe_trust_root, workspace_key};
use parking_lot::Mutex;

// Decision-side (scan/decide/prompt/store) lives in `distill-workspace` (client crate)
// Only the helpers used by this consume-side module are imported; callers should use the workspace API for explicit trust decisions
use distill_workspace::folder_trust::{
    DecideInputs, GrantOutcome, PersistStatus, TrustOutcome, claude_project_mcp_names, decide,
    decide_inputs, decide_inputs_with_interactive, feature_enabled, folder_trust_inert,
    grant_folder_trust_key, prompt_for_trust,
};

use crate::session::managed_mcp::mcp_server_name;
use crate::util::config::{MCP_SCOPE_PROJECT, RemoteSettings};

// NOTE: this folder-trust store (`~/.grok/trusted_folders.toml`) is SEPARATE from the pre-existing per-plugin trust store (`distill_agent::plugins::TrustStore` at `~/.grok/trusted-plugins`, plus the hooks' own project-trust gating)
// Trusting a folder here does NOT imply plugin trust and vice versa; the two are independent and non-contradicting
// Unifying them is a tracked follow-up

/// Per-workspace resolved decision: `true` = repo-local (project-scoped)
/// servers are allowed to spawn. Keyed by canonical workspace key.
static DECISIONS: LazyLock<Mutex<HashMap<PathBuf, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Compatibility alias; new call sites should use the workspace API directly.
pub use distill_workspace::folder_trust::grant_folder_trust;

/// Revoke trust for `cwd`'s workspace and downgrade the consume-side cache.
/// Without the cache downgrade a cached grant would short-circuit [`resolve_and_record`], so hooks would keep loading until restart.
/// Unrecordable roots ($HOME / fs-root) are refused because no later grant can lift a cache deny for them.
pub(crate) fn revoke_folder_trust(cwd: &Path) -> bool {
    // Local/dev builds are fully inert: nothing was trusted-via-gate to revoke
    // Recording `false` here would make `project_scope_allowed` wrongly gate
    if folder_trust_inert() {
        return false;
    }
    let key = workspace_key(cwd);
    // Mirror the store's over-broad-root refusal: an unrecordable key resolves Trusted by rule and can never be store-granted
    // A cache deny here would be permanent for the process with no in-product recovery
    if is_unsafe_trust_root(&key) {
        tracing::warn!(
            key = %key.display(),
            "refusing folder-trust revoke for an over-broad root (never gated)"
        );
        return false;
    }
    let was_trusted = distill_workspace::folder_trust::revoke_folder_trust_store(cwd);
    // Always downgrade the in-process cache so a mid-session untrust takes effect immediately for this process
    // That holds even for a cached grant with no backing store record (e.g. a kill-switch / feature-off resolve).
    // A later legitimate grant reconciles it: the `Some(false)` arm of `resolve_and_record_inner` re-checks the store
    record(&key, false);
    was_trusted
}

/// Authoritative and fail-closed, mirroring [`resolve_and_record_inner`]'s arms. A cached **grant** short-circuits (allow).
/// A cached **untrusted** verdict is RE-READ against the store so a `grant_folder_trust` issued AFTER the untrusted resolve is honored. That re-read records the upgrade and allows.
/// So this never over-denies the common no-configs case, whose Trusted verdict is provisional and therefore never cached. There [`resolve_and_record`] would short-circuit to allow before reaching the cache. `DECISIONS` uses `parking_lot::Mutex` (no poisoning), so this gate cannot fail OPEN on a poisoned lock.
pub fn project_scope_allowed(cwd: &Path) -> bool {
    let key = workspace_key(cwd);
    // Copy out of the lock so the Some(false) reconcile can re-acquire it (parking_lot mutexes are not re-entrant)
    let cached = DECISIONS.lock().get(&key).copied();
    match cached {
        Some(true) => true,
        // Re-read the store so a grant issued after the untrusted resolve is honored without a restart (mirrors `resolve_and_record_inner`)
        Some(false) => {
            if distill_workspace::folder_trust::is_trusted_this_process(&key) {
                record(&key, true);
                true
            } else {
                false
            }
        }
        // Unrecorded: re-resolve fail-closed (no-configs / trusted / feature-off / inert allow; untrusted with configs deny)
        None => resolve_and_record(cwd, None, false),
    }
}

/// Whether an interactive GUI trust PROMPT is warranted for `cwd`. Warranted means the feature is on, the workspace is NOT store-trusted, and repo-local code-exec configs are present (something to gate).
/// Interactivity is forced `true` because the caller already confirmed the client can prompt (it advertised `x.ai/folderTrust.interactive`). The TTY-based [`decide_inputs`] default is false under the ACP stdio transport.
/// Mirrors the [`decide`] precedence so it cannot drift from the gate. Feature-off (kill-switch / opt-out) / store-trusted / no-configs all collapse to a non-`Prompt` verdict and return false.
pub(crate) fn prompt_warranted(cwd: &Path, remote: Option<&RemoteSettings>) -> bool {
    let key = workspace_key(cwd);
    matches!(
        decide(
            feature_enabled(remote),
            &decide_inputs_with_interactive(cwd, &key, true),
        ),
        TrustOutcome::Prompt
    )
}

/// Display-only summary of which repo-local code-exec config kinds are present for `cwd`, for the interactive trust prompt's UI. The kinds are the reasons the folder is gated.
/// Single-sourced from the SAME scan as the canonical gate ([`distill_workspace::folder_trust::repo_config_kinds`] / [`repo_configs_present`]).
/// The prompt's reason list therefore cannot drift from what actually gated the folder (same markers, same cwd-to-git-root walk). So an `.grok/lsp.json`-only repo still has a non-empty reason list. Only the post-grant *hot-reload* skips LSP; project LSP applies on the next session open (the backend is spawn-baked into the tool bridge).
pub(crate) fn detected_config_kinds(cwd: &Path) -> Vec<String> {
    distill_workspace::folder_trust::repo_config_kinds(cwd)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Project agents require folder trust; user/bundled/built-in agents always pass. Plugin deny is call-site-only, before this predicate.
/// `trusted` is evaluated lazily so non-project agents skip the filesystem-walking trust verdict. Primary-session passes its already-computed `hooks_trusted`; subagent sites pass `project_scope_allowed(parent_cwd)`.
pub(crate) fn agent_inline_hooks_allowed(
    scope: distill_agent::config::AgentScope,
    trusted: impl FnOnce() -> bool,
) -> bool {
    scope != distill_agent::config::AgentScope::Project || trusted()
}

fn record(workspace_key: &Path, allowed: bool) {
    DECISIONS
        .lock()
        .insert(workspace_key.to_path_buf(), allowed);
}

/// Test-only: force the recorded decision for `cwd`'s workspace key.
/// Tests use UNIQUE temp-dir keys and never globally clear `DECISIONS`, so they can run in parallel without clobbering each other's decisions.
/// Consumed by the MCP project-scope gate tests here and in `managed_mcp`.
#[cfg(test)]
pub(crate) fn record_for_test(cwd: &Path, allowed: bool) {
    record(&workspace_key(cwd), allowed);
}

/// Resolve the trust decision for `cwd` ONCE and record it for the loaders. A cached **grant** short-circuits.
/// A cached **untrusted** verdict is re-checked against the store so a later `--trust` grant is honored without a restart. `allow_prompt` must be `true` ONLY where a blocking stdin y/N read is safe.
/// That is agent `initialize` for the launch directory, before the TUI takes over the terminal. An unresolved interactive-but-untrusted workspace therefore resolves **fail-closed** (untrusted, no prompt). Only the launch dir is ever prompted for.
pub(crate) fn resolve_and_record(
    cwd: &Path,
    remote: Option<&RemoteSettings>,
    allow_prompt: bool,
) -> bool {
    // Local/dev builds are fully inert: project scope is always allowed, so skip the `trusted_folders.toml` read entirely
    if folder_trust_inert() {
        return true;
    }
    let key = workspace_key(cwd);
    resolve_and_record_inner(
        &key,
        || distill_workspace::folder_trust::is_trusted_this_process(&key),
        || compute(cwd, &key, remote, allow_prompt),
    )
}

/// Not a `DECISIONS` entry: a later resolve on a provisional no-configs allow still re-walks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustScan {
    inputs: Option<DecideInputs>,
}

impl TrustScan {
    pub(crate) fn skipped() -> Self {
        Self { inputs: None }
    }

    pub(crate) fn is_gathered(self) -> bool {
        self.inputs.is_some()
    }
}

pub(crate) fn gather_and_record(
    cwd: &Path,
    remote: Option<&RemoteSettings>,
    allow_prompt: bool,
) -> (bool, TrustScan) {
    if folder_trust_inert() {
        return (true, TrustScan::skipped());
    }
    let key = workspace_key(cwd);
    let inputs = decide_inputs(cwd, &key);
    let allowed = resolve_and_record_inner(
        &key,
        || distill_workspace::folder_trust::is_trusted_this_process(&key),
        || compute_from_inputs(&inputs, feature_enabled(remote), &key, allow_prompt),
    );
    (
        allowed,
        TrustScan {
            inputs: Some(inputs),
        },
    )
}

/// Same `DECISIONS` contract as [`resolve_and_record`]. Reuse `scan` only for a definitive decision (configs present → trusted/untrusted).
/// A provisional no-configs allow re-walks, so a config added after gather is not a frozen grant. A missing scan falls back to a full resolve.
pub(crate) fn resolve_and_record_from_scan(
    cwd: &Path,
    remote: Option<&RemoteSettings>,
    allow_prompt: bool,
    scan: TrustScan,
) -> bool {
    if folder_trust_inert() {
        return true;
    }
    // Provisional no-configs allow is not durable; reuse would freeze that allow.
    let Some(mut inputs) = scan.inputs.filter(|inputs| inputs.repo_configs_present) else {
        return resolve_and_record(cwd, remote, allow_prompt);
    };
    let key = workspace_key(cwd);
    // A grant after the gather must be visible without another workspace walk.
    inputs.store_trusted = distill_workspace::folder_trust::is_trusted_this_process(&key);
    resolve_and_record_inner(
        &key,
        || distill_workspace::folder_trust::is_trusted_this_process(&key),
        || compute_from_inputs(&inputs, feature_enabled(remote), &key, allow_prompt),
    )
}

/// Resolve the launch dir's project-scope trust verdict with a SINGLE expensive gather, so the one-time deferred init helpers can share it.
/// This is the AUTHORITATIVE description of the launch-dir dedup and TOCTOU contract (the `MvpAgent` field/method docs point here).
/// The provisional "no repo configs" allow is non-durable (never absorbed by the `DECISIONS` cache). So without memoization the launch dir is scanned multiple times during init. TOCTOU: this records ONLY what [`resolve_and_record`] records. The init-time dedup belongs to the one-shot caller (a `OnceCell` on `MvpAgent`), NOT to any new shared-cache entry.
pub(crate) fn resolve_launch_dir_trust(cwd: &Path, remote: Option<&RemoteSettings>) -> bool {
    // Local/dev builds are fully inert: project scope is always allowed, skipping the store read and repo scan entirely
    if folder_trust_inert() {
        return true;
    }
    let key = workspace_key(cwd);
    let feature = feature_enabled(remote);
    let inputs = decide_inputs(cwd, &key);
    // Re-read the store for the cached-untrusted reconciliation EXACTLY as resolve_and_record does A `--trust` granted after a parallel resolve recorded untrusted is then still honored
    // Reuse the gathered inputs only for the recompute That keeps the DECISIONS cache contract identical to resolve_and_record without repeating the expensive repo_configs scan
    resolve_and_record_inner(
        &key,
        || distill_workspace::folder_trust::is_trusted_this_process(&key),
        || compute_from_inputs(&inputs, feature, &key, false),
    )
}

/// Cache-reconciling core of [`resolve_and_record`], split out so the invalidation path is testable without the process-global trust store.
/// A cached **grant** (`Some(true)`) is durable and short-circuits; neither `store_trusted` nor `recompute` runs.
/// A `grok --trust` grant issued AFTER this workspace was first resolved writes the store, so honor it on the next session without a restart. An **unrecorded** key (`None`) does a full `recompute`, which reports `(allowed, durable)`; the verdict is recorded ONLY when `durable`. The provisional "no repo configs" allow is non-durable, so it stays unrecorded.
fn resolve_and_record_inner(
    key: &Path,
    store_trusted: impl FnOnce() -> bool,
    recompute: impl FnOnce() -> (bool, bool),
) -> bool {
    // Copy out of the lock so `record` below can re-acquire it (parking_lot mutexes are not re-entrant)
    let cached = DECISIONS.lock().get(key).copied();
    match cached {
        Some(true) => true,
        Some(false) => {
            if store_trusted() {
                record(key, true);
                true
            } else {
                false
            }
        }
        None => {
            // Record only durable verdicts; a provisional no-configs allow is left uncached so the next resolve re-checks `repo_configs_present`
            let (allowed, durable) = recompute();
            if durable {
                record(key, allowed);
            }
            allowed
        }
    }
}

/// Two allows are NON-durable. (1) The "no repo configs" allow: repo-local code-exec config can appear after this resolve (git pull / agent write).
/// Caching that provisional grant would let a later `/hooks reload` or new session run the new code with no trust decision (TOCTOU).
/// (2) The unrecordable-key allow (cwd is $HOME / fs-root), which the store can never persist anyway. Store-trusted, feature-off, and an accepted prompt are durable. An untrusted verdict is recorded so a later `--trust` grant can reconcile it (see [`resolve_and_record_inner`]).
fn compute(
    cwd: &Path,
    key: &Path,
    remote: Option<&RemoteSettings>,
    allow_prompt: bool,
) -> (bool, bool) {
    let feature = feature_enabled(remote);
    let inputs = decide_inputs(cwd, key);
    compute_from_inputs(&inputs, feature, key, allow_prompt)
}

/// [`compute`] split at the gather: derive `(allowed, durable)` from an already-gathered [`DecideInputs`].
/// A caller needing more than one verdict (see [`resolve_launch_dir_trust`]) then pays for the expensive `decide_inputs` gather only ONCE.
/// That gather is the store read plus the `repo_configs_present` scan.
fn compute_from_inputs(
    inputs: &DecideInputs,
    feature: bool,
    key: &Path,
    allow_prompt: bool,
) -> (bool, bool) {
    match decide(feature, inputs) {
        TrustOutcome::Trusted => {
            // Within the Trusted arm the non-durable ("provisional") allows are the "no repo configs" rule and the unrecordable-key rule The latter is Case 2: cwd is $HOME / fs-root, which can never be persisted
            // Both are feature-on and not store-trusted; feature-off and store-trusted are durable Leave the non-durable allows uncached.
            let durable = !feature || inputs.store_trusted;
            (true, durable)
        }
        TrustOutcome::Prompt if allow_prompt => {
            if !prompt_for_trust(key) {
                return (false, true);
            }
            // Unreadable store is Refused and must not be reported durable-trusted.
            // ProcessLocalOnly is not a durable grant and must not dismiss the gate.
            let outcome = grant_folder_trust_key(key);
            if outcome.dismisses_gate() {
                (true, true)
            } else {
                match outcome {
                    GrantOutcome::Granted {
                        persist: PersistStatus::ProcessLocalOnly { error },
                        ..
                    } => {
                        tracing::warn!(
                            error = %error,
                            "folder trust: grant is process-local only and will not survive restart"
                        );
                        (false, false)
                    }
                    GrantOutcome::Refused { .. }
                    | GrantOutcome::Granted { .. }
                    | GrantOutcome::AlreadyDurable { .. } => (false, true),
                }
            }
        }
        // Untrusted, OR interactive where prompting is unsafe here (TUI owns stdin); the agent-`initialize` path owns the launch-dir prompt
        // Both resolve fail-closed
        TrustOutcome::Untrusted | TrustOutcome::Prompt => (false, true),
    }
}

/// It MUST enumerate every project MCP source the loaders read. Name-based (not `ConfigSource`-based) ON PURPOSE. Sources: project `.grok/config.toml [mcp_servers]` (NOT the user-tier global config).
/// Also project `.mcp.json` (`cwd` up to the repo root, never `$HOME`), project `.cursor/mcp.json`, and `~/.claude.json projects.<cwd>.mcpServers`.
/// Edge case: a name declared in BOTH a project config and the global `~/.grok/config.toml` is dropped when untrusted. This is intended — untrusted project content must not influence the command spawned for a shared name.
pub(crate) fn project_scoped_mcp_names(cwd: &Path) -> HashSet<String> {
    let mut names = HashSet::new();

    // `.grok/config.toml [mcp_servers]` entries tagged project (the loader's key is the display name, matching `mcp_server_name` of the merged server)
    for (name, (_cfg, scope)) in crate::util::config::load_mcp_server_configs_with_project(cwd) {
        if scope == MCP_SCOPE_PROJECT {
            names.insert(name);
        }
    }

    // Project `.mcp.json` (repo-root-down; this loader never reads $HOME).
    for server in crate::util::config::load_mcp_json_servers(cwd) {
        names.insert(mcp_server_name(&server).to_string());
    }

    // Project `.cursor/mcp.json` only
    // `load_cursor_mcp_servers` would also merge the user-scoped `~/.cursor/mcp.json`, so read the project file directly instead
    for server in crate::util::config::load_mcp_json_file(&cwd.join(".cursor").join("mcp.json")) {
        names.insert(mcp_server_name(&server).to_string());
    }

    // `~/.claude.json projects.<cwd>.mcpServers` object keys.
    if let Some(claude_names) = claude_project_mcp_names(cwd) {
        names.extend(claude_names);
    }

    names
}

/// Drop repo-local (project-scoped) MCP servers from a merged server list when `cwd`'s workspace is untrusted. No-op when project scope is allowed. Mirrors [`filter_untrusted_project_lsp`].
/// Matches on display name ([`mcp_server_name`]) rather than the URL/key, so a project server is dropped regardless of transport. That name is the same identity the merge dedups and the disabled/allowlist filters use.
/// A server from ANY tier (client/plugin/user/managed) whose name COLLIDES with a project-declared name is ALSO dropped when untrusted. An untrusted repo must not influence the command spawned for that name (see [`project_scoped_mcp_names`]).
pub(crate) fn filter_untrusted_project_mcp(
    cwd: &Path,
    merged: Vec<acp::McpServer>,
) -> Vec<acp::McpServer> {
    let project = project_scoped_mcp_names(cwd);
    filter_untrusted_project_mcp_with(cwd, merged, &project, |s| mcp_server_name(s))
}

/// [`filter_untrusted_project_mcp`] with a precomputed name set, generic over the carried item so
/// the merge and discovery can filter `(server, subject)` pairs; `name_of` yields the merge identity.
pub(crate) fn filter_untrusted_project_mcp_with<T>(
    cwd: &Path,
    merged: Vec<T>,
    project: &HashSet<String>,
    name_of: impl Fn(&T) -> &str,
) -> Vec<T> {
    if project_scope_allowed(cwd) {
        return merged;
    }
    merged
        .into_iter()
        .filter(|item| {
            let name = name_of(item);
            if project.contains(name) {
                tracing::warn!(
                    server = %name,
                    "folder untrusted: skipping repo-local (project-scoped) MCP server"
                );
                false
            } else {
                true
            }
        })
        .collect()
}

/// Drop repo-local (project-scoped) LSP servers from a sourced LSP map when `cwd`'s workspace is untrusted. Mirrors the MCP session gate; user- and plugin-scoped servers are retained. No-op when project scope is allowed.
/// Thin cwd-to-verdict wrapper over the shared [`distill_tools::implementations::lsp::config::filter_project_lsp_when_untrusted`] predicate. Site B and the workspace build path therefore share one gate.
pub(crate) fn filter_untrusted_project_lsp(
    cwd: &Path,
    sourced: std::collections::BTreeMap<
        String,
        (
            distill_tools::implementations::lsp::config::LspServerConfig,
            distill_tools::types::config_source::ConfigSource,
        ),
    >,
) -> std::collections::BTreeMap<String, distill_tools::implementations::lsp::config::LspServerConfig>
{
    distill_tools::implementations::lsp::config::filter_project_lsp_when_untrusted(
        sourced,
        project_scope_allowed(cwd),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Used only by tests. Imported here (not at module scope) so the non-test
    // build doesn't carry unused imports (clippy `-D unused-imports`).
    use distill_workspace::folder_trust::repo_configs_present;
    use distill_workspace::trust::TrustStore;

    /// A `git init`'d temp dir bounds `find_mcp_json_files` / `find_project_configs` to the temp dir.
    /// Those discover the enclosing repo and walk to its root, so they would otherwise reach any ancestor repo the system temp dir lives in.
    fn repo_tmp() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        tmp
    }

    /// Simulate a release-stamped build so the folder-trust gate engages: an unstamped local/dev build auto-trusts and never gates/persists.
    /// Hold the returned guard for the test body (drop restores the prior value).
    fn simulate_release_build() -> EnvGuard {
        EnvGuard::set(distill_version::TEST_VERSION_ENV, "0.0.0-sim")
    }

    #[test]
    fn record_and_lookup_round_trip() {
        // `repo_tmp` git-inits the dir so `workspace_key` yields a unique key
        // It returns the git-repo root, which would otherwise collapse to a shared ambient root if `$TMPDIR` is inside a checkout
        // The `DECISIONS` map is process-global, so unique keys keep parallel tests from clobbering each other's recorded decisions
        let tmp = repo_tmp();
        let key = tmp.path().to_path_buf();
        // A fresh, never-recorded key re-resolves fail-closed and is allowed here (inert local build / no repo configs, never a durable default-open)
        assert!(project_scope_allowed(&key));
        record(&workspace_key(&key), false);
        assert!(!project_scope_allowed(&key));
    }

    #[test]
    #[serial_test::serial]
    fn grant_folder_trust_seeds_decisions_cache() {
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        grant_folder_trust(tmp.path());
        let key = workspace_key(tmp.path());
        assert!(
            distill_workspace::folder_trust::is_trusted_this_process(&key),
            "explicit grant must be visible to consume-side resolution"
        );
    }

    #[test]
    #[serial_test::serial]
    fn revoke_folder_trust_downgrades_cache() {
        let _sim = simulate_release_build();
        // A mid-session untrust of a TRUSTED folder must take effect immediately Revoke downgrades the in-process cache so `project_scope_allowed` flips to false at once
        // A cached grant would otherwise short-circuit `resolve_and_record` Seed the trust store so `was_trusted` is genuinely true
        // GROK_HOME-isolated so the seed can't touch the real user file, and `#[serial]` because GROK_HOME is process-global
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        record(&workspace_key(tmp.path()), true);
        assert!(project_scope_allowed(tmp.path()));
        assert!(
            revoke_folder_trust(tmp.path()),
            "a store-trusted folder must report was_trusted=true"
        );
        assert!(
            !project_scope_allowed(tmp.path()),
            "cache must be downgraded so untrust is immediate"
        );
    }

    #[test]
    #[serial_test::serial]
    fn revoke_never_trusted_folder_writes_no_deny() {
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();

        record(&workspace_key(tmp.path()), true);

        assert!(
            !revoke_folder_trust(tmp.path()),
            "revoking a never-trusted folder must return false"
        );

        assert!(
            !project_scope_allowed(tmp.path()),
            "revoke must downgrade the in-process cache even for a never-trusted folder"
        );

        let store = TrustStore::load();
        assert!(
            !store.has_decision(&workspace_key(tmp.path())),
            "revoking a never-trusted folder must not record a child deny"
        );
    }

    #[test]
    #[serial_test::serial]
    fn revoke_on_unrecordable_home_root_records_no_deny() {
        let _sim = simulate_release_build();
        // cwd == $HOME (git-inited so `workspace_key` resolves the home root, which the store refuses to record)
        // Revoke must NOT seed a cache deny decide() always trusts an unrecordable root and no grant/store/prompt could ever lift the deny The gate must therefore keep allowing after an untrust click
        // HOME overridden so workspace_key sees the tempdir as home; GROK_HOME-isolated store GROK_FOLDER_TRUST unset so the default-on flag applies.
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("HOME", home.path());
        let distill_home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", distill_home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        git2::Repository::init(home.path()).unwrap();
        // Repo-local code-exec config, so the final allow is the unrecordable-key rule at work
        // A recordable key with configs and an empty store would deny
        std::fs::create_dir_all(home.path().join(".grok").join("hooks")).unwrap();

        assert!(
            !revoke_folder_trust(home.path()),
            "an unrecordable root reports was_trusted=false"
        );
        assert!(
            DECISIONS.lock().get(&workspace_key(home.path())).is_none(),
            "revoke must record no cache deny for an unrecordable root"
        );
        assert!(
            project_scope_allowed(home.path()),
            "$HOME must stay allowed after a revoke attempt"
        );
    }

    #[test]
    #[serial_test::serial]
    fn envrc_gate_drops_untrusted_then_loads_when_store_trusted() {
        let _sim = simulate_release_build();
        // The `.envrc` load sites gate on the folder-trust verdict An `.envrc`-only untrusted clone resolves false (so the call site loads an empty env) A store-trusted folder resolves true and the loader actually reads `.envrc`
        // GROK_HOME-isolated so the trust store is empty; GROK_FOLDER_TRUST unset so the default-on feature flag applies
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".envrc"), "export GATED_ENVRC=1\n").unwrap();

        // Untrusted: the gate verdict is false, so the call site skips loading
        assert!(
            !resolve_and_record(tmp.path(), None, false),
            "untrusted `.envrc` clone must gate the load"
        );

        // Store-trust the folder: the verdict reconciles to true and the loader runs
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        assert!(resolve_and_record(tmp.path(), None, false));
        let env = distill_workspace::envrc::load_envrc_or_empty(tmp.path());
        assert_eq!(
            env.get("GATED_ENVRC"),
            Some(&"1".to_string()),
            "trusted folder must load `.envrc`"
        );
    }

    #[test]
    #[serial_test::serial]
    fn claude_env_gate_drops_project_env_when_untrusted() {
        let _sim = simulate_release_build();
        // The `.claude/settings.json` env load site mirrors `load_claude_env_with_project(cwd, project_scope_allowed(cwd))`
        // An untrusted clone's repo-tree env (which would feed BASH_ENV / GIT_SSH_COMMAND / ... to every subprocess) is dropped. A store-trusted folder merges it
        // GROK_HOME-isolated so the trust store is empty; GROK_FOLDER_TRUST unset so the default-on feature flag applies
        use distill_workspace::permission::claude_settings::load_claude_env_with_project;
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"env": {"REPO_TREE_ENV_GATED": "1"}}"#,
        )
        .unwrap();

        // Untrusted: verdict false, so the repo-tree env is dropped
        assert!(!resolve_and_record(tmp.path(), None, false));
        let untrusted =
            load_claude_env_with_project(tmp.path(), resolve_and_record(tmp.path(), None, false));
        assert!(
            !untrusted.contains_key("REPO_TREE_ENV_GATED"),
            "untrusted folder must drop repo-tree .claude env"
        );

        // Store-trust: the verdict reconciles to true and the repo-tree env is merged
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        assert!(resolve_and_record(tmp.path(), None, false));
        let trusted =
            load_claude_env_with_project(tmp.path(), resolve_and_record(tmp.path(), None, false));
        assert_eq!(
            trusted.get("REPO_TREE_ENV_GATED"),
            Some(&"1".to_string()),
            "trusted folder must merge repo-tree .claude env"
        );
    }

    #[test]
    #[serial_test::serial]
    fn claude_env_gate_drops_subdir_project_env_when_untrusted() {
        let _sim = simulate_release_build();
        // RCE regression (subdir bypass) A `.claude/settings.json` with `env` in a SUBDIR, the ONLY repo config, launched from that subdir must flip the folder untrusted Its env must also be dropped
        // The env loader walks cwd to repo-root, so detection MUST walk too (a git-root-only probe missed this) GROK_HOME-isolated so the trust store is empty
        use distill_workspace::permission::claude_settings::load_claude_env_with_project;
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let subdir = tmp.path().join("sub");
        let claude = subdir.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"env": {"SUBDIR_REPO_ENV_GATED": "1"}}"#,
        )
        .unwrap();

        // Detection walks cwd to root, so the subdir-only `.claude` is detected and the folder resolves untrusted
        assert!(
            repo_configs_present(&subdir),
            "subdir .claude/settings.json must be detected"
        );
        assert!(!resolve_and_record(&subdir, None, false));
        let untrusted =
            load_claude_env_with_project(&subdir, resolve_and_record(&subdir, None, false));
        assert!(
            !untrusted.contains_key("SUBDIR_REPO_ENV_GATED"),
            "untrusted subdir folder must drop its repo-tree .claude env"
        );

        // Granting trust re-enables it (proves the env file is real/loadable).
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(&subdir)).unwrap();
        assert!(resolve_and_record(&subdir, None, false));
        let trusted =
            load_claude_env_with_project(&subdir, resolve_and_record(&subdir, None, false));
        assert_eq!(
            trusted.get("SUBDIR_REPO_ENV_GATED"),
            Some(&"1".to_string()),
            "trusted folder must merge the subdir repo-tree .claude env"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_agent_inline_hooks_gated_when_untrusted_but_user_kept() {
        let _sim = simulate_release_build();
        // A cwd-discovered PROJECT agent's inline `hooks:` is gated on folder-trust (it can SHADOW a built-in subagent, near-auto RCE) A user/built-in agent's hooks are kept
        // Exercises real discovery and the exact call-site predicate used at mvp_agent/subagent GROK_HOME-isolated (empty store)
        use distill_agent::config::AgentScope;
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let agents = tmp.path().join(".grok").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // Shadows the built-in `explore` subagent and carries a command hook.
        std::fs::write(
            agents.join("explore.md"),
            "---\nname: explore\ndescription: x\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: \"true\"\n---\nbody\n",
        )
        .unwrap();
        let def = distill_agent::discovery::by_name_in_cwd("explore", tmp.path())
            .expect("project agent must be discovered");
        assert_eq!(def.scope, AgentScope::Project);
        assert!(def.hooks.is_some(), "project agent must carry inline hooks");

        // Exercise the REAL shared predicate both append sites use (not a copy).
        let allowed = |scope: AgentScope| {
            agent_inline_hooks_allowed(scope, || project_scope_allowed(tmp.path()))
        };

        // Untrusted: the project agent's inline hooks are dropped...
        assert!(
            !allowed(def.scope),
            "untrusted project agent inline hooks must be gated"
        );
        // ...while user/built-in agents (not cwd-sourced) keep their hooks.
        assert!(allowed(AgentScope::User));
        assert!(allowed(AgentScope::BuiltIn));

        // Grant trust and reconcile the cache (as the post-grant reload flow does): the project agent's inline hooks are appended
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        resolve_and_record(tmp.path(), None, false);
        assert!(
            allowed(def.scope),
            "trusted project agent inline hooks must be appended"
        );
    }

    use distill_test_support::EnvGuard;

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_denies_untrusted_repo_with_configs() {
        // Fail-closed (the dangerous case) The setup: a release-stamped build, the feature on by default, an untrusted folder shipping code-exec config (here `.grok/hooks`) With no store grant that must be DENIED
        // That holds even though no verdict was recorded first (the gate re-resolves fail-closed rather than defaulting open) GROK_HOME-isolated (empty store); GROK_FOLDER_TRUST unset so the default-on flag applies
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        assert!(
            !project_scope_allowed(tmp.path()),
            "untrusted folder with repo configs must be denied (fail-closed)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_allows_repo_without_configs() {
        // The over-deny guard: a folder with NO repo-local code-exec config has nothing to gate It must be ALLOWED even though its (provisional) Trusted verdict is never cached
        // A naive `.unwrap_or(false)` cache peek would wrongly deny it Release-stamped and GROK_HOME-isolated so the verdict comes from `decide` rule 4 (no repo configs), not the inert short-circuit
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        assert!(
            project_scope_allowed(tmp.path()),
            "a folder with no repo configs must be allowed (no over-deny)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_allows_store_trusted_repo() {
        // A folder the user explicitly trusted is ALLOWED even with repo-local configs present
        // GROK_HOME-isolated so the seeded store is the temp one; GROK_FOLDER_TRUST unset so the default-on flag applies
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        assert!(
            project_scope_allowed(tmp.path()),
            "a store-trusted folder must be allowed"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_allows_inert_local_build() {
        // On a local/dev build the whole feature is inert (auto-trust): a folder with repo-local configs and an empty store is still ALLOWED
        // Assert only when compiled unstamped (mirrors the inert tests elsewhere)
        // GROK_TEST_VERSION is unset so `is_local_build()` is genuinely true
        let _unset_ver = EnvGuard::unset(distill_version::TEST_VERSION_ENV);
        if option_env!("GROK_VERSION").is_some() {
            return; // a release-stamped test binary is not a local build
        }
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        assert!(
            project_scope_allowed(tmp.path()),
            "inert local/dev build must allow project scope even with configs"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_denies_untrusted_plugin_only_repo() {
        // A plugin-only untrusted repo (just `.grok/plugins/<x>/`, no hooks/MCP/LSP, no store grant) is repo-controlled code-exec It must be DENIED
        // That is the verdict the shell plugin call sites feed into discover_plugins/build_for_cwd/reload GROK_HOME-isolated (empty store); GROK_FOLDER_TRUST unset so the default-on flag applies
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("plugins").join("evil")).unwrap();
        assert!(
            !project_scope_allowed(tmp.path()),
            "plugin-only untrusted repo must be denied"
        );
    }

    #[test]
    #[serial_test::serial]
    fn project_scope_allowed_denies_untrusted_permission_only_repo() {
        // Bridge: a clone whose ONLY repo-local config is `.grok/config.toml` `[permission]` (no MCP/hooks/plugins) must still gate
        // It produces untrusted via the real `repo_configs_present` / `decide` / `project_scope_allowed` path Resolver unit tests inject `project_trusted = false` directly and miss this detector gap
        // Subdir launch exercises the cwd-to-git-root walk
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("config.toml"),
            "[permission]\nallow = [\"Bash(*)\"]\n",
        )
        .unwrap();
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(
            !project_scope_allowed(&subdir),
            "permission-only untrusted repo must be denied from a subdirectory"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn project_scope_allowed_denies_workspace_user_only_instructions() {
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let workspace_user = tmp.path().join("x/alice");
        std::fs::create_dir_all(&workspace_user).unwrap();
        std::fs::write(
            workspace_user.join("AGENTS.md"),
            "workspace-user-only-instructions",
        )
        .unwrap();
        let skill_dir = workspace_user.join(".grok/skills/workspace-user-only-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: workspace-user-only-skill\ndescription: test\n---\n",
        )
        .unwrap();
        let _root = EnvGuard::set("XAI_ROOT", tmp.path().as_os_str());
        let _user = EnvGuard::set("XAI_USER", "alice");

        let cwd = tmp.path().to_string_lossy().into_owned();
        let verdict = project_scope_allowed(tmp.path());
        assert!(!verdict, "workspace-user-only markers must trigger trust");
        let instructions = distill_agent::prompt::agents_md::read_agents_config_with_paths(
            &cwd,
            distill_agent::prompt::skills::CompatConfig::default(),
            &distill_agent::prompt::paths::PathsConfig::default(),
            verdict,
        )
        .await;
        let skills = distill_agent::prompt::skills::list_skills(
            Some(&cwd),
            &distill_agent::prompt::skills::SkillsConfig::default(),
            distill_agent::prompt::skills::CompatConfig::default(),
            verdict,
        )
        .await;
        assert!(
            instructions
                .iter()
                .all(|config| config.content != "workspace-user-only-instructions")
        );
        assert!(
            skills
                .iter()
                .all(|skill| skill.name != "workspace-user-only-skill")
        );

        grant_folder_trust(tmp.path());
        let verdict = project_scope_allowed(tmp.path());
        assert!(verdict);
        let instructions = distill_agent::prompt::agents_md::read_agents_config_with_paths(
            &cwd,
            distill_agent::prompt::skills::CompatConfig::default(),
            &distill_agent::prompt::paths::PathsConfig::default(),
            verdict,
        )
        .await;
        let skills = distill_agent::prompt::skills::list_skills(
            Some(&cwd),
            &distill_agent::prompt::skills::SkillsConfig::default(),
            distill_agent::prompt::skills::CompatConfig::default(),
            verdict,
        )
        .await;
        assert!(
            instructions
                .iter()
                .any(|config| config.content == "workspace-user-only-instructions")
        );
        assert!(
            skills
                .iter()
                .any(|skill| skill.name == "workspace-user-only-skill")
        );
    }

    #[test]
    #[serial_test::serial]
    fn kill_switch_allows_untrusted_repo_after_authoritative_resolve() {
        // Regression (chat/load-path kill-switch) An untrusted folder WITH repo configs under a remote kill-switch (folder_trust_enabled = Some(false)) must resolve ALLOWED
        // The session spawn path resolves once with the real RemoteSettings before any gate read, so the gate cache-hits that verdict GROK_HOME-isolated (empty store); GROK_FOLDER_TRUST unset so the kill-switch is the only signal
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let remote = RemoteSettings {
            folder_trust_enabled: Some(false),
            ..Default::default()
        };

        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        assert!(
            resolve_and_record(tmp.path(), Some(&remote), false),
            "kill-switch (feature off) must resolve trusted even with repo configs"
        );
        assert!(
            project_scope_allowed(tmp.path()),
            "gate must allow a kill-switched folder once the authoritative resolve ran"
        );

        // Contrast: a cold `remote = None` gate read (no prior authoritative resolve) misses the kill-switch and denies the same scenario
        // That is the exact gap the up-front spawn resolve closes for chat/load sessions
        let cold = repo_tmp();
        std::fs::create_dir_all(cold.path().join(".grok").join("hooks")).unwrap();
        assert!(
            !project_scope_allowed(cold.path()),
            "cold remote=None gate read denies a kill-switched folder (regression contrast)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_for_cwd_with_trust_verdict_gates_active_project_plugin() {
        let _sim = simulate_release_build();
        // Pins the SHELL plugin wiring end-to-end The call-site expression is `build_for_cwd(cwd, &cfg, dirs, <folder-trust verdict>)` It must keep an ENABLED project plugin OUT of `active_plugins()` while the folder is untrusted
        // It must let the plugin in after `grant_folder_trust` The verdict/discovery/registry unit tests alone do NOT catch a silent un-gating here
        // GROK_HOME-isolated so both the folder-trust store and the plugin trust store start empty (deterministic untrusted) GROK_FOLDER_TRUST unset so the default-on flag applies; `#[serial]` because both are process-global
        use distill_agent::plugins::discovery::DiscoveryConfig;
        use distill_agent::plugins::{PluginRegistry, SharedPluginRegistryHandle};
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        // A project plugin
        // Project scope is default-disabled, so name it in the `enabled` list to isolate the TRUST gate (not the enable gate)
        let plugin = tmp.path().join(".grok").join("plugins").join("trustgate");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), r#"{"name":"trustgate"}"#).unwrap();
        let cfg = DiscoveryConfig {
            enabled: vec!["trustgate".to_string()],
            ..Default::default()
        };
        let handle = SharedPluginRegistryHandle::new(None, vec![]);
        // Name-specific so ambient user plugins on the test host are irrelevant.
        let active_has = |reg: Option<std::sync::Arc<PluginRegistry>>| {
            reg.is_some_and(|r| r.active_plugins().iter().any(|p| p.name == "trustgate"))
        };

        // Untrusted: the verdict is false, so the plugin is still discovered but not trusted, and absent from `active_plugins()`
        let untrusted = handle.build_for_cwd(
            tmp.path(),
            &cfg,
            &[],
            resolve_and_record(tmp.path(), None, false),
        );
        assert!(
            !active_has(untrusted),
            "untrusted folder: enabled project plugin must be gated out of active_plugins()"
        );

        // Grant trust and recompute the same expression: the plugin becomes active
        grant_folder_trust(tmp.path());
        let trusted = handle.build_for_cwd(
            tmp.path(),
            &cfg,
            &[],
            resolve_and_record(tmp.path(), None, false),
        );
        assert!(
            active_has(trusted),
            "granted folder: enabled project plugin must appear in active_plugins()"
        );
    }

    #[test]
    #[serial_test::serial]
    fn discover_hooks_gates_then_loads_project_hook_via_trust_verdict() {
        let _sim = simulate_release_build();
        // End-to-end load path: the folder-trust verdict threaded into `discover_hooks` excludes a repo-local project hook while untrusted
        // It includes the hook after the folder is granted trust, the path where the regression historically re-opened
        // GROK_HOME-isolated so the grant writes to a temp store; GROK_FOLDER_TRUST unset so the default-on flag applies
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();
        let hooks_dir = tmp.path().join(".grok").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        // Top-level `{"hooks":{...}}` wrapper; no matcher means match-all
        // The parsed spec name is `<file_stem>:PreToolUse[..]`, so the file stem identifies it
        std::fs::write(
            hooks_dir.join("trust_load_gate.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .unwrap();
        // Discovery prefixes project specs with `project/` and parse names them `<file_stem>:<event>[..]`, so the unique stem appears mid-name
        // `contains` matches it without coupling to the full name format.
        let has_project_hook = |reg: &distill_hooks::discovery::HookRegistry| {
            reg.all_hooks()
                .iter()
                .any(|h| h.name.contains("trust_load_gate"))
        };

        // Untrusted: verdict false, so discovery omits the project hook
        let untrusted = resolve_and_record(tmp.path(), None, false);
        assert!(!untrusted, "untrusted repo must resolve the gate false");
        // Mirror the production startup/reload path via the single load entry point.
        let git_root = distill_workspace::session::git::find_git_root_from_path(tmp.path()).ok();
        let (reg, _errs) = crate::util::hooks::discover_hooks(
            git_root.as_deref(),
            &distill_tools::types::compat::CompatConfig::default(),
            untrusted,
        );
        assert!(
            !has_project_hook(&reg),
            "untrusted: repo-local project hook must be gated out of discovery"
        );

        // Grant trust and re-resolve: discovery loads the project hook
        grant_folder_trust(tmp.path());
        let trusted = resolve_and_record(tmp.path(), None, false);
        assert!(trusted, "granted repo must resolve the gate true");
        let (reg, _errs) = crate::util::hooks::discover_hooks(
            git_root.as_deref(),
            &distill_tools::types::compat::CompatConfig::default(),
            trusted,
        );
        assert!(
            has_project_hook(&reg),
            "trusted: repo-local project hook must load"
        );
    }

    #[test]
    fn filter_untrusted_project_lsp_drops_only_project() {
        use distill_tools::implementations::lsp::config::LspServerConfig;
        use distill_tools::types::config_source::ConfigSource;
        use std::collections::BTreeMap;

        fn sourced() -> BTreeMap<String, (LspServerConfig, ConfigSource)> {
            let mut m = BTreeMap::new();
            m.insert(
                "proj".to_string(),
                (
                    LspServerConfig::default(),
                    ConfigSource::Project {
                        path: PathBuf::from("/repo/.grok/lsp.json"),
                    },
                ),
            );
            m.insert(
                "usr".to_string(),
                (
                    LspServerConfig::default(),
                    ConfigSource::User {
                        path: PathBuf::from("/home/.grok/lsp.json"),
                    },
                ),
            );
            m
        }

        // Untrusted workspace: only the project-scoped server is dropped.
        // `repo_tmp` git-inits so `workspace_key` yields a unique per-dir key even when `$TMPDIR` itself lives inside a git checkout
        let untrusted = repo_tmp();
        record_for_test(untrusted.path(), false);
        let kept = filter_untrusted_project_lsp(untrusted.path(), sourced());
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key("usr"));
        assert!(!kept.contains_key("proj"));

        // Trusted workspace (a different temp-dir key): both are retained.
        let trusted = repo_tmp();
        record_for_test(trusted.path(), true);
        let kept = filter_untrusted_project_lsp(trusted.path(), sourced());
        assert_eq!(kept.len(), 2);
        assert!(kept.contains_key("usr"));
        assert!(kept.contains_key("proj"));
    }

    #[test]
    fn load_servers_sourced_tags_project_lsp_json() {
        use distill_tools::implementations::lsp::config::load_servers_with_plugins_sourced;
        use distill_tools::types::config_source::ConfigSource;

        // A `<cwd>/.grok/lsp.json` server must be tagged `Project` so the gate can distinguish it from user/plugin servers
        // Asserts on the specific
        // key, so any real `~/.grok/lsp.json` on the test host is irrelevant.
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("lsp.json"), r#"{"projlsp": {"command": "true"}}"#).unwrap();

        let sourced = load_servers_with_plugins_sourced(tmp.path(), &[], &[], &[], &[]);
        let (_, source) = sourced.get("projlsp").expect("project server present");
        assert!(
            matches!(source, ConfigSource::Project { .. }),
            "project lsp.json must be tagged Project; got {source:?}"
        );
    }

    #[test]
    fn untrusted_workspace_drops_loaded_project_lsp() {
        use distill_tools::implementations::lsp::config::load_servers_with_plugins_sourced;

        // End-to-end of the load-site gate (Sites A/B)
        // A project server loaded from `<cwd>/.grok/lsp.json` is dropped once the workspace is untrusted
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("lsp.json"), r#"{"projlsp": {"command": "true"}}"#).unwrap();

        let sourced = load_servers_with_plugins_sourced(tmp.path(), &[], &[], &[], &[]);
        assert!(
            sourced.contains_key("projlsp"),
            "loader must see the project server"
        );

        record_for_test(tmp.path(), false);
        let kept = filter_untrusted_project_lsp(tmp.path(), sourced);
        assert!(
            !kept.contains_key("projlsp"),
            "untrusted folder must drop the project-scoped LSP server"
        );
    }

    fn http(name: &str) -> acp::McpServer {
        acp::McpServer::Http(
            acp::McpServerHttp::new(name.to_string(), format!("https://example.com/{name}"))
                .headers(vec![]),
        )
    }

    /// A git-init'd repo declaring two project-scoped MCP servers: `projjson` (`.mcp.json`) and `projtoml` (`.grok/config.toml [mcp_servers]`).
    fn repo_with_project_mcp() -> tempfile::TempDir {
        let tmp = repo_tmp();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers": {"projjson": {"url": "https://proj.example.com/mcp"}}}"#,
        )
        .unwrap();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("config.toml"),
            "[mcp_servers.projtoml]\nurl = \"https://projtoml.example.com/mcp\"\n",
        )
        .unwrap();
        tmp
    }

    /// Pins the three known repo-local FILE sources of [`project_scoped_mcp_names`]. A project server declared in each of `.grok/config.toml`, `.mcp.json`, and `.cursor/mcp.json` must appear in the returned set.
    /// That catches a REGRESSION that drops one of them. It cannot catch a brand-new source TYPE added only to a loader; the single-source-of-truth doc on `project_scoped_mcp_names` is that guard.
    /// `~/.claude.json` is excluded: it lives under `$HOME` and a test must not clobber the real user file; its keys are covered by the shared reader.
    #[test]
    fn project_scoped_mcp_names_cover_every_source() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("config.toml"),
            "[mcp_servers.cfgsrv]\nurl = \"https://cfg.example.com/mcp\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers": {"jsonsrv": {"url": "https://json.example.com/mcp"}}}"#,
        )
        .unwrap();
        let cursor = tmp.path().join(".cursor");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(
            cursor.join("mcp.json"),
            r#"{"mcpServers": {"cursorsrv": {"url": "https://cursor.example.com/mcp"}}}"#,
        )
        .unwrap();

        let names = project_scoped_mcp_names(tmp.path());
        for expected in ["cfgsrv", "jsonsrv", "cursorsrv"] {
            assert!(
                names.contains(expected),
                "project_scoped_mcp_names missing {expected:?} — a project MCP source is no longer gated; got {names:?}"
            );
        }
    }

    #[test]
    fn filter_untrusted_project_mcp_drops_only_project() {
        let merged = || {
            vec![
                http("projjson"),
                http("projtoml"),
                http("client"),
                http("global"),
            ]
        };

        // Untrusted: both project-declared servers are dropped
        // A client-supplied and a user/global server (neither in a project config) are retained
        let untrusted = repo_with_project_mcp();
        record_for_test(untrusted.path(), false);
        let kept = filter_untrusted_project_mcp(untrusted.path(), merged());
        let names: HashSet<&str> = kept.iter().map(mcp_server_name).collect();
        assert_eq!(names, ["client", "global"].into_iter().collect());

        // Trusted: every server is retained even with project configs present.
        let trusted = repo_with_project_mcp();
        record_for_test(trusted.path(), true);
        let kept = filter_untrusted_project_mcp(trusted.path(), merged());
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn cached_grant_short_circuits_without_store_read() {
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());
        record(&key, true);
        // A cached grant must consult neither the store nor a recompute.
        let allowed = resolve_and_record_inner(
            &key,
            || panic!("store must not be read for a cached grant"),
            || panic!("must not recompute for a cached grant"),
        );
        assert!(allowed);
    }

    #[test]
    fn cached_untrusted_is_upgraded_by_a_later_grant() {
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());
        record(&key, false);
        assert!(!project_scope_allowed(tmp.path()));
        // Simulate a `grok --trust` grant landing in the store after the untrusted verdict was cached
        // The re-read sees trusted, so the next resolve upgrades the cache without a process restart
        let allowed = resolve_and_record_inner(
            &key,
            || true,
            || panic!("must not recompute when reconciling from the store"),
        );
        assert!(allowed);
        // The cheap gate the merge consults now allows (cache refreshed).
        assert!(project_scope_allowed(tmp.path()));
    }

    #[test]
    fn cached_untrusted_stays_untrusted_when_store_still_denies() {
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());
        record(&key, false);
        let allowed =
            resolve_and_record_inner(&key, || false, || panic!("must not recompute when cached"));
        assert!(!allowed);
        assert!(!project_scope_allowed(tmp.path()));
    }

    #[test]
    fn unrecorded_key_recomputes_then_caches() {
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());
        // No prior record: recompute and record the (durable untrusted) verdict
        let allowed = resolve_and_record_inner(
            &key,
            || panic!("store re-read is only for a cached untrusted verdict"),
            || (false, true),
        );
        assert!(!allowed);
        assert!(!project_scope_allowed(tmp.path()));
        // Now cached untrusted: a later resolve re-reads the store (still denies).
        let allowed =
            resolve_and_record_inner(&key, || false, || panic!("must not recompute when cached"));
        assert!(!allowed);
    }

    #[test]
    fn explicit_child_untrust_survives_reload_despite_ancestor_trust() {
        // Untrust-undone-on-reload: an ancestor is trusted and the child is explicitly untrusted, so a reload must NOT re-promote the child revoke downgrades the cache to untrusted
        // The reconcile re-reads the store, which honors the most-specific (child) decision and stays untrusted
        // The ancestor's cascade no longer undoes the explicit untrust
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join(distill_workspace::trust::TRUST_FILE_NAME);
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let mut store = TrustStore::load_from(store_path);
        store.set_trusted(&parent).unwrap();
        store.set_untrusted(&child).unwrap();

        // revoke downgraded the in-process cache for the child.
        record(&child, false);

        let allowed = resolve_and_record_inner(
            &child,
            || store.is_trusted(&child),
            || panic!("must not recompute when reconciling a cached untrusted verdict"),
        );
        assert!(
            !allowed,
            "explicit child untrust must survive the ancestor's trust on reload"
        );
    }

    #[test]
    #[serial_test::serial]
    fn no_configs_trust_is_provisional_and_regated_when_configs_appear() {
        // F5 regression: the "no repo configs means Trusted" verdict is PROVISIONAL, so it must NOT be cached as a durable grant
        // Otherwise a clone that is empty when first resolved, then gains a code-exec config (git pull / agent write), would ride the stale grant
        // The new hooks/plugins would then load and run ungated on the next /hooks reload or new session (TOCTOU) Drives the real `resolve_and_record` and `project_scope_allowed` Force the feature on via env (highest precedence) so the test does not depend on the host's folder-trust config
        unsafe { std::env::set_var("GROK_FOLDER_TRUST", "1") };
        // Simulate a release-stamped build
        // An unstamped local/dev build (as in CI, no GROK_VERSION) auto-trusts, so the gate would never engage without this
        unsafe { std::env::set_var(distill_version::TEST_VERSION_ENV, "0.0.0-sim") };
        let tmp = repo_tmp();

        // Empty repo: nothing to gate, so allowed, but left UNRECORDED (provisional)
        assert!(resolve_and_record(tmp.path(), None, false));
        assert!(
            DECISIONS.lock().get(&workspace_key(tmp.path())).is_none(),
            "provisional no-configs allow must not be cached as a durable grant"
        );

        // A repo-local code-exec config appears after the first resolve.
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();

        // The next resolve re-checks `repo_configs_present` (no stale grant to ride)
        // Headless resolves untrusted, so the newly-added hooks are now gated
        assert!(
            !resolve_and_record(tmp.path(), None, false),
            "configs that appear after the first resolve must be re-checked and gated"
        );
        assert!(!project_scope_allowed(tmp.path()));

        unsafe { std::env::remove_var(distill_version::TEST_VERSION_ENV) };
        unsafe { std::env::remove_var("GROK_FOLDER_TRUST") };
    }

    #[test]
    #[serial_test::serial]
    fn resolve_launch_dir_trust_matches_resolve_and_record() {
        // `resolve_launch_dir_trust` derives the launch-dir verdict from one gather It must agree with `resolve_and_record(cwd, None, false)`
        // It must leave the provisional no-configs grant UNCACHED (the TOCTOU contract on the shared path) Force the gate on via env (highest precedence) so the test does not depend on the host config
        // Isolate GROK_HOME so the store is empty/seeded in temp; `#[serial]` because both vars are process-global
        let _feature = EnvGuard::set("GROK_FOLDER_TRUST", "1");
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());

        // (a) No configs: provisional Trusted, NOT cached by the shared path
        let empty = repo_tmp();
        let lt = resolve_launch_dir_trust(empty.path(), None);
        assert!(
            DECISIONS.lock().get(&workspace_key(empty.path())).is_none(),
            "provisional no-configs allow must not be cached by resolve_launch_dir_trust"
        );
        assert_eq!(lt, resolve_and_record(empty.path(), None, false));
        assert!(lt, "no-configs launch dir must be allowed");

        // (b) Configs present and untrusted (empty store, headless): false
        let untrusted = repo_tmp();
        std::fs::create_dir_all(untrusted.path().join(".grok").join("hooks")).unwrap();
        let lt = resolve_launch_dir_trust(untrusted.path(), None);
        assert_eq!(lt, resolve_and_record(untrusted.path(), None, false));
        assert!(!lt, "untrusted configs launch dir must be denied");

        // (c) Configs present and store-trusted: true
        let trusted = repo_tmp();
        std::fs::create_dir_all(trusted.path().join(".grok").join("hooks")).unwrap();
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(trusted.path())).unwrap();
        let lt = resolve_launch_dir_trust(trusted.path(), None);
        assert_eq!(lt, resolve_and_record(trusted.path(), None, false));
        assert!(lt, "store-trusted launch dir must be allowed");
    }

    #[test]
    #[serial_test::serial]
    fn local_build_is_inert_launch_trust_auto_trusts() {
        // On a local/dev build the whole folder-trust system is inert
        // An untrusted repo that HAS repo-local configs (here an `.envrc`) with an EMPTY store still resolves trusted `resolve_launch_dir_trust` returns true, and the `.envrc` loads without any grant
        // Assert the local branch ONLY when compiled unstamped (mirrors the workspace `is_local_build_honors_test_version_override`) GROK_TEST_VERSION is unset so `is_local_build()` is genuinely true; GROK_HOME-isolated so the real store is never touched
        let _sim = EnvGuard::unset(distill_version::TEST_VERSION_ENV);
        if option_env!("GROK_VERSION").is_some() {
            return; // a release-stamped test binary is not a local build
        }
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".envrc"), "export LOCAL_BUILD_ENVRC=1\n").unwrap();

        // The `.envrc` makes this a gating-eligible repo: on a release build it would resolve untrusted with an empty store
        // On a local build it does not
        assert!(
            repo_configs_present(tmp.path()),
            "the `.envrc` must make this a gating-eligible repo"
        );
        assert!(
            project_scope_allowed(tmp.path()),
            "local build: inert gate must auto-trust an untrusted repo that has configs"
        );
        assert!(
            resolve_launch_dir_trust(tmp.path(), None),
            "local build: launch-dir verdict must be trusted"
        );

        // The gated `.envrc` load (the call-site contract) runs because the gate is inert/trusted, so the var is present with no store grant
        let env = distill_workspace::envrc::load_envrc_or_empty(tmp.path());
        assert_eq!(
            env.get("LOCAL_BUILD_ENVRC"),
            Some(&"1".to_string()),
            "local build: `.envrc` must load without any store grant"
        );
    }

    #[test]
    #[serial_test::serial]
    fn prompt_warranted_true_for_untrusted_repo_with_configs() {
        // Feature on (via remote), untrusted (empty store), repo configs present: the GUI prompt is warranted
        // GROK_HOME-isolated so the store starts empty; `#[serial]` because GROK_HOME is process-global
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".mcp.json"), "{}").unwrap();
        // Simulate a release-stamped build so the inert local-build gate is off and the remote `folder_trust_enabled` flag actually engages
        // GROK_FOLDER_TRUST unset: env outranks the remote flag, so an ambient opt-out would otherwise false-fail the Prompt assertion
        let _sim = EnvGuard::set(distill_version::TEST_VERSION_ENV, "0.0.0-sim");
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let remote = RemoteSettings {
            folder_trust_enabled: Some(true),
            ..Default::default()
        };
        assert!(prompt_warranted(tmp.path(), Some(&remote)));
    }

    #[test]
    #[serial_test::serial]
    fn prompt_warranted_false_when_feature_disabled() {
        // The remote kill-switch (folder_trust_enabled = Some(false)) disables the feature even on a release-stamped build So no prompt is warranted even with repo configs present
        // Simulate a release build so the inert local-build path is not what's under test GROK_HOME-isolated and GROK_FOLDER_TRUST unset so the kill-switch is the only signal
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let _sim = simulate_release_build();
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".mcp.json"), "{}").unwrap();
        let remote = RemoteSettings {
            folder_trust_enabled: Some(false),
            ..Default::default()
        };
        assert!(!prompt_warranted(tmp.path(), Some(&remote)));
    }

    #[test]
    #[serial_test::serial]
    fn prompt_warranted_false_when_store_trusted() {
        // A folder the user already trusted resolves Trusted, not Prompt.
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".mcp.json"), "{}").unwrap();
        let mut store = TrustStore::load();
        store.set_trusted(&workspace_key(tmp.path())).unwrap();
        let remote = RemoteSettings {
            folder_trust_enabled: Some(true),
            ..Default::default()
        };
        assert!(!prompt_warranted(tmp.path(), Some(&remote)));
    }

    #[test]
    #[serial_test::serial]
    fn prompt_warranted_false_without_repo_configs() {
        // Nothing repo-local to gate resolves Trusted, not Prompt
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let tmp = repo_tmp();
        let remote = RemoteSettings {
            folder_trust_enabled: Some(true),
            ..Default::default()
        };
        assert!(!prompt_warranted(tmp.path(), Some(&remote)));
    }

    #[test]
    fn detected_config_kinds_summarizes_present_markers() {
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".mcp.json"), "{}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        std::fs::write(tmp.path().join(".grok").join("lsp.json"), "{}").unwrap();
        std::fs::write(tmp.path().join(".envrc"), "export X=1\n").unwrap();
        let kinds = detected_config_kinds(tmp.path());
        assert!(kinds.contains(&"mcp".to_string()));
        assert!(kinds.contains(&"hooks".to_string()));
        assert!(kinds.contains(&"envrc".to_string()));
        // `lsp` is reported (a real gate reason), NOT filtered out, so an lsp-only repo never prompts with an empty reason list
        assert!(kinds.contains(&"lsp".to_string()));
    }

    #[test]
    fn detected_config_kinds_reports_lsp_only_repo() {
        // Regression for the "empty configKinds" bug: a repo gated SOLELY by `.grok/lsp.json` must still produce a non-empty reason list
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok")).unwrap();
        std::fs::write(tmp.path().join(".grok").join("lsp.json"), "{}").unwrap();
        let kinds = detected_config_kinds(tmp.path());
        assert_eq!(kinds, vec!["lsp".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn provisional_no_configs_scan_rewalks_when_config_appears() {
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("GROK_HOME", home.path());
        let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
        let tmp = repo_tmp();

        let (allowed, scan) = gather_and_record(tmp.path(), None, false);
        assert!(allowed, "no-configs resolve is a provisional allow");
        assert!(
            DECISIONS.lock().get(&workspace_key(tmp.path())).is_none(),
            "provisional allow must stay uncached"
        );

        std::fs::write(tmp.path().join(".mcp.json"), "{}\n").unwrap();
        let reused = resolve_and_record_from_scan(tmp.path(), None, false, scan);
        assert!(
            !reused,
            "a config added after gather must flip the provisional allow"
        );
    }
}
