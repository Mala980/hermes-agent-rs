//! Qwen OAuth2 token flow for Alibaba DashScope / Alibaba Cloud.
//!
//! ## Honesty notice — read before using in production
//!
//! DashScope's public documentation authenticates the OpenAI-compatible
//! endpoint (`https://dashscope.aliyuncs.com/compatible-mode/v1`) with
//! **API keys** (`Authorization: Bearer sk-...`). Alibaba Cloud does publish
//! OAuth2/OIDC endpoints for IDaaS (single sign-on), but **no documented
//! OAuth2 flow that mints tokens usable against the DashScope model API**
//! could be verified at the time of writing.
//!
//! This module is therefore a **standards-compliant, endpoint-configurable**
//! RFC 6749 OAuth2 client — client-credentials, authorization-code (+ PKCE
//! S256), and refresh-token grants — wired into [`OAuthManager`] for
//! caching/auto-refresh and into the existing [`QwenProvider`] for chat
//! completions. The token endpoint is **not** defaulted: set
//! `QWEN_OAUTH_TOKEN_URL` explicitly. Until Alibaba documents a real Qwen
//! OAuth token endpoint, treat the whole `qwen-oauth` flow as best-effort.
//!
//! Token persistence: `<hermes_home>/oauth/qwen_oauth.json` (mode `0600` on
//! unix). Secrets (client secret) are **never** persisted; they are read from
//! `QWEN_OAUTH_CLIENT_SECRET` on each process start.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use hermes_core::{AgentError, LlmProvider, LlmResponse, Message, StreamChunk, ToolSchema};

use crate::oauth::{OAuthManager, OAuthToken, TokenFetcher};
use crate::providers_extra::QwenProvider;

/// Fallback token lifetime when the token endpoint omits `expires_in`.
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;

/// Placeholder bearer value used to build the inner [`QwenProvider`].
/// It is always replaced with a real OAuth token before any request.
const PLACEHOLDER_API_KEY: &str = "qwen-oauth-placeholder";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// OAuth2 configuration for the Qwen flow.
///
/// All values are endpoint-configurable because Alibaba publishes no
/// verified default token endpoint for DashScope model API access.
#[derive(Debug, Clone)]
pub struct QwenOAuthConfig {
    /// RFC 6749 token endpoint, e.g. `https://<idp>/oauth2/token`.
    pub token_url: String,
    /// Authorization endpoint for the authorization-code flow (optional).
    pub authorize_url: Option<String>,
    /// OAuth2 client id.
    pub client_id: String,
    /// OAuth2 client secret. Required for client-credentials and refresh
    /// grants. Read transiently from the environment; never persisted.
    pub client_secret: Option<String>,
    /// Optional space-delimited scope.
    pub scope: Option<String>,
    /// Redirect URI registered for the authorization-code flow (optional).
    pub redirect_uri: Option<String>,
}

impl QwenOAuthConfig {
    /// Build from environment:
    /// - `QWEN_OAUTH_TOKEN_URL` (required — no verified default exists)
    /// - `QWEN_OAUTH_CLIENT_ID` (required)
    /// - `QWEN_OAUTH_CLIENT_SECRET` (optional, needed for refresh / client-credentials)
    /// - `QWEN_OAUTH_AUTHORIZE_URL`, `QWEN_OAUTH_SCOPE`, `QWEN_OAUTH_REDIRECT_URI` (optional)
    pub fn from_env() -> Result<Self, AgentError> {
        fn env_first(names: &[&str]) -> Option<String> {
            names
                .iter()
                .filter_map(|n| std::env::var(n).ok())
                .map(|s| s.trim().to_string())
                .find(|s| !s.is_empty())
        }
        let client_id = env_first(&["QWEN_OAUTH_CLIENT_ID", "HERMES_QWEN_OAUTH_CLIENT_ID"])
            .ok_or_else(|| {
                AgentError::Config(
                    "QWEN_OAUTH_CLIENT_ID is not set; the qwen-oauth flow needs a client id"
                        .to_string(),
                )
            })?;
        // Deliberately no default: Alibaba documents no OAuth2 token endpoint
        // for DashScope model API access.
        let token_url = env_first(&["QWEN_OAUTH_TOKEN_URL", "HERMES_QWEN_OAUTH_TOKEN_URL"])
            .ok_or_else(|| {
                AgentError::Config(
                    "QWEN_OAUTH_TOKEN_URL is not set. Alibaba publishes no documented OAuth2 token \
                 endpoint for DashScope model API access; point this at your identity provider's \
                 RFC 6749 token endpoint to use the qwen-oauth flow"
                        .to_string(),
                )
            })?;
        Ok(Self {
            token_url,
            authorize_url: env_first(&[
                "QWEN_OAUTH_AUTHORIZE_URL",
                "HERMES_QWEN_OAUTH_AUTHORIZE_URL",
            ]),
            client_id,
            client_secret: env_first(&[
                "QWEN_OAUTH_CLIENT_SECRET",
                "HERMES_QWEN_OAUTH_CLIENT_SECRET",
            ]),
            scope: env_first(&["QWEN_OAUTH_SCOPE", "HERMES_QWEN_OAUTH_SCOPE"]),
            redirect_uri: env_first(&["QWEN_OAUTH_REDIRECT_URI", "HERMES_QWEN_OAUTH_REDIRECT_URI"]),
        })
    }
}

