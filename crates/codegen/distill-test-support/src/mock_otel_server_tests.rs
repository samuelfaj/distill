use std::time::Duration;

use reqwest::Method;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;

use super::*;
use crate::loopback_client::send;
use crate::otel_event::{OtelBody, OtelDecodeError, OtelEvent};
use crate::otel_fixtures::{logs_body, logs_event};
use crate::otel_recorder::OtelRecorderError;

const PROTOBUF: &str = "application/x-protobuf";

async fn post(server: &MockOtelServer, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> u16 {
    send(
        Method::POST,
        &format!("{}{path}", server.origin()),
        headers,
        body,
    )
    .await
    .status()
    .as_u16()
}

async fn post_protobuf(server: &MockOtelServer, path: &str, body: Vec<u8>) -> u16 {
    post(server, path, &[("content-type", PROTOBUF)], body).await
}

#[tokio::test]
async fn undecodable_body_is_kept_as_a_fault_and_still_acknowledged() {
    let cases = [
        (
            "/v1/logs",
            PROTOBUF,
            b"\xff\xff\xff".to_vec(),
            OtelSignal::Logs,
            None,
        ),
        (
            "/v1/metrics",
            "application/json",
            b"{}".to_vec(),
            OtelSignal::Metrics,
            Some("application/json"),
        ),
    ];

    for (path, content_type, body, expected_signal, refused_content_type) in cases {
        let server = MockOtelServer::start().await.unwrap();

        let status = post(
            &server,
            path,
            &[("content-type", content_type)],
            body.clone(),
        )
        .await;

        let faults = server.recorder().faults();
        let [OtelFault::Undecodable { signal, len, error }] = faults.as_slice() else {
            panic!("expected one undecodable fault, got {faults:?}");
        };
        let content_type_refused = match error {
            OtelDecodeError::UnsupportedContentType { content_type } => Some(content_type.as_str()),
            OtelDecodeError::Protobuf { .. } => None,
        };
        assert_eq!(
            (200, expected_signal, body.len(), refused_content_type),
            (status, *signal, *len, content_type_refused),
            "{content_type}"
        );
    }
}

#[tokio::test]
async fn body_over_the_limit_is_kept_as_refused() {
    let server = MockOtelServer::start_with_body_limit(16).await.unwrap();
    let body = logs_body("grok_code.session_start");

    let status = post_protobuf(&server, "/v1/logs", body.clone()).await;

    assert_eq!(413, status);
    let faults = server.recorder().faults();
    let [OtelFault::Unread { body: refused, .. }] = faults.as_slice() else {
        panic!("expected one unread fault, got {faults:?}");
    };
    assert_eq!(
        OtelUnreadBody::Refused {
            declared_len: Some(body.len())
        },
        *refused
    );
}

#[tokio::test]
async fn export_log_keeps_lowercase_headers_and_the_body() {
    let server = MockOtelServer::start().await.unwrap();
    let body = logs_body("grok_code.user_prompt");
    post(
        &server,
        "/v1/logs",
        &[("content-type", PROTOBUF), ("X-Collector-Token", "abc")],
        body.clone(),
    )
    .await;

    let exports = server.recorder().exports();

    let [export] = exports.as_slice() else {
        panic!("expected one export, got {exports:?}");
    };
    assert_eq!(
        (
            OtelSignal::Logs,
            &OtelBody::Kept(body),
            true,
            Some("abc"),
            None
        ),
        (
            export.signal,
            &export.body,
            export
                .headers
                .contains(&("x-collector-token".to_owned(), "abc".to_owned())),
            export.header("X-Collector-Token"),
            export.header("authorization"),
        )
    );
}

#[tokio::test]
async fn wait_for_events_resolves_when_a_later_export_satisfies_the_predicate() {
    let server = MockOtelServer::start().await.unwrap();

    let (events, status) = tokio::join!(
        server.recorder().wait_for_events(Duration::from_secs(5), |events| {
            events.iter().any(|event| {
                matches!(event, OtelEvent::LogRecord(record) if record.event_name == "grok_code.user_prompt")
            })
        }),
        post_protobuf(&server, "/v1/logs", logs_body("grok_code.user_prompt")),
    );

    assert_eq!(
        (200, Ok(vec![logs_event("grok_code.user_prompt")])),
        (status, events)
    );
}

#[tokio::test]
async fn request_the_collector_does_not_serve_is_refused_and_fails_every_later_wait() {
    let grpc_path = "/opentelemetry.proto.collector.logs.v1.LogsService/Export";
    let cases = [
        (Method::POST, grpc_path, 404),
        (Method::GET, "/v1/logs", 405),
    ];

    for (method, path, expected_status) in cases {
        let server = MockOtelServer::start().await.unwrap();
        let status = send(
            method.clone(),
            &format!("{}{path}", server.origin()),
            &[("content-type", PROTOBUF)],
            logs_body("grok_code.user_prompt"),
        )
        .await
        .status()
        .as_u16();
        let outcome = server
            .recorder()
            .wait_for_events(Duration::from_secs(5), |_| true)
            .await;

        assert_eq!(
            (
                expected_status,
                Err(OtelRecorderError::Fault(OtelFault::Unserved {
                    method: method.to_string(),
                    path: path.to_owned(),
                }))
            ),
            (status, outcome)
        );
    }
}

#[tokio::test]
async fn body_cut_off_by_the_client_is_kept_as_incomplete() {
    let server = MockOtelServer::start().await.unwrap();
    let mut client = TcpStream::connect(server.server.addr()).await.unwrap();
    let head_and_partial_body = format!(
        "POST /v1/logs HTTP/1.1\r\nhost: server\r\ncontent-type: {PROTOBUF}\r\n\
         content-length: 64\r\n\r\npartial"
    );
    client
        .write_all(head_and_partial_body.as_bytes())
        .await
        .unwrap();
    drop(client);

    let error = server
        .recorder()
        .wait_for_events(Duration::from_secs(5), |_| false)
        .await
        .unwrap_err();

    let OtelRecorderError::Fault(OtelFault::Unread { body, .. }) = error else {
        panic!("expected an unread fault, got {error}");
    };
    assert_eq!(
        OtelUnreadBody::Incomplete {
            declared_len: Some(64)
        },
        body
    );
}
