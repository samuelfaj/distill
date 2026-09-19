// Modified for Distill by Samuel Fajreldines, 2026.
use pretty_assertions::assert_eq;

#[test]
fn reap_request_for_task_kills_with_session_scope() {
    let session_id = acp::SessionId::new("sess-1");
    let work = super::BackgroundWork::Task("task-42".into());
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/task/kill");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(
        params.get("sessionId").and_then(|v| v.as_str()),
        Some("sess-1")
    );
    assert_eq!(
        params.get("taskId").and_then(|v| v.as_str()),
        Some("task-42")
    );
    assert_eq!(
        params.get("source").and_then(|v| v.as_str()),
        Some("teardown")
    );
}

/// A numeric `task_id` is coerced to its string form, tracked, and reaped on exit.
#[test]
fn numeric_task_id_is_decoded_tracked_and_reaped() {
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": { "sessionUpdate": "task_backgrounded", "task_id": 4242 },
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let notif = distill_acp_lib::AcpArgs {
        request: acp::ExtNotification::new("x.ai/task_backgrounded", raw.into()),
        response_tx: tx,
    }
    .boxed();
    let event = super::handle_ext_notification(&notif);
    let mut pending = std::collections::HashSet::new();
    let mut completed = super::BackgroundLifecycleState::default();
    super::track_background_lifecycle(event, &mut pending, &mut completed);
    let work = super::BackgroundWork::Task("4242".into());
    assert!(
        pending.contains(&work),
        "numeric task_id tracked as the coerced string id"
    );
    let session_id = acp::SessionId::new("sess-1");
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/task/kill");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(params.get("taskId").and_then(|v| v.as_str()), Some("4242"));
    assert_eq!(
        params.get("sessionId").and_then(|v| v.as_str()),
        Some("sess-1")
    );
    assert_eq!(
        params.get("source").and_then(|v| v.as_str()),
        Some("teardown")
    );
}

#[test]
fn reap_request_for_subagent_cancels_with_typed_id() {
    let session_id = acp::SessionId::new("sess-1");
    let work = super::BackgroundWork::Subagent("sub-7".into());
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/subagent/cancel");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(
        params.get("subagentId").and_then(|v| v.as_str()),
        Some("sub-7")
    );
}

/// A `task_backgrounded` delivered right at prompt completion is still recorded by the drain.
#[test]
fn drain_records_task_backgrounded_delivered_at_exit() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": { "sessionUpdate": "task_backgrounded", "task_id": "late-1" },
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
    tx.send(distill_acp_lib::AcpClientMessage::ExtNotification(
        distill_acp_lib::AcpArgs {
            request: acp::ExtNotification::new("x.ai/task_backgrounded", raw.into()),
            response_tx: resp_tx,
        },
    ))
    .unwrap();

    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, false);
    let mut pending = std::collections::HashSet::new();
    let mut completed = super::BackgroundLifecycleState::default();
    let mut ttf_logged = false;
    super::drain_pending_acp_messages(
        &mut rx,
        &mut emitter,
        std::time::Instant::now(),
        &mut ttf_logged,
        false,
        &mut pending,
        &mut completed,
    );
    assert!(
        pending.contains(&super::BackgroundWork::Task("late-1".into())),
        "drain-to-empty records a task_backgrounded buffered at exit"
    );
}

/// `begin_session` runs before the model and effort are applied, so a post-open error carries the real context.
#[test]
fn post_open_error_carries_real_session_context() {
    let mut pre = reducer_for(OutputFormat::StreamingMessagesJson).unwrap();
    let pre_lines = pre.error("boom", None, 0, None);
    let pre_result = pre_lines
        .iter()
        .find(|l| l.get("type").and_then(|t| t.as_str()) == Some("result"))
        .expect("result line");
    assert_eq!(
        pre_result.get("session_id").and_then(|v| v.as_str()),
        Some(""),
        "pre-session error keeps the startup-error fallback"
    );

    let mut post = reducer_for(OutputFormat::StreamingMessagesJson).unwrap();
    post.begin(SessionContext {
        session_id: "sess-real".into(),
        model: Some("grok-4".into()),
        cwd: "/work/dir".into(),
        permission_mode: None,
        mcp_servers: Vec::new(),
        include_partial_messages: false,
        api_key_auth: true,
        context_window: None,
    });
    let post_lines = post.error("boom", None, 0, None);
    let post_result = post_lines
        .iter()
        .find(|l| l.get("type").and_then(|t| t.as_str()) == Some("result"))
        .expect("result line");
    assert_eq!(
        post_result.get("session_id").and_then(|v| v.as_str()),
        Some("sess-real"),
        "post-open error carries the real session id"
    );
    let init = post_lines
        .iter()
        .find(|l| {
            l.get("type").and_then(|t| t.as_str()) == Some("system")
                && l.get("subtype").and_then(|s| s.as_str()) == Some("init")
        })
        .expect("system/init line");
    assert_eq!(
        init.get("session_id").and_then(|v| v.as_str()),
        Some("sess-real")
    );
    assert_eq!(init.get("cwd").and_then(|v| v.as_str()), Some("/work/dir"));
}

