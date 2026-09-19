// Modified for Distill by Samuel Fajreldines, 2026.
//! OpenRouter browser authorization. Its key is stored separately from Grok and ChatGPT.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use base64::Engine as _;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, oneshot};

pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

pub fn auth_file_path() -> PathBuf {
    crate::util::distill_home::distill_home().join("openrouter-auth.json")
}

#[derive(Deserialize, Serialize)]
struct Credentials {
    key: String,
}

/// Explicit environment credentials take precedence over the browser sign-in.
pub fn api_key() -> io::Result<Option<String>> {
    if let Ok(key) = std::env::var(API_KEY_ENV)
        && !key.trim().is_empty()
    {
        return Ok(Some(key));
    }
    load_key_at(&auth_file_path())
}

pub fn is_logged_in() -> bool {
    load_key_at(&auth_file_path()).ok().flatten().is_some()
}

/// Disconnect the browser account; explicit environment keys remain configured.
pub fn logout() -> io::Result<()> {
    match std::fs::remove_file(auth_file_path()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn load_key_at(path: &Path) -> io::Result<Option<String>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    crate::util::secure_file::ensure_owner_only_permissions(path)?;
    let credentials: Credentials = serde_json::from_slice(&bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid OpenRouter credential file",
        )
    })?;
    Ok((!credentials.key.trim().is_empty()).then_some(credentials.key))
}

fn save_key_at(path: &Path, credentials: &Credentials) -> io::Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing credential directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    crate::util::secure_file::ensure_owner_only_permissions(temp.path())?;
    serde_json::to_writer(temp.as_file_mut(), credentials).map_err(io::Error::other)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn is_openrouter_url(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some("openrouter.ai"))
}

#[derive(Default, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

type CallbackSender = Arc<Mutex<Option<oneshot::Sender<Result<String, &'static str>>>>>;

async fn callback(
    State(sender): State<CallbackSender>,
    Query(query): Query<CallbackQuery>,
) -> Html<&'static str> {
    let result = if query.error.is_some() {
        Err("OpenRouter authorization was declined. Please try again.")
    } else if let Some(code) = query.code.filter(|code| !code.trim().is_empty()) {
        Ok(code)
    } else {
        return Html("Missing authorization code. Return to OpenRouter to finish signing in.");
    };
    if let Some(sender) = sender.lock().await.take() {
        let _ = sender.send(result);
    }
    Html("Authorization received. Return to Distill to see the result.")
}

fn authorization_url(callback_url: &str, verifier: &str) -> Result<String> {
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse("https://openrouter.ai/auth")?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("key_label", "Distill");
    Ok(url.into())
}

async fn exchange_code(endpoint: &str, code: &str, verifier: &str, path: &Path) -> Result<()> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .post(endpoint)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .context("could not exchange OpenRouter authorization code")?;
    if !response.status().is_success() {
        bail!(
            "OpenRouter authorization failed (HTTP {}). Please try again.",
            response.status()
        );
    }
    let credentials: Credentials = response
        .json()
        .await
        .context("invalid OpenRouter authorization response")?;
    if credentials.key.trim().is_empty() {
        bail!("OpenRouter returned an empty API key");
    }
    save_key_at(path, &credentials).context("could not save OpenRouter credentials")
}

/// A random callback path binds the response to this attempt; PKCE binds the code exchange.
/// The server future is owned by this login, so completion/cancellation closes the listener.
pub async fn run_tui_login(browser_fallback: oneshot::Sender<String>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut random = [0u8; 32];
    rand::rng().fill_bytes(&mut random);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    rand::rng().fill_bytes(&mut random);
    let callback_path = format!(
        "/auth/callback/{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    );
    let callback_url = format!(
        "http://localhost:{}{callback_path}",
        listener.local_addr()?.port()
    );
    let url = authorization_url(&callback_url, &verifier)?;
    let (sender, receiver) = oneshot::channel();
    let app = Router::new()
        .route(&callback_path, get(callback))
        .with_state(Arc::new(Mutex::new(Some(sender))));
    let open_url = url.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || webbrowser::open(&open_url)).await,
        Ok(Ok(()))
    ) {
        let _ = browser_fallback.send(url);
    }
    let code = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(300), receiver) => {
            result.context("OpenRouter login timed out; please try again")?
                .context("OpenRouter callback stopped")?.map_err(anyhow::Error::msg)?
        }
        result = axum::serve(listener, app) => {
            result?;
            bail!("OpenRouter callback server stopped");
        }
    };
    exchange_code(
        "https://openrouter.ai/api/v1/auth/keys",
        &code,
        &verifier,
        &auth_file_path(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pkce_exchange_saves_a_private_key_without_touching_other_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openrouter-auth.json");
        std::fs::write(dir.path().join("auth.json"), "grok-account").unwrap();
        std::fs::write(dir.path().join("codex-auth.json"), "chatgpt-account").unwrap();
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let url = url::Url::parse(
            &authorization_url("http://localhost:1234/callback/nonce", verifier).unwrap(),
        )
        .unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(
            params["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            params["callback_url"],
            "http://localhost:1234/callback/nonce"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/auth/keys", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/auth/keys",
            axum::routing::post(
                move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    assert_eq!(body["code"], "test-code");
                    assert_eq!(body["code_verifier"], verifier);
                    assert_eq!(body["code_challenge_method"], "S256");
                    axum::Json(serde_json::json!({"key": "test-openrouter-key"}))
                },
            ),
        );
        let app = app.route(
            "/reject",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    "private-response-must-not-be-logged",
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        exchange_code(&endpoint, "test-code", verifier, &path)
            .await
            .unwrap();
        let error = exchange_code(
            &endpoint.replace("/auth/keys", "/reject"),
            "expired-code",
            verifier,
            &path,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("403"));
        assert!(!error.to_string().contains("private-response"));
        server.abort();
        assert_eq!(
            load_key_at(&path).unwrap().as_deref(),
            Some("test-openrouter-key")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            "grok-account"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("codex-auth.json")).unwrap(),
            "chatgpt-account"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn invalid_callback_does_not_consume_the_login() {
        let (sender, mut receiver) = oneshot::channel();
        let state = Arc::new(Mutex::new(Some(sender)));
        let _ = callback(State(state.clone()), Query(CallbackQuery::default())).await;
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let _ = callback(
            State(state),
            Query(CallbackQuery {
                code: Some("valid-code".into()),
                error: None,
            }),
        )
        .await;
        assert_eq!(receiver.await.unwrap().unwrap(), "valid-code");
    }
}
