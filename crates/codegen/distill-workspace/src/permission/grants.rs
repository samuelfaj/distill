// Modified for Distill by Samuel Fajreldines, 2026.
//! Stateless permission-grant evaluation: safe-command lists, word-boundary prefix and glob grant matching,
//! per-segment bash scrutiny with deny-wins, the ambient-git scan, the protected-target floor, and the
//! MCP / web_fetch / session-grant pre-decisions.
//! Holds no session state; callers pass a `PermissionState` snapshot.

use std::collections::HashSet;
use std::path::Path;

use crate::permission::auto_mode::{
    BashSecurityAssessment, ClassifierSecurityFinding, EnvRisk, KUBECTL_UNSAFE_FLAGS,
    rg_has_unsafe_flag, script_env_risk,
};
use crate::permission::bash_command_splitting::{
    is_setup_command, try_parse_shell, try_parse_word_only_commands_sequence, unwrap_wrappers,
};
use crate::permission::exec_risk::{
    SAFE_GIT_SUBCOMMANDS, ambient_exec_risk_from_plan, ambient_scan_plan_from_segments,
    git_words_are_read_only_query, git_words_have_unsafe_query_option, script_may_invoke_git,
    segment_exec_facts,
};
use crate::permission::policy::ShellWord;
use crate::permission::prompter::PromptOutcome;
use crate::permission::reasons;
use crate::permission::shell_access::{
    ProtectedEditReason, command_write_paths_split, edit_target_protection, is_creation_program,
    is_safe_write_sink, script_has_cwd_change, tree_has_opaque_shell, words_are_opaque_shell,
};
use crate::permission::state::PermissionState;
use crate::permission::types::{AccessKind, Decision, RequestPathContext};
use distill_mcp::servers::parse_mcp_qualified_name;
use distill_tools::implementations::distill::web_fetch::{
    DomainMatcher, domain::normalize_domain,
};
use distill_tools::types::resources::resolve_model_path;

mod bash_grants;

use bash_grants::whole_script_grant;
pub use bash_grants::{always_allow_row_is_effective, always_allow_scope_persists};
pub(crate) use bash_grants::{
    bash_glob_covers_script, bash_grant_segments, persist_bash_always_allow,
};

/// True iff `name` is a valid qualified MCP ID whose server is in `servers`.
/// Malformed names fail closed, including `{""}` or names like `"__tool"`.
fn mcp_server_prefix_allowed(name: &str, servers: &HashSet<String>) -> bool {
    !servers.is_empty()
        && parse_mcp_qualified_name(name).is_some_and(|(_, server, _)| servers.contains(server))
}

/// Pre-decision lookup for an MCP tool. A remembered "never allow" rejects, checked before the `ask`-floor early return so a deny wins over any grant (mirroring the bash disallow path).
/// With `remember_tool_approvals` on, an existing grant instead satisfies the rule (ask once, then remember); ungranted tools still prompt.
pub(crate) fn mcp_pre_decision(
    name: &str,
    state: &PermissionState,
    policy_forced_prompt: bool,
    remember_tool_approvals: bool,
) -> Option<Decision> {
    // Exact qualified `server__tool` match, same lookup key as `allowed_mcp_tools`
    if state.disallowed_mcp_tools.contains(name) {
        tracing::debug!(%name, source = "session_denylist_tool", "MCP tool auto-rejected");
        return Some(Decision::Reject(format!(
            "User previously rejected `{name}` in this project"
        )));
    }
    if policy_forced_prompt && !remember_tool_approvals {
        return None;
    }
    if state.allowed_mcp_tools.contains(name) {
        tracing::debug!(
            %name,
            source = "session_allowlist_tool",
            "MCP tool auto-approved"
        );
        return Some(Decision::Allow);
    }
    if mcp_server_prefix_allowed(name, &state.allowed_mcp_servers) {
        tracing::debug!(
            %name,
            source = "session_allowlist_server",
            "MCP tool auto-approved"
        );
        return Some(Decision::Allow);
    }
    None
}

/// Canonical key for a persisted web_fetch deny: the host lowercased with the trailing dot trimmed. Entry `example.com` still denies `www.example.com`, because `www.` is an ordinary subdomain label to the matcher.
pub(crate) fn web_fetch_deny_key(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

/// [`web_fetch_deny_key`] of a raw URL's host, if it parses to a non-empty one.
pub(crate) fn web_fetch_deny_key_from_url(url: &str) -> Option<String> {
    let key = web_fetch_deny_key(url::Url::parse(url).ok()?.host_str()?);
    (!key.is_empty()).then_some(key)
}

/// The persisted "never allow" entry matching a web_fetch host, if any. A deny covers the exact host and its subdomains, but never a parent of the entry.
/// That is broader than the exact-match allow lookup on purpose: denies fail safe.
fn denied_web_fetch_domain<'a>(host: &str, disallowed: &'a HashSet<String>) -> Option<&'a str> {
    if disallowed.is_empty() {
        return None;
    }
    let domain = web_fetch_deny_key(host);
    disallowed
        .iter()
        .find(|denied| {
            // A hand-edited empty entry must never match (it would match any host ending in '.')
            !denied.is_empty()
                && (domain == **denied
                    || (domain.len() > denied.len() + 1
                        && domain.ends_with(denied.as_str())
                        && domain.as_bytes().get(domain.len() - denied.len() - 1) == Some(&b'.')))
        })
        .map(String::as_str)
}

