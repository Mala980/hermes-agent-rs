//! `hermes auth` handler and provider helpers.

use super::common::{hermes_state_root, is_truthy};
use super::gateway::configure_platform_basic_prompts;
use super::serve::resolve_llm_login_token;
use crate::oauth_store::AuthManager;
use crate::oauth_store::FileTokenStore;
use crate::oauth_store::OAuthCredential;
use hermes_cli::app::provider_api_key_from_env;
use hermes_cli::cli::Cli;
use hermes_config::{
    hermes_home, load_user_config_file, platform_token_or_extra, save_config_yaml, validate_config,
    PlatformConfig,
};
use hermes_core::AgentError;
use std::path::PathBuf;

/// Default auth provider: CLI arg, then `HERMES_AUTH_DEFAULT_PROVIDER`, then `openai`.
///
/// Set `HERMES_AUTH_DEFAULT_PROVIDER=telegram` if you primarily use the Telegram gateway.
fn resolve_auth_provider(provider: Option<String>) -> String {
    if let Some(raw) = provider.filter(|s| !s.trim().is_empty()) {
        return normalize_auth_provider(&raw);
    }

    if let Ok(pool) = std::env::var("HERMES_AUTH_PROVIDER_POOL") {
        for item in pool.split(',') {
            let item = item.trim();
            if !item.is_empty() {
                return normalize_auth_provider(item);
            }
        }
    }

    let raw = std::env::var("HERMES_AUTH_DEFAULT_PROVIDER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "openai".to_string());
    normalize_auth_provider(&raw)
}

pub(crate) fn normalize_auth_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "wechat" | "wx" => "weixin".to_string(),
        "qq" => "qqbot".to_string(),
        "tg" => "telegram".to_string(),
        "api-server" => "api_server".to_string(),
        "home-assistant" => "homeassistant".to_string(),
        "wecom-callback" => "wecom_callback".to_string(),
        "mm" => "mattermost".to_string(),
        "github-copilot" => "copilot".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn gateway_platform_provider_key(provider: &str) -> Option<&'static str> {
    match provider {
        "discord" => Some("discord"),
        "slack" => Some("slack"),
        "matrix" => Some("matrix"),
        "mattermost" => Some("mattermost"),
        "signal" => Some("signal"),
        "whatsapp" => Some("whatsapp"),
        "dingtalk" => Some("dingtalk"),
        "feishu" => Some("feishu"),
        "wecom" => Some("wecom"),
        "wecom_callback" => Some("wecom_callback"),
        "qqbot" | "qq" => Some("qqbot"),
        "bluebubbles" => Some("bluebubbles"),
        "email" => Some("email"),
        "sms" => Some("sms"),
        "homeassistant" => Some("homeassistant"),
        "webhook" => Some("webhook"),
        "api_server" => Some("api_server"),
        _ => None,
    }
}

fn is_weixin_provider(provider: &str) -> bool {
    provider == "weixin"
}

async fn telegram_bot_token_from_env_or_prompt() -> Result<String, AgentError> {
    if let Ok(t) = std::env::var("TELEGRAM_BOT_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let line = tokio::task::spawn_blocking(|| {
        use std::io::{self, Write};
        print!("Enter Telegram bot token (from @BotFather): ");
        let _ = io::stdout().flush();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf).map(|_| buf)
    })
    .await
    .map_err(|e| AgentError::Io(format!("telegram token prompt: {e}")))?
    .map_err(|e| AgentError::Io(format!("stdin: {e}")))?;
    let t = line.trim().to_string();
    if t.is_empty() {
        return Err(AgentError::Config(
            "Telegram bot token cannot be empty (set TELEGRAM_BOT_TOKEN or paste token)".into(),
        ));
    }
    Ok(t)
}

async fn weixin_account_id_from_env_or_prompt() -> Result<String, AgentError> {
    if let Ok(v) = std::env::var("WEIXIN_ACCOUNT_ID") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let line = tokio::task::spawn_blocking(|| {
        use std::io::{self, Write};
        print!("Enter Weixin account_id (个人号 wxid/账号标识): ");
        let _ = io::stdout().flush();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf).map(|_| buf)
    })
    .await
    .map_err(|e| AgentError::Io(format!("weixin account_id prompt: {e}")))?
    .map_err(|e| AgentError::Io(format!("stdin: {e}")))?;
    let v = line.trim().to_string();
    if v.is_empty() {
        return Err(AgentError::Config(
            "Weixin account_id cannot be empty (set WEIXIN_ACCOUNT_ID or input manually)".into(),
        ));
    }
    Ok(v)
}

