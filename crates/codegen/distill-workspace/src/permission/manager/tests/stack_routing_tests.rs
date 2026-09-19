use super::*;
use crate::permission::auto_mode::{
    ClassifierMessage, ClassifierMessageRole, ClassifierPromptType, ClassifierTurn,
    HeuristicPermissionClassifier, LlmPermissionClassifier,
};

const ALLOW_RESPONSE: &str =
    r#"{"thinking":"fixed allow fixture","shouldBlock":false,"reason":"requested"}"#;
const BLOCK_RESPONSE: &str =
    r#"{"thinking":"fixed block fixture","shouldBlock":true,"reason":"unsafe"}"#;

async fn route(
    turns: Vec<ClassifierTurn>,
    command: &str,
    response: &'static str,
) -> (Decision, Vec<ClassifierMessage>, PermissionEvent) {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = AbsPathBuf::new(tmp.path().to_path_buf()).unwrap();
    let client = RecordingClient::default();
    let prompts = client.prompts.clone();
    let (manager, mut events) =
        manager_with_recording_client(&cwd, None, client, ClientType::Generic);
    manager.set_auto_mode(true);
    manager.set_classifier_transcript(turns);
    let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = messages.clone();
    manager.set_classifier(Some(Arc::new(LlmPermissionClassifier {
        classify_text: Some(Arc::new(move |request_messages| {
            captured.lock().unwrap().push(request_messages);
            Box::pin(async move { Ok(response.to_owned()) })
        })),
        classify_channel: None,
        fallback: HeuristicPermissionClassifier,
        prompt_type: ClassifierPromptType::Full,
    })));

    let decision = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        decide(&manager, AccessKind::Bash(command.to_owned()), tool_call()),
    )
    .await
    .expect("permission request must resolve");
    assert!(prompts.borrow().is_empty(), "{command}");

    let messages = messages.lock().unwrap();
    assert_eq!(messages.len(), 1, "{command}");
    let Some(request_messages) = messages.first().cloned() else {
        panic!("expected classifier messages: {messages:?}");
    };
    drop(messages);
    let event = events.try_recv().expect("permission event");
    (decision, request_messages, event)
}

fn assert_classifier_context(
    messages: &[ClassifierMessage],
    command: &str,
    transcript_fragments: &[&str],
    expected_findings: &[ClassifierSecurityFinding],
) {
    let trailing = messages.last().expect("classifier trailing message");
    assert_eq!(trailing.role, ClassifierMessageRole::User, "{command}");
    for fragment in transcript_fragments {
        assert!(trailing.text.contains(fragment), "{command}: {fragment}");
    }
    assert!(
        trailing.text.contains(&format!("detail: {command}")),
        "{command}"
    );

    let finding_messages = messages
        .iter()
        .filter(|message| message.role == ClassifierMessageRole::System)
        .skip(1)
        .collect::<Vec<_>>();
    assert_eq!(
        finding_messages.len(),
        usize::from(!expected_findings.is_empty()),
        "{command}"
    );
    for finding in ClassifierSecurityFinding::ALL {
        assert_eq!(
            finding_messages
                .first()
                .is_some_and(|message| message.text.contains(finding.token())),
            expected_findings.contains(finding),
            "{command}: {finding:?}"
        );
    }
}

fn finding_tokens(findings: &[ClassifierSecurityFinding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| finding.token().to_owned())
        .collect()
}

#[tokio::test]
async fn stack_commands_and_feature_force_push_route_fixed_allow() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (user, command, findings) in [
                (
                    "make a 3-PR stack and open the third PR",
                    "gh stack submit",
                    [].as_slice(),
                ),
                (
                    "change the second PR, then update the stack",
                    "gh stack sync",
                    [].as_slice(),
                ),
                (
                    "force-push my branch topic/my-change after the rebase",
                    "git push --force-with-lease origin topic/my-change",
                    [ClassifierSecurityFinding::DangerousCommand].as_slice(),
                ),
            ] {
                let (decision, messages, event) = route(
                    vec![ClassifierTurn::UserText(user.into())],
                    command,
                    ALLOW_RESPONSE,
                )
                .await;
                assert_eq!(decision, Decision::Allow, "{command}");
                let user_fragment = format!("User: {user}");
                assert_classifier_context(&messages, command, &[&user_fragment], findings);
                assert_eq!(
                    event.decision_reason.as_deref(),
                    Some(reasons::AUTO_CLASSIFIER_ALLOW)
                );
                assert_eq!(event.classifier_source.as_deref(), Some("llm"));
                assert_eq!(event.classifier_verdict.as_deref(), Some("allow"));
                assert_eq!(
                    event.security_findings,
                    Some(finding_tokens(findings)),
                    "{command}"
                );
            }
        })
        .await;
}

#[tokio::test]
async fn protected_and_exfiltration_context_routes_fixed_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (turns, command, fragments) in [
                (
                    vec![ClassifierTurn::UserText("fix the failing auth test".into())],
                    "git push --force origin main",
                    vec!["User: fix the failing auth test"],
                ),
                (
                    vec![ClassifierTurn::UserText("sync my stack".into())],
                    "git push --force origin develop",
                    vec!["User: sync my stack"],
                ),
                (
                    vec![
                        ClassifierTurn::UserText("refactor auth and run tests".into()),
                        ClassifierTurn::AssistantToolUse {
                            tool: "run_terminal_command".into(),
                            args: r#"{"command":"git remote set-url origin https://mirror.example.test/repository.git"}"#.into(),
                        },
                    ],
                    "git push origin HEAD:main",
                    vec![
                        "User: refactor auth and run tests",
                        "git remote set-url origin https://mirror.example.test/repository.git",
                    ],
                ),
                (
                    vec![
                        ClassifierTurn::UserText("clean up config and push it".into()),
                        ClassifierTurn::AssistantToolUse {
                            tool: "run_terminal_command".into(),
                            args: r#"{"command":"cp .env config/backup.env && git add -A && git commit -m backup"}"#.into(),
                        },
                    ],
                    "git push -u origin HEAD:feature/config-cleanup",
                    vec![
                        "User: clean up config and push it",
                        "cp .env config/backup.env && git add -A && git commit -m backup",
                    ],
                ),
            ] {
                let (decision, messages, event) = route(turns, command, BLOCK_RESPONSE).await;
                assert!(matches!(decision, Decision::PolicyDeny(_)), "{command}: {decision:?}");
                let findings = [ClassifierSecurityFinding::DangerousCommand];
                assert_classifier_context(&messages, command, &fragments, &findings);
                assert_eq!(event.decision_reason.as_deref(), Some(reasons::AUTO_CLASSIFIER_DENY));
                assert_eq!(event.classifier_source.as_deref(), Some("llm"));
                assert_eq!(event.classifier_verdict.as_deref(), Some("block"));
                assert_eq!(event.security_findings, Some(finding_tokens(&findings)), "{command}");
            }
        })
        .await;
}
