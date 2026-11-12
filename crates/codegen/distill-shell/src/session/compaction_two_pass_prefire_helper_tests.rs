// Modified for Distill by Samuel Fajreldines, 2026.
use super::{fingerprint_prefix, prefire_lead_percent};
use distill_sampling_types::ConversationItem;

#[test]
fn fingerprint_stable_for_same_prefix() {
    let items = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hello"),
        ConversationItem::assistant("hi"),
    ];
    assert_eq!(fingerprint_prefix(&items), fingerprint_prefix(&items));
}

#[test]
fn fingerprint_changes_when_prefix_content_changes() {
    let base = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("hello"),
    ];
    let edited = vec![
        ConversationItem::system("sys"),
        ConversationItem::user("HELLO there"), // The changed text stands in for a real edit or rewind of the prefix
    ];
    assert_ne!(
        fingerprint_prefix(&base),
        fingerprint_prefix(&edited),
        "a changed prefix must invalidate the cached NOTE1 fingerprint"
    );
}

#[test]
fn fingerprint_changes_with_length() {
    let short = vec![ConversationItem::user("a")];
    let long = vec![
        ConversationItem::user("a"),
        ConversationItem::assistant("b"),
    ];
    assert_ne!(fingerprint_prefix(&short), fingerprint_prefix(&long));
}

/// History eviction and the hard clear shorten an old tool result in place
/// after pass 1 ran; the NOTE1 written from the fuller copy still summarizes
/// it, so the main-model pass 1 is not thrown away. A result for another call
/// (a rewind and a new branch) still invalidates it.
#[test]
fn fingerprint_survives_a_tool_result_shortened_in_place() {
    let with = |id: &str, content: &str| {
        vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
            ConversationItem::tool_result(id, content.to_owned()),
        ]
    };
    let full = with("call-1", &"cargo output line\n".repeat(500));
    let evicted = with("call-1", "[evicted from history 25 rounds after this call: stored at /s/x]");
    assert_eq!(fingerprint_prefix(&full), fingerprint_prefix(&evicted));
    assert_ne!(fingerprint_prefix(&full), fingerprint_prefix(&with("call-2", "x")));
}

/// A prompt refresh (the worker, the label, the memory section) rewrites a
/// system item in place; the conversation NOTE1 summarized is the same, so a
/// prepared pass 1 stays usable. A changed message still invalidates it.
#[test]
fn fingerprint_survives_a_rewritten_system_prompt() {
    let with = |head: &str, ask: &str| {
        vec![
            ConversationItem::system(head),
            ConversationItem::user(ask),
            ConversationItem::assistant("done"),
        ]
    };
    let before = with("worker: model-a", "fix the parser");
    assert_eq!(
        fingerprint_prefix(&before),
        fingerprint_prefix(&with("worker: model-b", "fix the parser"))
    );
    assert_ne!(
        fingerprint_prefix(&before),
        fingerprint_prefix(&with("worker: model-a", "rename the module"))
    );
}

#[test]
fn prefire_lead_percent_defaults_to_10() {
    // SAFETY: single-threaded test mutation of our own env var.
    unsafe { std::env::remove_var("GROK_PREFIRE_LEAD_PERCENT") };
    assert_eq!(prefire_lead_percent(), 10);
}