fn weixin_account_file_path(account_id: &str) -> PathBuf {
    hermes_home()
        .join("weixin")
        .join("accounts")
        .join(format!("{account_id}.json"))
}

fn load_persisted_weixin_token(account_id: &str) -> Option<String> {
    let p = weixin_account_file_path(account_id);
    let s = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("token")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
}

fn save_persisted_weixin_account(
    account_id: &str,
    token: &str,
    base_url: Option<&str>,
    user_id: Option<&str>,
) -> Result<(), AgentError> {
    let p = weixin_account_file_path(account_id);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("create weixin account dir: {e}")))?;
    }
    let payload = serde_json::json!({
        "token": token,
        "base_url": base_url.unwrap_or(""),
        "user_id": user_id.unwrap_or(""),
        "saved_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&p, payload.to_string())
        .map_err(|e| AgentError::Io(format!("write weixin account file {}: {e}", p.display())))?;
    Ok(())
}

async fn weixin_token_from_env_or_prompt(account_id: &str) -> Result<String, AgentError> {
    if let Ok(v) = std::env::var("WEIXIN_TOKEN") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if let Some(v) = load_persisted_weixin_token(account_id) {
        return Ok(v);
    }
    let line = tokio::task::spawn_blocking(|| {
        use std::io::{self, Write};
        print!("Enter Weixin iLink token (WEIXIN_TOKEN): ");
        let _ = io::stdout().flush();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf).map(|_| buf)
    })
    .await
    .map_err(|e| AgentError::Io(format!("weixin token prompt: {e}")))?
    .map_err(|e| AgentError::Io(format!("stdin: {e}")))?;
    let v = line.trim().to_string();
    if v.is_empty() {
        return Err(AgentError::Config(
            "Weixin token cannot be empty (set WEIXIN_TOKEN / saved account file / input manually)"
                .into(),
        ));
    }
    Ok(v)
}

