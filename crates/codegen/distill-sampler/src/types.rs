use std::fmt;

use serde::{Deserialize, Serialize};

/// Wraps a `String` so callers can pass an externally-assigned ID (e.g., a session-assigned UUID) or generate a fresh one via [`RequestId::random`].
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestId(String);

impl RequestId {
    /// Generate a fresh random request ID backed by a UUIDv4.
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_produces_unique_values() {
        let a = RequestId::random();
        let b = RequestId::random();
        assert_ne!(a, b, "two random IDs must differ");
        // UUIDv4 strings are 36 characters (8-4-4-4-12 hex with hyphens).
        assert_eq!(a.as_str().len(), 36);
    }
}
