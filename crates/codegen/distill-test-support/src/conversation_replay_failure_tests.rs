use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::tests::{
    DISTILL_TOOLS, chat_body, chat_result, read_call, read_reply, reply, respond_with_headers,
    shell_call, shell_reply,
};
use super::{ConversationReplay, ServedReply};
use crate::conversation::ConversationId;
use crate::conversation_script::{Conversation, ScriptViolation};
use crate::failure::{
    DOOM_LOOP_CHECK_HEADER, ErrorPosition, ObservedFailure, StatusFailure, StreamError,
};
use crate::model_reply::ModelReply;
use crate::tools::Tool;

const HOLD: Duration = Duration::from_secs(30);

fn replay_for(conversation: Conversation) -> ConversationReplay {
    let replay = ConversationReplay::default();
    replay.set(vec![conversation]);
    replay
}

fn respond(replay: &ConversationReplay, turns: Vec<Value>) -> ServedReply {
    respond_with_headers(
        replay,
        ConversationId::nth(1),
        HeaderMap::new(),
        &chat_body("s", &DISTILL_TOOLS, turns),
    )
    .unwrap()
}

fn observed(served: &[ServedReply]) -> Vec<Option<ObservedFailure>> {
    served.iter().map(|served| served.observed).collect()
}

#[test]
fn status_failure_refuses_its_count_of_requests_then_the_script_answers() {
    let replay = replay_for(
        Conversation::nth(1)
            .refuse(
                StatusFailure::new(503)
                    .with_count(2)
                    .with_retry_after(Duration::from_secs(8))
                    .with_body("upstream down"),
            )
            .reply("PONG"),
    );

    let served = [
        respond(&replay, vec![]),
        respond(&replay, vec![]),
        respond(&replay, vec![]),
    ];

    assert_eq!(
        vec![
            Some(ObservedFailure::Status(503)),
            Some(ObservedFailure::Status(503)),
            None
        ],
        observed(&served)
    );
    let refusal = || {
        ModelReply::Refusal(
            StatusFailure::new(503)
                .with_count(2)
                .with_retry_after(Duration::from_secs(8))
                .with_body("upstream down"),
        )
    };
    assert_eq!(
        [refusal(), refusal(), reply("PONG")],
        served.map(|served| served.reply)
    );
}

#[test]
fn retry_after_a_status_failure_meets_the_same_script_position() {
    let replay = replay_for(
        Conversation::nth(1)
            .calls([read_call()])
            .refuse(StatusFailure::new(503))
            .reply("DONE"),
    );

    let refused = respond(&replay, vec![]);
    let retried = respond(&replay, vec![]);

    assert_eq!(
        (
            Some(ObservedFailure::Status(503)),
            read_reply("call_mock_1_1")
        ),
        (refused.observed, retried.reply)
    );
}

#[test]
fn cut_turn_answers_the_first_request_short_then_its_reply() {
    let replay = replay_for(Conversation::nth(1).cut(1).reply("PONG"));

    let cut = respond(&replay, vec![]);
    let full = respond(&replay, vec![]);

    assert_eq!(
        (
            (Some(ObservedFailure::Cut), ModelReply::CutReply),
            (None, reply("PONG"))
        ),
        ((cut.observed, cut.reply), (full.observed, full.reply))
    );
}

#[test]
fn stream_error_fails_its_count_of_streams_then_the_script_answers() {
    let stream_error = StreamError::new()
        .with_count(2)
        .with_message("at capacity")
        .with_position(ErrorPosition::Midway);
    let replay = replay_for(
        Conversation::nth(1)
            .fail_stream(stream_error.clone())
            .reply("PONG"),
    );

    let served: Vec<ServedReply> = (0..3).map(|_| respond(&replay, vec![])).collect();

    assert_eq!(
        vec![
            Some(ObservedFailure::StreamError),
            Some(ObservedFailure::StreamError),
            None
        ],
        observed(&served)
    );
    assert_eq!(
        ModelReply::StreamError(stream_error),
        served.into_iter().next().unwrap().reply
    );
}

#[test]
fn doom_loop_repeats_the_first_tool_call_under_fresh_ids_then_the_script_goes_on() {
    let replay = replay_for(
        Conversation::nth(1)
            .calls([shell_call()])
            .doom_loop(2)
            .reply("DONE"),
    );
    let looped_once = vec![chat_result("call_mock_1_1_loop1", "a.rs")];
    let looped_twice = [
        looped_once.clone(),
        vec![chat_result("call_mock_1_1_loop2", "a.rs")],
    ]
    .concat();
    let done = [
        looped_twice.clone(),
        vec![chat_result("call_mock_1_1", "a.rs")],
    ]
    .concat();

    let served = [
        respond(&replay, vec![]),
        respond(&replay, looped_once),
        respond(&replay, looped_twice),
        respond(&replay, done),
    ];

    assert_eq!(
        vec![
            Some(ObservedFailure::DoomLoop),
            Some(ObservedFailure::DoomLoop),
            None,
            None
        ],
        observed(&served)
    );
    assert_eq!(
        [
            shell_reply("call_mock_1_1_loop1"),
            shell_reply("call_mock_1_1_loop2"),
            shell_reply("call_mock_1_1"),
            reply("DONE"),
        ],
        served.map(|served| served.reply)
    );
}

