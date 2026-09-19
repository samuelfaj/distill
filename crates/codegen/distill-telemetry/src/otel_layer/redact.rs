// Modified for Distill by Samuel Fajreldines, 2026.
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::LazyLock;

use opentelemetry::trace::{Event, Status};
use opentelemetry::{Array, KeyValue, StringValue, Value};
use opentelemetry_sdk::trace::SpanData;

pub(crate) static ALLOWED_STRING_KEYS: &[&str] = &[
    "level",
    "target",
    "code.namespace",
    "code.filepath",
    "thread.name",
    "session_id",
    "prompt_id",
    "req_id",
    "request_id",
    "child_session_id",
    "parent_session_id",
    "subagent_id",
    "agent_id",
    "task_id",
    "tool_call_id",
    "call_id",
    "event_id",
    "conv_id",
    "turn_id",
    "model_id",
    "model",
    "compact_model",
    "client_type",
    "client_version",
    "subagent_type",
    "persona",
    "role",
    "skill_name",
    "server_name",
    "tool_name",
    "tool_names",
    "method",
    "operation",
    "endpoint",
    "path",
    "file_path",
    "repo_path",
    "gcs_path",
    "gcs_url",
    "url",
    "output_path",
    "dir",
    "dir_path",
    "notebook",
    "cwd",
    "original_cwd",
    "chosen_repo_root",
    "worktree",
    "source",
    "bucket_url",
    "object_path",
    "archive_name",
    "artifact",
    "verdict",
    "pattern_class",
    "phase",
    "upload_reason",
    "suppress_reason",
    "error_kind",
    "error_category",
    "error_type",
    "outcome",
    "ttft_outcome",
    "freshness",
    "decision",
    "update_type",
    "kind",
    "step",
    "token_type",
    "stop_reason",
    "compaction_outcome",
    "compaction_stop_reason",
    "compaction_trigger",
    "compaction_prefire_outcome",
    "aspect_ratio",
    "resolution",
    "schedule",
    "interval",
    "mode",
    "isolation",
    "detail",
    "metric",
    "strategy",
    "size_class",
    "status",
    "action",
    "auth_method",
    "to_mode",
    "trigger",
    "survey_type",
    "mention_type",
    "install_kind",
    "transport_type",
    "invocation_trigger",
    "skill_source",
    "plugin_name",
    "plugin_version",
    "plugin_scope",
    "hook_event",
    "hook_name",
    "hook_type",
    "hook_source",
    "server_scope",
    "mcp_server.name",
    "mcp_tool.name",
    "agent.name",
    "skill.name",
    "query_source",
    "effort",
    "start_type",
    "error",
    "location",
    "user_id",
    "parent_agent_id",
    "from_mode",
    "tool_use_id",
    "command_name",
    "command_source",
    "event_type",
    "appearance_id",
    "terminal.brand",
    "terminal.multiplexer",
    "terminal.tmux_version",
    "terminal.term_var",
    "terminal.term_version",
    "terminal.term_version_source",
    "skip_reason",
    "auto_cadence_reason",
];

static ALLOWED_STRING_KEY_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ALLOWED_STRING_KEYS.iter().copied().collect());

static ORIGIN_REDUCED_KEYS: &[&str] = &["server_name", "url", "endpoint", "gcs_url", "bucket_url"];

pub(crate) fn redact_batch(batch: &mut [SpanData]) {
    for span in batch.iter_mut() {
        let SpanData {
            name,
            attributes,
            events,
            links,
            status,
            span_context: _,
            parent_span_id: _,
            parent_span_is_remote: _,
            span_kind: _,
            start_time: _,
            end_time: _,
            dropped_attributes_count: _,
            instrumentation_scope: _,
        } = span;
        redact_in_place(name);
        scrub_attributes(attributes);
        for event in &mut events.events {
            neuter_event_name(event);

            redact_in_place(&mut event.name);
            scrub_attributes(&mut event.attributes);
        }

        if let Status::Error { description } = status {
            redact_in_place(description);
        }
        for link in &mut links.links {
            scrub_attributes(&mut link.attributes);
        }
    }
}

