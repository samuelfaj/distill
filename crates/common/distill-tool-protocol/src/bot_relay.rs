// Modified for Distill by Samuel Fajreldines, 2026.
//! Bot-relay wire types: JSON-RPC method payloads, the closed hub error
//! enum, and the `bot.event` envelope.
//!
//! These types are the client-facing stability boundary. Gateway command
//! names and payloads pass through verbatim; this module does not type them.

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use typeshare::typeshare;

use crate::{JsonRpcError, Method};

/// Wire method strings a `hello_ack` `capabilities` list advertises for
/// the bot-relay surface.
pub const BOT_RELAY_CAPABILITIES: &[&str] = &[
    Method::BotCommand.as_wire_str(),
    Method::BotVncDescriptor.as_wire_str(),
    Method::BotRoster.as_wire_str(),
    Method::BotStatus.as_wire_str(),
    Method::BotTranscriptOffbox.as_wire_str(),
    Method::BotUsage.as_wire_str(),
    Method::BotSubscribe.as_wire_str(),
    Method::BotUnsubscribe.as_wire_str(),
    Method::BotBindConversation.as_wire_str(),
    Method::BotEvent.as_wire_str(),
];

/// `bot.event` envelope version carried in [`BotEventEnvelope::v`].
pub const BOT_EVENT_ENVELOPE_V: u32 = 1;

/// `reason` on `command_rejected` when the command is compiled in but not allowlisted.
pub const COMMAND_REJECTED_NOT_YET_ENABLED: &str = "not_yet_enabled";

/// `reason` on `command_rejected` when envelope `agentId` and `args.agentId` disagree.
pub const COMMAND_REJECTED_AGENT_ID_MISMATCH: &str = "agent_id_mismatch";

/// `reason` on `command_rejected` when `uploadAttachment` args JSON exceeds 3 MiB.
pub const COMMAND_REJECTED_ARGS_TOO_LARGE: &str = "args_too_large";

/// `reason` on `command_rejected` when required command args are missing or empty.
pub const COMMAND_REJECTED_ARGS_INVALID: &str = "args_invalid";

/// `reason` on `command_rejected` when the session's audience (owner vs. viewer) may not act on that agent.
pub const COMMAND_REJECTED_AUDIENCE_UNSUPPORTED: &str = "audience_unsupported";

/// `reason` on `command_rejected` when Live mode cannot accept attachments.
pub const COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE: &str =
    "attachments_not_supported_in_live";

/// `reason` on `command_rejected` when Live mode cannot interrupt or look up prompt acceptance.
pub const COMMAND_REJECTED_NOT_SUPPORTED_IN_LIVE: &str = "not_supported_in_live";

/// `reason` on `command_rejected` when attachUpload cannot fetch the file because this connection has no usable credential.
pub const COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE: &str =
    "attachment_credential_unavailable";

/// `reason` on `command_rejected` when the owning harness refused the send.
/// Nothing was accepted and the same message may be sent again. The upstream
/// `failureCode` is logged rather than surfaced, because this list is a closed
/// client contract.
pub const COMMAND_REJECTED_HARNESS_REFUSED: &str = "harness_refused";

/// `reason` on `command_rejected` when attachUpload cannot see the file (missing or not the caller's).
pub const COMMAND_REJECTED_ATTACHMENT_NOT_FOUND: &str = "attachment_not_found";

/// `reason` on `command_rejected` when the file exists but is not a BOT_CHAT upload.
pub const COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE: &str = "attachment_wrong_source";

/// `reason` on `command_rejected` when the stored BotChat object exceeds 25 MiB.
pub const COMMAND_REJECTED_ATTACHMENT_TOO_LARGE: &str = "attachment_too_large";

/// `reason` on `command_rejected` when the BotChat upload is not PostProcessDone.
pub const COMMAND_REJECTED_ATTACHMENT_NOT_READY: &str = "attachment_not_ready";

/// `reason` on `command_rejected` when the live box gateway refused a well-formed command with its own sentence.
/// The refusal is an HTTP 4xx carrying a JSON `error` body, or a 5xx whose `error`
/// names a missing or malformed agent id: `detail.upstream_message` carries that
/// sentence, and a `failureCode` lands in `detail.upstream` as
/// `code=<failureCode>`. Nothing was accepted.
pub const COMMAND_REJECTED_BOX_REFUSED: &str = "box_refused";

/// `reason` on `command_rejected` when the box refused a well-formed catalog method (capability skew, not a client catalog bug).
pub const COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD: &str = "gateway/unknown-method";

/// `reason` on `command_rejected` when the target agent's harness owns this state and exposes no RPC for the operation.
/// The box never held the state, so a box answer would be empty or a ghost.
pub const COMMAND_REJECTED_TEMPORAL_UNSUPPORTED: &str = "temporal_unsupported";

/// Every `command_rejected` reason above, sorted. Codegen fails if this
/// disagrees with the `COMMAND_REJECTED_*` consts, and the hub checks its
/// metrics label set against it, so a new reason cannot land uncounted.
pub const COMMAND_REJECTED_REASONS: &[&str] = &[
    COMMAND_REJECTED_AGENT_ID_MISMATCH,
    COMMAND_REJECTED_ARGS_INVALID,
    COMMAND_REJECTED_ARGS_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE,
    COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE,
    COMMAND_REJECTED_ATTACHMENT_NOT_FOUND,
    COMMAND_REJECTED_ATTACHMENT_NOT_READY,
    COMMAND_REJECTED_ATTACHMENT_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE,
    COMMAND_REJECTED_AUDIENCE_UNSUPPORTED,
    COMMAND_REJECTED_BOX_REFUSED,
    COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD,
    COMMAND_REJECTED_HARNESS_REFUSED,
    COMMAND_REJECTED_NOT_SUPPORTED_IN_LIVE,
    COMMAND_REJECTED_NOT_YET_ENABLED,
    COMMAND_REJECTED_TEMPORAL_UNSUPPORTED,
];

/// True only when the hub classified a box unknown-method refusal.
/// False for `unknown_method` (catalog-miss), `not_yet_enabled`,
/// `host_only`, and every non-`command_rejected` code.
pub fn is_gateway_method_unsupported(err: &BotRelayError) -> bool {
    err.code == BotRelayErrorCode::CommandRejected
        && err.reason.as_deref() == Some(COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD)
}

// ── Shared empty payloads ────────────────────────────────────────────────

/// Empty JSON object (`{}`). Used by verbs whose design params/result are `{}`.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotEmptyParams {}

/// Empty JSON object (`{}`). Alias of [`BotEmptyParams`].
#[typeshare]
pub type BotEmptyResult = BotEmptyParams;

// ── bot.command ──────────────────────────────────────────────────────────

/// `bot.command` params. `name` and `args` are upstream-verbatim.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotCommandParams {
    pub agent_id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Upstream-verbatim command result. Not interpreted by this crate.
#[typeshare]
pub type BotCommandResult = serde_json::Value;

// ── bot.vncDescriptor ────────────────────────────────────────────────────

/// `bot.vncDescriptor` params.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotVncDescriptorParams {
    pub agent_id: String,
}

/// `bot.vncDescriptor` result.
///
/// `expires_hint` is unix milliseconds. `null` means a legacy network-token
/// URL (valid until pod migration); a concrete value is the port-token
/// expiry the client should refresh before.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotVncDescriptorResult {
    pub vnc_url: String,
    /// Unix time in milliseconds, or `null` when the URL has no expiry.
    /// Present as `null` on the wire when unset; omitted keys also read as `None`.
    #[serde(default)]
    #[typeshare(serialized_as = "Option<NullableMillis>")]
    pub expires_hint: Option<i64>,
}

// ── bot.roster ───────────────────────────────────────────────────────────

