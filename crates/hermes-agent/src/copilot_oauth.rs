//! GitHub Copilot OAuth device authorization flow.
//!
//! GitHub Copilot is authenticated through the OAuth *device authorization*
//! flow (RFC 8628):
//!
//! 1. `POST https://github.com/login/device/code` with `client_id` returns a
//!    `device_code`, a human-readable `user_code` and a `verification_uri`.
//! 2. The user opens the verification URI and enters the user code.
//! 3. The client polls `POST https://github.com/login/oauth/access_token`
//!    with `client_id` + `device_code` + the device grant type until GitHub
//!    returns an `access_token` (handling `authorization_pending`,
//!    `slow_down`, `expired_token` and `access_denied` errors).
//! 4. The access token is persisted to
//!    `<hermes_home>/copilot_token.json` and served to [`CopilotProvider`]
//!    through [`OAuthManager`] with in-memory caching.
//!
//! [`CopilotProvider`]: crate::providers_extra::CopilotProvider

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use hermes_core::{AgentError, LlmProvider, LlmResponse, Message, StreamChunk, ToolSchema};

use crate::oauth::{OAuthManager, OAuthToken, TokenFetcher};
use crate::providers_extra::CopilotProvider;

/// Default GitHub device-code endpoint.
pub const DEFAULT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// Default GitHub OAuth access-token endpoint.
pub const DEFAULT_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// File name (inside [`hermes_config::hermes_home`]) used to persist the token.
pub const TOKEN_FILE_NAME: &str = "copilot_token.json";

/// Grant type constant for the device authorization flow (RFC 8628 §3.4).
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// How long to wait extra between polls after a `slow_down` response.
const DEFAULT_SLOW_DOWN_EXTRA_SECS: u64 = 5;

/// Fallback token TTL when GitHub does not return `expires_in`.
/// (GitHub OAuth app tokens do not expire by default.)
const DEFAULT_TOKEN_TTL_SECS: u64 = 30 * 24 * 60 * 60; // 30 days

/// Response from the device-code endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

impl DeviceCodeResponse {
    /// URI the user should open; prefers the pre-filled complete URI.
    pub fn verification_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// Successful access-token response from the poll endpoint.
#[derive(Debug, Deserialize)]
struct AccessTokenSuccess {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Error response from the poll endpoint.
#[derive(Debug, Deserialize)]
struct AccessTokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// GitHub Copilot OAuth device-flow client.
///
/// The endpoint URLs are configurable so tests can point at a mock server;
/// use [`CopilotDeviceFlow::new`] for the real GitHub endpoints.
#[derive(Debug, Clone)]
pub struct CopilotDeviceFlow {
    client: Client,
    device_code_url: String,
    access_token_url: String,
    slow_down_extra_secs: u64,
}

impl CopilotDeviceFlow {
    /// Create a flow against the real GitHub endpoints.
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            device_code_url: DEFAULT_DEVICE_CODE_URL.to_string(),
            access_token_url: DEFAULT_ACCESS_TOKEN_URL.to_string(),
            slow_down_extra_secs: DEFAULT_SLOW_DOWN_EXTRA_SECS,
        }
    }

    /// Override the endpoint URLs (used by tests with a mock server).
    pub fn with_endpoints(
        mut self,
        device_code_url: impl Into<String>,
        access_token_url: impl Into<String>,
    ) -> Self {
        self.device_code_url = device_code_url.into();
        self.access_token_url = access_token_url.into();
        self
    }

    /// Override the extra backoff applied after a `slow_down` response.
    pub fn with_slow_down_extra_secs(mut self, secs: u64) -> Self {
        self.slow_down_extra_secs = secs;
        self
    }