/// Session-deny pre-decision for a web_fetch URL: `Some(Reject)` when the host (or a parent domain of it) is on `disallowed_web_fetch_domains`.
/// Consulted before every allow source (static allowlist, persisted grant), so a remembered deny wins over grants, mirroring the bash disallow path.
pub(crate) fn web_fetch_deny_pre_decision(
    parsed_url: &url::Url,
    state: &PermissionState,
) -> Option<Decision> {
    let denied =
        denied_web_fetch_domain(parsed_url.host_str()?, &state.disallowed_web_fetch_domains)?;
    tracing::debug!(
        url = %parsed_url,
        %denied,
        source = "session_denylist",
        "web_fetch domain auto-rejected"
    );
    Some(Decision::Reject(format!(
        "User previously rejected `{denied}` in this project"
    )))
}

/// True when `words` is a `kubectl` invocation that selects a caller-controlled kubeconfig, endpoint, auth, or identity.
/// A read verb like `get`/`logs`/`describe` is not side-effect-free once any of these flags point kubectl at attacker-supplied config/auth.
/// Such invocations must not ride the safe-command auto-allow (nor a broader whitelist *prefix* grant, see `evaluate_bash`).
fn kubectl_has_unsafe_flag(words: &[String]) -> bool {
    if crate::permission::policy::normalized_command_head(words).as_deref() != Some("kubectl") {
        return false;
    }
    words.iter().skip(1).any(|w| {
        let name = w.split_once('=').map_or(w.as_str(), |(name, _)| name);
        KUBECTL_UNSAFE_FLAGS.contains(&name)
    })
}

/// True when `words` is a `ps` that dumps process environments. Uppercase `E` dumps env on macOS (`-E`); we prompt on any `E` on all platforms because the runtime OS is unknown (fail-safe).
/// Plain UNIX `-e`/`-ef`/`-Ae` stay select-all; the `a`/`x` match is deliberately case-sensitive so `-Ae` is not treated as BSD.
fn ps_dumps_environment(words: &[String]) -> bool {
    if crate::permission::policy::normalized_command_head(words).as_deref() != Some("ps") {
        return false;
    }
    let mut skip_next = false;
    for w in words.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        let s = w.as_str();
        if s.starts_with("--format=") || s.starts_with("--sort=") {
            continue;
        }
        // Only flags whose VALUES can contain e/E need listing; an omission merely over-prompts (never leaks)
        // Skipping only ever swallows a ps operand
        if matches!(
            s,
            "-o" | "-O"
                | "--format"
                | "--sort"
                | "-p"
                | "-q"
                | "-t"
                | "-u"
                | "-U"
                | "-g"
                | "-G"
                | "-C"
                | "-s"
                | "--pid"
                | "--ppid"
                | "--sid"
                | "--tty"
                | "--user"
                | "--group"
                | "--cols"
                | "--columns"
                | "--width"
                // BSD dashless format selectors take a following format list.
                | "o"
                | "O"
        ) {
            skip_next = true;
            continue;
        }
        // Attached short form: `-oetime`, `-Opid`, …
        if s.starts_with("-o") || s.starts_with("-O") {
            continue;
        }

        // Env-dump option letters (checked before the trailing-o skip so `-Eo`/`-axeo` still force a prompt)
        let has_upper_e = s.contains('E');
        let has_lower_e = s.contains('e');
        let dashless = !s.starts_with('-');
        // Lowercase a/x only: `-Ae` is UNIX select-all, `-AE` has an E and dumps env
        let bsd_selector_cluster =
            s.starts_with('-') && !s.starts_with("--") && s.contains(['a', 'x']);
        if has_upper_e || (has_lower_e && (dashless || bsd_selector_cluster)) {
            return true;
        }

        // Short cluster ending in arg-taking `o`/`O` (`-eo etime`, `-axo cmd`): the next word is the format list, not an option cluster
        if s.starts_with('-') && !s.starts_with("--") && s.ends_with(['o', 'O']) {
            skip_next = true;
            continue;
        }
    }
    false
}

/// Check whether the command words (already parsed by tree-sitter) match one of the known safe command prefixes.
fn is_safe_command_words(words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    if rg_has_unsafe_flag(words) {
        return false;
    }
    if kubectl_has_unsafe_flag(words) {
        return false;
    }
    if ps_dumps_environment(words) {
        return false;
    }
    // Git rides its own shared decision helper (verb allowlist and unsafe-option table in `exec_risk.rs`), not the string prefixes below
    if words.first().map(String::as_str) == Some("git") {
        return git_words_are_read_only_query(words);
    }
    let joined = words.join(" ");
    is_safe_command_words_str(&joined)
}

fn matches_command_prefix(cmd: &str, pattern: &str) -> bool {
    cmd == pattern || (cmd.starts_with(pattern) && cmd.as_bytes().get(pattern.len()) == Some(&b' '))
}

/// `git <read-only verb>` prefix match, derived from the single [`SAFE_GIT_SUBCOMMANDS`] verb table.
/// String-level only (whitelist scope and fallback); the words paths decide via [`git_words_are_read_only_query`], which also rejects unsafe options.
fn is_safe_git_query_prefix(cmd: &str) -> bool {
    cmd.strip_prefix("git ").is_some_and(|rest| {
        SAFE_GIT_SUBCOMMANDS
            .iter()
            .any(|verb| matches_command_prefix(rest, verb))
    })
}