fn is_content_value(value: &Value) -> bool {
    !matches!(
        value,
        Value::Bool(_)
            | Value::I64(_)
            | Value::F64(_)
            | Value::Array(Array::Bool(_) | Array::I64(_) | Array::F64(_))
    )
}

fn enforce_allowlist(attrs: &mut Vec<KeyValue>) {
    attrs.retain(|kv| {
        !is_content_value(&kv.value) || ALLOWED_STRING_KEY_SET.contains(kv.key.as_str())
    });
}

fn scrub_attributes(attrs: &mut Vec<KeyValue>) {
    enforce_allowlist(attrs);
    for kv in attrs.iter_mut() {
        if ORIGIN_REDUCED_KEYS.contains(&kv.key.as_str()) {
            reduce_url_to_origin(&mut kv.value);
        }
        redact_value(&mut kv.value);
    }
}

fn neuter_event_name(event: &mut Event) {
    let mut file: Option<String> = None;
    let mut line: Option<i64> = None;
    for kv in &event.attributes {
        match kv.key.as_str() {
            "code.filepath" => {
                if let Value::String(s) = &kv.value {
                    file = Some(s.as_str().to_owned());
                }
            }
            "code.lineno" => {
                if let Value::I64(n) = &kv.value {
                    line = Some(*n);
                }
            }
            _ => {}
        }
    }
    event.name = match (file, line) {
        (Some(f), Some(l)) => format!("{f}:{l}").into(),
        (Some(f), None) => f.into(),

        _ => Cow::Borrowed("event"),
    };
}

fn reduce_url_to_origin(value: &mut Value) {
    if let Value::String(s) = value
        && let Cow::Owned(origin) = crate::redact_common::url_origin(s.as_str())
    {
        *s = StringValue::from(origin);
    }
}

fn redact_owned(input: &str) -> Option<String> {
    crate::redact_common::redact_owned(input)
}

