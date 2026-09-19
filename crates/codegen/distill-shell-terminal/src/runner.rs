// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol as acp;
use distill_paths::AbsPathBuf;

#[derive(thiserror::Error, Debug)]
pub enum TerminalError {
    #[error("{0}")]
    Other(String),
    #[error("Command could not be quoted")]
    CommandNotQuoted,
}

pub struct TerminalRunRequest {
    pub tool_call_id: acp::ToolCallId,
    pub command: String,
    pub cwd: AbsPathBuf,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub output_byte_limit: usize,
    /// Whether to stream output updates and register in the terminal registry.
    /// - `true`: Agent tool calls (streaming updates, killable via x.ai/terminal/kill)
    /// - `false`: Extension methods, git helpers (no updates, not killable)
    pub stream: bool,
    /// When Some, the streaming loop writes output to this file as it arrives.
    /// Background tasks use it to retrieve the full output after the in-memory buffer truncates.
    pub output_file: Option<PathBuf>,
}

pub struct TerminalRunResult {
    pub combined_output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub signal: Option<String>,
    pub timed_out: bool,
}

#[async_trait::async_trait]
pub trait AsyncTerminalRunner: Send + Sync {
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, TerminalError>;
}