/// Shared prefix check used by both the tree-sitter path and the fallback path.
fn is_safe_command_words_str(cmd: &str) -> bool {
    matches_command_prefix(cmd, "ls")
        || matches_command_prefix(cmd, "cat")
        || matches_command_prefix(cmd, "pwd")
        || matches_command_prefix(cmd, "date")
        || is_safe_git_query_prefix(cmd)
        || matches_command_prefix(cmd, "whoami")
        || matches_command_prefix(cmd, "hostname")
        || matches_command_prefix(cmd, "uptime")
        || matches_command_prefix(cmd, "grep")
        || matches_command_prefix(cmd, "rg")
        || matches_command_prefix(cmd, "kubectl get")
        || matches_command_prefix(cmd, "kubectl logs")
        || matches_command_prefix(cmd, "kubectl describe")
        || matches_command_prefix(cmd, "ps")
        || matches_command_prefix(cmd, "head")
        || matches_command_prefix(cmd, "tail")
        || matches_command_prefix(cmd, "wc")
        || matches_command_prefix(cmd, "sort")
        || matches_command_prefix(cmd, "uniq")
        || matches_command_prefix(cmd, "tr")
        || matches_command_prefix(cmd, "cut")
        // Stdout-only; a redirect to a real file floors the script as a request-level `FileWrite` before the safe-list allow
        // Without these, an `…; echo saved` tail makes the whole chain impossible to cover with a grant
        || matches_command_prefix(cmd, "echo")
        || matches_command_prefix(cmd, "printf")
    // CWE-863: `tee` is not safe-listed; it writes stdin to arbitrary files, so pipelines like `cat data | tee /target` could bypass edit permissions
    //
    // [`rg_has_unsafe_flag`] is checked at the words level; the string form here cannot see flag structure reliably after join
}

/// Commands which are always safe to execute and should never prompt the user.
/// This list is checked against the primary command after bash command splitting/parsing.
const ALWAYS_SAFE_COMMANDS: &[&str] = &[
    // Read-only filesystem commands
    "ls",
    "cat",
    "pwd",
    "date",
    "whoami",
    "hostname",
    "uptime",
    "ps",
    // Git read-only queries are NOT listed here
    // They go through `exec_risk::git_words_are_read_only_query` (shared verb and unsafe-option tables) in `is_always_safe_command_words`
    // Search commands
    "grep",
    "rg",
    // Kubernetes read-only commands
    "kubectl get",
    "kubectl logs",
    "kubectl describe",
];

/// Auto-allow bare `mkdir`/`touch`. Bare name only: `/bin/mkdir` still classifies (fail-safe).
fn is_safe_creation_command(words: &[String]) -> bool {
    words
        .first()
        .map(String::as_str)
        .is_some_and(is_creation_program)
}

/// Check whether parsed command words match the always-safe list. Applied per chained segment so that scripts like `ls && rm -rf /` cannot auto-approve via the always-safe primary alone.
/// Every non-setup segment must independently pass this check (or the broader `is_safe_command_words`, or a user whitelist).
fn is_always_safe_command_words(words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    if rg_has_unsafe_flag(words) {
        return false;
    }
    if kubectl_has_unsafe_flag(words) {
        return false;
    }
    if ps_dumps_environment(words) {
        return false;
    }
    // Git rides its own shared decision helper (verb allowlist and unsafe-option table in `exec_risk.rs`), not the prefix list below
    if words.first().map(String::as_str) == Some("git") {
        return git_words_are_read_only_query(words);
    }

    let joined = words.join(" ");

    // CWE-183: use matches_command_prefix to require a word boundary after the safe prefix, preventing e.g. "tr" from matching "truncate".
    for safe_pattern in ALWAYS_SAFE_COMMANDS {
        if matches_command_prefix(&joined, safe_pattern) {
            return true;
        }
    }

    false
}

/// Whether an always-allow grant for `words` must pin to the exact full command instead of a narrower prefix. Dangerous verbs (`rm`, `git push`, …) qualify because enforcement honors them only as exact whole-command grants.
/// Exec vehicles (interpreters, package runners, `sudo`/`ssh`) qualify because a bare `python3`/`sudo git` prefix would authorize any arguments.
fn always_allow_scope_pinned(words: &[String]) -> bool {
    // `sed` writes via script content (`-i`, `1w/path`), not a word prefix, so a `sed -n` prefix grant would silently cover those writes; pin it
    is_dangerous_command_words(words)
        || crate::permission::policy::head_is_exec_vehicle(words)
        || crate::permission::policy::normalized_command_head(words).as_deref() == Some("sed")
}

/// Default always-allow whitelist scope (word count) for a parsed command. Scope narrowing applies only when the **full** invocation is safe-listed.
/// Otherwise a non-auto-allowed form like `rg --pre …` would still scope to bare `rg`, and "Always allow" would re-open the preprocessor exec hole.
pub fn default_always_allow_scope(words: &[String]) -> usize {
    if words.is_empty() {
        return 0;
    }
    // Pinned commands (dangerous verbs, exec vehicles) offer only the full command A narrowed default like "Always allow:
    // git push" would save a rule that can never match "Always allow: sudo git" or "python3" would authorize arbitrary
    // arguments Wrapped/chained forms whose full-scope grant still cannot match get no row at all (`always_allow_row_is_effective`)
    if always_allow_scope_pinned(words) {
        return words.len();
    }
    if let Some(n) = gh_always_allow_scope(words) {
        return n;
    }
    base_scope(words)
}

/// `gh`'s remote-mutating verb is its third word (`gh pr merge`), so a narrower `gh pr` prefix would cover it.
/// Scope to group and action, else pin to the full command.
/// Both the default and the minimum use this, so the left arrow can't narrow below it.
fn gh_always_allow_scope(words: &[String]) -> Option<usize> {
    if crate::permission::policy::normalized_command_head(words).as_deref() != Some("gh") {
        return None;
    }
    Some(match (words.get(1), words.get(2)) {
        (Some(group), Some(verb)) if !group.starts_with('-') && !verb.starts_with('-') => 3,
        _ => words.len(),
    })
}

