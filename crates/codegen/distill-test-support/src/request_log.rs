//! Every route records an entry; the count stays exact after a long test stops retaining entries.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use axum::http::HeaderMap;
use serde_json::Value;

use crate::conversation::ConversationId;
use crate::failure::ObservedFailure;
use crate::inference_request::{
    InferenceRequest, first_system_message, offered_tools, tool_results,
};

pub(crate) fn authorization_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(String::from)
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    /// The request's place in arrival order over every route, counted from 1, which names the entry
    /// for [`RequestLog::note_failure`] after older entries are evicted.
    sequence: u64,
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
    pub authorization: Option<String>,
    /// Lowercase names in arrival order. Empty for the GET endpoints.
    pub headers: Vec<(String, String)>,
    /// The latency harness builds request timelines from this.
    pub at: std::time::SystemTime,
    /// Foreground inference requests only.
    pub conversation: Option<usize>,
    /// What the mock decided to do to an inference request on its own; `None` for a plain answer or a
    /// response the test enqueued. Noted before any hold so a stall still shows it.
    pub observed_failure: Option<ObservedFailure>,
}

impl LogEntry {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn first_system_prompt(&self) -> Option<String> {
        self.body.as_ref().and_then(first_system_message)
    }

    pub fn offered_tool_names(&self) -> Vec<String> {
        self.body
            .as_ref()
            .map(|body| offered_tools(body).names())
            .unwrap_or_default()
    }

    pub fn tool_results(&self) -> BTreeMap<String, String> {
        self.body.as_ref().map(tool_results).unwrap_or_default()
    }
}

/// An entry holds a whole conversation, so the log evicts oldest first.
const MAX_LOGGED_REQUESTS: usize = 1024;

pub(crate) struct RequestLog {
    count: AtomicU32,
    entries: std::sync::Mutex<Vec<LogEntry>>,
    keep_entries: AtomicBool,
}

impl RequestLog {
    pub(crate) fn new() -> Self {
        RequestLog {
            count: AtomicU32::new(0),
            entries: std::sync::Mutex::new(Vec::new()),
            keep_entries: AtomicBool::new(true),
        }
    }

    pub(crate) fn record_get(&self, path: &str) {
        let sequence = self.next_sequence();
        self.keep(LogEntry {
            sequence,
            method: "GET".to_owned(),
            path: path.to_owned(),
            body: None,
            authorization: None,
            headers: Vec::new(),
            at: std::time::SystemTime::now(),
            conversation: None,
            observed_failure: None,
        });
    }

    pub(crate) fn record(&self, method: &str, path: &str, body: &Value, headers: &HeaderMap) {
        let sequence = self.next_sequence();
        self.keep(RequestLog::entry(sequence, method, path, body, headers));
    }

    /// Returns the sequence that names the entry for [`Self::note_failure`].
    pub(crate) fn record_inference(&self, request: &InferenceRequest<'_>) -> u64 {
        let sequence = self.next_sequence();
        self.keep(LogEntry {
            conversation: request.conversation().map(ConversationId::number),
            ..RequestLog::entry(
                sequence,
                "POST",
                request.endpoint().path(),
                request.body(),
                request.headers(),
            )
        });
        sequence
    }

    pub(crate) fn note_failure(&self, sequence: u64, failure: ObservedFailure) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.sequence == sequence) {
            entry.observed_failure = Some(failure);
        }
    }

    fn entry(
        sequence: u64,
        method: &str,
        path: &str,
        body: &Value,
        headers: &HeaderMap,
    ) -> LogEntry {
        LogEntry {
            sequence,
            method: method.to_owned(),
            path: path.to_owned(),
            body: Some(body.clone()),
            authorization: authorization_header(headers),
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            at: std::time::SystemTime::now(),
            conversation: None,
            observed_failure: None,
        }
    }

    /// Count the arrival; the count stays exact when entries are not kept.
    fn next_sequence(&self) -> u64 {
        u64::from(self.count.fetch_add(1, Ordering::SeqCst)) + 1
    }

    fn keep(&self, entry: LogEntry) {
        if !self.keep_entries.load(Ordering::SeqCst) {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_LOGGED_REQUESTS {
            entries.remove(0);
        }
        entries.push(entry);
    }

    pub(crate) fn count(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }

    pub(crate) fn set_keep_entries(&self, enabled: bool) {
        self.keep_entries.store(enabled, Ordering::SeqCst);
    }

    pub(crate) fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub(crate) fn count_for(&self, path: &str) -> usize {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.path == path)
            .count()
    }

    pub(crate) fn has_path_containing(&self, fragment: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.path.contains(fragment))
    }

    pub(crate) fn bodies(&self) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.body.clone())
            .collect()
    }

    pub(crate) fn summary(&self) -> String {
        let entries = self.entries.lock().unwrap();
        if entries.is_empty() {
            return "(no requests received)".to_owned();
        }
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| format!("  [{index}] {} {}", entry.method, entry.path))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn last_system_prompt(&self) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|entry| {
                entry.path.contains("chat/completions") || entry.path.contains("responses")
            })
            .and_then(LogEntry::first_system_prompt)
    }
}
