//! Optional OpenRouter facts. Routing reads memory; refreshes happen in the background.
//! A profile-local disk cache and nonblocking file lock share refreshes across processes.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const API: &str = "https://openrouter.ai/api/v1";
const DAY: u64 = 86_400;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    fetched_at: u64,
    attempted_at: u64,
    data: Value,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Snapshot {
    entries: BTreeMap<String, Entry>,
}

#[derive(Default)]
struct State {
    snapshot: Arc<Snapshot>,
    checked: Option<Instant>,
    refreshing: bool,
}

struct Cache {
    path: PathBuf,
    state: Mutex<State>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn ttl(key: &str) -> u64 {
    if key.starts_with("models/") {
        15 * 60
    } else {
        DAY
    }
}

impl Entry {
    fn due(&self, key: &str, time: u64) -> bool {
        (self.fetched_at == 0 || time.saturating_sub(self.fetched_at) >= ttl(key))
            && (self.attempted_at == 0
                || time.saturating_sub(self.attempted_at) >= 3600.min(ttl(key)))
    }

    fn usable(&self, key: &str, time: u64) -> bool {
        self.fetched_at > 0
            && time.saturating_sub(self.fetched_at)
                <= if key.starts_with("models/") {
                    3600
                } else {
                    7 * DAY
                }
    }
}

impl Cache {
    fn new(path: PathBuf) -> Self {
        let snapshot = Arc::new(read_snapshot(&path).unwrap_or_default());
        Self {
            path,
            state: Mutex::new(State {
                snapshot,
                ..State::default()
            }),
        }
    }

    fn snapshot(self: &Arc<Self>, candidates: Vec<String>) -> Arc<Snapshot> {
        let mut state = self.state.lock();
        let snapshot = state.snapshot.clone();
        if !state.refreshing
            && state
                .checked
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            state.checked = Some(Instant::now());
            state.refreshing = true;
            let cache = self.clone();
            tokio::spawn(async move {
                // Resolve auth only for the optional authenticated endpoint. Public metadata
                // remains useful without an account; no sign-in prompt or credential reuse.
                let key = crate::openrouter_auth::api_key().ok().flatten();
                let result = refresh(&cache.path, API, key.as_deref(), &candidates, now()).await;
                let mut state = cache.state.lock();
                if let Ok(snapshot) = result {
                    state.snapshot = Arc::new(snapshot);
                }
                state.refreshing = false;
            });
        }
        snapshot
    }
}

fn read_snapshot(path: &Path) -> std::io::Result<Snapshot> {
    let file = std::fs::File::open(path)?;
    serde_json::from_reader(file.take(MAX_BYTES as u64)).map_err(std::io::Error::other)
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing cache directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(file.as_file_mut(), snapshot).map_err(std::io::Error::other)?;
    file.flush()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

async fn refresh(
    path: &Path,
    api: &str,
    key: Option<&str>,
    candidates: &[String],
    time: u64,
) -> anyhow::Result<Snapshot> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing cache directory"))?;
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(read_snapshot(path).unwrap_or_default());
    }
    let mut snapshot = read_snapshot(path).unwrap_or_default();
    let mut requests = vec!["models".to_owned()];
    if key.is_some() {
        requests.push("benchmarks".to_owned());
    }
    // Only configured OpenRouter candidates need provider performance information.
    requests.extend(
        candidates
            .iter()
            .take(3)
            .map(|id| format!("models/{id}/endpoints")),
    );
    requests.dedup();
    for resource in requests {
        if !snapshot
            .entries
            .entry(resource.clone())
            .or_default()
            .due(&resource, time)
        {
            continue;
        }
        snapshot
            .entries
            .entry(resource.clone())
            .or_default()
            .attempted_at = time;
        // Persist the backoff before I/O, including failures and process restarts.
        write_snapshot(path, &snapshot)?;
        if let Ok(data) = fetch(api, &resource, key).await {
            snapshot.entries.insert(
                resource.clone(),
                Entry {
                    data,
                    fetched_at: time,
                    attempted_at: time,
                },
            );
            write_snapshot(path, &snapshot)?;
        }
    }
    Ok(snapshot)
}

async fn fetch(api: &str, resource: &str, key: Option<&str>) -> anyhow::Result<Value> {
    let mut request = crate::http::shared_client()
        .get(format!("{api}/{resource}"))
        .timeout(Duration::from_secs(8));
    if resource == "benchmarks" {
        request = request
            .bearer_auth(key.ok_or_else(|| anyhow::anyhow!("no benchmark credential"))?)
            .query(&[("source", "artificial-analysis")]);
    }
    let mut response = request.send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "metadata response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&bytes)?;
    normalize(resource, &body).ok_or_else(|| anyhow::anyhow!("invalid model metadata"))
}

