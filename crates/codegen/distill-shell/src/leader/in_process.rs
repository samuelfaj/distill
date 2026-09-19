// Modified for Distill by Samuel Fajreldines, 2026.
//! A real agent behind a leader server, in this process rather than a child.

use std::sync::Arc;

use agent_client_protocol as acp;
use distill_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, simplex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::agent::config::Config as AgentConfig;
use crate::agent::mvp_agent::MvpAgent;

const SIMPLEX_BUF: usize = 8 * 1024 * 1024;

/// Spawns an agent on the current `LocalSet`, reading requests from `to_agent` and writing responses to `from_agent`.
/// Returns the task handles so a caller can end the agent.
/// Panics if the ambient configuration cannot build one.
pub fn spawn_agent(
    mut to_agent: UnboundedReceiver<String>,
    from_agent: UnboundedSender<String>,
) -> Vec<JoinHandle<()>> {
    let (agent_in_read, mut agent_in_write) = simplex(SIMPLEX_BUF);
    let (agent_out_read, agent_out_write) = simplex(SIMPLEX_BUF);

    let agent = tokio::task::spawn_local(async move {
        let mut config = AgentConfig::default();
        let auth_manager = Arc::new(config.create_auth_manager());
        // This runs on a current-thread `LocalSet`, where the sync bootstrap in `MvpAgent::new`
        // cannot drive the settings load itself. Resolve it on the async runtime first so
        // bootstrap observes a finished wait rather than falling open to bundled defaults.
        let boot = crate::agent::init::resolve_boot_startup_settings(
            &mut config,
            &tokio_util::sync::CancellationToken::new(),
            true,
            auth_manager.current(),
        )
        .await
        .ok();
        let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(
            GatewaySender::new(gateway_tx),
            &config,
            auth_manager,
            None,
            boot,
        )
        .expect("valid agent config");
        let incoming = LineBufferedRead::spawn_local(agent_in_read.compat());
        let (conn, handle_io) =
            acp::AgentSideConnection::new(agent, agent_out_write.compat_write(), incoming, |fut| {
                tokio::task::spawn_local(fut);
            });
        tokio::task::spawn_local(
            GatewayReceiver::new(gateway_rx, conn)
                .with_on_meta(distill_otel::span_from_meta_traceparent)
                .run(),
        );
        let _ = handle_io.await;
    });

    let requests = tokio::task::spawn_local(async move {
        while let Some(msg) = to_agent.recv().await {
            if agent_in_write.write_all(msg.as_bytes()).await.is_err()
                || agent_in_write.write_all(b"\n").await.is_err()
            {
                break;
            }
        }
    });

    let responses = tokio::task::spawn_local(async move {
        let mut reader = BufReader::new(agent_out_read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let msg = line.trim_end_matches(['\r', '\n']).to_string();
                    if !msg.is_empty() {
                        let _ = from_agent.send(msg);
                    }
                }
            }
        }
    });

    vec![agent, requests, responses]
}
