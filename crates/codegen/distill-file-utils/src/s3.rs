// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashSet;
use std::path::Path;

use anyhow::Context;
use futures::StreamExt;

/// Map an S3 HeadObject error to NotFound / Unauthorized / ProbeFailed.
/// `ServiceError` and `ResponseError` both carry a status; everything else
/// (construction, dispatch, timeout) is transient.
fn classify_head_error<E>(err: &aws_sdk_s3::error::SdkError<E>) -> HeadOutcome
where
    E: std::fmt::Debug,
{
    use aws_sdk_s3::error::SdkError;
    let status = match err {
        SdkError::ServiceError(ctx) => Some(ctx.raw().status().as_u16()),
        SdkError::ResponseError(ctx) => Some(ctx.raw().status().as_u16()),
        _ => None,
    };
    match status {
        Some(404) => HeadOutcome::NotFound,
        Some(401) | Some(403) => HeadOutcome::Unauthorized,
        _ => HeadOutcome::ProbeFailed,
    }
}

#[derive(Debug)]
enum HeadOutcome {
    NotFound,
    Unauthorized,
    ProbeFailed,
}

/// Some S3-compatible endpoints reject single PutObject chunks above 16 MiB.
/// Use multipart upload with 8 MiB parts to stay within that limit.
const MULTIPART_THRESHOLD: usize = 8 * 1024 * 1024;
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// Parse credential content (JSON or INI format) into AWS SDK credentials.
fn parse_aws_credentials(content: &str) -> anyhow::Result<aws_sdk_s3::config::Credentials> {
    #[derive(serde::Deserialize)]
    struct JsonCreds {
        aws_access_key_id: String,
        aws_secret_access_key: String,
        #[serde(default)]
        aws_session_token: Option<String>,
    }

    if let Ok(parsed) = serde_json::from_str::<JsonCreds>(content) {
        return Ok(aws_sdk_s3::config::Credentials::new(
            &parsed.aws_access_key_id,
            &parsed.aws_secret_access_key,
            parsed.aws_session_token,
            None,
            "grok-shell-trace-upload",
        ));
    }

    let strip_comment = |v: &str| {
        v.split_once('#')
            .map_or(v, |(before, _)| before)
            .trim()
            .to_owned()
    };
    let mut key_id = None;
    let mut secret = None;
    let mut token = None;
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "aws_access_key_id" => key_id = Some(strip_comment(v)),
                "aws_secret_access_key" => secret = Some(strip_comment(v)),
                "aws_session_token" => token = Some(strip_comment(v)),
                _ => {}
            }
        }
    }

    match (key_id, secret) {
        (Some(k), Some(s)) => Ok(aws_sdk_s3::config::Credentials::new(
            &k,
            &s,
            token,
            None,
            "grok-shell-trace-upload",
        )),
        _ => anyhow::bail!(
            "AWS credentials are neither valid JSON \
             nor contain aws_access_key_id and aws_secret_access_key"
        ),
    }
}

/// Build an S3 client. Uses path-style addressing when `endpoint_url` is set.
/// Reads proxy env vars so S3 traffic can route through a corporate HTTP proxy.
pub(crate) async fn build_s3_client(
    region: &str,
    credentials_content: Option<&str>,
    credentials_file: Option<&str>,
    endpoint_url: Option<&str>,
) -> anyhow::Result<aws_sdk_s3::Client> {
    let proxy_config = aws_smithy_http_client::proxy::ProxyConfig::from_env();
    let tls_context = extra_ca_tls_context()?;
    let http_client = aws_smithy_http_client::Builder::new().build_with_connector_fn(
        move |settings, _runtime_components| {
            let mut builder =
                aws_smithy_http_client::Connector::builder().proxy_config(proxy_config.clone());
            if let Some(s) = settings {
                builder.set_connector_settings(Some(s.clone()));
            }
            builder
                .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                    aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
                ))
                .tls_context(tls_context.clone())
                .build()
        },
    );

    let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .http_client(http_client)
        .region(aws_config::Region::new(region.to_owned()));

    let resolved_content = match (credentials_content, credentials_file) {
        (Some(inline), _) => Some(inline.to_owned()),
        (None, Some(path)) => Some(
            tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read AWS credentials file: {path}"))?,
        ),
        (None, None) => None,
    };

    if let Some(ref content) = resolved_content {
        config_loader = config_loader.credentials_provider(parse_aws_credentials(content)?);
    } else if endpoint_url.is_some() {
        config_loader = config_loader.credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test",
            "test",
            None,
            None,
            "grok-shell-test",
        ));
    }

    let sdk_config = config_loader.load().await;
    let mut builder =
        aws_sdk_s3::config::Builder::from(&sdk_config).force_path_style(endpoint_url.is_some());
    if let Some(url) = endpoint_url {
        builder = builder.endpoint_url(url);
    }
    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}

fn extra_ca_tls_context() -> anyhow::Result<aws_smithy_http_client::tls::TlsContext> {
    use aws_smithy_http_client::tls::{TlsContext, TrustStore};
    let mut trust_store = TrustStore::default();
    for pem in distill_extra_ca::extra_root_pems() {
        trust_store = trust_store.with_pem_certificate(pem.clone());
    }
    TlsContext::builder()
        .with_trust_store(trust_store)
        .build()
        .context("build S3 TLS context with extra CA roots")
}

