//! Wire names of the hub-synthesized Grok Bot harness tools.
//!
//! Single source of truth shared by the hub that registers the tools and
//! by the clients that gate the tools per agent config.

/// Handwritten harness tool ids: every id a hub may register or Plane may
/// allowlist. A hub registers a prefix of this list, so an id can be
/// declared (validated, offered, reserved) before any hub implements it.
pub const GROK_BOT_TOOL_IDS: &[&str] = &[
    "bot_create_agent",
    "bot_list_agents",
    "bot_send_prompt",
    "bot_get_agent_transcript",
    "bot_get_agent_transcript_page",
    "bot_get_agent_transcript_tail",
    "bot_get_agent_transcript_window",
    "bot_transcript_offbox",
    "bot_await_turn",
    "bot_search_agents",
];

/// Whether `name` is a hub-synthesized Grok Bot harness tool.
pub fn is_grok_bot_tool(name: &str) -> bool {
    GROK_BOT_TOOL_IDS.contains(&name)
}

/// Bot tools a toolbox receives when `grok_bot_allowed_tools` is empty.
/// A new id is added here only once a hub implements it; until then it
/// needs explicit per-toolbox opt-in.
pub const GROK_BOT_DEFAULT_TOOL_IDS: &[&str] = &[
    "bot_create_agent",
    "bot_list_agents",
    "bot_send_prompt",
    "bot_get_agent_transcript",
    "bot_get_agent_transcript_page",
    "bot_get_agent_transcript_tail",
    "bot_get_agent_transcript_window",
    "bot_transcript_offbox",
    "bot_await_turn",
    "bot_search_agents",
];

/// Whether `name` is in the empty-allowlist default set.
pub fn is_grok_bot_default_tool(name: &str) -> bool {
    GROK_BOT_DEFAULT_TOOL_IDS.contains(&name)
}

/// Model-facing descriptions, one per [`GROK_BOT_TOOL_IDS`] entry (same
/// order). The hub registers its tools with these strings, and clients
/// use them to advertise opted-in bot tools before the hub connection is
/// live, so both surfaces render the same text.
pub const GROK_BOT_TOOL_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "bot_create_agent",
        "Create a Grok Bot agent. It greets the user itself; send no first \
         prompt, never quote its id. Cannot be deleted; check bot_list_agents \
         first. Only when the user asks for a new agent; to reach an existing \
         one use bot_list_agents then bot_send_prompt. One agent per call.",
    ),
    (
        "bot_list_agents",
        "List all Grok Bot agents on the user's box with id, name, description, \
         and status. Wakes the box. Its ids are the only valid agent_id values.",
    ),
    (
        "bot_send_prompt",
        "Send a prompt to a Grok Bot agent. Returns once accepted unless mode \
         waits for the reply. on_busy is supersede (default), reject, or queue. \
         After a timeout or a missing notification, resume with bot_await_turn \
         and the returned handle; never re-send. Empty reply with \
         finished:true means no text. A <grok_bot agent_id> tag is that \
         agent's id.",
    ),
    (
        "bot_get_agent_transcript",
        "Read an agent's entire transcript. Wakes the box.",
    ),
    (
        "bot_get_agent_transcript_page",
        "Read a time-bounded page of an agent's transcript. Wakes the box.",
    ),
    (
        "bot_get_agent_transcript_tail",
        "Read the latest page of an agent's transcript, such as the reply after \
         a send. Wakes the box. Do not poll it to wait for a turn; use \
         bot_await_turn.",
    ),
    (
        "bot_get_agent_transcript_window",
        "Like bot_get_agent_transcript_tail, plus per-thread counts.",
    ),
    (
        "bot_transcript_offbox",
        "Read an agent's transcript without waking the box. Pass the previous \
         page's nextCursor to continue.",
    ),
    (
        "bot_await_turn",
        "Wait for an agent's turn to finish. Pass the handle from \
         bot_send_prompt to keep waiting after a timeout or while its async \
         wait is pending, instead of re-sending. Without a handle, waits for \
         the pending turn if one is in flight, otherwise for idle, and \
         returns the last message.",
    ),
    (
        "bot_search_agents",
        "Find Grok Bot agents by name or description when you know what you \
         want. Returns the best matches only; bot_list_agents shows every bot. \
         Wakes the box.",
    ),
];

/// The model-facing description for a Grok Bot tool id, if known.
pub fn grok_bot_tool_description(name: &str) -> Option<&'static str> {
    GROK_BOT_TOOL_DESCRIPTIONS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, desc)| *desc)
}