    /// Step 1: request a device code for `client_id`.
    pub async fn request_device_code(
        &self,
        client_id: &str,
    ) -> Result<DeviceCodeResponse, AgentError> {
        debug!(client_id = %client_id, url = %self.device_code_url, "requesting device code");
        let resp = self
            .client
            .post(&self.device_code_url)
            .header("Accept", "application/json")
            .form(&[("client_id", client_id)])
            .send()
            .await
            .map_err(|e| AgentError::LlmApi(format!("device code request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(AgentError::LlmApi(format!(
                "device code request returned HTTP {status}"
            )));
        }
        resp.json::<DeviceCodeResponse>()
            .await
            .map_err(|e| AgentError::LlmApi(format!("invalid device code response: {e}")))
    }

    /// Step 3: poll the access-token endpoint until the user authorizes
    /// (or denies / the code expires).
    ///
    /// - `interval_secs`: base poll interval (from the device-code response).
    /// - `expires_in_secs`: total budget for polling (from the device-code
    ///   response); polling stops with an error once exceeded.
    pub async fn poll_for_token(
        &self,
        client_id: &str,
        device_code: &str,
        interval_secs: u64,
        expires_in_secs: u64,
    ) -> Result<OAuthToken, AgentError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in_secs);
        let mut interval = Duration::from_secs(interval_secs.max(1));

        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AgentError::AuthFailed(
                    "device code expired before authorization completed".to_string(),
                ));
            }

            debug!(url = %self.access_token_url, "polling for access token");
            let resp = self
                .client
                .post(&self.access_token_url)
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", client_id),
                    ("device_code", device_code),
                    ("grant_type", DEVICE_GRANT_TYPE),
                ])
                .send()
                .await
                .map_err(|e| AgentError::LlmApi(format!("token poll request failed: {e}")))?;

            let body: Value = resp
                .json()
                .await
                .map_err(|e| AgentError::LlmApi(format!("invalid token poll response: {e}")))?;

            if body.get("access_token").is_some() {
                let success: AccessTokenSuccess = serde_json::from_value(body)
                    .map_err(|e| AgentError::LlmApi(format!("invalid token response: {e}")))?;
                info!("device flow authorized; access token obtained");
                return Ok(OAuthToken {
                    access_token: success.access_token,
                    refresh_token: success.refresh_token,
                    expires_in_secs: success.expires_in.unwrap_or(DEFAULT_TOKEN_TTL_SECS),
                });
            }

            let err: AccessTokenError = serde_json::from_value(body).map_err(|e| {
                AgentError::LlmApi(format!("unrecognized token poll response: {e}"))
            })?;
            let desc = err
                .error_description
                .as_deref()
                .unwrap_or("no description")
                .to_string();

            match err.error.as_str() {
                "authorization_pending" => {
                    debug!("authorization pending; retrying");
                    tokio::time::sleep(interval).await;
                }
                "slow_down" => {
                    interval += Duration::from_secs(self.slow_down_extra_secs);
                    warn!(
                        ?interval,
                        "rate limited by token endpoint (slow_down); backing off"
                    );
                    tokio::time::sleep(interval).await;
                }
                "access_denied" => {
                    return Err(AgentError::AuthFailed(format!(
                        "user denied the authorization request: {desc}"
                    )));
                }
                "expired_token" => {
                    return Err(AgentError::AuthFailed(format!(
                        "device code expired: {desc}"
                    )));
                }
                other => {
                    return Err(AgentError::LlmApi(format!(
                        "token endpoint error '{other}': {desc}"
                    )));
                }
            }
        }
    }

    /// Convenience: run the full interactive flow.
    ///
    /// Requests a device code, logs the user instructions via `tracing`
    /// (the caller is responsible for showing them to the user), polls until
    /// authorized, and persists the token to the default path.
    pub async fn run_interactive(&self, client_id: &str) -> Result<OAuthToken, AgentError> {
        let code = self.request_device_code(client_id).await?;
        info!(
            user_code = %code.user_code,
            verification_url = %code.verification_url(),
            expires_in_secs = code.expires_in,
            "visit the verification URL and enter the user code to authorize"
        );
        let token = self
            .poll_for_token(client_id, &code.device_code, code.interval, code.expires_in)
            .await?;
        save_token_to(&default_token_path(), &token)?;
        Ok(token)
    }
}