/// Static access-key credentials for presigning S3 URLs.
///
/// `Debug` is intentionally redacted — the struct holds plaintext secrets.
#[derive(Clone)]
pub struct S3StaticCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for S3StaticCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3StaticCredentials")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .finish()
    }
}

impl S3StaticCredentials {
    fn to_credentials_content(&self) -> String {
        serde_json::json!({
            "aws_access_key_id": self.access_key_id,
            "aws_secret_access_key": self.secret_access_key,
        })
        .to_string()
    }
}

pub async fn presign_put_url(
    region: &str,
    endpoint_url: Option<&str>,
    creds: &S3StaticCredentials,
    bucket: &str,
    key: &str,
    content_type: &str,
    expires_in: std::time::Duration,
) -> anyhow::Result<String> {
    let content = creds.to_credentials_content();
    let client = build_s3_client(region, Some(&content), None, endpoint_url).await?;
    let presigning_config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)?;
    let presigned = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .content_type(content_type)
        .presigned(presigning_config)
        .await?;
    Ok(presigned.uri().to_string())
}

pub async fn presign_get_url(
    region: &str,
    endpoint_url: Option<&str>,
    creds: &S3StaticCredentials,
    bucket: &str,
    key: &str,
    expires_in: std::time::Duration,
) -> anyhow::Result<String> {
    let content = creds.to_credentials_content();
    let client = build_s3_client(region, Some(&content), None, endpoint_url).await?;
    let presigning_config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)?;
    let presigned = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .presigned(presigning_config)
        .await?;
    Ok(presigned.uri().to_string())
}

/// Aborts a created multipart upload when its future is dropped before completion.
/// Callers cancel by drop; a dropped future skips Complete and the error-path abort.
/// That leaves orphaned billable parts in the (possibly customer-managed) bucket.
struct AbortMultipartOnDrop {
    client: aws_sdk_s3::Client,
    bucket: String,
    key: String,
    upload_id: String,
    armed: bool,
}

impl AbortMultipartOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }

    /// In-scope abort for the error path: awaited, unlike the drop hook.
    /// Disarms only after the send completes, so cancelling this await still gets the drop-hook abort.
    /// AbortMultipartUpload is idempotent server-side.
    async fn abort(mut self) {
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .send()
            .await;
        self.armed = false;
    }
}

impl Drop for AbortMultipartOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Detached: a drop site cannot await. Bounded, since the cancellation
        // often means the endpoint is already unresponsive.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let abort = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id);
        handle.spawn(async move {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), abort.send()).await;
        });
    }
}

/// Multipart upload for payloads that exceed [`MULTIPART_THRESHOLD`].
/// Splits into [`MULTIPART_PART_SIZE`] chunks. Aborts on any part failure or cancellation
/// so incomplete multipart uploads are not leaked.
async fn multipart_upload_bytes(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    object_path: &str,
    content: &[u8],
    content_type: &str,
) -> anyhow::Result<()> {
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(object_path)
        .content_type(content_type)
        .send()
        .await
        .with_context(|| {
            format!("Failed to create multipart upload for s3://{bucket}/{object_path}")
        })?;

    let upload_id = create
        .upload_id()
        .context("CreateMultipartUpload response missing upload_id")?
        .to_owned();
    let abort_guard = AbortMultipartOnDrop {
        client: client.clone(),
        bucket: bucket.to_owned(),
        key: object_path.to_owned(),
        upload_id: upload_id.clone(),
        armed: true,
    };

    let mut completed_parts = Vec::new();
    let mut offset = 0usize;
    let mut part_number = 1i32;

    let result: anyhow::Result<()> = async {
        while offset < content.len() {
            let end = (offset + MULTIPART_PART_SIZE).min(content.len());
            let Some(chunk) = content.get(offset..end) else {
                break;
            };

            let upload_part = client
                .upload_part()
                .bucket(bucket)
                .key(object_path)
                .upload_id(&upload_id)
                .part_number(part_number)
                .body(aws_sdk_s3::primitives::ByteStream::from(chunk.to_vec()))
                .send()
                .await
                .with_context(|| {
                    format!("Failed to upload part {part_number} for s3://{bucket}/{object_path}")
                })?;

            let etag = upload_part
                .e_tag()
                .context("UploadPart response missing ETag")?
                .to_owned();

            completed_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build(),
            );

            offset = end;
            part_number += 1;
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(object_path)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .with_context(|| {
                format!("Failed to complete multipart upload for s3://{bucket}/{object_path}")
            })?;

        Ok(())
    }
    .await;

    if result.is_err() {
        abort_guard.abort().await;
    } else {
        abort_guard.disarm();
    }

    result
}