use super::*;
use distill_workspace::permission::types::{RuleAction, ToolFilter};

fn s(v: &str) -> String {
    v.to_owned()
}

#[test]
fn headless_materialize_ctx_stays_non_chat() {
    use crate::app::session_startup::TitleResolution;
    for pinned in [false, true] {
        for restore_code in [false, true] {
            for has_worktree in [false, true] {
                let ctx = headless_materialize_ctx(pinned, restore_code, has_worktree);
                assert!(!ctx.chat_mode);
                assert_eq!(ctx.has_worktree, has_worktree);
                assert_eq!(ctx.restore_code, restore_code);
                assert_eq!(
                    ctx.title_resolution,
                    if pinned {
                        TitleResolution::PinnedPreSandbox
                    } else {
                        TitleResolution::Allowed
                    }
                );
            }
        }
    }
}

#[test]
fn headless_remote_miss_restores_conversation_instead_of_deferring_worktree() {
    use crate::app::session_startup::{RemoteMissPlan, plan_remote_miss};
    for restore_code in [false, true] {
        let ctx = headless_materialize_ctx(false, restore_code, false);
        assert!(!matches!(
            plan_remote_miss(ctx, true),
            RemoteMissPlan::DeferToWorktree { .. }
        ));
    }
    let mut conv = headless_materialize_ctx(false, false, false);
    conv.allow_remote_restore = true;
    assert_eq!(
        plan_remote_miss(conv, true),
        RemoteMissPlan::RestoreConversation
    );
    let mut code = headless_materialize_ctx(false, true, false);
    code.allow_remote_restore = true;
    assert_eq!(
        plan_remote_miss(code, true),
        RemoteMissPlan::RejectInPlaceCodeRestore {
            title_miss_hint: false,
        }
    );
}

#[test]
fn headless_remote_miss_defers_to_worktree_when_requested() {
    use crate::app::session_startup::{RemoteMissPlan, plan_remote_miss};
    for restore_code in [false, true] {
        let ctx = headless_materialize_ctx(false, restore_code, true);
        assert_eq!(
            plan_remote_miss(ctx, true),
            RemoteMissPlan::DeferToWorktree {
                deferred_local_miss: false,
            }
        );
    }
}

/// Fake agent for the worktree paths: answers the extension method with `ext_reply`, then
/// `session/new` and `session/load` as directed. Records every request for assertions.
#[derive(Default)]
struct FakeAgentLog {
    ext: Vec<(String, serde_json::Value)>,
    new_sessions: Vec<(std::path::PathBuf, Option<acp::Meta>)>,
    loads: Vec<(String, std::path::PathBuf, Option<acp::Meta>)>,
}

fn spawn_fake_agent(
    ext_reply: serde_json::Value,
    session_open: Result<&'static str, &'static str>,
) -> (
    distill_acp_lib::AcpAgentTx,
    std::sync::Arc<std::sync::Mutex<FakeAgentLog>>,
) {
    use distill_acp_lib::AcpAgentMessage;
    use std::sync::{Arc, Mutex};
    let log = Arc::new(Mutex::new(FakeAgentLog::default()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpAgentMessage>();
    let log_for_task = log.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                AcpAgentMessage::ExtMethod(args) => {
                    let params: serde_json::Value =
                        serde_json::from_str(args.request.params.get()).unwrap();
                    log_for_task
                        .lock()
                        .unwrap()
                        .ext
                        .push((args.request.method.to_string(), params));
                    let raw = serde_json::value::to_raw_value(&ext_reply).unwrap();
                    let _ = args
                        .response_tx
                        .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                }
                AcpAgentMessage::NewSession(args) => {
                    log_for_task
                        .lock()
                        .unwrap()
                        .new_sessions
                        .push((args.request.cwd.clone(), args.request.meta.clone()));
                    // Like the agent, a `meta.sessionId` names the new session; otherwise mint one.
                    let forced = args
                        .request
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("sessionId"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let _ = args.response_tx.send(match session_open {
                        Ok(sid) => Ok(acp::NewSessionResponse::new(
                            forced.unwrap_or_else(|| sid.to_owned()),
                        )),
                        Err(msg) => Err(acp::Error::internal_error().data(msg)),
                    });
                }
                AcpAgentMessage::LoadSession(args) => {
                    log_for_task.lock().unwrap().loads.push((
                        args.request.session_id.0.to_string(),
                        args.request.cwd.clone(),
                        args.request.meta.clone(),
                    ));
                    let _ = args.response_tx.send(match session_open {
                        Ok(_) => Ok(acp::LoadSessionResponse::new()),
                        Err(msg) => Err(acp::Error::internal_error().data(msg)),
                    });
                }
                _ => {}
            }
        }
    });
    (tx, log)
}

