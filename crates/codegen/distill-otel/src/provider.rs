// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::{Arc, OnceLock};

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Layer as _;
use tracing_subscriber::registry::LookupSpan;
use distill_auth::AuthCredentialProvider;

use crate::config::{OtelClientInfo, OtelLayerConfig};

static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub enum OtelProviderMode {
    Server,
    Local,
}

pub type SessionMetricsGate = Arc<dyn Fn() -> bool + Send + Sync>;

/// Redacts each span batch in place before export. The domain layer injects the allowlist,
/// so this foundation crate holds no product-specific field policy.
pub type SpanRedactor = Arc<dyn Fn(&mut [opentelemetry_sdk::trace::SpanData]) + Send + Sync>;

const ENV_OTEL_FILTER: &str = "GROK_OTEL_FILTER";
const DEFAULT_OTEL_FILTER: &str = "info";

pub fn build_otel_layer<S>(
    client: OtelClientInfo,
    config: OtelLayerConfig,
    mode: OtelProviderMode,
    session_metrics_gate: SessionMetricsGate,
    redact: SpanRedactor,
) -> impl tracing_subscriber::layer::Layer<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    let provider = TRACER_PROVIDER
        .get_or_init(|| build_tracer_provider(client, config, mode, session_metrics_gate, redact));
    let tracer = provider.tracer("grok-cli");

    global::set_tracer_provider(provider.clone());

    global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());

    let otel_filter =
        std::env::var(ENV_OTEL_FILTER).unwrap_or_else(|_| DEFAULT_OTEL_FILTER.to_string());
    let otel_filter = tracing_subscriber::filter::EnvFilter::try_new(&otel_filter)
        .unwrap_or_else(|e| {
            eprintln!(
                "[otel] Invalid GROK_OTEL_FILTER '{}': {}. Using default '{}'.",
                otel_filter, e, DEFAULT_OTEL_FILTER
            );

            tracing_subscriber::filter::EnvFilter::try_new(DEFAULT_OTEL_FILTER)
                .expect("default otel filter must parse")
        })
        .add_directive(
            "sampling_log=off"
                .parse()
                .expect("static directive must parse"),
        );

    OpenTelemetryLayer::new(tracer)
        .with_context_activation(false)
        .with_filter(otel_filter)
}

fn build_tracer_provider(
    client: OtelClientInfo,
    config: OtelLayerConfig,
    mode: OtelProviderMode,
    session_metrics_gate: SessionMetricsGate,
    redact: SpanRedactor,
) -> SdkTracerProvider {
    match mode {
        OtelProviderMode::Server => {
            build_server_provider(client, config, session_metrics_gate, redact)
        }
        OtelProviderMode::Local => SdkTracerProvider::builder().build(),
    }
}

struct RefreshableSpanExporter {
    endpoint: Arc<str>,
    static_headers: Arc<std::collections::HashMap<String, String>>,
    credentials: Arc<dyn AuthCredentialProvider>,
    last_token: parking_lot::Mutex<String>,
    http_client: crate::otlp::BlockingOtlpClient,
    resource: parking_lot::Mutex<opentelemetry_sdk::Resource>,
    token_header_value: Arc<str>,
    extra_headers: Arc<Vec<(String, String)>>,
    session_metrics_gate: SessionMetricsGate,
    redact: SpanRedactor,
}

impl std::fmt::Debug for RefreshableSpanExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshableSpanExporter")
            .field("endpoint", &self.endpoint)
            .field("static_headers", &self.static_headers)
            .field("credentials", &"configured")
            .field("last_token", &"***")
            .finish_non_exhaustive()
    }
}

fn build_otlp_exporter(
    endpoint: &str,
    static_headers: &std::collections::HashMap<String, String>,
    token: &str,
    token_auth_header: Option<&str>,
    extra_headers: &[(String, String)],
    http_client: crate::otlp::BlockingOtlpClient,
    snapshot: &distill_auth::CredentialSnapshot,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    let headers = crate::config::build_export_headers(
        static_headers,
        token,
        token_auth_header,
        extra_headers,
        snapshot,
    );
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .build()
}

async fn export_batch(
    exporter: &mut opentelemetry_otlp::SpanExporter,
    resource: &opentelemetry_sdk::Resource,
    batch: Vec<opentelemetry_sdk::trace::SpanData>,
) -> opentelemetry_sdk::error::OTelSdkResult {
    use opentelemetry_sdk::trace::SpanExporter as _;
    exporter.set_resource(resource);
    exporter.export(batch).await
}

struct ExportInputs {
    one_shot: Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError>,
    resource: opentelemetry_sdk::Resource,
    credentials: Arc<dyn AuthCredentialProvider>,
    endpoint: Arc<str>,
    static_headers: Arc<std::collections::HashMap<String, String>>,
    token_header_value: Arc<str>,
    http_client: crate::otlp::BlockingOtlpClient,
    extra_headers: Arc<Vec<(String, String)>>,
}

