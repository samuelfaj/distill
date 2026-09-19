// Modified for Distill by Samuel Fajreldines, 2026.
//! Maps `WorkspaceError` to and from wire codes for the workspace RPC envelope (the envelope types are canonical in `distill_workspace_types::rpc`).
use crate::error::WorkspaceError;
pub use distill_workspace_types::rpc::{RpcEnvelope, RpcError};
pub fn envelope_err<T>(error: &WorkspaceError) -> RpcEnvelope<T> {
    RpcEnvelope::err_parts(error_code(error), error.to_string())
}
/// The match has no wildcard, so a new `WorkspaceError` variant fails to compile until it gets a wire code.
pub fn error_code(err: &WorkspaceError) -> &'static str {
    match err {
        WorkspaceError::ParentSessionNotFound(_) => "parent_session_not_found",
        WorkspaceError::SessionNotFound(_) => "session_not_found",
        WorkspaceError::SessionAlreadyExists(_) => "session_already_exists",
        WorkspaceError::EmptyAgentId => "empty_agent_id",
        WorkspaceError::CannotDropMainSession => "cannot_drop_main",
        WorkspaceError::Finalize(_) => "finalize",
        WorkspaceError::CapabilityWidening { .. } => "capability_widening",
        WorkspaceError::Unauthorized { .. } => "unauthorized",
        WorkspaceError::TurnActive(_) => distill_workspace_types::rpc::envelope::TURN_ACTIVE,
        WorkspaceError::MaxDepthExceeded { .. } => "max_depth_exceeded",
        WorkspaceError::JoinError(_) => "join_error",
        WorkspaceError::InvalidHunkAction(_) => "invalid_hunk_action",
        WorkspaceError::HunkActionFailed(_) => "hunk_action_failed",
        WorkspaceError::HubError(_) | WorkspaceError::HubRefused { .. } => "hub_error",
        WorkspaceError::UnknownMethod(_) => "unknown_method",
        WorkspaceError::ExportArchiveLimitExceeded(_) => "export_archive_limit_exceeded",
        WorkspaceError::ExportGithub { kind, .. } => kind.wire_code(),
        WorkspaceError::ShuttingDown => "shutting_down",
        WorkspaceError::ToolsetExternallyOwned(_) => "toolset_externally_owned",
    }
}
/// Known codes map back to their variants. Unknown codes become `HubError` so an older shell survives newer workspace codes.
/// `CapabilityWidening`, `Unauthorized`, and `MaxDepthExceeded` lose struct fields on the wire, so they also become `HubError` with the original code prefixed;
/// `HubRefused` travels as the `hub_error` it renders to.
pub fn rpc_error_to_workspace(err: RpcError) -> WorkspaceError {
    if let Some(kind) =
        distill_workspace_types::rpc::export_github::ExportGithubError::from_wire_code(&err.code)
    {
        return WorkspaceError::ExportGithub {
            kind,
            message: err.message,
        };
    }
    match err.code.as_str() {
        "parent_session_not_found" => WorkspaceError::ParentSessionNotFound(err.message),
        "session_not_found" => WorkspaceError::SessionNotFound(err.message),
        "session_already_exists" => WorkspaceError::SessionAlreadyExists(err.message),
        "empty_agent_id" => WorkspaceError::EmptyAgentId,
        "cannot_drop_main" => WorkspaceError::CannotDropMainSession,
        "finalize" => WorkspaceError::Finalize(err.message),
        "capability_widening" => {
            WorkspaceError::HubError(format!("capability_widening: {}", err.message))
        }
        "unauthorized" => WorkspaceError::HubError(format!("unauthorized: {}", err.message)),
        "turn_active" => WorkspaceError::TurnActive(err.message),
        "max_depth_exceeded" => {
            WorkspaceError::HubError(format!("max_depth_exceeded: {}", err.message))
        }
        "join_error" => WorkspaceError::JoinError(err.message),
        "invalid_hunk_action" => WorkspaceError::InvalidHunkAction(err.message),
        "hunk_action_failed" => WorkspaceError::HunkActionFailed(err.message),
        "hub_error" => WorkspaceError::HubError(err.message),
        "unknown_method" => WorkspaceError::UnknownMethod(err.message),
        "export_archive_limit_exceeded" => WorkspaceError::ExportArchiveLimitExceeded(err.message),
        "shutting_down" => WorkspaceError::ShuttingDown,
        "toolset_externally_owned" => WorkspaceError::ToolsetExternallyOwned(err.message),
        unknown => {
            WorkspaceError::HubError(format!("unknown error code: {unknown}: {}", err.message))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityMode;
    #[test]
    fn error_code_round_trip_all_variants() {
        let mut variants: Vec<WorkspaceError> = vec![
            WorkspaceError::ParentSessionNotFound("p".into()),
            WorkspaceError::SessionNotFound("s".into()),
            WorkspaceError::SessionAlreadyExists("s".into()),
            WorkspaceError::EmptyAgentId,
            WorkspaceError::CannotDropMainSession,
            WorkspaceError::Finalize("f".into()),
            WorkspaceError::CapabilityWidening {
                parent: CapabilityMode::ReadOnly,
                child: CapabilityMode::All,
            },
            WorkspaceError::Unauthorized {
                caller: "a".into(),
                target: "b".into(),
            },
            WorkspaceError::TurnActive("s".into()),
            WorkspaceError::MaxDepthExceeded { parent: "p".into() },
            WorkspaceError::JoinError("j".into()),
            WorkspaceError::InvalidHunkAction("h".into()),
            WorkspaceError::HunkActionFailed("h".into()),
            WorkspaceError::HubError("hub".into()),
            WorkspaceError::HubRefused {
                status: 403,
                refusal: None,
            },
            WorkspaceError::UnknownMethod("workspace.bogus".into()),
            WorkspaceError::ExportArchiveLimitExceeded("too big".into()),
            WorkspaceError::ShuttingDown,
            WorkspaceError::ToolsetExternallyOwned("s".into()),
        ];
        for err in &variants {
            let code = error_code(err);
            assert!(!code.is_empty(), "code must not be empty for {err:?}");
            let rpc_err = RpcError {
                code: code.to_owned(),
                message: err.to_string(),
            };
            let recovered = rpc_error_to_workspace(rpc_err);
            let recovered_code = error_code(&recovered);
            match err {
                WorkspaceError::CapabilityWidening { .. } => {
                    assert_eq!(recovered_code, "hub_error");
                    let msg = recovered.to_string();
                    assert!(
                        msg.contains("capability_widening"),
                        "degraded error should contain original code: {msg}"
                    );
                }
                WorkspaceError::Unauthorized { .. } => {
                    assert_eq!(recovered_code, "hub_error");
                    let msg = recovered.to_string();
                    assert!(
                        msg.contains("unauthorized"),
                        "degraded error should contain original code: {msg}"
                    );
                }
                WorkspaceError::MaxDepthExceeded { .. } => {
                    assert_eq!(recovered_code, "hub_error");
                    let msg = recovered.to_string();
                    assert!(
                        msg.contains("max_depth_exceeded"),
                        "degraded error should contain original code: {msg}"
                    );
                }
                _ => {
                    assert_eq!(
                        recovered_code, code,
                        "round-trip mismatch for {err:?}: got code {recovered_code}"
                    );
                }
            }
        }
    }
    /// Verify unknown codes degrade to HubError.
    #[test]
    fn export_github_codes_round_trip_typed() {
        for kind in distill_workspace_types::rpc::export_github::ExportGithubError::ALL {
            let err = WorkspaceError::ExportGithub {
                kind,
                message: "boom".into(),
            };
            let rpc_err = RpcError {
                code: error_code(&err).into(),
                message: "boom".into(),
            };
            let recovered = rpc_error_to_workspace(rpc_err);
            assert!(
                matches!(recovered, WorkspaceError::ExportGithub { kind: k, .. } if k == kind),
                "lost typed export error for {kind:?}: {recovered:?}"
            );
        }
    }
    #[test]
    fn unknown_code_degrades_to_hub_error() {
        let rpc_err = RpcError {
            code: "future_new_variant".into(),
            message: "something new".into(),
        };
        let recovered = rpc_error_to_workspace(rpc_err);
        assert!(matches!(recovered, WorkspaceError::HubError(_)));
        let msg = recovered.to_string();
        assert!(msg.contains("future_new_variant"));
    }
    #[test]
    fn envelope_serde_round_trip_ok() {
        let env: RpcEnvelope<String> = RpcEnvelope::ok("hello".into());
        let json = serde_json::to_value(&env).unwrap();
        let recovered: RpcEnvelope<String> = serde_json::from_value(json).unwrap();
        match recovered.into_result() {
            Ok(v) => assert_eq!(v, "hello"),
            Err(e) => panic!("expected Ok, got {e:?}"),
        }
    }
    /// Verify serde round-trip of RpcEnvelope error, through the `WorkspaceError` mapping in both directions.
    #[test]
    fn envelope_serde_round_trip_err() {
        let err = WorkspaceError::SessionNotFound("ghost".into());
        let env: RpcEnvelope<String> = envelope_err(&err);
        let json = serde_json::to_value(&env).unwrap();
        let recovered: RpcEnvelope<String> = serde_json::from_value(json).unwrap();
        match recovered.into_result().map_err(rpc_error_to_workspace) {
            Ok(_) => panic!("expected Err"),
            Err(e) => {
                assert_eq!(error_code(&e), "session_not_found");
            }
        }
    }
}