impl Default for CopilotDeviceFlow {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

/// Default path of the persisted Copilot OAuth token:
/// `<hermes_home>/copilot_token.json`.
pub fn default_token_path() -> PathBuf {
    hermes_config::hermes_home().join(TOKEN_FILE_NAME)
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredCopilotToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in_secs: u64,
    issued_at_epoch: u64,
}

/// Persist an OAuth token to `path` (JSON, `0o600` permissions when possible).
pub fn save_token_to(path: &Path, token: &OAuthToken) -> Result<(), AgentError> {
    let issued_at_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stored = StoredCopilotToken {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_in_secs: token.expires_in_secs,
        issued_at_epoch,
    };
    let data =
        serde_json::to_vec_pretty(&stored).map_err(|e| AgentError::Config(format!("{e}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Config(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, data)
        .map_err(|e| AgentError::Config(format!("cannot write {}: {e}", path.display())))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perm) {
            warn!(path = %path.display(), "could not set 0o600 on token file: {e}");
        }
    }

    debug!(path = %path.display(), "copilot token saved");
    Ok(())
}

/// Load a persisted token. Returns `Ok(None)` when the file is missing or the
/// stored token has expired; the returned token's `expires_in_secs` is adjusted
/// to the remaining lifetime.
pub fn load_token_from(path: &Path) -> Result<Option<OAuthToken>, AgentError> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AgentError::Config(format!(
                "cannot read {}: {e}",
                path.display()
            )))
        }
    };
    let stored: StoredCopilotToken = serde_json::from_slice(&data)
        .map_err(|e| AgentError::Config(format!("invalid token file {}: {e}", path.display())))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(stored.issued_at_epoch);
    let remaining = stored.expires_in_secs.saturating_sub(elapsed);
    if remaining == 0 {
        debug!(path = %path.display(), "stored copilot token expired");
        return Ok(None);
    }

    Ok(Some(OAuthToken {
        access_token: stored.access_token,
        refresh_token: stored.refresh_token,
        expires_in_secs: remaining,
    }))
}

// ---------------------------------------------------------------------------
// OAuthManager integration
// ---------------------------------------------------------------------------

/// Build a [`TokenFetcher`] that loads the persisted Copilot token from the
/// default path.
///
/// The GitHub device flow has no refresh-token mechanism, so the fetcher
/// simply re-reads the token file (after `OAuthManager::clear()` a re-run of
/// the device flow that saved a new token is picked up). It fails with
/// [`AgentError::AuthFailed`] when no valid token is cached, telling the user
/// to run the device flow.
pub fn copilot_token_fetcher(client_id: impl Into<String>) -> TokenFetcher {
    let client_id = client_id.into();
    Arc::new(move |_refresh: Option<String>| {
        let _ = client_id.as_str(); // kept for future per-client token files
        Box::pin(async move {
            match load_token_from(&default_token_path()) {
                Ok(Some(token)) => Ok(token),
                Ok(None) => Err(AgentError::AuthFailed(
                    "no valid Copilot OAuth token cached; run the Copilot device \
                     authorization flow to obtain one"
                        .to_string(),
                )),
                Err(e) => Err(e),
            }
        })
    })
}

/// Build an [`OAuthManager`] for Copilot backed by the persisted token file.
pub fn copilot_oauth_manager(client_id: impl Into<String>) -> OAuthManager {
    OAuthManager::new(copilot_token_fetcher(client_id))
}

// ---------------------------------------------------------------------------
// LlmProvider adapter
// ---------------------------------------------------------------------------

/// [`LlmProvider`] that feeds a device-flow OAuth token into [`CopilotProvider`].
///
/// The access token is fetched from the [`OAuthManager`] on every call, so
/// token rotation/refresh is honored without rebuilding the provider.
#[derive(Clone)]
pub struct CopilotOauthProvider {
    manager: OAuthManager,
    base_url: String,
    model: String,
}

impl CopilotOauthProvider {
    /// Build from an [`OAuthManager`] (see [`copilot_oauth_manager`]).
    pub fn new(
        manager: OAuthManager,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Convenience: build directly from a device-flow client id, using the
    /// default token-file-backed manager.
    pub fn from_client_id(
        client_id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(
            copilot_oauth_manager(client_id),
            base_url.into(),
            model.into(),
        )
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    fn delegate(&self, token: String, model: Option<&str>) -> CopilotProvider {
        CopilotProvider::new(self.base_url.clone(), token)
            .with_model(model.unwrap_or(&self.model).to_string())
    }
}

#[async_trait]
impl LlmProvider for CopilotOauthProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let token = self.manager.access_token().await?;
        self.delegate(token, model)
            .chat_completion(messages, tools, max_tokens, temperature, model, extra_body)
            .await
    }

    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let this = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(|s| s.to_string());
        let extra_body = extra_body.cloned();