#[test]
fn doom_loop_without_tool_calls_streams_the_looping_reply_and_reports_when_asked() {
    let replay = replay_for(Conversation::nth(1).doom_loop(1).reply("PONG"));
    let mut asking = HeaderMap::new();
    asking.insert(DOOM_LOOP_CHECK_HEADER, HeaderValue::from_static("4"));
    let body = chat_body("s", &DISTILL_TOOLS, vec![]);

    let looping = respond_with_headers(&replay, ConversationId::nth(1), asking, &body).unwrap();
    let recovered = respond(&replay, vec![]);

    assert_eq!(
        (
            (
                Some(ObservedFailure::DoomLoop),
                ModelReply::LoopingReply { reported: true }
            ),
            (None, reply("PONG"))
        ),
        (
            (looping.observed, looping.reply),
            (recovered.observed, recovered.reply)
        )
    );
}

#[test]
fn doom_loop_whose_first_call_is_not_offered_streams_the_looping_reply_and_is_recorded() {
    let replay = replay_for(
        Conversation::nth(1)
            .calls([read_call()])
            .doom_loop(1)
            .reply("DONE"),
    );

    let respond = |turns: Vec<Value>| {
        respond_with_headers(
            &replay,
            ConversationId::nth(1),
            HeaderMap::new(),
            &chat_body("s", &["run_terminal_command"], turns),
        )
        .unwrap()
    };
    let not_offered = |call_id: &str| ScriptViolation::ToolNotOffered {
        conversation: 1,
        call_id: call_id.to_owned(),
        tool: Tool::Read,
        offered: vec!["run_terminal_command".to_owned()],
    };

    let looping = respond(vec![]);
    let ended = respond(vec![json!({ "role": "user", "content": "again" })]);

    assert_eq!(
        (
            ModelReply::LoopingReply { reported: false },
            reply("DONE"),
            vec![
                not_offered("call_mock_1_1_loop1"),
                not_offered("call_mock_1_1")
            ]
        ),
        (looping.reply, ended.reply, replay.violations())
    );
}

#[test]
fn failures_answer_in_the_order_the_case_declared_them() {
    let replay = replay_for(
        Conversation::nth(1)
            .cut(1)
            .refuse(StatusFailure::new(503))
            .drop_connection(1)
            .reply("PONG"),
    );

    let served: Vec<ServedReply> = (0..4).map(|_| respond(&replay, vec![])).collect();

    assert_eq!(
        vec![
            Some(ObservedFailure::Cut),
            Some(ObservedFailure::Status(503)),
            Some(ObservedFailure::Dropped),
            None
        ],
        observed(&served)
    );
}

#[test]
fn drop_closes_its_count_of_connections_then_the_script_answers() {
    let replay = replay_for(Conversation::nth(1).drop_connection(1).reply("PONG"));

    let dropped = respond(&replay, vec![]);
    let answered = respond(&replay, vec![]);

    assert_eq!(
        (
            (Some(ObservedFailure::Dropped), ModelReply::Dropped),
            (None, reply("PONG"))
        ),
        (
            (dropped.observed, dropped.reply),
            (answered.observed, answered.reply)
        )
    );
}

#[test]
fn stall_holds_the_answer_and_is_recorded_unless_another_failure_is() {
    let stalled = replay_for(Conversation::nth(1).stall(HOLD).reply("PONG"));
    let stalled_refusal = replay_for(
        Conversation::nth(1)
            .stall(HOLD)
            .refuse(StatusFailure::new(503))
            .reply("PONG"),
    );

    let plain = respond(&stalled, vec![]);
    let refused = respond(&stalled_refusal, vec![]);

    assert_eq!(
        (
            (Some(HOLD), Some(ObservedFailure::Stalled)),
            (Some(HOLD), Some(ObservedFailure::Status(503)))
        ),
        (
            (plain.hold, plain.observed),
            (refused.hold, refused.observed)
        )
    );
}

#[test]
fn retried_request_keeps_the_hold_of_the_turn_that_answered_it() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1).reply("PONG").stall(HOLD).reply("NEXT"),
    ]);

    let first = respond(&replay, vec![]);
    let retried = respond(&replay, vec![]);

    assert_eq!(
        [(None, None), (None, None)],
        [first, retried].map(|served| (served.hold, served.observed))
    );
}

#[test]
fn pin_taking_over_a_mid_failure_turn_supersedes_it_and_still_reports_the_trailing_turn() {
    let replay = replay_for(
        Conversation::nth(1)
            .refuse(StatusFailure::new(503))
            .reply("A")
            .at_request(2)
            .reply("B")
            .reply("C"),
    );

    let refused = respond(&replay, vec![]);
    let taken_over = respond(&replay, vec![]);

    assert_eq!(
        (
            Some(ObservedFailure::Status(503)),
            reply("B"),
            vec![ScriptViolation::Unfinished {
                conversation: 1,
                replies_served: 1,
                superseded: 1,
                entry_count: 3,
            }],
        ),
        (refused.observed, taken_over.reply, replay.violations())
    );
}

#[test]
#[should_panic(expected = "invalid failure status 99")]
fn status_failure_with_an_invalid_status_panics_at_construction() {
    let _ = StatusFailure::new(99);
}