/// `bot.roster` params. A live read from the box that may wake a
/// hibernated box. The hub bounds the wait and, while the box is coming up,
/// answers either a retryable `box_unavailable` (`box_waking` /
/// `box_hibernated` / `wake_failed`) or `box_migrating`, or, when it
/// remembers one, the last live roster with `rememberedAtMs` set and every
/// row `unknown`; an empty `agents` list is only ever a real answer from a
/// live box, never the result of a failure.
#[typeshare]
pub type BotRosterParams = BotEmptyParams;

fn default_viewer_is_owner() -> bool {
    true
}

/// One roster row, read live from the box.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRosterEntry {
    pub agent_id: String,
    pub name: String,
    /// One of `running`, `idle` or `unknown`.
    pub status: String,
    /// `false` marks a row shared with the viewer; mutations on it are
    /// rejected with `audience_unsupported`.
    #[serde(default = "default_viewer_is_owner")]
    pub viewer_is_owner: bool,
    /// Unix time in milliseconds of the agent's last turn, when the box
    /// reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<I54>")]
    pub last_turn_at: Option<i64>,
    /// Box `avatarColor`. A short palette id, never an image payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_color: Option<String>,
    /// Box `avatarShape`. A short glyph name, never an image payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_shape: Option<String>,
    /// Sidebar hide. Omitted means unknown, not false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_from_sidebar: Option<bool>,
    /// Custom-picture presence. Omitted means unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_custom_image: Option<bool>,
}

impl Default for BotRosterEntry {
    /// Mirrors the serde default so `..Default::default()` reads as an owned row.
    fn default() -> Self {
        BotRosterEntry {
            agent_id: String::new(),
            name: String::new(),
            status: String::new(),
            viewer_is_owner: true,
            last_turn_at: None,
            avatar_color: None,
            avatar_shape: None,
            hidden_from_sidebar: None,
            has_custom_image: None,
        }
    }
}

/// `bot.roster` result.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRosterResult {
    pub agents: Vec<BotRosterEntry>,
    /// Unix time in milliseconds of the live read this roster was remembered
    /// from. Present only when the box was not ready and the hub served the
    /// last live roster in its place; every row then reads `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<I54>")]
    pub remembered_at_ms: Option<i64>,
}

// ── bot.status ───────────────────────────────────────────────────────────

/// `bot.status` params. Cold — never wakes the box.
#[typeshare]
pub type BotStatusParams = BotEmptyParams;

/// Off-box run state. Senders emit only the named variants. Receivers
/// treat any unknown wire string as [`Self::Unknown`].
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRunState {
    Absent,
    Hibernated,
    Running,
    Unknown,
}

impl BotRunState {
    pub const ALL: &'static [Self] =
        &[Self::Absent, Self::Hibernated, Self::Running, Self::Unknown];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Hibernated => "hibernated",
            Self::Running => "running",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "absent" => Self::Absent,
            "hibernated" => Self::Hibernated,
            "running" => Self::Running,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for BotRunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotRunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// `bot.status` result: off-box run state.
///
/// `runState` is a string on the wire. Generated clients see `string` and
/// compare against [`BotRunState`]. Unknown values degrade to `unknown`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotStatusResult {
    #[typeshare(serialized_as = "String")]
    pub run_state: BotRunState,
}

// ── bot.transcript.offbox ────────────────────────────────────────────────

/// `bot.transcript.offbox` params. Cold — never wakes the box.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotTranscriptOffboxParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `bot.transcript.offbox` result. `entries` is the upstream page, verbatim.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotTranscriptOffboxResult {
    pub entries: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── transcript entries ───────────────────────────────────────────────────

/// Fields the hub merges into every transcript entry it decodes from the
/// durable store — `getAgentTranscriptTail` / window / thread reads and
/// `transcript` events alike — next to the box's own fields. Entries the box
/// answers directly carry none of them.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotTranscriptEntryStamp {
    /// Grows with every write to the entry, so of two copies with the same id
    /// the higher `entry_version` is the newer one. Taken from the store's
    /// per-agent write sequence, so versions are not contiguous per entry and
    /// only compare between copies of the same entry.
    #[typeshare(serialized_as = "I54")]
    pub entry_version: u64,
}

// ── bot.usage ────────────────────────────────────────────────────────────

/// `bot.usage` params. Cold — never wakes the box.
#[typeshare]
pub type BotUsageParams = BotEmptyParams;

/// `bot.usage` result: the caller's weekly Grok Bot allowance. Percent-only
/// by contract — no currency amounts cross the wire — so clients render a
/// meter, not a balance.
///
/// `usage_percent` is absent when the account has no personal meter (a
/// pooled team allowance, or a zero denominator with no grants). It is not
/// clamped: on-demand overage reads above 100.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotUsageResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_percent: Option<f64>,
    /// Unix time in milliseconds of the current period start.
    #[typeshare(serialized_as = "I54")]
    pub current_period_start_ms: i64,
    /// Unix time in milliseconds of the next reset, or of the trial expiry
    /// when `trial` is set. Absent until the user's first metered turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<I54>")]
    pub next_reset_at_ms: Option<i64>,
    pub has_available_usage: bool,
    pub has_non_zero_included_limit: bool,
    pub included_limit_zero: bool,
    /// The allowance is a live trial grant rather than a weekly bucket.
    pub trial: bool,
    pub is_team_seat: bool,
    /// `supergrok-plus` / `supergrok-heavy` when the SuperGrok tier is the
    /// population funding the meter; absent when another plan funds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding_plan: Option<String>,
    /// Server-owned meter label (e.g. `SuperGrok Heavy`, `Grok Bot Plan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_label: Option<String>,
    /// Where the caller manages on-demand usage for this meter.
    pub manage_url: String,
    pub on_demand_eligible: bool,
    pub on_demand_enabled: bool,
}

// ── bot.subscribe / bot.unsubscribe ──────────────────────────────────────

/// `bot.subscribe` / `bot.unsubscribe` params.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotSubscribeParams {
    pub agent_ids: Vec<String>,
    /// Opt in to full-fidelity SSE payloads (inline avatars). Default slim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full_fidelity: bool,
}

/// `bot.unsubscribe` params — same shape as [`BotSubscribeParams`].
#[typeshare]
pub type BotUnsubscribeParams = BotSubscribeParams;

/// `bot.subscribe` result.
#[typeshare]
pub type BotSubscribeResult = BotEmptyResult;

/// `bot.unsubscribe` result.
#[typeshare]
pub type BotUnsubscribeResult = BotEmptyResult;

// ── bot.bindConversation ─────────────────────────────────────────────────

/// `bot.bindConversation` params. Binding is an index only; the hub never
/// infers a command target from `conversationId`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotBindConversationParams {
    pub conversation_id: String,
    pub agent_ids: Vec<String>,
    pub primary: String,
}

/// `bot.bindConversation` result.
#[typeshare]
pub type BotBindConversationResult = BotEmptyResult;

// ── Closed error enum ────────────────────────────────────────────────────

/// Closed hub-owned error code. This list is the client stability boundary.
///
/// Senders emit only these codes. Receivers treat any unknown wire string
/// as `upstream_error` and keep `retryable` / `detail`.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRelayErrorCode {
    IdentityUnavailable,
    /// The Grok account has never been linked to a Cursor account. The user
    /// must run the link flow (or JIT provisions once opted in).
    LinkRequired,
    /// The link was explicitly removed (a sticky unlink on the Cursor
    /// side). Re-linking takes an explicit flow, never a silent retry.
    LinkRemoved,
    /// Linking needs the user's recorded consent before an existing Cursor
    /// account can be attached. Definitive until the consent UX runs.
    ConsentRequired,
    /// Enterprise-managed on either side (enterprise-claimed email domain,
    /// active team, or server-side enterprise policy); the flow serves
    /// self-serve accounts only. `reason` names which rule refused.
    EnterpriseUnsupported,
    /// The matched Cursor account is on legacy request-based pricing.
    LegacyPricingUnsupported,
    /// The Grok account has no verified email, so no Cursor account can be
    /// matched or created. Fixable on the Grok side.
    EmailUnverified,
    /// Linking hit a conflict that needs manual resolution: the email
    /// matches multiple accounts, or a 1:1 link rule declined the pair.
    /// `reason` distinguishes.
    LinkConflict,
    /// A link exists, but its Cursor account is gone or unusable.
    CursorAccountUnavailable,
    /// Definitive self-serve refusal this client build does not know more
    /// precisely (a reason token newer than the mapping). `reason` carries
    /// the token verbatim.
    LinkUnsupported,
    /// The linked Cursor account, or a Cursor team that seats it, is in
    /// Privacy Mode (Legacy), which refuses cloud-agent storage. Definitive
    /// until the user (or a team admin) switches to Privacy Mode. `reason`
    /// is `personal` or `team`.
    LegacyPrivacyUnsupported,
    NoPlan,
    UsageExhausted,
    BoxMigrating,
    BoxRecreating,
    BoxUnavailable,
    CommandRejected,
    ComputerUnavailable,
    UpstreamError,
}