        async_stream::stream! {
            let token = match this.manager.access_token().await {
                Ok(t) => t,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let provider = this.delegate(token, model.as_deref());
            let mut stream = provider.chat_completion_stream(
                &messages,
                &tools,
                max_tokens,
                temperature,
                model.as_deref(),
                extra_body.as_ref(),
            );
            while let Some(chunk) = stream.next().await {
                yield chunk;
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Tests (mocked HTTP, no real credentials)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal scripted HTTP mock server: each connection consumes one queued
    /// response. Records the request line + body for assertions.
    struct MockServer {
        addr: std::net::SocketAddr,
        requests: Arc<StdMutex<Vec<String>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let responses = Arc::new(StdMutex::new(VecDeque::from(responses)));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let r2 = Arc::clone(&responses);
            let q2 = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    let (mut socket, _) = match listener.accept().await {
                        Ok(a) => a,
                        Err(_) => break,
                    };
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    q2.lock().unwrap().push(raw);
                    let body = r2.lock().unwrap().pop_front().unwrap_or_default();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                }
            });
            Self { addr, requests }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    const DEVICE_CODE_JSON: &str = r#"{
        "device_code": "dc-123",
        "user_code": "ABCD-EFGH",
        "verification_uri": "https://github.com/login/device",
        "verification_uri_complete": "https://github.com/login/device?user_code=ABCD-EFGH",
        "expires_in": 900,
        "interval": 5
    }"#;