#[tokio::test]
async fn worktree_create_opens_session_at_worktree_subdirectory() {
    let source = tempfile::tempdir().unwrap();
    let launch_cwd = source.path().join("crates").join("pager");
    std::fs::create_dir_all(&launch_cwd).unwrap();
    let wt_root = tempfile::tempdir().unwrap();
    let (tx, log) = spawn_fake_agent(
        serde_json::json!({"result": {
            "worktreePath": wt_root.path(),
            "sourceGitRoot": source.path(),
        }}),
        Ok("sess-new"),
    );
    let spec = WorktreeSpec::from_cli(Some("fix"), Some("origin/main")).unwrap();

    let opened = open_session_in_new_worktree(&tx, &launch_cwd, &spec, None)
        .await
        .unwrap();

    assert_eq!(opened.session_id.0.as_ref(), "sess-new");
    assert_eq!(opened.cwd, wt_root.path().join("crates").join("pager"));
    let log = log.lock().unwrap();
    let Some((method, params)) = log.ext.first() else {
        panic!("expected an ext call: {:?}", log.ext);
    };
    assert_eq!(method, "x.ai/git/worktree/create_from_worktree_sync");
    assert_eq!(
        params.get("sourceWorktreePath").and_then(|v| v.as_str()),
        Some(launch_cwd.to_string_lossy().as_ref())
    );
    assert_eq!(
        params.get("copyMode").and_then(|v| v.as_str()),
        Some("clean")
    );
    assert_eq!(params.get("label").and_then(|v| v.as_str()), Some("fix"));
    assert_eq!(
        params.get("gitRef").and_then(|v| v.as_str()),
        Some("origin/main")
    );
    assert!(
        params
            .get("newSessionId")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("pager-"))
    );
    assert_eq!(log.new_sessions.len(), 1);
    assert_eq!(log.new_sessions.first().map(|s| &s.0), Some(&opened.cwd));
    assert!(log.loads.is_empty());
}

#[tokio::test]
async fn worktree_create_with_session_id_names_worktree_and_session() {
    let source = tempfile::tempdir().unwrap();
    let wt_root = tempfile::tempdir().unwrap();
    let (tx, log) = spawn_fake_agent(
        serde_json::json!({"worktreePath": wt_root.path()}),
        Ok("minted-if-not-forced"),
    );
    let sid = "2d3c6b3e-3d43-4f0a-9d2e-2b6d1b6a9c11";

    let opened =
        open_session_in_new_worktree(&tx, source.path(), &WorktreeSpec::default(), Some(sid))
            .await
            .unwrap();

    assert_eq!(opened.session_id.0.as_ref(), sid);
    assert_eq!(opened.cwd, wt_root.path());
    let log = log.lock().unwrap();
    let Some((_, params)) = log.ext.first() else {
        panic!("expected an ext call: {:?}", log.ext);
    };
    assert_eq!(
        params.get("newSessionId").and_then(|v| v.as_str()),
        Some(sid)
    );
    assert_eq!(
        params.get("copyMode").and_then(|v| v.as_str()),
        Some("dirty")
    );
    let meta = log
        .new_sessions
        .first()
        .and_then(|s| s.1.as_ref())
        .expect("session id forced via meta");
    assert_eq!(meta.get("sessionId").and_then(|v| v.as_str()), Some(sid));
}

