use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{ConversationReplay, ServedReply};
use crate::conversation::{ConversationId, ConversationKey, ConversationTracker};
use crate::conversation_script::{Conversation, MockToolCall, ScriptViolation};
use crate::failure::StatusFailure;
use crate::inference_request::{InferenceEndpoint, InferenceRequest};
use crate::model_reply::ModelReply;
use crate::tools::{PickedToolCall, Tool};

pub(super) const DISTILL_TOOLS: [&str; 2] = ["read_file", "run_terminal_command"];

fn respond(
    replay: &ConversationReplay,
    conversation: ConversationId,
    body: &Value,
) -> Option<ModelReply> {
    respond_with_headers(replay, conversation, HeaderMap::new(), body).map(|served| served.reply)
}

pub(super) fn respond_with_headers(
    replay: &ConversationReplay,
    conversation: ConversationId,
    mut headers: HeaderMap,
    body: &Value,
) -> Option<ServedReply> {
    let conversations = ConversationTracker::default();
    for earlier in 1..conversation.number() {
        conversations.assign(ConversationKey::SessionId(format!("session-{earlier}")));
    }
    headers.insert("x-grok-turn-idx", HeaderValue::from_static("1"));
    headers.insert(
        "x-grok-session-id",
        HeaderValue::from_str(&format!("session-{}", conversation.number())).unwrap(),
    );
    let request = InferenceRequest::new(
        &conversations,
        InferenceEndpoint::ChatCompletions,
        &headers,
        body,
    );
    replay.respond(&request)
}

pub(super) fn chat_body(system: &str, tools: &[&str], turns: Vec<Value>) -> Value {
    let mut messages = vec![json!({ "role": "system", "content": system })];
    messages.push(json!({ "role": "user", "content": "go" }));
    messages.extend(turns);
    let tools: Vec<Value> = tools
        .iter()
        .map(|name| json!({ "type": "function", "function": { "name": name } }))
        .collect();
    json!({ "model": "m", "messages": messages, "tools": tools })
}

pub(super) fn chat_result(call_id: &str, text: &str) -> Value {
    json!({ "role": "tool", "tool_call_id": call_id, "content": text })
}

fn chat_turn(reply: &str, user: &str) -> [Value; 2] {
    [
        json!({ "role": "assistant", "content": reply }),
        json!({ "role": "user", "content": user }),
    ]
}

fn call(call_id: &str, name: &str, arguments: Value) -> ModelReply {
    ModelReply::ToolCall {
        call_id: call_id.to_owned(),
        call: PickedToolCall {
            name: name.to_owned(),
            arguments,
        },
    }
}

pub(super) fn reply(text: &str) -> ModelReply {
    ModelReply::Text(text.to_owned())
}

pub(super) fn read_call() -> MockToolCall {
    MockToolCall::new(Tool::Read, json!({ "target_file": "a.rs" }))
}

pub(super) fn read_reply(call_id: &str) -> ModelReply {
    call(call_id, "read_file", json!({ "target_file": "a.rs" }))
}

pub(super) fn shell_call() -> MockToolCall {
    MockToolCall::new(Tool::Shell, json!({ "command": "ls" }))
}

pub(super) fn shell_reply(call_id: &str) -> ModelReply {
    call(
        call_id,
        "run_terminal_command",
        json!({ "command": "ls", "description": "ls" }),
    )
}

fn turn(n: usize) -> Vec<Value> {
    vec![json!({ "role": "user", "content": format!("turn {n}") })]
}

fn chat_scripts(
    conversations: Vec<Conversation>,
) -> impl Fn(ConversationId, Vec<Value>) -> ModelReply {
    let replay = ConversationReplay::default();
    replay.set(conversations);
    move |conversation, turns| {
        respond(
            &replay,
            conversation,
            &chat_body("s", &DISTILL_TOOLS, turns),
        )
        .unwrap()
    }
}