/// Upload bytes to an S3-compatible bucket.
pub async fn upload_bytes(
    bucket: &str,
    object_path: &str,
    content: &[u8],
    content_type: &str,
    region: &str,
    credentials_content: Option<&str>,
    credentials_file: Option<&str>,
    endpoint_url: Option<&str>,
) -> anyhow::Result<String> {
    let client =
        build_s3_client(region, credentials_content, credentials_file, endpoint_url).await?;
    if content.len() >= MULTIPART_THRESHOLD {
        multipart_upload_bytes(&client, bucket, object_path, content, content_type).await?;
    } else {
        client
            .put_object()
            .bucket(bucket)
            .key(object_path)
            .content_type(content_type)
            .body(aws_sdk_s3::primitives::ByteStream::from(content.to_vec()))
            .send()
            .await
            .with_context(|| format!("Failed to upload to s3://{bucket}/{object_path}"))?;
    }
    Ok(format!("s3://{bucket}/{object_path}"))
}

/// Upload a file to an S3-compatible bucket by streaming from disk.
pub async fn upload_file(
    bucket: &str,
    object_path: &str,
    file_path: &Path,
    content_type: &str,
    region: &str,
    credentials_content: Option<&str>,
    credentials_file: Option<&str>,
    endpoint_url: Option<&str>,
) -> anyhow::Result<String> {
    let client =
        build_s3_client(region, credentials_content, credentials_file, endpoint_url).await?;
    let file_size = tokio::fs::metadata(file_path)
        .await
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    if file_size >= MULTIPART_THRESHOLD {
        let content = tokio::fs::read(file_path).await.with_context(|| {
            format!("Failed to read file for S3 upload: {}", file_path.display())
        })?;
        multipart_upload_bytes(&client, bucket, object_path, &content, content_type).await?;
    } else {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(file_path)
            .await
            .with_context(|| {
                format!("Failed to open file for S3 upload: {}", file_path.display())
            })?;
        client
            .put_object()
            .bucket(bucket)
            .key(object_path)
            .content_type(content_type)
            .body(body)
            .send()
            .await
            .with_context(|| format!("Failed to upload to s3://{bucket}/{object_path}"))?;
    }
    Ok(format!("s3://{bucket}/{object_path}"))
}

/// Upload an async reader to S3.
/// Buffers first because S3 PutObject requires a known Content-Length.
/// Acceptable for the primary caller: zstd-compressed dedup blobs are typically small.
pub async fn upload_stream<R: tokio::io::AsyncRead + Send + Sync + 'static>(
    bucket: &str,
    object_path: &str,
    reader: R,
    content_type: &str,
    region: &str,
    credentials_content: Option<&str>,
    credentials_file: Option<&str>,
    endpoint_url: Option<&str>,
) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    tokio::pin!(reader);
    reader.read_to_end(&mut buf).await?;

    upload_bytes(
        bucket,
        object_path,
        &buf,
        content_type,
        region,
        credentials_content,
        credentials_file,
        endpoint_url,
    )
    .await
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Used once the S3 storage backend is wired up.
pub struct S3ExistsResponse {
    pub bucket: String,
    pub path: String,
    pub size: i64,
}

/// S3-native storage client providing batch operations via concurrent SDK calls.
/// Caches the AWS SDK `Client` for the lifetime of the struct.
#[allow(dead_code)] // Used once the S3 storage backend is wired up.
pub struct S3StorageClient {
    client: aws_sdk_s3::Client,
    bucket: String,
}

#[allow(dead_code)] // Used once the S3 storage backend is wired up.
impl S3StorageClient {
    pub fn bucket_name(&self) -> &str {
        &self.bucket
    }

    pub async fn new(
        bucket: String,
        region: &str,
        credentials_content: Option<&str>,
        credentials_file: Option<&str>,
        endpoint_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let client =
            build_s3_client(region, credentials_content, credentials_file, endpoint_url).await?;
        tracing::debug!(bucket = %bucket, region, endpoint = ?endpoint_url, "S3StorageClient created");
        Ok(Self { client, bucket })
    }

    /// Check existence of multiple S3 objects via concurrent HeadObject calls.
    /// Aggregates worst-first: 401/403 → `Unauthorized`, non-404 transient → `ProbeFailed`, else found/not.
    pub async fn batch_check_exists<S: AsRef<str>>(
        &self,
        paths: &[S],
    ) -> crate::storage_client::ExistsResult<HashSet<String>> {
        use crate::storage_client::ExistsResult;
        let client = &self.client;
        let bucket = &*self.bucket;

        // Collect into owned strings up front: HeadObject requires owned keys
        // and this keeps closures HRTB-clean for callers passing both
        // `&[String]` and `&[&str]` from the same async fn.
        let owned_paths: Vec<String> = paths
            .iter()
            .map(|p| <S as AsRef<str>>::as_ref(p).to_string())
            .collect();
        let total = owned_paths.len();

        #[derive(Debug)]
        enum PathOutcome {
            Exists(String),
            NotFound,
            Unauthorized,
            ProbeFailed,
        }

        let outcomes: Vec<PathOutcome> = futures::stream::iter(owned_paths)
            .map(|path| async move {
                match client.head_object().bucket(bucket).key(&path).send().await {
                    Ok(_) => PathOutcome::Exists(path),
                    Err(e) => match classify_head_error(&e) {
                        HeadOutcome::NotFound => PathOutcome::NotFound,
                        HeadOutcome::Unauthorized => PathOutcome::Unauthorized,
                        HeadOutcome::ProbeFailed => PathOutcome::ProbeFailed,
                    },
                }
            })
            .buffer_unordered(32)
            .collect()
            .await;

        let mut found = HashSet::new();
        let mut not_found_count: usize = 0;
        let mut any_unauthorized = false;
        let mut any_transient = false;
        for o in outcomes {
            match o {
                PathOutcome::Exists(p) => {
                    found.insert(p);
                }
                PathOutcome::NotFound => not_found_count += 1,
                PathOutcome::Unauthorized => any_unauthorized = true,
                PathOutcome::ProbeFailed => any_transient = true,
            }
        }
        if any_unauthorized {
            ExistsResult::Unauthorized
        } else if any_transient {
            ExistsResult::ProbeFailed
        } else if total > 0 && not_found_count == total {
            // Symmetric with the proxy: all-404 batch surfaces as NotFound,
            // not Found(empty_set).
            ExistsResult::NotFound
        } else {
            ExistsResult::Found(found)
        }
    }