// Never forward descriptions or arbitrary remote strings as instructions to Jev.
fn fields(value: &Value, names: &[&str]) -> Value {
    Value::Object(
        names
            .iter()
            .filter_map(|name| value.get(*name).cloned().map(|v| ((*name).to_owned(), v)))
            .collect(),
    )
}

fn normalize(resource: &str, body: &Value) -> Option<Value> {
    if resource == "models" {
        let items = body.get("data")?.as_array()?;
        let models: Vec<_> = items
            .iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_str()?;
                if !valid_slug(id) {
                    return None;
                }
                let mut value = fields(
                    item,
                    &[
                        "id",
                        "canonical_slug",
                        "context_length",
                        "supported_parameters",
                        "expiration_date",
                    ],
                );
                value["input_modalities"] = item
                    .pointer("/architecture/input_modalities")
                    .cloned()
                    .unwrap_or(Value::Null);
                value["pricing"] = fields(
                    &item["pricing"],
                    &[
                        "prompt",
                        "completion",
                        "input_cache_read",
                        "input_cache_write",
                        "request",
                    ],
                );
                value["benchmarks"] = fields(
                    &item["benchmarks"]["artificial_analysis"],
                    &["coding_index", "agentic_index", "intelligence_index"],
                );
                Some(value)
            })
            .collect();
        if !items.is_empty() && models.is_empty() {
            return None;
        }
        Some(Value::Array(models))
    } else if resource == "benchmarks" {
        let items = body.get("data")?.as_array()?;
        Some(json!({
            "as_of": body.pointer("/meta/as_of"),
            "scores": items.iter().filter(|item| item["source"] == "artificial-analysis")
                .map(|item| fields(item, &["model_permaslug", "coding_index", "agentic_index", "intelligence_index"]))
                .collect::<Vec<_>>()
        }))
    } else {
        let items = body.pointer("/data/endpoints")?.as_array()?;
        Some(Value::Array(
            items
                .iter()
                .filter(|item| item["status"] == 0)
                .take(3)
                .map(|item| {
                    let mut value = fields(
                        item,
                        &[
                            "provider_name",
                            "context_length",
                            "max_completion_tokens",
                            "supports_implicit_caching",
                            "supported_parameters",
                            "uptime_last_30m",
                        ],
                    );
                    value["pricing"] = fields(
                        &item["pricing"],
                        &[
                            "prompt",
                            "completion",
                            "input_cache_read",
                            "input_cache_write",
                        ],
                    );
                    value["latency_p50_seconds"] = item
                        .pointer("/latency_last_30m/p50")
                        .cloned()
                        .unwrap_or(Value::Null);
                    value["throughput_p50_tokens_per_second"] = item
                        .pointer("/throughput_last_30m/p50")
                        .cloned()
                        .unwrap_or(Value::Null);
                    value
                })
                .collect(),
        ))
    }
}

fn valid_slug(id: &str) -> bool {
    id.len() <= 160
        && id.split('/').count() == 2
        && !id.contains("..")
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/-_.:".contains(&c))
}

fn catalog_id(model: &str, base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    // Exact wire IDs only. Never infer equivalence from a display name, family, or prefix.
    if crate::openrouter_auth::is_openrouter_url(base_url) && valid_slug(model) {
        return Some(model.to_owned());
    }
    let namespace = match url.host_str()? {
        "api.openai.com" | "chatgpt.com" => "openai",
        "api.x.ai" => "x-ai",
        "api.anthropic.com" => "anthropic",
        _ => return None,
    };
    let id = format!("{namespace}/{model}");
    valid_slug(&id).then_some(id)
}

