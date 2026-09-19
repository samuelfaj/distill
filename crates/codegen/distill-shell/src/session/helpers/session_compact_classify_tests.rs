// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

fn is_det(failure: &CompactFailure) -> bool {
    matches!(failure, CompactFailure::Deterministic(_))
}

fn is_overflow(failure: &CompactFailure) -> bool {
    matches!(failure, CompactFailure::Overflow(_))
}

fn api_error(status: StatusCode, message: &str) -> SamplingError {
    SamplingError::Api {
        status,
        message: message.into(),
        model_metadata: None,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
    }
}

#[test]
fn sampling_api_4xx_is_deterministic_except_408_and_429() {
    let det = |s: StatusCode| is_det(&classify_sampling_error(api_error(s, "test")));
    assert!(det(StatusCode::BAD_REQUEST));
    assert!(det(StatusCode::UNAUTHORIZED));
    assert!(det(StatusCode::FORBIDDEN));
    assert!(det(StatusCode::NOT_FOUND));
    assert!(!det(StatusCode::REQUEST_TIMEOUT));
    assert!(!det(StatusCode::TOO_MANY_REQUESTS));
    assert!(!det(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(!det(StatusCode::BAD_GATEWAY));
    assert!(!det(StatusCode::SERVICE_UNAVAILABLE));
}

#[test]
fn sampling_api_413_is_overflow_by_status_alone() {
    assert!(is_overflow(&classify_sampling_error(api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Request failed (HTTP 413)."
    ))));
}

#[test]
fn sampling_api_drifted_size_wordings_are_overflow() {
    // Fleet-observed size-overflow wordings.
    for msg in [
        "exceed_context_size_error: request (300000 tokens) exceeds the model context size",
        "payload_too_large: Chat history exceeds the 800-message limit",
        "Input length (300000 tokens) exceeds the maximum allowed length (200000 tokens)",
    ] {
        assert!(
            is_overflow(&classify_sampling_error(api_error(
                StatusCode::BAD_REQUEST,
                msg
            ))),
            "should be overflow: {msg}"
        );
    }
    // A generic 400 stays plain Deterministic — no ladder.
    assert!(is_det(&classify_sampling_error(api_error(
        StatusCode::BAD_REQUEST,
        "invalid tool schema"
    ))));
}

#[test]
fn sampling_tpm_429_with_retry_after_stays_transient() {
    // A TPM 429 with Retry-After and size wording: the server is promising
    // capacity later, so the compaction loop backs off instead of burning an
    // input-ladder stage (see SamplingError::is_context_length_error).
    let size_text = "Request too large for model: Limit 30000, Requested 50000 tokens per min";
    let mut with_retry_after = api_error(StatusCode::TOO_MANY_REQUESTS, size_text);
    if let SamplingError::Api {
        retry_after_secs, ..
    } = &mut with_retry_after
    {
        *retry_after_secs = Some(7);
    }
    assert!(matches!(
        classify_sampling_error(with_retry_after),
        CompactFailure::Transient(_)
    ));
    // Without Retry-After the same wording is a per-request cap — Overflow.
    assert!(is_overflow(&classify_sampling_error(api_error(
        StatusCode::TOO_MANY_REQUESTS,
        size_text
    ))));
}

#[test]
fn sampling_stream_error_with_size_message_is_overflow() {
    // The fleet shape "stream error (BAD_REQUEST): Input length (N tokens)
    // exceeds the maximum allowed length (M tokens)".
    assert!(is_overflow(&classify_sampling_error(
        SamplingError::StreamError {
            error_type: "BAD_REQUEST".into(),
            message: "Input length (300000 tokens) exceeds the maximum allowed length \
                      (200000 tokens)"
                .into(),
            code: None,
        }
    )));
}

#[test]
fn sampling_stream_error_with_structured_size_code_is_overflow() {
    // Opaque message: the structured code slot is the only signal.
    for code in ["413", "payload_too_large", "request_too_large"] {
        assert!(
            is_overflow(&classify_sampling_error(SamplingError::StreamError {
                error_type: "BAD_REQUEST".into(),
                message: "request rejected".into(),
                code: Some(distill_sampling_types::ApiErrorCode::parse(code)),
            })),
            "should be overflow for code: {code}"
        );
    }
    // An opaque stream error with a non-size code stays transient.
    assert!(!is_det(&classify_sampling_error(
        SamplingError::StreamError {
            error_type: "server_error".into(),
            message: "internal error".into(),
            code: Some(distill_sampling_types::ApiErrorCode::parse(
                "overloaded_error"
            )),
        }
    )));
}

#[test]
fn sampling_non_api_variants_classify_correctly() {
    assert!(is_det(&classify_sampling_error(
        SamplingError::auth_unknown("expired")
    )));
    assert!(is_det(&classify_sampling_error(
        SamplingError::InvalidConfiguration("missing key")
    )));
    assert!(is_det(&classify_sampling_error(
        SamplingError::IdleTimeout { elapsed_secs: 60 }
    )));
    assert!(!is_det(&classify_sampling_error(
        SamplingError::EventStreamError("conn reset".into())
    )));
    assert!(!is_det(&classify_sampling_error(
        SamplingError::StreamError {
            error_type: "overloaded_error".into(),
            message: "try again".into(),
            code: None,
        }
    )));
}

#[test]
fn response_event_invalid_request_error_marker_is_deterministic() {
    // The documented schema-violation marker for the Anthropic Messages API
    // Production `messages.X.content.Y: thinking blocks ...` errors take this branch
    assert!(is_det(&classify_response_event_error(
        Some("invalid_request_error"),
        "messages.27.content.1: ..."
    )));
    // The marker can also appear in the message body (e.g. wrapped error envelopes from gateways).
    assert!(is_det(&classify_response_event_error(
        Some("400"),
        "Provider returned invalid_request_error: messages.X..."
    )));
}

#[test]
fn response_event_numeric_codes_match_http_classification() {
    let det = |c: &str| is_det(&classify_response_event_error(Some(c), "msg"));
    assert!(det("400"));
    assert!(det("401"));
    assert!(det("403"));
    assert!(det("404"));
    assert!(!det("408"));
    assert!(!det("429"));
    assert!(!det("500"));
    assert!(!det("503"));
}

#[test]
fn response_event_invalid_request_marker_with_size_text_is_overflow() {
    // Real overflows arrive AS invalid_request_error with size text — pins
    // that size outranks the schema marker.
    assert!(is_overflow(&classify_response_event_error(
        Some("invalid_request_error"),
        "prompt is too long: 300000 tokens > 200000 maximum"
    )));
}

#[test]
fn response_event_size_codes_are_overflow() {
    for code in ["413", "payload_too_large", "request_too_large"] {
        assert!(
            is_overflow(&classify_response_event_error(Some(code), "msg")),
            "should be overflow for code: {code}"
        );
    }
}

#[test]
fn response_event_unknown_code_defaults_to_transient() {
    // Uncertainty defaults to retry so we don't swallow blips
    assert!(!is_det(&classify_response_event_error(None, "msg")));
    assert!(!is_det(&classify_response_event_error(
        Some("error"),
        "msg"
    )));
    assert!(!is_det(&classify_response_event_error(
        Some("overloaded_error"),
        "msg"
    )));
}

#[test]
fn response_event_marker_in_message_with_no_code_is_deterministic() {
    // The most permissive shape an Anthropic Messages API might emit
    assert!(is_det(&classify_response_event_error(
        None,
        "messages.X.content.Y: invalid_request_error: ..."
    )));
}

#[test]
fn response_event_context_length_message_is_overflow() {
    // The inference backend streams the size overflow as a ResponseError with no usable code (`code="none"`); only the message identifies it
    assert!(is_overflow(&classify_response_event_error(
        None,
        "The prompt is too long for this model's context window."
    )));
}

#[test]
fn sampling_api_500_with_context_length_message_is_overflow() {
    // The sampler synthesizes status=500 from a streamed size overflow, so
    // status alone reads transient; the message must still short-circuit.
    assert!(is_overflow(&classify_sampling_error(api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "API error (status 500 Internal Server Error): \
         The prompt is too long for this model's context window."
    ))));
}

