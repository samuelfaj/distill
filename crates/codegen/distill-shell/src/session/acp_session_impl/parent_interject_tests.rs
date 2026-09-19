// Modified for Distill by Samuel Fajreldines, 2026.
use super::super::parent_message::tests::delivery_message;
use super::super::turn_report_slot::TurnReportSlot;
use super::{ParentInterjectSignal, order_for_delivery};
use distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation;

#[test]
fn interjects_precede_steers_and_each_lane_keeps_admission_order() {
    let messages = [
        delivery_message("s1", ActiveAgentMessageOperation::Steer),
        delivery_message("i1", ActiveAgentMessageOperation::Interject),
        delivery_message("i2", ActiveAgentMessageOperation::Interject),
        delivery_message("s2", ActiveAgentMessageOperation::Steer),
    ];
    assert_eq!(
        order_for_delivery(&messages)
            .map(|message| message.identity().as_str())
            .collect::<Vec<_>>(),
        ["i1", "i2", "s1", "s2"]
    );
}

#[test]
fn a_late_note_from_an_earlier_turn_does_not_replace_the_live_turns_mark() {
    let turns = TurnReportSlot::default();
    let earlier = turns.start_next_turn();
    let live = turns.start_next_turn();
    let signal = ParentInterjectSignal::default();

    signal.note_wait_aborted(live);
    signal.note_wait_aborted(earlier);

    assert_eq!(
        [true, false],
        [
            signal.take_wait_aborted(live),
            signal.take_wait_aborted(earlier)
        ]
    );
}