/// Default "Never allow" scope (word count) for a parsed command.
/// Denies honor prefixes for every command, so the dangerous full-command pin does not apply.
/// "Never allow: git push" blocking all pushes is the point.
pub fn default_always_deny_scope(words: &[String]) -> usize {
    if words.is_empty() {
        return 0;
    }
    base_scope(words)
}

/// Verb-plus-flags scope shared by the allow default (non-dangerous arm) and the deny default.
fn base_scope(words: &[String]) -> usize {
    if is_safe_command_words(words) {
        if words.first().is_some_and(|w| is_safe_command_words_str(w)) {
            return 1;
        }
        if words.len() >= 2
            && words
                .get(..2)
                .is_some_and(|pair| is_safe_command_words_str(&pair.join(" ")))
        {
            return 2;
        }
    }
    let mut n = words.len().min(2);
    while n < words.len() && words.get(n).is_some_and(|w| w.starts_with('-')) {
        n += 1;
    }
    n
}

/// Narrowest always-allow scope (word count) the prompt may offer for a parsed command. Only the exact command the user saw may persist.
/// Deny scopes are not pinned (see [`default_always_deny_scope`]).
pub fn minimum_always_allow_scope(words: &[String]) -> usize {
    if always_allow_scope_pinned(words) {
        return words.len();
    }
    // Narrowing `gh` broadens the grant (fewer words cover more subcommands), so the floor equals the default: the left arrow cannot reach `gh pr`
    gh_always_allow_scope(words).unwrap_or(1)
}

/// Check whether parsed command words begin with a known dangerous command. Applied per chained segment, not only the start of the script.
/// A segment matching this check is NEVER auto-approved via a user whitelist; the user must always be prompted for it.
fn is_dangerous_command_words(words: &[String]) -> bool {
    // Match on the normalized basename so `/bin/rm`, `RM`, and `rm.exe` are all caught (consistent with `head_is_exec_vehicle` and the sed pin)
    let Some(head) = crate::permission::policy::normalized_command_head(words) else {
        return false;
    };
    let joined = if words.len() == 1 {
        head
    } else {
        format!("{head} {}", words.get(1..).unwrap_or(&[]).join(" "))
    };
    matches_command_prefix(&joined, "rm")
        || matches_command_prefix(&joined, "chmod")
        || matches_command_prefix(&joined, "chown")
        || matches_command_prefix(&joined, "chgrp")
        || matches_command_prefix(&joined, "chattr")
        || matches_command_prefix(&joined, "pkill")
        || matches_command_prefix(&joined, "kill")
        || matches_command_prefix(&joined, "killall")
        || matches_command_prefix(&joined, "git push")
}

/// Uses `matches_command_prefix` so user allow/deny entries enforce a word boundary after the prefix.
/// That keeps a "git" entry from matching "gitleaks" (CWE-183).
/// Metacharacters in a literal grant stay literal; glob patterns live in `allowed_bash_globs`, matched separately (see [`matches_bash_glob`]).
fn matches_whitelist_prefix(segment_str: &str, allowed_prefix: &str) -> bool {
    matches_command_prefix(segment_str, allowed_prefix)
}

/// Whether a user-authored glob grant (`allowed_bash_globs`) authorizes `segment_str`.
/// Uses the same matcher as the config `[permission]` rules and the pattern-editor preview, so what the user previewed is what auto-allows.
fn matches_bash_glob(segment_str: &str, pattern: &str) -> bool {
    super::policy::bash_pattern_matches_command(pattern, segment_str)
}

/// Ordinary command-segment outcome, before script-level effect floors.
#[derive(Debug)]
pub(crate) enum SegmentEvaluation {
    /// All non-setup segments safe/always-safe or on an allow-prefix.
    /// `via_session_grant`: at least one segment hit `allowed_bash_commands`.
    AutoAllow { via_session_grant: bool },
    /// Disallow-prefix matched; reject without prompting.
    Reject(String),
    /// One or more segments need a user decision.
    NeedsPrompts {
        #[allow(dead_code)]
        segments: Vec<String>,
    },
    /// Tree-sitter could not decompose the script (heredoc, `$(…)`, backtick, single `&` background, …).
    /// Caller should fall back to a single conservative prompt with the full script.
    Unparseable,
}

/// One request's parsed Bash authorization facts.
#[derive(Debug)]
pub(crate) struct BashEvaluation {
    segments: SegmentEvaluation,
    pub(crate) exact_grant: bool,
    all_segments_granted: bool,
    /// Canonical, ordered, deduplicated security findings for this request.
    /// The single source for grant/sandbox floor disposition and classifier evidence.
    /// `ExecOrAmbientGit` may be added later by the ambient git scan.
    pub(crate) assessment: BashSecurityAssessment,
    /// An unsafe write target came from a redirect (`> f`), which allow-rule word matching cannot see; no configured allow rule may vouch for it.
    /// `true` (fail closed) on undecomposable scripts.
    pub(crate) redirect_write: bool,
    /// Raw segment word lists for ambient cwd tracking (git present, flags clean).
    pub(crate) ambient_segments: Option<Vec<Vec<String>>>,
    /// `mkdir`/`touch`/redirect/command-word write targets for the protected-target floor.
    pub(crate) protected_paths: Vec<String>,
    /// Script has an in-scope `cd`/`pushd`/`popd`, so relative operands cannot be pinned.
    pub(crate) has_cwd_change: bool,
}

