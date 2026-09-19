//! Subagent lineage: the root session and spawner chain of each admitted child.

use std::collections::HashMap;

/// One node per child id in any coordinator map, removed only at completed-record eviction.
#[derive(Default)]
pub(super) struct SpawnGraph {
    nodes: HashMap<String, SpawnNode>,
}

struct SpawnNode {
    root: String,
    /// Spawner sessions between the root and this child, outermost first.
    spawner_chain: Vec<String>,
    /// Host capability only; independent of the lineage above.
    advertise_to_spawner: bool,
    /// The ingress `surface_completion`, which reparent clears on the request itself.
    surface_to_spawner: bool,
}

pub(super) struct NestedSpawner {
    pub(super) child_id: String,
    /// The pre-reparent `parent_session_id`, which lineage checks compare against.
    pub(super) session_id: String,
    pub(super) surface_completion: bool,
}

/// The spawner has no node, so the child's ancestry cannot be completed.
#[derive(Debug)]
pub(super) struct MissingSpawnerNode;

impl SpawnGraph {
    pub(super) fn insert_root_child(&mut self, child_id: &str, root: &str) {
        self.insert(
            child_id,
            SpawnNode {
                root: root.to_owned(),
                spawner_chain: Vec::new(),
                advertise_to_spawner: false,
                surface_to_spawner: false,
            },
        );
    }

    /// Fails closed rather than recording a chain with missing ancestors.
    pub(super) fn insert_nested(
        &mut self,
        child_id: &str,
        root: &str,
        spawner: NestedSpawner,
    ) -> Result<(), MissingSpawnerNode> {
        let mut spawner_chain = self
            .nodes
            .get(&spawner.child_id)
            .ok_or(MissingSpawnerNode)?
            .spawner_chain
            .clone();
        spawner_chain.push(spawner.session_id);
        self.insert(
            child_id,
            SpawnNode {
                root: root.to_owned(),
                spawner_chain,
                advertise_to_spawner: true,
                surface_to_spawner: spawner.surface_completion,
            },
        );
        Ok(())
    }

    fn insert(&mut self, child_id: &str, node: SpawnNode) {
        debug_assert!(
            !self.nodes.contains_key(child_id),
            "spawn graph already holds {child_id}"
        );
        self.nodes.insert(child_id.to_owned(), node);
    }

    pub(super) fn surface_target(&self, child_id: &str) -> Option<&str> {
        self.nodes
            .get(child_id)?
            .surface_to_spawner
            .then(|| self.direct_spawner(child_id))
            .flatten()
    }

    pub(super) fn remove(&mut self, child_id: &str) {
        self.nodes.remove(child_id);
    }

    pub(super) fn contains(&self, child_id: &str) -> bool {
        self.nodes.contains_key(child_id)
    }

    /// A missing node is unreachable.
    pub(super) fn is_reachable_from(&self, child_id: &str, session_id: &str) -> bool {
        self.nodes.get(child_id).is_some_and(|node| {
            node.root == session_id || node.spawner_chain.iter().any(|id| id == session_id)
        })
    }

    pub(super) fn is_ancestor(&self, child_id: &str, ancestor_id: &str) -> bool {
        self.nodes
            .get(child_id)
            .is_some_and(|node| node.spawner_chain.iter().any(|id| id == ancestor_id))
    }

    pub(super) fn root_session(&self, child_id: &str) -> Option<&str> {
        self.nodes.get(child_id).map(|node| node.root.as_str())
    }

    pub(super) fn direct_spawner(&self, child_id: &str) -> Option<&str> {
        self.nodes
            .get(child_id)?
            .spawner_chain
            .last()
            .map(String::as_str)
    }

    pub(super) fn advertise_target(&self, child_id: &str) -> Option<&str> {
        self.nodes
            .get(child_id)?
            .advertise_to_spawner
            .then(|| self.direct_spawner(child_id))
            .flatten()
    }

    pub(super) fn drop_advertise(&mut self, child_id: &str) {
        if let Some(node) = self.nodes.get_mut(child_id) {
            node.advertise_to_spawner = false;
        }
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
