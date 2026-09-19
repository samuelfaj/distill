// Modified for Distill by Samuel Fajreldines, 2026.
use super::WorkspaceHostKind;
use crate::LockedTestEnv;
use crate::capability::CapabilityMode;
use crate::config::SessionContextFactory;
use crate::handle::tests::{bind_resolver_fixture, handler_names};
use crate::handle::{LocalWorkspaceConnectOptions, WorkspaceHandle, build_local_workspace};
use crate::hub_ids::WORKSPACE_RPC_TOOL_ID;
use crate::session::tool_config::resolve_session_toolset;
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use distill_computer_hub_sdk::{AuthCredential, SharedAuthProvider};
use distill_tools::implementations::distill::image_gen::ImageGenClient;
use distill_tools::implementations::distill::video_gen::VideoGenClient;
use distill_tools::implementations::web_search::WebSearchConfig;
use distill_tools::implementations::web_search::client::WebSearchClient;
use distill_tools::registry::types::{FinalizedToolset, ToolServerConfig};
use distill_tool_protocol::SessionId;
/// The tools a hub-only host must not offer: each one calls the API with the server's own credential.
const API_BACKED_TOOLS: &[&str] = &[
    "web_search",
    "image_gen",
    "image_to_video",
    "reference_to_video",
];
const API_BASE_URL: &str = "https://api.invalid/v1";
/// Tool ids without their namespace, in catalog order.
fn unqualified_ids(config: &ToolServerConfig) -> Vec<&str> {
    config
        .tools
        .iter()
        .map(|tool| {
            tool.id
                .rsplit_once(':')
                .map_or(tool.id.as_str(), |(_, id)| id)
        })
        .collect()
}
fn bearer() -> SharedAuthProvider {
    Arc::new(AuthCredential::bearer("serve-scoped-token"))
}
#[test]
fn a_caller_that_names_no_host_is_hub_only() {
    assert_eq!(WorkspaceHostKind::Daemon, WorkspaceHostKind::default());
    assert!(WorkspaceHostKind::default().is_hub_only());
    assert!(!WorkspaceHostKind::Sandbox.is_hub_only());
}
/// Only a daemon's root is edited from outside the agent, so only a daemon arms the OS watcher.
#[test]
fn only_a_daemon_streams_fs_changes() {
    assert!(WorkspaceHostKind::Daemon.streams_fs_changes());
    assert!(!WorkspaceHostKind::Sandbox.streams_fs_changes());
}
/// The sandbox keeps the whole workspace catalog, API-backed tools included; the daemon gets the
/// same catalog with exactly those tools cut, in the same order.
#[test]
fn daemon_catalog_is_the_sandbox_catalog_minus_the_api_backed_tools() {
    let full = distill_agent::workspace_distill_toolset();
    let sandbox = WorkspaceHostKind::Sandbox.default_toolset();
    let daemon = WorkspaceHostKind::Daemon.default_toolset();
    let full_ids = unqualified_ids(&full);
    for tool in API_BACKED_TOOLS {
        assert!(
            full_ids.contains(tool),
            "the sandbox catalog must still ship `{tool}`: {full_ids:?}"
        );
    }
    assert_eq!(full_ids, unqualified_ids(&sandbox));
    let expected: Vec<&str> = full_ids
        .iter()
        .copied()
        .filter(|id| !API_BACKED_TOOLS.contains(id))
        .collect();
    assert_eq!(expected, unqualified_ids(&daemon));
}
/// Build a session the way `connect_local_workspace` does for `host`, with a bearer credential on hand.
async fn session_for(host: WorkspaceHostKind) -> Arc<FinalizedToolset> {
    let factory = host.session_context_factory(bearer(), API_BASE_URL.to_owned());
    let (_effective, toolset, _backend) = resolve_session_toolset(
        host.default_toolset(),
        CapabilityMode::All,
        &[],
        &[],
        PathBuf::from("/tmp"),
        Arc::new(HashMap::new()),
        &format!("host-kind-{host:?}"),
        &factory,
        None,
        None,
        None,
        None,
    )
    .expect("the host's catalog must finalize");
    toolset
}
/// A daemon host builds sessions with no credential: no gen or search config, and no auth provider
/// or API client in `Resources`. The sandbox still hands them all the credential.
#[tokio::test]
async fn only_a_sandbox_session_carries_the_credential() {
    for (host, expected) in [
        (WorkspaceHostKind::Daemon, false),
        (WorkspaceHostKind::Sandbox, true),
    ] {
        let factory = host.session_context_factory(bearer(), API_BASE_URL.to_owned());
        let ctx = factory.build_session_context(
            &format!("host-kind-{host:?}-ctx"),
            PathBuf::from("/tmp"),
            Arc::new(HashMap::new()),
            factory.build_terminal_backend().backend().clone(),
        );
        assert_eq!(expected, ctx.auth_provider.is_some(), "{host:?}");
        assert_eq!(expected, ctx.image_gen_config.has_credentials(), "{host:?}");
        assert_eq!(expected, ctx.video_gen_config.is_enabled(), "{host:?}");
        assert_eq!(
            expected,
            matches!(ctx.web_search_config, WebSearchConfig::Enabled { .. }),
            "{host:?}"
        );
        let toolset = session_for(host).await;
        let resources = toolset.resources.lock().await;
        assert_eq!(
            expected,
            resources.contains::<SharedAuthProvider>(),
            "{host:?}"
        );
        assert_eq!(expected, resources.contains::<ImageGenClient>(), "{host:?}");
        assert_eq!(expected, resources.contains::<VideoGenClient>(), "{host:?}");
        assert_eq!(
            expected,
            resources.contains::<WebSearchClient>(),
            "{host:?}"
        );
    }
}
/// Run `test` against everything `connect_local_workspace` builds for `host` short of the hub
/// connection, under a private workspace home and with data collection switched on: the
/// production seams decide the catalog, bind policy, credential reach and uploads, not a copy of
/// the rule.
fn with_workspace<F: Future<Output = ()>>(
    host: WorkspaceHostKind,
    test: impl FnOnce(WorkspaceHandle) -> F,
) {
    let root = tempfile::tempdir().expect("workspace root");
    let _env = LockedTestEnv::lock()
        .set("GROK_WORKSPACE_HOME", &root.path().join("home"))
        .set(
            "GROK_WORKSPACE_DATA_COLLECTION_DISABLED",
            Path::new("false"),
        );
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let handle = build_local_workspace(
                root.path().to_path_buf(),
                url::Url::parse("ws://127.0.0.1:1/").expect("hub url"),
                bearer(),
                LocalWorkspaceConnectOptions {
                    allow_insecure_ws: true,
                    host_kind: host,
                    ..LocalWorkspaceConnectOptions::default()
                },
            )
            .await
            .expect("workspace");
            handle.create_session("main").expect("main session");
            test(handle).await;
        });
}
fn pinned(tool_ids: &[&str]) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = tool_ids.iter().map(|id| json!({ "id": id })).collect();
    json!({ "metadata": { "tools": tools } })
}
/// A daemon host, as `connect_local_workspace` builds it: binds must pin their toolset (one
/// without fails closed with `missing_tool_config`), a pinned API-backed tool comes back unserved
/// rather than registering without its client, no upload machinery exists even with data
/// collection switched on, and the deployer RPCs have no credential to carry off-host.
#[test]
fn a_daemon_host_serves_pinned_toolsets_only_and_can_neither_upload_nor_deploy() {
    with_workspace(WorkspaceHostKind::Daemon, |handle| async move {
        let shared = handle.shared();
        assert!(shared.require_explicit_toolset);
        assert!(shared.upload_queue().is_none());
        assert!(shared.auth_provider().is_none());
        let resolver = bind_resolver_fixture(&handle);
        let served = resolver(
            SessionId::new("daemon-pinned").unwrap(),
            Some(pinned(&[
                "Distill:read_file",
                "Distill:web_search",
                "Distill:image_gen",
            ])),
        )
        .await
        .expect("a pinned bind is served");
        let names = handler_names(&served);
        assert!(names.iter().any(|n| n == "read_file"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "web_search" || n == "image_gen"),
            "{names:?}"
        );
        assert_eq!(
            vec![
                "Distill:image_gen".to_owned(),
                "Distill:web_search".to_owned()
            ],
            served.unserved_tool_ids
        );
        assert_eq!(None, served.resolve_error);
        let refused = resolver(SessionId::new("daemon-unpinned").unwrap(), None)
            .await
            .expect("a refused bind still answers");
        assert_eq!(
            vec![WORKSPACE_RPC_TOOL_ID.to_owned()],
            handler_names(&refused),
            "the RPC handler alone"
        );
        let reason = refused
            .resolve_error
            .expect("the bind must say why it failed closed");
        assert!(reason.starts_with("missing_tool_config:"), "{reason}");
    });
}
/// The sandbox is unchanged: uploads and the deployer credential are in place, a pinned API-backed
/// tool is served, and a bind without a toolset still widens to the full catalog unless the
/// launcher asked for strict mode itself.
#[test]
fn a_sandbox_host_keeps_uploads_the_credential_and_lax_binds() {
    with_workspace(WorkspaceHostKind::Sandbox, |handle| async move {
        let shared = handle.shared();
        assert!(!shared.require_explicit_toolset);
        assert!(shared.upload_queue().is_some());
        assert!(shared.auth_provider().is_some());
        let resolver = bind_resolver_fixture(&handle);
        let served = resolver(
            SessionId::new("sandbox-pinned").unwrap(),
            Some(pinned(&["Distill:read_file", "Distill:web_search"])),
        )
        .await
        .expect("bind");
        let names = handler_names(&served);
        assert!(names.iter().any(|n| n == "web_search"), "{names:?}");
        assert!(served.unserved_tool_ids.is_empty());
        assert_eq!(None, served.resolve_error);
        let widened = resolver(SessionId::new("sandbox-unpinned").unwrap(), None)
            .await
            .expect("bind");
        let names = handler_names(&widened);
        assert!(
            names.iter().any(|n| n == "web_search") && names.iter().any(|n| n == "read_file"),
            "{names:?}"
        );
        assert_eq!(None, widened.resolve_error);
    });
}
