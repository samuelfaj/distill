//! Data-consent product telemetry events.

use serde::Serialize;

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingDataConsentSource {
    PrivacyBanner,
    Settings,
    /// "Opt in" on the `/feedback` trace-consent card while individually opted out.
    FeedbackTraceCard,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodingDataConsentChoice {
    OptIn,
    OptOut,
}

impl CodingDataConsentChoice {
    pub fn from_opted_in(opted_in: bool) -> Self {
        if opted_in { Self::OptIn } else { Self::OptOut }
    }
}

#[derive(Serialize)]
pub struct CodingDataConsentSelected {
    pub source: CodingDataConsentSource,
    pub choice: CodingDataConsentChoice,
    pub previous_choice: CodingDataConsentChoice,
    pub changed: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTraceConsentChoice {
    /// Enable trace uploads for future sessions.
    TurnOn,
    /// Upload only the session attached to this feedback report.
    SendThisSession,
    /// Send the feedback without a trace.
    NoUpload,
    /// Close the trace prompt without choosing.
    Dismissed,
    /// "Opt out and don't ask again".
    NeverAsk,
}

/// The `/feedback` trace-consent card was shown (funnel denominator for [`FeedbackTraceConsentSelected`]).
#[derive(Serialize)]
pub struct FeedbackTraceCardShown {
    /// The "yes" option disclosed that it re-enables coding-data sharing.
    pub reenables_sharing: bool,
}

/// Outcome of the `/feedback` trace-consent card (only emitted when the card was shown).
#[derive(Serialize)]
pub struct FeedbackTraceConsentSelected {
    pub choice: FeedbackTraceConsentChoice,
    /// The "yes" option disclosed that it re-enables coding-data sharing.
    pub reenables_sharing: bool,
}