// ---------------------------------------------------------------------------
// Token endpoint interaction (RFC 6749)
// ---------------------------------------------------------------------------

/// Token endpoint response. `expires_in` accepts both numbers and numeric
/// strings since providers disagree on the encoding.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    #[allow(dead_code)]
    token_type: Option<String>,
    #[serde(default, deserialize_with = "de_expires_in")]
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

fn de_expires_in<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(u64),
        Str(String),
    }

    let opt = Option::<NumOrStr>::deserialize(deserializer)?;
    Ok(match opt {
        None => None,
        Some(NumOrStr::Num(n)) => Some(n),
        Some(NumOrStr::Str(s)) => s.parse::<u64>().ok(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

/// POST an `application/x-www-form-urlencoded` token request and parse the
/// RFC 6749 token response.
async fn post_token_request(
    config: &QwenOAuthConfig,
    params: Vec<(&str, String)>,
) -> Result<OAuthToken, AgentError> {
    let client = reqwest::Client::new();
    let mut request = client.post(&config.token_url).form(&params);
    // RFC 6749 §2.3.1: prefer HTTP Basic for client authentication; the
    // client_id form field is still included for servers that expect it.
    if let Some(secret) = config.client_secret.as_deref() {
        request = request.basic_auth(&config.client_id, Some(secret));
    }
    let response = request
        .send()
        .await
        .map_err(|e| AgentError::Io(format!("Qwen OAuth token request failed: {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| AgentError::Io(format!("Qwen OAuth: failed to read token response: {e}")))?;
    if !status.is_success() {
        return Err(AgentError::AuthFailed(format!(
            "Qwen OAuth token endpoint returned {status}: {}",
            truncate(&body, 300)
        )));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| AgentError::AuthFailed(format!("Qwen OAuth: invalid token JSON: {e}")))?;
    let access_token = parsed
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            AgentError::AuthFailed("Qwen OAuth: token response missing access_token".to_string())
        })?;
    Ok(OAuthToken {
        access_token,
        refresh_token: parsed.refresh_token.filter(|t| !t.is_empty()),
        expires_in_secs: parsed.expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECS),
    })
}

/// Client-credentials grant (RFC 6749 §4.4). Machine-to-machine flow.
pub async fn fetch_client_credentials_token(
    config: &QwenOAuthConfig,
) -> Result<OAuthToken, AgentError> {
    if config.client_secret.is_none() {
        return Err(AgentError::Config(
            "client-credentials grant requires QWEN_OAUTH_CLIENT_SECRET".to_string(),
        ));
    }
    let mut params = vec![
        ("grant_type", "client_credentials".to_string()),
        ("client_id", config.client_id.clone()),
    ];
    if let Some(scope) = &config.scope {
        params.push(("scope", scope.clone()));
    }
    tracing::debug!("qwen-oauth: requesting token via client-credentials grant");
    post_token_request(config, params).await
}

/// Authorization-code exchange (RFC 6749 §4.1.3). Call after the user
/// completes the browser flow started by [`authorization_url`].
pub async fn exchange_authorization_code(
    config: &QwenOAuthConfig,
    code: &str,
    code_verifier: Option<&str>,
) -> Result<OAuthToken, AgentError> {
    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("client_id", config.client_id.clone()),
    ];
    if let Some(uri) = &config.redirect_uri {
        params.push(("redirect_uri", uri.clone()));
    }
    if let Some(verifier) = code_verifier {
        params.push(("code_verifier", verifier.to_string()));
    }
    tracing::debug!("qwen-oauth: exchanging authorization code for tokens");
    post_token_request(config, params).await
}

/// Refresh-token grant (RFC 6749 §6).
pub async fn refresh_access_token(
    config: &QwenOAuthConfig,
    refresh_token: &str,
) -> Result<OAuthToken, AgentError> {
    let mut params = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", config.client_id.clone()),
    ];
    if let Some(scope) = &config.scope {
        params.push(("scope", scope.clone()));
    }
    tracing::debug!("qwen-oauth: refreshing access token");
    post_token_request(config, params).await
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636)
// ---------------------------------------------------------------------------

/// Generate a `code_verifier`: 32 random bytes, base64url-encoded (43 chars).
pub fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    bytes[..16].copy_from_slice(a.as_bytes());
    bytes[16..].copy_from_slice(b.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// S256 code challenge for a verifier.
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Build the authorization URL that starts the authorization-code flow.
///
/// Returns `(url, code_verifier)`. Direct the user to `url`; keep
/// `code_verifier` and pass it to [`exchange_authorization_code`] with the
/// returned `code`.
pub fn authorization_url(
    config: &QwenOAuthConfig,
    state: &str,
) -> Result<(String, String), AgentError> {
    let authorize_url = config.authorize_url.as_deref().ok_or_else(|| {
        AgentError::Config(
            "QWEN_OAUTH_AUTHORIZE_URL is not set; cannot start the authorization-code flow"
                .to_string(),
        )
    })?;
    let verifier = generate_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let mut url = reqwest::Url::parse(authorize_url)
        .map_err(|e| AgentError::Config(format!("invalid QWEN_OAUTH_AUTHORIZE_URL: {e}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("response_type", "code");
        pairs.append_pair("client_id", &config.client_id);
        pairs.append_pair(
            "redirect_uri",
            config
                .redirect_uri
                .as_deref()
                .unwrap_or("urn:ietf:wg:oauth:2.0:oob"),
        );
        pairs.append_pair("code_challenge", &challenge);
        pairs.append_pair("code_challenge_method", "S256");
        pairs.append_pair("state", state);
        if let Some(scope) = &config.scope {
            pairs.append_pair("scope", scope);
        }
    }
    Ok((url.to_string(), verifier))
}

// ---------------------------------------------------------------------------
// Persistence (<hermes_home>/oauth/qwen_oauth.json)
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// On-disk token record. The client secret is deliberately *not* stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedQwenOAuthToken {
    access_token: String,
    refresh_token: Option<String>,
    /// Unix timestamp (seconds) at which the access token expires.
    expires_at_unix: u64,
    token_url: String,
    client_id: String,
}

impl PersistedQwenOAuthToken {
    fn remaining_secs(&self) -> u64 {
        self.expires_at_unix.saturating_sub(unix_now())
    }

    fn to_oauth_token(&self) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            // Keep at least 1s so OAuthManager treats it as usable.
            expires_in_secs: self.remaining_secs().max(1),
        }
    }
}

/// Default token path: `<hermes_home>/oauth/qwen_oauth.json`.
/// Uses [`hermes_config::hermes_home`], never a hardcoded `~/.hermes`.
pub fn qwen_oauth_token_path() -> PathBuf {
    hermes_config::hermes_home()
        .join("oauth")
        .join("qwen_oauth.json")
}

/// Persist a freshly minted token. File permissions are restricted to the
/// owner (`0600`) on unix.
pub fn save_qwen_oauth_token(
    token: &OAuthToken,
    config: &QwenOAuthConfig,
) -> Result<PathBuf, AgentError> {
    let path = qwen_oauth_token_path();
    save_qwen_oauth_token_to(&path, token, config)?;
    Ok(path)
}

fn save_qwen_oauth_token_to(
    path: &Path,
    token: &OAuthToken,
    config: &QwenOAuthConfig,
) -> Result<(), AgentError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("Qwen OAuth: cannot create token dir: {e}")))?;
    }
    let persisted = PersistedQwenOAuthToken {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at_unix: unix_now().saturating_add(token.expires_in_secs),
        token_url: config.token_url.clone(),
        client_id: config.client_id.clone(),
    };
    let json = serde_json::to_string_pretty(&persisted)
        .map_err(|e| AgentError::Io(format!("Qwen OAuth: cannot serialize token: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| AgentError::Io(format!("Qwen OAuth: cannot write token file: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| AgentError::Io(format!("Qwen OAuth: cannot chmod token file: {e}")))?;
    }
    tracing::debug!("qwen-oauth: token saved to {}", path.display());
    Ok(())
}