impl Snapshot {
    fn facts(&self, model: &str, base_url: &str, time: u64) -> Value {
        let Some(id) = catalog_id(model, base_url) else {
            return Value::Null;
        };
        let catalog = self
            .entries
            .get("models")
            .filter(|e| e.usable("models", time));
        let record = catalog
            .and_then(|e| e.data.as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id));
        let canonical = record
            .and_then(|item| item["canonical_slug"].as_str())
            .unwrap_or(&id);
        let benchmark_entry = self
            .entries
            .get("benchmarks")
            .filter(|e| e.usable("benchmarks", time));
        let scores = benchmark_entry
            .and_then(|e| e.data["scores"].as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["model_permaslug"] == canonical)
            });
        let mut facts = json!({"catalog_id": id});
        if let Some(record) = record {
            facts["catalog_age_seconds"] = json!(time.saturating_sub(catalog.unwrap().fetched_at));
            facts["benchmarks"] = record["benchmarks"].clone();
        }
        if let Some(scores) = scores {
            facts["benchmarks"] = fields(
                scores,
                &["coding_index", "agentic_index", "intelligence_index"],
            );
            facts["benchmark_as_of"] = benchmark_entry.unwrap().data["as_of"].clone();
            facts["benchmark_age_seconds"] =
                json!(time.saturating_sub(benchmark_entry.unwrap().fetched_at));
        }
        if facts.get("benchmarks").is_some() {
            facts["benchmark_source"] = json!("Artificial Analysis via OpenRouter");
            facts["benchmark_effort"] = Value::Null;
        }
        // Provider-specific prices, limits and capabilities must never leak into
        // decisions about a direct provider or subscription with different terms.
        if crate::openrouter_auth::is_openrouter_url(base_url) {
            if let Some(record) = record {
                facts["openrouter"] = fields(
                    record,
                    &[
                        "pricing",
                        "context_length",
                        "supported_parameters",
                        "input_modalities",
                        "expiration_date",
                    ],
                );
            }
            let key = format!("models/{id}/endpoints");
            if let Some(entry) = self.entries.get(&key).filter(|e| e.usable(&key, time)) {
                facts["openrouter_endpoints"] = entry.data.clone();
                facts["endpoints_age_seconds"] = json!(time.saturating_sub(entry.fetched_at));
            }
        }
        if facts.as_object().is_none_or(|object| object.len() == 1) {
            Value::Null
        } else {
            facts
        }
    }
}

