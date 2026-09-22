// Modified for Distill by Samuel Fajreldines, 2026.
//! User-message construction for `SessionActor`.
//! Covers the templated prefix, rules partitioning, and image payload preparation; large-prompt offload lives in `prompt_offload`.
#![allow(clippy::items_after_test_module)]
use super::*;
use crate::session::repo_status_prefix::RepoStatusSnapshot;
/// Replaces anything outside `[A-Za-z0-9._-]` with `_` so the result is a portable directory name on macOS/Linux.
/// Whether `url` is an `http://` or `https://` URL, one the upstream API can fetch directly.
/// `file://` and other local schemes are rejected by the API and must be inlined as a `data:` URL instead.
pub(super) fn is_remote_image_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
/// The remote API accepts only a base64 `data:` URL or an HTTP(S) URL; `file://` and other local schemes return 400.
/// Inline bytes win when present (the canonical payload); `uri` is forwarded directly only when it is a remote URL with no inline bytes.
/// Extracted so production and the regression tests assert against the same selector, and a rule change cannot drift past the tests.
pub(super) fn pick_user_image_url(image: &agent_client_protocol::ImageContent) -> String {
    if let Some(uri) = image.uri.as_deref()
        && image.data.is_empty()
        && is_remote_image_url(uri)
    {
        uri.to_owned()
    } else {
        format!("data:{};base64,{}", image.mime_type, image.data)
    }
}
/// A `Configured` file is a user rule wherever its dir sits; everything else is scoped by path.
fn partition_rules_by_scope(
    files: Vec<distill_agent::prompt::agents_md::AgentConfigFile>,
    distill_home: &std::path::Path,
    vendor_homes: &[(std::path::PathBuf, bool)],
    workspace_roots: &[&std::path::Path],
) -> (
    Vec<distill_agent::prompt::user_message::RuleEntry>,
    Vec<distill_agent::prompt::user_message::RuleEntry>,
) {
    let mut workspace = Vec::new();
    let mut user = Vec::new();
    for file in files {
        let is_user_rule = file.source
            == distill_agent::prompt::agents_md::InstructionSource::Configured
            || crate::util::is_user_instruction_path(
                std::path::Path::new(&file.file_path),
                distill_home,
                vendor_homes,
                workspace_roots,
            );
        let entry = distill_agent::prompt::user_message::RuleEntry::from(file);
        if is_user_rule {
            user.push(entry);
        } else {
            workspace.push(entry);
        }
    }
    (workspace, user)
}
#[cfg(test)]
mod partition_rules_by_scope_tests {
    use super::partition_rules_by_scope;
    use distill_agent::prompt::agents_md::AgentConfigFile;
    use std::path::Path;
    fn file(path: &str) -> AgentConfigFile {
        AgentConfigFile {
            file_name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            file_path: path.to_string(),
            content: path.to_string(),
            source: Default::default(),
        }
    }
    #[test]
    fn configured_rule_dir_is_a_user_rule_wherever_it_sits() {
        use distill_agent::prompt::agents_md::InstructionSource;
        let configured = |path: &str| AgentConfigFile {
            source: InstructionSource::Configured,
            ..file(path)
        };
        let files = vec![
            configured("/home/user/team-rules/a.md"),
            // A configured dir under the workspace that a project root also covers
            configured("/repo/.claude/rules/b.md"),
            file("/repo/src/AGENTS.md"),
        ];
        let (workspace, user) = partition_rules_by_scope(
            files,
            Path::new("/home/user/.grok"),
            &[],
            &[Path::new("/repo")],
        );
        assert_eq!(vec!["/repo/src/AGENTS.md"], paths(&workspace));
        assert_eq!(
            vec!["/home/user/team-rules/a.md", "/repo/.claude/rules/b.md"],
            paths(&user)
        );
    }
    fn paths(entries: &[distill_agent::prompt::user_message::RuleEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.content.as_str()).collect()
    }
    #[test]
    fn partitions_custom_grok_and_vendor_home_rules_as_user_scope() {
        let files = vec![
            file("/custom/config/rules/a.md"),
            file("/home/user/.cursor/rules/b.md"),
            file("/repo/.cursor/rules/c.md"),
            file("/repo/src/AGENTS.md"),
            file("/custom/config/rules/d.md"),
        ];
        let vendor_homes = vec![
            (Path::new("/home/user/.claude").to_path_buf(), true),
            (Path::new("/home/user/.cursor").to_path_buf(), true),
        ];
        let (workspace, user) = partition_rules_by_scope(
            files,
            Path::new("/custom/config"),
            &vendor_homes,
            &[Path::new("/repo")],
        );
        assert_eq!(
            paths(&user),
            vec![
                "/custom/config/rules/a.md",
                "/home/user/.cursor/rules/b.md",
                "/custom/config/rules/d.md",
            ]
        );
        assert_eq!(
            paths(&workspace),
            vec!["/repo/.cursor/rules/c.md", "/repo/src/AGENTS.md"]
        );
    }
    #[test]
    fn distill_home_nested_in_workspace_keeps_direct_surfaces_user_scoped() {
        let files = vec![
            file("/repo/config/AGENTS.md"),
            file("/repo/config/rules/global.md"),
            file("/repo/config/.grok/rules/project.md"),
            file("/repo/config/src/AGENTS.md"),
        ];
        let (workspace, user) =
            partition_rules_by_scope(files, Path::new("/repo/config"), &[], &[Path::new("/repo")]);
        assert_eq!(
            paths(&user),
            vec!["/repo/config/AGENTS.md", "/repo/config/rules/global.md"]
        );
        assert_eq!(
            paths(&workspace),
            vec![
                "/repo/config/.grok/rules/project.md",
                "/repo/config/src/AGENTS.md",
            ]
        );
    }
    #[test]
    fn vendor_home_nested_in_workspace_keeps_direct_surfaces_user_scoped() {
        let files = vec![
            file("/repo/.claude/rules/global.md"),
            file("/repo/.claude/CLAUDE.md"),
            file("/repo/.claude/.claude/rules/project.md"),
            file("/repo/.claude/src/AGENTS.md"),
        ];
        let vendor_homes = vec![(Path::new("/repo/.claude").to_path_buf(), true)];
        let (workspace, user) = partition_rules_by_scope(
            files,
            Path::new("/other/grok"),
            &vendor_homes,
            &[Path::new("/repo")],
        );
        assert_eq!(
            paths(&user),
            vec!["/repo/.claude/rules/global.md", "/repo/.claude/CLAUDE.md"]
        );
        assert_eq!(
            paths(&workspace),
            vec![
                "/repo/.claude/.claude/rules/project.md",
                "/repo/.claude/src/AGENTS.md",
            ]
        );
    }
    #[test]
    fn nested_distill_home_workspace_files_stay_workspace_scoped() {
        let files = vec![
            file("/custom/grok/rules/global.md"),
            file("/custom/grok/worktrees/repo/.cursor/rules/project.md"),
            file("/custom/grok/worktrees/repo/src/AGENTS.md"),
        ];
        let (workspace, user) = partition_rules_by_scope(
            files,
            Path::new("/custom/grok"),
            &[],
            &[Path::new("/custom/grok/worktrees/repo")],
        );
        assert_eq!(paths(&user), vec!["/custom/grok/rules/global.md"]);
        assert_eq!(
            paths(&workspace),
            vec![
                "/custom/grok/worktrees/repo/.cursor/rules/project.md",
                "/custom/grok/worktrees/repo/src/AGENTS.md",
            ]
        );
    }
    #[test]
    fn partitioned_snapshot_bodies_match_rules_and_old_reminder() {
        let files = vec![
            AgentConfigFile {
                file_name: "AGENTS.md".into(),
                file_path: "/repo/AGENTS.md".into(),
                content: "repo-agents-body".into(),
                source: Default::default(),
            },
            AgentConfigFile {
                file_name: "CLAUDE.md".into(),
                file_path: "/repo/CLAUDE.md".into(),
                content: "repo-claude-body".into(),
                source: Default::default(),
            },
            AgentConfigFile {
                file_name: "AGENTS.md".into(),
                file_path: "/home/user/.grok/AGENTS.md".into(),
                content: "home-grok-body".into(),
                source: Default::default(),
            },
            AgentConfigFile {
                file_name: "CLAUDE.md".into(),
                file_path: "/home/user/.claude/CLAUDE.md".into(),
                content: "home-claude-body".into(),
                source: Default::default(),
            },
            AgentConfigFile {
                file_name: "x.md".into(),
                file_path: "/repo/.grok/rules/x.md".into(),
                content: "repo-grok-rules-x".into(),
                source: Default::default(),
            },
        ];
        let vendor_homes = vec![(Path::new("/home/user/.claude").to_path_buf(), true)];
        let (workspace, user) = partition_rules_by_scope(
            files.clone(),
            Path::new("/home/user/.grok"),
            &vendor_homes,
            &[Path::new("/repo")],
        );
        let rules =
            distill_agent::prompt::user_message::format_rules_section(&workspace, &user).unwrap();
        let reminder = distill_agent::prompt::agents_md::format_agents_md_section(&files).unwrap();
        for body in [
            "repo-agents-body",
            "repo-claude-body",
            "home-grok-body",
            "home-claude-body",
            "repo-grok-rules-x",
        ] {
            assert!(rules.contains(body), "rules missing {body}");
            assert!(reminder.contains(body), "reminder missing {body}");
        }
        assert!(rules.contains("name=\"/repo/AGENTS.md\""));
        assert!(rules.contains("name=\"/repo/CLAUDE.md\""));
        assert!(rules.contains("name=\"/repo/.grok/rules/x.md\""));
        assert!(rules.contains("<user_rule>\nhome-grok-body\n</user_rule>"));
        assert!(rules.contains("<user_rule>\nhome-claude-body\n</user_rule>"));
        assert!(!rules.contains("## From:"));
        assert!(!rules.contains("<system-reminder>"));
    }
    #[test]
    fn fork_ondisk_and_display_prefixes_both_count_as_workspace() {
        let files = vec![
            file("/home/user/.grok/worktrees/repo/AGENTS.md"),
            file("/home/user/repo/crates/foo/AGENTS.md"),
            file("/home/user/.grok/AGENTS.md"),
        ];
        let (workspace, user) = partition_rules_by_scope(
            files,
            Path::new("/home/user/.grok"),
            &[],
            &[
                Path::new("/home/user/.grok/worktrees/repo"),
                Path::new("/home/user/repo/crates/foo"),
            ],
        );
        assert_eq!(
            paths(&workspace),
            vec![
                "/home/user/.grok/worktrees/repo/AGENTS.md",
                "/home/user/repo/crates/foo/AGENTS.md",
            ]
        );
        assert_eq!(paths(&user), vec!["/home/user/.grok/AGENTS.md"]);
    }
}
/// True iff `conversation` already contains a project-instructions reminder (see [`is_project_instructions`]).
/// `spawn_session_actor` uses this for idempotent AGENTS.md injection, so resumed sessions and forks don't duplicate the message.
pub(super) fn conversation_has_project_instructions(conversation: &[ConversationItem]) -> bool {
    conversation.iter().any(is_project_instructions)
}
/// A project-instructions (AGENTS.md) reminder is a `User` item tagged [`SyntheticReason::ProjectInstructions`], or a legacy untagged copy.
/// The legacy copy's first text part starts with [`LEGACY_AGENTS_MD_REMINDER_PREFIX`].
/// Single source of truth for both spawn-time idempotent injection and the compaction de-dup.
pub(super) fn is_project_instructions(item: &ConversationItem) -> bool {
    let ConversationItem::User(u) = item else {
        return false;
    };
    if u.synthetic_reason == SyntheticReason::ProjectInstructions {
        return true;
    }
    u.content
        .first()
        .and_then(|p| match p {
            ContentPart::Text { text } => Some(text.as_ref()),
            _ => None,
        })
        .is_some_and(|t| t.starts_with(LEGACY_AGENTS_MD_REMINDER_PREFIX))
}
/// Subagent spawns (including `resume_from`) overwrite the leading System with the fresh prompt; top-level user-resumed sessions keep theirs.
/// When no System is present, insert one and grow the preserved prefix.
pub(super) fn install_system_prompt(
    conversation: &mut Vec<ConversationItem>,
    inherited_prefix_len: &mut Option<usize>,
    is_subagent_spawn: bool,
    preserve_inherited_system: bool,
    system_prompt: &str,
) {
    if let Some(ConversationItem::System(sys)) = conversation.first_mut() {
        if is_subagent_spawn && !preserve_inherited_system {
            sys.content = std::sync::Arc::<str>::from(system_prompt);
        }
    } else {
        conversation.insert(0, ConversationItem::system(system_prompt.to_string()));
        if let Some(len) = inherited_prefix_len {
            *len += 1;
        }
    }
}
/// A resumed head keeps its enriched prompt, but its `<memory>` section must match this
/// process's memory state: a `/memory` toggle persists the section into the head, while
/// enablement is re-resolved from config on every spawn. On a mismatch the fresh prompt replaces
/// the head; an injected manifest block is kept only while memory is on. Returns whether it changed.
pub(super) fn reconcile_resumed_memory_section(
    conversation: &mut [ConversationItem],
    system_prompt: &str,
) -> bool {
    let Some(ConversationItem::System(sys)) = conversation.first_mut() else {
        return false;
    };
    let manifest_start = sys
        .content
        .find(distill_chat_state::MEMORY_CONTEXT_OPEN_TAG);
    let head_prompt = manifest_start
        .and_then(|start| sys.content.get(..start))
        .unwrap_or(&sys.content);
    let fresh_has_memory = has_memory_section(system_prompt);
    if has_memory_section(head_prompt) == fresh_has_memory {
        return false;
    }
    let manifest_block = manifest_start
        .filter(|_| fresh_has_memory)
        .and_then(|start| sys.content.get(start..))
        .map(str::to_owned);
    sys.content = match manifest_block {
        Some(block) => std::sync::Arc::<str>::from(format!(
            "{}\n\n{block}",
            system_prompt.trim_end_matches('\n')
        )),
        None => std::sync::Arc::<str>::from(system_prompt),
    };
    true
}
/// The `<memory>` block rendered by `templates/prompt.md` when `memory_v2_enabled` is set.
fn has_memory_section(prompt: &str) -> bool {
    prompt.contains("\n<memory>\n")
}
#[cfg(test)]
mod reconcile_resumed_memory_section_tests {
    use super::reconcile_resumed_memory_section;
    use distill_chat_state::MEMORY_CONTEXT_OPEN_TAG;
    use distill_sampling_types::conversation::ConversationItem;
    const WITH_MEMORY: &str = "rules\n\n<memory>\nuse memory\n</memory>\n\nmore";
    const WITHOUT_MEMORY: &str = "rules\n\nmore";
    fn head(conv: &[ConversationItem]) -> &str {
        match conv.first() {
            Some(ConversationItem::System(s)) => s.content.as_ref(),
            _ => panic!("first item is not System"),
        }
    }
    #[test]
    fn matching_heads_are_left_alone() {
        let mut conv = vec![ConversationItem::system(format!(
            "{WITH_MEMORY} (enriched)"
        ))];
        assert!(!reconcile_resumed_memory_section(&mut conv, WITH_MEMORY));
        assert_eq!(head(&conv), format!("{WITH_MEMORY} (enriched)"));
        let mut conv = vec![ConversationItem::system(WITHOUT_MEMORY)];
        assert!(!reconcile_resumed_memory_section(&mut conv, WITHOUT_MEMORY));
    }
    #[test]
    fn memory_now_off_drops_section_and_manifest() {
        let manifest = format!("{MEMORY_CONTEXT_OPEN_TAG}\nindex\n</memory-context>");
        let mut conv = vec![ConversationItem::system(format!(
            "{WITH_MEMORY}\n\n{manifest}"
        ))];
        assert!(reconcile_resumed_memory_section(&mut conv, WITHOUT_MEMORY));
        assert_eq!(head(&conv), WITHOUT_MEMORY);
    }
    #[test]
    fn memory_now_on_adds_section_and_keeps_manifest() {
        let manifest = format!("{MEMORY_CONTEXT_OPEN_TAG}\nindex\n</memory-context>");
        let mut conv = vec![ConversationItem::system(format!(
            "{WITHOUT_MEMORY}\n\n{manifest}"
        ))];
        assert!(reconcile_resumed_memory_section(&mut conv, WITH_MEMORY));
        assert_eq!(head(&conv), format!("{WITH_MEMORY}\n\n{manifest}"));
    }
    #[test]
    fn memory_word_inside_manifest_does_not_count_as_a_section() {
        let manifest = format!("{MEMORY_CONTEXT_OPEN_TAG}\nnote says\n<memory>\n</memory-context>");
        let mut conv = vec![ConversationItem::system(format!(
            "{WITHOUT_MEMORY}\n\n{manifest}"
        ))];
        assert!(!reconcile_resumed_memory_section(&mut conv, WITHOUT_MEMORY));
    }
}
#[cfg(test)]
mod install_system_prompt_tests {
    use super::install_system_prompt;
    use distill_sampling_types::conversation::ConversationItem;
    fn system_text(item: &ConversationItem) -> &str {
        match item {
            ConversationItem::System(s) => s.content.as_ref(),
            _ => panic!("first item is not System"),
        }
    }
    #[test]
    fn subagent_spawn_overwrites_leading_system() {
        let mut conv = vec![
            ConversationItem::system("old"),
            ConversationItem::user("hi"),
        ];
        let mut prefix = Some(1);
        install_system_prompt(&mut conv, &mut prefix, true, false, "fresh");
        assert_eq!(conv.first().map(system_text), Some("fresh"));
        assert_eq!(prefix, Some(1), "prefix unchanged — System already present");
    }
    #[test]
    fn preserve_inherited_system_keeps_head_untouched() {
        let mut conv = vec![
            ConversationItem::system("parent system verbatim"),
            ConversationItem::user("hi"),
        ];
        let mut prefix = Some(2);
        install_system_prompt(&mut conv, &mut prefix, true, true, "child fresh prompt");
        assert_eq!(
            conv.first().map(system_text),
            Some("parent system verbatim"),
            "preserve_inherited_system must not overwrite the inherited head"
        );
        assert_eq!(prefix, Some(2), "prefix unchanged — System already present");
    }
    #[test]
    fn summarized_fork_system_placeholder_is_overwritten() {
        let mut conv = vec![
            ConversationItem::system("parent system placeholder"),
            ConversationItem::user("<background_context>…</background_context>"),
        ];
        let mut prefix = Some(2);
        install_system_prompt(&mut conv, &mut prefix, true, false, "child subagent system");
        assert_eq!(
            conv.first().map(system_text),
            Some("child subagent system"),
            "summarized fork (preserve=false) must overwrite [0] with the child system"
        );
    }
    #[test]
    fn top_level_resume_keeps_stored_system() {
        let mut conv = vec![
            ConversationItem::system("stored"),
            ConversationItem::user("hi"),
        ];
        let mut prefix = None;
        install_system_prompt(&mut conv, &mut prefix, false, false, "fresh");
        assert_eq!(conv.first().map(system_text), Some("stored"));
    }
    #[test]
    fn inserts_system_and_bumps_prefix_when_absent() {
        let mut conv = vec![ConversationItem::user("hi")];
        let mut prefix = Some(0);
        install_system_prompt(&mut conv, &mut prefix, true, false, "fresh");
        assert_eq!(conv.first().map(system_text), Some("fresh"));
        assert_eq!(
            prefix,
            Some(1),
            "inserted System grows the preserved prefix"
        );
    }
}
impl SessionActor {
    /// Rewrite the user-message prefix at conversation index 1.
    /// Caller must guarantee zero turns.
    /// When `drop_startup_skill_reminder` is true, also strips the synthetic `<system-reminder>` user item.
    pub(super) fn rewrite_zero_turn_prefix(
        conversation: &mut Vec<ConversationItem>,
        new_prefix: String,
        drop_startup_skill_reminder: bool,
    ) {
        let is_prefix_slot = matches!(
            conversation.get(1),
            Some(ConversationItem::User(u)) if u.synthetic_reason.is_human()
        );
        if is_prefix_slot {
            if let Some(slot) = conversation.get_mut(1) {
                *slot = ConversationItem::user(new_prefix);
            }
        } else {
            let insert_at = conversation.len().min(1);
            conversation.insert(insert_at, ConversationItem::user(new_prefix));
        }
        if drop_startup_skill_reminder {
            conversation.retain(|item| {
                !matches!(
                    item,
                    ConversationItem::User(u)
                        if u.synthetic_reason
                            == distill_sampling_types::SyntheticReason::SystemReminder
                )
            });
        }
    }
    #[tracing::instrument(skip_all)]
    pub(super) async fn build_user_message_prefix(&self) -> String {
        self.build_user_message_prefix_with_skill_rows(None).await
    }
    /// Render the first-user-message prefix, optionally replacing the
    /// renderer-owned skill rows with a model-facing projection. The custom
    /// template still owns all surrounding text and project/user instructions.
    #[tracing::instrument(skip_all)]
    pub(super) async fn build_user_message_prefix_with_skill_rows(
        &self,
        skill_listing_rows: Option<&str>,
    ) -> String {
        let display_path = self
            .display_cwd
            .get()
            .map(|s| s.as_str())
            .unwrap_or(&self.session_info.cwd);
        let cwd = std::path::Path::new(display_path);
        use distill_agent::prompt::user_message::UserMessageTemplate;
        let (template, include_verification) = {
            let agent = self.agent.borrow();
            let def = agent.definition();
            (
                def.user_message_template.clone(),
                def.include_browser_verification(),
            )
        };
        let repo_status = self.resolve_repo_status_prefix().await;
        let mut prefix_carries_fallback_date = false;
        let mut out = if !matches!(template, UserMessageTemplate::Default) {
            if let Some(rendered) = self
                .build_templated_user_message(
                    cwd,
                    template.clone(),
                    repo_status.as_ref(),
                    skill_listing_rows,
                )
                .await
            {
                rendered
            } else {
                tracing::warn!(
                    "templated user message render failed; falling back to legacy prefix"
                );
                prefix_carries_fallback_date = !template.surfaces_local_date();
                self.construct_legacy_prefix(cwd, repo_status.as_ref())
            }
        } else {
            self.construct_legacy_prefix(cwd, repo_status.as_ref())
        };
        if matches!(template, UserMessageTemplate::Default) && include_verification {
            let (workspace_rules, mut user_rules) = self.gather_partitioned_rules();
            user_rules.splice(
                0..0,
                distill_agent::prompt::browser_verification::synthetic_user_rules(),
            );
            distill_agent::prompt::user_message::append_rules_section(
                &mut out,
                &workspace_rules,
                &user_rules,
            );
        }
        self.last_announced_local_date
            .set(chrono::Local::now().date_naive());
        self.prefix_carries_fallback_date
            .set(prefix_carries_fallback_date);
        out
    }
    fn gather_partitioned_rules(
        &self,
    ) -> (
        Vec<distill_agent::prompt::user_message::RuleEntry>,
        Vec<distill_agent::prompt::user_message::RuleEntry>,
    ) {
        let files = self.agent.borrow().prompt_context().agents_md_files.clone();
        let distill_home = distill_config::distill_home();
        let vendor_homes = distill_dirs::home_dir()
            .map(|home_dir| {
                vec![
                    (
                        home_dir.join(".claude"),
                        self.rebuild_spec.compat.claude.agents,
                    ),
                    (
                        home_dir.join(".cursor"),
                        self.rebuild_spec.compat.cursor.agents,
                    ),
                ]
            })
            .unwrap_or_default();
        let on_disk_cwd = std::path::Path::new(&self.session_info.cwd);
        let on_disk_root = git2::Repository::discover(on_disk_cwd)
            .ok()
            .and_then(|repo| repo.workdir().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| on_disk_cwd.to_path_buf());
        let display_root = self
            .display_cwd
            .get()
            .map(|s| std::path::PathBuf::from(s.as_str()));
        let workspace_roots: Vec<&std::path::Path> = display_root
            .as_deref()
            .into_iter()
            .chain(std::iter::once(on_disk_root.as_path()))
            .collect();
        partition_rules_by_scope(files, &distill_home, &vendor_homes, &workspace_roots)
    }
    /// Build the custom-templated first user message.
    /// Gathers session-scoped inputs: today's date, VCS status, AGENTS.md rules, skill registry, and MCP servers.
    /// Dispatches through `UserMessageContext::render`.
    async fn build_templated_user_message(
        &self,
        cwd: &std::path::Path,
        template: distill_agent::prompt::user_message::UserMessageTemplate,
        repo_status: Option<&RepoStatusSnapshot>,
        skill_listing_rows: Option<&str>,
    ) -> Option<String> {
        use distill_agent::prompt::user_message::UserMessageContext;
        self.wait_for_mcp_startup_grace().await;
        let bridge = self.agent.borrow().tool_bridge().clone();
        let (vcs_root, vcs_status) = match repo_status {
            Some(snapshot) => (snapshot.root.clone(), snapshot.templated_status()),
            None => (None, None),
        };
        let (workspace_rules, user_rules) = self.gather_partitioned_rules();
        let mut user_rules = user_rules;
        let skills = self.slash_skills_for_resolve().await;
        let mcp_servers = self.gather_mcp_servers(cwd).await;
        if self
            .agent
            .borrow()
            .definition()
            .include_browser_verification()
        {
            user_rules.splice(
                0..0,
                distill_agent::prompt::browser_verification::synthetic_user_rules(),
            );
        }
        let shell = resolve_session_shell();
        let today_local = chrono::Local::now().date_naive();
        let mcps_root = Self::workspace_mcps_root(cwd).map(|p| p.to_string_lossy().to_string());
        #[allow(unused_variables)]
        let is_cursor_template = crate::session::is_cursor_user_template(&template);
        let terminals_folder = None;
        let skill_listing_budget_chars = None;
        let ctx = UserMessageContext {
            template: template.clone(),
            workspace_path: cwd.to_path_buf(),
            os_family: crate::util::uname::os_kernel_and_release(),
            shell,
            vcs_root,
            vcs_status,
            today_local: Some(today_local),
            terminals_folder,
            workspace_rules,
            user_rules,
            skills,
            skill_listing_budget_chars,
            skill_listing_rows_override: skill_listing_rows.map(str::to_owned),
            mcp_servers,
            mcps_root,
            read_tool_name: bridge
                .render_prompt(
                    "${{ tools.by_kind.read }}",
                    &serde_json::Value::Object(Default::default()),
                )
                .await
                .unwrap_or_else(|| "Read".to_string()),
        };
        ctx.render(&bridge).await
    }
    fn construct_legacy_prefix(
        &self,
        cwd: &std::path::Path,
        repo_status: Option<&RepoStatusSnapshot>,
    ) -> String {
        let mut prefix = construct_user_message_minimal(cwd, None);
        if let Some(status) = repo_status.and_then(|s| s.legacy_status()) {
            prefix.push_str(&crate::session::user_message::format_vcs_status_block(
                &status,
                self.vcs_kind,
            ));
        }
        prefix
    }
    async fn resolve_repo_status_prefix(&self) -> Option<RepoStatusSnapshot> {
        use crate::session::repo_status_prefix::{
            REPO_STATUS_WAIT_BUDGET, RepoStatusPlan, gather_repo_status,
        };
        match self.repo_status_prefetch.plan() {
            RepoStatusPlan::NoRepo => None,
            RepoStatusPlan::RootOnly { root, vcs_kind } => {
                Some(RepoStatusSnapshot::root_only(root.clone(), *vcs_kind))
            }
            RepoStatusPlan::Gather { inputs, .. } => {
                use tracing::Instrument;
                let wait_start = std::time::Instant::now();
                let snapshot = match self.repo_status_prefetch.take_prefetch() {
                    Some(mut prefetch) => {
                        prefetch
                            .snapshot_within(REPO_STATUS_WAIT_BUDGET)
                            .instrument(tracing::info_span!("prompt.repo_status_wait"))
                            .await
                    }
                    None => gather_repo_status(inputs).await,
                };
                self.log_repo_status_wait(wait_start.elapsed());
                Some(snapshot.unwrap_or_else(|| inputs.missed_snapshot()))
            }
        }
    }
    fn log_repo_status_wait(&self, waited: std::time::Duration) {
        let wait_ms = self.repo_status_prefetch.record_wait(waited);
        if wait_ms > 0 {
            tracing::info!(
                session_id = %self.session_info.id.0,
                repo_status_wait_ms = wait_ms,
                "user prefix waited on the repo status prefetch"
            );
        }
    }
    /// `None` twin: descriptor materialization is unavailable in this build.
    fn workspace_mcps_root(_cwd: &std::path::Path) -> Option<std::path::PathBuf> {
        None
    }
    /// Only servers that expose tools or resources get files; models read these before issuing `CallMcpTool` or `FetchMcpResource` calls.
    /// The model then sees an empty descriptor directory rather than a missing one.
    /// No-op when the descriptor root is unavailable (`workspace_mcps_root` is `None`).
    async fn gather_mcp_servers(
        &self,
        workspace: &std::path::Path,
    ) -> Vec<distill_agent::prompt::user_message::McpServerEntry> {
        use distill_agent::prompt::user_message::McpServerEntry;
        let mcps_root = Self::workspace_mcps_root(workspace);
        let clients: Vec<(
            String,
            std::sync::Arc<crate::session::mcp_servers::McpClient>,
        )> = {
            let state = self.mcp_state.lock().await;
            tracing::debug!(
                session_id = %self.session_info.id.0,
                client_count = state.owned_clients.len() + state.shared_clients.len(),
                initializing_count = state.handshaking_servers_count(),
                finished_init = state.has_finished_init(),
                config_count = state.configs.len(),
                "gather_mcp_servers: snapshotting MCP state for user preamble render"
            );
            state
                .all_clients()
                .filter(|(n, _)| !state.has_failure_record(n))
                .map(|(n, c)| (n.clone(), std::sync::Arc::clone(c)))
                .collect()
        };
        let mut ready_clients = Vec::with_capacity(clients.len());
        for (name, client) in clients {
            if client.is_ready().await {
                ready_clients.push((name, client));
            }
        }
        let clients = ready_clients;
        let mut entries: Vec<McpServerEntry> = Vec::with_capacity(clients.len());
        for (name, client) in &clients {
            let instructions = client.server_instructions().await;
            let server_dir = mcps_root
                .as_deref()
                .map(|root| crate::session::mcp_descriptors::server_descriptor_dir(root, name));
            entries.push(McpServerEntry {
                name: name.clone(),
                server_use_instructions: instructions.filter(|s: &String| !s.trim().is_empty()),
                folder_path: server_dir.map(|d| d.to_string_lossy().to_string()),
            });
        }
        let gateway_entries = self.gather_gateway_mcp_servers(mcps_root.as_deref()).await;
        entries.extend(gateway_entries);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
    async fn gather_gateway_mcp_servers(
        &self,
        mcps_root: Option<&std::path::Path>,
    ) -> Vec<distill_agent::prompt::user_message::McpServerEntry> {
        use distill_agent::prompt::user_message::McpServerEntry;
        let disabled_gateway_tools = crate::util::config::get_all_mcp_disabled_tools(
            std::path::Path::new(&self.session_info.cwd),
        );
        let catalog = {
            let state = self.managed_mcp_handle.lock().await;
            if state.gateway_tools_active {
                match &state.gateway_tool_cache {
                    crate::session::managed_mcp::GatewayToolCatalogCache::Ready(catalog) => {
                        Some(catalog.clone())
                    }
                    _ => None,
                }
            } else {
                None
            }
        };
        let Some(catalog) = catalog else {
            return Vec::new();
        };
        let mut connectors = std::collections::BTreeMap::<String, String>::new();
        let mut gateway_connectors: Vec<String> = catalog
            .tools
            .iter()
            .map(|tool| tool.connector_id.clone())
            .collect();
        gateway_connectors.sort_unstable();
        gateway_connectors.dedup();
        let mut descriptors = Vec::new();
        for tool in &catalog.tools {
            if gateway_tool_is_disabled(tool, &disabled_gateway_tools) {
                continue;
            }
            connectors
                .entry(tool.connector_id.clone())
                .or_insert_with(|| tool.connector_name.clone());
            descriptors.push(crate::session::mcp_descriptors::GatewayToolDescriptor {
                connector_id: tool.connector_id.clone(),
                tool_id: tool.tool_id.clone(),
                description: tool.description.clone(),
                json_schema: tool.json_schema.clone(),
            });
        }
        connectors
            .into_iter()
            .map(|(connector_id, connector_name)| McpServerEntry {
                folder_path: mcps_root.map(|root| {
                    crate::session::mcp_descriptors::server_descriptor_dir(root, &connector_id)
                        .to_string_lossy()
                        .to_string()
                }),
                name: connector_id,
                server_use_instructions: (!connector_name.trim().is_empty())
                    .then_some(connector_name),
            })
            .collect()
    }
    /// Build a `PathRewriter` for sanitizing overlay paths in model-facing text.
    /// Returns `None` when `display_cwd` is unset (no rewriting needed).
    /// Tool-result handlers use it to rewrite prompt_text, error messages, and any other model-visible content that may embed the real worktree cwd.
    pub(super) fn path_rewriter(&self) -> Option<crate::session::acp_conversion::PathRewriter> {
        crate::session::acp_conversion::PathRewriter::new(
            &self.session_info.cwd,
            self.display_cwd.get().map(|s| s.as_str()),
        )
    }
    /// Add a followup message from the permission panel as a user turn in the conversation.
    /// This sends the message to the scrollback and adds it to the conversation context.
    pub(super) async fn add_followup_message_as_user_turn(&self, message: &str) {
        self.inject_synthetic_user_message(
            message,
            ConversationItem::user(message.to_string()),
            true,
            &[],
        )
        .await;
    }
    /// Run the image-transcription pipeline for a turn that contains user-supplied images.
    /// Returns the new `user_message` text with the `<image>` and `<image_files>` envelopes prepended.
    /// On any failure returns an `acp::Error` so the entire turn is aborted; we never silently drop image context.
    pub(super) async fn transcribe_user_images(
        &self,
        original_user_message: String,
        images: &[agent_client_protocol::ImageContent],
    ) -> Result<String, acp::Error> {
        let prior = self.chat_state_handle.get_conversation().await;
        let outline = crate::session::image_describe::build_conversation_outline(&prior);
        let session_dir = crate::session::persistence::ensure_owner_only_session_dir(
            &crate::session::info::Info {
                id: self.session_info.id.clone(),
                cwd: self.session_info.cwd.clone(),
            },
        )
        .map_err(|e| {
            acp::Error::internal_error().data(format!("failed to create session dir: {e}"))
        })?;
        let persisted = crate::session::image_describe::persist_user_images(&session_dir, images)
            .map_err(|e| {
            acp::Error::internal_error()
                .data(format!("failed to save user images to assets dir: {e}"))
        })?;
        let image_paths: Vec<String> = persisted
            .iter()
            .map(|p| p.path.to_string_lossy().into_owned())
            .collect();
        let current_query = crate::session::image_describe::strip_template_context_tags(
            &distill_chat_state::compaction_utils::extract_user_query(&original_user_message),
        );
        let active_session_config = self.reconstruct_full_config().await;
        let resolved_describe = self
            .resolve_aux_sampler_config(&self.image_description_model)
            .await;
        let (describe_model, sampler_config) =
            crate::agent::config::finalize_image_describe_sampler_config(
                resolved_describe,
                &active_session_config,
                self.client_identifier.clone(),
                Some(self.max_retries),
            );
        let client = distill_sampler::SamplingClient::new(sampler_config).map_err(|e| {
            acp::Error::internal_error().data(format!(
                "failed to build image-describe sampling client: {e}"
            ))
        })?;
        let model = &describe_model;
        let limit = crate::session::image_describe::IMAGE_DESCRIPTION_PROCESSING_LIMIT;
        let skip_count = persisted.len().saturating_sub(limit);
        if skip_count > 0 {
            tracing::info!(
                session_id = %self.session_info.id,
                total = persisted.len(),
                skipped = skip_count,
                limit,
                "image transcription: skipping oldest images due to processing limit",
            );
        }
        let mut description_parts = Vec::with_capacity(persisted.len());
        for (i, p) in persisted.iter().enumerate() {
            let part = if i < skip_count {
                crate::session::image_describe::SKIPPED_IMAGE_MARKER.to_owned()
            } else {
                self.image_describe_cache
                    .get_or_describe(
                        client.clone(),
                        model,
                        &p.raw_bytes,
                        &p.mime_type,
                        outline.as_deref(),
                        &current_query,
                        crate::session::image_describe::ImageDescribeSource::UserAttachment,
                        "",
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("image transcription failed: {e}"))
                    })?
            };
            if persisted.len() > 1 {
                description_parts.push(format!("Image {}: {}", i + 1, part));
            } else {
                description_parts.push(part);
            }
        }
        let description = description_parts.join("\n\n");
        Ok(crate::session::image_describe::render_image_user_message(
            &description,
            &image_paths,
            &original_user_message,
        ))
    }
}
