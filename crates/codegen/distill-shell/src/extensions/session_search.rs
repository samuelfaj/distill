// Modified for Distill by Samuel Fajreldines, 2026.
//! ACP extension handler for session search (`x.ai/session/search`).
//!
//! Exposes session full-text search as an ACP extension method.
//! The client sends a query and receives ranked results across all (or workspace-filtered) past sessions.
//!
//! ```text
//! JSON-RPC -> mvp_agent.ext_method()
//!          -> session_search::handle()
//!          -> storage::search::execute_search()
//!          -> search_fts::SessionSearchIndex (SQLite FTS5)
//! ```

use std::io;

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use crate::agent::MvpAgent;
use crate::session::storage::search::{SessionSearchRequest, SessionSearchResponse};
use crate::session::storage::search_fts::SessionSearchRow;
use crate::session::visibility::{ClassifiedSessionKind, HeadlessPolicy, policy_admits};

use super::ExtResult;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchSessionsRequest {
    /// The search query string.
    pub query: String,
    /// Optional workspace directory to scope results to.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Maximum number of results to return. Defaults to 20.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Offset for pagination. Defaults to 0.
    #[serde(default)]
    pub offset: usize,
    /// Whether to include content snippets in results.
    #[serde(default)]
    pub include_content: bool,
    /// Headless policy (`"exclude"|"only"|"include"`).
    /// Omission preserves legacy inclusive search; unknown explicit values fail closed to exclude.
    #[serde(default)]
    pub headless: Option<String>,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchSessionsResponse {
    pub results: Vec<SearchSessionHit>,
    pub next_offset: Option<usize>,
    pub total_estimate: Option<usize>,
    /// True when the FTS5 index is still being bootstrapped.
    pub bootstrapping: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSessionHit {
    pub session_id: String,
    pub cwd: String,
    /// Session title/summary for display
    pub summary: String,
    /// RFC 3339 formatted updated_at
    pub updated_at: String,
    pub score: f32,
    pub matched_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Route `x.ai/session/search` extension method calls.
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/session/search" => {
            let req: SearchSessionsRequest = super::parse_params(args)?;
            let headless = HeadlessPolicy::from_wire(req.headless.as_deref());
            let SearchSessionsRequest {
                query,
                cwd,
                limit,
                offset,
                include_content,
                headless: _,
            } = req;
            validate_search_window(limit, offset)?;

            let kind_index = std::sync::Arc::new(
                tokio::task::spawn_blocking(crate::session::persistence::SessionKindIndex::load)
                    .await
                    .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
                    .map_err(|error| acp::Error::internal_error().data(error.to_string()))?,
            );
            let decision = agent.search_index();
            let root_dir = crate::util::distill_home::distill_home();
            let fetch = |index_offset: usize, batch: usize| {
                let query = query.clone();
                let cwd = cwd.clone();
                let root_dir = root_dir.clone();
                let kind_index = kind_index.clone();
                async move {
                    let internal_req = SessionSearchRequest {
                        query,
                        cwd,
                        limit: batch,
                        offset: index_offset,
                        include_content,
                    };
                    let resp = to_response(
                        crate::session::storage::search::execute_search(
                            decision,
                            &root_dir,
                            &internal_req,
                        )
                        .await?,
                    );
                    let has_more = resp.next_offset.is_some();
                    let bootstrapping = resp.bootstrapping;
                    let hits = tokio::task::spawn_blocking(move || {
                        resp.results
                            .into_iter()
                            .map(|hit| {
                                let kind = kind_index.kind(&hit.session_id);
                                (hit, kind)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                    .map_err(io::Error::other)?;
                    Ok(ClassifiedPage {
                        hits,
                        has_more,
                        bootstrapping,
                    })
                }
            };
            let result = walk_admitted_window(fetch, offset, limit, headless)
                .await
                .map_err(|e| anyhow::anyhow!(e));

            super::to_ext_response(result)
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

const MAX_SEARCH_RESULTS: usize = 100;
const MAX_FILTERED_OFFSET: usize = 1_000;
/// Hard cap on authoritative summary resolutions for one request.
const MAX_CLASSIFIED_HITS: usize = 1_200;
const WALK_BATCH: usize = 50;

fn validate_search_window(limit: usize, offset: usize) -> Result<(), acp::Error> {
    if limit == 0 || limit > MAX_SEARCH_RESULTS || offset > MAX_FILTERED_OFFSET {
        return Err(acp::Error::invalid_params().data(format!(
            "session search limit must be 1..={MAX_SEARCH_RESULTS} and offset <= {MAX_FILTERED_OFFSET}"
        )));
    }
    Ok(())
}

struct ClassifiedPage {
    hits: Vec<(SearchSessionHit, ClassifiedSessionKind)>,
    has_more: bool,
    bootstrapping: bool,
}

/// Fill one policy-admitted window while classifying at most [`MAX_CLASSIFIED_HITS`] raw hits.
/// `offset` and pagination metadata describe the filtered view.
/// Exhaustion yields an exact total; a capped walk returns no exact total and no continuation claim rather than doing unbounded I/O.
async fn walk_admitted_window<F, Fut>(
    mut fetch: F,
    offset: usize,
    limit: usize,
    headless: HeadlessPolicy,
) -> io::Result<SearchSessionsResponse>
where
    F: FnMut(usize, usize) -> Fut,
    Fut: Future<Output = io::Result<ClassifiedPage>>,
{
    let batch = limit.clamp(WALK_BATCH, MAX_CLASSIFIED_HITS);
    let mut index_offset = 0;
    // Admitted hits consumed to honor the filtered-view `offset`.
    let mut skipped = 0;
    let mut results = Vec::new();
    let mut has_more_results = false;
    let mut is_exhausted = false;
    let first_page = fetch(index_offset, batch).await?;
    let mut bootstrapping = first_page.bootstrapping;
    let mut page = first_page;
    loop {
        let remaining = MAX_CLASSIFIED_HITS.saturating_sub(index_offset);
        // A truncated last FTS page still has unclassified tail hits, so `has_more == false` is not exhaustion of the filtered view
        let truncated = page.hits.len() > remaining;
        let page_len = page.hits.len().min(remaining);
        for (hit, kind) in page.hits.into_iter().take(page_len) {
            if !policy_admits(headless, kind) {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if results.len() < limit {
                results.push(hit);
            } else {
                has_more_results = true;
                break;
            }
        }
        if has_more_results {
            break;
        }
        index_offset += page_len;
        if truncated || (index_offset >= MAX_CLASSIFIED_HITS && page.has_more) {
            break;
        }
        if !page.has_more || page_len == 0 {
            is_exhausted = true;
            break;
        }
        page = fetch(index_offset, batch.min(MAX_CLASSIFIED_HITS - index_offset)).await?;
        bootstrapping |= page.bootstrapping;
    }
    Ok(SearchSessionsResponse {
        next_offset: has_more_results.then(|| offset + results.len()),
        total_estimate: is_exhausted.then(|| skipped + results.len()),
        results,
        bootstrapping,
    })
}

#[cfg(test)]
#[path = "session_search_tests.rs"]
mod tests;

/// Convert the internal response to the ACP-facing response.
fn to_response(resp: SessionSearchResponse) -> SearchSessionsResponse {
    SearchSessionsResponse {
        results: resp
            .results
            .into_iter()
            .map(|row: SessionSearchRow| SearchSessionHit {
                session_id: row.session_id,
                cwd: row.cwd,
                summary: row.title,
                updated_at: chrono::DateTime::from_timestamp(row.updated_at_unix, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default(),
                score: row.score,
                matched_fields: row.matched_fields,
                snippet: row.snippet,
            })
            .collect(),
        next_offset: resp.next_offset,
        total_estimate: resp.total_estimate,
        bootstrapping: resp.bootstrapping,
    }
}
