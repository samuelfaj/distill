//! Live `user_message_chunk` during a prompt is gated by `x.ai/userMessageEcho`.
//!
//! ACP v1 documents `user_message_chunk` as the `session/load` replay shape, not as an
//! echo of `session/prompt`. The agent always persists the chunk so replay stays
//! spec-complete. Absent the capability, live broadcast is off: an external ACP client
//! never asked for an echo of `session/prompt`. An explicit `true` opts in.

/// Advertised in `initialize`'s `clientCapabilities._meta`.
/// Absent means persist-only. `true` is live echo.
pub const USER_MESSAGE_ECHO_CAPABILITY: &str = "x.ai/userMessageEcho";

/// Per-session spelling injected by a leader into `session/new`, `session/load`, and
/// `session/resume` `_meta`. A leader multiplexes clients, so the answer travels with
/// the session, not the process.
pub const CLIENT_USER_MESSAGE_ECHO_META: &str = "clientUserMessageEcho";
