//! The session-owned MCP client map.

use std::collections::HashMap;
use std::sync::Arc;

use crate::servers::{McpClient, McpServerName};

#[derive(Default)]
pub struct OwnedClients {
    clients: HashMap<McpServerName, Arc<McpClient>>,
}

impl OwnedClients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        name: McpServerName,
        client: Arc<McpClient>,
    ) -> Option<Arc<McpClient>> {
        let displaced = self.clients.insert(name, client);
        if let Some(old) = &displaced {
            cancel_watcher(old);
        }
        displaced
    }

    pub fn remove(&mut self, name: &str) -> Option<Arc<McpClient>> {
        let removed = self.clients.remove(name);
        if let Some(old) = &removed {
            cancel_watcher(old);
        }
        removed
    }

    /// Removes `name` only while its slot still holds `client`.
    pub fn remove_if_same(&mut self, name: &str, client: &Arc<McpClient>) {
        if self
            .clients
            .get(name)
            .is_some_and(|held| Arc::ptr_eq(held, client))
        {
            self.remove(name);
        }
    }

    pub fn clear(&mut self) {
        for client in self.clients.values() {
            cancel_watcher(client);
        }
        self.clients.clear();
    }

    pub fn get(&self, name: &str) -> Option<&Arc<McpClient>> {
        self.clients.get(name)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&McpServerName, &Arc<McpClient>)> {
        self.clients.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &McpServerName> {
        self.clients.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = &Arc<McpClient>> {
        self.clients.values()
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

/// An evicted client's liveness watcher holds a strong `Arc` to it; cancel the watcher so the client, its `Ready` state, and its gauge slot can drop.
fn cancel_watcher(client: &McpClient) {
    client.set_liveness_handle(None);
}

impl Drop for OwnedClients {
    fn drop(&mut self) {
        for client in self.clients.values() {
            cancel_watcher(client);
        }
    }
}

impl FromIterator<(McpServerName, Arc<McpClient>)> for OwnedClients {
    fn from_iter<I: IntoIterator<Item = (McpServerName, Arc<McpClient>)>>(iter: I) -> Self {
        Self {
            clients: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
#[path = "owned_clients_tests.rs"]
mod tests;
