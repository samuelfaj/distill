//! Rows the leader adds to the roster without hosting them in `MvpAgent`.
//!
//! The worker door publishes its claims into an [`ExternalRoster`];
//! [`RosterListMerge`] sits on the `run_leader` agent boundary, remembers the
//! id of every `x.ai/sessions/list` request that passes inbound (IPC and relay
//! alike), and appends the roster's rows to the matching response before the
//! fan-out. [`run_changed_notifier`] turns each roster generation into an
//! `x.ai/sessions/changed` broadcast on the same fan-out. Pending ids are
//! compared as JSON values because IPC ids are namespaced strings while relay
//! ids are whatever grok.com sent, and they expire after
//! [`PENDING_LIST_TTL`] so an unanswered request cannot leak. Compiled in
//! every build: without the worker door the roster is simply empty and
//! every line passes through untouched.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::warn;

use crate::agent::roster::{RosterChanged, RosterEntry, SESSIONS_CHANGED_METHOD};
use crate::leader::server::method_of;

/// A list request unanswered for this long is forgotten; the TTL is the only bound on the table.
pub(crate) const PENDING_LIST_TTL: Duration = Duration::from_secs(60);

struct RosterInner {
    rows: Mutex<HashMap<u64, Vec<RosterEntry>>>,
    next_publisher: AtomicU64,
    generation: watch::Sender<u64>,
}

/// Cloneable handle to the externally owned roster rows.
#[derive(Clone)]
pub(crate) struct ExternalRoster {
    inner: Arc<RosterInner>,
}

impl ExternalRoster {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(RosterInner {
                rows: Mutex::new(HashMap::new()),
                next_publisher: AtomicU64::new(1),
                generation: watch::channel(0).0,
            }),
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the cursor worker is the only publisher")
    )]
    pub(crate) fn publisher(&self) -> ExternalRosterPublisher {
        let id = self.inner.next_publisher.fetch_add(1, Ordering::Relaxed);
        ExternalRosterPublisher {
            id,
            roster: self.clone(),
        }
    }

    /// Drops every partition. `close` calls this after both doors are stopped, so a
    /// publisher that never ran still cannot leave a row behind.
    pub(crate) fn clear(&self) {
        let mut partitions = self.inner.rows.lock();
        if partitions.is_empty() {
            return;
        }
        partitions.clear();
        drop(partitions);
        self.inner
            .generation
            .send_modify(|generation| *generation += 1);
    }

    /// Replaces the default partition and bumps the generation; tests and single publishers use it.
    #[cfg(test)]
    pub(crate) fn replace(&self, rows: Vec<RosterEntry>) {
        self.replace_partition(0, rows);
    }

    fn replace_partition(&self, id: u64, rows: Vec<RosterEntry>) {
        let mut partitions = self.inner.rows.lock();
        if rows.is_empty() {
            partitions.remove(&id);
        } else {
            partitions.insert(id, rows);
        }
        drop(partitions);
        self.inner.generation.send_modify(|g| *g += 1);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.inner.rows.lock().values().all(Vec::is_empty)
    }

    pub(crate) fn rows(&self) -> Vec<RosterEntry> {
        self.inner.rows.lock().values().flatten().cloned().collect()
    }

    /// Generation counter, bumped by every [`Self::replace`].
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.inner.generation.subscribe()
    }
}

impl std::fmt::Debug for ExternalRoster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalRoster")
            .field("rows", &self.rows().len())
            .finish()
    }
}

pub(crate) struct ExternalRosterPublisher {
    id: u64,
    roster: ExternalRoster,
}

impl ExternalRosterPublisher {
    pub(crate) fn replace(&self, rows: Vec<RosterEntry>) {
        self.roster.replace_partition(self.id, rows);
    }
}

impl Drop for ExternalRosterPublisher {
    fn drop(&mut self) {
        self.roster.replace_partition(self.id, Vec::new());
    }
}

/// Appends [`ExternalRoster`] rows to `x.ai/sessions/list` responses. Pending ids are a
/// linear scan: a dashboard polls the list a few times a second at most.
pub(crate) struct RosterListMerge {
    roster: ExternalRoster,
    pending: Mutex<Vec<(Value, Instant)>>,
}

