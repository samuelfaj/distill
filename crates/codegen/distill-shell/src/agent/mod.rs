pub mod activity;
pub mod app;
pub mod auth_method;
pub mod chat_modes;
pub mod config;
pub(crate) mod config_model_override_parse;
pub(crate) mod cursor_worker_config;
mod ext_parsers;
pub mod external_otel_pin;
pub mod feedback_client;
pub mod folder_trust;
pub(crate) mod handlers;
pub mod init;
pub mod model_providers;
pub mod mvp_agent;
pub(crate) mod otel_gate;
pub(crate) mod proxy;
pub mod relay;
pub mod remote_config;
pub(crate) mod restore_code;
pub mod roster;
pub mod server;
pub mod session_config;
pub(crate) mod session_metrics;
pub mod session_registry_client;
pub(crate) mod subagent;
pub(crate) mod subscription_check;
#[cfg(feature = "test-support")]
pub mod testkit;
pub(crate) mod update_chunk_merge;

pub use mvp_agent::MvpAgent;
pub use mvp_agent::SessionSetupPhase;
pub use relay::{RelayConfig, RelayHandle, spawn_relay_connection};
pub use server::{ServerConfig, run_agent_server};

#[cfg(test)]
mod storage_client_tests;
