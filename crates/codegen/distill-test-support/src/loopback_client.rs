//! The one HTTP client the loopback doubles' own tests send requests with.

use reqwest::{Method, Response};

#[allow(clippy::disallowed_methods, reason = "loopback only client")]
pub(crate) async fn send(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Response {
    let mut request = reqwest::Client::new().request(method, url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(body).send().await.unwrap()
}