    /// Upload multiple small files via concurrent PutObject calls.
    /// Returns the proxy-compatible `BatchUploadResult` so downstream handling stays unchanged.
    pub async fn batch_upload(
        &self,
        files: Vec<(String, Vec<u8>, String)>,
    ) -> Option<Vec<prod_mc_cli_chat_proxy_types::BatchUploadResult>> {
        let client = &self.client;
        let bucket = &*self.bucket;
        let results: Vec<prod_mc_cli_chat_proxy_types::BatchUploadResult> = futures::stream::iter(
            files,
        )
        .map(|(path, content, content_type)| async move {
            let size = content.len() as i64;
            let bucket_owned = bucket.to_string();
            let upload_result = if content.len() >= MULTIPART_THRESHOLD {
                multipart_upload_bytes(client, bucket, &path, &content, &content_type).await
            } else {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(&path)
                    .content_type(&content_type)
                    .body(aws_sdk_s3::primitives::ByteStream::from(content))
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!(e))
            };
            match upload_result {
                Ok(_) => prod_mc_cli_chat_proxy_types::BatchUploadResult {
                    path,
                    bucket: Some(bucket_owned),
                    status: prod_mc_cli_chat_proxy_types::BatchUploadStatus::Ok,
                    size: Some(size),
                    generation: None,
                    error: None,
                },
                Err(e) => {
                    let error_msg = format!("{:#}", e);
                    tracing::warn!(path = %path, error = %error_msg, "S3 batch upload item failed");
                    prod_mc_cli_chat_proxy_types::BatchUploadResult {
                        path,
                        bucket: Some(bucket_owned),
                        status: prod_mc_cli_chat_proxy_types::BatchUploadStatus::Error,
                        size: None,
                        generation: None,
                        error: Some(error_msg),
                    }
                }
            }
        })
        .buffer_unordered(16)
        .collect()
        .await;

