// Modified for Distill by Samuel Fajreldines, 2026.
//! Client-side error taxonomy.
//!
//! Wire-level [`distill_tool_protocol::ToolErrorWire`] variants and JSON-RPC
//! error envelopes are mapped into the smaller [`ClientError`] vocabulary
//! at the SDK boundary so consumers can match on a single enum without
//! re-deriving the numeric/string code mapping.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use distill_tool_protocol::{IdError, JsonRpcError, ToolCallId, ToolErrorWire};

/// The most of an unknown code that is kept as spelled (the same cap as the IdP body excerpt in
/// the daemon's `cause`): the 403 body is a stranger's bytes, and the code reaches the user's
/// message, the daemon's last log line and its stop marker, each of which is one line.
pub const MAX_REFUSAL_CODE_LEN: usize = 500;

/// The policy the hub named when it refused an upgrade: the `code` of its 403 body. A code this
/// build does not know is kept as it was spelled — collapsed to one line and cut at
/// [`MAX_REFUSAL_CODE_LEN`] — so a newer hub's policy still reaches the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    LocalAccessDisabled,
    XaiInternalGate,
    MissingScope,
    #[serde(untagged)]
    Unknown(String),
}

impl<'de> Deserialize<'de> for RefusalCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

impl RefusalCode {
    fn parse(code: &str) -> Self {
        match code {
            "local_access_disabled" => Self::LocalAccessDisabled,
            "xai_internal_gate" => Self::XaiInternalGate,
            "missing_scope" => Self::MissingScope,
            unknown => Self::Unknown(bounded_one_line(unknown)),
        }
    }

    /// The code as the hub spelled it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::LocalAccessDisabled => "local_access_disabled",
            Self::XaiInternalGate => "xai_internal_gate",
            Self::MissingScope => "missing_scope",
            Self::Unknown(code) => code,
        }
    }
}

impl std::fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whitespace runs (newlines included) become one space; past [`MAX_REFUSAL_CODE_LEN`] characters
/// the rest is an ellipsis.
fn bounded_one_line(code: &str) -> String {
    let one_line = code.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_REFUSAL_CODE_LEN {
        return one_line;
    }
    one_line
        .chars()
        .take(MAX_REFUSAL_CODE_LEN)
        .chain(['…'])
        .collect()
}

#[derive(Deserialize)]
struct RefusalBody {
    code: RefusalCode,
}

/// Errors surfaced by the client SDK.
#[derive(Debug, Error)]
pub enum ClientError {
    /// WebSocket transport failure: failed to connect, dropped socket,
    /// or in-flight request interrupted by a reconnect cycle.
    #[error("network error: {0}")]
    NetworkError(String),

    /// Wire-protocol violation: malformed JSON, unexpected method,
    /// hello/hello_ack mismatch, or unsupported `protocol_version`.
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// Authentication or authorisation rejected by the server.
    #[error("auth error: {0}")]
    AuthError(String),

    /// Server rejected the WebSocket upgrade with an HTTP auth status
    /// (401/403). Non-retryable: replaying the same credential is
    /// rejected identically, so the reconnect loop classifies this as
    /// fatal instead of retrying forever. `refusal` is the policy a 403
    /// body names, when it names one.
    #[error("handshake auth failed: HTTP {status}")]
    HandshakeAuthFailed {
        status: u16,
        refusal: Option<RefusalCode>,
    },

    /// `register_tool` / `register_session` ack reported a conflict
    /// (cross-connection contention or an already-bound entry the
    /// caller did not expect).
    #[error("registration conflict: {0}")]
    RegistrationConflict(String),

    /// Outbound mpsc full or call-site bounded wait elapsed before the
    /// frame could be enqueued. Distinct from [`Self::NetworkError`]:
    /// the socket may still be healthy.
    #[error("backpressure: {0}")]
    BackpressureError(String),

    /// JSON serialise / deserialise failure inside the SDK.
    #[error("serde error: {0}")]
    Serde(String),

    /// Builder consistency error: missing URL, missing auth, etc.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// Wrapped wire-format tool error; surfaces the upstream
    /// [`ToolErrorWire`] variant verbatim for callers that need to
    /// switch on the stable string code.
    #[error(transparent)]
    Wire(ToolErrorWire),

    /// Server-side close / shutdown signal received during steady state.
    #[error("server closed connection: {0}")]
    Closed(String),