    const TOKEN_SUCCESS_JSON: &str = r#"{
        "access_token": "ghu_testtoken",
        "token_type": "bearer",
        "scope": "copilot"
    }"#;

    fn pending_json() -> String {
        r#"{"error":"authorization_pending","error_description":"waiting"}"#.to_string()
    }

    fn slow_down_json() -> String {
        r#"{"error":"slow_down","error_description":"slow down"}"#.to_string()
    }

    fn flow_for(server: &MockServer) -> CopilotDeviceFlow {
        CopilotDeviceFlow::new().with_endpoints(
            server.url("/device/code"),
            server.url("/oauth/access_token"),
        )
    }

    #[tokio::test]
    async fn device_code_request_parses_response() {
        let server = MockServer::start(vec![DEVICE_CODE_JSON.to_string()]).await;
        let flow = flow_for(&server);
        let code = flow.request_device_code("test-client-id").await.unwrap();
        assert_eq!(code.device_code, "dc-123");
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert_eq!(code.interval, 5);
        assert_eq!(code.expires_in, 900);
        assert_eq!(
            code.verification_url(),
            "https://github.com/login/device?user_code=ABCD-EFGH"
        );
        let req = server.requests.lock().unwrap();
        assert!(req[0].contains("POST /device/code"));
        assert!(req[0].contains("client_id=test-client-id"));
    }

    #[tokio::test]
    async fn poll_pending_then_success() {
        let server = MockServer::start(vec![
            pending_json(),
            pending_json(),
            TOKEN_SUCCESS_JSON.to_string(),
        ])
        .await;
        let flow = flow_for(&server).with_slow_down_extra_secs(0);
        let token = flow.poll_for_token("cid", "dc-123", 0, 60).await.unwrap();
        assert_eq!(token.access_token, "ghu_testtoken");
        assert_eq!(server.request_count(), 3);
        let req = server.requests.lock().unwrap();
        assert!(req[0].contains("POST /oauth/access_token"));
        assert!(req[0].contains("device_code=dc-123"));
        assert!(
            req[0].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
                || req[0].contains("grant_type=urn:ietf:params:oauth:grant-type:device_code")
        );
    }

    #[tokio::test]
    async fn poll_slow_down_backoff_then_success() {
        let server =
            MockServer::start(vec![slow_down_json(), TOKEN_SUCCESS_JSON.to_string()]).await;
        let flow = flow_for(&server).with_slow_down_extra_secs(1);
        let start = tokio::time::Instant::now();
        let token = flow.poll_for_token("cid", "dc-123", 0, 60).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(token.access_token, "ghu_testtoken");
        // interval was 0 -> max(1) base + 1s slow_down extra => at least ~2s of sleep
        assert!(
            elapsed >= Duration::from_secs(1),
            "expected backoff sleep, got {elapsed:?}"
        );
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn poll_expired_token_errors() {
        let server = MockServer::start(vec![
            r#"{"error":"expired_token","error_description":"expired"}"#.to_string(),
        ])
        .await;
        let flow = flow_for(&server);
        let err = flow
            .poll_for_token("cid", "dc-123", 0, 60)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::AuthFailed(_)));
        assert!(err.to_string().contains("expired"));
    }

    #[tokio::test]
    async fn poll_access_denied_errors() {
        let server = MockServer::start(vec![
            r#"{"error":"access_denied","error_description":"denied by user"}"#.to_string(),
        ])
        .await;
        let flow = flow_for(&server);
        let err = flow
            .poll_for_token("cid", "dc-123", 0, 60)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn poll_unknown_error_is_llm_api_error() {
        let server = MockServer::start(vec![
            r#"{"error":"incorrect_client_credentials"}"#.to_string()
        ])
        .await;
        let flow = flow_for(&server);
        let err = flow
            .poll_for_token("cid", "dc-123", 0, 60)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::LlmApi(_)));
    }

    #[tokio::test]
    async fn poll_times_out_when_never_authorized() {
        // expires_in_secs = 0 => deadline already passed => immediate error,
        // so use 1 and an interval that keeps polling past the deadline.
        let server = MockServer::start(vec![pending_json(); 50]).await;
        let flow = flow_for(&server);
        let start = tokio::time::Instant::now();
        let err = flow
            .poll_for_token("cid", "dc-123", 1, 2)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::AuthFailed(_)));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn token_persist_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("copilot_oauth_test_{}", std::process::id()));
        let path = dir.join("copilot_token.json");
        let token = OAuthToken {
            access_token: "ghu_roundtrip".to_string(),
            refresh_token: None,
            expires_in_secs: 3600,
        };
        save_token_to(&path, &token).unwrap();
        let loaded = load_token_from(&path).unwrap().unwrap();
        assert_eq!(loaded.access_token, "ghu_roundtrip");
        assert!(loaded.expires_in_secs <= 3600 && loaded.expires_in_secs > 3500);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_token_returns_none() {
        let path =
            std::env::temp_dir().join(format!("copilot_oauth_missing_{}.json", std::process::id()));
        assert!(load_token_from(&path).unwrap().is_none());
    }

    #[test]
    fn load_expired_token_returns_none() {
        let dir = std::env::temp_dir().join(format!("copilot_oauth_exp_{}", std::process::id()));
        let path = dir.join("copilot_token.json");
        let stored = StoredCopilotToken {
            access_token: "ghu_old".to_string(),
            refresh_token: None,
            expires_in_secs: 10,
            issued_at_epoch: 1, // long ago
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(load_token_from(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn oauth_manager_serves_token_from_fetcher() {
        let token = OAuthToken {
            access_token: "ghu_managed".to_string(),
            refresh_token: None,
            expires_in_secs: 3600,
        };
        let fetcher: TokenFetcher = Arc::new(move |_refresh: Option<String>| {
            let token = token.clone();
            Box::pin(async move { Ok(token) })
        });
        let manager = OAuthManager::new(fetcher);
        assert_eq!(manager.access_token().await.unwrap(), "ghu_managed");
        assert_eq!(
            manager.authorization_header().await.unwrap(),
            "Bearer ghu_managed"
        );
    }

    #[tokio::test]
    async fn oauth_provider_delegates_with_manager_token() {
        // A mock chat-completions endpoint recording the Authorization header.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_auth = Arc::new(StdMutex::new(String::new()));
        let seen_auth2 = Arc::clone(&seen_auth);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            let auth = raw
                .lines()
                .find(|l| l.to_lowercase().starts_with("authorization:"))
                .unwrap_or("")
                .to_string();
            *seen_auth2.lock().unwrap() = auth;
            let body = r#"{"id":"x","object":"chat.completion","created":1,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        });

        let token = OAuthToken {
            access_token: "ghu_oauth_delegate".to_string(),
            refresh_token: None,
            expires_in_secs: 3600,
        };
        let fetcher: TokenFetcher = Arc::new(move |_r: Option<String>| {
            let token = token.clone();
            Box::pin(async move { Ok(token) })
        });
        let provider = CopilotOauthProvider::new(
            OAuthManager::new(fetcher),
            format!("http://{addr}"),
            "gpt-4o",
        );
        let resp = provider
            .chat_completion(&[], &[], None, None, None, None)
            .await
            .unwrap();
        assert_eq!(resp.message.content.as_deref(), Some("hi"));
        assert!(
            seen_auth
                .lock()
                .unwrap()
                .contains("Bearer ghu_oauth_delegate"),
            "manager token was not forwarded as Bearer"
        );
    }
}
