mod cursor;
mod envelope;
mod facets;
mod row;
use crate::agent::session_registry_client::SessionRegistryClient;
use crate::remote::{ConvError, ConvQuery, ConversationsClient};
pub use crate::session::merge::CwdScope;
pub use crate::session::visibility::HeadlessPolicy;
use agent_client_protocol as acp;
use cursor::{CompositeCursor, ConvLane, Paginated, merge_and_paginate};
pub use envelope::{FacetMap, FacetValue, SessionKind, SessionMetaEnvelope};
pub use facets::{
    BRANCH_FACET_KEY, BranchFacet, CWD_FACET_KEY, CwdFacet, FacetProvider, FacetRegistry,
    FacetSummary, FacetSummaryKey, FacetSummaryValue, GIT_ROOT_FACET_KEY, GitRootFacet,
    KIND_FACET_KEY, KindFacet, NormalizedItem, Pushdown, REPO_FACET_KEY, RepoFacet,
    SOURCE_WORKSPACE_FACET_KEY, STARRED_FACET_KEY, SourceQuery, SourceWorkspaceFacet, StarredFacet,
    WORKSPACE_FACET_KEY, WORKTREE_FACET_KEY, WorkspaceFacet, WorktreeFacet, build_facet_registry,
};
pub use row::{
    ExtSupersetRow, RowMeta, SessionInfo, UnifiedRow, conversation_to_row, merged_session_to_row,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;
pub const DEFAULT_LIMIT: usize = 30;
const CONV_PAGE_HEADROOM: usize = 5;
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PartialReason {
    Timeout,
    Error,
    NoOauth,
}
static FACET_REGISTRY: LazyLock<FacetRegistry> = LazyLock::new(build_facet_registry);
pub(crate) fn facet_registry() -> &'static FacetRegistry {
    &FACET_REGISTRY
}
/// Hard-off in release builds so they can't enable the conversations lane via env.
pub(crate) fn conversations_lane_enabled() -> bool {
    false
}
/// Env lane (desktop `GROK_SESSION_LIST_CONVERSATIONS`) OR process-wide `--chat` (`GROK_CHAT_MODE`); hard-off in release builds.
/// The single predicate `MvpAgent::conversations_client()` keys on.
pub fn conversations_lane_active() -> bool {
    conversations_lane_enabled() || crate::agent::chat_modes::process_chat_mode_enabled()
}
/// Parse `x.ai/session/list` params and, under process-wide chat mode, force the conversations-only `kind` facet.
/// Client-sent `kind` of `chat`/`build` is honored only behind `feature = "local-workspace"` (pager welcome Local history).
/// Chat-only Desktop/ACP agents keep the force-rewrite so `kind: ["build"]` cannot surface Build rows.
pub fn parse_list_req(raw: &str) -> Result<ListReq, serde_json::Error> {
    let mut req: ListReq = serde_json::from_str(raw)?;
    if crate::agent::chat_modes::process_chat_mode_enabled() {
        let honor_client_kind = cfg!(feature = "local-workspace") && client_sent_kind_filter(&req);
        if !honor_client_kind {
            force_kind_chat(&mut req);
        }
    }
    Ok(req)
}
fn cwd_scope_from_allow_relax<'de, D>(deserializer: D) -> Result<CwdScope, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(if bool::deserialize(deserializer)? {
        CwdScope::RelaxIfEmpty
    } else {
        CwdScope::WithSiblings
    })
}
fn client_sent_kind_filter(req: &ListReq) -> bool {
    let Some(kind) = req
        .meta
        .as_ref()
        .and_then(|m| m.get("x.ai/facetFilters"))
        .and_then(|f| f.get("kind"))
    else {
        return false;
    };
    match kind {
        serde_json::Value::Array(arr) if !arr.is_empty() => arr
            .iter()
            .any(|v| matches!(v.as_str(), Some("chat" | "build"))),
        serde_json::Value::String(s) if s == "chat" || s == "build" => true,
        _ => false,
    }
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListReq {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    /// Which directories the listing draws from. The wire carries the original `allowRelax` boolean.
    /// `Only` is reachable only in code (ACP `session/list`), so "exact" and "relax" cannot be requested together.
    /// A relaxed response sets `_meta["x.ai/listScope"]`, re-evaluated per page.
    #[serde(
        default,
        rename = "allowRelax",
        deserialize_with = "cwd_scope_from_allow_relax"
    )]
    pub cwd_scope: CwdScope,
    /// `session_kind=headless` policy: `"exclude"` | `"only"` | `"include"`.
    /// Omission preserves the legacy inclusive behavior; unknown explicit values fail closed to exclude.
    #[serde(default)]
    pub headless: Option<String>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}
