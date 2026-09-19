//! The `grok agent stdio` child that `GrokStdioClient` and `AcpTestClient` drive: the sandbox it runs in, what
//! is applied on top of the sandbox's hermetic baseline, and the spawn that hands the sandbox back together
//! with the child.

use std::path::Path;

use crate::env::grok_binary;
use crate::mock_server::MockInferenceServer;
use crate::process::{TestOutput, TestProcess, TestProcessConfig, TestStdin};
use crate::sandbox::TestSandbox;

pub(crate) struct AgentProcessOptions {
    sandbox: TestSandbox,
    pub(crate) extra_env: Vec<(String, String)>,
    /// Keys removed from the sandbox baseline after the mock URL and `extra_env` are applied, so a
    /// scenario can drop a variable the baseline sets (e.g. the mock's `XAI_API_KEY`).
    pub(crate) removed_env: Vec<String>,
    pub(crate) leading_args: Vec<String>,
}

/// A running agent and the sandbox it runs in; the child is declared first so it drops before its sandbox.
#[must_use]
pub(crate) struct SpawnedAgent {
    pub(crate) process: TestProcess,
    pub(crate) sandbox: TestSandbox,
}

impl AgentProcessOptions {
    #[must_use]
    pub(crate) fn new(sandbox: TestSandbox) -> Self {
        AgentProcessOptions {
            sandbox,
            extra_env: Vec::new(),
            removed_env: Vec::new(),
            leading_args: Vec::new(),
        }
    }

    /// Points the sandbox at the mock server before applying the env overrides, so an override can replace a
    /// mock endpoint.
    pub(crate) fn spawn(self, server: &MockInferenceServer, cwd: &Path) -> SpawnedAgent {
        let AgentProcessOptions {
            mut sandbox,
            extra_env,
            removed_env,
            leading_args,
        } = self;
        sandbox.set_mock_url(server.url());
        sandbox.extend_env(extra_env);
        for key in removed_env {
            sandbox.remove_env(key);
        }

        let binary = grok_binary();
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&leading_args)
            .args(["agent", "stdio"])
            .current_dir(cwd);

        let process = TestProcess::spawn(
            cmd,
            &sandbox,
            TestProcessConfig::new()
                .label("grok agent stdio")
                .stdin(TestStdin::Piped)
                .stdout(TestOutput::Piped),
        )
        .unwrap_or_else(|error| {
            panic!(
                "failed to spawn grok agent stdio at {}: {error}\n{}",
                binary.display(),
                sandbox.diagnostic_summary(),
            )
        });
        SpawnedAgent { process, sandbox }
    }
}
