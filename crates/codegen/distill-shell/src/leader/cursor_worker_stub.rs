// Modified for Distill by Samuel Fajreldines, 2026.
//! Twin of `cursor_worker` for builds without the `cursor-worker` feature: the same control
//! surface, with every command answering the "not compiled in" `ControlError`. Keeps
//! `server.rs` and `run_leader` free of feature gates.

use std::path::PathBuf;
use std::sync::Arc;

use distill_login::AuthManager;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::agent::config::CursorWorkerConfig;
use crate::cpu_profile::{ControlError, ControlErrorCode};
use crate::leader::protocol::{ControlPayload, CursorWorkerStartArgs, CursorWorkerSummary};
use crate::leader::roster_merge::ExternalRoster;

/// `LeaderCapabilities.cursor_worker` for this build.
pub(crate) const COMPILED_IN: bool = false;

fn not_compiled_in() -> ControlError {
    ControlError {
        code: ControlErrorCode::InternalError,
        message: "cursor worker support is not compiled into this leader".to_owned(),
        details: None,
    }
}

#[derive(Debug)]
pub(crate) struct CursorWorkerControl {
    auto_start: bool,
}

impl CursorWorkerControl {
    pub(crate) fn new(
        config: CursorWorkerConfig,
        _leader_hub_url: Option<String>,
        _distill_home: PathBuf,
        _roster: ExternalRoster,
    ) -> Self {
        Self {
            auto_start: config.auto_start,
        }
    }

    pub(crate) fn set_auth_manager(&self, _auth_manager: Arc<AuthManager>) {}

    pub(crate) async fn start(
        &self,
        _args: CursorWorkerStartArgs,
        _cancel: &CancellationToken,
        _pid: u32,
    ) -> Result<ControlPayload, ControlError> {
        Err(not_compiled_in())
    }

    pub(crate) async fn stop(&self, _pid: u32) -> Result<ControlPayload, ControlError> {
        Err(not_compiled_in())
    }

    pub(crate) async fn status(&self, _pid: u32) -> Result<ControlPayload, ControlError> {
        Err(not_compiled_in())
    }

    pub(crate) fn info_summary(&self) -> CursorWorkerSummary {
        CursorWorkerSummary {
            state: "none".to_owned(),
            claims: 0,
            doors: Vec::new(),
        }
    }

    pub(crate) async fn start_at_boot(
        &self,
        boot: Option<CursorWorkerStartArgs>,
        _cancel: &CancellationToken,
        _pid: u32,
    ) {
        if boot.is_some() || self.auto_start {
            warn!("cursor worker requested at boot, but support is not compiled into this leader");
        }
    }

    pub(crate) async fn finalize_on_shutdown(&self) {}
}
