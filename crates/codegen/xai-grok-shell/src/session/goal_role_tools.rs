//! Each `/goal` role (planner, strategist, skeptics/verifier) can run on a different toolset, so its prompt must name that role's OWN tools.
//! This module owns the cross-role naming machinery so it does not couple a role-specific module (e.g. `goal_planner`) to its siblings.
//!
//! The per-spawn model override [`RoleSpawnOverride`] and the spawn-and-retry-once wrapper live in [`goal_planner`](crate::session::goal_planner).

use xai_grok_tools::types::tool::ToolKind;

/// Resolved client-facing tool names for a role's prompt placeholders.
/// Built parent-side from the role's resolved toolset, with one literal fallback per placeholder.
/// Every resolved name is run through [`sanitized_tool_name`] before it is stored; an unsafe name falls back to the literal default.
#[derive(Debug, Clone)]
pub(crate) struct RoleToolNames {
    /// `{READ_TOOL}`: `ToolKind::Read`.
    pub read: String,
    /// `{LIST_TOOL}`: `ToolKind::ListDir`.
    pub list: String,
    /// `{SEARCH_TOOL}`: `ToolKind::Search` (grep maps here).
    pub search: String,
    /// `{WRITE_TOOL}`: `ToolKind::Write`, falling back to `ToolKind::Edit` when `Write` is absent from the describe summary.
    /// `ToolKind::Edit` is the default remote-code host's `search_replace` mutator.
    pub write: String,
    /// `{EXECUTE_TOOL}`: `ToolKind::Execute` (terminal/bash maps here).
    pub execute: String,
    /// `{WEB_SEARCH_TOOL}`: `ToolKind::WebSearch` (planner template only).
    pub web_search: String,
    /// `{WEB_FETCH_TOOL}`: `ToolKind::WebFetch` (planner template only).
    pub web_fetch: String,
    /// `{TOOLSET_TOOLS}` block (verifier-only placeholder).
    /// Empty on the inherit path.
    pub toolset_tools: String,
}

impl RoleToolNames {
    const READ_FALLBACK: &'static str = "read_file";
    const LIST_FALLBACK: &'static str = "list_dir";
    const SEARCH_FALLBACK: &'static str = "grep";
    const WRITE_FALLBACK: &'static str = "write";
    const EXECUTE_FALLBACK: &'static str = "run_terminal_command";
    const WEB_SEARCH_FALLBACK: &'static str = "web_search";
    const WEB_FETCH_FALLBACK: &'static str = "web_fetch";

    /// Single fallback-and-sanitize applier shared by every constructor.
    fn from_parts(
        read: Option<String>,
        list: Option<String>,
        search: Option<String>,
        write: Option<String>,
        execute: Option<String>,
        web_search: Option<String>,
        web_fetch: Option<String>,
        toolset_tools: String,
    ) -> Self {
        Self {
            read: sanitized_or_default(read, Self::READ_FALLBACK),
            list: sanitized_or_default(list, Self::LIST_FALLBACK),
            search: sanitized_or_default(search, Self::SEARCH_FALLBACK),
            write: sanitized_or_default(write, Self::WRITE_FALLBACK),
            execute: sanitized_or_default(execute, Self::EXECUTE_FALLBACK),
            web_search: sanitized_or_default(web_search, Self::WEB_SEARCH_FALLBACK),
            web_fetch: sanitized_or_default(web_fetch, Self::WEB_FETCH_FALLBACK),
            toolset_tools,
        }
    }

    /// All-fallback names with an empty `{TOOLSET_TOOLS}` block.
    /// Used as the per-index default when no assignment exists, and by tests.
    pub(crate) fn inherit_defaults() -> Self {
        Self::from_parts(None, None, None, None, None, None, None, String::new())
    }

