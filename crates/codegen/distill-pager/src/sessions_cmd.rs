// Modified for Distill by Samuel Fajreldines, 2026.
use anyhow::Result;
use clap::Subcommand;
use distill_login::{AuthManager, try_ensure_fresh_auth};
use distill_shell::agent::config::Config as AgentConfig;
use distill_shell::session::merge::MergedSession;
use distill_shell::util::distill_home::distill_home;
#[derive(Debug, clap::Args, Clone)]
pub struct SessionsArgs {
    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum SessionsCommand {
    /// List recent sessions (same as search with no query)
    List {
        /// Maximum number of sessions to show
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
    /// Search sessions by keyword
    Search {
        /// Search query (searches summaries and first prompts).
        query: String,
        /// Maximum number of sessions to show
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
    },
    /// Permanently delete a session from history
    Delete {
        /// Session id to delete.
        id: String,
    },
}

pub async fn run(args: SessionsArgs, agent_config: &AgentConfig) -> Result<()> {
    // Best-effort only: never force an interactive public login here. Enterprise deployments may configure only a
    // deployment_key and a custom xai_api_base_url. Otherwise we still proceed so the SessionRegistryClient can use
    // the deployment_key when talking to the custom proxy.
    let auth = try_ensure_fresh_auth(
        &agent_config.grok_com_config,
        agent_config.endpoints.proxy_url(),
    )
    .await;

    let auth_manager = std::sync::Arc::new(AuthManager::new_with_proxy_base_url(
        &distill_home(),
        agent_config.grok_com_config.clone(),
        agent_config.endpoints.proxy_url(),
    ));

    let client = distill_shell::agent::session_registry_client::SessionRegistryClient::new(
        agent_config.endpoints.proxy_url(),
        String::new(),
    )
    .with_deployment_key(agent_config.endpoints.deployment_key.clone())
    .with_alpha_test_key(agent_config.endpoints.alpha_test_key.clone())
    .with_auth(auth_manager.clone());

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());

    match args.command {
        SessionsCommand::List { limit } => {
            let sessions = distill_shell::session::merge::fetch_merged(
                Some(&client),
                cwd.to_str(),
                distill_shell::session::merge::CwdScope::WithSiblings,
                None,
                limit,
                // The CLI listing is an inventory, not the resume picker.
                distill_shell::session::visibility::HeadlessPolicy::Include,
            )
            .await;
            print_sessions_grouped(&sessions);
        }
        SessionsCommand::Search { query, limit } => {
            use distill_shell::session::merge::REMOTE_TIMEOUT;
            use distill_shell::session::storage::search::{
                IndexDecision, SessionSearchRequest, execute_search,
            };
            use std::collections::HashSet;

            // Search is the only subcommand that reads the index, so it is the only one to start one
            let search = distill_shell::session::storage::search::start_if_enabled(agent_config);

            let req = SessionSearchRequest {
                query,
                cwd: Some(cwd.to_string_lossy().to_string()),
                limit,
                offset: 0,
                include_content: true,
            };
            let root = distill_home();

            let remote_limit = (limit * 3).max(100) as i64;
            let (local_resp, remote_results) = tokio::join!(
                execute_search(IndexDecision::settled(&search), &root, &req),
                async {
                    tokio::time::timeout(
                        REMOTE_TIMEOUT,
                        client.search(Some(&req.query), remote_limit),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        eprintln!("warning: remote session search timed out");
                        Ok(Vec::new())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("warning: remote session search failed: {e}");
                        Vec::new()
                    })
                }
            );

            let resp = local_resp?;
            if let Some(by) = search.off_reason() {
                eprintln!(
                    "warning: local session search is off ({by}); searched remote sessions only."
                );
            }
            let local_ids: HashSet<&str> =
                resp.results.iter().map(|r| r.session_id.as_str()).collect();

            for hit in &resp.results {
                let title = if hit.title.is_empty() {
                    "(untitled)"
                } else {
                    &hit.title
                };
                let time = chrono::DateTime::from_timestamp(hit.updated_at_unix, 0)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%b %d, %l:%M%P")
                            .to_string()
                    })
                    .unwrap_or_default();
                println!(
                    "{} (score: {:.2})  {}\n  {}\n  {}",
                    hit.session_id,
                    hit.score,
                    time,
                    title,
                    hit.snippet.as_deref().unwrap_or("")
                );
            }