/// Directory scope the returned sessions were drawn from.
/// Wire form is the `as_str` value (`x.ai/listScope`), so no serde derive is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ListScope {
    /// Scoped to the request cwd.
    #[default]
    Cwd,
    /// Relaxed to the cwd's repo when the cwd itself had no sessions.
    Repo,
    /// Relaxed to all directories when the cwd is not a git repo.
    All,
}
impl ListScope {
    /// True when the scope relaxed past the cwd, to the repo or to all directories.
    pub const fn is_relaxed(self) -> bool {
        !matches!(self, Self::Cwd)
    }
}
pub struct UnifiedListResult {
    pub rows: Vec<UnifiedRow>,
    pub next_cursor: Option<String>,
    pub facets: FacetSummary,
    pub conversations_partial: Option<PartialReason>,
    /// Directory scope `rows` were drawn from; see [`ListReq::allow_relax`].
    pub scope: ListScope,
}
#[derive(Debug, Default)]
struct ParsedMeta {
    facet_filters: BTreeMap<String, Vec<serde_json::Value>>,
    query: Option<String>,
    limit: Option<usize>,
}
impl ParsedMeta {
    fn parse(meta: Option<&serde_json::Value>) -> Self {
        let Some(meta) = meta else {
            return Self::default();
        };
        let facet_filters = meta
            .get("x.ai/facetFilters")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), value_list(v)))
                    .collect()
            })
            .unwrap_or_default();
        let query = meta
            .get("x.ai/query")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let limit = meta
            .get("x.ai/limit")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        Self {
            facet_filters,
            query,
            limit,
        }
    }
}
fn value_list(v: &serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(arr) => arr.clone(),
        other => vec![other.clone()],
    }
}
/// Rewrite `req` so the `kind` facet filter is exactly `["chat"]`.
/// Used when process chat mode is on and the client omitted a recognized `kind` facet.
/// Welcome history sends an explicit `kind` (`chat` / `build`) that must not be rewritten.
pub(crate) fn force_kind_chat(req: &mut ListReq) {
    force_kind(req, SessionKind::Chat);
}
/// REPLACES any client-sent `kind` allow-list (a union would re-enable the excluded lanes); every other facet filter and `_meta` key is untouched.
pub(crate) fn force_kind(req: &mut ListReq, kind: SessionKind) {
    let mut meta = match req.meta.take() {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let mut filters = match meta.remove("x.ai/facetFilters") {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    filters.insert(
        KIND_FACET_KEY.to_owned(),
        serde_json::json!([kind.as_ref()]),
    );
    meta.insert(
        "x.ai/facetFilters".to_owned(),
        serde_json::Value::Object(filters),
    );
    req.meta = Some(serde_json::Value::Object(meta));
}
pub async fn build_unified_list(
    registry_client: Option<&SessionRegistryClient>,
    conversations_client: Option<&ConversationsClient>,
    mut req: ListReq,
) -> UnifiedListResult {
    if crate::agent::chat_modes::process_chat_mode_enabled() && !client_sent_kind_filter(&req) {
        force_kind_chat(&mut req);
    }
    let reg = facet_registry();
    let ParsedMeta {
        facet_filters,
        query: meta_query,
        limit: meta_limit,
    } = ParsedMeta::parse(req.meta.as_ref());
    let limit = req.limit.or(meta_limit).unwrap_or(DEFAULT_LIMIT);
    let query = req.query.or(meta_query);
    let cursor = CompositeCursor::decode(req.cursor.as_deref());
    let mut source_query = SourceQuery::default();
    reg.apply_pushdown(&facet_filters, &mut source_query);
    let headless = HeadlessPolicy::from_wire(req.headless.as_deref());
    let exclude_conversations = excludes_conversations(&facet_filters, headless);
    let exclude_build = excludes_build(&facet_filters);
    let over = crate::session::merge::over_fetch(limit);
    let cwd_scope = req.cwd_scope;
    let can_relax = relax_eligible(RelaxGate {
        opted_in: matches!(req.cwd_scope, CwdScope::RelaxIfEmpty),
        no_facet_filters: facet_filters.is_empty(),
        has_cwd: req.cwd.is_some(),
        is_search: query.is_some(),
    });
    let local_fut = async {
        if exclude_build {
            return LocalLane::default();
        }
        let cwd = req.cwd.as_deref();
        if can_relax {
            let lanes = crate::session::merge::fetch_lanes(
                registry_client,
                cwd,
                cwd_scope,
                None,
                over,
                headless,
            )
            .await;
            let rows = to_rows(
                crate::session::merge::merge(
                    lanes.remote.clone(),
                    lanes.local,
                    None,
                    &lanes.repo_urls,
                    over,
                ),
                reg,
            );
            LocalLane {
                rows,
                relax: Some(RelaxInputs {
                    remote: lanes.remote,
                    repo_urls: lanes.repo_urls,
                    cwd_rows_dropped_by_policy: lanes.rows_dropped_by_policy,
                }),
            }
        } else {
            let merged = crate::session::merge::fetch_merged(
                registry_client,
                cwd,
                cwd_scope,
                query.as_deref(),
                over,
                headless,
            )
            .await;
            LocalLane {
                rows: to_rows(merged, reg),
                relax: None,
            }
        }
    };
    let conv_fut = async {
        if exclude_conversations {
            return ConvLane::Skipped;
        }
        let Some(client) = conversations_client else {
            return ConvLane::Skipped;
        };
        let q = ConvQuery {
            page_size: (limit + CONV_PAGE_HEADROOM) as i64,
            page_token: cursor.conv_page_token.clone(),
            search_query: query.clone(),
            workspace_id: source_query.workspace_id.clone(),
        };
        match tokio::time::timeout(
            crate::session::merge::REMOTE_TIMEOUT,
            client.list_conversations(&q),
        )
        .await
        {
            Ok(Ok(page)) => {
                let next_token = page.next_page_token;
                let rows: Vec<UnifiedRow> = page
                    .conversations
                    .into_iter()
                    .map(|c| conversation_to_row(c, reg))
                    .collect();
                let frontier = cursor::conv_frontier(&rows, next_token.is_some());
                ConvLane::Page {
                    rows,
                    next_token,
                    frontier,
                }
            }
            Ok(Err(ConvError::NoOauth)) => ConvLane::Degraded(PartialReason::NoOauth),
            Ok(Err(e)) => {
                tracing::warn!("conversation list failed: {e}");
                ConvLane::Degraded(PartialReason::Error)
            }
            Err(_) => {
                tracing::warn!("conversation list timed out");
                ConvLane::Degraded(PartialReason::Timeout)
            }
        }
    };
    let (
        LocalLane {
            rows: local_rows,
            relax,
        },
        conv_lane,
    ) = tokio::join!(local_fut, conv_fut);
    let (local_rows, scope) = maybe_relax(local_rows, relax, over, reg, headless).await;
    {
        let (conv_lane_status, conv_rows) = match &conv_lane {
            ConvLane::Skipped => ("skipped", 0),
            ConvLane::Degraded(reason) => (reason.as_ref(), 0),
            ConvLane::Page { rows, .. } => ("ok", rows.len()),
        };
        tracing::debug!(
            local_lane_skipped = exclude_build,
            local_rows = local_rows.len(),
            conv_lane = conv_lane_status,
            conv_rows,
            "session list lanes"
        );
    }
    let local_rows = reg.apply_in_memory_filters(&facet_filters, local_rows);
    let conv_lane = match conv_lane {
        ConvLane::Page {
            rows,
            next_token,
            frontier,
        } => ConvLane::Page {
            rows: reg.apply_in_memory_filters(&facet_filters, rows),
            next_token,
            frontier,
        },
        other => other,
    };
    let Paginated {
        candidates,
        emit_count,
        next_cursor,
        partial,
    } = merge_and_paginate(local_rows, conv_lane, &cursor, limit);
    let mut rows = candidates;
    rows.truncate(emit_count);
    let facets = reg.summarize_window(&rows);
    UnifiedListResult {
        rows,
        next_cursor: next_cursor.map(|c| c.encode()),
        facets,
        conversations_partial: partial,
        scope,
    }
}
#[derive(Default)]
struct LocalLane {
    rows: Vec<UnifiedRow>,
    relax: Option<RelaxInputs>,
}
struct RelaxInputs {
    remote: Vec<crate::agent::session_registry_client::SessionRecord>,
    repo_urls: Vec<String>,
    /// The visibility policy dropped a local row relevant to this cwd/repo.
    cwd_rows_dropped_by_policy: bool,
}
fn to_rows(
    merged: Vec<crate::session::merge::MergedSession>,
    reg: &FacetRegistry,
) -> Vec<UnifiedRow> {
    merged
        .into_iter()
        .map(|m| merged_session_to_row(m, reg))
        .collect()
}
#[derive(Clone, Copy)]
struct RelaxGate {
    opted_in: bool,
    no_facet_filters: bool,
    has_cwd: bool,
    is_search: bool,
}
fn relax_eligible(gate: RelaxGate) -> bool {
    gate.opted_in && gate.no_facet_filters && gate.has_cwd && !gate.is_search
}
/// True when no row has messages (a post-rebuild placeholder counts as empty).
fn lane_has_no_messages(rows: &[UnifiedRow]) -> bool {
    rows.iter().all(|r| r.legacy.num_messages == 0)
}
/// Policy emptied this cwd's local lane (`retain_session_lanes` dropped every remaining row).
/// A partial drop that still leaves interactive husks must not block the stranded-cwd widen.
fn policy_emptied_cwd_lane(dropped: bool, remaining: &[UnifiedRow]) -> bool {
    dropped && remaining.is_empty()
}
async fn maybe_relax(
    local_rows: Vec<UnifiedRow>,
    relax: Option<RelaxInputs>,
    over: usize,
    reg: &FacetRegistry,
    headless: HeadlessPolicy,
) -> (Vec<UnifiedRow>, ListScope) {
    let Some(relax) = relax.filter(|r| {
        !policy_emptied_cwd_lane(r.cwd_rows_dropped_by_policy, &local_rows)
            && lane_has_no_messages(&local_rows)
    }) else {
        return (local_rows, ListScope::Cwd);
    };
    let scope = if relax.repo_urls.is_empty() {
        ListScope::All
    } else {
        ListScope::Repo
    };
    let all_local = crate::session::persistence::list_summaries(None)
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("cwd scan failed: {e}");
            Vec::new()
        });
    match relax_rows(relax, all_local, over, reg, headless) {
        Some(relaxed) => {
            tracing::debug!(
                rows = relaxed.len(),
                scope = scope.as_ref(),
                "cwd empty; relaxing scope"
            );
            (relaxed, scope)
        }
        None => (local_rows, ListScope::Cwd),
    }
}
/// Re-merge the registry page with a repo-scoped local scan (all directories when the cwd is not a repo).
/// Relax only when it reveals a messaged session.
fn relax_rows(
    relax: RelaxInputs,
    mut all_local: Vec<crate::session::persistence::Summary>,
    over: usize,
    reg: &FacetRegistry,
    headless: HeadlessPolicy,
) -> Option<Vec<UnifiedRow>> {
    let RelaxInputs {
        mut remote,
        repo_urls,
        ..
    } = relax;
    crate::session::visibility::retain_session_lanes(&mut all_local, &mut remote, headless);
    let scoped = crate::session::merge::filter_summaries_by_repo(all_local, &repo_urls);
    let rows = to_rows(
        crate::session::merge::merge(remote, scoped, None, &repo_urls, over),
        reg,
    );
    (!lane_has_no_messages(&rows)).then_some(rows)
}
fn excludes_conversations(
    filters: &BTreeMap<String, Vec<serde_json::Value>>,
    headless: HeadlessPolicy,
) -> bool {
    headless == HeadlessPolicy::Only
        || match filters.get(KIND_FACET_KEY) {
            Some(allowed) if !allowed.is_empty() => !allowed
                .iter()
                .any(|v| v.as_str() == Some(SessionKind::Chat.as_ref())),
            _ => false,
        }
}
/// Mirror of [`excludes_conversations`]: `true` when a non-empty `kind` allow-list does not include `"build"`, so the local lane can be skipped.
fn excludes_build(filters: &BTreeMap<String, Vec<serde_json::Value>>) -> bool {
    match filters.get(KIND_FACET_KEY) {
        Some(allowed) if !allowed.is_empty() => !allowed
            .iter()
            .any(|v| v.as_str() == Some(SessionKind::Build.as_ref())),
        _ => false,
    }
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExtListResponse {
    pub sessions: Vec<ExtSupersetRow>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: ExtListResponseMeta,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExtListResponseMeta {
    #[serde(rename = "x.ai/facets")]
    pub facets: FacetSummary,
    #[serde(rename = "x.ai/partial")]
    pub partial: PartialInfo,
    /// Present only when the listing relaxed beyond the cwd.
    #[serde(rename = "x.ai/listScope", skip_serializing_if = "Option::is_none")]
    pub list_scope: Option<&'static str>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PartialInfo {
    pub conversations: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}
fn list_response_meta(result: &UnifiedListResult) -> ExtListResponseMeta {
    ExtListResponseMeta {
        facets: result.facets.clone(),
        partial: PartialInfo {
            conversations: result.conversations_partial.is_some(),
            reason: result.conversations_partial.map(Into::into),
        },
        list_scope: result.scope.is_relaxed().then_some(result.scope.into()),
    }
}
pub(crate) fn ext_list_response(result: UnifiedListResult) -> ExtListResponse {
    let meta = list_response_meta(&result);
    ExtListResponse {
        sessions: result
            .rows
            .into_iter()
            .map(UnifiedRow::into_ext_superset)
            .collect(),
        next_cursor: result.next_cursor,
        meta,
    }
}
pub(crate) fn acp_response_meta(result: &UnifiedListResult) -> Option<acp::Meta> {
    to_meta(serde_json::to_value(list_response_meta(result)))
}
pub(super) fn to_meta<E: std::fmt::Display>(
    value: Result<serde_json::Value, E>,
) -> Option<acp::Meta> {
    match value {
        Ok(serde_json::Value::Object(map)) => Some(map),
        Ok(other) => {
            tracing::warn!(kind = ?other, "session list _meta was not an object");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "session list _meta failed to serialize");
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::merge::MergedSession;
    fn local(session_id: &str, updated_at: &str) -> MergedSession {
        MergedSession {
            session_id: session_id.into(),
            summary: "a summary".into(),
            first_prompt: Some("first prompt".into()),
            updated_at: updated_at.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            cwd: "/Users/me/xai".into(),
            hostname: Some("devbox".into()),
            source: "local".into(),
            model_id: Some("remote-code".into()),
            num_messages: 7,
            last_active_at: Some(updated_at.into()),
            branch: Some("main".into()),
            repo_name: Some("xai".into()),
            worktree_label: Some("wt".into()),
            git_root_dir: Some("/Users/me/xai".into()),
            git_remotes: vec!["git@github.com:example/repo.git".into()],
            source_workspace_dir: Some("/Users/me/xai-src".into()),
            last_turn_summary: None,
            last_recap: None,
            session_kind: Some("worktree".into()),
        }
    }
    fn row(session_id: &str, updated_at: &str) -> UnifiedRow {
        merged_session_to_row(local(session_id, updated_at), facet_registry())
    }
    #[test]
    fn ext_superset_preserves_every_legacy_field_and_adds_title_and_meta() {
        let value = serde_json::to_value(row("s1", "2026-06-18T20:10:00Z").into_ext_superset())
            .expect("serialize");
        for field in [
            "sessionId",
            "summary",
            "firstPrompt",
            "updatedAt",
            "createdAt",
            "cwd",
            "hostname",
            "source",
            "modelId",
            "numMessages",
            "lastActiveAt",
            "branch",
            "repoName",
            "worktreeLabel",
        ] {
            assert!(value.get(field).is_some(), "missing legacy field: {field}");
        }
        assert_eq!(value.get("sessionId").and_then(|v| v.as_str()), Some("s1"));
        assert_eq!(value.get("source").and_then(|v| v.as_str()), Some("local"));
        assert_eq!(value.get("numMessages").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(
            value.get("title").and_then(|v| v.as_str()),
            Some("a summary")
        );
        assert_eq!(
            value
                .get("_meta")
                .and_then(|m| m.get("x.ai/session"))
                .and_then(|s| s.get("kind"))
                .and_then(|v| v.as_str()),
            Some("build")
        );
        assert_eq!(
            value.get("gitRootDir").and_then(|v| v.as_str()),
            Some("/Users/me/xai")
        );
        assert_eq!(
            value
                .get("gitRemotes")
                .and_then(|r| r.get(0))
                .and_then(|v| v.as_str()),
            Some("git@github.com:example/repo.git")
        );
        assert_eq!(
            value.get("sourceWorkspaceDir").and_then(|v| v.as_str()),
            Some("/Users/me/xai-src")
        );
        assert_eq!(
            value.get("sessionKind").and_then(|v| v.as_str()),
            Some("worktree")
        );
    }
    #[test]
    fn facets_carry_kind_and_cwd() {
        let r = row("s1", "2026-06-18T20:10:00Z");
        assert!(matches!(
            r.facets.get(KIND_FACET_KEY),
            Some(FacetValue::One(serde_json::Value::String(k))) if k == "build"
        ));
        assert!(matches!(
            r.facets.get(CWD_FACET_KEY),
            Some(FacetValue::One(serde_json::Value::String(c))) if c == "/Users/me/xai"
        ));
    }
    #[test]
    fn bare_session_info_is_minimal_plus_meta() {
        let value =
            serde_json::to_value(row("s1", "2026-06-18T20:10:00Z").into_session_info()).unwrap();
        assert_eq!(value.get("sessionId").and_then(|v| v.as_str()), Some("s1"));
        assert_eq!(
            value.get("cwd").and_then(|v| v.as_str()),
            Some("/Users/me/xai")
        );
        assert_eq!(
            value.get("title").and_then(|v| v.as_str()),
            Some("a summary")
        );
        assert_eq!(
            value
                .get("_meta")
                .and_then(|m| m.get("x.ai/session"))
                .and_then(|s| s.get("kind"))
                .and_then(|v| v.as_str()),
            Some("build")
        );
        assert!(value.get("summary").is_none());
        assert!(value.get("source").is_none());
    }
    #[test]
    fn total_order_is_updated_at_desc_then_session_id() {
        let mut rows = [
            row("b", "2026-01-01T00:00:00Z"),
            row("a", "2026-06-01T00:00:00Z"),
            row("c", "2026-06-01T00:00:00Z"),
        ];
        rows.sort_by(super::cursor::cmp_total_order);
        let ids: Vec<&str> = rows.iter().map(|r| r.legacy.session_id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }
    #[test]
    fn kind_filter_local_keeps_local_rows() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), vec![serde_json::json!("build")]);
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn kind_filter_conversation_drops_local_rows() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), vec![serde_json::json!("chat")]);
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert!(kept.is_empty());
    }
    #[test]
    fn cwd_filter_is_skipped_in_memory() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(
            CWD_FACET_KEY.to_owned(),
            vec![serde_json::json!("/some/other/dir")],
        );
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn empty_allow_list_is_a_no_op() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), Vec::new());
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn parsed_meta_reads_facet_filters_query_and_limit() {
        let meta = serde_json::json!({
            "x.ai/facetFilters": { "kind": ["build"], "starred": true },
            "x.ai/query": "antelope",
            "x.ai/limit": 5,
        });
        let parsed = ParsedMeta::parse(Some(&meta));
        assert_eq!(parsed.query.as_deref(), Some("antelope"));
        assert_eq!(parsed.limit, Some(5));
        assert_eq!(
            parsed.facet_filters.get("kind"),
            Some(&vec![serde_json::json!("build")])
        );
        assert_eq!(
            parsed.facet_filters.get("starred"),
            Some(&vec![serde_json::json!(true)])
        );
    }
    fn kind_filter(values: &[&str]) -> BTreeMap<String, Vec<serde_json::Value>> {
        let mut filters = BTreeMap::new();
        filters.insert(
            KIND_FACET_KEY.to_owned(),
            values.iter().map(|v| serde_json::json!(v)).collect(),
        );
        filters
    }
    #[test]
    fn only_policy_excludes_the_conversations_lane() {
        let filters = BTreeMap::new();
        assert!(!excludes_conversations(&filters, HeadlessPolicy::Exclude));
        assert!(excludes_conversations(&filters, HeadlessPolicy::Only));
    }
    #[test]
    fn excludes_build_mirrors_excludes_conversations() {
        assert!(excludes_build(&kind_filter(&["chat"])));
        assert!(!excludes_conversations(
            &kind_filter(&["chat"]),
            HeadlessPolicy::Exclude,
        ));
        assert!(!excludes_build(&kind_filter(&["build"])));
        assert!(excludes_conversations(
            &kind_filter(&["build"]),
            HeadlessPolicy::Exclude,
        ));
        assert!(!excludes_build(&kind_filter(&["build", "chat"])));
        assert!(!excludes_conversations(
            &kind_filter(&["build", "chat"]),
            HeadlessPolicy::Exclude,
        ));
        assert!(!excludes_build(&kind_filter(&[])));
        assert!(!excludes_conversations(
            &kind_filter(&[]),
            HeadlessPolicy::Exclude,
        ));
        assert!(!excludes_build(&BTreeMap::new()));
        assert!(!excludes_conversations(
            &BTreeMap::new(),
            HeadlessPolicy::Exclude,
        ));
    }
    /// The forced `kind` REPLACES a client-sent `kind: ["build"]` (never unions), so the local lane stays excluded.
    #[test]
    fn forced_kind_replaces_client_build_filter() {
        let mut req = ListReq {
            meta: Some(serde_json::json!({
                "x.ai/facetFilters": { "kind": ["build"] },
            })),
            ..ListReq::default()
        };
        force_kind_chat(&mut req);
        let parsed = ParsedMeta::parse(req.meta.as_ref());
        assert_eq!(
            parsed.facet_filters.get(KIND_FACET_KEY),
            Some(&vec![serde_json::json!("chat")]),
            "forced kind must replace the client filter, not union with it"
        );
        assert!(excludes_build(&parsed.facet_filters));
        assert!(!excludes_conversations(
            &parsed.facet_filters,
            HeadlessPolicy::Exclude,
        ));
    }
    #[test]
    fn forced_kind_preserves_other_facets() {
        let mut req = ListReq {
            meta: Some(serde_json::json!({
                "x.ai/facetFilters": { "kind": ["build"], "starred": [true], "workspace": ["w1"] },
                "x.ai/query": "antelope",
                "x.ai/limit": 5,
            })),
            ..ListReq::default()
        };
        force_kind_chat(&mut req);
        let parsed = ParsedMeta::parse(req.meta.as_ref());
        assert_eq!(
            parsed.facet_filters.get(KIND_FACET_KEY),
            Some(&vec![serde_json::json!("chat")])
        );
        assert_eq!(
            parsed.facet_filters.get("starred"),
            Some(&vec![serde_json::json!(true)])
        );
        assert_eq!(
            parsed.facet_filters.get("workspace"),
            Some(&vec![serde_json::json!("w1")])
        );
        assert_eq!(parsed.query.as_deref(), Some("antelope"));
        assert_eq!(parsed.limit, Some(5));
    }
    #[test]
    fn forced_kind_creates_facet_filters_when_meta_absent() {
        let mut req = ListReq::default();
        force_kind_chat(&mut req);
        let parsed = ParsedMeta::parse(req.meta.as_ref());
        assert_eq!(
            parsed.facet_filters.get(KIND_FACET_KEY),
            Some(&vec![serde_json::json!("chat")])
        );
    }
    fn xai_auth_manager(dir: &std::path::Path) -> std::sync::Arc<xai_grok_login::AuthManager> {
        let am = std::sync::Arc::new(xai_grok_login::AuthManager::new(
            dir,
            xai_grok_login::GrokComConfig::default(),
        ));
        am.hot_swap(xai_grok_login::GrokAuth {
            auth_mode: xai_grok_login::AuthMode::Oidc,
            oidc_issuer: Some(xai_grok_login::xai_oauth2_issuer().to_owned()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..xai_grok_login::GrokAuth::test_default()
        });
        am
    }
    /// Minimal HTTP/1.1 responder serving `body` as JSON to every request.
    async fn spawn_conversations_stub(body: String) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }
    /// A client-sent `kind: ["build"]` rewritten by [`force_kind_chat`] yields conversations only.
    #[tokio::test]
    #[serial_test::serial]
    async fn forced_kind_serves_conversations_only() {
        let addr = spawn_conversations_stub(
                serde_json::json!({
                "conversations": [
                    { "conversationId": "c1", "title": "Hello", "modifyTime": "2026-07-01T00:00:00Z" },
                    { "conversationId": "c2", "title": "", "modifyTime": "2026-07-02T00:00:00Z" },
                ],
            })
                    .to_string(),
            )
            .await;
        let _env = xai_grok_test_support::EnvGuard::set(
            "GROK_CONVERSATIONS_BASE_URL",
            format!("http://{addr}"),
        );
        let home = tempfile::tempdir().expect("tempdir");
        let client = ConversationsClient::new(xai_auth_manager(home.path()));
        let mut req = ListReq {
            meta: Some(serde_json::json!({
                "x.ai/facetFilters": { "kind": ["build"] },
            })),
            ..ListReq::default()
        };
        force_kind_chat(&mut req);
        let result = build_unified_list(None, Some(&client), req).await;
        let ids: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r.legacy.session_id.as_str())
            .collect();
        assert_eq!(ids, ["c2", "c1"], "conversations only, newest first");
        assert!(
            result
                .rows
                .iter()
                .all(|r| r.legacy.source == "conversation"),
            "no build row may survive the forced kind filter"
        );
        assert_eq!(result.conversations_partial, None);
    }
    /// A degraded conversations lane (no OAuth) is reported through `conversations_partial` instead of failing the list.
    #[tokio::test]
    #[serial_test::serial]
    async fn degraded_conversations_lane_reports_no_oauth() {
        let home = tempfile::tempdir().expect("tempdir");
        let auth = std::sync::Arc::new(xai_grok_login::AuthManager::new(
            home.path(),
            xai_grok_login::GrokComConfig::default(),
        ));
        let client = ConversationsClient::new(auth);
        let mut req = ListReq::default();
        force_kind_chat(&mut req);
        let result = build_unified_list(None, Some(&client), req).await;
        assert!(result.rows.is_empty());
        assert_eq!(result.conversations_partial, Some(PartialReason::NoOauth));
    }
    /// Build-mode canary: with no conversations client the lane is skipped, not degraded.
    #[tokio::test]
    async fn non_chat_list_without_client_skips_conversations_lane() {
        let req = ListReq {
            cwd: Some("/nonexistent/unified-list-canary".into()),
            ..ListReq::default()
        };
        let result = build_unified_list(None, None, req).await;
        assert_eq!(
            result.conversations_partial, None,
            "no client ⇒ lane skipped, never reported as degraded"
        );
        assert!(result.rows.is_empty());
    }
    /// Desktop env lane stays env-gated; process chat mode is feature-gated.
    #[test]
    #[serial_test::serial]
    fn conversations_lane_env_gating_matrix() {
        {
            let _off = xai_grok_test_support::EnvGuard::unset("GROK_SESSION_LIST_CONVERSATIONS");
            assert!(!conversations_lane_enabled());
        }
        {
            let _on = xai_grok_test_support::EnvGuard::set("GROK_SESSION_LIST_CONVERSATIONS", "1");
            assert_eq!(conversations_lane_enabled(), false);
        }
        {
            let _off = xai_grok_test_support::EnvGuard::set("GROK_SESSION_LIST_CONVERSATIONS", "0");
            assert!(!conversations_lane_enabled());
        }
    }
    /// Truth table for `conversations_lane_active`: desktop env lane OR process chat mode, hard-off in release builds.
    #[test]
    #[serial_test::serial]
    fn conversations_lane_active_truth_table() {
        use crate::agent::chat_modes::GROK_CHAT_MODE_ENV;
        let _chat_off = xai_grok_test_support::EnvGuard::unset(GROK_CHAT_MODE_ENV);
        let _desktop_off =
            xai_grok_test_support::EnvGuard::unset("GROK_SESSION_LIST_CONVERSATIONS");
        assert!(
            !conversations_lane_active(),
            "no env ⇒ lane off (Build-mode default)"
        );
        {
            let _desktop =
                xai_grok_test_support::EnvGuard::set("GROK_SESSION_LIST_CONVERSATIONS", "1");
            assert_eq!(conversations_lane_active(), false);
        }
        {
            let _chat = xai_grok_test_support::EnvGuard::set(GROK_CHAT_MODE_ENV, "1");
            assert_eq!(
                conversations_lane_active(),
                false,
                "process chat mode must enable the lane (chat feature only)"
            );
        }
    }
    /// `parse_list_req` forces the conversations-only `kind` exactly when process chat mode is on; otherwise the client request is untouched.
    #[test]
    #[serial_test::serial]
    fn parse_list_req_forces_kind_under_process_chat_mode_only() {
        use crate::agent::chat_modes::GROK_CHAT_MODE_ENV;
        let raw = serde_json::json!({
            "_meta": { "x.ai/facetFilters": { "kind": ["build"], "starred": [true] } },
        })
        .to_string();
        {
            let _off = xai_grok_test_support::EnvGuard::unset(GROK_CHAT_MODE_ENV);
            let req = parse_list_req(&raw).expect("parse");
            let parsed = ParsedMeta::parse(req.meta.as_ref());
            assert_eq!(
                parsed.facet_filters.get(KIND_FACET_KEY),
                Some(&vec![serde_json::json!("build")]),
                "non-chat: client kind filter untouched"
            );
        }
        {
            let _on = xai_grok_test_support::EnvGuard::set(GROK_CHAT_MODE_ENV, "1");
            let req = parse_list_req(&raw).expect("parse");
            let parsed = ParsedMeta::parse(req.meta.as_ref());
            let expected_build = if cfg!(feature = "local-workspace") {
                Some(&vec![serde_json::json!("build")])
            } else {
                Some(&vec![serde_json::json!("build")])
            };
            assert_eq!(
                parsed.facet_filters.get(KIND_FACET_KEY),
                expected_build,
                "client kind=build under process chat mode"
            );
            assert_eq!(
                parsed.facet_filters.get("starred"),
                Some(&vec![serde_json::json!(true)]),
                "other facets pass through"
            );
            let req = parse_list_req("{}").expect("parse");
            let parsed = ParsedMeta::parse(req.meta.as_ref());
            let expected = None;
            assert_eq!(
                parsed.facet_filters.get(KIND_FACET_KEY),
                expected,
                "absent client kind still forces chat under process chat mode"
            );
            for bad in [
                serde_json::json!({ "_meta": { "x.ai/facetFilters": { "kind": [] } } }),
                serde_json::json!({ "_meta": { "x.ai/facetFilters": { "kind": null } } }),
                serde_json::json!({ "_meta": { "x.ai/facetFilters": { "kind": ["other"] } } }),
            ] {
                let req = parse_list_req(&bad.to_string()).expect("parse");
                let parsed = ParsedMeta::parse(req.meta.as_ref());
                assert_eq!(
                    parsed.facet_filters.get(KIND_FACET_KEY),
                    expected,
                    "empty/null/unknown kind must still force chat: {bad}"
                );
            }
        }
    }
    /// Wire pin for the cross-crate `x.ai/partial` envelope the pager parses: the serialized reason strings must not drift.
    /// The pager maps unknown reasons to a generic retry notice, masking a rename.
    #[test]
    fn ext_list_response_serializes_partial_reasons() {
        for (reason, wire) in [
            (PartialReason::NoOauth, "no_oauth"),
            (PartialReason::Timeout, "timeout"),
            (PartialReason::Error, "error"),
        ] {
            let value = serde_json::to_value(ext_list_response(UnifiedListResult {
                rows: Vec::new(),
                next_cursor: None,
                facets: facet_registry().summarize_window(&[]),
                conversations_partial: Some(reason),
                scope: ListScope::Cwd,
            }))
            .expect("serialize");
            assert_eq!(
                value.get("_meta").and_then(|m| m.get("x.ai/partial")),
                Some(&serde_json::json!({ "conversations": true, "reason": wire }))
            );
        }
        let healthy = serde_json::to_value(ext_list_response(UnifiedListResult {
            rows: Vec::new(),
            next_cursor: None,
            facets: facet_registry().summarize_window(&[]),
            conversations_partial: None,
            scope: ListScope::Cwd,
        }))
        .expect("serialize");
        assert_eq!(
            healthy.get("_meta").and_then(|m| m.get("x.ai/partial")),
            Some(&serde_json::json!({ "conversations": false }))
        );
    }
    /// Receive-side wire pin: a field rename would silently drop the pager's `allowRelax`.
    #[test]
    fn list_req_deserializes_allow_relax_key() {
        let req: ListReq = serde_json::from_str(r#"{"allowRelax": true}"#).expect("parse");
        assert_eq!(req.cwd_scope, CwdScope::RelaxIfEmpty);
        let req: ListReq = serde_json::from_str("{}").expect("parse");
        assert_eq!(req.cwd_scope, CwdScope::WithSiblings);
    }
    #[test]
    fn list_req_deserializes_headless_key() {
        let req: ListReq = serde_json::from_str(r#"{"headless": "only"}"#).expect("parse");
        assert_eq!(
            HeadlessPolicy::from_wire(req.headless.as_deref()),
            HeadlessPolicy::Only
        );
        let req: ListReq = serde_json::from_str("{}").expect("parse");
        assert_eq!(
            HeadlessPolicy::from_wire(req.headless.as_deref()),
            HeadlessPolicy::Include
        );
    }
    #[test]
    fn relax_rows_scopes_to_repo_and_requires_messages() {
        use crate::session::persistence::Summary;
        let this_repo = "git@github.com:example/app.git";
        let repo_url = xai_grok_workspace::session::git::normalize_repo_url(this_repo).unwrap();
        let summary = |id: &str, remote: Option<&str>, num_messages: usize| {
            let mut s = Summary::new(
                &crate::session::info::Info {
                    id: agent_client_protocol::SessionId::new(id),
                    cwd: format!("/elsewhere/{id}"),
                },
                agent_client_protocol::ModelId::new("m"),
            )
            .expect("summary");
            s.num_messages = num_messages;
            s.git_remotes = remote.map(|r| vec![r.to_string()]).unwrap_or_default();
            s
        };
        let relax = || RelaxInputs {
            remote: Vec::new(),
            repo_urls: vec![repo_url.clone()],
            cwd_rows_dropped_by_policy: false,
        };
        let rows = relax_rows(
            relax(),
            vec![
                summary("mine", Some(this_repo), 4),
                summary("theirs", Some("git@github.com:xai-org/other.git"), 9),
            ],
            30,
            facet_registry(),
            HeadlessPolicy::Exclude,
        )
        .expect("relaxes onto the same-repo messaged session");
        let ids: Vec<_> = rows.iter().map(|r| r.legacy.session_id.clone()).collect();
        assert_eq!(ids, ["mine"], "only the same-repo session survives");
        assert!(
            relax_rows(
                relax(),
                vec![summary("husk", Some(this_repo), 0)],
                30,
                facet_registry(),
                HeadlessPolicy::Exclude,
            )
            .is_none(),
            "placeholder-only scan keeps the scoped view"
        );
    }
    #[test]
    fn relaxed_scan_drops_headless_remote_twin() {
        use crate::agent::session_registry_client::SessionRecord;
        use crate::session::persistence::Summary;
        let headless_local = || {
            let mut s = Summary::new(
                &crate::session::info::Info {
                    id: agent_client_protocol::SessionId::new("h1"),
                    cwd: "/elsewhere/h1".into(),
                },
                agent_client_protocol::ModelId::new("m"),
            )
            .expect("summary");
            s.num_messages = 6;
            s.session_kind = Some("headless".into());
            s
        };
        let remote_twin = || SessionRecord {
            session_id: "h1".into(),
            summary: "one-shot".into(),
            first_prompt: None,
            model_id: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-03-01T00:00:00Z".into(),
            last_turn_number: 6,
            restorable_turn_number: None,
            cwd: "/elsewhere/h1".into(),
            repo_remote_url: None,
            hostname: Some("devbox".into()),
            status: "active".into(),
            gcs_trace_prefix: "traces/".into(),
            gcs_bucket: "bucket".into(),
            last_active_at: None,
        };
        let relax = || RelaxInputs {
            remote: vec![remote_twin()],
            repo_urls: Vec::new(),
            cwd_rows_dropped_by_policy: false,
        };
        assert!(
            relax_rows(
                relax(),
                vec![headless_local()],
                30,
                facet_registry(),
                HeadlessPolicy::Exclude,
            )
            .is_none(),
            "the headless session must not leak back as its kind-less remote twin"
        );
        let rows = relax_rows(
            relax(),
            vec![headless_local()],
            30,
            facet_registry(),
            HeadlessPolicy::Include,
        )
        .expect("Include keeps the pair, proving the fixture would leak");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.first().map(|r| r.legacy.session_id.as_str()),
            Some("h1")
        );
    }
    #[tokio::test]
    async fn policy_emptied_cwd_lane_does_not_relax() {
        let (rows, scope) = maybe_relax(
            Vec::new(),
            Some(RelaxInputs {
                remote: Vec::new(),
                repo_urls: Vec::new(),
                cwd_rows_dropped_by_policy: true,
            }),
            30,
            facet_registry(),
            HeadlessPolicy::Exclude,
        )
        .await;
        assert!(rows.is_empty(), "the filtered page stays empty");
        assert_eq!(scope, ListScope::Cwd, "scope must not relax");
    }
    #[test]
    fn policy_relax_gate_only_blocks_emptied_lanes() {
        assert!(policy_emptied_cwd_lane(true, &[]));
        assert!(!policy_emptied_cwd_lane(false, &[]));
        let husk = to_rows(
            crate::session::merge::merge(
                Vec::new(),
                vec![{
                    let mut summary = crate::session::persistence::Summary::new(
                        &crate::session::info::Info {
                            id: agent_client_protocol::SessionId::new("husk"),
                            cwd: "/repo".into(),
                        },
                        agent_client_protocol::ModelId::new("m"),
                    )
                    .expect("summary");
                    summary.num_messages = 0;
                    // Named empty: merge keeps it. An unnamed husk is the
                    // optimistic-home shape and is dropped from the list.
                    summary.session_summary = "husk".into();
                    summary
                }],
                None,
                &[],
                30,
            ),
            facet_registry(),
        );
        assert!(!husk.is_empty());
        assert!(!policy_emptied_cwd_lane(true, &husk));
        assert!(!policy_emptied_cwd_lane(false, &husk));
    }
    /// Send-side wire pin: `x.ai/listScope` present iff the scope relaxed.
    #[test]
    fn ext_list_response_serializes_scope() {
        let result = |scope| UnifiedListResult {
            rows: Vec::new(),
            next_cursor: None,
            facets: facet_registry().summarize_window(&[]),
            conversations_partial: None,
            scope,
        };
        let with =
            serde_json::to_value(ext_list_response(result(ListScope::Repo))).expect("serialize");
        assert_eq!(
            with.get("_meta").and_then(|m| m.get("x.ai/listScope")),
            Some(&serde_json::json!("repo"))
        );
        let without =
            serde_json::to_value(ext_list_response(result(ListScope::Cwd))).expect("serialize");
        assert!(
            without
                .get("_meta")
                .and_then(|m| m.get("x.ai/listScope"))
                .is_none()
        );
    }
}