impl BotRelayErrorCode {
    pub const ALL: &'static [Self] = &[
        Self::IdentityUnavailable,
        Self::LinkRequired,
        Self::LinkRemoved,
        Self::ConsentRequired,
        Self::EnterpriseUnsupported,
        Self::LegacyPricingUnsupported,
        Self::EmailUnverified,
        Self::LinkConflict,
        Self::CursorAccountUnavailable,
        Self::LinkUnsupported,
        Self::LegacyPrivacyUnsupported,
        Self::NoPlan,
        Self::UsageExhausted,
        Self::BoxMigrating,
        Self::BoxRecreating,
        Self::BoxUnavailable,
        Self::CommandRejected,
        Self::ComputerUnavailable,
        Self::UpstreamError,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityUnavailable => "identity_unavailable",
            Self::LinkRequired => "link_required",
            Self::LinkRemoved => "link_removed",
            Self::ConsentRequired => "consent_required",
            Self::EnterpriseUnsupported => "enterprise_unsupported",
            Self::LegacyPricingUnsupported => "legacy_pricing_unsupported",
            Self::EmailUnverified => "email_unverified",
            Self::LinkConflict => "link_conflict",
            Self::CursorAccountUnavailable => "cursor_account_unavailable",
            Self::LinkUnsupported => "link_unsupported",
            Self::LegacyPrivacyUnsupported => "legacy_privacy_unsupported",
            Self::NoPlan => "no_plan",
            Self::UsageExhausted => "usage_exhausted",
            Self::BoxMigrating => "box_migrating",
            Self::BoxRecreating => "box_recreating",
            Self::BoxUnavailable => "box_unavailable",
            Self::CommandRejected => "command_rejected",
            Self::ComputerUnavailable => "computer_unavailable",
            Self::UpstreamError => "upstream_error",
        }
    }

    /// Parse a wire code. Unknown strings become [`Self::UpstreamError`].
    pub fn from_wire(s: &str) -> Self {
        match s {
            "identity_unavailable" => Self::IdentityUnavailable,
            "link_required" => Self::LinkRequired,
            "link_removed" => Self::LinkRemoved,
            "consent_required" => Self::ConsentRequired,
            "enterprise_unsupported" => Self::EnterpriseUnsupported,
            "legacy_pricing_unsupported" => Self::LegacyPricingUnsupported,
            "email_unverified" => Self::EmailUnverified,
            "link_conflict" => Self::LinkConflict,
            "cursor_account_unavailable" => Self::CursorAccountUnavailable,
            "link_unsupported" => Self::LinkUnsupported,
            "legacy_privacy_unsupported" => Self::LegacyPrivacyUnsupported,
            "no_plan" => Self::NoPlan,
            "usage_exhausted" => Self::UsageExhausted,
            "box_migrating" => Self::BoxMigrating,
            "box_recreating" => Self::BoxRecreating,
            "box_unavailable" => Self::BoxUnavailable,
            "command_rejected" => Self::CommandRejected,
            "computer_unavailable" => Self::ComputerUnavailable,
            _ => Self::UpstreamError,
        }
    }

    /// The `(numeric, key)` JSON-RPC class from [`crate::ERROR_CODES`] for
    /// every code — the single exhaustive mapping both
    /// [`Self::jsonrpc_numeric`] and [`Self::jsonrpc_code_key`] project
    /// from, so the two companion values cannot drift and adding a variant
    /// forces an intentional classification.
    const fn jsonrpc_class(self) -> (i32, &'static str) {
        match self {
            Self::NoPlan
            | Self::LinkRequired
            | Self::LinkRemoved
            | Self::ConsentRequired
            | Self::EnterpriseUnsupported
            | Self::LegacyPricingUnsupported
            | Self::EmailUnverified
            | Self::LinkConflict
            | Self::CursorAccountUnavailable
            | Self::LinkUnsupported
            | Self::LegacyPrivacyUnsupported => (-32003, "forbidden"),
            Self::UsageExhausted => (-32099, "rate_limited"),
            Self::IdentityUnavailable
            | Self::BoxMigrating
            | Self::BoxRecreating
            | Self::BoxUnavailable
            | Self::ComputerUnavailable => (-32013, "tool_unavailable"),
            Self::CommandRejected => (-32600, "invalid_request"),
            Self::UpstreamError => (-32603, "internal_error"),
        }
    }

    /// Closest existing [`crate::ERROR_CODES`] numeric. Receivers switch on
    /// the string [`Self::as_str`] in `data`, not this companion.
    pub const fn jsonrpc_numeric(self) -> i32 {
        self.jsonrpc_class().0
    }

    /// Whether this is a definitive link-state refusal of the account
    /// link: never retryable, always carries the machine `reason` token.
    pub const fn is_link_state(self) -> bool {
        matches!(
            self,
            Self::LinkRequired
                | Self::LinkRemoved
                | Self::ConsentRequired
                | Self::EnterpriseUnsupported
                | Self::LegacyPricingUnsupported
                | Self::EmailUnverified
                | Self::LinkConflict
                | Self::CursorAccountUnavailable
                | Self::LinkUnsupported
        )
    }

    /// [`crate::ERROR_CODES`] key paired with [`Self::jsonrpc_numeric`].
    pub const fn jsonrpc_code_key(self) -> &'static str {
        self.jsonrpc_class().1
    }
}

/// The link-state subset of [`BotRelayErrorCode`] as its own type, so
/// constructors that only accept link states are infallible by shape
/// instead of guarded by asserts. Converts losslessly into the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkStateCode {
    LinkRequired,
    LinkRemoved,
    ConsentRequired,
    EnterpriseUnsupported,
    LegacyPricingUnsupported,
    EmailUnverified,
    LinkConflict,
    CursorAccountUnavailable,
    LinkUnsupported,
}

impl From<LinkStateCode> for BotRelayErrorCode {
    fn from(code: LinkStateCode) -> Self {
        match code {
            LinkStateCode::LinkRequired => Self::LinkRequired,
            LinkStateCode::LinkRemoved => Self::LinkRemoved,
            LinkStateCode::ConsentRequired => Self::ConsentRequired,
            LinkStateCode::EnterpriseUnsupported => Self::EnterpriseUnsupported,
            LinkStateCode::LegacyPricingUnsupported => Self::LegacyPricingUnsupported,
            LinkStateCode::EmailUnverified => Self::EmailUnverified,
            LinkStateCode::LinkConflict => Self::LinkConflict,
            LinkStateCode::CursorAccountUnavailable => Self::CursorAccountUnavailable,
            LinkStateCode::LinkUnsupported => Self::LinkUnsupported,
        }
    }
}

/// The reverse of the `From` above: `Err` carries the non-link-state code back.
impl TryFrom<BotRelayErrorCode> for LinkStateCode {
    type Error = BotRelayErrorCode;

