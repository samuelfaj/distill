use std::sync::Arc;
use std::sync::OnceLock;

use tokio_util::sync::CancellationToken;

/// The server set a piece of MCP work was started for. Only `McpState` hands one out and only `McpState` cancels it,
/// so an uncancelled `Generation` is the current one.
#[derive(Clone, Debug)]
pub struct Generation {
    token: CancellationToken,
    replaced_by: Arc<OnceLock<Replacement>>,
}

/// What replaced a generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    ServerSetChange,
    Rebuild,
}

/// The premise a caller acted on (server set, claim, installed client) has been replaced.
#[derive(Debug, PartialEq, Eq)]
pub struct Superseded;

impl Generation {
    pub(crate) fn current() -> Self {
        Self {
            token: CancellationToken::new(),
            replaced_by: Arc::new(OnceLock::new()),
        }
    }

    /// Recorded before the token is cancelled, so anyone woken by the cancellation can read it.
    pub(crate) fn replace(&self, by: Replacement) {
        let _ = self.replaced_by.set(by);
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn replaced_by(&self) -> Option<Replacement> {
        self.replaced_by.get().copied()
    }

    /// Runs `wait` until it completes or this generation is replaced. `wait` is dropped mid-way on cancellation, so
    /// wrap only work whose partial results release themselves on drop.
    pub async fn or_cancel<F: Future>(&self, wait: F) -> Result<F::Output, Superseded> {
        tokio::select! {
            biased;
            () = self.token.cancelled() => Err(Superseded),
            output = wait => Ok(output),
        }
    }
}