#[test]
fn calls_are_answered_in_order_until_their_results_arrive_then_the_reply() {
    let respond = chat_scripts(vec![
        Conversation::nth(1)
            .calls([read_call(), shell_call()])
            .reply("ALL-DONE"),
    ]);

    let first = respond(ConversationId::nth(1), vec![]);
    let second = respond(
        ConversationId::nth(1),
        vec![chat_result("call_mock_1_1", "file body")],
    );
    let third = respond(
        ConversationId::nth(1),
        vec![
            chat_result("call_mock_1_1", "file body"),
            chat_result("call_mock_1_2", "a.rs"),
        ],
    );

    assert_eq!(
        [
            read_reply("call_mock_1_1"),
            shell_reply("call_mock_1_2"),
            reply("ALL-DONE"),
        ],
        [first, second, third]
    );
}

#[test]
fn request_after_a_reply_starts_the_next_entry() {
    let respond = chat_scripts(vec![
        Conversation::nth(1)
            .calls([read_call()])
            .reply("ONE")
            .calls([shell_call()])
            .reply("TWO"),
    ]);
    let first_turn_done = vec![chat_result("call_mock_1_1", "file body")];
    let second_turn = [first_turn_done.clone(), chat_turn("ONE", "more").to_vec()].concat();
    let second_turn_done = [
        second_turn.clone(),
        vec![chat_result("call_mock_1_2", "a.rs")],
    ]
    .concat();

    let answers = [
        respond(ConversationId::nth(1), vec![]),
        respond(ConversationId::nth(1), first_turn_done),
        respond(ConversationId::nth(1), second_turn),
        respond(ConversationId::nth(1), second_turn_done),
    ];

    assert_eq!(
        [
            read_reply("call_mock_1_1"),
            reply("ONE"),
            shell_reply("call_mock_1_2"),
            reply("TWO"),
        ],
        answers
    );
}

#[test]
fn last_reply_repeats_once_the_script_is_served_through() {
    let respond = chat_scripts(vec![Conversation::nth(1).reply("ONE")]);

    let first = respond(ConversationId::nth(1), vec![]);
    let next_turn = respond(ConversationId::nth(1), chat_turn("ONE", "more").to_vec());

    assert_eq!([reply("ONE"), reply("ONE")], [first, next_turn]);
}

#[test]
fn request_posted_again_gets_the_reply_it_already_took() {
    let respond = chat_scripts(vec![Conversation::nth(1).reply("ONE").reply("TWO")]);

    let first = respond(ConversationId::nth(1), vec![]);
    let retried = respond(ConversationId::nth(1), vec![]);
    let next_turn = respond(ConversationId::nth(1), chat_turn("ONE", "more").to_vec());

    assert_eq!(
        [reply("ONE"), reply("ONE"), reply("TWO")],
        [first, retried, next_turn]
    );
}

#[test]
fn resent_request_repeats_its_reply_and_does_not_advance_to_the_pinned_turn() {
    let respond = chat_scripts(vec![
        Conversation::nth(1).reply("A").at_request(2).reply("X"),
    ]);

    let first = respond(ConversationId::nth(1), vec![]);
    let retried = respond(ConversationId::nth(1), vec![]);
    let next_request = respond(ConversationId::nth(1), chat_turn("A", "more").to_vec());

    assert_eq!(
        [reply("A"), reply("A"), reply("X")],
        [first, retried, next_request]
    );
}

#[test]
fn resent_tool_call_repeats_it_and_a_later_pin_still_lands_on_its_request() {
    let respond = chat_scripts(vec![
        Conversation::nth(1)
            .calls([read_call()])
            .reply("A")
            .at_request(2)
            .reply("X"),
    ]);

    let first = respond(ConversationId::nth(1), vec![]);
    let resent = respond(ConversationId::nth(1), vec![]);
    let next_request = respond(
        ConversationId::nth(1),
        vec![chat_result("call_mock_1_1", "file body")],
    );

    assert_eq!(
        [
            read_reply("call_mock_1_1"),
            read_reply("call_mock_1_1"),
            reply("X"),
        ],
        [first, resent, next_request]
    );
}