    fn try_from(code: BotRelayErrorCode) -> Result<Self, Self::Error> {
        match code {
            BotRelayErrorCode::LinkRequired => Ok(Self::LinkRequired),
            BotRelayErrorCode::LinkRemoved => Ok(Self::LinkRemoved),
            BotRelayErrorCode::ConsentRequired => Ok(Self::ConsentRequired),
            BotRelayErrorCode::EnterpriseUnsupported => Ok(Self::EnterpriseUnsupported),
            BotRelayErrorCode::LegacyPricingUnsupported => Ok(Self::LegacyPricingUnsupported),
            BotRelayErrorCode::EmailUnverified => Ok(Self::EmailUnverified),
            BotRelayErrorCode::LinkConflict => Ok(Self::LinkConflict),
            BotRelayErrorCode::CursorAccountUnavailable => Ok(Self::CursorAccountUnavailable),
            BotRelayErrorCode::LinkUnsupported => Ok(Self::LinkUnsupported),
            BotRelayErrorCode::IdentityUnavailable
            | BotRelayErrorCode::LegacyPrivacyUnsupported
            | BotRelayErrorCode::NoPlan
            | BotRelayErrorCode::UsageExhausted
            | BotRelayErrorCode::BoxMigrating
            | BotRelayErrorCode::BoxRecreating
            | BotRelayErrorCode::BoxUnavailable
            | BotRelayErrorCode::CommandRejected
            | BotRelayErrorCode::ComputerUnavailable
            | BotRelayErrorCode::UpstreamError => Err(code),
        }
    }
}

impl fmt::Display for BotRelayErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotRelayErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// How one of the caller's Grok accounts signs in. Senders emit only the
/// named variants. Receivers treat any unknown wire string as
/// [`Self::Other`].
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRelaySignIn {
    X,
    Google,
    Apple,
    Password,
    Github,
    Sso,
    EmailCode,
    Other,
}

impl BotRelaySignIn {
    pub const ALL: &'static [Self] = &[
        Self::X,
        Self::Google,
        Self::Apple,
        Self::Password,
        Self::Github,
        Self::Sso,
        Self::EmailCode,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Google => "google",
            Self::Apple => "apple",
            Self::Password => "password",
            Self::Github => "github",
            Self::Sso => "sso",
            Self::EmailCode => "email_code",
            Self::Other => "other",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "x" => Self::X,
            "google" => Self::Google,
            "apple" => Self::Apple,
            "password" => Self::Password,
            "github" => Self::Github,
            "sso" => Self::Sso,
            "email_code" => Self::EmailCode,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for BotRelaySignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotRelaySignIn {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// One of the caller's other Grok accounts on the same verified email.
///
/// `signIn` is a string on the wire. Generated clients see `string` and
/// compare against [`BotRelaySignIn`]. Unknown values degrade to `other`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRelaySiblingAccount {
    #[typeshare(serialized_as = "String")]
    pub sign_in: BotRelaySignIn,
    /// X username without `@`, Google / password / email-code email, GitHub username.
    /// Absent for Apple / SSO / other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Unix time in milliseconds the account was created.
    #[typeshare(serialized_as = "I54")]
    pub created_at_ms: i64,
    /// This sibling holds a live relay session in the hub, so it is the
    /// account Cursor is linked to. At most one account in a list is `true`.
    pub linked: bool,
}

/// Cap on [`BotRelayErrorDetail::upstream_message`], in chars.
pub const UPSTREAM_MESSAGE_MAX_CHARS: usize = 240;

/// Structured context on a bot-relay error. `upstream` is an opaque
/// diagnostic present for debugging only; clients must not parse it.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRelayErrorDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The upstream's own user-facing sentence for a refusal, trimmed and
    /// capped at [`UPSTREAM_MESSAGE_MAX_CHARS`] chars. Never the raw body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_message: Option<String>,
    /// Set only on `link_conflict` with reason `jit_link_declined` when the
    /// sibling lookup succeeded and found at least one account. Ordered by
    /// `createdAtMs`. Absent when the lookup failed or found nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling_accounts: Option<Vec<BotRelaySiblingAccount>>,
}

/// Hub-owned bot-relay error object.
///
/// Wire form: `{code, retryable, detail, reason?}`.
/// `detail` is always present (empty object when unused).
/// `reason` is set for [`BotRelayErrorCode::CommandRejected`] and for the
/// link-state codes ([`BotRelayErrorCode::is_link_state`]), where it
/// carries the exchange's machine reason token for per-case client copy.
///
/// On the JSON-RPC envelope this object is `error.data`. Receivers
/// switch on `data.code`. The envelope `error.message` is the snake_case
/// [`BotRelayErrorCode`].
///
/// `code` is a string on the wire. Generated clients see `string` and
/// compare against [`BotRelayErrorCode`]. Unknown values degrade to
/// `upstream_error` while preserving `retryable` and `detail`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRelayError {
    #[typeshare(serialized_as = "String")]
    pub code: BotRelayErrorCode,
    pub retryable: bool,
    #[serde(default)]
    pub detail: BotRelayErrorDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&BotRelayError> for JsonRpcError {
    fn from(err: &BotRelayError) -> Self {
        let data = serde_json::to_value(err);
        debug_assert!(data.is_ok(), "BotRelayError must serialize");
        JsonRpcError {
            code: err.code.jsonrpc_numeric(),
            message: err.code.as_str().to_owned(),
            data: data.ok(),
        }
    }
}

impl From<BotRelayError> for JsonRpcError {
    fn from(err: BotRelayError) -> Self {
        JsonRpcError::from(&err)
    }
}

// ── Event channel ────────────────────────────────────────────────────────

/// Enumerated hub-owned event channel. Prefixed `hub:` so the set is
/// structurally disjoint from any unprefixed upstream channel name.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum HubChannel {
    #[serde(rename = "hub:turn_finished")]
    TurnFinished,
    #[serde(rename = "hub:resync_required")]
    ResyncRequired,
}

impl HubChannel {
    pub const ALL: &'static [Self] = &[Self::TurnFinished, Self::ResyncRequired];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnFinished => "hub:turn_finished",
            Self::ResyncRequired => "hub:resync_required",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "hub:turn_finished" => Some(Self::TurnFinished),
            "hub:resync_required" => Some(Self::ResyncRequired),
            _ => None,
        }
    }
}

impl fmt::Display for HubChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Future `hub:*` channel this crate does not yet name. Constructed only
/// via [`BotEventChannel::from_wire`]; the string includes the `hub:` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HubUnknownChannel(String);

impl HubUnknownChannel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Upstream-verbatim channel. Never starts with `hub:`. Constructed only
/// via [`BotEventChannel::from_wire`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamChannel(String);

impl UpstreamChannel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `bot.event` channel: an upstream-verbatim string or a hub-owned `hub:*`
/// name.
///
/// Known `hub:*` values are [`Self::Hub`]. Any other `hub:`-prefixed
/// string is [`Self::HubUnknown`]. Non-`hub:` strings are [`Self::Upstream`].
#[derive(Debug, Clone)]
pub enum BotEventChannel {
    Hub(HubChannel),
    HubUnknown(HubUnknownChannel),
    Upstream(UpstreamChannel),
}

impl BotEventChannel {
    pub fn from_wire(s: impl Into<String>) -> Self {
        let s = s.into();
        if let Some(hub) = HubChannel::from_wire(&s) {
            return Self::Hub(hub);
        }
        if s.starts_with("hub:") {
            return Self::HubUnknown(HubUnknownChannel(s));
        }
        Self::Upstream(UpstreamChannel(s))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Hub(hub) => hub.as_str(),
            Self::HubUnknown(s) => s.as_str(),
            Self::Upstream(s) => s.as_str(),
        }
    }
}

impl From<HubChannel> for BotEventChannel {
    fn from(hub: HubChannel) -> Self {
        Self::Hub(hub)
    }
}

impl PartialEq for BotEventChannel {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for BotEventChannel {}

impl Hash for BotEventChannel {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Display for BotEventChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for BotEventChannel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotEventChannel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(s))
    }
}

// ── Hub-owned event bodies ───────────────────────────────────────────────