/// No request contents are sent to OpenRouter: only the public model IDs are fetched.
pub(crate) fn model_facts(candidates: &[(&str, &str)]) -> Vec<Value> {
    #[cfg(test)]
    {
        return candidates.iter().map(|_| Value::Null).collect();
    }
    #[cfg(not(test))]
    {
        static CACHE: std::sync::OnceLock<Arc<Cache>> = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(|| {
            Arc::new(Cache::new(
                crate::util::distill_home::distill_home().join("jev/model-facts-v1.json"),
            ))
        });
        let endpoint_ids = candidates
            .iter()
            .filter(|(_, url)| crate::openrouter_auth::is_openrouter_url(url))
            .filter_map(|(model, url)| catalog_id(model, url))
            .collect();
        let snapshot = cache.snapshot(endpoint_ids);
        let time = now();
        candidates
            .iter()
            .map(|(model, url)| snapshot.facts(model, url, time))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse,
        routing::get,
    };

    fn catalog() -> Value {
        json!({"data": [{
            "id": "openai/test-model", "canonical_slug": "openai/test-model-v1",
            "context_length": 128000, "supported_parameters": ["tools", "reasoning"],
            "pricing": {"prompt": "0.000002", "completion": "0.00001", "input_cache_read": "0.000001"},
            "architecture": {"input_modalities": ["text"]},
            "description": "not routing instructions",
            "benchmarks": {"artificial_analysis": {"coding_index": 60.0, "agentic_index": 50.0}}
        }]})
    }

    #[tokio::test]
    async fn cache_survives_restart_without_auth_and_backs_off_failures() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let capture = seen.clone();
        let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
            let capture = capture.clone();
            async move {
                assert!(
                    !headers.contains_key("authorization"),
                    "public facts need no key"
                );
                capture.lock().push(uri.path().to_owned());
                if uri.path() == "/models" && capture.lock().len() == 1 {
                    (StatusCode::OK, axum::Json(catalog())).into_response()
                } else {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.json");
        let time = 1_800_000_000;
        let (first, concurrent) = tokio::join!(
            refresh(&path, &base, None, &[], time),
            refresh(&path, &base, None, &[], time)
        );
        first.unwrap();
        concurrent.unwrap();
        assert_eq!(seen.lock().as_slice(), &["/models"]);
        let cache = Cache::new(path.clone());
        assert_eq!(
            cache
                .state
                .lock()
                .snapshot
                .facts("openai/test-model", API, time)["benchmarks"]["coding_index"],
            60.0
        );
        refresh(&path, &base, None, &[], time + 60).await.unwrap();
        assert_eq!(
            seen.lock().len(),
            1,
            "fresh disk cache must avoid another request"
        );
        let stale = refresh(&path, &base, None, &[], time + DAY).await.unwrap();
        assert_eq!(seen.lock().len(), 2);
        assert!(
            !stale.facts("openai/test-model", API, time + DAY).is_null(),
            "failed refresh retains useful data"
        );
        refresh(&path, &base, None, &[], time + DAY + 30)
            .await
            .unwrap();
        assert_eq!(
            seen.lock().len(),
            2,
            "failure backoff persists across refreshes"
        );
        assert!(
            stale
                .facts("openai/test-model", API, time + 8 * DAY)
                .is_null()
        );
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("not routing instructions")
        );
        std::fs::write(&path, b"broken json").unwrap();
        assert!(Cache::new(path).state.lock().snapshot.entries.is_empty());
        server.abort();
    }

    #[test]
    fn facts_match_exact_models_and_do_not_price_subscriptions_as_openrouter() {
        let time = 1_800_000_000;
        let mut snapshot = Snapshot::default();
        snapshot.entries.insert(
            "models".into(),
            Entry {
                fetched_at: time,
                data: normalize("models", &catalog()).unwrap(),
                ..Entry::default()
            },
        );
        let subscription =
            snapshot.facts("test-model", "https://chatgpt.com/backend-api/codex", time);
        assert_eq!(subscription["benchmarks"]["agentic_index"], 50.0);
        assert!(subscription.get("openrouter").is_none());
        assert!(subscription["benchmark_effort"].is_null());
        assert!(
            snapshot
                .facts(
                    "test-model-v2",
                    "https://chatgpt.com/backend-api/codex",
                    time
                )
                .is_null()
        );
        assert!(
            snapshot
                .facts("test-model", "https://unrelated.example/v1", time)
                .is_null()
        );
        assert_eq!(
            snapshot.facts("openai/test-model", API, time)["openrouter"]["pricing"]["prompt"],
            "0.000002"
        );
        snapshot.entries.insert("benchmarks".into(), Entry {
            fetched_at: time, data: normalize("benchmarks", &json!({
                "data": [{"source": "artificial-analysis", "model_permaslug": "openai/test-model-v1", "coding_index": 70.0}],
                "meta": {"as_of": "2026-09-21"}
            })).unwrap(), ..Entry::default()
        });
        assert_eq!(
            snapshot.facts("openai/test-model", API, time)["benchmarks"]["coding_index"],
            70.0
        );
        assert!(!valid_slug("openai/../secrets"));
    }
    #[tokio::test]
    async fn benchmarks_use_auth_but_public_endpoints_have_a_shorter_cache() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let capture = seen.clone();
        let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
            let capture = capture.clone();
            async move {
                capture.lock().push(uri.path().to_owned());
                let body = match uri.path() {
                    "/benchmarks" => {
                        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-benchmark-key");
                        assert_eq!(uri.query(), Some("source=artificial-analysis"));
                        json!({"data": [{"source": "artificial-analysis", "model_permaslug": "openai/test-model-v1", "coding_index": 75}], "meta": {"as_of": "2026-09-21"}})
                    }
                    "/models" => { assert!(!headers.contains_key("authorization")); catalog() }
                    "/models/openai/test-model/endpoints" => {
                        assert!(!headers.contains_key("authorization"));
                        json!({"data": {"endpoints": [{"status": 0, "provider_name": "Example", "latency_last_30m": {"p50": 0.5}, "uptime_last_30m": 99.9}]}})
                    }
                    _ => panic!("unexpected metadata path"),
                };
                axum::Json(body)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.json");
        let candidates = vec!["openai/test-model".to_owned()];
        let time = 1_800_000_000;
        let snapshot = refresh(&path, &base, Some("test-benchmark-key"), &candidates, time)
            .await
            .unwrap();
        let facts = snapshot.facts("openai/test-model", API, time);
        assert_eq!(facts["benchmarks"]["coding_index"], 75);
        assert_eq!(facts["openrouter_endpoints"][0]["latency_p50_seconds"], 0.5);
        assert_eq!(seen.lock().len(), 3);
        refresh(
            &path,
            &base,
            Some("test-benchmark-key"),
            &candidates,
            time + 901,
        )
        .await
        .unwrap();
        assert_eq!(
            seen.lock().len(),
            4,
            "only endpoint performance expires after 15 minutes"
        );
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("test-benchmark-key")
        );
        server.abort();
    }
}
