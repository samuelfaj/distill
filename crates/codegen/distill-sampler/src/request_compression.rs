//! zstd compression of JSON request bodies. The shell decides whether an
//! endpoint may receive compressed bodies (`SamplerConfig::request_compression`);
//! this module decides whether a given body is worth compressing and does it.

use crate::config::RequestCompression;

/// Below this the CPU spent compressing is not worth the bytes saved.
pub(crate) const MIN_COMPRESS_BYTES: usize = 64 * 1024;

/// Level 3: fast, good ratio on JSON, and a 2 MiB window that stays well
/// inside the proxy's decoder bound.
const ZSTD_LEVEL: i32 = 3;

/// From here up, compression leaves the async worker: level-3 zstd runs at a
/// few hundred MB/s, so a multi-megabyte body would stall it for tens of ms.
const OFFLOAD_COMPRESS_BYTES: usize = 1024 * 1024;

pub(crate) fn should_compress(config: RequestCompression, body_len: usize) -> bool {
    config == RequestCompression::Zstd && body_len >= MIN_COMPRESS_BYTES
}

/// Compress `json` for the wire, off the runtime for large bodies. `None`
/// means compression failed and the caller should send the plain bytes.
pub(crate) async fn compress_body(json: &[u8]) -> Option<Vec<u8>> {
    let result = if json.len() >= OFFLOAD_COMPRESS_BYTES {
        let owned = json.to_vec();
        match tokio::task::spawn_blocking(move || zstd_compress(&owned)).await {
            Ok(result) => result,
            Err(join) => Err(std::io::Error::other(join)),
        }
    } else {
        zstd_compress(json)
    };
    result
        .inspect_err(|error| tracing::warn!(%error, "zstd compression failed; sending plain JSON"))
        .ok()
}

pub(crate) fn zstd_compress(json: &[u8]) -> std::io::Result<Vec<u8>> {
    let started = std::time::Instant::now();
    let compressed = zstd::encode_all(json, ZSTD_LEVEL)?;
    // Info, not debug: the `--debug` firehose keeps this crate at info and a support log must show whether a body was compressed.
    tracing::info!(
        pre_compression_bytes = json.len(),
        post_compression_bytes = compressed.len(),
        compression_duration_ms = started.elapsed().as_millis() as u64,
        "compressed request body with zstd"
    );
    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compress_requires_zstd_config_and_min_size() {
        assert!(should_compress(
            RequestCompression::Zstd,
            MIN_COMPRESS_BYTES
        ));
        assert!(!should_compress(
            RequestCompression::Zstd,
            MIN_COMPRESS_BYTES - 1
        ));
        assert!(!should_compress(
            RequestCompression::None,
            MIN_COMPRESS_BYTES * 4
        ));
    }
}
