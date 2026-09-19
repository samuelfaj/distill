//! Minimal HTTP/1.1 servers for wire-level tests, sharing one request-framing
//! loop: a connection-counting server for asserting TCP reuse (e.g.
//! shared-client pooling) and a scriptable responder that maps each request's
//! header block to raw response bytes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Reads one full HTTP/1.1 request off `sock` — header block, then
/// `content-length` body bytes — returning the header block.
pub async fn read_http_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let mut chunk = [0u8; 4096];
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                if let Some(read) = chunk.get(..n) {
                    buf.extend_from_slice(read);
                }
            }
        }
    };
    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or(buf)).to_string();
    let body_len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    while buf.len() < head_end + body_len {
        let mut chunk = [0u8; 4096];
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                if let Some(read) = chunk.get(..n) {
                    buf.extend_from_slice(read);
                }
            }
        }
    }
    buf.drain(..head_end + body_len);
    Some(head)
}

/// Minimal keep-alive HTTP/1.1 server: `respond` maps each request's header
/// block to the raw bytes written back.
pub async fn spawn_http_server<H>(respond: H) -> String
where
    H: Fn(&str) -> Vec<u8> + Clone + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let respond = respond.clone();
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                while let Some(head) = read_http_request(&mut sock, &mut buf).await {
                    if sock.write_all(&respond(&head)).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    base_url
}

/// Minimal keep-alive HTTP/1.1 server: counts accepted connections and records each request's header block.
pub async fn spawn_counting_server() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let accepts = Arc::new(AtomicUsize::new(0));
    let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (accepts_l, heads_l) = (Arc::clone(&accepts), Arc::clone(&heads));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            accepts_l.fetch_add(1, Ordering::SeqCst);
            let heads = Arc::clone(&heads_l);
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                while let Some(head) = read_http_request(&mut sock, &mut buf).await {
                    heads.lock().unwrap().push(head);
                    let resp =
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}";
                    if sock.write_all(resp).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (base_url, accepts, heads)
}