fn unparseable_exec_risk(cmd: &str) -> bool {
    // WHY: word-only decomposition failed; ambient git never ran
    // Fail closed when the script may still invoke git so sandbox/Auto cannot auto-allow
    script_may_invoke_git(cmd)
}

/// Map an unsafe-environment risk tier to its finding (`Safe` maps to none).
fn env_risk_finding(env_risk: EnvRisk) -> Option<ClassifierSecurityFinding> {
    match env_risk {
        EnvRisk::Injection => Some(ClassifierSecurityFinding::EnvInjection),
        EnvRisk::Unvetted => Some(ClassifierSecurityFinding::UnvettedEnv),
        EnvRisk::Safe => None,
    }
}

/// A persisted deny matching the raw script text (word-boundary prefix, the deny regime everywhere else).
/// Unparseable scripts never reach per-segment deny matching, so without this their "don't ask again" denies would be silently inert.
/// Matching the raw text can only over-block (deny-safe).
fn raw_deny_rejection(cmd: &str, state: &PermissionState) -> Option<SegmentEvaluation> {
    state
        .disallowed_bash_commands
        .iter()
        .find(|d| matches_whitelist_prefix(cmd, d))
        .map(|d| {
            SegmentEvaluation::Reject(format!("User previously rejected `{d}` in this project"))
        })
}

/// Parse and classify one Bash request once.
/// Ordinary segment outcome stays separate from the script-level real-file-write and unsafe-environment floors.
pub(crate) fn evaluate_bash(
    cmd: &str,
    state: &PermissionState,
    honor_safe_lists: bool,
) -> BashEvaluation {
    use ClassifierSecurityFinding as Finding;
    let exact_grant = state.allowed_bash_commands.contains(cmd);
    let mut assessment = BashSecurityAssessment::default();
    let Some(tree) = try_parse_shell(cmd) else {
        // Undecomposable at the top level: unparseable structure, plus fail closed on ambient git exec risk (word-only decomposition never ran)
        assessment.insert(Finding::UnparseableShell);
        if unparseable_exec_risk(cmd) {
            assessment.insert(Finding::ExecOrAmbientGit);
        }
        return BashEvaluation {
            segments: raw_deny_rejection(cmd, state).unwrap_or(SegmentEvaluation::Unparseable),
            exact_grant,
            all_segments_granted: false,
            assessment,
            redirect_write: true,
            ambient_segments: None,
            protected_paths: Vec::new(),
            has_cwd_change: false,
        };
    };
    let writes = command_write_paths_split(tree.root_node(), cmd);
    let has_cwd_change = script_has_cwd_change(tree.root_node(), cmd);
    // An unextractable write-redirect target (`> $OUT`) is a write nothing can vouch for: it both counts as FileWrite and pins `redirect_write`
    let redirect_write = writes.unextracted_write_redirect
        || writes
            .redirect_paths
            .iter()
            .any(|path| !is_safe_write_sink(path));
    if redirect_write
        || writes
            .word_paths
            .iter()
            .any(|path| !is_safe_write_sink(path))
    {
        assessment.insert(Finding::FileWrite);
    }
    let mut protected_paths = writes.creation_paths;
    protected_paths.extend(writes.word_paths);
    protected_paths.extend(writes.redirect_paths);
    let segments = try_parse_word_only_commands_sequence(&tree, cmd);
    if let Some(finding) = env_risk_finding(script_env_risk(
        tree.root_node(),
        cmd,
        segments.as_deref().unwrap_or_default(),
    )) {
        assessment.insert(finding);
    }
    let Some(segments) = segments else {
        // WHY: undecomposable dynamic `bash -c "$X"`/`eval` is still opaque shell.
        assessment.insert(Finding::UnparseableShell);
        if tree_has_opaque_shell(tree.root_node(), cmd) {
            assessment.insert(Finding::OpaqueShell);
        }
        if unparseable_exec_risk(cmd) {
            assessment.insert(Finding::ExecOrAmbientGit);
        }
        return BashEvaluation {
            segments: raw_deny_rejection(cmd, state).unwrap_or(SegmentEvaluation::Unparseable),
            exact_grant,
            all_segments_granted: false,
            assessment,
            redirect_write: true,
            ambient_segments: None,
            protected_paths,
            has_cwd_change,
        };
    };
    // Upgrade the raw-string compare with the dequoted single-command form now that the parse is available (see `whole_script_grant`)
    let exact_grant = whole_script_grant(cmd, &segments, state);
    let mut needs_prompt: Vec<String> = Vec::new();
    let mut via_session_grant = false;
    let mut all_segments_granted = true;
    let mut exec_risk = false;
    let mut has_git_command = false;
    let mut ambient_raw: Vec<Vec<String>> = Vec::new();
    for parsed in segments {
        let raw_words = parsed.words();
        ambient_raw.push(raw_words.to_vec());
        // Peel wrapper commands like `timeout 30 …`, `env FOO=1 …`, `nice -n 5 …` so we classify the *inner* program
        // Without this, `timeout 30 rm -rf /tmp/foo` would be treated as a benign `timeout` invocation and silently auto-allowed
        let words = unwrap_wrappers(raw_words);
        let shell_words: Vec<ShellWord<'_>> = words.iter().map(ShellWord::from).collect();
        if words_are_opaque_shell(&shell_words) {
            assessment.insert(Finding::OpaqueShell);
        }
        // Raw words: interleaved normalize lives in segment_exec_facts.
        let facts = segment_exec_facts(raw_words);
        if facts.exec_risk {
            exec_risk = true;
            assessment.insert(Finding::ExecOrAmbientGit);
        }
        if facts.has_git {
            has_git_command = true;
        }
        if is_setup_command(words) {
            continue;
        }
        let s = words.join(" ");

        // 1. Disallow takes priority: reject the whole script.
        if let Some(d) = state
            .disallowed_bash_commands
            .iter()
            .find(|d| matches_whitelist_prefix(&s, d))
        {
            return BashEvaluation {
                segments: SegmentEvaluation::Reject(format!(
                    "User previously rejected `{d}` in this project"
                )),
                exact_grant,
                all_segments_granted,
                assessment: std::mem::take(&mut assessment),
                redirect_write,
                ambient_segments: None,
                protected_paths: Vec::new(),
                has_cwd_change: false,
            };
        }

        // Pinned commands run whatever argv follows, so a prefix grant would widen
        // `docker run nginx` must not match `... --privileged`, nor `sed -n 1p f` match `sed -n 1p f -e 'w /tmp/x'`.
        // Their saved scope is the full command, so enforce it on the exact segment only
        let matched_command_grant = if always_allow_scope_pinned(words) {
            state.allowed_bash_commands.contains(s.as_str())
        } else {
            state
                .allowed_bash_commands
                .iter()
                .any(|a| matches_whitelist_prefix(&s, a))
        };
        let matched_grant = matched_command_grant
            || state
                .allowed_bash_globs
                .iter()
                .any(|g| matches_bash_glob(&s, g));
        all_segments_granted &= matched_grant;

        // 2. Dangerous commands must be prompted even if a whitelist prefix would otherwise match.
        if is_dangerous_command_words(words) {
            assessment.insert(Finding::DangerousCommand);
            needs_prompt.push(s);
            continue;
        }

        // kubectl config/auth flags, `rg --pre`, env-dumping `ps`, and git driver/write options must prompt even under a whitelist prefix or blanket grant
        // Always-allow persists only the verb prefix, so that grant cannot cover these variants. An exact segment grant still auto-allows. Do not insert DangerousCommand; that would also block exact grants
        if (kubectl_has_unsafe_flag(words)
            || rg_has_unsafe_flag(words)
            || ps_dumps_environment(words)
            || git_words_have_unsafe_query_option(words))
            && !state.allowed_bash_commands.contains(&s)
        {
            assessment.insert(Finding::SpecialExecSurface);
            needs_prompt.push(s);
            continue;
        }

        // 3. Auto-allow conditions. Built-in safe lists count only when `honor_safe_lists` is set; an explicit user grant always counts.
        let matched_safe = honor_safe_lists
            && (is_safe_command_words(words)
                || is_always_safe_command_words(words)
                || is_safe_creation_command(words));
        if matched_grant || matched_safe {
            if matched_grant {
                via_session_grant = true;
            }
            continue;
        }

        // 4. Otherwise: prompt for this segment.
        needs_prompt.push(s);
    }
    let segments = if needs_prompt.is_empty() {
        SegmentEvaluation::AutoAllow { via_session_grant }
    } else {
        SegmentEvaluation::NeedsPrompts {
            segments: needs_prompt,
        }
    };
    let ambient_segments = if has_git_command && !exec_risk {
        Some(ambient_raw)
    } else {
        None
    };
    BashEvaluation {
        segments,
        exact_grant,
        all_segments_granted,
        assessment,
        redirect_write,
        ambient_segments,
        protected_paths,
        has_cwd_change,
    }
}