#[test]
fn results_of_another_conversation_do_not_advance_the_script() {
    let respond = chat_scripts(vec![
        Conversation::nth(2).calls([shell_call()]).reply("done"),
    ]);
    let answer = respond(
        ConversationId::nth(2),
        vec![chat_result("call_mock_1_1", "inherited from the parent")],
    );
    assert_eq!(shell_reply("call_mock_2_1"), answer);
}

#[test]
fn call_the_request_does_not_offer_is_recorded_and_ends_its_entry_with_the_reply() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1)
            .calls([read_call()])
            .reply("ONE")
            .reply("TWO"),
    ]);
    let respond = |turns: Vec<Value>| {
        respond(
            &replay,
            ConversationId::nth(1),
            &chat_body("s", &["run_terminal_command"], turns),
        )
        .unwrap()
    };

    let first = respond(vec![]);
    let second = respond(chat_turn("ONE", "more").to_vec());

    assert_eq!(
        (
            reply("ONE"),
            reply("TWO"),
            vec![ScriptViolation::ToolNotOffered {
                conversation: 1,
                call_id: "call_mock_1_1".to_owned(),
                tool: Tool::Read,
                offered: vec!["run_terminal_command".to_owned()],
            }]
        ),
        (first, second, replay.violations())
    );
}

#[test]
fn replacing_the_scripts_keeps_a_replaced_script_unfinished() {
    let replay = ConversationReplay::default();
    replay.set(vec![Conversation::nth(1).reply("ONE").reply("TWO")]);
    respond(
        &replay,
        ConversationId::nth(1),
        &chat_body("s", &DISTILL_TOOLS, vec![]),
    );

    replay.set(vec![Conversation::nth(2).reply("child")]);

    assert_eq!(
        vec![
            ScriptViolation::Unfinished {
                conversation: 1,
                replies_served: 1,
                superseded: 0,
                entry_count: 2,
            },
            ScriptViolation::Unfinished {
                conversation: 2,
                replies_served: 0,
                superseded: 0,
                entry_count: 1,
            },
        ],
        replay.violations()
    );
}

#[test]
fn script_for_a_system_prompt_finds_its_conversation_whatever_the_arrival_order() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::for_system_prompt("reviewer").reply("REVIEWED"),
        Conversation::for_system_prompt("tester").reply("TESTED"),
    ]);

    let answers = [(2, "tester"), (3, "reviewer")].map(|(number, system)| {
        respond(
            &replay,
            ConversationId::nth(number),
            &chat_body(system, &DISTILL_TOOLS, vec![]),
        )
        .unwrap()
    });

    assert_eq!(
        ([reply("TESTED"), reply("REVIEWED")], Vec::new()),
        (answers, replay.violations())
    );
}

#[test]
fn script_no_conversation_opens_falls_through_and_is_reported_unopened() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::for_system_prompt("tester").reply("TESTED"),
    ]);
    let answer = respond(
        &replay,
        ConversationId::nth(2),
        &chat_body("reviewer prompt", &DISTILL_TOOLS, vec![]),
    );
    assert_eq!(
        (
            None,
            vec![ScriptViolation::Unopened {
                expected: "tester".to_owned(),
            }]
        ),
        (answer, replay.violations())
    );
}

#[test]
fn result_check_records_a_mismatch_once_and_only_when_it_fails() {
    let cases = [
        ("blocked by hook", Vec::new()),
        (
            "removed everything",
            vec![ScriptViolation::ResultMismatch {
                conversation: 1,
                call_id: "call_mock_1_1".to_owned(),
                expected: "blocked by hook".to_owned(),
                result: "removed everything".to_owned(),
            }],
        ),
    ];
    for (result, expected) in cases {
        let replay = ConversationReplay::default();
        replay.set(vec![
            Conversation::nth(1)
                .calls([
                    MockToolCall::new(Tool::Shell, json!({ "command": "rm -rf /" }))
                        .result_contains("blocked by hook"),
                ])
                .reply("done"),
        ]);
        let respond = |turns: Vec<Value>| {
            respond(
                &replay,
                ConversationId::nth(1),
                &chat_body("s", &DISTILL_TOOLS, turns),
            )
        };

        respond(vec![]);
        respond(vec![chat_result("call_mock_1_1", result)]);
        respond(vec![chat_result("call_mock_1_1", result)]);

        assert_eq!(expected, replay.violations(), "{result}");
    }
}