#[test]
fn sampling_http_is_transient() {
    // reqwest::Error has no public constructor; trigger one via a known-bad request
    // reqwest's TCP connect needs a Tokio reactor; futures::executor is not enough (CI runs in a Bazel sandbox where the failure surfaces)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let http_err = rt
        .block_on(reqwest::get("http://127.0.0.1:0"))
        .expect_err("connecting to port 0 must fail");
    assert!(!is_det(&classify_sampling_error(SamplingError::Http(
        http_err
    ))));
}

#[test]
fn sampling_serialization_is_deterministic() {
    let serde_err = serde_json::from_str::<u32>("not a number").unwrap_err();
    assert!(is_det(&classify_sampling_error(
        SamplingError::Serialization(serde_err)
    )));
}

#[test]
fn classifier_preserves_acp_error_data() {
    let CompactFailure::Deterministic(err) = classify_sampling_error(SamplingError::Api {
        status: StatusCode::BAD_REQUEST,
        message: "bad payload".into(),
        model_metadata: None,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
    }) else {
        panic!("expected Deterministic for 400");
    };
    let data = err.data.as_ref().and_then(|d| d.as_str()).unwrap();
    assert!(data.contains("compact failed"));
    assert!(data.contains("bad payload"));

    let CompactFailure::Transient(err) = classify_sampling_error(SamplingError::Api {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "upstream blip".into(),
        model_metadata: None,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
    }) else {
        panic!("expected Transient for 500");
    };
    let data = err.data.as_ref().and_then(|d| d.as_str()).unwrap();
    assert!(data.contains("upstream blip"));

    let CompactFailure::Overflow(err) = classify_sampling_error(api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Request failed (HTTP 413).",
    )) else {
        panic!("expected Overflow for 413");
    };
    let data = err.data.as_ref().and_then(|d| d.as_str()).unwrap();
    assert!(data.contains("compact failed"));
    assert!(data.contains("Request failed (HTTP 413)."));
}

#[test]
fn stream_timing_boundaries() {
    let mut t = StreamTiming::new();
    assert_eq!(t.count, 0);
    assert_eq!(t.ttft_ms(), None);
    assert_eq!(t.stream_ms(), None);
    assert_eq!(t.itl_max_ms(), None);
    t.record_delta();
    assert_eq!(t.count, 1);
    assert!(t.ttft_ms().is_some());
    assert!(t.stream_ms().is_some());
    assert_eq!(t.itl_max_ms(), None); // A gap needs at least 2 deltas
    t.record_delta();
    assert_eq!(t.count, 2);
    assert!(t.itl_max_ms().is_some());
}

#[test]
fn compaction_outcome_as_str_is_stable() {
    assert_eq!(CompactionOutcome::Success.as_ref(), "success");
    assert_eq!(CompactionOutcome::Truncated.as_ref(), "truncated");
    assert_eq!(CompactionOutcome::Deterministic.as_ref(), "deterministic");
    assert_eq!(CompactionOutcome::Transient.as_ref(), "transient");
    assert_eq!(CompactionOutcome::Degenerate.as_ref(), "degenerate");
    assert_eq!(CompactionOutcome::Failed.as_ref(), "failed");
}