fn redact_in_place(s: &mut Cow<'static, str>) {
    if let Some(redacted) = redact_owned(s.as_ref()) {
        *s = Cow::Owned(redacted);
    }
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Some(redacted) = redact_owned(s.as_str()) {
                *s = StringValue::from(redacted);
            }
        }
        Value::Array(Array::String(items)) => {
            for s in items.iter_mut() {
                if let Some(redacted) = redact_owned(s.as_str()) {
                    *s = StringValue::from(redacted);
                }
            }
        }

        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_value_scrubs_secret_string() {
        let mut v = Value::String(StringValue::from(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.foo.bar.baz".to_string(),
        ));
        redact_value(&mut v);
        let Value::String(s) = &v else {
            panic!("expected string value");
        };
        assert!(
            s.as_str().contains("[REDACTED_SECRET]"),
            "secret not scrubbed: {}",
            s.as_str()
        );
    }

    #[test]
    fn event_name_neutered_to_callsite_drops_message_content() {
        let mut ev = Event::new(
            "received prompt: rm -rf /Users/alice/secret",
            std::time::SystemTime::now(),
            vec![
                KeyValue::new("code.filepath", "src/foo.rs"),
                KeyValue::new("code.lineno", 42_i64),
            ],
            0,
        );
        neuter_event_name(&mut ev);
        assert_eq!(ev.name, "src/foo.rs:42");
        assert!(!ev.name.contains("prompt") && !ev.name.contains("rm -rf"));
    }

    #[test]
    fn event_name_without_location_drops_to_marker() {
        let mut ev = Event::new("SECRET {x:?}", std::time::SystemTime::now(), vec![], 0);
        neuter_event_name(&mut ev);
        assert_eq!(ev.name, "event");
    }

    #[test]
    fn allowlist_drops_nonallowlisted_content_keeps_safe_and_numeric() {
        let mut attrs = vec![
            KeyValue::new("session_id", "sess-abc"),
            KeyValue::new("path", "/tmp/x.rs"),
            KeyValue::new("prompt", "CANARY_PROMPT secret user text"),
            KeyValue::new("command", "echo CANARY_SECRET"),
            KeyValue::new("turn_number", 7_i64),
            KeyValue::new("is_background", true),
        ];
        enforce_allowlist(&mut attrs);
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        assert!(keys.contains(&"session_id"));
        assert!(keys.contains(&"path"));
        assert!(keys.contains(&"turn_number"));
        assert!(keys.contains(&"is_background"));
        assert!(
            !keys.contains(&"prompt"),
            "non-allowlisted content must be dropped"
        );
        assert!(
            !keys.contains(&"command"),
            "non-allowlisted content must be dropped"
        );

        let blob = format!("{attrs:?}");
        assert!(
            !blob.contains("CANARY_PROMPT"),
            "prompt content leaked: {blob}"
        );
        assert!(
            !blob.contains("CANARY_SECRET"),
            "command content leaked: {blob}"
        );
    }

    #[test]
    fn allowlist_contents_are_pinned() {
        let expected: &[&str] = &[
            "level",
            "target",
            "code.namespace",
            "code.filepath",
            "thread.name",
            "session_id",
            "prompt_id",
            "req_id",
            "request_id",
            "child_session_id",
            "parent_session_id",
            "subagent_id",
            "agent_id",
            "task_id",
            "tool_call_id",
            "call_id",
            "event_id",
            "conv_id",
            "turn_id",
            "model_id",
            "model",
            "compact_model",
            "client_type",
            "client_version",
            "subagent_type",
            "persona",
            "role",
            "skill_name",
            "server_name",
            "tool_name",
            "tool_names",
            "method",
            "operation",
            "endpoint",
            "path",
            "file_path",
            "repo_path",
            "gcs_path",
            "gcs_url",
            "url",
            "output_path",
            "dir",
            "dir_path",
            "notebook",
            "cwd",
            "original_cwd",
            "chosen_repo_root",
            "worktree",
            "source",
            "bucket_url",
            "object_path",
            "archive_name",
            "artifact",
            "verdict",
            "pattern_class",
            "phase",
            "upload_reason",
            "suppress_reason",
            "error_kind",
            "error_category",
            "error_type",
            "outcome",
            "ttft_outcome",
            "freshness",
            "decision",
            "update_type",
            "kind",
            "step",
            "token_type",
            "stop_reason",
            "compaction_outcome",
            "compaction_stop_reason",
            "compaction_trigger",
            "compaction_prefire_outcome",
            "aspect_ratio",
            "resolution",
            "schedule",
            "interval",
            "mode",
            "isolation",
            "detail",
            "metric",
            "strategy",
            "size_class",
            "status",
            "action",
            "auth_method",
            "to_mode",
            "trigger",
            "survey_type",
            "mention_type",
            "install_kind",
            "transport_type",
            "invocation_trigger",
            "skill_source",
            "plugin_name",
            "plugin_version",
            "plugin_scope",
            "hook_event",
            "hook_name",
            "hook_type",
            "hook_source",
            "server_scope",
            "mcp_server.name",
            "mcp_tool.name",
            "agent.name",
            "skill.name",
            "query_source",
            "effort",
            "start_type",
            "error",
            "location",
            "user_id",
            "parent_agent_id",
            "from_mode",
            "tool_use_id",
            "command_name",
            "command_source",
            "event_type",
            "appearance_id",
            "terminal.brand",
            "terminal.multiplexer",
            "terminal.tmux_version",
            "terminal.term_var",
            "terminal.term_version",
            "terminal.term_version_source",
            "skip_reason",
            "auto_cadence_reason",
        ];
        assert_eq!(
            ALLOWED_STRING_KEYS, expected,
            "ALLOWED_STRING_KEYS changed: adding a key exports a new field — confirm it carries no \
             user content and get telemetry-owner review, then update this pin."
        );
    }

    #[test]
    fn error_status_message_retained_but_secret_scrubbed() {
        let mut status = Status::error("upstream auth failed: sk-CANARYabcdefghij1234567890");
        if let Status::Error { description } = &mut status {
            redact_in_place(description);
        }
        let Status::Error { description } = status else {
            panic!("status code must stay Error");
        };
        assert!(
            description.contains("upstream auth failed"),
            "useful message lost: {description}"
        );
        assert!(
            !description.contains("CANARY"),
            "secret survived: {description}"
        );
    }

    #[test]
    fn url_value_reduced_to_origin_dropping_path_and_query() {
        let mut attrs = vec![KeyValue::new(
            "url",
            "https://example.com:8443/search?q=CANARY+secret+terms&u=bob#frag",
        )];
        scrub_attributes(&mut attrs);
        let blob = format!("{attrs:?}");
        assert!(
            blob.contains("https://example.com:8443"),
            "origin lost: {blob}"
        );
        assert!(!blob.contains("CANARY"), "query content survived: {blob}");
        assert!(!blob.contains("search"), "path survived: {blob}");
    }

    #[test]
    fn url_valued_keys_reduced_to_origin_but_storage_paths_kept() {
        let mut attrs = vec![
            KeyValue::new(
                "bucket_url",
                "https://store.example.com/b/CANARY/o?sig=CANARYSIG",
            ),
            KeyValue::new(
                "endpoint",
                "https://api.example.com:8443/v1/chat?u=CANARYUSER",
            ),
            KeyValue::new("gcs_path", "sessions/abc123/artifact-kept.tar"),
        ];
        scrub_attributes(&mut attrs);
        let blob = format!("{attrs:?}");
        assert!(
            blob.contains("https://store.example.com"),
            "bucket_url origin lost: {blob}"
        );
        assert!(
            blob.contains("https://api.example.com:8443"),
            "endpoint origin lost: {blob}"
        );
        assert!(
            !blob.contains("CANARY"),
            "url path/query content survived: {blob}"
        );
        assert!(
            blob.contains("sessions/abc123/artifact-kept.tar"),
            "storage path was wrongly reduced: {blob}"
        );
    }

    #[test]
    fn allowlisted_value_is_still_secret_scrubbed() {
        let mut attrs = vec![KeyValue::new("source", "sk-CANARYabcdefghij1234567890")];
        scrub_attributes(&mut attrs);
        let blob = format!("{attrs:?}");
        assert!(
            !blob.contains("CANARY"),
            "secret in allowlisted value not scrubbed: {blob}"
        );
    }

    #[test]
    fn allowlisted_path_values_are_still_home_scrubbed() {
        let home = distill_dirs::home_dir().expect("home dir for path-scrub test");
        let home_str = home.to_string_lossy();

        if home_str.len() < 4 {
            return;
        }
        let full = format!("{home_str}/secret-project/src/main.rs");
        let mut attrs = vec![
            KeyValue::new("path", full.clone()),
            KeyValue::new("file_path", full.clone()),
            KeyValue::new("cwd", full.clone()),
        ];
        scrub_attributes(&mut attrs);
        let blob = format!("{attrs:?}");
        assert!(
            !blob.contains(home_str.as_ref()),
            "home path survived allowlisted scrub: {blob}"
        );
        assert!(
            blob.contains("main.rs") || blob.contains("[HOME]") || blob.contains("~"),
            "expected redacted path to retain a filename or home marker: {blob}"
        );
    }

    #[test]
    fn error_key_value_is_secret_and_path_scrubbed() {
        let home = distill_dirs::home_dir().expect("home dir");
        let home_str = home.to_string_lossy();
        let msg =
            format!("failed reading {home_str}/.config/creds with sk-CANARYabcdefghij1234567890");
        let mut attrs = vec![KeyValue::new("error", msg)];
        scrub_attributes(&mut attrs);
        let blob = format!("{attrs:?}");
        assert!(!blob.contains("CANARY"), "secret survived in error: {blob}");
        if home_str.len() >= 4 {
            assert!(
                !blob.contains(home_str.as_ref()),
                "home path survived in error: {blob}"
            );
        }
    }
}