#[cfg(test)]
pub(crate) fn evaluate_bash_segments(cmd: &str, state: &PermissionState) -> SegmentEvaluation {
    evaluate_bash(cmd, state, true).segments
}

#[cfg(test)]
pub(crate) fn evaluate_bash_segments_inner(
    cmd: &str,
    state: &PermissionState,
    honor_safe_lists: bool,
) -> SegmentEvaluation {
    evaluate_bash(cmd, state, honor_safe_lists).segments
}

/// Whether persisted state auto-approves bash `cmd`.
/// The user-writable `allow_bash_execute` is clamped under the pin so it can't substitute for `--yolo`.
/// Explicit `allowed_bash_commands` grants still apply.
fn persisted_bash_auto_allows(
    state: &PermissionState,
    cmd: &str,
    yolo_pin: Option<&'static str>,
) -> bool {
    (state.allow_bash_execute && yolo_pin.is_none()) || state.allowed_bash_commands.contains(cmd)
}

/// [`evaluate_bash`] plus the ambient git scan, run on the caller's thread: a `git` segment whose
/// checkout carries executable config (`core.fsmonitor`, hooks) is `ExecOrAmbientGit`.
pub(crate) fn evaluate_bash_with_ambient(
    cmd: &str,
    state: &PermissionState,
    cwd: &Path,
) -> BashEvaluation {
    let mut evaluation = evaluate_bash(cmd, state, true);
    if let Some(raw) = evaluation.ambient_segments.take()
        && ambient_exec_risk_from_plan(&ambient_scan_plan_from_segments(&raw, cwd))
    {
        evaluation
            .assessment
            .insert(ClassifierSecurityFinding::ExecOrAmbientGit);
    }
    evaluation
}