#[test]
fn result_matches_reports_its_pattern_when_the_result_does_not_match() {
    let cases = [
        ("/repo/src/a.rs", Vec::new()),
        (
            "blocked by hook",
            vec![ScriptViolation::ResultMismatch {
                conversation: 1,
                call_id: "call_mock_1_1".to_owned(),
                expected: r"/.*\.rs$".to_owned(),
                result: "blocked by hook".to_owned(),
            }],
        ),
    ];
    for (result, expected) in cases {
        let replay = ConversationReplay::default();
        replay.set(vec![
            Conversation::nth(1)
                .calls([read_call().result_matches(r"/.*\.rs$").unwrap()])
                .reply("done"),
        ]);
        let respond = |turns: Vec<Value>| {
            respond(
                &replay,
                ConversationId::nth(1),
                &chat_body("s", &DISTILL_TOOLS, turns),
            )
        };

        respond(vec![]);
        respond(vec![chat_result("call_mock_1_1", result)]);

        assert_eq!(expected, replay.violations(), "{result}");
    }
}

#[test]
#[should_panic(expected = "duplicate script for conversation 1")]
fn two_scripts_for_one_conversation_panic_at_registration() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1).reply("a"),
        Conversation::nth(1).reply("b"),
    ]);
}

#[test]
#[should_panic(expected = "conversation 1: an open turn after the last reply")]
fn calls_no_reply_closed_panic_at_registration() {
    let replay = ConversationReplay::default();
    replay.set(vec![Conversation::nth(1).reply("ONE").calls([read_call()])]);
}

#[test]
#[should_panic(expected = "conversation 1: an open turn after the last reply")]
fn failure_setter_after_the_closing_reply_panics_at_registration() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1)
            .reply("A")
            .refuse(StatusFailure::new(500)),
    ]);
}

#[test]
fn pinned_turn_starts_there_and_hands_over_to_the_next_unpinned_turn() {
    let respond = chat_scripts(vec![
        Conversation::nth(1)
            .reply("A")
            .at_request(2)
            .reply("X")
            .reply("B"),
    ]);

    let replies: Vec<ModelReply> = (1..=4)
        .map(|n| respond(ConversationId::nth(1), turn(n)))
        .collect();

    assert_eq!(
        vec![reply("A"), reply("X"), reply("B"), reply("B")],
        replies
    );
}

#[test]
fn pinned_turn_with_no_turn_after_it_keeps_answering() {
    let respond = chat_scripts(vec![
        Conversation::nth(1).reply("A").at_request(2).reply("X"),
    ]);

    let replies: Vec<ModelReply> = (1..=3)
        .map(|n| respond(ConversationId::nth(1), turn(n)))
        .collect();

    assert_eq!(vec![reply("A"), reply("X"), reply("X")], replies);
}

#[test]
fn pinned_turn_with_tool_calls_serves_them_then_its_reply() {
    let respond = chat_scripts(vec![
        Conversation::nth(1)
            .calls([read_call()])
            .reply("PLANNED")
            .at_request(3)
            .calls([shell_call()])
            .reply("DONE"),
    ]);
    let read_done = vec![chat_result("call_mock_1_1", "plan")];
    let approved = [read_done.clone(), turn(3)].concat();
    let shell_done = [approved.clone(), vec![chat_result("call_mock_1_2", "")]].concat();

    let answers = [
        respond(ConversationId::nth(1), vec![]),
        respond(ConversationId::nth(1), read_done),
        respond(ConversationId::nth(1), approved),
        respond(ConversationId::nth(1), shell_done),
    ];

    assert_eq!(
        [
            read_reply("call_mock_1_1"),
            reply("PLANNED"),
            shell_reply("call_mock_1_2"),
            reply("DONE"),
        ],
        answers
    );
}

