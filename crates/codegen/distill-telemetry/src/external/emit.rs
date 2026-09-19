//! The *how* of external emission: content gates, secret scrubbing and truncation, ctx injection, and metric-increment conversion.
//!
//! The per-event *what*, which field becomes which attribute, lives in [`super::schema`].
//! The `telemetry_event!` macro's `external = …` arm wires it in.

use opentelemetry::KeyValue;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, Severity};

use super::ExternalTelemetry;
use super::metrics::{Instruments, MetricIncrement};
use super::schema::{AttrValue, ExternalKey, ExternalRecord};

/// Scrub and truncate one string attribute value.
/// Every string passes the secret/path scrub; prompt, response, tool_input, tool_output, and full_command get the 60 KB content cap; tool_parameters preview and error_message get the 4 KB preview cap; everything else the standard 512-to-128 value truncation.
/// This is defense in depth only: the export-time validators in [`super::redact`] enforce the result.
fn scrub_string(key: ExternalKey, s: String) -> String {
    let scrubbed = crate::redact_common::redact_to_owned(&s);
    match key {
        ExternalKey::Prompt
        | ExternalKey::Response
        | ExternalKey::FullCommand
        | ExternalKey::ToolInput
        | ExternalKey::ToolOutput => {
            super::truncate::truncate_content(&scrubbed).unwrap_or(scrubbed)
        }
        ExternalKey::ToolParameters | ExternalKey::ErrorMessage => {
            super::truncate::truncate_preview(&scrubbed).unwrap_or(scrubbed)
        }
        _ => super::truncate::truncate_value_owned(scrubbed),
    }
}

fn to_any_value(v: AttrValue) -> AnyValue {
    match v {
        AttrValue::Str(s) => AnyValue::String(s.into()),
        AttrValue::I64(i) => AnyValue::Int(i),
        AttrValue::Bool(b) => AnyValue::Boolean(b),
        AttrValue::DeferredJson(v) => AnyValue::String(v.to_string().into()),
    }
}

/// Shared by the log and metric paths so the identity attr set cannot drift.
fn for_each_identity_attr(
    identity: &super::IdentityAttrs,
    mut sink: impl FnMut(&'static str, String),
) {
    for (key, value) in [
        (ExternalKey::UserId, identity.user_id.as_deref()),
        (ExternalKey::UserEmail, identity.email.as_deref()),
        (
            ExternalKey::OrganizationId,
            identity.organization_id.as_deref(),
        ),
        (ExternalKey::TeamId, identity.team_id.as_deref()),
        (ExternalKey::DeploymentId, identity.deployment_id.as_deref()),
    ] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            sink(<&'static str>::from(key), v.to_owned());
        }
    }
}

/// Convert one mapped [`ExternalRecord`] into a log record and metric increments.
/// Synchronous and cheap: the `BatchLogProcessor` queues the record; unlike the product-events path there is no `tokio::spawn`.
pub(crate) fn emit_record(ext: &ExternalTelemetry, mut record: ExternalRecord) {
    let gates = *ext.gates.read();

    // Gated attributes: emitted only when the matching gate is on
    // A gated value sharing a key with a default attr (verbatim vs. sanitized `tool_name`) replaces the default.
    for gated in std::mem::take(&mut record.gated) {
        if !gated.gate.is_open(&gates) {
            continue;
        }
        let value = match gated.value {
            AttrValue::DeferredJson(v) => AttrValue::Str(v.to_string()),
            other => other,
        };
        if let Some(existing) = record.attrs.iter_mut().find(|(k, _)| *k == gated.key) {
            existing.1 = value;
        } else {
            record.attrs.push((gated.key, value));
        }
    }

    // Ambient ctx: a mapping-supplied `session.id` wins
    // The ctx is a fallback for in-session events (the session-start sites are spawned outside the ctx scope and carry their own ids)
    let ctx = crate::session_ctx::external_ctx_snapshot();
    let mapped_session_id = record
        .attrs
        .iter()
        .find(|(k, _)| *k == ExternalKey::SessionId)
        .and_then(|(_, v)| match v {
            AttrValue::Str(s) => Some(s.clone()),
            _ => None,
        });
    let session_id = mapped_session_id.or_else(|| ctx.as_ref().map(|c| c.session_id.clone()));

    for (key, value) in record.attrs.iter_mut() {
        if let AttrValue::Str(s) = value {
            *value = AttrValue::Str(scrub_string(*key, std::mem::take(s)));
        }
    }

    let identity = ext.identity.read().clone();

    if let (Some(event), Some(logger)) = (record.event, ext.logger.as_ref()) {
        let mut log_record = logger.create_log_record();
        log_record.set_event_name(event.into());
        log_record.set_severity_number(Severity::Info);
        let now = std::time::SystemTime::now();
        log_record.set_timestamp(now);
        log_record.set_observed_timestamp(now);
        log_record.add_attribute(
            ExternalKey::EventSequence.as_ref(),
            ext.next_sequence() as i64,
        );
        if record
            .attrs
            .iter()
            .all(|(k, _)| *k != ExternalKey::SessionId)
            && let Some(sid) = session_id.as_deref()
        {
            log_record.add_attribute(ExternalKey::SessionId.as_ref(), sid.to_owned());
        }
        if let Some(ctx) = ctx.as_ref() {
            if let Some(turn) = ctx.turn_number {
                log_record.add_attribute(ExternalKey::TurnNumber.as_ref(), turn as i64);
            }
            // prompt.id: events only, never metrics (unbounded cardinality).
            if let Some(prompt_id) = ctx.prompt_id.as_deref() {
                log_record.add_attribute(ExternalKey::PromptId.as_ref(), prompt_id.to_owned());
            }
        }
        for (key, value) in &record.attrs {
            log_record.add_attribute(
                Into::<&'static str>::into(*key),
                to_any_value(value.clone()),
            );
        }
        for_each_identity_attr(&identity, |key, value| {
            log_record.add_attribute(key, value);
        });
        logger.emit(log_record);
    }

    if let Some(instruments) = ext.instruments.as_ref() {
        for increment in record.metrics {
            add_increment(
                ext,
                instruments,
                increment,
                session_id.as_deref(),
                &identity,
            );
        }
    }
}

fn add_increment(
    ext: &ExternalTelemetry,
    instruments: &Instruments,
    increment: MetricIncrement,
    session_id: Option<&str>,
    identity: &super::IdentityAttrs,
) {
    // Identity/cardinality attrs shared by every instrument
    // `prompt.id` is deliberately never attached to metrics
    let mut attrs: Vec<KeyValue> = Vec::with_capacity(8);
    if ext.include_session_id_on_metrics
        && let Some(sid) = session_id.filter(|s| !s.is_empty())
    {
        attrs.push(KeyValue::new("session.id", sid.to_owned()));
    }
    if ext.include_version_on_metrics && !ext.app_version.is_empty() {
        attrs.push(KeyValue::new("app.version", ext.app_version.clone()));
    }
    for_each_identity_attr(identity, |key, value| attrs.push(KeyValue::new(key, value)));

    instruments.record_increment(increment, attrs);
}
