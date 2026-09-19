//! [`AcpTestClient`] drives `grok agent stdio` with verbatim JSON-RPC lines, backed by the same
//! [`TestProcess`] as the typed `GrokStdioClient`.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use crate::acp_agent_process::{AgentProcessOptions, SpawnedAgent};
use crate::headless::stderr_tail;
use crate::mock_server::MockInferenceServer;
use crate::process::{TestProcess, TestProcessStdout};
use crate::sandbox::TestSandbox;
use crate::scaled;

const STDERR_TAIL_CHARS: usize = 1200;

/// Writes one verbatim JSON-RPC line at a time and reads until the response carrying the same id, for wire
/// shapes the typed client cannot produce, such as Xcode's Swift/Foundation `JSONEncoder` output: escaped-slash
/// methods (`"session\/prompt"`) and string UUID request ids.
pub struct AcpTestClient {
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<TestProcessStdout>,
    process: TestProcess,
    _sandbox: TestSandbox,
}

impl AcpTestClient {
    pub async fn spawn(server: &MockInferenceServer, cwd: &Path) -> Self {
        let SpawnedAgent {
            mut process,
            sandbox,
        } = AgentProcessOptions::new(TestSandbox::new()).spawn(server, cwd);

        let stdin = process.take_stdin().expect("child stdin missing");
        let child_stdout = process.take_stdout().expect("child stdout missing");

        AcpTestClient {
            stdin,
            stdout: tokio::io::BufReader::new(child_stdout),
            process,
            _sandbox: sandbox,
        }
    }

    pub fn stderr(&self) -> String {
        self.process.stderr_tail().text
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.process.pid()
    }

    pub fn process_diagnostics(&self) -> String {
        self.process.diagnostic_summary()
    }

    pub fn start_terminate(&mut self) -> std::io::Result<()> {
        self.process.start_terminate()
    }

    pub fn start_kill(&mut self) {
        self.process.start_kill();
    }

    pub async fn close(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.process.close().await
    }

    /// Write `line` verbatim followed by `\n`, and flush.
    pub async fn send_line(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write line to agent stdin");
        self.stdin.write_all(b"\n").await.expect("write newline");
        self.stdin.flush().await.expect("flush agent stdin");
    }

    /// Read stdout lines until the response to `id` arrives: a message with no `method` key whose id is the same
    /// string. Returning is itself the id-echo assertion: an id echoed with different bytes or as a different JSON
    /// type never matches. Any agent-to-client request is refused with a JSON-RPC error so a turn can never hang
    /// on this client, which advertises no capabilities. A skipped count of zero means the agent sent nothing at
    /// all.
    pub async fn response_for_id(
        &mut self,
        id: &str,
        what: &str,
        timeout: Duration,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + scaled(timeout);
        let mut line = String::new();
        let mut skipped = 0_usize;
        let mut skipped_tail: Vec<String> = Vec::new();
        loop {
            line.clear();
            let next_line = self.stdout.read_line(&mut line);
            let Ok(io_result) = tokio::time::timeout_at(deadline, next_line).await else {
                panic!(
                    "{what}: no matching response within {timeout:?} ({skipped} other messages \
                     seen; last: {skipped_tail:?})\nstderr:\n{}",
                    stderr_tail(&self.stderr(), STDERR_TAIL_CHARS)
                );
            };
            let read = io_result
                .unwrap_or_else(|error| panic!("{what}: agent stdout read failed: {error}"));
            if read == 0 {
                panic!(
                    "{what}: agent closed stdout before responding ({skipped} other messages \
                     seen)\nstderr:\n{}",
                    stderr_tail(&self.stderr(), STDERR_TAIL_CHARS)
                );
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
                push_skipped_tail(&mut skipped, &mut skipped_tail, &line);
                continue;
            };
            let is_response = msg.get("method").is_none();
            if is_response && msg.get("id").and_then(|v| v.as_str()) == Some(id) {
                return msg;
            }
            push_skipped_tail(&mut skipped, &mut skipped_tail, &line);
            if !is_response && let Some(req_id) = msg.get("id") {
                let refusal = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32601, "message": "unsupported by the test client" },
                });
                self.send_line(&refusal.to_string()).await;
            }
        }
    }
}

/// Only the last three skipped lines are kept, truncated, so a timeout panic stays readable.
fn push_skipped_tail(skipped: &mut usize, tail: &mut Vec<String>, line: &str) {
    *skipped += 1;
    if tail.len() == 3 {
        tail.remove(0);
    }
    tail.push(line.trim_end().chars().take(200).collect());
}
