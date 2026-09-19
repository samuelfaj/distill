use std::pin::pin;

use agent_client_protocol as acp;
use futures_util::FutureExt as _;

use super::HoldRegistry;

#[tokio::test]
async fn hold_is_released_only_by_a_later_release_of_its_own_session() {
    let registry = HoldRegistry::default();
    let session_id = acp::SessionId::new("s1");
    registry.release_held_requests(&session_id).await;
    let mut held = pin!(registry.hold_until_released(&session_id));

    assert!(held.as_mut().now_or_never().is_none());
    registry
        .release_held_requests(&acp::SessionId::new("other"))
        .await;
    assert!(held.as_mut().now_or_never().is_none());

    let mut releasing = pin!(registry.release_held_requests(&session_id));
    assert_eq!(None, releasing.as_mut().now_or_never());
    assert!(held.now_or_never().is_some());
}

#[tokio::test]
async fn release_held_requests_resolves_only_once_the_released_hold_is_dropped() {
    let registry = HoldRegistry::default();
    let session_id = acp::SessionId::new("s1");
    let mut held = pin!(registry.hold_until_released(&session_id));
    assert!(held.as_mut().now_or_never().is_none());
    let mut releasing = pin!(registry.release_held_requests(&session_id));
    assert_eq!(None, releasing.as_mut().now_or_never());

    let released = held.await;
    assert_eq!(None, releasing.as_mut().now_or_never());

    drop(released);
    assert_eq!(Some(()), releasing.now_or_never());
}

#[tokio::test]
async fn wait_for_held_request_resolves_once_a_request_for_the_session_is_held() {
    let registry = HoldRegistry::default();
    let session_id = acp::SessionId::new("s1");
    let mut waiting = pin!(registry.wait_for_held_request(&session_id));

    assert_eq!(None, waiting.as_mut().now_or_never());

    let mut held = pin!(registry.hold_until_released(&session_id));
    assert!(held.as_mut().now_or_never().is_none());

    assert_eq!(Some(()), waiting.now_or_never());
}

#[tokio::test]
async fn hold_dropped_before_its_release_leaves_no_entry() {
    let registry = HoldRegistry::default();
    let session_id = acp::SessionId::new("s1");
    let mut held = Box::pin(registry.hold_until_released(&session_id));
    assert!(held.as_mut().now_or_never().is_none());
    drop(held);

    assert_eq!(
        None,
        registry.wait_for_held_request(&session_id).now_or_never()
    );
    assert_eq!(
        Some(()),
        registry.release_held_requests(&session_id).now_or_never()
    );
}