/// The protected-target floor: why an edit, or a creation command's write operands, touch a path
/// no grant may pre-decide (hook roots, `.git/hooks`, `.ssh`, shell rc files, the permission store).
/// A creation after a `cd` with a relative operand is `Sensitive` outright: the target cannot be resolved.
pub(crate) fn protected_target(
    access: &AccessKind,
    bash_evaluation: Option<&BashEvaluation>,
    cwd: &Path,
    path_context: Option<&RequestPathContext>,
) -> Option<ProtectedEditReason> {
    match (access, path_context) {
        (AccessKind::Edit(path), Some(context)) => {
            let resolved =
                resolve_model_path(&context.real_cwd, context.display_cwd.as_deref(), path);
            edit_target_protection(&resolved)
        }
        (AccessKind::Edit(path), None) => {
            let resolved = resolve_model_path(cwd, None, path);
            edit_target_protection(&resolved)
        }
        (AccessKind::Bash(_), context) => bash_evaluation.and_then(|e| {
            if e.has_cwd_change
                && e.protected_paths
                    .iter()
                    .any(|p| !Path::new(p).is_absolute())
            {
                return Some(ProtectedEditReason::Sensitive);
            }
            e.protected_paths.iter().find_map(|path| {
                let resolved = match context {
                    Some(ctx) => {
                        resolve_model_path(&ctx.real_cwd, ctx.display_cwd.as_deref(), path)
                    }
                    None => resolve_model_path(cwd, None, path),
                };
                edit_target_protection(&resolved)
            })
        }),
        _ => None,
    }
}

/// A broad grant must prompt rather than auto-allow when the request's assessment carries a grant-floor finding. Broad covers the session `allow_bash_execute` blanket, prefix/glob grants, and sandbox auto-allow.
/// It also covers a broad configured policy Allow deferred to the confirmation floor.
pub(crate) fn bash_request_floor_requires_prompt(evaluation: Option<&BashEvaluation>) -> bool {
    evaluation.is_some_and(|e| !e.exact_grant && e.assessment.constrains_broad_grant())
}

/// Policy knobs for [`bash_grant_pre_decision`].
#[derive(Clone, Copy)]
pub(crate) struct BashGrantOpts {
    honor_safe_lists: bool,
    allow_blanket: bool,
    conservative_blanket: bool,
}

impl BashGrantOpts {
    pub(crate) const PRE_CLASSIFIER: Self = Self {
        honor_safe_lists: true,
        allow_blanket: true,
        conservative_blanket: true,
    };
    pub(crate) const ASK_FLOOR_REMEMBER: Self = Self {
        honor_safe_lists: false,
        allow_blanket: false,
        conservative_blanket: false,
    };
    pub(crate) fn post_classify(auto_forced_prompt: bool) -> Self {
        Self {
            honor_safe_lists: true,
            allow_blanket: !auto_forced_prompt,
            conservative_blanket: false,
        }
    }
}

fn grant_allow(reason: &'static str) -> Option<(Decision, &'static str)> {
    Some((Decision::Allow, reason))
}

pub(crate) fn bash_grant_pre_decision(
    cmd: &str,
    evaluation: &BashEvaluation,
    state: &PermissionState,
    yolo_pin: Option<&'static str>,
    opts: BashGrantOpts,
) -> Option<(Decision, &'static str)> {
    if let SegmentEvaluation::Reject(reason) = &evaluation.segments {
        return Some((Decision::Reject(reason.to_owned()), reasons::SESSION_DENY));
    }
    if bash_request_floor_requires_prompt(Some(evaluation)) {
        return None;
    }
    match &evaluation.segments {
        SegmentEvaluation::Reject(_) => unreachable!(),
        SegmentEvaluation::AutoAllow { via_session_grant } => {
            if !opts.honor_safe_lists && !evaluation.all_segments_granted {
                None
            } else {
                grant_allow(if *via_session_grant {
                    reasons::SESSION_GRANT
                } else {
                    reasons::SAFE_COMMAND
                })
            }
        }
        SegmentEvaluation::NeedsPrompts { .. } => {
            if !opts.allow_blanket {
                None
            } else if opts.conservative_blanket
                && evaluation
                    .assessment
                    .contains(ClassifierSecurityFinding::DangerousCommand)
            {
                // An exact whole-command grant is explicit user authority for THIS command, so the auto classifier must not silent-deny it
                // (It would make auto mode stricter than ask mode for the same persisted grant.)
                // Blanket/prefix grants stay excluded; a dangerous verb prefix like `git push` is never trusted
                evaluation
                    .exact_grant
                    .then_some((Decision::Allow, reasons::SESSION_GRANT))
            } else {
                persisted_bash_auto_allows(state, cmd, yolo_pin)
                    .then_some((Decision::Allow, reasons::SESSION_GRANT))
            }
        }
        SegmentEvaluation::Unparseable => {
            if !opts.allow_blanket {
                None
            } else {
                let allowed = if opts.conservative_blanket {
                    evaluation.exact_grant
                } else {
                    persisted_bash_auto_allows(state, cmd, yolo_pin)
                };
                allowed.then_some((Decision::Allow, reasons::SESSION_GRANT))
            }
        }
    }
}

