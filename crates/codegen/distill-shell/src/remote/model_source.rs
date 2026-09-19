// Modified for Distill by Samuel Fajreldines, 2026.
//! Picks the URL and auth used to fetch the model list.
use crate::agent::config::EndpointsConfig;
use crate::agent::remote_config::ModelFetchAuth;
use crate::remote::client::{BackendError, FetchModelsResult};
use distill_login::GrokAuth;
mod oai;
pub(crate) trait ModelSource {
    /// Identifies this source in the models disk cache, so entries fetched from one URL never load for another.
    fn cache_origin(&self) -> String;
    fn fetch(&self, auth: Option<&GrokAuth>) -> Result<FetchModelsResult, BackendError>;
}
pub(crate) fn active_model_source(
    endpoints: &EndpointsConfig,
    fetch_auth: ModelFetchAuth,
) -> impl ModelSource {
    oai::OaiModelSource::new(endpoints, fetch_auth)
}