impl RosterListMerge {
    pub(crate) fn new(roster: ExternalRoster) -> Self {
        Self {
            roster,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Records the `id` of an inbound `x.ai/sessions/list` request; every other line is ignored.
    pub(crate) fn observe_inbound(&self, line: &str) {
        // Every client request passes here (prompts with attachments included); only a list
        // request is worth parsing, and only while there are rows to merge into its answer
        // (the common no-door configuration then never parses a roster line).
        if self.roster.is_empty() || !line.contains("sessions/list") {
            return;
        }
        let Ok(json) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if method_of(&json) != Some(crate::agent::roster::SESSIONS_LIST_METHOD) {
            return;
        }
        let Some(id) = json.get("id").filter(|id| !id.is_null()) else {
            return;
        };
        let now = Instant::now();
        let mut pending = self.pending.lock();
        pending.retain(|(_, seen)| now.duration_since(*seen) < PENDING_LIST_TTL);
        pending.push((id.clone(), now));
    }

    /// Extends the `sessions` array of a response to a pending list request with the roster's
    /// rows. Everything else, including an errored response or an empty roster, is returned
    /// unchanged (and unparsed where the cheap checks allow).
    pub(crate) fn filter_outbound<'a>(&self, line: &'a str) -> Cow<'a, str> {
        if self.roster.is_empty() {
            // Ids recorded while the door was live would otherwise wait for a reused JSON-RPC id
            // after a restart; with no rows there is nothing they could merge into.
            self.pending.lock().clear();
            return Cow::Borrowed(line);
        }
        let Some(mut json) = self.take_pending_response(line) else {
            return Cow::Borrowed(line);
        };
        let rows = self.roster.rows();
        if rows.is_empty() {
            return Cow::Borrowed(line);
        }
        let Some(sessions) = list_sessions_mut(&mut json) else {
            return Cow::Borrowed(line);
        };
        sessions.extend(
            rows.iter()
                .filter_map(|row| match serde_json::to_value(row) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        warn!(error = %error, "cursor worker roster row failed to serialize");
                        None
                    }
                }),
        );
        match serde_json::to_string(&json) {
            Ok(merged) => Cow::Owned(merged),
            Err(error) => {
                warn!(error = %error, "merged sessions/list response failed to serialize");
                Cow::Borrowed(line)
            }
        }
    }

    /// The parsed response when `line` answers a pending list request, which is then forgotten.
    fn take_pending_response(&self, line: &str) -> Option<Value> {
        {
            let mut pending = self.pending.lock();
            if pending.is_empty() {
                return None;
            }
            let now = Instant::now();
            pending.retain(|(_, seen)| now.duration_since(*seen) < PENDING_LIST_TTL);
            if pending.is_empty() {
                return None;
            }
        }
        // A response always carries an `id` key; this keeps streamed notifications (the bulk of
        // the fan-out) off the parser while a list is in flight.
        if !line.contains("\"id\"") {
            return None;
        }
        let json: Value = serde_json::from_str(line).ok()?;
        if json.get("method").is_some() {
            return None;
        }
        let id = json.get("id")?;
        let mut pending = self.pending.lock();
        let position = pending
            .iter()
            .position(|(pending_id, _)| pending_id == id)?;
        pending.remove(position);
        Some(json)
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }
}

/// `result.result.sessions` (the `ExtMethodResult` envelope) or the bare `result.sessions`;
/// `None` for an errored response or a body of neither shape.
fn list_sessions_mut(json: &mut Value) -> Option<&mut Vec<Value>> {
    let result = json.get_mut("result")?.as_object_mut()?;
    if result.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }
    let body = if matches!(result.get("result"), Some(Value::Object(_))) {
        result.get_mut("result")?.as_object_mut()?
    } else {
        result
    };
    body.get_mut("sessions")?.as_array_mut()
}

/// The `x.ai/sessions/changed` line for a roster change, shaped like the agent's own
/// broadcast (`_`-prefixed method, bare params).
pub(crate) fn changed_notification(upserted: Vec<RosterEntry>, removed: Vec<String>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": format!("_{SESSIONS_CHANGED_METHOD}"),
        "params": RosterChanged { upserted, removed },
    })
    .to_string()
}

/// Writes one `x.ai/sessions/changed` line into `sink` per roster generation that changed a
/// row, carrying only the rows that differ from the last emission plus the removals. Runs for
/// the leader's lifetime: the owning task is dropped with the `LocalSet`.
pub(crate) async fn run_changed_notifier(roster: ExternalRoster, mut sink: impl FnMut(String)) {
    let mut changes = roster.changes();
    // A generation published between spawn and first poll must not be skipped.
    changes.mark_changed();
    let mut known: HashMap<String, RosterEntry> = HashMap::new();
    while changes.changed().await.is_ok() {
        let rows = roster.rows();
        let removed: Vec<String> = known
            .keys()
            .filter(|id| !rows.iter().any(|row| &row.session_id == *id))
            .cloned()
            .collect();
        let upserted: Vec<RosterEntry> = rows
            .iter()
            .filter(|row| known.get(&row.session_id) != Some(*row))
            .cloned()
            .collect();
        known = rows
            .into_iter()
            .map(|row| (row.session_id.clone(), row))
            .collect();
        if upserted.is_empty() && removed.is_empty() {
            continue;
        }
        sink(changed_notification(upserted, removed));
    }
}

#[cfg(test)]
#[path = "roster_merge_tests.rs"]
mod tests;
