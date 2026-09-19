// Modified for Distill by Samuel Fajreldines, 2026.
//! [`Metadata`] is a string-keyed map intended to mirror gRPC metadata 1:1 when the call goes over the wire.
//! It carries per-call context like session ids, trace context, and deadlines.
//!
//! `Extensions` (the typed in-process map) is in-process only and not serialized, so it is not defined here.
//! It lives in the runtime `distill-workspace` crate alongside the transport implementations.
//!
//! [grpc-spec]: https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Standard metadata key for the active session id.
pub const META_SESSION_ID: &str = "x-workspace-session-id";

/// Standard metadata key for the W3C trace parent (`traceparent`).
pub const META_TRACEPARENT: &str = "traceparent";

/// Standard metadata key for the W3C trace state (`tracestate`).
pub const META_TRACESTATE: &str = "tracestate";

/// Standard metadata key for the originating client identity.
pub const META_CLIENT_ID: &str = "x-workspace-client-id";

/// Standard metadata key for the prompt index of the current turn.
pub const META_PROMPT_INDEX: &str = "x-workspace-prompt-index";

/// Standard metadata key for the gRPC call deadline.
/// The `grpc-timeout` header is a unit-suffixed string per the gRPC HTTP/2 spec, not a bare millisecond count.
pub const META_GRPC_TIMEOUT: &str = "grpc-timeout";

/// All standard metadata keys defined by this crate, in declaration order.
///
/// Useful for tests that need to assert uniqueness or for callers that want to scrub well-known keys from a metadata map.
pub const STANDARD_META_KEYS: &[&str] = &[
    META_SESSION_ID,
    META_TRACEPARENT,
    META_TRACESTATE,
    META_CLIENT_ID,
    META_PROMPT_INDEX,
    META_GRPC_TIMEOUT,
];

/// String-keyed metadata headers.
///
/// Backed by a `BTreeMap` so serialization order is deterministic, which matters for snapshot tests and for stable wire-bytes hashing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Metadata(pub BTreeMap<String, String>);

impl Metadata {
    /// Create an empty metadata map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a header. Returns the previous value if any.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.0.insert(key.into(), value.into())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Remove a header. Returns the removed value if any.
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }

    /// Iterate over the map's `(key, value)` pairs in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Iterate over the map's keys in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Iterate over the map's values in key order.
    pub fn values(&self) -> impl Iterator<Item = &str> {
        self.0.values().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, V> FromIterator<(K, V)> for Metadata
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

/// Allows `for (k, v) in &metadata { ... }` without going through [`Metadata::iter`] explicitly.
impl<'a> IntoIterator for &'a Metadata {
    type Item = (&'a str, &'a str);
    type IntoIter = std::iter::Map<
        std::collections::btree_map::Iter<'a, String, String>,
        fn((&'a String, &'a String)) -> (&'a str, &'a str),
    >;

    fn into_iter(self) -> Self::IntoIter {
        // Using a `fn` pointer (rather than a closure) keeps the associated type nameable
        self.0
            .iter()
            .map(|(k, v): (&'a String, &'a String)| -> (&'a str, &'a str) {
                (k.as_str(), v.as_str())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn standard_meta_keys_are_unique() {
        let set: HashSet<&&str> = STANDARD_META_KEYS.iter().collect();
        assert_eq!(
            set.len(),
            STANDARD_META_KEYS.len(),
            "duplicate standard metadata key"
        );
    }

    #[test]
    fn metadata_iteration_is_sorted() {
        let mut m = Metadata::new();
        m.insert("z", "1");
        m.insert("a", "2");
        m.insert("m", "3");
        let keys: Vec<&str> = m.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["a", "m", "z"]);
    }

    #[test]
    fn metadata_supports_for_loop_via_into_iterator() {
        let mut m = Metadata::new();
        m.insert("a", "1");
        m.insert("b", "2");
        let mut collected = Vec::new();
        for (k, v) in &m {
            collected.push((k.to_owned(), v.to_owned()));
        }
        assert_eq!(
            collected,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }
}
