use agent_client_protocol as acp;

pub(crate) fn resolve_session_auto_mode(
    meta: Option<&acp::Meta>,
    default_auto_mode: bool,
    session_yolo_mode: bool,
) -> bool {
    meta.and_then(|m| m.get("autoMode").or_else(|| m.get("auto_mode")))
        .and_then(|v| v.as_bool())
        .unwrap_or(default_auto_mode && !session_yolo_mode)
}
