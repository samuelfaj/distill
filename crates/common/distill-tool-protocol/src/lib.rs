// Modified for Distill by Samuel Fajreldines, 2026.
//! Grok Computer Hub — wire-protocol types.
//!
//! Identifier newtypes, registration payloads, capabilities, hook events,
//! handshake messages, the JSON-RPC 2.0 envelope and method catalog, the
//! `ToolErrorWire` / `ToolOutputWire` / `WireToolNotification` wire enums,
//! every method's `params` / `result` payload struct, the numeric ↔
//! string error-code mapping, and the bot-relay frame types.

#![forbid(unsafe_code)]

pub mod bot_relay;
mod capabilities;
mod connection;
pub mod envelope;
pub mod error_codes;
pub mod error_wire;
pub mod frames;
mod handshake;
mod hook;
mod ids;
pub mod methods;
pub mod notification_wire;
pub mod output_wire;
mod registration;
mod registry_error;
pub mod session_event;
pub mod turn_hook;

pub use bot_relay::{
    BOT_EVENT_ENVELOPE_V, BOT_RELAY_CAPABILITIES, BotBindConversationParams,
    BotBindConversationResult, BotCommandParams, BotCommandResult, BotEmptyParams, BotEmptyResult,
    BotEventChannel, BotEventEnvelope, BotRelayError, BotRelayErrorCode, BotRelayErrorDetail,
    BotRelaySiblingAccount, BotRelaySignIn, BotRosterEntry, BotRosterParams, BotRosterResult,
    BotRunState, BotStatusParams, BotStatusResult, BotSubscribeParams, BotSubscribeResult,
    BotTranscriptEntryStamp, BotTranscriptOffboxParams, BotTranscriptOffboxResult,
    BotUnsubscribeParams, BotUnsubscribeResult, BotUsageParams, BotUsageResult,
    BotVncDescriptorParams, BotVncDescriptorResult, COMMAND_REJECTED_AGENT_ID_MISMATCH,
    COMMAND_REJECTED_ARGS_INVALID, COMMAND_REJECTED_ARGS_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE, COMMAND_REJECTED_ATTACHMENT_NOT_FOUND,
    COMMAND_REJECTED_ATTACHMENT_NOT_READY, COMMAND_REJECTED_ATTACHMENT_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE, COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE,
    COMMAND_REJECTED_AUDIENCE_UNSUPPORTED, COMMAND_REJECTED_BOX_REFUSED,
    COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD, COMMAND_REJECTED_HARNESS_REFUSED,
    COMMAND_REJECTED_NOT_SUPPORTED_IN_LIVE, COMMAND_REJECTED_NOT_YET_ENABLED,
    COMMAND_REJECTED_REASONS, COMMAND_REJECTED_TEMPORAL_UNSUPPORTED, HubChannel,
    HubResyncRequiredEvent, HubTurnFinishedEvent, HubUnknownChannel, UpstreamChannel,
    is_gateway_method_unsupported,
};
pub use capabilities::{HookKind, NotificationSchemas, StreamingSpec, ToolCapabilities, ToolScope};
pub use connection::{ConnectionKind, ToolDefinitionMode};
pub use envelope::{
    JsonRpcError, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion,
    ResponseOutcome,
};
pub use error_codes::{
    ERROR_CODES, WORKSPACE_UNAVAILABLE_JSONRPC_CODE, WORKSPACE_UNAVAILABLE_MESSAGE,
    WORKSPACE_UNAVAILABLE_SUBCODE, WorkspaceGonePhase, WorkspaceGoneReason,
    WorkspaceUnavailableDetails, workspace_unavailable_wire,
};
pub use error_wire::ToolErrorWire;
pub use frames::{
    AttachRoute, HOST_KIND_DAEMON, HOST_KIND_DESKTOP, HOST_KIND_SANDBOX,
    HUB_KIND_BOT_AGENT_TURN_COMPLETED, HookFrame, HookReplyFrame, IMAGE_CAPABILITIES_V1,
    IdleWithholdReason, LastSeq, LogsDonateParams, MAX_DONATION_BYTES, MAX_IMAGE_CAPABILITIES,
    MAX_IMAGE_CAPABILITY_LEN, MAX_LOG_RECORDS_PER_DONATION, MAX_METRICS_PER_DONATION,
    MAX_SPANS_PER_DONATION, MAX_SYSTEM_NOTIFY_PAYLOAD_BYTES, MetricsDonateParams,
    NotificationFilter, PingFrame, PongFrame, SESSION_BIND_ACK_TIMEOUT, ServeParams, ServeResult,
    ServerBindAck, ServerBindOutcome, ServerBindParams, ServerIdentityMetadata, ServerInfo,
    ServerUnbindAck, ServerUnbindOutcome, ServerUnbindParams, ServersListParams, ServersListResult,
    SessionAttachServerParams, SessionAttachServerResult, SessionBindParams, SessionBindResult,
    SessionBindServerParams, SessionBindServerResult, SessionCloseParams, SessionDetachParams,
    SessionOpenParams, SessionOpenResult, SessionUnbindParams, SessionUnbindServerParams,
    SubscribeAck, SubscribeNotificationsParams, SubscribeOutcome, SystemNotifyParams,
    ToolCallParams, ToolCallProgressFrame, ToolCallResult, ToolNotificationFrame, ToolSearchResult,
    ToolServerConnectionStatus, ToolServerDisconnectReason, ToolServerEvictParams,
    ToolServerGetStatusParams, ToolServerGetStatusResult, ToolServerLifecycleStatus,
    ToolServerStatusPayload, ToolsChanged, ToolsListParams, ToolsListResult, ToolsSearchParams,
    ToolsSearchResultBody, TracesDonateParams, UnsubscribeAck, UnsubscribeNotificationsParams,
    UnsubscribeOutcome, is_image_capability_token,
};
pub use handshake::{HelloAckMsg, HelloMsg, PROTOCOL_VERSION};
pub use hook::HookEvent;
pub use ids::{
    ConnectionId, FrameSeq, HUB_RESERVED_SESSION_PREFIX, IdError, RequestId, ServerId, SessionId,
    ToolCallId, ToolId, UserId,
};
pub use methods::{Method, UNKNOWN_METHOD_MSG_PREFIX};
pub use notification_wire::{
    KNOWN_NOTIFICATION_KINDS, KnownVariantCollision, WireCustomNotification, WireToolNotification,
    check_custom_kind, known_notification_kinds,
};
pub use output_wire::{McpBlock, ToolOutputWire};
pub use registration::{
    RegistrationOutcome, ToolDescriptionWithSchema, ToolRegistration, ToolServerRegistration,
    TransportKind,
};
pub use registry_error::RegistryError;
pub use session_event::{SessionEvent, SessionPhase, ToolCallOutcome};
