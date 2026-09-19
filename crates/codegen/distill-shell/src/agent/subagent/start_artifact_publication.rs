// Modified for Distill by Samuel Fajreldines, 2026.
//! Ordinary spawns publish durable metadata and `SubagentSpawned` at preparation;
//! wakes publish only after `Started` is accepted, leaving prior metadata intact
//! when a wake fails before start.

use std::path::PathBuf;

use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_telemetry::instrument_task;

use super::{
    ShellChildRuntime, SpawnerAddressTarget, SubagentMeta, SubagentSpawnContext,
    emit_subagent_notification, write_subagent_meta,
};
use crate::extensions::notification::SessionUpdate;
use crate::session::SessionCommand;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationBoundary {
    Prepared,
    Started,
}

pub(super) struct PreparedStartArtifacts {
    pub(super) meta_dir: PathBuf,
    pub(super) meta: SubagentMeta,
    pub(super) spawned: SessionUpdate,
    pub(super) advertised_address: Option<String>,
    pub(super) reasoning_effort: Option<String>,
    pub(super) role_name: Option<String>,
    pub(super) parent_prompt_id: Option<String>,
}

pub(super) struct StartArtifactPublication {
    boundary: PublicationBoundary,
    durable_publication: bool,
    prepared: PreparedStartArtifacts,
    gateway: GatewaySender,
    reporter: distill_tools::implementations::distill::task::coordinator::ChildReporter<
        ShellChildRuntime,
    >,
    parent_session_id: String,
    parent_cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    spawner_address_target: Option<SpawnerAddressTarget>,
    bucket_url: Option<String>,
    upload_method: Option<crate::session::repo_changes::UploadMethod>,
    metadata_parent: Option<tracing::Span>,
    auth_manager: std::sync::Arc<crate::auth::AuthManager>,
    #[cfg(test)]
    fail_metadata_write: bool,
}

impl StartArtifactPublication {
    pub(super) fn new(
        boundary: PublicationBoundary,
        metadata_parent: Option<tracing::Span>,
        prepared: PreparedStartArtifacts,
        gateway: GatewaySender,
        reporter: distill_tools::implementations::distill::task::coordinator::ChildReporter<
            ShellChildRuntime,
        >,
        ctx: &SubagentSpawnContext,
    ) -> Self {
        Self {
            boundary,
            durable_publication: false,
            prepared,
            gateway,
            reporter,
            parent_session_id: ctx.parent_session_id.clone(),
            parent_cmd_tx: ctx.parent_cmd_tx.clone(),
            spawner_address_target: ctx.spawner_address_target.clone(),
            bucket_url: ctx.gcs_bucket_url.clone(),
            upload_method: ctx.gcs_upload_method.clone(),
            metadata_parent,
            auth_manager: ctx.auth_manager.clone(),
            #[cfg(test)]
            fail_metadata_write: ctx.fail_start_metadata_write,
        }
    }

    pub(super) fn publish_at(&mut self, boundary: PublicationBoundary) -> bool {
        if self.boundary == boundary {
            self.publish();
        }
        self.durable_publication
    }

    pub(super) fn terminal_persistence_allowed(&mut self) -> bool {
        if self.boundary == PublicationBoundary::Prepared && !self.durable_publication {
            self.durable_publication =
                write_subagent_meta(&self.prepared.meta_dir, &self.prepared.meta);
        }
        self.boundary == PublicationBoundary::Prepared || self.durable_publication
    }

    pub(super) fn forbid_terminal_persistence(&mut self) {
        self.boundary = PublicationBoundary::Started;
        self.durable_publication = false;
    }

    fn publish(&mut self) {
        let metadata_persist_span = match self.metadata_parent.as_ref() {
            Some(parent) => distill_telemetry::region!(
                "subagent_spawn.metadata_persist",
                distill_telemetry::region::Parent::Explicit(parent)
            ),
            None => distill_telemetry::region!(
                "subagent_spawn.metadata_persist",
                distill_telemetry::region::Parent::Inherit
            ),
        };
        #[cfg(test)]
        let metadata_written = !self.fail_metadata_write
            && write_subagent_meta(&self.prepared.meta_dir, &self.prepared.meta);
        #[cfg(not(test))]
        let metadata_written = write_subagent_meta(&self.prepared.meta_dir, &self.prepared.meta);
        self.durable_publication = metadata_written;
        metadata_persist_span.close();
        if self.durable_publication
            && let (Some(bucket_url), Some(upload_method)) = (&self.bucket_url, &self.upload_method)
        {
            let gcs_meta = super::SubagentSessionMetadata::from_meta(
                &self.prepared.meta,
                self.prepared.meta.effective_model_id.as_deref(),
                self.prepared.meta.child_cwd.as_deref(),
                None,
                None,
                None,
                self.prepared.reasoning_effort.as_deref(),
                self.prepared.role_name.as_deref(),
                self.prepared.parent_prompt_id.as_deref(),
                0,
            );
            let bucket = bucket_url.clone();
            let method = upload_method.clone();
            let auth_manager = self.auth_manager.clone();
            tokio::spawn(instrument_task!(
                debug,
                "subagent.metadata_upload",
                distill_telemetry::region::Parent::Root,
                async move {
                    crate::upload::trace::upload_subagent_metadata(
                        &gcs_meta,
                        &bucket,
                        method,
                        auth_manager,
                    )
                    .await;
                }
            ));
        }
        emit_subagent_notification(
            &self.gateway,
            &self.parent_session_id,
            self.prepared.spawned.clone(),
            self.parent_cmd_tx.as_ref(),
        );
        if let Some(target) = self.spawner_address_target.as_ref()
            && target.session_id != self.parent_session_id
        {
            let live = emit_subagent_notification(
                &self.gateway,
                &target.session_id,
                self.prepared.spawned.clone(),
                None,
            );
            if self.prepared.advertised_address.is_some() && !live {
                self.reporter.drop_spawner_claim();
            }
        }
    }
}