impl opentelemetry_sdk::trace::SpanExporter for RefreshableSpanExporter {
    fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send {
        let redact = Arc::clone(&self.redact);
        let prepared = ((self.session_metrics_gate)() && self.credentials.has_usable_credential())
            .then(|| {
                let snapshot = self.credentials.snapshot();
                let token = snapshot.token.clone().unwrap_or_else(|| {
                    tracing::debug!(
                        "auth: otel credential snapshot has no token, using cached last_token"
                    );
                    self.last_token.lock().clone()
                });
                *self.last_token.lock() = token.clone();
                let token_auth = self
                    .credentials
                    .needs_token_auth_header()
                    .then(|| Arc::clone(&self.token_header_value));
                ExportInputs {
                    one_shot: build_otlp_exporter(
                        &self.endpoint,
                        &self.static_headers,
                        &token,
                        token_auth.as_deref(),
                        &self.extra_headers,
                        self.http_client.clone(),
                        &snapshot,
                    ),

                    resource: crate::config::resource_with_tenant_id(
                        self.resource.lock().clone(),
                        &snapshot,
                    ),
                    credentials: Arc::clone(&self.credentials),
                    endpoint: Arc::clone(&self.endpoint),
                    static_headers: Arc::clone(&self.static_headers),
                    token_header_value: Arc::clone(&self.token_header_value),
                    http_client: self.http_client.clone(),
                    extra_headers: Arc::clone(&self.extra_headers),
                }
            });
        async move {
            let Some(ExportInputs {
                one_shot,
                resource,
                credentials,
                endpoint,
                static_headers,
                token_header_value,
                http_client,
                extra_headers,
            }) = prepared
            else {
                return Ok(());
            };
            let mut exporter = match one_shot {
                Ok(e) => e,
                Err(e) => {
                    return Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                        format!("failed to build exporter: {e}"),
                    ));
                }
            };

            let mut batch = batch;
            redact(&mut batch);

            let batch_for_retry = tokio::runtime::Handle::try_current()
                .is_ok()
                .then(|| batch.clone());
            let result = export_batch(&mut exporter, &resource, batch).await;
            if result.is_ok() {
                return result;
            }

            let Some(batch_for_retry) = batch_for_retry else {
                return result;
            };
            tracing::debug!("otel export failed, attempting token refresh");
            if !credentials.refresh_after_unauthorized().await {
                return result;
            }
            let retry_snapshot = credentials.snapshot();
            let new_token = retry_snapshot.token.clone().unwrap_or_default();
            if new_token.is_empty() {
                tracing::warn!("token refresh reported success but snapshot returned no token");
                return result;
            }
            let retry_token_auth = credentials
                .needs_token_auth_header()
                .then(|| token_header_value.as_ref());
            match build_otlp_exporter(
                &endpoint,
                &static_headers,
                &new_token,
                retry_token_auth,
                &extra_headers,
                http_client,
                &retry_snapshot,
            ) {
                Ok(mut retry_exporter) => {
                    let retry_resource =
                        crate::config::resource_with_tenant_id(resource, &retry_snapshot);
                    export_batch(&mut retry_exporter, &retry_resource, batch_for_retry)
                        .await
                        .or(result)
                }
                Err(e) => {
                    tracing::debug!("failed to build retry exporter: {e}");
                    result
                }
            }
        }
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        *self.resource.lock() = resource.clone();
    }

    fn shutdown(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

fn build_server_provider(
    client: OtelClientInfo,
    config: OtelLayerConfig,
    session_metrics_gate: SessionMetricsGate,
    redact: SpanRedactor,
) -> SdkTracerProvider {
    let snapshot = config.credentials.snapshot();
    let initial_token = snapshot.token.unwrap_or_default();
    if initial_token.is_empty() {
        tracing::debug!(
            "No authentication credentials found at init. OTLP exporter will retry after auth."
        );
    }

    let mut provider =
        SdkTracerProvider::builder().with_resource(crate::config::build_base_resource(client));

    if config.exporter.enabled {
        let traces_url = config.exporter.traces_url;
        let static_headers = crate::config::build_static_headers(
            client.client_version,
            config.alpha_test_key,
            &traces_url,
        );

        let timeout = config
            .exporter
            .timeout
            .unwrap_or(crate::config::DEFAULT_EXPORT_TIMEOUT);

        let http_client = match crate::otlp::build_blocking_client(timeout, &[]) {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(error = %err, "otel: OTLP HTTP client build failed; span export disabled");
                return provider.build();
            }
        };

        let refreshable_exporter = RefreshableSpanExporter {
            endpoint: Arc::from(traces_url),
            static_headers: Arc::new(static_headers),
            credentials: config.credentials,
            last_token: parking_lot::Mutex::new(initial_token),
            http_client,

            resource: parking_lot::Mutex::new(opentelemetry_sdk::Resource::builder_empty().build()),
            token_header_value: Arc::from(config.token_header_value.as_str()),
            extra_headers: Arc::new(config.exporter.extra_headers),
            session_metrics_gate,
            redact,
        };

        let mut batch_builder = opentelemetry_sdk::trace::BatchConfigBuilder::default()
            .with_max_export_batch_size(crate::config::MAX_EXPORT_BATCH_SIZE)
            .with_max_queue_size(crate::config::MAX_QUEUE_SIZE);
        if let Some(interval) = config.exporter.export_interval {
            batch_builder = batch_builder.with_scheduled_delay(interval);
        }
        let batch_processor =
            opentelemetry_sdk::trace::BatchSpanProcessor::builder(refreshable_exporter)
                .with_batch_config(batch_builder.build())
                .build();

        provider = provider.with_span_processor(batch_processor);
    }
    provider.build()
}

pub fn shutdown_provider() {
    let Some(provider) = TRACER_PROVIDER.get() else {
        return;
    };
    let started = std::time::Instant::now();
    crate::timeout::run_with_timeout(
        "otel provider",
        crate::timeout::OTEL_SHUTDOWN_TIMEOUT,
        move || {
            if let Err(e) = provider.force_flush() {
                tracing::debug!("[otel] Failed to flush tracer provider: {}", e);
            }
            if let Err(e) = provider.shutdown() {
                tracing::debug!("[otel] Failed to shutdown tracer provider: {}", e);
            }
        },
    );
    tracing::debug!(
        "[otel] provider shutdown took {}ms",
        started.elapsed().as_millis()
    );
}
