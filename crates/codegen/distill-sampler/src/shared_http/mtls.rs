// Modified for Distill by Samuel Fajreldines, 2026.
//! Builds and caches HTTP clients for model-specific mTLS identities.
//!
//! Cache keys include the certificate and private-key contents so rotated credentials create a
//! new client without retaining unbounded client state. Credentialed clients require HTTPS and
//! disable redirects to keep the identity scoped to its configured origin.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use distill_sampling_types::SamplingError;

use super::{configure_http1, configure_http2, sharing_disabled};

const MAX_MTLS_IDENTITY_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MTLS_CLIENTS: usize = 16;

#[derive(Clone, PartialEq, Eq, Hash)]
struct MtlsClientCacheKey {
    identity_digest: [u8; 32],
    force_http1: bool,
}

struct MtlsClientCacheEntry {
    client: reqwest::Client,
    last_used: u64,
}

#[derive(Default)]
struct MtlsClientCache {
    tick: u64,
    entries: HashMap<MtlsClientCacheKey, MtlsClientCacheEntry>,
}

pub(crate) fn client(
    cert_dir: &Path,
    force_http1: bool,
) -> distill_sampling_types::Result<reqwest::Client> {
    let (cert_path, key_path) = identity_paths(cert_dir)?;
    let cert_pem = read_identity_file(&cert_path)?;
    let key_pem = read_identity_file(&key_path)?;
    let cache_key = client_cache_key(&cert_pem, &key_pem, force_http1);
    let mut identity_pem = cert_pem;
    if !identity_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(&key_pem);
    let build = || {
        let identity = reqwest::Identity::from_pem(&identity_pem).map_err(|error| {
            SamplingError::MtlsConfiguration(format!(
                "invalid mTLS client identity from certificate '{}' and key '{}': {}",
                cert_path.display(),
                key_path.display(),
                error_detail(&error)
            ))
        })?;

        let built = if force_http1 {
            distill_extra_ca::build_reqwest_client(|builder| {
                configure_http1(builder)
                    .identity(identity.clone())
                    .https_only(true)
                    .redirect(reqwest::redirect::Policy::none())
            })
        } else {
            distill_extra_ca::build_reqwest_client(|builder| {
                configure_http2(builder)
                    .identity(identity.clone())
                    .https_only(true)
                    .redirect(reqwest::redirect::Policy::none())
            })
        };
        built.map_err(|error| {
            SamplingError::MtlsConfiguration(format!(
                "failed to build mTLS HTTP client from certificate '{}' and key '{}': {}",
                cert_path.display(),
                key_path.display(),
                error_detail(&error)
            ))
        })
    };
    if sharing_disabled() {
        build()
    } else {
        cached_client(cache_key, build)
    }
}

fn client_cache_key(cert_pem: &[u8], key_pem: &[u8], force_http1: bool) -> MtlsClientCacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(cert_pem.len() as u64).to_le_bytes());
    hasher.update(cert_pem);
    hasher.update(&(key_pem.len() as u64).to_le_bytes());
    hasher.update(key_pem);
    MtlsClientCacheKey {
        identity_digest: *hasher.finalize().as_bytes(),
        force_http1,
    }
}

fn cached_client<E>(
    key: MtlsClientCacheKey,
    build: impl FnOnce() -> Result<reqwest::Client, E>,
) -> Result<reqwest::Client, E> {
    static CACHE: OnceLock<Mutex<MtlsClientCache>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache.tick = cache.tick.wrapping_add(1);
    let tick = cache.tick;
    if let Some(entry) = cache.entries.get_mut(&key) {
        entry.last_used = tick;
        return Ok(entry.client.clone());
    }

    let client = build()?;
    if cache.entries.len() >= MAX_MTLS_CLIENTS
        && let Some(lru) = cache
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
    {
        cache.entries.remove(&lru);
    }
    cache.entries.insert(
        key,
        MtlsClientCacheEntry {
            client: client.clone(),
            last_used: tick,
        },
    );
    Ok(client)
}

fn identity_paths(cert_dir: &Path) -> distill_sampling_types::Result<(PathBuf, PathBuf)> {
    let client_cert = cert_dir.join("client.crt");
    let client_key = cert_dir.join("client.key");
    let has_client_cert = try_path_exists(&client_cert)?;
    let has_client_key = try_path_exists(&client_key)?;
    if has_client_cert || has_client_key {
        Ok((client_cert, client_key))
    } else {
        Ok((cert_dir.join("tls.crt"), cert_dir.join("tls.key")))
    }
}

fn try_path_exists(path: &Path) -> distill_sampling_types::Result<bool> {
    path.try_exists().map_err(|error| {
        SamplingError::MtlsConfiguration(format!(
            "failed to inspect mTLS identity file '{}': {}",
            path.display(),
            error_detail(&error)
        ))
    })
}

fn read_identity_file(path: &Path) -> distill_sampling_types::Result<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(|error| {
        SamplingError::MtlsConfiguration(format!(
            "failed to open mTLS identity file '{}': {}",
            path.display(),
            error_detail(&error)
        ))
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MTLS_IDENTITY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            SamplingError::MtlsConfiguration(format!(
                "failed to read mTLS identity file '{}': {}",
                path.display(),
                error_detail(&error)
            ))
        })?;
    if bytes.len() as u64 > MAX_MTLS_IDENTITY_FILE_BYTES {
        return Err(SamplingError::MtlsConfiguration(format!(
            "mTLS identity file '{}' exceeds the {} byte limit",
            path.display(),
            MAX_MTLS_IDENTITY_FILE_BYTES
        )));
    }
    Ok(bytes)
}

fn error_detail(error: &dyn std::error::Error) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    detail
}

#[allow(clippy::disallowed_methods)] // Tests intentionally build isolated reqwest clients.
#[cfg(test)]
#[path = "mtls_tests.rs"]
mod tests;
