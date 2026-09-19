use agent_client_protocol::{self as acp};
use serde::{Deserialize, Serialize};

use super::super::mvp_agent::MvpAgent;
use crate::remote::{ListWorkspacesPage, WsError, WsQuery};
use crate::session::ExtMethodResult;

const DEFAULT_PAGE_SIZE: i64 = 50;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspacesListRequest {
    #[serde(default)]
    page_size: Option<i64>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRow {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    create_time: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspacesListResponse {
    workspaces: Vec<WorkspaceRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_page_token: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<WorkspacesMeta>,
}

#[derive(Debug, Serialize)]
struct WorkspacesMeta {
    #[serde(rename = "x.ai/partial")]
    partial: PartialInfo,
}

#[derive(Debug, Serialize)]
struct PartialInfo {
    workspaces: bool,
    reason: &'static str,
}

pub(crate) async fn handle(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
) -> Result<acp::ExtResponse, acp::Error> {
    let req: WorkspacesListRequest = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("invalid params: {e}")))?;

    let q = WsQuery {
        // A missing, zero, or negative `pageSize` falls back to the default instead of being forwarded verbatim to `/rest/workspaces`
        page_size: match req.page_size {
            Some(n) if n > 0 => n,
            _ => DEFAULT_PAGE_SIZE,
        },
        page_token: req.page_token,
        query: req.query,
        kind: req.kind,
    };

    let response = match agent.workspaces_client().list_workspaces(&q).await {
        Ok(page) => success_response(page),
        Err(WsError::NoOauth) => degraded_response("no_oauth"),
        Err(e) => {
            // Degrade to a partial result, but log the cause so failures in the field stay diagnosable
            tracing::warn!("workspaces/list fetch failed: {e}");
            degraded_response("error")
        }
    };

    ExtMethodResult::success(response)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

fn success_response(page: ListWorkspacesPage) -> WorkspacesListResponse {
    WorkspacesListResponse {
        workspaces: page
            .workspaces
            .into_iter()
            .map(|w| WorkspaceRow {
                id: w.workspace_id,
                name: w.name,
                kind: w.kind,
                create_time: w.create_time,
            })
            .collect(),
        next_page_token: page.next_page_token,
        meta: None,
    }
}

fn degraded_response(reason: &'static str) -> WorkspacesListResponse {
    WorkspacesListResponse {
        workspaces: Vec::new(),
        next_page_token: None,
        meta: Some(WorkspacesMeta {
            partial: PartialInfo {
                workspaces: true,
                reason,
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Workspace;

    #[test]
    fn request_parses_camelcase_and_defaults_page_size() {
        let req: WorkspacesListRequest =
            serde_json::from_value(serde_json::json!({})).expect("empty params parse");
        assert!(req.page_size.is_none());

        let req: WorkspacesListRequest = serde_json::from_value(serde_json::json!({
            "pageSize": 10,
            "pageToken": "tok",
            "query": "gpu",
            "kind": "WORKSPACE_KIND_IMAGINE"
        }))
        .expect("full params parse");
        assert_eq!(req.page_size, Some(10));
        assert_eq!(req.page_token.as_deref(), Some("tok"));
        assert_eq!(req.query.as_deref(), Some("gpu"));
        assert_eq!(req.kind.as_deref(), Some("WORKSPACE_KIND_IMAGINE"));
    }

    #[test]
    fn success_response_projects_grok_workspace_fields() {
        let page = ListWorkspacesPage {
            workspaces: vec![Workspace {
                workspace_id: "ws_1".into(),
                name: "Research".into(),
                create_time: Some("2026-06-18T17:30:00Z".into()),
                kind: Some("WORKSPACE_KIND_IMAGINE".into()),
            }],
            next_page_token: Some("tok2".into()),
        };
        let value = serde_json::to_value(success_response(page)).unwrap();
        let Some(ws) = value
            .get("workspaces")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        else {
            panic!("expected one workspace: {value:?}");
        };
        assert_eq!(ws.get("id").and_then(|v| v.as_str()), Some("ws_1"));
        assert_eq!(ws.get("name").and_then(|v| v.as_str()), Some("Research"));
        assert_eq!(
            ws.get("kind").and_then(|v| v.as_str()),
            Some("WORKSPACE_KIND_IMAGINE")
        );
        assert_eq!(
            ws.get("createTime").and_then(|v| v.as_str()),
            Some("2026-06-18T17:30:00Z")
        );
        assert_eq!(
            value.get("nextPageToken").and_then(|v| v.as_str()),
            Some("tok2")
        );
        assert!(value.get("_meta").is_none());
    }

    #[test]
    fn degraded_response_carries_partial_reason() {
        let value = serde_json::to_value(degraded_response("no_oauth")).unwrap();
        assert_eq!(
            value
                .get("workspaces")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0)
        );
        assert!(value.get("nextPageToken").is_none());
        let partial = value.get("_meta").and_then(|m| m.get("x.ai/partial"));
        assert_eq!(
            partial
                .and_then(|p| p.get("workspaces"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            partial
                .and_then(|p| p.get("reason"))
                .and_then(|v| v.as_str()),
            Some("no_oauth")
        );
    }
}