fn weixin_login_base_url_from_disk(disk: &hermes_config::GatewayConfig) -> String {
    if let Some(wx) = disk.platforms.get("weixin") {
        if let Some(v) = wx.extra.get("base_url").and_then(|v| v.as_str()) {
            let s = v.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Ok(v) = std::env::var("WEIXIN_BASE_URL") {
        let s = v.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "https://ilinkai.weixin.qq.com".to_string()
}

fn weixin_login_endpoints_from_disk(disk: &hermes_config::GatewayConfig) -> (String, String) {
    let mut start_ep = "ilink/bot/get_bot_qrcode".to_string();
    let mut poll_ep = "ilink/bot/get_qrcode_status".to_string();
    if let Some(wx) = disk.platforms.get("weixin") {
        if let Some(v) = wx
            .extra
            .get("qr_get_bot_qrcode_endpoint")
            .or_else(|| wx.extra.get("qr_start_endpoint"))
            .and_then(|v| v.as_str())
        {
            let s = v.trim();
            if !s.is_empty() {
                start_ep = s.to_string();
            }
        }
        if let Some(v) = wx
            .extra
            .get("qr_get_qrcode_status_endpoint")
            .or_else(|| wx.extra.get("qr_poll_endpoint"))
            .and_then(|v| v.as_str())
        {
            let s = v.trim();
            if !s.is_empty() {
                poll_ep = s.to_string();
            }
        }
    }
    (start_ep, poll_ep)
}

fn weixin_extract_string(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn render_qr_to_terminal(data: &str) {
    let len = data.len();
    let side = (len as f64).sqrt().ceil() as usize;
    if side == 0 {
        println!("(empty QR data)");
        return;
    }
    let bytes = data.as_bytes();
    let is_dark = |row: usize, col: usize| -> bool {
        let idx = row * side + col;
        if idx < bytes.len() {
            bytes[idx] % 2 == 1
        } else {
            false
        }
    };
    let mut row = 0;
    while row < side {
        let mut line = String::new();
        for col in 0..side {
            let top = is_dark(row, col);
            let bottom = if row + 1 < side {
                is_dark(row + 1, col)
            } else {
                false
            };
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        println!("  {}", line);
        row += 2;
    }
}

async fn weixin_qr_login_flow(
    base_url: &str,
    start_ep: &str,
    poll_ep: &str,
    _account_id_hint: Option<&str>,
) -> Result<(String, String, String, String), AgentError> {
    let initial_base = base_url.trim_end_matches('/').to_string();
    let client = reqwest::Client::new();
    async fn fetch_weixin_qr(
        client: &reqwest::Client,
        base: &str,
        start_ep: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let url = format!(
            "{}/{}",
            base.trim_end_matches('/'),
            start_ep.trim_start_matches('/')
        );
        let resp = client
            .get(&url)
            .query(&[("bot_type", "3")])
            .timeout(std::time::Duration::from_secs(35))
            .send()
            .await
            .map_err(|e| AgentError::Io(format!("weixin qr get_bot_qrcode request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Config(format!(
                "weixin qr get_bot_qrcode failed ({}): {}",
                status, body
            )));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| AgentError::Io(format!("weixin qr get_bot_qrcode parse: {e}")))
    }

    let mut current_base = initial_base.clone();
    let mut qr_json = fetch_weixin_qr(&client, &current_base, start_ep).await?;
    let mut qrcode_value = weixin_extract_string(&qr_json, &["qrcode"])
        .ok_or_else(|| AgentError::Config("weixin qr response missing qrcode".to_string()))?;
    let mut qrcode_url =
        weixin_extract_string(&qr_json, &["qrcode_img_content"]).unwrap_or_default();
    let qr_scan_data = if !qrcode_url.trim().is_empty() {
        qrcode_url.clone()
    } else {
        qrcode_value.clone()
    };
    println!();
    if !qrcode_url.trim().is_empty() {
        println!("{}", qrcode_url);
    }
    render_qr_to_terminal(&qr_scan_data);
    println!();
    println!("请使用微信扫描二维码，并在手机端确认登录。");

    let poll_interval = std::time::Duration::from_secs(1);
    let timeout = std::time::Duration::from_secs(480);
    let started = std::time::Instant::now();
    let mut refresh_count = 0u8;
    loop {
        if started.elapsed() >= timeout {
            return Err(AgentError::Config(
                "weixin qr login timed out after 480s".to_string(),
            ));
        }
        tokio::time::sleep(poll_interval).await;
        let poll_url = format!(
            "{}/{}",
            current_base.trim_end_matches('/'),
            poll_ep.trim_start_matches('/')
        );
        let poll_resp = match client
            .get(&poll_url)
            .query(&[("qrcode", qrcode_value.as_str())])
            .timeout(std::time::Duration::from_secs(35))
            .send()
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !poll_resp.status().is_success() {
            continue;
        }
        let poll_json: serde_json::Value = match poll_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let status = weixin_extract_string(&poll_json, &["status"])
            .unwrap_or_else(|| "wait".to_string())
            .to_ascii_lowercase();
        match status.as_str() {
            "wait" => {}
            "scaned" => {
                println!("已扫码，请在微信里确认...");
            }
            "scaned_but_redirect" => {
                if let Some(redirect_host) =
                    weixin_extract_string(&poll_json, &["redirect_host"]).filter(|s| !s.is_empty())
                {
                    current_base = format!("https://{}", redirect_host);
                }
            }
            "expired" => {
                refresh_count = refresh_count.saturating_add(1);
                if refresh_count > 3 {
                    return Err(AgentError::Config(
                        "weixin qr expired too many times".to_string(),
                    ));
                }
                println!("二维码已过期，正在刷新... ({}/3)", refresh_count);
                qr_json = fetch_weixin_qr(&client, &initial_base, start_ep).await?;
                qrcode_value = weixin_extract_string(&qr_json, &["qrcode"]).ok_or_else(|| {
                    AgentError::Config("weixin qr refresh missing qrcode".to_string())
                })?;
                qrcode_url =
                    weixin_extract_string(&qr_json, &["qrcode_img_content"]).unwrap_or_default();
                let refreshed_qr = if !qrcode_url.trim().is_empty() {
                    qrcode_url.clone()
                } else {
                    qrcode_value.clone()
                };
                if !qrcode_url.trim().is_empty() {
                    println!("{}", qrcode_url);
                }
                render_qr_to_terminal(&refreshed_qr);
            }
            "confirmed" => {
                let account_id = weixin_extract_string(&poll_json, &["ilink_bot_id", "account_id"])
                    .unwrap_or_default();
                let token =
                    weixin_extract_string(&poll_json, &["bot_token", "token"]).unwrap_or_default();
                let resolved_base_url =
                    weixin_extract_string(&poll_json, &["baseurl"]).unwrap_or(initial_base.clone());
                let user_id = weixin_extract_string(&poll_json, &["ilink_user_id", "user_id"])
                    .unwrap_or_default();
                if account_id.trim().is_empty() || token.trim().is_empty() {
                    return Err(AgentError::Config(
                        "weixin qr confirmed but payload missing ilink_bot_id/bot_token"
                            .to_string(),
                    ));
                }
                return Ok((account_id, token, resolved_base_url, user_id));
            }
            _ => {}
        }
    }
}

async fn print_auth_status_matrix(cli: &Cli, manager: &AuthManager) -> Result<(), AgentError> {
    let cfg_path = hermes_state_root(cli).join("config.yaml");
    let disk = load_user_config_file(&cfg_path).map_err(|e| AgentError::Config(e.to_string()))?;

    println!("Auth status matrix:");
    println!("-------------------");

    for provider in ["openai", "anthropic", "openrouter", "copilot"] {
        let env_present = provider_api_key_from_env(provider).is_some()
            || (provider == "copilot"
                && std::env::var("GITHUB_COPILOT_TOKEN")
                    .ok()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false));
        let store_present = manager.get_access_token(provider).await?.is_some();
        let (present, source) = if env_present {
            (true, "env")
        } else if store_present {
            (true, "token_store")
        } else {
            (false, "none")
        };
        println!("  - {:<16} present={} source={}", provider, present, source);
    }

    for provider in [
        "telegram",
        "weixin",
        "discord",
        "slack",
        "qqbot",
        "wecom_callback",
    ] {
        let (enabled, cfg_token) = disk
            .platforms
            .get(provider)
            .map(|p| (p.enabled, platform_token_or_extra(p).is_some()))
            .unwrap_or((false, false));
        let env_present = match provider {
            "telegram" => std::env::var("TELEGRAM_BOT_TOKEN")
                .ok()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            "weixin" => std::env::var("WEIXIN_TOKEN")
                .ok()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            "qqbot" => {
                std::env::var("QQ_APP_ID")
                    .ok()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
                    && std::env::var("QQ_CLIENT_SECRET")
                        .ok()
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
            }
            _ => false,
        };
        let (present, source) = if env_present {
            (true, "env")
        } else if cfg_token {
            (true, "config")
        } else {
            (false, "none")
        };
        println!(
            "  - {:<16} present={} source={} enabled={}",
            provider, present, source, enabled
        );
    }
    Ok(())
}

pub(crate) async fn run_auth(
    cli: Cli,
    action: Option<String>,
    provider: Option<String>,
    qr: bool,
) -> Result<(), AgentError> {
    let provider = resolve_auth_provider(provider);
    let auth_store_path = hermes_home().join("auth").join("tokens.json");
    let token_store = FileTokenStore::new(auth_store_path).await?;
    let manager = AuthManager::new(token_store.clone());
    match action.as_deref().unwrap_or("status") {
        "login" => {
            if provider == "telegram" {
                let token = telegram_bot_token_from_env_or_prompt().await?;
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let tg = disk
                    .platforms
                    .entry("telegram".to_string())
                    .or_insert_with(PlatformConfig::default);
                tg.token = Some(token);
                tg.enabled = true;
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Telegram: token saved and platform enabled in {}",
                    cfg_path.display()
                );
                return Ok(());
            }
            if is_weixin_provider(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let qr_preferred = qr
                    || std::env::var("HERMES_WEIXIN_QR_LOGIN")
                        .ok()
                        .map(|v| is_truthy(&v))
                        .unwrap_or(false);
                let mut account_id_opt = disk
                    .platforms
                    .get("weixin")
                    .and_then(|p| p.extra.get("account_id"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let (account_id, token, qr_base_url, qr_user_id) = if qr_preferred {
                    let base_url = weixin_login_base_url_from_disk(&disk);
                    let (start_ep, poll_ep) = weixin_login_endpoints_from_disk(&disk);
                    match weixin_qr_login_flow(
                        &base_url,
                        &start_ep,
                        &poll_ep,
                        account_id_opt.as_deref(),
                    )
                    .await
                    {
                        Ok(pair) => pair,
                        Err(e) => {
                            println!("Weixin QR 登录失败，将回退到手动 token 输入: {}", e);
                            let fallback_account_id = if let Some(v) = account_id_opt.take() {
                                v
                            } else {
                                weixin_account_id_from_env_or_prompt().await?
                            };
                            let fallback_token =
                                weixin_token_from_env_or_prompt(&fallback_account_id).await?;
                            (fallback_account_id, fallback_token, base_url, String::new())
                        }
                    }
                } else {
                    let manual_account_id = if let Some(v) = account_id_opt.take() {
                        v
                    } else {
                        weixin_account_id_from_env_or_prompt().await?
                    };
                    let manual_token = weixin_token_from_env_or_prompt(&manual_account_id).await?;
                    let base_url = weixin_login_base_url_from_disk(&disk);
                    (manual_account_id, manual_token, base_url, String::new())
                };
                let wx = disk
                    .platforms
                    .entry("weixin".to_string())
                    .or_insert_with(PlatformConfig::default);
                wx.enabled = true;
                wx.token = Some(token.clone());
                wx.extra.insert(
                    "account_id".to_string(),
                    serde_json::Value::String(account_id.clone()),
                );
                if !qr_base_url.trim().is_empty() {
                    wx.extra.insert(
                        "base_url".to_string(),
                        serde_json::Value::String(qr_base_url.clone()),
                    );
                }
                save_persisted_weixin_account(
                    &account_id,
                    &token,
                    Some(qr_base_url.as_str()),
                    Some(qr_user_id.as_str()),
                )?;
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Weixin: account_id/token saved and platform enabled in {}",
                    cfg_path.display()
                );
                return Ok(());
            }
            if let Some(platform_key) = gateway_platform_provider_key(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                configure_platform_basic_prompts(&mut disk, platform_key).await?;
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "{}: config updated and platform enabled in {}",
                    platform_key,
                    cfg_path.display()
                );
                return Ok(());
            }
            if provider == "copilot" || provider == "github-copilot" {
                let access_token = hermes_cli::copilot_auth::start_copilot_device_flow().await?;
                manager
                    .save_credential(OAuthCredential {
                        provider: "copilot".to_string(),
                        access_token,
                        refresh_token: None,
                        token_type: "bearer".to_string(),
                        scope: None,
                        expires_at: None,
                    })
                    .await?;
                println!("GitHub device login complete; credential saved as provider 'copilot'.");
                println!("Ensure GITHUB_COPILOT_TOKEN is set for the agent (see printed instructions above).");
                return Ok(());
            }

            let access_token = resolve_llm_login_token(&cli, &provider).await?;
            manager
                .save_credential(OAuthCredential {
                    provider: provider.clone(),
                    access_token,
                    refresh_token: None,
                    token_type: "bearer".to_string(),
                    scope: None,
                    expires_at: None,
                })
                .await?;
            let msg = hermes_cli::auth::login(&provider).await?;
            println!("{}", msg);
        }
        "logout" => {
            if provider == "telegram" {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                if let Some(tg) = disk.platforms.get_mut("telegram") {
                    tg.token = None;
                    tg.enabled = false;
                }
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Telegram: token cleared and platform disabled in {}",
                    cfg_path.display()
                );
                return Ok(());
            }
            if is_weixin_provider(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                if let Some(wx) = disk.platforms.get_mut("weixin") {
                    wx.token = None;
                    wx.enabled = false;
                }
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "Weixin: token cleared and platform disabled in {} (account file retained)",
                    cfg_path.display()
                );
                return Ok(());
            }
            if let Some(platform_key) = gateway_platform_provider_key(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let mut disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                if let Some(p) = disk.platforms.get_mut(platform_key) {
                    p.enabled = false;
                    p.token = None;
                }
                validate_config(&disk).map_err(|e| AgentError::Config(e.to_string()))?;
                save_config_yaml(&cfg_path, &disk)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                println!(
                    "{}: disabled and token cleared in {}",
                    platform_key,
                    cfg_path.display()
                );
                return Ok(());
            }
            let msg = hermes_cli::auth::logout(&provider).await?;
            token_store.remove(&provider).await?;
            println!("{} (removed credential for provider: {})", msg, provider);
        }
        _ => {
            if provider == "all" || provider == "*" {
                print_auth_status_matrix(&cli, &manager).await?;
                return Ok(());
            }
            if provider == "telegram" {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let (has, en) = disk
                    .platforms
                    .get("telegram")
                    .map(|p| {
                        (
                            p.token
                                .as_deref()
                                .map(|t| !t.trim().is_empty())
                                .unwrap_or(false),
                            p.enabled,
                        )
                    })
                    .unwrap_or((false, false));
                println!(
                    "Telegram ({}): token_present={} enabled={}",
                    cfg_path.display(),
                    has,
                    en
                );
                return Ok(());
            }
            if is_weixin_provider(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let (account_id, has_cfg_token, enabled) = disk
                    .platforms
                    .get("weixin")
                    .map(|p| {
                        let account_id = p
                            .extra
                            .get("account_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let has_cfg_token = p
                            .token
                            .as_deref()
                            .map(|t| !t.trim().is_empty())
                            .unwrap_or(false);
                        (account_id, has_cfg_token, p.enabled)
                    })
                    .unwrap_or_else(|| ("".to_string(), false, false));
                let has_saved_token = if account_id.is_empty() {
                    false
                } else {
                    load_persisted_weixin_token(&account_id).is_some()
                };
                println!(
                    "Weixin ({}): account_id={} cfg_token_present={} saved_token_present={} enabled={}",
                    cfg_path.display(),
                    if account_id.is_empty() {
                        "(none)"
                    } else {
                        account_id.as_str()
                    },
                    has_cfg_token,
                    has_saved_token,
                    enabled
                );
                return Ok(());
            }
            if let Some(platform_key) = gateway_platform_provider_key(&provider) {
                let cfg_path = hermes_state_root(&cli).join("config.yaml");
                let disk = load_user_config_file(&cfg_path)
                    .map_err(|e| AgentError::Config(e.to_string()))?;
                let (enabled, token_present) = disk
                    .platforms
                    .get(platform_key)
                    .map(|p| (p.enabled, platform_token_or_extra(p).is_some()))
                    .unwrap_or((false, false));
                println!(
                    "{} ({}): credential_present={} enabled={}",
                    platform_key,
                    cfg_path.display(),
                    token_present,
                    enabled
                );
                return Ok(());
            }
            let env_present = provider_api_key_from_env(&provider).is_some();
            let store_present = manager.get_access_token(&provider).await?.is_some();
            let (has_token, source) = if env_present {
                (true, "env")
            } else if store_present {
                (true, "token_store")
            } else {
                (false, "none")
            };
            println!(
                "Auth status: provider='{}', credential_present={}, source={}",
                provider, has_token, source
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_provider_aliases_cover_primary_chains() {
        assert_eq!(normalize_auth_provider("tg"), "telegram");
        assert_eq!(normalize_auth_provider("wechat"), "weixin");
        assert_eq!(normalize_auth_provider("wx"), "weixin");
        assert_eq!(normalize_auth_provider("api-server"), "api_server");
        assert_eq!(normalize_auth_provider("mm"), "mattermost");
    }

    #[test]
    fn gateway_auth_provider_keys_include_primary_platforms() {
        for key in ["telegram", "weixin", "discord", "slack"] {
            let mapped = gateway_platform_provider_key(key);
            if key == "telegram" || key == "weixin" {
                assert!(mapped.is_none(), "{key} handled by dedicated auth flow");
            } else {
                assert_eq!(mapped, Some(key));
            }
        }
    }
}