    /// Refused to send credentials over an insecure `ws://` scheme to a
    /// non-loopback host. Local-loopback (`127.0.0.1`, `::1`,
    /// `localhost`) is the only exception; every other host MUST be
    /// reached over `wss://` so the bearer token never crosses the
    /// network in plaintext.
    #[error(
        "insecure scheme: refusing to send credentials over plaintext ws:// to non-loopback host {url}"
    )]
    InsecureScheme { url: Url },

    /// Caller passed a `ToolCallId` that already keys an in-flight
    /// dispatch on the same connection. The prior call's progress
    /// waiter and response correlation are left intact; this error
    /// surfaces synchronously so the second caller can retry with a
    /// fresh id. Mint a fresh [`ToolCallId::new_v7`] (or use
    /// [`distill_tool_runtime::ToolCallContext::default`], which does so)
    /// per call. This is client misuse, not a transport or server
    /// failure.
    #[error("call_id {call_id} already in flight on this connection")]
    CallIdInUse { call_id: ToolCallId },
}

impl ClientError {
    /// Map a JSON-RPC envelope error into a [`ClientError`]. The
    /// envelope's `data` payload (when present) carries the stable
    /// [`ToolErrorWire`] discriminator; the numeric `code` is used as a
    /// coarse fallback when `data` is absent or undecodable.
    pub fn from_jsonrpc_error(err: JsonRpcError) -> Self {
        if let Some(data) = err.data
            && let Ok(wire) = serde_json::from_value::<ToolErrorWire>(data)
        {
            return Self::from_wire(wire);
        }
        match err.code {
            -32002 | -32003 => Self::AuthError(err.message),
            -32004 => Self::NetworkError(err.message),
            -32600..=-32500 => Self::ProtocolError(err.message),
            _ => Self::Wire(ToolErrorWire::Custom {
                subcode: format!("jsonrpc_{}", err.code),
                message: err.message,
                details: None,
            }),
        }
    }

    /// `true` when a `data`-less envelope collapsed to the given `jsonrpc_<code>`
    /// subcode (see [`Self::from_jsonrpc_error`]); shared by the bind recognizers.
    fn has_collapsed_jsonrpc_subcode(&self, subcode: &str) -> bool {
        matches!(
            self,
            Self::Wire(ToolErrorWire::Custom { subcode: s, .. }) if s == subcode
        )
    }

    /// `true` for the server's "server not found" bind rejection (JSON-RPC `-32601`):
    /// no workspace-server is registered for this user.
    pub fn is_server_not_found(&self) -> bool {
        self.has_collapsed_jsonrpc_subcode("jsonrpc_-32601")
    }

    /// `true` for the server's `-32013` "server found but bind did not complete" error
    /// (the `ServerBindOutcome::Unavailable` cases). Recognized so the harness
    /// re-provisions this recoverable case, distinct from [`Self::is_server_not_found`].
    pub fn is_tool_unavailable(&self) -> bool {
        self.has_collapsed_jsonrpc_subcode("jsonrpc_-32013")
    }

    /// The server's `-32099` `rate_limited` rejection: it did not act on the
    /// request, so retrying after a backoff is duplicate-safe.
    pub fn is_rate_limited(&self) -> bool {
        self.has_collapsed_jsonrpc_subcode("jsonrpc_-32099")
    }

    /// Map a [`ToolErrorWire`] variant into the SDK error taxonomy.
    pub fn from_wire(wire: ToolErrorWire) -> Self {
        match wire {
            ToolErrorWire::PermissionDenied { reason } => Self::AuthError(reason),
            ToolErrorWire::TransportClosed { tool_id } => {
                Self::NetworkError(format!("transport closed for {tool_id}"))
            }
            ToolErrorWire::UnsupportedProtocolVersion { supported } => {
                Self::ProtocolError(format!("unsupported protocol; supported: {supported:?}"))
            }
            other => Self::Wire(other),
        }
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serde(err.to_string())
    }
}

impl From<IdError> for ClientError {
    fn from(err: IdError) -> Self {
        Self::ProtocolError(err.to_string())
    }
}

impl From<url::ParseError> for ClientError {
    fn from(err: url::ParseError) -> Self {
        Self::InvalidConfig(format!("invalid url: {err}"))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::NetworkError(err.to_string())
    }
}