#[tokio::test]
async fn worktree_create_failure_is_reported_before_any_session_opens() {
    let source = tempfile::tempdir().unwrap();
    for (reply, expect) in [
        (
            serde_json::json!({"error": "no space left for worktree"}),
            "no space left for worktree",
        ),
        (
            serde_json::json!({"result": {}}),
            "response missing worktreePath",
        ),
    ] {
        let (tx, log) = spawn_fake_agent(reply, Ok("never"));
        let err = open_session_in_new_worktree(&tx, source.path(), &WorktreeSpec::default(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("couldn't create worktree"), "{err}");
        assert!(err.contains(expect), "{err}");
        assert!(log.lock().unwrap().new_sessions.is_empty());
    }
}

#[tokio::test]
async fn worktree_create_then_session_failure_names_the_orphaned_worktree() {
    let source = tempfile::tempdir().unwrap();
    let wt_root = tempfile::tempdir().unwrap();
    let (tx, _log) = spawn_fake_agent(
        serde_json::json!({"worktreePath": wt_root.path()}),
        Err("agent refused"),
    );

    let err = open_session_in_new_worktree(&tx, source.path(), &WorktreeSpec::default(), None)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("agent refused"), "{err}");
    assert!(err.contains(&wt_root.path().display().to_string()), "{err}");
    assert!(err.contains("grok worktree rm"), "{err}");
}

#[tokio::test]
async fn worktree_resume_loads_reported_session_without_re_restoring_code() {
    let source = tempfile::tempdir().unwrap();
    let wt_root = tempfile::tempdir().unwrap();
    let eff_cwd = wt_root.path().join("sub");
    let (tx, log) = spawn_fake_agent(
        serde_json::json!({"result": {
            "sessionId": "forked-in-worktree",
            "worktreePath": wt_root.path(),
            "effectiveCwd": eff_cwd,
            "codeRestored": true,
        }}),
        Ok("unused"),
    );
    let spec = WorktreeSpec::from_cli(Some(""), Some("v1.2")).unwrap();

    let opened =
        resume_session_in_new_worktree(&tx, source.path(), &spec, "orig", Some(true), false)
            .await
            .unwrap();

    assert_eq!(opened.session_id.0.as_ref(), "forked-in-worktree");
    assert_eq!(opened.cwd, eff_cwd);
    let log = log.lock().unwrap();
    let Some((method, params)) = log.ext.first() else {
        panic!("expected an ext call: {:?}", log.ext);
    };
    assert_eq!(method, "x.ai/git/worktree/resume_session");
    assert_eq!(
        params.get("sessionId").and_then(|v| v.as_str()),
        Some("orig")
    );
    assert_eq!(
        params.get("sourceCwd").and_then(|v| v.as_str()),
        Some(source.path().to_string_lossy().as_ref())
    );
    assert_eq!(
        params.get("copyMode").and_then(|v| v.as_str()),
        Some("clean")
    );
    assert_eq!(params.get("gitRef").and_then(|v| v.as_str()), Some("v1.2"));
    assert_eq!(
        params.get("restoreCode").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(params.get("worktreeType").is_some());
    let Some((loaded_sid, loaded_cwd, meta)) = log.loads.first() else {
        panic!("expected a load: {:?}", log.loads);
    };
    assert_eq!(loaded_sid, "forked-in-worktree");
    assert_eq!(loaded_cwd, &eff_cwd);
    let meta = meta.as_ref().unwrap();
    assert_eq!(meta.get("noReplay").and_then(|v| v.as_bool()), Some(true));
    assert!(
        meta.get("x.ai/restore_code").is_none(),
        "load must not request code restore a second time"
    );
    assert!(log.new_sessions.is_empty());
}

#[tokio::test]
async fn worktree_resume_failure_carries_local_miss_hint_like_the_tui() {
    let source = tempfile::tempdir().unwrap();
    let (tx, _log) = spawn_fake_agent(serde_json::json!({"error": "archive unavailable"}), Ok("x"));
    let spec = WorktreeSpec::default();

    let hinted = resume_session_in_new_worktree(&tx, source.path(), &spec, "my title", None, true)
        .await
        .unwrap_err()
        .to_string();
    let plain = resume_session_in_new_worktree(&tx, source.path(), &spec, "my title", None, false)
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(
        plain,
        crate::app::session_title_resolve::worktree_resume_failure_message(
            None,
            "archive unavailable"
        )
    );
    assert_eq!(
        hinted,
        crate::app::session_title_resolve::worktree_resume_failure_message(
            Some("my title"),
            "archive unavailable"
        )
    );
    assert_ne!(hinted, plain);
}

#[test]
fn worktree_with_fork_is_rejected_at_intent() {
    use crate::app::session_startup::{
        SessionStartupFlags, StartupFlagError, session_startup_intent_from_flags,
    };
    let err = session_startup_intent_from_flags(SessionStartupFlags {
        session_id: None,
        resume_session_id: Some("01a06380-62b5-7881-b173-c69cd2c213fd"),
        resume_most_recent: false,
        continue_last_session: false,
        fork_session: true,
        has_worktree: true,
    })
    .unwrap_err();
    assert!(matches!(err, StartupFlagError::ForkWithWorktree));
}

#[test]
fn strict_valid_rules_parse_deny_before_allow() {
    let allow = vec![s("Bash(npm*)")];
    let deny = vec![s("Bash(rm*)"), s("Edit(/etc/**)")];
    let rules = parse_permission_rules_strict(&allow, &deny).unwrap();
    assert_eq!(rules.len(), 3);
    let [r0, r1, r2] = rules.as_slice() else {
        panic!("expected three rules: {rules:?}");
    };
    assert_eq!(r0.action, RuleAction::Deny);
    assert!(matches!(r0.tool, ToolFilter::Bash));
    assert_eq!(r1.action, RuleAction::Deny);
    assert!(matches!(r1.tool, ToolFilter::Edit));
    assert_eq!(r2.action, RuleAction::Allow);
    assert!(matches!(r2.tool, ToolFilter::Bash));
}

#[test]
fn strict_invalid_rule_errors() {
    let result = parse_permission_rules_strict(&[], &[s("EnterWorktree(foo)")]);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("--deny"));
    assert!(msg.contains("EnterWorktree"));
}