/// Session always-allow consulted before the auto classifier. Caller must skip under policy/shell Ask floors. `static_domain_matcher` is `None` when auto mode must classify built-in-default web-fetch domains instead of granting them, and on the hub path, where every fetch is a prompt or a persisted grant.
pub(crate) fn session_grant_pre_decision(
    access: &AccessKind,
    bash_evaluation: Option<&BashEvaluation>,
    state: &PermissionState,
    allow_edits_for_session: bool,
    static_domain_matcher: Option<&DomainMatcher>,
    yolo_pin: Option<&'static str>,
) -> Option<(Decision, &'static str)> {
    match access {
        AccessKind::MCPTool { name, .. } => mcp_pre_decision(name, state, false, false).map(|d| {
            let reason = if matches!(d, Decision::Reject(_)) {
                reasons::SESSION_DENY
            } else {
                reasons::SESSION_GRANT
            };
            (d, reason)
        }),
        AccessKind::WebFetch(url) => {
            let Ok(parsed_url) = url::Url::parse(url) else {
                return None;
            };
            // Remembered deny wins over the static allowlist and any grant.
            if let Some(reject) = web_fetch_deny_pre_decision(&parsed_url, state) {
                return Some((reject, reasons::SESSION_DENY));
            }
            if static_domain_matcher.is_some_and(|matcher| matcher.check(&parsed_url).is_none()) {
                return grant_allow(reasons::STATIC_ALLOWLIST);
            }
            let domain = normalize_domain(parsed_url.host_str()?);
            if state.allowed_web_fetch_domains.contains(&domain) {
                grant_allow(reasons::SESSION_GRANT)
            } else {
                None
            }
        }
        AccessKind::Edit(_) if allow_edits_for_session => grant_allow(reasons::SESSION_GRANT),
        AccessKind::Bash(cmd) => bash_grant_pre_decision(
            cmd,
            bash_evaluation?,
            state,
            yolo_pin,
            BashGrantOpts::PRE_CLASSIFIER,
        ),
        AccessKind::Read(_)
        | AccessKind::Grep { .. }
        | AccessKind::WebSearch(_)
        | AccessKind::Edit(_)
        | AccessKind::AgentMessage { .. }
        | AccessKind::Tool(_) => None,
    }
}

/// Apply the persistent half of a prompt answer to `state`, returning the key it recorded (the caller
/// persists when `Some`). Every key is re-derived from `access`, never trusted from the reply, so a forged
/// scope cannot mint a grant the prompt did not show. Session-only answers (`AllowEditsForSession`,
/// once, cancel) and a scope that does not fit the access record nothing.
pub(crate) fn record_prompt_outcome(
    state: &mut PermissionState,
    access: &AccessKind,
    outcome: &PromptOutcome,
) -> Option<String> {
    match (access, outcome) {
        (AccessKind::Bash(cmd), PromptOutcome::AllowAlways) => {
            state.allowed_bash_commands.insert(cmd.clone());
            state.allowed_bash_commands.extend(bash_grant_segments(cmd));
            Some(cmd.clone())
        }
        (AccessKind::Bash(cmd), PromptOutcome::AllowAlwaysBashCommand(prefix)) => {
            persist_bash_always_allow(state, cmd, prefix);
            Some(prefix.clone())
        }
        (AccessKind::Bash(cmd), PromptOutcome::AllowAlwaysBashGlob(pattern)) => {
            if !bash_glob_covers_script(cmd, pattern) {
                tracing::warn!(glob = %pattern, "always-allow glob does not match the prompted script; not persisted");
                return None;
            }
            state.allowed_bash_globs.insert(pattern.clone());
            Some(pattern.clone())
        }
        (AccessKind::Bash(_), PromptOutcome::RejectAlwaysBashCommand(prefix)) => {
            state.disallowed_bash_commands.insert(prefix.clone());
            Some(prefix.clone())
        }
        (AccessKind::MCPTool { name, .. }, PromptOutcome::AllowAlways) => {
            state.allowed_mcp_tools.insert(name.clone());
            Some(name.clone())
        }
        (AccessKind::MCPTool { name, .. }, PromptOutcome::AllowAlwaysMcpTool(client_name)) => {
            if client_name != name {
                tracing::warn!(client_supplied = %client_name, access_name = %name, "AllowAlwaysMcpTool tool_name mismatch; persisting access-kind name");
            }
            state.allowed_mcp_tools.insert(name.clone());
            Some(name.clone())
        }
        (AccessKind::MCPTool { name, .. }, PromptOutcome::AllowAlwaysMcpServer(server_prefix)) => {
            match parse_mcp_qualified_name(name).map(|(_, server, _)| server) {
                Some(server) if server == server_prefix => {
                    state.allowed_mcp_servers.insert(server.to_owned());
                    tracing::info!(%server, count = state.allowed_mcp_servers.len(), "added MCP server to session allowlist");
                    Some(server.to_owned())
                }
                _ => {
                    tracing::warn!(client_supplied = %server_prefix, access_name = %name, "AllowAlwaysMcpServer prefix mismatch; downgrading to tool-scope");
                    state.allowed_mcp_tools.insert(name.clone());
                    Some(name.clone())
                }
            }
        }
        (AccessKind::MCPTool { name, .. }, PromptOutcome::RejectAlwaysMcpTool(client_name)) => {
            if client_name != name {
                tracing::warn!(client_supplied = %client_name, access_name = %name, "RejectAlwaysMcpTool tool_name mismatch; persisting access-kind name");
            }
            state.disallowed_mcp_tools.insert(name.clone());
            Some(name.clone())
        }
        (AccessKind::WebFetch(url), PromptOutcome::AllowAlwaysDomain(client_domain)) => {
            let domain = url::Url::parse(url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(normalize_domain))?;
            if domain != *client_domain {
                tracing::warn!(client_supplied = %client_domain, access_domain = %domain, "AllowAlwaysDomain mismatch; persisting access-URL domain");
            }
            state.allowed_web_fetch_domains.insert(domain.clone());
            Some(domain)
        }
        (AccessKind::WebFetch(url), PromptOutcome::RejectAlwaysDomain(client_domain)) => {
            let domain = web_fetch_deny_key_from_url(url)?;
            if domain != *client_domain {
                tracing::warn!(client_supplied = %client_domain, access_domain = %domain, "RejectAlwaysDomain mismatch; persisting access-URL domain");
            }
            state.disallowed_web_fetch_domains.insert(domain.clone());
            Some(domain)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "grants_tests.rs"]
mod tests;
