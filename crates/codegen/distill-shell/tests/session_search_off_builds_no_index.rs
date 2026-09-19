// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

use agent_client_protocol as acp;
use distill_shell::session::info::Info;
use distill_shell::session::storage::search::{
    IndexDecision, SessionSearchRequest, execute_search,
};
use distill_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use distill_test_support::EnvGuard;

#[tokio::test]
async fn saved_session_is_neither_indexed_nor_found_with_search_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let _home = EnvGuard::set("GROK_HOME", root);
    let _off = EnvGuard::set("GROK_SESSION_SEARCH", "0");

    let config = distill_shell::config::load_agent_config_disk_only().expect("config loads");
    let search = distill_shell::session::storage::search::start_if_enabled(&config);
    assert!(
        search.index().is_none(),
        "the switch is off, so no index is started"
    );
    assert_eq!(
        search.off_reason(),
        Some("the GROK_SESSION_SEARCH environment variable"),
        "the caller is told which setting to look at, not which enum arm"
    );

    let info = Info {
        id: acp::SessionId::new("s1"),
        cwd: "/ws".to_string(),
    };
    let storage = JsonlStorageAdapter::with_root(root.to_path_buf());
    storage
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .unwrap();
    storage
        .update_session_title(&info, "zzqqtitle".to_string())
        .await
        .unwrap();

    let resp = execute_search(
        IndexDecision::settled(&search),
        root,
        &SessionSearchRequest {
            query: "zzqqtitle".to_string(),
            cwd: None,
            limit: 10,
            offset: 0,
            include_content: false,
        },
    )
    .await
    .unwrap();

    assert!(resp.results.is_empty(), "a search must find nothing");
    // Prefix, not the plain name: on a network home the journal mode picks a per-host sibling file
    // That is why the operator doc says to delete `session_search*`
    let index_files: Vec<String> = std::fs::read_dir(root.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("session_search"))
        .collect();
    assert!(
        index_files.is_empty(),
        "the switch is off, so no index may be built, found {index_files:?}",
    );
}
