//! Slash catalog replacement funnel; every swap passes here so the unified log sees each change.

use std::collections::HashSet;

use agent_client_protocol as acp;

use super::agent::AgentSession;

#[derive(Debug, Clone, Copy)]
pub(crate) enum CommandCatalogSource {
    SessionUpdate,
    QueueDrain,
    CommandsList,
}

impl CommandCatalogSource {
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            Self::SessionUpdate => "session_update",
            Self::QueueDrain => "queue_drain",
            Self::CommandsList => "commands_list",
        }
    }
}

fn command_name_diff(
    prev: &[acp::AvailableCommand],
    next: &[acp::AvailableCommand],
) -> (Vec<String>, Vec<String>) {
    let prev_names: HashSet<&str> = prev.iter().map(|c| c.name.as_str()).collect();
    let next_names: HashSet<&str> = next.iter().map(|c| c.name.as_str()).collect();
    let mut added: Vec<String> = next_names
        .difference(&prev_names)
        .map(ToString::to_string)
        .collect();
    let mut removed: Vec<String> = prev_names
        .difference(&next_names)
        .map(ToString::to_string)
        .collect();
    added.sort_unstable();
    removed.sort_unstable();
    (added, removed)
}

impl AgentSession {
    pub(crate) fn replace_available_commands(
        &mut self,
        commands: Vec<acp::AvailableCommand>,
        source: CommandCatalogSource,
    ) {
        let (added, removed) = command_name_diff(&self.available_commands, &commands);
        // Bootstrap seeds generation 1, so the first shell-sent catalog gets the full listing
        let initial = self.available_commands_generation <= 1;
        if initial || !added.is_empty() || !removed.is_empty() {
            let names = initial.then(|| {
                let mut names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                names
            });
            crate::unified_log::info(
                "slash.registry.update",
                self.session_id.as_ref().map(|id| id.0.as_ref()),
                Some(serde_json::json!({
                    "source": source.as_label(),
                    "count": commands.len(),
                    "added": added,
                    "removed": removed,
                    "names": names,
                })),
            );
        }
        self.available_commands = commands;
        self.available_commands_generation += 1;
    }
}

#[cfg(test)]
#[path = "command_catalog_tests.rs"]
mod tests;