/// The model must paste a roster id, not invent a name or a shortened UUID;
/// the full rule sits on bot_send_prompt, the reads carry the pointer.
const AGENT_ID_PARAM_DESCRIPTION: &str = "Agent id pasted from bot_list_agents.";
const SEND_AGENT_ID_DESCRIPTION: &str = "Opaque id copied exactly from \
     bot_list_agents or bot_search_agents; never a name, never typed from \
     memory or shortened.";

/// Flattened JSON Schema for a Grok Bot tool's arguments.
///
/// Same shape the hub advertises via `schema_for_kind`. Pre-bind synthesis
/// uses this so constrained decoding can emit required fields (`agent_id`,
/// `prompt`, …) instead of locking the call to `{}`.
pub fn grok_bot_tool_arguments_schema(name: &str) -> Option<serde_json::Value> {
    Some(match name {
        "bot_create_agent" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Display name."
                },
                "description": {
                    "type": "string",
                    "description": "Optional persona or instructions."
                }
            }
        }),
        "bot_list_agents" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
        "bot_send_prompt" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id", "prompt"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": SEND_AGENT_ID_DESCRIPTION
                },
                "prompt": {
                    "type": "string",
                    "description": "Text to send."
                },
                "mode": {
                    "type": "string",
                    "enum": ["fire_and_forget", "blocking", "async"],
                    "description": "fire_and_forget (default) returns on accept; blocking waits and returns the reply; async returns a handle and notifies when the turn ends."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Omit it. A turn often takes longer than two minutes. Async ignores this."
                },
                "on_busy": {
                    "type": "string",
                    "enum": ["reject", "queue", "supersede"],
                    "description": "supersede (default) the current wait, reject, or queue after idle."
                },
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Up to 8 files, 25 MiB each. Workspace-relative paths such as attachments/note.pdf; absolute guest paths are rewritten. Without a connected workspace, artifacts/ and attachments/ paths fetch conversation files."
                }
            }
        }),
        "bot_get_agent_transcript" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": AGENT_ID_PARAM_DESCRIPTION
                }
            }
        }),
        "bot_get_agent_transcript_page" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id", "limit", "until_ms"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": AGENT_ID_PARAM_DESCRIPTION
                },
                "limit": {
                    "type": "integer",
                    "description": "Max entries, at least 1."
                },
                "until_ms": {
                    "type": "integer",
                    "description": "Inclusive upper bound, unix ms."
                },
                "before_seq": {
                    "type": "integer",
                    "description": "Return entries before this seq."
                },
                "since_ms": {
                    "type": "integer",
                    "description": "Inclusive lower bound, unix ms."
                }
            }
        }),
        "bot_get_agent_transcript_tail" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id", "limit"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": AGENT_ID_PARAM_DESCRIPTION
                },
                "limit": {
                    "type": "integer",
                    "description": "Max entries, at least 1."
                },
                "before_seq": {
                    "type": "integer",
                    "description": "Return entries before this seq; omit for the latest page."
                }
            }
        }),
        "bot_get_agent_transcript_window" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id", "limit"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": AGENT_ID_PARAM_DESCRIPTION
                },
                "limit": {
                    "type": "integer",
                    "description": "Max entries, at least 1."
                },
                "before_seq": {
                    "type": "integer",
                    "description": "Return entries before this seq; omit for the latest page."
                }
            }
        }),
        "bot_transcript_offbox" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": AGENT_ID_PARAM_DESCRIPTION
                },
                "cursor": {
                    "type": "string",
                    "description": "nextCursor from the previous page; omit on the first."
                }
            }
        }),
        "bot_await_turn" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agent_id"],
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Agent id pasted from bot_list_agents; must match the handle's agent."
                },
                "handle": {
                    "type": "object",
                    "default": null,
                    "description": "From bot_send_prompt or a prior bot_await_turn, unchanged. Omit to wait for idle."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Omit it. On timeout, finished is false; wait again with the handle."
                }
            }
        }),
        "bot_search_agents" => serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What the agent is for, or its name."
                },
                "status": {
                    "type": "string",
                    "enum": ["running", "idle"],
                    "description": "Filter by status."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max agents, 1 to 64."
                }
            }
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_cover_every_id_in_order() {
        let desc_ids: Vec<&str> = GROK_BOT_TOOL_DESCRIPTIONS
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(desc_ids, GROK_BOT_TOOL_IDS);
        assert!(
            GROK_BOT_TOOL_DESCRIPTIONS
                .iter()
                .all(|(_, desc)| !desc.trim().is_empty())
        );
    }

    #[test]
    fn descriptions_steer_copied_agent_ids_and_create_sparingly() {
        let create = grok_bot_tool_description("bot_create_agent").expect("bot_create_agent");
        for needle in [
            "Only when the user asks for a new agent",
            "bot_list_agents then bot_send_prompt",
            "One agent per call",
        ] {
            assert!(
                create.contains(needle),
                "bot_create_agent missing {needle:?}"
            );
        }
        let send = grok_bot_tool_arguments_schema("bot_send_prompt").expect("bot_send_prompt");
        let agent_id = send["properties"]["agent_id"]["description"]
            .as_str()
            .expect("agent_id description");
        for needle in [
            "copied exactly from bot_list_agents or bot_search_agents",
            "opaque id",
            "never typed from memory or shortened",
        ] {
            assert!(
                agent_id.to_ascii_lowercase().contains(needle),
                "bot_send_prompt.agent_id missing {needle:?}: {agent_id}"
            );
        }
    }

    #[test]
    fn arguments_schema_covers_every_id() {
        for id in GROK_BOT_TOOL_IDS {
            let schema = grok_bot_tool_arguments_schema(id)
                .unwrap_or_else(|| panic!("{id} must have an arguments schema"));
            assert_eq!(schema["type"], "object", "{id}");
        }
        assert!(grok_bot_tool_arguments_schema("bot_typo").is_none());
    }

    #[test]
    fn default_tool_ids_are_a_prefix_of_shared_ids() {
        for id in GROK_BOT_DEFAULT_TOOL_IDS {
            assert!(is_grok_bot_tool(id), "{id} is not in GROK_BOT_TOOL_IDS");
        }
        assert_eq!(
            GROK_BOT_DEFAULT_TOOL_IDS.len(),
            10,
            "default set size changed; check the surface budget and every other copy of this list"
        );
        assert_eq!(
            GROK_BOT_DEFAULT_TOOL_IDS,
            &GROK_BOT_TOOL_IDS[..GROK_BOT_DEFAULT_TOOL_IDS.len()],
            "new ids go after the default set"
        );
    }

    #[test]
    fn search_agents_is_a_default_tool() {
        assert!(is_grok_bot_default_tool("bot_search_agents"));
        assert!(!is_grok_bot_default_tool("bot_future_tool"));
    }

    /// Clients advertise this schema from the shared table before the
    /// hub bind resolves, so its shape is pinned here rather than only by a
    /// hub's schema goldens.
    #[test]
    fn search_agents_schema_shape() {
        let schema = grok_bot_tool_arguments_schema("bot_search_agents").unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!(["query"]));
        let props = schema["properties"].as_object().unwrap();
        let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["limit", "query", "status"]);
        assert_eq!(props["query"]["type"], "string");
        assert_eq!(props["status"]["type"], "string");
        assert_eq!(
            props["status"]["enum"],
            serde_json::json!(["running", "idle"])
        );
        assert_eq!(props["limit"]["type"], "integer");
        for key in ["minimum", "maximum", "format"] {
            assert!(
                props["limit"].get(key).is_none(),
                "limit must not carry {key}"
            );
        }
    }

    /// Model-facing bytes of one tool as advertised to the model.
    fn surface_bytes(id: &str) -> usize {
        serde_json::json!({
            "name": id,
            "description": grok_bot_tool_description(id),
            "parameters": grok_bot_tool_arguments_schema(id),
        })
        .to_string()
        .len()
    }

    /// Every byte of the default set is prompt context on every turn of an
    /// opted-in toolbox with an empty allowlist, so it keeps its own cap; an
    /// opt-in tool is paid for only by the toolboxes that list it and must
    /// not squeeze the default descriptions to fit.
    #[test]
    fn surface_fits_budget() {
        const DEFAULT_SET_BUDGET: usize = 6_000;
        const OPT_IN_TOOL_BUDGET: usize = 700;
        let default_total: usize = GROK_BOT_DEFAULT_TOOL_IDS
            .iter()
            .map(|id| surface_bytes(id))
            .sum();
        assert!(
            default_total <= DEFAULT_SET_BUDGET,
            "default bot tool surface is {default_total} bytes (budget {DEFAULT_SET_BUDGET}); trim before raising the budget"
        );
        for id in &GROK_BOT_TOOL_IDS[GROK_BOT_DEFAULT_TOOL_IDS.len()..] {
            let bytes = surface_bytes(id);
            assert!(
                bytes <= OPT_IN_TOOL_BUDGET,
                "opt-in tool {id} is {bytes} bytes (budget {OPT_IN_TOOL_BUDGET}); trim before raising the per-tool budget"
            );
        }
    }
}
