// Modified for Distill by Samuel Fajreldines, 2026.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{pin_env, send_one, settle_pool, test_config};
use distill_sampler::{PrewarmOutcome, SamplingClient, prewarm_transport};
use distill_test_support::counting_server::spawn_http_server;
use distill_test_support::spawn_counting_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prewarm_wire_lifecycle() {
    pin_env();

    const ATTEMPTS: usize = 8;
    for attempt in 0..ATTEMPTS {
        let (base_url, accepts, heads) = spawn_counting_server().await;

        assert_eq!(
            prewarm_transport(&base_url).await.outcome,
            PrewarmOutcome::Warmed,
            "warm phase: a fresh origin must dial and warm"
        );
        assert_eq!(
            prewarm_transport(&base_url).await.outcome,
            PrewarmOutcome::AlreadyClaimed,
            "warm phase: a warmed origin must not be re-dialed"
        );
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "warm phase: exactly one dial reaches the origin"
        );
        {
            let heads = heads.lock().unwrap();
            assert_eq!(
                heads.len(),
                1,
                "warm phase: repeat prewarm must send nothing"
            );
            assert!(
                heads[0].starts_with("GET / HTTP/1.1"),
                "warm phase: prewarm must be a bare GET: {}",
                heads[0]
            );
            let lower = heads[0].to_ascii_lowercase();
            assert!(
                !lower.contains("content-length"),
                "warm phase: prewarm must carry no body: {}",
                heads[0]
            );
            assert!(
                !lower.contains("authorization"),
                "warm phase: prewarm must not leak credentials: {}",
                heads[0]
            );
            assert!(
                !lower.contains("x-api-key"),
                "warm phase: prewarm must not leak credentials: {}",
                heads[0]
            );
        }

        settle_pool().await;
        send_one(&SamplingClient::new(test_config(&base_url, "prewarm-token")).unwrap()).await;
        if accepts.load(Ordering::SeqCst) == 1 {
            break;
        }
        assert!(
            attempt + 1 < ATTEMPTS,
            "reuse phase: first sampling request never reused the prewarmed connection"
        );
    }

    let unreachable_url = "http://127.0.0.1:0/v1";
    assert_eq!(
        prewarm_transport(unreachable_url).await.outcome,
        PrewarmOutcome::Failed,
        "failed-dial phase: an unreachable origin must report Failed"
    );
    assert_eq!(
        prewarm_transport(unreachable_url).await.outcome,
        PrewarmOutcome::Failed,
        "failed-dial phase: a failed origin must be retried by a later session, not skipped forever"
    );

    let (oversized_url, requests) = spawn_large_body_server().await;
    assert_eq!(
        prewarm_transport(&oversized_url).await.outcome,
        PrewarmOutcome::Truncated,
        "truncated phase: an h1 body over the drain cap leaves the connection unpooled"
    );
    assert_eq!(
        prewarm_transport(&oversized_url).await.outcome,
        PrewarmOutcome::Truncated,
        "truncated phase: the dropped claim must let a later prewarm re-dial, not report AlreadyClaimed"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "truncated phase: both prewarms must re-dial the origin"
    );
}

async fn spawn_large_body_server() -> (String, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let base_url = spawn_http_server(move |_head| {
        counter.fetch_add(1, Ordering::SeqCst);
        let body = vec![b'x'; 128 * 1024];
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body);
        resp
    })
    .await;
    (base_url, requests)
}