#[test]
fn strict_reports_all_invalid_rules() {
    let result = parse_permission_rules_strict(
        &[s("BadTool(x)")],
        &[s("EnterWorktree(foo)"), s("Bash(rm*)")],
    );
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("EnterWorktree"),
        "should mention first bad deny"
    );
    assert!(msg.contains("BadTool"), "should mention bad allow");
}

#[test]
fn lenient_skips_invalid_keeps_valid() {
    let allow = vec![s("Bash(npm*)")];
    let deny = vec![s("EnterWorktree(foo)"), s("Bash(rm*)")];
    let rules = parse_permission_rules_lenient(&allow, &deny);
    assert_eq!(rules.len(), 2);
    let [r0, r1] = rules.as_slice() else {
        panic!("expected two rules: {rules:?}");
    };
    assert_eq!(r0.action, RuleAction::Deny);
    assert_eq!(r0.pattern.as_deref(), Some("rm*"));
    assert_eq!(r1.action, RuleAction::Allow);
    assert_eq!(r1.pattern.as_deref(), Some("npm*"));
}

#[test]
fn empty_inputs_produce_empty_rules() {
    let rules = parse_permission_rules_strict(&[], &[]).unwrap();
    assert!(rules.is_empty());
    let rules = parse_permission_rules_lenient(&[], &[]);
    assert!(rules.is_empty());
}

#[test]
fn domain_mode_web_fetch() {
    let rules = parse_permission_rules_strict(&[], &[s("WebFetch(domain:evil.com)")]).unwrap();
    assert_eq!(rules.len(), 1);
    let Some(rule) = rules.first() else {
        panic!("expected a WebFetch rule: {rules:?}");
    };
    assert!(matches!(rule.tool, ToolFilter::WebFetch));
    assert_eq!(
        rule.pattern_mode,
        distill_workspace::permission::types::PatternMode::Domain
    );
    assert_eq!(rule.pattern.as_deref(), Some("evil.com"));
}

#[test]
fn bash_colon_wildcard_deny_translates_to_prefix() {
    let rules = parse_permission_rules_strict(&[], &[s("Bash(sed:*)")]).unwrap();
    assert_eq!(rules.len(), 1);
    let Some(rule) = rules.first() else {
        panic!("expected a Bash rule: {rules:?}");
    };
    assert!(matches!(rule.tool, ToolFilter::Bash));
    assert_eq!(rule.pattern.as_deref(), Some("sed"));
}

#[test]
fn structured_output_without_meta_errors_never_parses_text() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.text_buffer = r#"{"name":"alice","age":30}"#.into();
    emitter.set_structured_output_from_meta(serde_json::json!({}).as_object());
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert!(result.get("structuredOutput").is_none_or(|v| v.is_null()));
    assert_eq!(
        result.get("structuredOutputError").and_then(|v| v.as_str()),
        Some("model did not produce structured output")
    );
}