            let remaining = limit.saturating_sub(resp.results.len());
            let mut remote_shown = 0usize;
            for r in &remote_results {
                if remote_shown >= remaining {
                    break;
                }
                if local_ids.contains(r.session_id.as_str()) {
                    continue;
                }
                let title = if r.summary.is_empty() {
                    "(untitled)"
                } else {
                    &r.summary
                };
                let time = chrono::DateTime::parse_from_rfc3339(&r.updated_at)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%b %d, %l:%M%P")
                            .to_string()
                    })
                    .unwrap_or_default();
                let snippet: String = r
                    .first_prompt
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                println!(
                    "{} (remote)  {}\n  {}\n  {}",
                    r.session_id, time, title, snippet
                );
                remote_shown += 1;
            }

            println!("\nTotal: {}", resp.results.len() + remote_shown);
        }
        SessionsCommand::Delete { id } => {
            // Always attempt the remote delete when authenticated and not ZDR; `list` and `search` likewise query remote
            // unconditionally. Gating on storage mode is impossible here: the CLI builds config without remote settings. ZDR
            // teams never upload, so there is nothing remote to delete.
            let needs_remote = auth.as_ref().is_some_and(|a| !a.is_zdr_team());

            // Pass `cwd = None` so the session is found by id regardless of which workspace it was created in
            // The local delete still uses the resolved per-session cwd
            // No search handle: the eviction inside prunes the row from another process's index, so a delete never needs one of its own
            let deletion = distill_shell::session::persistence::delete_session_history(
                &id,
                None,
                needs_remote,
                auth_manager.clone(),
                None,
            )
            .await?;

            if deletion.any_removed() {
                println!("Deleted session {id}");
            } else {
                println!("No session found with id {id}.");
            }
        }
    }

    Ok(())
}

/// Print sessions grouped by worktree label, preserving the original table format with a `Label: <label>` header before each group.
fn print_sessions_grouped(sessions: &[MergedSession]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    // Group by worktree_label, sort alphabetically, None last.
    let mut groups: std::collections::BTreeMap<Option<&str>, Vec<&MergedSession>> =
        std::collections::BTreeMap::new();
    for s in sessions {
        groups
            .entry(s.worktree_label.as_deref())
            .or_default()
            .push(s);
    }

    let header = format!(
        "{:<36}  {:<10}  {:<10}  {:<10}  {}",
        "SESSION ID", "CREATED", "UPDATED", "STATUS", "SUMMARY"
    );

    // Labeled groups first (alphabetical), then unlabeled last.
    let none_group = groups.remove(&None);
    let print_group = |label_line: &str, members: &[&MergedSession]| {
        println!("\n{label_line}");
        println!("{header}");
        for s in members {
            let first_line;
            let summary: &str = if !s.summary.is_empty() {
                &s.summary
            } else if let Some(ref fp) = s.first_prompt
                && let Some(line) = fp.lines().find(|l| !l.trim().is_empty())
            {
                first_line = line.trim().to_string();
                &first_line
            } else {
                "(no summary)"
            };
            let truncated: String = summary.chars().take(50).collect();
            let created = s
                .created_at
                .get(..s.created_at.len().min(10))
                .unwrap_or(s.created_at.as_str());
            let updated = s
                .updated_at
                .get(..s.updated_at.len().min(10))
                .unwrap_or(s.updated_at.as_str());
            println!(
                "{}  {}  {}  {}  {}",
                s.session_id, created, updated, s.source, truncated
            );
        }
    };

    for (label, members) in &groups {
        let line = format!("Label: {}", label.unwrap_or(""));
        print_group(&line, members);
    }
    if let Some(members) = &none_group {
        print_group("(no label)", members);
    }
}
