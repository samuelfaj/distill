//! Fault injection for tests of the attach and recovery paths.
//! Environment-driven hooks ship in the binary and are inert unless their variable is set (one env
//! read at the hook site). `#[cfg(test)]` pause points compile out: a task-local slot parks the
//! attach future.

use agent_client_protocol as acp;

/// Where a cold attach parks for a test: with its actor's init result in hand but not yet installed,
/// installed and bound, or after a failed stamp drained its actor.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachPause {
    AfterSpawn,
    AfterInstall,
    BeforeRelease,
}

#[cfg(test)]
tokio::task_local! {
    static ATTACH_PAUSE: std::cell::RefCell<Option<(
        AttachPause,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>>;
}

#[cfg(test)]
pub(crate) fn with_pause_at<T>(
    point: AttachPause,
    future: impl std::future::Future<Output = T>,
) -> (
    impl std::future::Future<Output = T>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    (
        ATTACH_PAUSE.scope(
            std::cell::RefCell::new(Some((point, reached_tx, release_rx))),
            future,
        ),
        reached_rx,
        release_tx,
    )
}

#[cfg(test)]
pub(crate) async fn pause_at(point: AttachPause) {
    let pause = ATTACH_PAUSE
        .try_with(|slot| {
            let mut slot = slot.borrow_mut();
            let armed = slot.as_ref().is_some_and(|(armed, ..)| *armed == point);
            armed.then(|| slot.take()).flatten()
        })
        .ok()
        .flatten();
    if let Some((_, reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}

/// `GROK_TEST_PROMPT_BLACKHOLE=1` parks `session/prompt` forever after the dispatch lock is taken.
pub(crate) const PROMPT_BLACKHOLE_ENV: &str = "GROK_TEST_PROMPT_BLACKHOLE";

/// Reproduce a shell whose prompt intake never reaches the session actor. The caller holds the session's
/// dispatch lock, so a follow-up `session/cancel` parks behind it too; that is the wedge shape the client's
/// acknowledgment watch covers and why its cancel send is bounded.
pub(crate) async fn park_forever_if_blackholed(session_id: &acp::SessionId) {
    let blackholed = std::env::var(PROMPT_BLACKHOLE_ENV).is_ok_and(|value| value.trim() == "1");
    if !blackholed {
        return;
    }
    tracing::warn!(
        env = PROMPT_BLACKHOLE_ENV,
        session_id = %session_id.0,
        "test hook: parking session/prompt forever"
    );
    std::future::pending::<()>().await;
}