    /// Inherit / fail-open path: the role runs on the parent toolset, so the names come from the parent tool bridge (already resolved by the caller).
    /// `{WRITE_TOOL}` falls back to the parent `Edit` tool name when the bridge has no `Write`, mirroring [`Self::from_summary`].
    pub(crate) fn from_parent(
        read: Option<String>,
        list: Option<String>,
        search: Option<String>,
        write: Option<String>,
        edit: Option<String>,
        execute: Option<String>,
        web_search: Option<String>,
        web_fetch: Option<String>,
    ) -> Self {
        Self::from_parts(
            read,
            list,
            search,
            first_safe_tool_name(write, edit),
            execute,
            web_search,
            web_fetch,
            String::new(),
        )
    }

    /// Explicit-pair path: names come from the role's `describe_subagent_type` summary (the `name_override`-aware client name per kind).
    /// `{TOOLSET_TOOLS}` enumerates the toolset.
    pub(crate) fn from_summary(
        summary: &xai_grok_tools::implementations::grok_build::task::types::SubagentTypeSummary,
    ) -> Self {
        let get = |kind: ToolKind| summary.tool_names.get(&kind).cloned();
        Self::from_parts(
            get(ToolKind::Read),
            get(ToolKind::ListDir),
            get(ToolKind::Search),
            // The default remote-code host's pre-spawn describe probe exposes only `Edit` (`search_replace`) as the file mutator
            // The injection-only `write`/`Write` tool is absent there
            first_safe_tool_name(get(ToolKind::Write), get(ToolKind::Edit)),
            get(ToolKind::Execute),
            get(ToolKind::WebSearch),
            get(ToolKind::WebFetch),
            enumerate_toolset_tools(&summary.tool_names),
        )
    }

    /// Substitute the role-prompt tool placeholders in `template` in a SINGLE left-to-right pass.
    /// A substituted value is never re-scanned, so a resolved name can never be re-expanded into another placeholder (order-independence).
    /// Replacing a placeholder a template does not contain is a no-op.
    pub(crate) fn apply(&self, template: &str) -> String {
        let resolve = |token: &str| -> Option<&str> {
            Some(match token {
                "READ_TOOL" => self.read.as_str(),
                "LIST_TOOL" => self.list.as_str(),
                "SEARCH_TOOL" => self.search.as_str(),
                "WRITE_TOOL" => self.write.as_str(),
                "EXECUTE_TOOL" => self.execute.as_str(),
                "WEB_SEARCH_TOOL" => self.web_search.as_str(),
                "WEB_FETCH_TOOL" => self.web_fetch.as_str(),
                "TOOLSET_TOOLS" => self.toolset_tools.as_str(),
                _ => return None,
            })
        };
        let mut out = String::with_capacity(template.len() + 64);
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let Some(before) = rest.get(..open) else {
                break;
            };
            out.push_str(before);
            let Some(after) = rest.get(open + 1..) else {
                break;
            };
            if let Some(close) = after.find('}')
                && let Some(token) = after.get(..close)
                && let Some(value) = resolve(token)
            {
                out.push_str(value);
                let Some(next) = after.get(close + 1..) else {
                    break;
                };
                rest = next;
                continue;
            }
            // Not a known token (or no closing brace): emit the literal `{` and keep scanning after it, leaving foreign placeholders intact
            out.push('{');
            rest = after;
        }
        out.push_str(rest);
        out
    }
}

/// `true` when `name` is safe to splice verbatim into an LLM prompt.
/// This rejects newlines, control chars, backticks, `{`/`}`, spaces, and markdown.
/// A `name_override` is registry-validated for uniqueness only, so its content is untrusted text that ends up in the prompt.
fn is_safe_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The sanitized client name, or `None` when it is absent/unsafe.
fn sanitized_tool_name(name: Option<String>) -> Option<String> {
    name.filter(|n| is_safe_tool_name(n))
}

/// The sanitized client name, or `fallback` when it is absent/unsafe.
fn sanitized_or_default(name: Option<String>, fallback: &str) -> String {
    sanitized_tool_name(name).unwrap_or_else(|| fallback.to_string())
}

