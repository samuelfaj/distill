// Modified for Distill by Samuel Fajreldines, 2026.
//! `grok usage <session-id> [turn]`: persisted token/cost usage.

use std::io::Write;

use anyhow::{Context, Result};
use distill_shell::session::usage_file::{SessionUsageFile, UsageLoad};

#[derive(Debug, clap::Args, Clone)]
pub struct UsageArgs {
    /// Session ID
    pub session_id: String,
    /// Turn number. Omit for session totals and every recorded turn.
    pub turn: Option<u32>,
}

pub fn run(args: UsageArgs) -> Result<()> {
    let payload = load_payload(&args.session_id, args.turn)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

fn load_payload(session_id: &str, turn: Option<u32>) -> Result<serde_json::Value> {
    match SessionUsageFile::load_for_session(session_id)
        .with_context(|| format!("Failed to read usage for session '{session_id}'"))?
    {
        UsageLoad::SessionNotFound => {
            anyhow::bail!("Session '{session_id}' not found.")
        }
        UsageLoad::NoUsage => {
            anyhow::bail!("No usage recorded for session '{session_id}'.")
        }
        UsageLoad::Ready(file) => select_payload(&file, turn, session_id),
    }
}

fn select_payload(
    file: &SessionUsageFile,
    turn: Option<u32>,
    session_id: &str,
) -> Result<serde_json::Value> {
    match turn {
        None => Ok(serde_json::to_value(file)?),
        Some(turn_number) => {
            let Some(row) = file.turn(turn_number) else {
                anyhow::bail!("Turn {turn_number} not found in session '{session_id}'.");
            };
            Ok(serde_json::json!({
                "sessionId": file.session_id,
                "updatedAt": file.updated_at,
                "session": file.session,
                "turns": [row],
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_shell::session::usage_file::UsageSummary;

    fn file_with_turns() -> SessionUsageFile {
        let mut file = SessionUsageFile::new("sess-1");
        file.apply_turn(
            1,
            "t1",
            &UsageSummary {
                input_tokens: 10,
                output_tokens: 2,
                total_tokens: 12,
                model_calls: 1,
                turn_count: 1,
                ..Default::default()
            },
            None,
        );
        file.apply_turn(
            2,
            "t2",
            &UsageSummary {
                input_tokens: 25,
                output_tokens: 7,
                total_tokens: 32,
                model_calls: 2,
                ..Default::default()
            },
            Some(&UsageSummary {
                input_tokens: 10,
                output_tokens: 2,
                total_tokens: 12,
                model_calls: 1,
                ..Default::default()
            }),
        );
        file
    }

    #[test]
    fn omitted_turn_returns_session_and_all_turns() {
        let file = file_with_turns();
        let value = select_payload(&file, None, "sess-1").unwrap();
        assert_eq!(
            value.get("sessionId").and_then(|v| v.as_str()),
            Some("sess-1")
        );
        assert_eq!(
            value
                .get("session")
                .and_then(|s| s.get("inputTokens"))
                .and_then(|v| v.as_u64()),
            Some(25)
        );
        assert_eq!(
            value.get("turns").and_then(|v| v.as_array()).map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn turn_index_returns_that_row() {
        let file = file_with_turns();
        let value = select_payload(&file, Some(2), "sess-1").unwrap();
        assert_eq!(
            value.get("sessionId").and_then(|v| v.as_str()),
            Some("sess-1")
        );
        assert_eq!(
            value
                .get("session")
                .and_then(|s| s.get("inputTokens"))
                .and_then(|v| v.as_u64()),
            Some(25)
        );
        let Some(turns) = value.get("turns").and_then(|v| v.as_array()) else {
            panic!("expected turns array: {value:?}");
        };
        let [turn] = turns.as_slice() else {
            panic!("expected one turn: {turns:?}");
        };
        assert_eq!(turn.get("turnNumber").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(turn.get("inputTokens").and_then(|v| v.as_u64()), Some(15));
    }

    #[test]
    fn missing_turn_is_an_error() {
        let file = file_with_turns();
        let err = select_payload(&file, Some(9), "sess-1").unwrap_err();
        assert!(err.to_string().contains("Turn 9 not found"));
        assert!(!err.to_string().contains("usage.json"));
    }
}