        Some(results)
    }

    /// Check if a single object exists via HeadObject.
    pub async fn check_exists(&self, path: &str) -> Option<S3ExistsResponse> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
        {
            Ok(output) => Some(S3ExistsResponse {
                bucket: self.bucket.clone(),
                path: path.to_string(),
                size: output.content_length().unwrap_or(0),
            }),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::Path as AxumPath,
        http::StatusCode,
        response::IntoResponse,
        routing::{head, put},
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// In-progress multipart uploads: upload_id -> (key, parts: part_number -> bytes).
    type MultipartUploads = HashMap<String, (String, HashMap<i32, Vec<u8>>)>;

    /// Shared state for the mock S3 server.
    struct MockS3State {
        objects: RwLock<HashMap<String, Vec<u8>>>,
        multipart_uploads: RwLock<MultipartUploads>,
    }

    fn xml_response(status: u16, body: String) -> axum::response::Response {
        axum::http::Response::builder()
            .status(status)
            .header("content-type", "application/xml")
            .body(axum::body::Body::from(body))
            .unwrap()
            .into_response()
    }

    /// Mock S3 server with PUT/GET/HEAD/POST/DELETE and multipart upload support.
    fn mock_s3_router() -> (Router, Arc<MockS3State>) {
        let state = Arc::new(MockS3State {
            objects: RwLock::new(HashMap::new()),
            multipart_uploads: RwLock::new(HashMap::new()),
        });

        let s = state.clone();
        let head_handler = move |AxumPath((_, key)): AxumPath<(String, String)>| {
            let state = s.clone();
            async move {
                match state.objects.read().await.get(&key) {
                    Some(data) => (StatusCode::OK, [("content-length", data.len().to_string())])
                        .into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
        };

        let s = state.clone();
        let put_handler = move |AxumPath((_, key)): AxumPath<(String, String)>,
                                query: axum::extract::Query<HashMap<String, String>>,
                                body: axum::body::Bytes| {
            let state = s.clone();
            async move {
                if let (Some(pn), Some(uid)) = (query.get("partNumber"), query.get("uploadId")) {
                    let part_num: i32 = pn.parse().unwrap_or(0);
                    let mut uploads = state.multipart_uploads.write().await;
                    if let Some((_, parts)) = uploads.get_mut(uid) {
                        parts.insert(part_num, body.to_vec());
                        return xml_response(200, "<UploadPartResult/>".into());
                    }
                    return StatusCode::NOT_FOUND.into_response();
                }
                if body.len() > 16 * 1024 * 1024 {
                    return (StatusCode::BAD_REQUEST, "chunk too big").into_response();
                }
                state.objects.write().await.insert(key, body.to_vec());
                StatusCode::OK.into_response()
            }
        };

        let s = state.clone();
        let get_handler = move |AxumPath((_, key)): AxumPath<(String, String)>| {
            let state = s.clone();
            async move {
                match state.objects.read().await.get(&key) {
                    Some(data) => (StatusCode::OK, data.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
        };

        let s = state.clone();
        let post_handler = move |AxumPath((_, key)): AxumPath<(String, String)>,
                                 query: axum::extract::Query<HashMap<String, String>>,
                                 body: axum::body::Bytes| {
            let state = s.clone();
            async move {
                if query.contains_key("uploads") {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static CTR: AtomicU64 = AtomicU64::new(0);
                    let uid = format!("upload-{}", CTR.fetch_add(1, Ordering::Relaxed));
                    state
                        .multipart_uploads
                        .write()
                        .await
                        .insert(uid.clone(), (key.clone(), HashMap::new()));
                    return xml_response(
                        200,
                        format!(
                            "<InitiateMultipartUploadResult><UploadId>{uid}</UploadId></InitiateMultipartUploadResult>"
                        ),
                    );
                }
                if let Some(uid) = query.get("uploadId") {
                    let mut uploads = state.multipart_uploads.write().await;
                    if let Some((stored_key, parts)) = uploads.remove(uid) {
                        let mut sorted: Vec<_> = parts.into_iter().collect();
                        sorted.sort_by_key(|(n, _)| *n);
                        let combined: Vec<u8> = sorted.into_iter().flat_map(|(_, d)| d).collect();
                        state.objects.write().await.insert(stored_key, combined);
                        return xml_response(200, "<CompleteMultipartUploadResult/>".into());
                    }
                    return StatusCode::NOT_FOUND.into_response();
                }
                let _ = body;
                StatusCode::BAD_REQUEST.into_response()
            }
        };

        let s = state.clone();
        let delete_handler =
            move |AxumPath((_, key)): AxumPath<(String, String)>,
                  query: axum::extract::Query<HashMap<String, String>>| {
                let state = s.clone();
                async move {
                    if let Some(uid) = query.get("uploadId") {
                        state.multipart_uploads.write().await.remove(uid);
                    } else {
                        state.objects.write().await.remove(&key);
                    }
                    StatusCode::NO_CONTENT
                }
            };

        let router = Router::new().route(
            "/{bucket}/{*key}",
            head(head_handler)
                .put(put_handler)
                .get(get_handler)
                .post(post_handler)
                .delete(delete_handler),
        );
        (router, state)
    }

    /// Start the mock server and return (endpoint_url, state).
    async fn start_mock_server() -> (String, Arc<MockS3State>) {
        let (router, state) = mock_s3_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    async fn make_test_client(endpoint_url: &str) -> S3StorageClient {
        S3StorageClient::new(
            "test-bucket".to_string(),
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(endpoint_url),
        )
        .await
        .unwrap()
    }

    /// Start a mock server that rejects PUT for keys containing "fail".
    async fn start_mock_server_rejecting_fail_keys() -> String {
        let put_handler = move |AxumPath((_, key)): AxumPath<(String, String)>,
                                _body: axum::body::Bytes| async move {
            if key.contains("fail") {
                StatusCode::FORBIDDEN.into_response()
            } else {
                StatusCode::OK.into_response()
            }
        };

        let router = Router::new().route("/{bucket}/{*key}", put(put_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn unwrap_found(r: crate::storage_client::ExistsResult<HashSet<String>>) -> HashSet<String> {
        match r {
            crate::storage_client::ExistsResult::Found(s) => s,
            other => panic!("expected Found, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn batch_check_exists_returns_existing_paths() {
        let (endpoint, state) = start_mock_server().await;
        {
            let mut objects = state.objects.write().await;
            objects.insert("file-a.txt".into(), b"hello".to_vec());
            objects.insert("file-c.txt".into(), b"world".to_vec());
        }

        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec![
            "file-a.txt".into(),
            "file-b.txt".into(),
            "file-c.txt".into(),
        ];

        let result = unwrap_found(client.batch_check_exists(&paths).await);
        assert_eq!(result.len(), 2);
        assert!(result.contains("file-a.txt"));
        assert!(result.contains("file-c.txt"));
        assert!(!result.contains("file-b.txt"));
    }

    #[tokio::test]
    async fn batch_check_exists_empty_input() {
        let (endpoint, _state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;

        let paths: &[String] = &[];
        let result = unwrap_found(client.batch_check_exists(paths).await);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn batch_check_exists_all_missing() {
        let (endpoint, _state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec!["missing-1".into(), "missing-2".into()];

        // All-404 collapses to top-level NotFound, symmetric with the proxy.
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::NotFound),
            "all-404 batch must map to NotFound, got {:?}",
            result
        );
    }

    /// Build a HEAD-only mock router that returns `responder(key)` per key.
    async fn start_head_mock<F>(responder: F) -> String
    where
        F: Fn(String) -> axum::http::StatusCode + Clone + Send + Sync + 'static,
    {
        use axum::routing::head as axum_head;

        let h = move |AxumPath((_, key)): AxumPath<(String, String)>| {
            let responder = responder.clone();
            async move {
                let code = responder(key);
                code.into_response()
            }
        };
        let router = Router::new().route("/{bucket}/{*key}", axum_head(h));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn batch_check_exists_403_maps_to_unauthorized() {
        let endpoint = start_head_mock(|_| axum::http::StatusCode::FORBIDDEN).await;
        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec!["any.txt".into()];
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::Unauthorized),
            "403 HEAD must map to Unauthorized, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn batch_check_exists_401_maps_to_unauthorized() {
        let endpoint = start_head_mock(|_| axum::http::StatusCode::UNAUTHORIZED).await;
        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec!["any.txt".into()];
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::Unauthorized),
            "401 HEAD must map to Unauthorized, got {:?}",
            result
        );
    }

    /// Mixed `[Found, NotFound, 5xx]` → `ProbeFailed`; transient dominates.
    #[tokio::test]
    async fn batch_check_exists_mixed_with_5xx_maps_to_probe_failed() {
        let endpoint = start_head_mock(|key| {
            if key == "exists.txt" {
                axum::http::StatusCode::OK
            } else if key == "missing.txt" {
                axum::http::StatusCode::NOT_FOUND
            } else {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            }
        })
        .await;
        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec!["exists.txt".into(), "missing.txt".into(), "boom.txt".into()];
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::ProbeFailed),
            "any 5xx must dominate, got {:?}",
            result
        );
    }

    /// Mixed `[Found, 5xx, 401]` → `Unauthorized`; auth outranks transient.
    #[tokio::test]
    async fn batch_check_exists_mixed_with_401_maps_to_unauthorized() {
        let endpoint = start_head_mock(|key| {
            if key == "exists.txt" {
                axum::http::StatusCode::OK
            } else if key == "transient.txt" {
                axum::http::StatusCode::BAD_GATEWAY
            } else {
                axum::http::StatusCode::UNAUTHORIZED
            }
        })
        .await;
        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec![
            "exists.txt".into(),
            "transient.txt".into(),
            "no-auth.txt".into(),
        ];
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::Unauthorized),
            "any 401 must dominate transient, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn batch_check_exists_transient_error_maps_to_probe_failed() {
        // Spin up a server whose HEAD returns 500 for everything.
        use axum::http::StatusCode;
        use axum::routing::head as axum_head;

        let head_handler = move |AxumPath((_, _key)): AxumPath<(String, String)>| async move {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        };
        let router = Router::new().route("/{bucket}/{*key}", axum_head(head_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let endpoint = format!("http://{addr}");

        let client = make_test_client(&endpoint).await;
        let paths: Vec<String> = vec!["x".into()];
        let result = client.batch_check_exists(&paths).await;
        assert!(
            matches!(result, crate::storage_client::ExistsResult::ProbeFailed),
            "5xx HEAD must map to ProbeFailed, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn batch_upload_all_succeed() {
        let (endpoint, state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;

        let files = vec![
            (
                "upload-a.txt".into(),
                b"content-a".to_vec(),
                "text/plain".into(),
            ),
            (
                "upload-b.bin".into(),
                b"content-b".to_vec(),
                "application/octet-stream".into(),
            ),
        ];

        let results = client.batch_upload(files).await.unwrap();
        assert_eq!(results.len(), 2);
        for r in &results {
            assert_eq!(
                r.status,
                prod_mc_cli_chat_proxy_types::BatchUploadStatus::Ok
            );
            assert!(r.error.is_none());
            assert_eq!(r.bucket.as_deref(), Some("test-bucket"));
        }

        let objects = state.objects.read().await;
        assert_eq!(objects.get("upload-a.txt").unwrap(), b"content-a");
        assert_eq!(objects.get("upload-b.bin").unwrap(), b"content-b");
    }

    #[tokio::test]
    async fn batch_upload_empty_input() {
        let (endpoint, _state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;

        let results = client.batch_upload(vec![]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn check_exists_found() {
        let (endpoint, state) = start_mock_server().await;
        state
            .objects
            .write()
            .await
            .insert("existing.txt".into(), b"data123".to_vec());

        let client = make_test_client(&endpoint).await;
        let resp = client.check_exists("existing.txt").await.unwrap();
        assert_eq!(resp.bucket, "test-bucket");
        assert_eq!(resp.path, "existing.txt");
        assert_eq!(resp.size, 7);
    }

    #[tokio::test]
    async fn check_exists_not_found() {
        let (endpoint, _state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;

        assert!(client.check_exists("nonexistent.txt").await.is_none());
    }

    #[tokio::test]
    async fn batch_upload_then_batch_exists_roundtrip() {
        let (endpoint, _state) = start_mock_server().await;
        let client = make_test_client(&endpoint).await;

        let files = vec![
            ("rt-a.txt".into(), b"a".to_vec(), "text/plain".into()),
            ("rt-b.txt".into(), b"b".to_vec(), "text/plain".into()),
        ];
        client.batch_upload(files).await.unwrap();

        let paths: Vec<String> = vec!["rt-a.txt".into(), "rt-b.txt".into(), "rt-c.txt".into()];
        let existing = unwrap_found(client.batch_check_exists(&paths).await);
        assert_eq!(existing.len(), 2);
        assert!(existing.contains("rt-a.txt"));
        assert!(existing.contains("rt-b.txt"));
    }

    #[tokio::test]
    async fn batch_upload_partial_failure() {
        let endpoint = start_mock_server_rejecting_fail_keys().await;
        let client = make_test_client(&endpoint).await;

        let files = vec![
            ("good.txt".into(), b"ok".to_vec(), "text/plain".into()),
            ("fail-item.txt".into(), b"bad".to_vec(), "text/plain".into()),
            (
                "also-good.txt".into(),
                b"fine".to_vec(),
                "text/plain".into(),
            ),
        ];

        let results = client.batch_upload(files).await.unwrap();
        assert_eq!(results.len(), 3);

        let by_path: HashMap<&str, &prod_mc_cli_chat_proxy_types::BatchUploadResult> =
            results.iter().map(|r| (r.path.as_str(), r)).collect();

        let Some(good) = by_path.get("good.txt").copied() else {
            panic!("missing good.txt: {by_path:?}");
        };
        assert_eq!(
            good.status,
            prod_mc_cli_chat_proxy_types::BatchUploadStatus::Ok
        );
        assert!(good.size.is_some());
        assert!(good.error.is_none());

        let Some(fail) = by_path.get("fail-item.txt").copied() else {
            panic!("missing fail-item.txt: {by_path:?}");
        };
        assert_eq!(
            fail.status,
            prod_mc_cli_chat_proxy_types::BatchUploadStatus::Error
        );
        assert!(fail.size.is_none());
        assert!(fail.error.is_some());

        let Some(also_good) = by_path.get("also-good.txt").copied() else {
            panic!("missing also-good.txt: {by_path:?}");
        };
        assert_eq!(
            also_good.status,
            prod_mc_cli_chat_proxy_types::BatchUploadStatus::Ok
        );
    }

    #[tokio::test]
    async fn new_returns_error_for_invalid_credentials_file() {
        let result = S3StorageClient::new(
            "test-bucket".to_string(),
            "us-east-1",
            None,
            Some("/nonexistent/path/to/credentials"),
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upload_stream_sends_content() {
        let (endpoint, state) = start_mock_server().await;
        let content = b"streamed content here";
        let reader = std::io::Cursor::new(content.to_vec());

        let result = upload_stream(
            "test-bucket",
            "stream-test.txt",
            reader,
            "text/plain",
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(&endpoint),
        )
        .await
        .unwrap();

        assert_eq!(result, "s3://test-bucket/stream-test.txt");
        let objects = state.objects.read().await;
        assert_eq!(objects.get("stream-test.txt").unwrap(), content);
    }

    #[tokio::test]
    async fn upload_stream_large_content() {
        let (endpoint, state) = start_mock_server().await;

        // 100 KB — larger payload exercising multi-chunk ReaderStream reads
        let content: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let reader = std::io::Cursor::new(content.clone());

        let result = upload_stream(
            "test-bucket",
            "large-stream.bin",
            reader,
            "application/octet-stream",
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(&endpoint),
        )
        .await
        .unwrap();

        assert_eq!(result, "s3://test-bucket/large-stream.bin");
        let objects = state.objects.read().await;
        assert_eq!(objects.get("large-stream.bin").unwrap(), &content);
    }

    #[test]
    fn multipart_threshold_is_under_16mib() {
        const MAX_CHUNK: usize = 16 * 1024 * 1024;
        const _: () = assert!(MULTIPART_THRESHOLD <= MAX_CHUNK);
        const _: () = assert!(MULTIPART_PART_SIZE <= MAX_CHUNK);
    }

    #[tokio::test]
    async fn upload_bytes_uses_single_put_for_small_content() {
        let (endpoint, state) = start_mock_server().await;

        let content = b"small payload".to_vec();
        upload_bytes(
            "test-bucket",
            "small-single.txt",
            &content,
            "text/plain",
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(&endpoint),
        )
        .await
        .unwrap();

        let objects = state.objects.read().await;
        assert_eq!(objects.get("small-single.txt").unwrap(), &content);
        assert!(state.multipart_uploads.read().await.is_empty());
    }
    /// Regression: dropping a multipart upload mid-part must still abort the upload.
    /// A dropped future skips Complete and the error-path abort, leaving orphaned billable parts.
    #[tokio::test]
    async fn dropped_multipart_upload_aborts_the_upload() {
        use axum::extract::Query;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TarpitState {
            part_started: tokio::sync::Notify,
            abort_seen: tokio::sync::Notify,
            aborted: AtomicBool,
        }
        let state = Arc::new(TarpitState {
            part_started: tokio::sync::Notify::new(),
            abort_seen: tokio::sync::Notify::new(),
            aborted: AtomicBool::new(false),
        });

        let s = state.clone();
        let put_handler = move |AxumPath((_, _key)): AxumPath<(String, String)>,
                                _query: Query<HashMap<String, String>>,
                                _body: axum::body::Bytes| {
            let state = s.clone();
            async move {
                // Tarpit the part upload so the caller's deadline fires mid-part.
                state.part_started.notify_one();
                std::future::pending::<()>().await;
                StatusCode::OK.into_response()
            }
        };
        let post_handler = move |AxumPath((_, _key)): AxumPath<(String, String)>,
                                 query: Query<HashMap<String, String>>| async move {
            if query.contains_key("uploads") {
                return xml_response(
                    200,
                    "<InitiateMultipartUploadResult><UploadId>tarpit-upload</UploadId></InitiateMultipartUploadResult>".into(),
                );
            }
            StatusCode::BAD_REQUEST.into_response()
        };
        let s = state.clone();
        let delete_handler =
            move |AxumPath((_, _key)): AxumPath<(String, String)>,
                  query: Query<HashMap<String, String>>| {
                let state = s.clone();
                async move {
                    if query.contains_key("uploadId") {
                        state.aborted.store(true, Ordering::Relaxed);
                        state.abort_seen.notify_one();
                    }
                    StatusCode::NO_CONTENT.into_response()
                }
            };
        let router = Router::new()
            .route(
                "/{bucket}/{*key}",
                axum::routing::put(put_handler)
                    .post(post_handler)
                    .delete(delete_handler),
            )
            // The 8 MiB part must reach the tarpit handler, not a 413.
            .layer(axum::extract::DefaultBodyLimit::max(
                2 * MULTIPART_PART_SIZE,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = build_s3_client(
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(&format!("http://{addr}")),
        )
        .await
        .unwrap();
        let content = vec![0u8; MULTIPART_THRESHOLD];
        let mut upload = Box::pin(multipart_upload_bytes(
            &client,
            "test-bucket",
            "obj.bin",
            &content,
            "application/octet-stream",
        ));
        // Drive the upload until the part request is in flight, then drop it —
        // the caller-side deadline-cancellation shape.
        tokio::select! {
            _ = &mut upload => panic!("tarpitted part upload must not complete"),
            _ = state.part_started.notified() => {}
        }
        drop(upload);

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            state.abort_seen.notified(),
        )
        .await
        .expect("dropped multipart upload must send AbortMultipartUpload");
        assert!(state.aborted.load(Ordering::Relaxed));
    }

    /// Regression: cancelling the awaited error-path abort mid-send must leave
    /// the drop backstop armed, so the abort still reaches the bucket.
    #[tokio::test]
    async fn cancelled_error_path_abort_still_aborts_via_drop_backstop() {
        use axum::extract::Query;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AbortTarpitState {
            abort_requests: AtomicUsize,
            first_abort_started: tokio::sync::Notify,
            second_abort_seen: tokio::sync::Notify,
        }
        let state = Arc::new(AbortTarpitState {
            abort_requests: AtomicUsize::new(0),
            first_abort_started: tokio::sync::Notify::new(),
            second_abort_seen: tokio::sync::Notify::new(),
        });

        // Part upload fails outright, driving the in-scope error-path abort.
        let put_handler = move |AxumPath((_, _key)): AxumPath<(String, String)>,
                                _query: Query<HashMap<String, String>>,
                                _body: axum::body::Bytes| async move {
            StatusCode::FORBIDDEN.into_response()
        };
        let post_handler = move |AxumPath((_, _key)): AxumPath<(String, String)>,
                                 query: Query<HashMap<String, String>>| async move {
            if query.contains_key("uploads") {
                return xml_response(
                    200,
                    "<InitiateMultipartUploadResult><UploadId>abort-tarpit</UploadId></InitiateMultipartUploadResult>".into(),
                );
            }
            StatusCode::BAD_REQUEST.into_response()
        };
        let s = state.clone();
        let delete_handler =
            move |AxumPath((_, _key)): AxumPath<(String, String)>,
                  query: Query<HashMap<String, String>>| {
                let state = s.clone();
                async move {
                    if query.contains_key("uploadId") {
                        let n = state.abort_requests.fetch_add(1, Ordering::Relaxed) + 1;
                        if n == 1 {
                            // Tarpit the first (awaited) abort so the caller's
                            // deadline can drop the future mid-send.
                            state.first_abort_started.notify_one();
                            std::future::pending::<()>().await;
                        } else {
                            state.second_abort_seen.notify_one();
                        }
                    }
                    StatusCode::NO_CONTENT.into_response()
                }
            };
        let router = Router::new()
            .route(
                "/{bucket}/{*key}",
                axum::routing::put(put_handler)
                    .post(post_handler)
                    .delete(delete_handler),
            )
            .layer(axum::extract::DefaultBodyLimit::max(
                2 * MULTIPART_PART_SIZE,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = build_s3_client(
            "us-east-1",
            Some(r#"{"aws_access_key_id":"test","aws_secret_access_key":"test"}"#),
            None,
            Some(&format!("http://{addr}")),
        )
        .await
        .unwrap();
        let content = vec![0u8; MULTIPART_THRESHOLD];
        let mut upload = Box::pin(multipart_upload_bytes(
            &client,
            "test-bucket",
            "obj.bin",
            &content,
            "application/octet-stream",
        ));
        // Drive until the awaited error-path abort is in flight, then drop —
        // the deadline cancellation landing mid-abort.
        tokio::select! {
            _ = &mut upload => panic!("upload must park on the tarpitted abort"),
            _ = state.first_abort_started.notified() => {}
        }
        drop(upload);

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            state.second_abort_seen.notified(),
        )
        .await
        .expect("an abort cancelled mid-send must re-send via the drop backstop");
    }
}