#[test]
fn script_whose_first_turn_is_pinned_falls_through_before_it_starts() {
    let replay = ConversationReplay::default();
    replay.set(vec![Conversation::nth(1).at_request(2).reply("X")]);
    let body = chat_body("s", &DISTILL_TOOLS, vec![]);

    let first = respond(&replay, ConversationId::nth(1), &body);
    let second = respond(&replay, ConversationId::nth(1), &body);

    assert_eq!((None, Some(reply("X"))), (first, second));
}

#[test]
fn pinned_turn_is_served_through_before_the_script_is_finished() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1).reply("A").at_request(2).reply("X"),
    ]);

    respond(
        &replay,
        ConversationId::nth(1),
        &chat_body("s", &DISTILL_TOOLS, turn(1)),
    );
    let after_one = replay.violations();
    respond(
        &replay,
        ConversationId::nth(1),
        &chat_body("s", &DISTILL_TOOLS, turn(2)),
    );
    let after_two = replay.violations();

    assert_eq!(
        (
            vec![ScriptViolation::Unfinished {
                conversation: 1,
                replies_served: 1,
                superseded: 0,
                entry_count: 2,
            }],
            Vec::new()
        ),
        (after_one, after_two)
    );
}

#[test]
fn pin_taking_over_a_turn_still_calling_leaves_no_unfinished_violation() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1)
            .calls([read_call()])
            .reply("A")
            .at_request(2)
            .reply("X"),
    ]);

    let calling = respond(
        &replay,
        ConversationId::nth(1),
        &chat_body("s", &DISTILL_TOOLS, vec![]),
    );
    let taken_over = respond(
        &replay,
        ConversationId::nth(1),
        &chat_body(
            "s",
            &DISTILL_TOOLS,
            vec![chat_result("call_mock_1_1", "file body")],
        ),
    );

    assert_eq!(
        (
            Some(read_reply("call_mock_1_1")),
            Some(reply("X")),
            Vec::new()
        ),
        (calling, taken_over, replay.violations())
    );
}

#[test]
fn pin_before_a_calling_turn_still_reports_the_trailing_turn_unfinished() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1)
            .at_request(2)
            .reply("PIN")
            .calls([read_call()])
            .reply("A")
            .reply("B"),
    ]);
    let respond = |turns: Vec<Value>| {
        respond(
            &replay,
            ConversationId::nth(1),
            &chat_body("s", &DISTILL_TOOLS, turns),
        )
    };
    let result = || vec![chat_result("call_mock_1_1", "file body")];

    let calling = respond(vec![]);
    let taken_over = respond(result());
    let resumed = respond([result(), chat_turn("PIN", "more").to_vec()].concat());

    assert_eq!(
        (
            [
                Some(read_reply("call_mock_1_1")),
                Some(reply("PIN")),
                Some(reply("A")),
            ],
            vec![ScriptViolation::Unfinished {
                conversation: 1,
                replies_served: 1,
                superseded: 1,
                entry_count: 3,
            }],
        ),
        ([calling, taken_over, resumed], replay.violations())
    );
}

#[test]
#[should_panic(expected = "two turns of the script for conversation 1 are pinned to request 2")]
fn two_turns_pinned_to_one_request_panic_at_registration() {
    let replay = ConversationReplay::default();
    replay.set(vec![
        Conversation::nth(1)
            .reply("a")
            .at_request(2)
            .reply("x")
            .at_request(2)
            .reply("y"),
    ]);
}

#[test]
#[should_panic(expected = "requests are counted from 1")]
fn turn_pinned_to_request_zero_panics_at_construction() {
    let _ = Conversation::nth(1).at_request(0);
}
