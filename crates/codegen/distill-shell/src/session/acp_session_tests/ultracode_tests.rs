// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode: the per-turn reminder is pushed only while the flag is on, and effort is untouched.
use super::support::*;
use super::*;

#[tokio::test]
async fn ultracode_reminder_only_when_on_and_effort_untouched() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            let effort_before = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|cfg| cfg.reasoning_effort);

            actor.inject_ultracode_reminder();
            assert_eq!(
                actor.chat_state_handle.get_conversation_len().await,
                0,
                "off must push nothing"
            );

            actor.ultracode.store(true, std::sync::atomic::Ordering::Relaxed);
            actor.inject_ultracode_reminder();
            let conv = actor.chat_state_handle.get_conversation().await;
            assert_eq!(conv.len(), 1);
            let text = conv[0].text_content();
            assert!(text.contains("<system-reminder>"), "{text}");
            assert!(text.contains("Ultracode mode is ON"), "{text}");

            actor.ultracode.store(false, std::sync::atomic::Ordering::Relaxed);
            actor.inject_ultracode_reminder();
            assert_eq!(
                actor.chat_state_handle.get_conversation_len().await,
                1,
                "off again must push nothing more"
            );

            let effort_after = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|cfg| cfg.reasoning_effort);
            assert_eq!(effort_before, effort_after, "Ultracode must not touch effort");
        })
        .await;
}