#[test]
fn structured_output_from_meta_wins_over_text_buffer() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.text_buffer = "thinking out loud...".into();
    emitter.set_structured_output_from_meta(
        serde_json::json!({"structuredOutput": {"name": "carol"}}).as_object(),
    );
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert_eq!(
        result
            .get("structuredOutput")
            .and_then(|o| o.get("name"))
            .and_then(|n| n.as_str()),
        Some("carol")
    );
    assert!(result.get("structuredOutputError").is_none());

    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.set_structured_output_from_meta(
        serde_json::json!({
            "structuredOutputError": "output does not match the required schema"
        })
        .as_object(),
    );
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert!(result.get("structuredOutput").is_none_or(|v| v.is_null()));
    assert_eq!(
        result.get("structuredOutputError").and_then(|v| v.as_str()),
        Some("output does not match the required schema")
    );
}

#[test]
fn streaming_json_structured_output_emits_from_meta() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingJson, true);
    emitter.on_text_chunk(r#"{"name":"#);
    emitter.on_text_chunk(r#""bob"}"#);
    assert!(emitter.text_buffer.is_empty());

    emitter.set_structured_output_from_meta(
        serde_json::json!({"structuredOutput": {"name": "bob"}}).as_object(),
    );
    let mut target = serde_json::json!({});
    emitter.attach_structured_output(&mut target);
    assert_eq!(
        target
            .get("structuredOutput")
            .and_then(|o| o.get("name"))
            .and_then(|n| n.as_str()),
        Some("bob")
    );
    assert!(target.get("structuredOutputError").is_none());
}

#[test]
fn broken_pipe_write_is_a_clean_latched_stop() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingMessagesJson, false);
    let result = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "pipe",
    )));
    assert!(result.is_ok(), "broken pipe is a clean stop");
    assert!(emitter.output_closed);
    assert!(emitter.take_output_error().is_none());
}

#[test]
fn hard_write_error_is_latched_and_surfaced_once() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingMessagesJson, false);
    let result = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "denied",
    )));
    assert!(result.is_err(), "hard error is surfaced to the caller");
    assert!(emitter.output_closed);
    let latched = emitter.take_output_error().expect("hard error latched");
    assert_eq!(latched.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        emitter.take_output_error().is_none(),
        "taken once, then cleared"
    );
}

#[test]
fn first_hard_write_error_wins_the_latch() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, false);
    let _ = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "first",
    )));
    let _ = emitter.record_write_result(Err(std::io::Error::other("second")));
    assert_eq!(
        emitter.take_output_error().map(|e| e.kind()),
        Some(std::io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn successful_write_leaves_no_latched_error() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Plain, false);
    assert!(emitter.record_write_result(Ok(())).is_ok());
    assert!(!emitter.output_closed);
    assert!(emitter.take_output_error().is_none());
}

#[test]
fn parse_json_schema_rejects_non_objects_and_invalid_json() {
    assert!(super::parse_json_schema(r#"{"type":"object"}"#).is_ok());
    assert!(
        super::parse_json_schema(r#"[1,2,3]"#)
            .unwrap_err()
            .to_string()
            .contains("must be a JSON object")
    );
    assert!(
        super::parse_json_schema(r#"{not json"#)
            .unwrap_err()
            .to_string()
            .contains("invalid JSON")
    );
}

#[test]
fn handler_answers_ext_method_instead_of_dropping() {
    use agent_client_protocol as acp;
    use distill_tools::implementations::distill::ask_user_question::AskUserQuestionExtResponse;
    let raw = serde_json::value::to_raw_value(&serde_json::json!({})).unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let msg = distill_acp_lib::AcpClientMessage::ExtMethod(distill_acp_lib::AcpArgs {
        request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
        response_tx: tx,
    });
    let mut emitter = super::HeadlessEmitter::new(super::OutputFormat::Json, false);
    let mut pending = std::collections::HashSet::new();
    let mut completed = super::BackgroundLifecycleState::default();
    let mut ttf_logged = false;
    super::handle_headless_acp_message(
        msg.boxed(),
        &mut emitter,
        std::time::Instant::now(),
        &mut ttf_logged,
        false,
        &mut pending,
        &mut completed,
    );
    let resp = rx
        .try_recv()
        .expect("ExtMethod must be answered, never dropped")
        .expect("policy reply, not an error");
    let parsed: AskUserQuestionExtResponse =
        serde_json::from_str(resp.0.get()).expect("typed wire reply");
    assert!(matches!(parsed, AskUserQuestionExtResponse::Cancelled));
}
