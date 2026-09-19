//! Writes to [`McpState`] on a premise formed before an `await`: the premise is re-checked and the write made under
//! one lock.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::generation::{Generation, Superseded};
use crate::servers::{InitClaimGuard, McpClient, McpState};

pub trait SharedMcpState {
    /// Writes while `generation` is still the current server set.
    fn write_if_current<R>(
        &self,
        generation: &Generation,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> impl Future<Output = Result<R, Superseded>>;

    /// Writes while `client` is still the one installed under `name`.
    fn write_if_installed<R>(
        &self,
        name: &str,
        client: &Arc<McpClient>,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> impl Future<Output = Result<R, Superseded>>;

    /// Writes while the slot for `name` is what the caller saw: `expected` by identity, or empty within the same
    /// server set with no live pass left to fill it.
    fn write_if_slot_is<R>(
        &self,
        name: &str,
        expected: Option<&Arc<McpClient>>,
        generation: &Generation,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> impl Future<Output = Result<R, Superseded>>;

    /// Writes while `claim` still owns init, handing it to the write.
    fn write_if_owner<R>(
        &self,
        claim: InitClaimGuard,
        write: impl FnOnce(&mut McpState, InitClaimGuard) -> R,
    ) -> impl Future<Output = Result<R, Superseded>>;
}

impl SharedMcpState for Arc<Mutex<McpState>> {
    async fn write_if_current<R>(
        &self,
        generation: &Generation,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> Result<R, Superseded> {
        let mut state = self.lock().await;
        if !state.is_current(generation) {
            return Err(Superseded);
        }
        Ok(write(&mut state))
    }

    async fn write_if_installed<R>(
        &self,
        name: &str,
        client: &Arc<McpClient>,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> Result<R, Superseded> {
        let mut state = self.lock().await;
        if !state.has_client(name, client) {
            return Err(Superseded);
        }
        Ok(write(&mut state))
    }

    async fn write_if_slot_is<R>(
        &self,
        name: &str,
        expected: Option<&Arc<McpClient>>,
        generation: &Generation,
        write: impl FnOnce(&mut McpState) -> R,
    ) -> Result<R, Superseded> {
        let mut state = self.lock().await;
        let unchanged = match expected {
            Some(client) => state.has_client(name, client),
            None => {
                state.is_current(generation)
                    && state.get_client(name).is_none()
                    && !state.is_server_pending(name)
            }
        };
        if !unchanged {
            return Err(Superseded);
        }
        Ok(write(&mut state))
    }

    async fn write_if_owner<R>(
        &self,
        claim: InitClaimGuard,
        write: impl FnOnce(&mut McpState, InitClaimGuard) -> R,
    ) -> Result<R, Superseded> {
        let mut state = self.lock().await;
        if !state.owns_init(&claim) {
            return Err(Superseded);
        }
        Ok(write(&mut state, claim))
    }
}