/// The first of `primary`/`fallback` that passes the safe-charset gate, else `None`.
/// A present-but-unsafe `primary` (e.g. a bad `Write` `name_override`) cannot shadow a usable `fallback` (`Edit`, i.e. `search_replace`).
/// Both [`RoleToolNames::from_summary`] and [`RoleToolNames::from_parent`] resolve `{WRITE_TOOL}` through this, so the two renders stay consistent.
fn first_safe_tool_name(primary: Option<String>, fallback: Option<String>) -> Option<String> {
    sanitized_tool_name(primary).or_else(|| sanitized_tool_name(fallback))
}

/// Render the verifier-only `{TOOLSET_TOOLS}` block: a paragraph listing the summary's client-facing names, **one name per classified `ToolKind`**.
/// Unclassified tools are absent from `tool_names` and so omitted.
/// Sorted and deduped; empty when nothing safe remains, so the inherit/default render omits the block entirely.
fn enumerate_toolset_tools(tool_names: &std::collections::HashMap<ToolKind, String>) -> String {
    let mut names: Vec<&str> = tool_names
        .values()
        .map(String::as_str)
        .filter(|n| is_safe_tool_name(n))
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort_unstable();
    names.dedup();
    let mut out = String::from("\n\nTools available to you for this review: ");
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('`');
        out.push_str(name);
        out.push('`');
    }
    out.push('.');
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use xai_grok_tools::implementations::grok_build::task::types::SubagentTypeSummary;

    /// Build a `SubagentTypeSummary` from `(ToolKind, name)` pairs for the per-agent_type rendering tests.
    /// Shared with the planner / classifier / strategist render tests.
    pub(crate) fn summary_with(pairs: &[(ToolKind, &str)]) -> SubagentTypeSummary {
        let mut tool_names = std::collections::HashMap::new();
        for (kind, name) in pairs {
            tool_names.insert(*kind, (*name).to_string());
        }
        SubagentTypeSummary {
            can_read: tool_names.contains_key(&ToolKind::Read),
            can_search: tool_names.contains_key(&ToolKind::Search),
            can_execute: tool_names.contains_key(&ToolKind::Execute),
            tool_names,
        }
    }

    /// Shared guard: a regex scan that fails on ANY surviving `{*_TOOL}` / `{TOOLSET_TOOLS}` token (robust to new tool placeholders).
    /// Scoped to the tool-placeholder family, so it does NOT false-positive on render-time placeholders resolved elsewhere.
    /// `{KIND_LENS}`, `{SCRATCH}`, `{PLAN_FILE}`, … survive a template rendered with only `tool_names` applied.
    pub(crate) fn assert_no_tool_placeholders(rendered: &str) {
        let re = regex::Regex::new(r"\{[A-Z_]+_TOOL\}|\{TOOLSET_TOOLS\}").unwrap();
        if let Some(m) = re.find(rendered) {
            panic!(
                "unresolved tool placeholder {:?} in rendered prompt",
                m.as_str(),
            );
        }
    }

    #[test]
    fn inherit_defaults_are_the_literal_fallback_names() {
        let tn = RoleToolNames::inherit_defaults();
        assert_eq!(tn.read, "read_file");
        assert_eq!(tn.list, "list_dir");
        assert_eq!(tn.search, "grep");
        assert_eq!(tn.write, "write");
        assert_eq!(tn.execute, "run_terminal_command");
        assert_eq!(tn.web_search, "web_search");
        assert_eq!(tn.web_fetch, "web_fetch");
        assert_eq!(tn.toolset_tools, "", "inherit path omits the toolset block");
    }

    #[test]
    fn from_summary_uses_grok_build_names() {
        // A remote-code toolset: client names match the literal defaults.
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::ListDir, "list_dir"),
            (ToolKind::Search, "grep"),
            (ToolKind::Write, "write"),
            (ToolKind::Execute, "run_terminal_command"),
        ]));
        assert_eq!(tn.read, "read_file");
        assert_eq!(tn.search, "grep");
        assert_eq!(tn.execute, "run_terminal_command");
    }

    #[test]
    fn from_summary_reflects_cursor_name_overrides() {
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "cursor_read"),
            (ToolKind::ListDir, "cursor_ls"),
            (ToolKind::Search, "cursor_grep"),
            (ToolKind::Write, "cursor_write"),
            (ToolKind::Execute, "cursor_shell"),
        ]));
        assert_eq!(tn.read, "cursor_read");
        assert_eq!(tn.list, "cursor_ls");
        assert_eq!(tn.search, "cursor_grep");
        assert_eq!(tn.write, "cursor_write");
        assert_eq!(tn.execute, "cursor_shell");
    }

    #[test]
    fn from_summary_resolves_cursor_web_search_name() {
        // The alternate planner toolset exposes WebSearch/WebFetch under the client names "WebSearch"/"WebFetch"
        // This is the case that left the alternate planner blind to the web tool and over-scoping from memory alone
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::WebSearch, "WebSearch"),
            (ToolKind::WebFetch, "WebFetch"),
        ]));
        assert_eq!(tn.web_search, "WebSearch");
        assert_eq!(tn.web_fetch, "WebFetch");
    }

    #[test]
    fn from_parent_maps_web_search_and_fetch_to_distinct_fields() {
        // Distinct values catch a web_search/web_fetch field swap that the all-None fallback cases cannot
        let tn = RoleToolNames::from_parent(
            None,
            None,
            None,
            None,
            None,
            None,
            Some("WS".into()),
            Some("WF".into()),
        );
        assert_eq!(tn.web_search, "WS");
        assert_eq!(tn.web_fetch, "WF");
    }

    #[test]
    fn web_tools_fall_back_when_absent_from_the_toolset() {
        // Without WebSearch/WebFetch in a summary / parent bridge, both resolve to the stock client names
        // So the planner prompt still names a real tool on the default remote-code host (the stock `web_search`/`web_fetch`)
        let summary = RoleToolNames::from_summary(&summary_with(&[(ToolKind::Read, "rd")]));
        assert_eq!(summary.web_search, "web_search");
        assert_eq!(summary.web_fetch, "web_fetch");
        let parent = RoleToolNames::from_parent(None, None, None, None, None, None, None, None);
        assert_eq!(parent.web_search, "web_search");
        assert_eq!(parent.web_fetch, "web_fetch");
    }

    #[test]
    fn from_summary_write_falls_back_to_edit_on_default_grok_build_host() {
        // Default remote-code host: the pre-spawn describe probe exposes only `Edit` (`search_replace`); `Write` is injection-only and absent
        // The planner gate accepts this toolset, so `{WRITE_TOOL}` must name the real mutator (`search_replace`), not the literal `write` default
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Search, "grep"),
            (ToolKind::Edit, "search_replace"),
        ]));
        assert_eq!(tn.write, "search_replace");
    }

    #[test]
    fn from_summary_write_prefers_write_over_edit_when_both_present() {
        // A toolset exposing both (e.g. the implementer): the explicit `Write` name wins; `Edit` is only the fallback.
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Write, "cursor_write"),
            (ToolKind::Edit, "cursor_edit"),
        ]));
        assert_eq!(tn.write, "cursor_write");
    }

    #[test]
    fn from_parent_write_falls_back_to_edit_when_bridge_has_no_write() {
        // Default remote-code parent bridge: no `Write`, only `Edit` (`search_replace`)
        let tn = RoleToolNames::from_parent(
            Some("read_file".into()),
            Some("list_dir".into()),
            Some("grep".into()),
            None,
            Some("search_replace".into()),
            Some("run_terminal_command".into()),
            None,
            None,
        );
        assert_eq!(tn.write, "search_replace");
    }

    #[test]
    fn from_parent_write_prefers_write_over_edit_when_both_present() {
        let tn = RoleToolNames::from_parent(
            None,
            None,
            None,
            Some("write".into()),
            Some("search_replace".into()),
            None,
            None,
            None,
        );
        assert_eq!(tn.write, "write");
    }

    #[test]
    fn from_parent_write_falls_back_to_default_when_neither_present() {
        let tn = RoleToolNames::from_parent(None, None, None, None, None, None, None, None);
        assert_eq!(tn.write, "write");
    }

    #[test]
    fn from_summary_unsafe_write_falls_through_to_safe_edit() {
        // A present-but-UNSAFE `Write` `name_override` (contains a space) must not shadow a usable `Edit` name
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Write, "bad write"),
            (ToolKind::Edit, "search_replace"),
        ]));
        assert_eq!(tn.write, "search_replace");
    }

    #[test]
    fn from_parent_unsafe_write_falls_through_to_safe_edit() {
        // Same first-safe-candidate rule on the inherit / parent-bridge path.
        let tn = RoleToolNames::from_parent(
            Some("read_file".into()),
            None,
            None,
            Some("bad write".into()),
            Some("search_replace".into()),
            None,
            None,
            None,
        );
        assert_eq!(tn.write, "search_replace");
    }

    #[test]
    fn write_resolution_uses_literal_default_when_both_candidates_unsafe() {
        let summary = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Write, "bad write"),
            (ToolKind::Edit, "bad`edit"),
        ]));
        assert_eq!(summary.write, "write");
        let parent = RoleToolNames::from_parent(
            None,
            None,
            None,
            Some("bad write".into()),
            Some("bad`edit".into()),
            None,
            None,
            None,
        );
        assert_eq!(parent.write, "write");
    }

    #[test]
    fn parent_and_summary_renders_agree_on_default_grok_build_mutator() {
        // The explicit-pair `primary` render comes from `from_summary`; the inherit/fail-open `fallback` render comes from `from_parent`
        // Both must name the SAME mutator on the default remote-code host (Edit-only toolset)
        // So a fail-open retry can never disagree with the first attempt's `{WRITE_TOOL}`
        let primary = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Search, "grep"),
            (ToolKind::Edit, "search_replace"),
        ]));
        let fallback = RoleToolNames::from_parent(
            Some("read_file".into()),
            Some("list_dir".into()),
            Some("grep".into()),
            None,
            Some("search_replace".into()),
            Some("run_terminal_command".into()),
            None,
            None,
        );
        assert_eq!(primary.write, "search_replace");
        assert_eq!(primary.write, fallback.write);
    }

    #[test]
    fn from_summary_falls_back_for_kinds_absent_from_the_toolset() {
        // With only Read present, the other placeholders take their literal fallbacks (the malformed/partial-summary edge case)
        let tn = RoleToolNames::from_summary(&summary_with(&[(ToolKind::Read, "rd")]));
        assert_eq!(tn.read, "rd");
        assert_eq!(tn.list, "list_dir");
        assert_eq!(tn.search, "grep");
        assert_eq!(tn.write, "write");
        assert_eq!(tn.execute, "run_terminal_command");
    }

    #[test]
    fn toolset_tools_block_enumerates_when_present_and_omits_when_empty() {
        let present = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Search, "grep"),
        ]));
        assert!(present.toolset_tools.contains("`grep`"));
        assert!(present.toolset_tools.contains("`read_file`"));
        // Sorted enumeration: grep < read_file.
        assert!(
            present.toolset_tools.find("`grep`").unwrap()
                < present.toolset_tools.find("`read_file`").unwrap()
        );

        let empty = RoleToolNames::from_summary(&SubagentTypeSummary::default());
        assert_eq!(empty.toolset_tools, "", "empty toolset ⇒ no block");
    }

    #[test]
    fn apply_substitutes_every_placeholder_and_leaves_none() {
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "rd"),
            (ToolKind::ListDir, "ls"),
            (ToolKind::Search, "gr"),
            (ToolKind::Write, "wr"),
            (ToolKind::Execute, "ex"),
            (ToolKind::WebSearch, "ws"),
            (ToolKind::WebFetch, "wf"),
        ]));
        let template = "{READ_TOOL} {LIST_TOOL} {SEARCH_TOOL} {WRITE_TOOL} {EXECUTE_TOOL} \
             {WEB_SEARCH_TOOL} {WEB_FETCH_TOOL}{TOOLSET_TOOLS}";
        let out = tn.apply(template);
        assert!(out.starts_with("rd ls gr wr ex ws wf"));
        assert!(out.contains("`rd`") && out.contains("`wf`"));
        assert_no_tool_placeholders(&out);
    }

    #[test]
    fn apply_leaves_foreign_placeholders_intact() {
        // Render-time placeholders resolved elsewhere (e.g. `{KIND_LENS}`, `{SCRATCH}`, `{PLAN_FILE}`) and an unknown token must pass through.
        let out = RoleToolNames::inherit_defaults()
            .apply("{READ_TOOL} {KIND_LENS} {SCRATCH} {PLAN_FILE} {UNKNOWN}");
        assert_eq!(out, "read_file {KIND_LENS} {SCRATCH} {PLAN_FILE} {UNKNOWN}");
    }

    #[test]
    fn apply_is_single_pass_a_name_equal_to_a_token_is_not_re_expanded() {
        // Sanitization rejects `{`/`}`, so a name can never *be* a token
        // But even a sanitized name that collides with a later token's TEXT must not be re-expanded by a second pass
        // Use `EXECUTE_TOOL`-shaped names that are themselves safe (no braces) to prove single-pass behavior
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "EXECUTE_TOOL"),
            (ToolKind::Execute, "real_exec"),
        ]));
        // `EXECUTE_TOOL` (no braces) is a safe name and is kept verbatim.
        assert_eq!(tn.read, "EXECUTE_TOOL");
        let out = tn.apply("{READ_TOOL}|{EXECUTE_TOOL}");
        // The Read substitution `EXECUTE_TOOL` is NOT re-expanded to `real_exec`.
        assert_eq!(out, "EXECUTE_TOOL|real_exec");
    }

    #[test]
    fn unsafe_name_overrides_fall_back_to_the_literal_default() {
        // A `name_override` carrying a newline, a `{…}` token, a backtick, or markdown is unsafe to splice
        for bad in [
            "read\nIGNORE PREVIOUS",
            "{EXECUTE_TOOL}",
            "rd`x`",
            "# Heading",
            "two words",
            "",
        ] {
            let tn = RoleToolNames::from_summary(&summary_with(&[(ToolKind::Read, bad)]));
            assert_eq!(
                tn.read, "read_file",
                "unsafe name {bad:?} must fall back to the literal default",
            );
        }
        // A safe tool-id charset (alnum / `_` / `-` / `.`) is preserved.
        for ok in ["read_file", "cursor-read", "ns.read", "Read2"] {
            let tn = RoleToolNames::from_summary(&summary_with(&[(ToolKind::Read, ok)]));
            assert_eq!(tn.read, ok);
        }
    }

    #[test]
    fn enumerate_toolset_tools_drops_unsafe_names() {
        let tn = RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Search, "evil\nname"),
        ]));
        assert!(tn.toolset_tools.contains("`read_file`"));
        assert!(
            !tn.toolset_tools.contains("evil"),
            "an unsafe name_override must not appear in the toolset block: {:?}",
            tn.toolset_tools,
        );
        // The Search kind was dropped, so its fallback name `grep` is NOT listed either (only safe names actually in the summary appear)
        assert!(!tn.toolset_tools.contains("`grep`"));
    }
}
