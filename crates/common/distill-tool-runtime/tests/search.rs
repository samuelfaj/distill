// Modified for Distill by Samuel Fajreldines, 2026.
//! Backend-agnostic search interface — basic shape coverage.

use std::sync::Arc;

use distill_tool_runtime::{SearchSnapshot, ServerSummary, ToolSearchIndex, ToolSearchResult};

struct StubIndex {
    summaries: Vec<ServerSummary>,
}

impl ToolSearchIndex for StubIndex {
    fn search_snapshot(&self, query: &str, limit: usize) -> SearchSnapshot {
        let results: Vec<_> = self
            .summaries
            .iter()
            .flat_map(|s| s.tool_names.iter().map(move |t| (s, t)))
            .filter(|(_, t)| t.contains(query))
            .take(limit)
            .map(|(s, t)| ToolSearchResult {
                tool_name: t.clone(),
                server_name: s.name.clone(),
                description: format!("{} from {}", t, s.name),
                score: 1.0,
                parameters: Vec::new(),
                input_schema: serde_json::json!({}),
            })
            .collect();
        let returned = results.len();
        SearchSnapshot {
            results,
            total_hidden_tools: self
                .summaries
                .iter()
                .map(|s| s.tool_count())
                .sum::<usize>()
                .saturating_sub(returned),
            is_ready: true,
        }
    }

    fn list_server_summaries(&self) -> Vec<ServerSummary> {
        self.summaries.clone()
    }
}

#[test]
fn search_index_object_safe_via_arc() {
    let index = StubIndex {
        summaries: vec![ServerSummary {
            name: "linear".into(),
            description: None,
            tool_names: vec!["save_issue".into(), "list_issues".into()],
        }],
    };
    let dyn_index: Arc<dyn ToolSearchIndex> = Arc::new(index);
    let snap = dyn_index.search_snapshot("save", 10);
    assert_eq!(snap.results.len(), 1);
    assert_eq!(snap.results[0].tool_name, "save_issue");
    assert_eq!(snap.total_hidden_tools, 1);
    assert!(snap.is_ready);

    let summaries = dyn_index.list_server_summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].tool_count(), 2);
}