/// Body of `hub:turn_finished` (`event` when [`HubChannel::TurnFinished`]).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubTurnFinishedEvent {
    pub agent_id: String,
    pub conversation_ids: Vec<String>,
    pub preview: String,
}

/// Body of `hub:resync_required` (`event` when [`HubChannel::ResyncRequired`]).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubResyncRequiredEvent {
    pub agent_id: String,
}

// ── bot.event envelope ───────────────────────────────────────────────────

/// `bot.event` envelope v1 (`v == `[`BOT_EVENT_ENVELOPE_V`]).
///
/// `seq` is a per-(connection, agent) monotonic counter starting at 1 on
/// each `bot.subscribe`. It is an ordering reference, not a dedupe key
/// (a redelivered event gets a fresh `seq`) and is never comparable
/// across connections. Resync is signaled by [`HubChannel::ResyncRequired`]
/// or a reconnect, never inferred from `seq`.
///
/// [`Self::event_id`] is reserved for content-identity dedupe and is
/// omitted from the wire when `None`.
///
/// `event` is upstream-verbatim for [`BotEventChannel::Upstream`]. For
/// [`HubChannel::TurnFinished`] it is [`HubTurnFinishedEvent`]; for
/// [`HubChannel::ResyncRequired`] it is [`HubResyncRequiredEvent`].
///
/// `channel` is a string on the wire. Typeshare cannot express the
/// hub / hub-unknown / upstream split, so generated clients see `string`.
/// Compare against [`HubChannel`] for the known `hub:*` values.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotEventEnvelope {
    pub v: u32,
    pub agent_id: String,
    // typeshare panics on bare 64-bit ints; `I54` is a JS-safe number.
    #[typeshare(serialized_as = "I54")]
    pub seq: u64,
    #[typeshare(serialized_as = "String")]
    pub channel: BotEventChannel,
    pub event: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl BotEventEnvelope {
    pub fn new(
        agent_id: impl Into<String>,
        seq: u64,
        channel: impl Into<BotEventChannel>,
        event: serde_json::Value,
    ) -> Self {
        Self {
            v: BOT_EVENT_ENVELOPE_V,
            agent_id: agent_id.into(),
            seq,
            channel: channel.into(),
            event,
            event_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_codes;
    use serde_json::{Value, json};

    fn roundtrip<T>(value: &T) -> Value
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).expect("serialize");
        let parsed: T = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(&parsed, value, "round-trip mismatch");
        json
    }

    fn assert_rejects<T>(value: Value)
    where
        T: serde::de::DeserializeOwned,
    {
        assert!(
            serde_json::from_value::<T>(value.clone()).is_err(),
            "expected deserialize error for {value}"
        );
    }

    #[test]
    fn capabilities_list_is_the_bot_verb_set() {
        let expected = [
            "bot.command",
            "bot.vncDescriptor",
            "bot.roster",
            "bot.status",
            "bot.transcript.offbox",
            "bot.usage",
            "bot.subscribe",
            "bot.unsubscribe",
            "bot.bindConversation",
            "bot.event",
        ];
        assert_eq!(BOT_RELAY_CAPABILITIES, expected);
        for wire in expected {
            assert!(
                Method::from_wire_str(wire).is_some(),
                "capability {wire} must be a Method"
            );
        }
        assert!(!expected.contains(&"bot.ensureBox"));
        assert!(Method::from_wire_str("bot.ensureBox").is_none());
    }

    #[test]
    fn command_params_match_design_frame() {
        let params = BotCommandParams {
            agent_id: "agt_...".to_owned(),
            name: "sendPrompt".to_owned(),
            args: json!({"prompt": "hello"}),
        };
        let wire = json!({
            "agentId": "agt_...",
            "name": "sendPrompt",
            "args": {"prompt": "hello"},
        });
        assert_eq!(roundtrip(&params), wire);
        let parsed: BotCommandParams = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, params);
        assert_rejects::<BotCommandParams>(json!({
            "agent_id": "agt_...",
            "name": "sendPrompt",
            "args": {"prompt": "hello"},
        }));
    }

    #[test]
    fn command_args_accept_any_json_value() {
        for args in [json!(null), json!([]), json!("x")] {
            let params = BotCommandParams {
                agent_id: "agt_1".to_owned(),
                name: "noop".to_owned(),
                args: args.clone(),
            };
            assert_eq!(
                roundtrip(&params),
                json!({"agentId": "agt_1", "name": "noop", "args": args})
            );
        }
    }

    #[test]
    fn vnc_descriptor_params_require_agent_id() {
        let params = BotVncDescriptorParams {
            agent_id: "agt_...".to_owned(),
        };
        let wire = json!({"agentId": "agt_..."});
        assert_eq!(roundtrip(&params), wire);
        let parsed: BotVncDescriptorParams = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, params);
        assert_rejects::<BotVncDescriptorParams>(json!({}));
        assert_rejects::<BotVncDescriptorParams>(json!({"agent_id": "agt_..."}));
    }

    #[test]
    fn vnc_descriptor_result_null_expires_hint() {
        let result = BotVncDescriptorResult {
            vnc_url: "https://example.invalid/vnc".to_owned(),
            expires_hint: None,
        };
        let wire = json!({
            "vncUrl": "https://example.invalid/vnc",
            "expiresHint": null,
        });
        assert_eq!(roundtrip(&result), wire);
        let parsed: BotVncDescriptorResult = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, result);
        let omitted: BotVncDescriptorResult = serde_json::from_value(json!({
            "vncUrl": "https://example.invalid/vnc",
        }))
        .unwrap();
        assert_eq!(omitted.expires_hint, None);
        assert_rejects::<BotVncDescriptorResult>(json!({
            "vnc_url": "https://example.invalid/vnc",
            "expiresHint": null,
        }));
    }

    #[test]
    fn vnc_descriptor_result_with_expires_hint() {
        let result = BotVncDescriptorResult {
            vnc_url: "https://example.invalid/vnc".to_owned(),
            expires_hint: Some(1_700_000_000_000_i64),
        };
        assert_eq!(
            roundtrip(&result),
            json!({
                "vncUrl": "https://example.invalid/vnc",
                "expiresHint": 1_700_000_000_000_i64,
            })
        );
    }

    #[test]
    fn empty_params_serialize_as_object() {
        assert_eq!(roundtrip(&BotEmptyParams {}), json!({}));
        assert_eq!(roundtrip(&BotEmptyResult {}), json!({}));
    }

    #[test]
    fn roster_round_trip_camel_case() {
        let roster = BotRosterResult {
            agents: vec![BotRosterEntry {
                agent_id: "agt_1".to_owned(),
                name: "Watcher".to_owned(),
                status: "idle".to_owned(),
                last_turn_at: Some(1_700_000_123_000_i64),
                ..Default::default()
            }],
            remembered_at_ms: None,
        };
        let wire = json!({
            "agents": [{
                "agentId": "agt_1",
                "name": "Watcher",
                "status": "idle",
                "viewerIsOwner": true,
                "lastTurnAt": 1_700_000_123_000_i64,
            }],
        });
        assert_eq!(roundtrip(&roster), wire);
        let parsed: BotRosterResult = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, roster);
        assert_rejects::<BotRosterEntry>(json!({
            "agent_id": "agt_1",
            "name": "Watcher",
            "status": "idle",
        }));
    }

    #[test]
    fn remembered_roster_round_trips_remembered_at_ms() {
        let remembered = BotRosterResult {
            agents: vec![BotRosterEntry {
                agent_id: "agt_1".to_owned(),
                name: "Watcher".to_owned(),
                status: "unknown".to_owned(),
                ..Default::default()
            }],
            remembered_at_ms: Some(1_700_000_200_000_i64),
        };
        let wire = json!({
            "agents": [{
                "agentId": "agt_1",
                "name": "Watcher",
                "status": "unknown",
                "viewerIsOwner": true,
            }],
            "rememberedAtMs": 1_700_000_200_000_i64,
        });
        assert_eq!(wire, roundtrip(&remembered));
        let parsed: BotRosterResult = serde_json::from_value(wire).unwrap();
        assert_eq!(remembered, parsed);
    }

    #[test]
    fn roster_viewer_is_owner_defaults_true_and_round_trips_false() {
        let omitted: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_5",
            "name": "Mine",
            "status": "idle",
        }))
        .unwrap();
        assert!(omitted.viewer_is_owner);
        let shared = BotRosterEntry {
            agent_id: "agt_6".to_owned(),
            name: "Theirs".to_owned(),
            status: "unknown".to_owned(),
            viewer_is_owner: false,
            ..Default::default()
        };
        let wire = json!({
            "agentId": "agt_6",
            "name": "Theirs",
            "status": "unknown",
            "viewerIsOwner": false,
        });
        assert_eq!(roundtrip(&shared), wire);
        let parsed: BotRosterEntry = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, shared);
    }

    #[test]
    fn empty_roster_is_agents_empty_array() {
        assert_eq!(
            roundtrip(&BotRosterResult::default()),
            json!({"agents": []})
        );
        let parsed: BotRosterResult = serde_json::from_value(json!({"agents": []})).unwrap();
        assert!(parsed.agents.is_empty());
    }

    #[test]
    fn last_turn_at_omitted_and_null_are_none() {
        let no_turn = BotRosterEntry {
            agent_id: "agt_2".to_owned(),
            name: "New".to_owned(),
            status: "unknown".to_owned(),
            last_turn_at: None,
            ..Default::default()
        };
        assert!(
            !roundtrip(&no_turn)
                .as_object()
                .unwrap()
                .contains_key("lastTurnAt")
        );
        let omitted: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_2",
            "name": "New",
            "status": "unknown",
        }))
        .unwrap();
        assert_eq!(omitted.last_turn_at, None);
        let explicit_null: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_2",
            "name": "New",
            "status": "unknown",
            "lastTurnAt": null,
        }))
        .unwrap();
        assert_eq!(explicit_null.last_turn_at, None);
    }

    #[test]
    fn roster_avatar_marks_round_trip_and_omit_when_absent() {
        let marked = BotRosterEntry {
            agent_id: "agt_3".to_owned(),
            name: "Painter".to_owned(),
            status: "idle".to_owned(),
            last_turn_at: None,
            avatar_color: Some("red".to_owned()),
            avatar_shape: Some("hex".to_owned()),
            ..Default::default()
        };
        let wire = json!({
            "agentId": "agt_3",
            "name": "Painter",
            "status": "idle",
            "viewerIsOwner": true,
            "avatarColor": "red",
            "avatarShape": "hex",
        });
        assert_eq!(roundtrip(&marked), wire);
        let parsed: BotRosterEntry = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, marked);
        let omitted: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_3",
            "name": "Painter",
            "status": "idle",
        }))
        .unwrap();
        assert_eq!(omitted.avatar_color, None);
        assert_eq!(omitted.avatar_shape, None);
        assert!(
            !roundtrip(&omitted)
                .as_object()
                .unwrap()
                .contains_key("avatarColor")
        );
        assert!(
            !roundtrip(&omitted)
                .as_object()
                .unwrap()
                .contains_key("avatarShape")
        );
    }

    #[test]
    fn roster_sidebar_facts_round_trip_and_omit_when_absent() {
        let facts = BotRosterEntry {
            agent_id: "agt_4".to_owned(),
            name: "Hidden".to_owned(),
            status: "idle".to_owned(),
            hidden_from_sidebar: Some(true),
            has_custom_image: Some(true),
            ..Default::default()
        };
        let wire = json!({
            "agentId": "agt_4",
            "name": "Hidden",
            "status": "idle",
            "viewerIsOwner": true,
            "hiddenFromSidebar": true,
            "hasCustomImage": true,
        });
        assert_eq!(roundtrip(&facts), wire);
        let parsed: BotRosterEntry = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, facts);
        let omitted: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_4",
            "name": "Hidden",
            "status": "idle",
        }))
        .unwrap();
        assert_eq!(omitted.hidden_from_sidebar, None);
        assert_eq!(omitted.has_custom_image, None);
        let object = roundtrip(&omitted).as_object().unwrap().clone();
        assert!(!object.contains_key("hiddenFromSidebar"));
        assert!(!object.contains_key("hasCustomImage"));
    }

    #[test]
    fn status_result_round_trip() {
        let status = BotStatusResult {
            run_state: BotRunState::Hibernated,
        };
        assert_eq!(roundtrip(&status), json!({"runState": "hibernated"}));
        let parsed: BotStatusResult =
            serde_json::from_value(json!({"runState": "hibernated"})).unwrap();
        assert_eq!(parsed, status);
        assert_rejects::<BotStatusResult>(json!({"run_state": "hibernated"}));
    }

    #[test]
    fn run_state_closed_send_open_receive() {
        for state in BotRunState::ALL {
            assert_eq!(BotRunState::from_wire(state.as_str()), *state);
            assert_eq!(roundtrip(state), json!(state.as_str()));
            assert_eq!(state.to_string(), state.as_str());
        }
        let unknown: BotRunState =
            serde_json::from_value(json!("draining")).expect("unknown run state");
        assert_eq!(unknown, BotRunState::Unknown);
        assert_eq!(serde_json::to_value(unknown).unwrap(), json!("unknown"));
    }

    #[test]
    fn transcript_offbox_params_and_result() {
        let params = BotTranscriptOffboxParams {
            agent_id: "agt_...".to_owned(),
            cursor: Some("c_1".to_owned()),
        };
        let wire = json!({"agentId": "agt_...", "cursor": "c_1"});
        assert_eq!(roundtrip(&params), wire);
        let parsed: BotTranscriptOffboxParams = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, params);

        let first_page = BotTranscriptOffboxParams {
            agent_id: "agt_...".to_owned(),
            cursor: None,
        };
        assert_eq!(roundtrip(&first_page), json!({"agentId": "agt_..."}));

        let result = BotTranscriptOffboxResult {
            entries: json!([{"id": "e1"}]),
            next_cursor: Some("c_2".to_owned()),
        };
        assert_eq!(
            roundtrip(&result),
            json!({"entries": [{"id": "e1"}], "nextCursor": "c_2"})
        );

        let last_page = BotTranscriptOffboxResult {
            entries: json!([{"id": "e9"}]),
            next_cursor: None,
        };
        let last_wire = json!({"entries": [{"id": "e9"}]});
        assert_eq!(roundtrip(&last_page), last_wire);
        let parsed: BotTranscriptOffboxResult = serde_json::from_value(last_wire).unwrap();
        assert_eq!(parsed.next_cursor, None);

        assert_rejects::<BotTranscriptOffboxParams>(json!({
            "agent_id": "agt_...",
            "cursor": "c_1",
        }));
    }

    #[test]
    fn usage_result_omits_absent_optionals_and_keeps_overage_unclamped() {
        let metered = BotUsageResult {
            usage_percent: Some(137.5),
            current_period_start_ms: 1_756_000_000_000,
            next_reset_at_ms: Some(1_756_604_800_000),
            has_available_usage: true,
            has_non_zero_included_limit: true,
            included_limit_zero: false,
            trial: false,
            is_team_seat: false,
            funding_plan: Some("supergrok-heavy".to_owned()),
            plan_label: Some("SuperGrok Heavy".to_owned()),
            manage_url: "https://example.com/usage".to_owned(),
            on_demand_eligible: true,
            on_demand_enabled: false,
        };
        assert_eq!(
            roundtrip(&metered),
            json!({
                "usagePercent": 137.5,
                "currentPeriodStartMs": 1_756_000_000_000_i64,
                "nextResetAtMs": 1_756_604_800_000_i64,
                "hasAvailableUsage": true,
                "hasNonZeroIncludedLimit": true,
                "includedLimitZero": false,
                "trial": false,
                "isTeamSeat": false,
                "fundingPlan": "supergrok-heavy",
                "planLabel": "SuperGrok Heavy",
                "manageUrl": "https://example.com/usage",
                "onDemandEligible": true,
                "onDemandEnabled": false,
            })
        );

        let unmetered = BotUsageResult {
            usage_percent: None,
            next_reset_at_ms: None,
            funding_plan: None,
            plan_label: None,
            has_available_usage: false,
            has_non_zero_included_limit: false,
            included_limit_zero: true,
            ..metered
        };
        let wire = roundtrip(&unmetered);
        for absent in ["usagePercent", "nextResetAtMs", "fundingPlan", "planLabel"] {
            assert!(wire.get(absent).is_none(), "{absent} must be omitted");
        }
        let parsed: BotUsageResult = serde_json::from_value(wire).unwrap();
        assert_eq!(unmetered, parsed);
    }

    #[test]
    fn subscribe_and_bind() {
        let sub = BotSubscribeParams {
            agent_ids: vec!["agt_a".to_owned(), "agt_b".to_owned()],
            full_fidelity: false,
        };
        assert_eq!(roundtrip(&sub), json!({"agentIds": ["agt_a", "agt_b"]}));

        let empty = BotSubscribeParams {
            agent_ids: vec![],
            full_fidelity: false,
        };
        let empty_wire = json!({"agentIds": []});
        assert_eq!(roundtrip(&empty), empty_wire);
        let parsed: BotSubscribeParams = serde_json::from_value(empty_wire).unwrap();
        assert!(parsed.agent_ids.is_empty());

        let bind = BotBindConversationParams {
            conversation_id: "conv_1".to_owned(),
            agent_ids: vec!["agt_a".to_owned()],
            primary: "agt_a".to_owned(),
        };
        let bind_wire = json!({
            "conversationId": "conv_1",
            "agentIds": ["agt_a"],
            "primary": "agt_a",
        });
        assert_eq!(roundtrip(&bind), bind_wire);
        let parsed: BotBindConversationParams = serde_json::from_value(bind_wire).unwrap();
        assert_eq!(parsed, bind);
        assert_rejects::<BotBindConversationParams>(json!({
            "conversation_id": "conv_1",
            "agent_ids": ["agt_a"],
            "primary": "agt_a",
        }));

        let empty_bind = BotBindConversationParams {
            conversation_id: "conv_2".to_owned(),
            agent_ids: vec![],
            primary: "agt_a".to_owned(),
        };
        let empty_bind_wire = json!({
            "conversationId": "conv_2",
            "agentIds": [],
            "primary": "agt_a",
        });
        assert_eq!(roundtrip(&empty_bind), empty_bind_wire);
        let parsed: BotBindConversationParams = serde_json::from_value(empty_bind_wire).unwrap();
        assert!(parsed.agent_ids.is_empty());
    }

    #[test]
    fn error_codes_round_trip_and_unknown_becomes_upstream_error() {
        for code in BotRelayErrorCode::ALL {
            assert_eq!(BotRelayErrorCode::from_wire(code.as_str()), *code);
            assert_eq!(roundtrip(code), json!(code.as_str()));
            assert_eq!(code.to_string(), code.as_str());
        }

        let unknown: BotRelayErrorCode =
            serde_json::from_value(json!("brand_new_code")).expect("unknown code must parse");
        assert_eq!(unknown, BotRelayErrorCode::UpstreamError);
        assert_eq!(
            serde_json::to_value(unknown).unwrap(),
            json!("upstream_error")
        );
    }

    #[test]
    fn retired_not_linked_degrades_to_upstream_error() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "not_linked",
            "retryable": false,
            "detail": {"upstream": "legacy client"},
        }))
        .expect("retired not_linked must deserialize");
        assert_eq!(parsed.code, BotRelayErrorCode::UpstreamError);
        assert!(!parsed.retryable);
        assert_eq!(parsed.detail.upstream.as_deref(), Some("legacy client"));
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {"upstream": "legacy client"},
            })
        );
    }

    #[test]
    fn error_object_matches_design_frame() {
        let err = BotRelayError {
            code: BotRelayErrorCode::UsageExhausted,
            retryable: false,
            detail: BotRelayErrorDetail {
                upstream: Some("...".to_owned()),
                ..Default::default()
            },
            reason: None,
        };
        let wire = json!({
            "code": "usage_exhausted",
            "retryable": false,
            "detail": {"upstream": "..."},
        });
        assert_eq!(roundtrip(&err), wire);
        let parsed: BotRelayError = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, err);
    }

    #[test]
    fn identity_unavailable_is_retryable_and_omits_login_url() {
        let err = BotRelayError {
            code: BotRelayErrorCode::IdentityUnavailable,
            retryable: true,
            detail: BotRelayErrorDetail::default(),
            reason: None,
        };
        let wire = roundtrip(&err);
        assert_eq!(
            wire,
            json!({
                "code": "identity_unavailable",
                "retryable": true,
                "detail": {},
            })
        );
        assert!(!wire.as_object().unwrap().contains_key("loginUrl"));
        assert_eq!(err.code.jsonrpc_code_key(), "tool_unavailable");
        assert_eq!(err.code.jsonrpc_numeric(), -32013);
    }

    #[test]
    fn command_rejected_carries_reason() {
        let err = BotRelayError {
            code: BotRelayErrorCode::CommandRejected,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some(COMMAND_REJECTED_NOT_YET_ENABLED.to_owned()),
        };
        assert_eq!(
            roundtrip(&err),
            json!({
                "code": "command_rejected",
                "retryable": false,
                "detail": {},
                "reason": "not_yet_enabled",
            })
        );
    }

    #[test]
    fn helper_true_only_for_gateway_unknown_method() {
        let skew = BotRelayError {
            code: BotRelayErrorCode::CommandRejected,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some(COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD.to_owned()),
        };
        assert!(is_gateway_method_unsupported(&skew));

        let catalog_miss = BotRelayError {
            code: BotRelayErrorCode::CommandRejected,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some("unknown_method".to_owned()),
        };
        assert!(!is_gateway_method_unsupported(&catalog_miss));

        let gated = BotRelayError {
            code: BotRelayErrorCode::CommandRejected,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some(COMMAND_REJECTED_NOT_YET_ENABLED.to_owned()),
        };
        assert!(!is_gateway_method_unsupported(&gated));

        let upstream = BotRelayError {
            code: BotRelayErrorCode::UpstreamError,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some(COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD.to_owned()),
        };
        assert!(!is_gateway_method_unsupported(&upstream));
    }

    #[test]
    fn unknown_error_code_preserves_fields_and_reserializes_as_upstream_error() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "some_future_code",
            "retryable": true,
            "detail": {"upstream": "upstream rejected the request"},
            "loginUrl": "https://example.invalid/unused",
            "reason": "future_reason",
        }))
        .expect("unknown code must deserialize");
        assert_eq!(parsed.code, BotRelayErrorCode::UpstreamError);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": true,
                "detail": {"upstream": "upstream rejected the request"},
                "reason": "future_reason",
            })
        );
        assert!(
            !serde_json::to_value(&parsed)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("loginUrl")
        );

        let with_reason_only: BotRelayError = serde_json::from_value(json!({
            "code": "brand_new",
            "retryable": false,
            "reason": "x",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&with_reason_only).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {},
                "reason": "x",
            })
        );

        let no_optionals: BotRelayError = serde_json::from_value(json!({
            "code": "brand_new",
            "retryable": false,
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&no_optionals).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {},
            })
        );
    }

    #[test]
    fn missing_detail_defaults() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "box_migrating",
            "retryable": true,
        }))
        .expect("detail is optional on receive");
        assert_eq!(parsed.code, BotRelayErrorCode::BoxMigrating);
        assert!(parsed.retryable);
        assert_eq!(parsed.detail, BotRelayErrorDetail::default());
    }

    #[test]
    fn bot_relay_error_wraps_as_jsonrpc_data() {
        for code in BotRelayErrorCode::ALL {
            assert_eq!(
                code.jsonrpc_numeric(),
                error_codes::numeric_for(code.jsonrpc_code_key()).expect("ERROR_CODES key"),
                "{code}"
            );
            let err = BotRelayError {
                code: *code,
                retryable: false,
                detail: BotRelayErrorDetail::default(),
                reason: None,
            };
            let envelope: JsonRpcError = err.clone().into();
            assert_eq!(envelope.code, code.jsonrpc_numeric(), "{code}");
            assert_eq!(envelope.message, code.as_str(), "{code}");
            let data = envelope.data.expect("data");
            let back: BotRelayError = serde_json::from_value(data).unwrap();
            assert_eq!(back, err, "{code}");
        }
    }

    #[test]
    fn hub_channel_all_round_trips_and_unprefixed_stays_upstream() {
        for ch in HubChannel::ALL {
            assert_eq!(HubChannel::from_wire(ch.as_str()), Some(*ch));
            assert_eq!(roundtrip(&BotEventChannel::from(*ch)), json!(ch.as_str()));
            assert_eq!(ch.to_string(), ch.as_str());
            assert_eq!(BotEventChannel::from(*ch).to_string(), ch.as_str());
        }
        let unprefixed = BotEventChannel::from_wire("turn_finished");
        assert!(matches!(unprefixed, BotEventChannel::Upstream(_)));
        assert_eq!(unprefixed.as_str(), "turn_finished");
        assert_eq!(roundtrip(&unprefixed), json!("turn_finished"));
    }

    #[test]
    fn channel_upstream_and_hub_wire_forms() {
        assert_eq!(
            roundtrip(&BotEventChannel::from_wire("transcript")),
            json!("transcript")
        );
        assert_eq!(
            roundtrip(&BotEventChannel::from(HubChannel::TurnFinished)),
            json!("hub:turn_finished")
        );
        assert_eq!(
            roundtrip(&BotEventChannel::from(HubChannel::ResyncRequired)),
            json!("hub:resync_required")
        );

        let future: BotEventChannel =
            serde_json::from_value(json!("hub:future_channel")).expect("future hub:* must parse");
        assert!(matches!(future, BotEventChannel::HubUnknown(_)));
        assert_eq!(future.as_str(), "hub:future_channel");
        assert_eq!(future.to_string(), "hub:future_channel");
        assert_ne!(
            future,
            BotEventChannel::from_wire("transcript"),
            "hub-unknown is not an upstream channel"
        );
    }

    #[test]
    fn event_envelope_matches_design_frame() {
        let env = BotEventEnvelope {
            v: 1,
            agent_id: "agt_...".to_owned(),
            seq: 1088,
            channel: BotEventChannel::from_wire("transcript"),
            event: json!({"kind": "entry"}),
            event_id: None,
        };
        let wire = json!({
            "v": 1,
            "agentId": "agt_...",
            "seq": 1088,
            "channel": "transcript",
            "event": {"kind": "entry"},
        });
        assert_eq!(roundtrip(&env), wire);
        let parsed: BotEventEnvelope = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, env);
        assert!(
            !serde_json::to_value(&env)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("eventId")
        );
        assert_rejects::<BotEventEnvelope>(json!({
            "v": 1,
            "agent_id": "agt_...",
            "seq": 1088,
            "channel": "transcript",
            "event": {"kind": "entry"},
        }));
    }

    #[test]
    fn event_id_omitted_and_null_are_none() {
        let omitted: BotEventEnvelope = serde_json::from_value(json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 1,
            "channel": "transcript",
            "event": {},
        }))
        .unwrap();
        assert_eq!(omitted.event_id, None);
        let explicit_null: BotEventEnvelope = serde_json::from_value(json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 1,
            "channel": "transcript",
            "event": {},
            "eventId": null,
        }))
        .unwrap();
        assert_eq!(explicit_null.event_id, None);
    }

    #[test]
    fn event_envelope_with_event_id() {
        let env = BotEventEnvelope {
            v: BOT_EVENT_ENVELOPE_V,
            agent_id: "agt_1".to_owned(),
            seq: 1,
            channel: HubChannel::TurnFinished.into(),
            event: json!({"preview": "done"}),
            event_id: Some("evt_9".to_owned()),
        };
        assert_eq!(
            roundtrip(&env),
            json!({
                "v": 1,
                "agentId": "agt_1",
                "seq": 1,
                "channel": "hub:turn_finished",
                "event": {"preview": "done"},
                "eventId": "evt_9",
            })
        );
    }

    #[test]
    fn hub_owned_event_bodies_round_trip() {
        let finished = HubTurnFinishedEvent {
            agent_id: "agt_1".to_owned(),
            conversation_ids: vec!["conv_1".to_owned()],
            preview: "done".to_owned(),
        };
        assert_eq!(
            roundtrip(&finished),
            json!({
                "agentId": "agt_1",
                "conversationIds": ["conv_1"],
                "preview": "done",
            })
        );
        let resync = HubResyncRequiredEvent {
            agent_id: "agt_1".to_owned(),
        };
        assert_eq!(roundtrip(&resync), json!({"agentId": "agt_1"}));
        assert_rejects::<HubResyncRequiredEvent>(json!({"agent_id": "agt_1"}));
    }

    #[test]
    fn event_envelope_composes_hub_owned_bodies() {
        let finished = HubTurnFinishedEvent {
            agent_id: "agt_1".to_owned(),
            conversation_ids: vec!["conv_1".to_owned()],
            preview: "done".to_owned(),
        };
        let finished_env = BotEventEnvelope::new(
            "agt_1",
            2,
            HubChannel::TurnFinished,
            serde_json::to_value(&finished).unwrap(),
        );
        let finished_wire = json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 2,
            "channel": "hub:turn_finished",
            "event": {
                "agentId": "agt_1",
                "conversationIds": ["conv_1"],
                "preview": "done",
            },
        });
        assert_eq!(roundtrip(&finished_env), finished_wire);
        let parsed: BotEventEnvelope = serde_json::from_value(finished_wire).unwrap();
        assert_eq!(parsed.channel, HubChannel::TurnFinished.into());
        let parsed_body: HubTurnFinishedEvent = serde_json::from_value(parsed.event).unwrap();
        assert_eq!(parsed_body, finished);

        let resync = HubResyncRequiredEvent {
            agent_id: "agt_1".to_owned(),
        };
        let resync_env = BotEventEnvelope::new(
            "agt_1",
            3,
            HubChannel::ResyncRequired,
            serde_json::to_value(&resync).unwrap(),
        );
        let resync_wire = json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 3,
            "channel": "hub:resync_required",
            "event": {"agentId": "agt_1"},
        });
        assert_eq!(roundtrip(&resync_env), resync_wire);
        let parsed: BotEventEnvelope = serde_json::from_value(resync_wire).unwrap();
        assert_eq!(parsed.channel, HubChannel::ResyncRequired.into());
        let parsed_body: HubResyncRequiredEvent = serde_json::from_value(parsed.event).unwrap();
        assert_eq!(parsed_body, resync);
    }
    #[test]
    fn event_envelope_new_omits_event_id_on_the_wire() {
        let env = BotEventEnvelope::new("agt_1", 1, HubChannel::ResyncRequired, json!({}));
        let wire = serde_json::to_value(&env).unwrap();
        assert_eq!(
            wire,
            json!({
                "v": 1,
                "agentId": "agt_1",
                "seq": 1,
                "channel": "hub:resync_required",
                "event": {},
            })
        );
        assert!(!wire.as_object().unwrap().contains_key("eventId"));
    }

    #[test]
    fn link_state_try_from_agrees_with_is_link_state() {
        for &code in BotRelayErrorCode::ALL {
            let converted = LinkStateCode::try_from(code);
            assert_eq!(code.is_link_state(), converted.is_ok(), "{code}");
            if let Ok(link) = converted {
                assert_eq!(code, BotRelayErrorCode::from(link));
            }
        }
    }
}