impl ClientError {
    /// Classify a failed WebSocket upgrade. A `401`/`403` on the HTTP
    /// upgrade is a non-retryable auth rejection
    /// ([`Self::HandshakeAuthFailed`]); every other failure stays a
    /// transport [`Self::NetworkError`] via the blanket `From` impl. The
    /// distinction must be made here, before `From` collapses the typed
    /// `Http` response (status and body) into an opaque string.
    pub(crate) fn from_handshake_error(err: tokio_tungstenite::tungstenite::Error) -> Self {
        if let tokio_tungstenite::tungstenite::Error::Http(resp) = &err {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let refusal = resp
                    .body()
                    .as_deref()
                    .and_then(|body| serde_json::from_slice::<RefusalBody>(body).ok())
                    .map(|body| body.code);
                return Self::HandshakeAuthFailed { status, refusal };
            }
        }
        Self::from(err)
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for ClientError {
    fn from(_: tokio::sync::oneshot::error::RecvError) -> Self {
        Self::NetworkError("response waiter dropped (connection closed)".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use distill_tool_protocol::{
        WORKSPACE_UNAVAILABLE_SUBCODE, WorkspaceGonePhase, WorkspaceGoneReason,
        workspace_unavailable_wire,
    };

    use super::*;

    fn workspace_gone_envelope() -> JsonRpcError {
        let wire = workspace_unavailable_wire(
            WorkspaceGoneReason::Disconnect,
            WorkspaceGonePhase::RouteMissing,
        );
        JsonRpcError {
            code: -32005,
            message: "workspace server gone".to_owned(),
            data: Some(serde_json::to_value(&wire).unwrap()),
        }
    }

    fn http_upgrade_error(status: u16) -> tokio_tungstenite::tungstenite::Error {
        http_upgrade_error_with_body(status, None)
    }

    fn http_upgrade_error_with_body(
        status: u16,
        body: Option<&str>,
    ) -> tokio_tungstenite::tungstenite::Error {
        let resp = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(status)
            .body(body.map(|body| body.as_bytes().to_vec()))
            .expect("response builds");
        tokio_tungstenite::tungstenite::Error::Http(Box::new(resp))
    }

    #[test]
    fn handshake_401_and_403_map_to_handshake_auth_failed() {
        for status in [401u16, 403] {
            match ClientError::from_handshake_error(http_upgrade_error(status)) {
                ClientError::HandshakeAuthFailed {
                    status: got,
                    refusal: None,
                } => assert_eq!(got, status),
                other => panic!("expected HandshakeAuthFailed for {status}; got {other:?}"),
            }
        }
    }

    /// The hub names the policy behind a 403 as `{"code": ...}`; an older hub's text body, or a
    /// code this SDK does not know, is a refusal with no code.
    #[test]
    fn handshake_403_carries_the_refusal_code_the_body_names() {
        let refusal = |body: Option<&str>| match ClientError::from_handshake_error(
            http_upgrade_error_with_body(403, body),
        ) {
            ClientError::HandshakeAuthFailed { refusal, .. } => refusal,
            other => panic!("expected HandshakeAuthFailed; got {other:?}"),
        };
        assert_eq!(
            refusal(Some(
                r#"{"code":"local_access_disabled","reason":"tool servers need the serve scope"}"#
            )),
            Some(RefusalCode::LocalAccessDisabled)
        );
        assert_eq!(
            refusal(Some(r#"{"code":"xai_internal_gate"}"#)),
            Some(RefusalCode::XaiInternalGate)
        );
        assert_eq!(
            refusal(Some(r#"{"code":"missing_scope"}"#)),
            Some(RefusalCode::MissingScope)
        );
        assert_eq!(refusal(Some("forbidden")), None);
        assert_eq!(
            refusal(Some(r#"{"code":"from_the_future"}"#)),
            Some(RefusalCode::Unknown("from_the_future".to_owned())),
            "a code this build does not know is still the hub's word"
        );
        assert_eq!(
            serde_json::to_string(&RefusalCode::Unknown("from_the_future".to_owned())).unwrap(),
            r#""from_the_future""#
        );
        assert_eq!(refusal(None), None);
        assert_eq!(
            RefusalCode::LocalAccessDisabled.to_string(),
            "local_access_disabled"
        );
    }

    /// The 403 body is a stranger's bytes, and the code reaches one-line places: the user's
    /// message, the daemon's last log line, its stop marker. An oversized, multi-line code is kept
    /// as one line of at most `MAX_REFUSAL_CODE_LEN` characters and an ellipsis.
    #[test]
    fn an_unknown_refusal_code_is_bounded_and_one_line() {
        let oversized = format!(
            "line one\n\tline two\r\n{}",
            "x".repeat(MAX_REFUSAL_CODE_LEN * 2)
        );
        let body = serde_json::to_string(&json!({ "code": oversized })).unwrap();
        let ClientError::HandshakeAuthFailed {
            refusal: Some(RefusalCode::Unknown(code)),
            ..
        } = ClientError::from_handshake_error(http_upgrade_error_with_body(403, Some(&body)))
        else {
            panic!("expected an unknown refusal code");
        };
        assert!(code.starts_with("line one line two xxx"), "{code}");
        assert!(!code.contains(['\n', '\r', '\t']), "{code}");
        assert_eq!(code.chars().count(), MAX_REFUSAL_CODE_LEN + 1);
        assert!(code.ends_with('…'), "{code}");
        assert_eq!(code.lines().count(), 1);

        // Within the bound, the code is the hub's word to the character; a known code with
        // stray whitespace around it is still unknown (the hub did not spell it).
        let short = ClientError::from_handshake_error(http_upgrade_error_with_body(
            403,
            Some(r#"{"code":"a_b-c.d"}"#),
        ));
        assert!(matches!(
            short,
            ClientError::HandshakeAuthFailed { refusal: Some(RefusalCode::Unknown(ref code)), .. } if code == "a_b-c.d"
        ));
    }

    #[test]
    fn handshake_non_auth_status_stays_network_error() {
        for status in [500u16, 502, 429] {
            match ClientError::from_handshake_error(http_upgrade_error(status)) {
                ClientError::NetworkError(_) => {}
                other => panic!("expected NetworkError for {status}; got {other:?}"),
            }
        }
    }

    #[test]
    fn from_jsonrpc_error_preserves_workspace_subcode_and_details() {
        // The `data` payload decodes as `ToolErrorWire` first, so the stable
        // subcode and structured details reach the SDK consumer intact rather
        // than collapsing to the numeric code.
        match ClientError::from_jsonrpc_error(workspace_gone_envelope()) {
            ClientError::Wire(ToolErrorWire::Custom {
                subcode, details, ..
            }) => {
                assert_eq!(subcode, WORKSPACE_UNAVAILABLE_SUBCODE);
                let details = details.expect("details present");
                assert_eq!(details["code"], json!(WORKSPACE_UNAVAILABLE_SUBCODE));
                assert_eq!(details["reason"], json!("disconnect"));
                assert_eq!(details["phase"], json!("route_missing"));
                assert_eq!(details["retryable"], json!(true));
            }
            other => panic!("expected Wire(Custom), got {other:?}"),
        }
    }

    #[test]
    fn is_server_not_found_recognizes_bare_minus_32601() {
        // data-less -32601 -> custom subcode.
        let err = ClientError::from_jsonrpc_error(JsonRpcError {
            code: -32601,
            message: "server abc not found for user".to_owned(),
            data: None,
        });
        assert!(err.is_server_not_found());
    }

    #[test]
    fn is_tool_unavailable_recognizes_bare_minus_32013() {
        let err = ClientError::from_jsonrpc_error(JsonRpcError {
            code: -32013,
            message: "server abc did not complete the bind".to_owned(),
            data: None,
        });
        assert!(err.is_tool_unavailable());
    }

    #[test]
    fn is_server_not_found_rejects_other_errors() {
        let auth = ClientError::from_jsonrpc_error(JsonRpcError {
            code: -32002,
            message: "nope".to_owned(),
            data: None,
        });
        assert!(!auth.is_server_not_found());
        // workspace-gone is the tool-call re-provision path, not bind ServerNotFound.
        assert!(!ClientError::from_jsonrpc_error(workspace_gone_envelope()).is_server_not_found());
    }

    #[test]
    fn bind_recognizers_are_mutually_exclusive() {
        let not_found = ClientError::from_jsonrpc_error(JsonRpcError {
            code: -32601,
            message: "not found".to_owned(),
            data: None,
        });
        let unavailable = ClientError::from_jsonrpc_error(JsonRpcError {
            code: -32013,
            message: "unavailable".to_owned(),
            data: None,
        });
        assert!(not_found.is_server_not_found());
        assert!(
            !not_found.is_tool_unavailable(),
            "-32601 must not be recognized as tool_unavailable"
        );
        assert!(unavailable.is_tool_unavailable());
        assert!(
            !unavailable.is_server_not_found(),
            "-32013 must not be recognized as server_not_found"
        );
    }

    #[test]
    fn sdk_reexported_recognizer_matches_decoded_error() {
        // SDK-only consumers reach the recognizer through the SDK re-export and
        // the core decode path.
        let err = distill_computer_hub_core::error_from_envelope(workspace_gone_envelope());
        assert!(crate::is_workspace_unavailable(&err));
    }

    #[test]
    fn sdk_reexported_recognizer_rejects_unrelated_custom_error() {
        let wire = ToolErrorWire::Custom {
            subcode: "unrelated".to_owned(),
            message: "nope".to_owned(),
            details: Some(json!({ "code": "unrelated" })),
        };
        let env = JsonRpcError {
            code: -32000,
            message: "nope".to_owned(),
            data: Some(serde_json::to_value(&wire).unwrap()),
        };
        let err = distill_computer_hub_core::error_from_envelope(env);
        assert!(!crate::is_workspace_unavailable(&err));
    }
}
