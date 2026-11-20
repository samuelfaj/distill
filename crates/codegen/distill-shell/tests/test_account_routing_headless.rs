use distill_shell::accounts::{AccountStore, AutoOption, AutoRouting, AutoStrategy, Provider};
use distill_test_support::{
    MockInferenceServer, TestSandbox, assert_headless_success, grok_binary, run_headless_in_sandbox,
};

#[tokio::test]
#[ignore = "requires pre-built distill binary via GROK_BINARY; run with --ignored"]
async fn headless_explicit_byok_model_admits_with_all_auto_accounts_disconnected() {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_response("explicit model accepted");
    let mut sandbox = TestSandbox::builder().mock_url(server.url()).build();
    let home = sandbox.distill_home().to_path_buf();
    sandbox.set_env("DISTILL_HOME", &home);
    let store = AccountStore::new(home.clone());
    store
        .add("offline", Provider::Chatgpt, "Disconnected ChatGPT")
        .unwrap();
    store
        .save_routing(&AutoRouting {
            enabled: true,
            strategy: AutoStrategy::OrderedFirst,
            options: vec![AutoOption {
                id: "offline".into(),
                account_id: "offline".into(),
                main_model: "gpt-6.1-sol".into(),
                worker_fallback_model: None,
                worker_model: None,
            }],
        })
        .unwrap();
    assert!(store.load_routing().unwrap().enabled);
    assert!(!store.is_connected("offline").unwrap());
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"[model.my-byok]
model = "chosen-wire-model"
base_url = "{}"
api_key = "explicit-byok-test-key"
api_backend = "chat_completions"
context_window = 262144
max_completion_tokens = 4096
"#,
            server.url(),
        ),
    )
    .unwrap();

    // The real headless ACP session/new omits modelId; -m must reach admission itself.
    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args([
        "-p",
        "hello",
        "-m",
        "my-byok",
        "--yolo",
        "--output-format",
        "json",
        "--cwd",
    ])
    .arg(sandbox.workspace())
    .current_dir(sandbox.workspace());
    let result = run_headless_in_sandbox(cmd, sandbox).await;
    assert_headless_success(
        &result,
        "explicit BYOK with unavailable Auto",
        Some(&server),
    );
    let requests = server.requests();
    let inference = requests
        .iter()
        .find(|request| {
            request.path == "/v1/chat/completions"
                && request
                    .body
                    .as_ref()
                    .is_some_and(|body| body["model"] == "chosen-wire-model")
        })
        .expect("the explicitly selected wire model reached inference");
    assert_eq!(
        inference.authorization.as_deref(),
        Some("Bearer explicit-byok-test-key")
    );
    assert!(inference.header("x-distill-credential-authority").is_none());
}