fn load_persisted_token_from(path: &Path) -> Option<PersistedQwenOAuthToken> {
    let data = std::fs::read(path).ok()?;
    let persisted: PersistedQwenOAuthToken = serde_json::from_slice(&data).ok()?;
    if persisted.access_token.is_empty() {
        return None;
    }
    Some(persisted)
}

/// Load the persisted token if it is still valid (with 60s clock skew).
/// Returns `None` when the file is missing, unreadable, or expired.
pub fn load_qwen_oauth_token() -> Option<OAuthToken> {
    load_qwen_oauth_token_from(&qwen_oauth_token_path())
}

fn load_qwen_oauth_token_from(path: &Path) -> Option<OAuthToken> {
    let persisted = load_persisted_token_from(path)?;
    // Treat near-expiry tokens as expired so the caller refreshes instead.
    if persisted.remaining_secs() <= 60 {
        tracing::debug!("qwen-oauth: persisted token expired or near expiry");
        return None;
    }
    Some(persisted.to_oauth_token())
}

/// Remove the persisted token file (logout).
pub fn clear_qwen_oauth_token() -> Result<(), AgentError> {
    let path = qwen_oauth_token_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            tracing::debug!("qwen-oauth: token file removed");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AgentError::Io(format!(
            "Qwen OAuth: cannot remove token file: {e}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// OAuthManager integration
// ---------------------------------------------------------------------------

/// Build an [`OAuthManager`] whose fetcher implements the full token
/// lifecycle: reuse a valid persisted token → refresh (in-memory, then
/// persisted) → client-credentials grant. Fresh tokens are persisted.
pub fn qwen_oauth_manager(config: QwenOAuthConfig) -> OAuthManager {
    let fetcher: TokenFetcher = Arc::new(move |refresh: Option<String>| {
        let config = config.clone();
        Box::pin(async move {
            // Cold start: a still-valid token from a previous run wins.
            if refresh.is_none() {
                let path = qwen_oauth_token_path();
                if let Some(persisted) = load_persisted_token_from(&path) {
                    if persisted.remaining_secs() > 60 {
                        tracing::debug!("qwen-oauth: reusing persisted access token");
                        return Ok(persisted.to_oauth_token());
                    }
                    if let Some(rt) = persisted.refresh_token.clone() {
                        tracing::debug!("qwen-oauth: refreshing expired persisted token");
                        match refresh_access_token(&config, &rt).await {
                            Ok(token) => {
                                if let Err(e) = save_qwen_oauth_token(&token, &config) {
                                    tracing::warn!("qwen-oauth: failed to persist token: {e}");
                                }
                                return Ok(token);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "qwen-oauth: persisted refresh failed ({e}); trying client credentials"
                                );
                            }
                        }
                    }
                }
            } else if let Some(rt) = refresh {
                tracing::debug!("qwen-oauth: refreshing in-memory token");
                let token = refresh_access_token(&config, &rt).await?;
                if let Err(e) = save_qwen_oauth_token(&token, &config) {
                    tracing::warn!("qwen-oauth: failed to persist token: {e}");
                }
                return Ok(token);
            }

            tracing::debug!("qwen-oauth: acquiring token via client-credentials grant");
            let token = fetch_client_credentials_token(&config).await?;
            if let Err(e) = save_qwen_oauth_token(&token, &config) {
                tracing::warn!("qwen-oauth: failed to persist token: {e}");
            }
            Ok(token)
        })
    });
    OAuthManager::new(fetcher)
}

// ---------------------------------------------------------------------------
// QwenOAuthProvider — LlmProvider backed by OAuth tokens
// ---------------------------------------------------------------------------

/// [`QwenProvider`] variant that mints its bearer credential through the
/// Qwen OAuth2 flow instead of a static API key.
///
/// Each request resolves a fresh token from the [`OAuthManager`] (cached and
/// auto-refreshed) and injects it as the `Authorization: Bearer` header of
/// the DashScope OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct QwenOAuthProvider {
    manager: OAuthManager,
    inner: Arc<tokio::sync::Mutex<QwenProvider>>,
    model: String,
    base_url: String,
}

impl std::fmt::Debug for QwenOAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QwenOAuthProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl QwenOAuthProvider {
    /// DashScope OpenAI-compatible base URL.
    pub const DEFAULT_BASE_URL: &'static str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
    /// Default model.
    pub const DEFAULT_MODEL: &'static str = "qwen-turbo";

    /// Build from an existing [`OAuthManager`] (useful for tests and for
    /// sharing one manager across providers).
    pub fn new(manager: OAuthManager, model: impl Into<String>) -> Self {
        let model_str = model.into();
        let inner = QwenProvider::new(PLACEHOLDER_API_KEY)
            .with_model(&model_str)
            .with_base_url(Self::DEFAULT_BASE_URL);
        Self {
            manager,
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
            model: model_str,
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Build from environment (`QWEN_OAUTH_*`; see [`QwenOAuthConfig::from_env`]).
    pub fn from_env(model: impl Into<String>) -> Result<Self, AgentError> {
        let config = QwenOAuthConfig::from_env()?;
        Ok(Self::new(qwen_oauth_manager(config), model))
    }

    fn rebuild_inner(&mut self) {
        let inner = QwenProvider::new(PLACEHOLDER_API_KEY)
            .with_model(&self.model)
            .with_base_url(&self.base_url);
        self.inner = Arc::new(tokio::sync::Mutex::new(inner));
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self.rebuild_inner();
        self
    }

    /// Override the DashScope-compatible base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self.rebuild_inner();
        self
    }

    /// Current access token (resolves via the manager, refreshing if needed).
    pub async fn current_access_token(&self) -> Result<String, AgentError> {
        self.manager.access_token().await
    }
}

#[async_trait]
impl LlmProvider for QwenOAuthProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra_body: Option<&serde_json::Value>,
    ) -> Result<LlmResponse, AgentError> {
        let token = self.manager.access_token().await?;
        let mut inner = self.inner.lock().await;
        inner.set_api_key(token);
        inner
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
        extra_body: Option<&serde_json::Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let manager = self.manager.clone();
        let inner = self.inner.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(str::to_string);
        let extra_body = extra_body.cloned();
        async_stream::stream! {
            let token = match manager.access_token().await {
                Ok(t) => t,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            // Clone the configured provider with the fresh token; the clone
            // shares the underlying reqwest client, so this is cheap.
            let provider = {
                let mut guard = inner.lock().await;
                guard.set_api_key(&token);
                guard.clone()
            };
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
// Tests (all HTTP is mocked; no real credentials)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    /// Serialize env-mutating tests: env vars are process-global.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn test_config(token_url: &str) -> QwenOAuthConfig {
        QwenOAuthConfig {
            token_url: token_url.to_string(),
            authorize_url: None,
            client_id: "test-client".to_string(),
            client_secret: Some("test-secret".to_string()),
            scope: None,
            redirect_uri: None,
        }
    }

    /// Spawn a one-shot mock HTTP server.
    ///
    /// The handler receives the full raw request (headers + body) and returns
    /// `(status, body)`. The raw request is recorded into `seen` for assertions.
    async fn spawn_mock(
        handler: impl Fn(String) -> (u16, String) + Send + Sync + 'static,
        seen: Arc<StdMutex<Option<String>>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read until end of headers.
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(buf.len());
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_len = headers
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            while buf.len() < header_end + content_len {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let raw = String::from_utf8_lossy(&buf).to_string();
            *seen.lock().unwrap() = Some(raw.clone());
            let (status, body) = handler(raw);
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn token_json(access: &str) -> String {
        format!(
            r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":3600,"refresh_token":"refresh-123"}}"#
        )
    }

    #[tokio::test]
    async fn client_credentials_grant_posts_form_and_parses_token() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(|_| (200, token_json("cc-token-abc")), seen.clone()).await;
        let config = test_config(&format!("{url}/oauth/token"));

        let token = fetch_client_credentials_token(&config).await.unwrap();

        assert_eq!(token.access_token, "cc-token-abc");
        assert_eq!(token.refresh_token.as_deref(), Some("refresh-123"));
        assert_eq!(token.expires_in_secs, 3600);

        let raw = seen.lock().unwrap().clone().unwrap();
        assert!(raw.starts_with("POST /oauth/token "), "raw: {raw}");
        assert!(raw.contains("grant_type=client_credentials"), "raw: {raw}");
        assert!(raw.contains("client_id=test-client"), "raw: {raw}");
        // RFC 6749 §2.3.1: client secret goes in HTTP Basic, not the body.
        assert!(
            raw.to_lowercase().contains("authorization: basic"),
            "expected Basic auth header, raw: {raw}"
        );
        assert!(
            !raw.contains("test-secret"),
            "client secret must not leak into the body, raw: {raw}"
        );
    }

    #[tokio::test]
    async fn refresh_grant_sends_refresh_token() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(|_| (200, token_json("refreshed-token")), seen.clone()).await;
        let config = test_config(&format!("{url}/oauth/token"));

        let token = refresh_access_token(&config, "old-refresh-xyz")
            .await
            .unwrap();

        assert_eq!(token.access_token, "refreshed-token");
        let raw = seen.lock().unwrap().clone().unwrap();
        assert!(raw.contains("grant_type=refresh_token"), "raw: {raw}");
        assert!(raw.contains("refresh_token=old-refresh-xyz"), "raw: {raw}");
    }

    #[tokio::test]
    async fn authorization_code_exchange_sends_code_and_verifier() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(|_| (200, token_json("code-token")), seen.clone()).await;
        let config = test_config(&format!("{url}/oauth/token"));

        let token = exchange_authorization_code(&config, "authcode-1", Some("verifier-9"))
            .await
            .unwrap();

        assert_eq!(token.access_token, "code-token");
        let raw = seen.lock().unwrap().clone().unwrap();
        assert!(raw.contains("grant_type=authorization_code"), "raw: {raw}");
        assert!(raw.contains("code=authcode-1"), "raw: {raw}");
        assert!(raw.contains("code_verifier=verifier-9"), "raw: {raw}");
    }

    #[tokio::test]
    async fn error_response_maps_to_auth_failed() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(
            |_| (400, r#"{"error":"invalid_client"}"#.to_string()),
            seen.clone(),
        )
        .await;
        let config = test_config(&format!("{url}/oauth/token"));

        let err = fetch_client_credentials_token(&config).await.unwrap_err();
        match err {
            AgentError::AuthFailed(msg) => assert!(msg.contains("400"), "msg: {msg}"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_access_token_errors() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(
            |_| {
                (
                    200,
                    r#"{"token_type":"Bearer","expires_in":3600}"#.to_string(),
                )
            },
            seen.clone(),
        )
        .await;
        let config = test_config(&format!("{url}/oauth/token"));

        let err = fetch_client_credentials_token(&config).await.unwrap_err();
        assert!(
            matches!(err, AgentError::AuthFailed(_)),
            "expected AuthFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn expires_in_string_is_parsed() {
        let seen = Arc::new(StdMutex::new(None));
        let url = spawn_mock(
            |_| {
                (
                    200,
                    r#"{"access_token":"t","expires_in":"7200"}"#.to_string(),
                )
            },
            seen.clone(),
        )
        .await;
        let config = test_config(&format!("{url}/oauth/token"));

        let token = fetch_client_credentials_token(&config).await.unwrap();
        assert_eq!(token.expires_in_secs, 7200);
    }

    #[tokio::test]
    async fn client_credentials_requires_secret() {
        let mut config = test_config("http://127.0.0.1:1/unused");
        config.client_secret = None;
        let err = fetch_client_credentials_token(&config).await.unwrap_err();
        assert!(
            matches!(err, AgentError::Config(_)),
            "expected Config, got {err:?}"
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 §B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn code_verifier_is_43_urlsafe_chars() {
        let v = generate_code_verifier();
        assert_eq!(v.len(), 43, "verifier: {v}");
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier: {v}"
        );
        assert_ne!(v, generate_code_verifier(), "verifiers must differ");
    }

    #[test]
    fn authorization_url_contains_pkce_params() {
        let mut config = test_config("http://127.0.0.1:1/token");
        config.authorize_url = Some("https://idp.example.com/authorize".to_string());
        config.redirect_uri = Some("http://localhost:9/callback".to_string());
        config.scope = Some("qwen".to_string());

        let (url, verifier) = authorization_url(&config, "state-1").unwrap();

        assert!(
            url.starts_with("https://idp.example.com/authorize?"),
            "url: {url}"
        );
        assert!(url.contains("response_type=code"), "url: {url}");
        assert!(url.contains("client_id=test-client"), "url: {url}");
        assert!(url.contains("code_challenge_method=S256"), "url: {url}");
        assert!(url.contains("state=state-1"), "url: {url}");
        assert!(url.contains("scope=qwen"), "url: {url}");
        // The challenge in the URL must match the returned verifier.
        let expected = code_challenge_s256(&verifier);
        assert!(
            url.contains(&format!("code_challenge={expected}")),
            "url: {url}"
        );
    }

    #[test]
    fn authorization_url_requires_authorize_url() {
        let config = test_config("http://127.0.0.1:1/token");
        let err = authorization_url(&config, "s").unwrap_err();
        assert!(matches!(err, AgentError::Config(_)), "got {err:?}");
    }

    #[test]
    fn token_persist_roundtrip() {
        let dir = std::env::temp_dir().join(format!("qwen-oauth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("oauth").join("qwen_oauth.json");
        let config = test_config("https://idp.example.com/token");
        let token = OAuthToken {
            access_token: "persisted-abc".to_string(),
            refresh_token: Some("persisted-refresh".to_string()),
            expires_in_secs: 3600,
        };

        save_qwen_oauth_token_to(&path, &token, &config).unwrap();
        let loaded = load_qwen_oauth_token_from(&path).expect("token should load");

        assert_eq!(loaded.access_token, "persisted-abc");
        assert_eq!(loaded.refresh_token.as_deref(), Some("persisted-refresh"));
        assert!(loaded.expires_in_secs > 3500);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be owner-only");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_persisted_token_is_not_loaded() {
        let dir = std::env::temp_dir().join(format!("qwen-oauth-exp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("qwen_oauth.json");
        let config = test_config("https://idp.example.com/token");
        let token = OAuthToken {
            access_token: "expired".to_string(),
            refresh_token: None,
            expires_in_secs: 0,
        };

        save_qwen_oauth_token_to(&path, &token, &config).unwrap();
        assert!(load_qwen_oauth_token_from(&path).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_env_requires_client_id_and_token_url() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("QWEN_OAUTH_CLIENT_ID");
        std::env::remove_var("QWEN_OAUTH_TOKEN_URL");

        let err = QwenOAuthConfig::from_env().unwrap_err();
        assert!(matches!(err, AgentError::Config(_)), "got {err:?}");

        std::env::set_var("QWEN_OAUTH_CLIENT_ID", "cid");
        let err = QwenOAuthConfig::from_env().unwrap_err();
        assert!(
            format!("{err}").contains("QWEN_OAUTH_TOKEN_URL"),
            "should complain about the missing token URL, got: {err}"
        );

        std::env::set_var("QWEN_OAUTH_TOKEN_URL", "https://idp.example.com/token");
        std::env::set_var("QWEN_OAUTH_CLIENT_SECRET", "s3cret");
        let config = QwenOAuthConfig::from_env().unwrap();
        assert_eq!(config.client_id, "cid");
        assert_eq!(config.token_url, "https://idp.example.com/token");
        assert_eq!(config.client_secret.as_deref(), Some("s3cret"));

        std::env::remove_var("QWEN_OAUTH_CLIENT_ID");
        std::env::remove_var("QWEN_OAUTH_TOKEN_URL");
        std::env::remove_var("QWEN_OAUTH_CLIENT_SECRET");
    }

    #[tokio::test]
    async fn oauth_manager_caches_and_refreshes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let fetcher: TokenFetcher = Arc::new(move |refresh: Option<String>| {
            let calls = calls_clone.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // First token is immediately expired; the refresh path must
                // mint a fresh one.
                let expired = refresh.is_none() && n == 0;
                Ok(OAuthToken {
                    access_token: if expired {
                        "token-old".to_string()
                    } else {
                        assert!(
                            refresh.is_some(),
                            "refresh grant must pass the refresh token"
                        );
                        "token-new".to_string()
                    },
                    refresh_token: Some("rt-1".to_string()),
                    expires_in_secs: if expired { 0 } else { 3600 },
                })
            })
        });
        let manager = OAuthManager::new(fetcher);

        // First call: fetcher mints an already-expired token.
        assert_eq!(manager.access_token().await.unwrap(), "token-old");
        // Second call: cache is expired → fetcher runs the refresh grant.
        assert_eq!(manager.access_token().await.unwrap(), "token-new");
        // Third call: served from cache, no extra fetch.
        assert_eq!(manager.access_token().await.unwrap(), "token-new");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_sends_oauth_bearer_to_chat_completions() {
        // Mock DashScope-compatible chat endpoint.
        let seen = Arc::new(StdMutex::new(None));
        let chat_url = spawn_mock(
            |_| {
                (
                    200,
                    r#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"oauth says hi"},"finish_reason":"stop"}],"usage":{}}"#.to_string(),
                )
            },
            seen.clone(),
        )
        .await;

        // Manager whose fetcher returns a fixed token without any HTTP.
        let fetcher: TokenFetcher = Arc::new(|_| {
            Box::pin(async move {
                Ok(OAuthToken {
                    access_token: "oauth-access-xyz".to_string(),
                    refresh_token: None,
                    expires_in_secs: 3600,
                })
            })
        });
        let provider = QwenOAuthProvider::new(OAuthManager::new(fetcher), "qwen-turbo")
            .with_base_url(&chat_url);

        let messages = vec![Message::user("hi")];
        let response = provider
            .chat_completion(&messages, &[], None, None, None, None)
            .await
            .unwrap();

        assert_eq!(response.message.content.as_deref(), Some("oauth says hi"));
        let raw = seen.lock().unwrap().clone().unwrap();
        assert!(raw.starts_with("POST /chat/completions "), "raw: {raw}");
        assert!(
            raw.to_lowercase()
                .contains("authorization: bearer oauth-access-xyz"),
            "expected OAuth bearer header, raw: {raw}"
        );
    }

    #[tokio::test]
    async fn provider_stream_yields_chunks_with_oauth_bearer() {
        let seen = Arc::new(StdMutex::new(None));
        let chat_url = spawn_mock(
            |_| {
                (
                    200,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"streamed\"}}]}\n\ndata: [DONE]\n\n"
                        .to_string(),
                )
            },
            seen.clone(),
        )
        .await;

        let fetcher: TokenFetcher = Arc::new(|_| {
            Box::pin(async move {
                Ok(OAuthToken {
                    access_token: "oauth-stream-tok".to_string(),
                    refresh_token: None,
                    expires_in_secs: 3600,
                })
            })
        });
        let provider = QwenOAuthProvider::new(OAuthManager::new(fetcher), "qwen-turbo")
            .with_base_url(&chat_url);

        let messages = vec![Message::user("hi")];
        let chunks: Vec<_> = provider
            .chat_completion_stream(&messages, &[], None, None, None, None)
            .collect()
            .await;
        assert!(!chunks.is_empty(), "expected at least one chunk");
        assert!(chunks.iter().all(|c| c.is_ok()), "chunks: {chunks:?}");

        let raw = seen.lock().unwrap().clone().unwrap();
        assert!(
            raw.to_lowercase()
                .contains("authorization: bearer oauth-stream-tok"),
            "expected OAuth bearer header, raw: {raw}"
        );
    }
}
