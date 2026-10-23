//! `hermes cloud` handler.

use super::common::prompt_line;
use hermes_core::AgentError;
use std::path::PathBuf;

/// Persistent cloud credential cached on disk.
///
/// Mirrors the response from `/api/v1/auth/login` and the user's chosen base
/// URL so subsequent `hermes cloud ...` invocations don't need any env vars.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredCloudCredential {
    base_url: String,
    access_token: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    saved_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn cloud_credential_path() -> PathBuf {
    hermes_config::hermes_home().join("cloud_credentials.json")
}

fn load_cloud_credential() -> Option<StoredCloudCredential> {
    let path = cloud_credential_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cloud_credential(cred: &StoredCloudCredential) -> Result<(), AgentError> {
    let path = cloud_credential_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Io(format!("create config dir: {}", e)))?;
    }
    let body = serde_json::to_string_pretty(cred)
        .map_err(|e| AgentError::Config(format!("serialize credential: {}", e)))?;
    std::fs::write(&path, body)
        .map_err(|e| AgentError::Io(format!("write {}: {}", path.display(), e)))?;
    // Restrict permissions on Unix-y systems.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    Ok(())
}

fn clear_cloud_credential() -> Result<bool, AgentError> {
    let path = cloud_credential_path();
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)
        .map_err(|e| AgentError::Io(format!("remove {}: {}", path.display(), e)))?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_cloud(
    action: Option<String>,
    agent_id: Option<String>,
    prompt: Option<String>,
    env: Option<String>,
    attempts: Option<u8>,
    email: Option<String>,
    password: Option<String>,
    url_override: Option<String>,
    register: bool,
    once: bool,
    poll_interval_secs: u64,
    limit: u32,
    json: bool,
) -> Result<(), AgentError> {
    let action = action.unwrap_or_else(|| "list".to_string());

    // Resolution order (highest to lowest):
    //   env: HERMES_CLOUD_API_URL / HERMES_CLOUD_BASE_URL
    //   stored credential: ~/.hermes/cloud_credentials.json (from `cloud login`)
    //   default: http://127.0.0.1:8787
    let stored = load_cloud_credential();
    let base_url = std::env::var("HERMES_CLOUD_API_URL")
        .ok()
        .or_else(|| std::env::var("HERMES_CLOUD_BASE_URL").ok())
        .or_else(|| stored.as_ref().map(|c| c.base_url.clone()))
        .or_else(|| url_override.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8787".to_string());
    let token = std::env::var("HERMES_CLOUD_TOKEN")
        .ok()
        .or_else(|| std::env::var("HERMES_AUTH_TOKEN").ok())
        .or_else(|| stored.as_ref().map(|c| c.access_token.clone()));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|e| AgentError::Io(format!("failed to initialize cloud HTTP client: {}", e)))?;

    let with_auth = |req: reqwest::RequestBuilder| {
        if let Some(t) = token.as_ref() {
            req.bearer_auth(t)
        } else {
            req
        }
    };

    match action.as_str() {
        "login" => {
            return cloud_login(&client, url_override, email, password, register).await;
        }
        "logout" => {
            return cloud_logout();
        }
        "whoami" => {
            return cloud_whoami(&client, &base_url, token.as_deref()).await;
        }
        "logs" => {
            let id = agent_id.ok_or_else(|| {
                AgentError::Config(
                    "Missing --agent-id. Usage: hermes cloud logs --agent-id <id>".into(),
                )
            })?;
            return cloud_logs(
                &client,
                &base_url,
                token.as_deref(),
                &id,
                once,
                poll_interval_secs,
                json,
            )
            .await;
        }
        _ => {}
    }

    match action.as_str() {
        "list" => {
            let req = with_auth(client.get(format!("{}/api/v1/agents", base_url)));
            let res = req
                .send()
                .await
                .map_err(|e| AgentError::Io(format!("cloud list request failed: {}", e)))?;
            if !res.status().is_success() {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                return Err(AgentError::Io(format!(
                    "cloud list failed ({}): {}",
                    status, body
                )));
            }
            let data: serde_json::Value = res
                .json()
                .await
                .map_err(|e| AgentError::Io(format!("cloud list JSON parse failed: {}", e)))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                );
                return Ok(());
            }
            let items = if let Some(arr) = data.as_array() {
                arr.clone()
            } else {
                data.get("sessions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            println!(
                "Cloud agents: {} (showing up to {})",
                items.len().min(limit as usize),
                limit
            );
            for item in items.into_iter().take(limit as usize) {
                let id = item
                    .get("session_id")
                    .or_else(|| item.get("agent_id"))
                    .or_else(|| item.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("<unknown>");
                let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let model = item.get("model").and_then(|v| v.as_str()).unwrap_or("-");
                let updated = item
                    .get("updated_at")
                    .or_else(|| item.get("last_activity_at"))
                    .or_else(|| item.get("last_active_at"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                println!(
                    "  • {}  status={}  model={}  updated={}",
                    id, status, model, updated
                );
            }
            Ok(())
        }
        "status" => {
            let id = agent_id.ok_or_else(|| {
                AgentError::Config(
                    "Missing --agent-id. Usage: hermes cloud status --agent-id <id>".into(),
                )
            })?;
            let req = with_auth(client.get(format!("{}/api/v1/agents/{}/status", base_url, id)));
            let res = req
                .send()
                .await
                .map_err(|e| AgentError::Io(format!("cloud status request failed: {}", e)))?;
            if !res.status().is_success() {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                return Err(AgentError::Io(format!(
                    "cloud status failed ({}): {}",
                    status, body
                )));
            }
            let data: serde_json::Value = res
                .json()
                .await
                .map_err(|e| AgentError::Io(format!("cloud status JSON parse failed: {}", e)))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                );
            } else {
                println!(
                    "Cloud agent status ({}): {}",
                    id,
                    data.get("status")
                        .or_else(|| data.get("session").and_then(|v| v.get("status")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                );
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                );
            }
            Ok(())
        }
        "exec" => {
            let prompt = prompt.ok_or_else(|| {
                AgentError::Config(
                    "Missing prompt. Usage: hermes cloud exec --agent-id <id> \"your task\"".into(),
                )
            })?;
            if let Some(n) = attempts {
                if !(1..=4).contains(&n) {
                    return Err(AgentError::Config(
                        "--attempts must be between 1 and 4".into(),
                    ));
                }
            }
            if env.is_some() || attempts.is_some() {
                println!(
                    "Note: --env/--attempts are accepted for Codex-style parity; this server may ignore them unless implemented."
                );
            }

            let target_agent_id = if let Some(id) = agent_id {
                id
            } else {
                let create_body = serde_json::json!({
                    "branch": "main",
                    "workspace_mode": "blank",
                    "startup_commands": []
                });
                let req = with_auth(
                    client
                        .post(format!("{}/api/v1/agents", base_url))
                        .json(&create_body),
                );
                let res = req.send().await.map_err(|e| {
                    AgentError::Io(format!("cloud exec create-agent request failed: {}", e))
                })?;
                if !res.status().is_success() {
                    let status = res.status();
                    let body = res.text().await.unwrap_or_default();
                    return Err(AgentError::Io(format!(
                        "cloud exec create-agent failed ({}): {}",
                        status, body
                    )));
                }
                let data: serde_json::Value = res.json().await.map_err(|e| {
                    AgentError::Io(format!("cloud exec create-agent JSON parse failed: {}", e))
                })?;
                data.get("session_id")
                    .or_else(|| data.get("agent_id"))
                    .or_else(|| data.get("id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AgentError::Io(
                            "cloud exec create-agent returned no session_id/agent_id/id".into(),
                        )
                    })?
                    .to_string()
            };

            let body = serde_json::json!({
                "text": prompt
            });
            let req = with_auth(
                client
                    .post(format!(
                        "{}/api/v1/agents/{}/messages",
                        base_url, target_agent_id
                    ))
                    .json(&body),
            );
            let res = req
                .send()
                .await
                .map_err(|e| AgentError::Io(format!("cloud exec request failed: {}", e)))?;
            if !res.status().is_success() {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                return Err(AgentError::Io(format!(
                    "cloud exec failed ({}): {}",
                    status, body
                )));
            }
            let data: serde_json::Value = res
                .json()
                .await
                .map_err(|e| AgentError::Io(format!("cloud exec JSON parse failed: {}", e)))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                );
            } else {
                println!("Cloud exec submitted to agent: {}", target_agent_id);
                if let Some(text) = data
                    .get("text")
                    .or_else(|| data.get("reply"))
                    .and_then(|v| v.as_str())
                {
                    println!("\n{}", text);
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                    );
                }
            }
            Ok(())
        }
        other => Err(AgentError::Config(format!(
            "Unknown cloud action: {}. Use 'list', 'exec', 'status', 'logs', \
             'login', 'logout', or 'whoami'.",
            other
        ))),
    }
}

async fn cloud_login(
    client: &reqwest::Client,
    url_override: Option<String>,
    email: Option<String>,
    password: Option<String>,
    register: bool,
) -> Result<(), AgentError> {
    let stored = load_cloud_credential();
    let base_url = url_override
        .or_else(|| std::env::var("HERMES_CLOUD_API_URL").ok())
        .or_else(|| std::env::var("HERMES_CLOUD_BASE_URL").ok())
        .or_else(|| stored.as_ref().map(|c| c.base_url.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:8787".to_string());

    let email = match email {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => prompt_line("Email: ").await?,
    };
    if email.is_empty() {
        return Err(AgentError::Config("email is required".into()));
    }
    let password = match password {
        Some(v) if !v.is_empty() => v,
        _ => prompt_line("Password: ").await?,
    };
    if password.is_empty() {
        return Err(AgentError::Config("password is required".into()));
    }

    let endpoint = if register {
        format!("{}/api/v1/auth/register", base_url.trim_end_matches('/'))
    } else {
        format!("{}/api/v1/auth/login", base_url.trim_end_matches('/'))
    };
    let body = serde_json::json!({ "email": email, "password": password });
    let res = client
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AgentError::Io(format!(
                "cloud {} request failed: {}",
                if register { "register" } else { "login" },
                e
            ))
        })?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(AgentError::Io(format!(
            "cloud {} failed ({}): {}",
            if register { "register" } else { "login" },
            status,
            text
        )));
    }
    let data: serde_json::Value = res
        .json()
        .await
        .map_err(|e| AgentError::Io(format!("cloud login response parse failed: {}", e)))?;
    let token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::Io("cloud login response missing access_token".into()))?
        .to_string();
    let user = data.get("user");
    let cred = StoredCloudCredential {
        base_url: base_url.trim_end_matches('/').to_string(),
        access_token: token,
        email: user
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(Some(email.clone())),
        user_id: user
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        tenant_id: user
            .and_then(|u| u.get("tenant_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        expires_in: data.get("expires_in").and_then(|v| v.as_u64()),
        saved_at: Some(chrono::Utc::now()),
    };
    save_cloud_credential(&cred)?;
    println!(
        "Signed in to {} as {}",
        cred.base_url,
        cred.email.as_deref().unwrap_or(&email)
    );
    println!("Credential cached at {}", cloud_credential_path().display());
    Ok(())
}

fn cloud_logout() -> Result<(), AgentError> {
    let removed = clear_cloud_credential()?;
    if removed {
        println!("Signed out (removed {})", cloud_credential_path().display());
    } else {
        println!("No cloud credential cached.");
    }
    Ok(())
}

async fn cloud_logs(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
    agent_id: &str,
    once: bool,
    poll_interval_secs: u64,
    json: bool,
) -> Result<(), AgentError> {
    let endpoint = format!(
        "{}/api/v1/agents/{}/messages",
        base_url.trim_end_matches('/'),
        agent_id
    );
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let interval = std::time::Duration::from_secs(poll_interval_secs.max(1));
    let mut iteration = 0u64;
    loop {
        iteration += 1;
        let mut req = client.get(&endpoint);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req
            .send()
            .await
            .map_err(|e| AgentError::Io(format!("cloud logs request failed: {}", e)))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(AgentError::Io(format!(
                "cloud logs failed ({}): {}",
                status, body
            )));
        }
        let data: serde_json::Value = res
            .json()
            .await
            .map_err(|e| AgentError::Io(format!("cloud logs parse failed: {}", e)))?;
        let messages = data
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for msg in messages.iter() {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() && !seen_ids.insert(id.clone()) {
                continue;
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string(msg).unwrap_or_else(|_| msg.to_string())
                );
                continue;
            }
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            let created = msg.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = match role {
                "user" => "↑",
                "assistant" => "↓",
                _ => "•",
            };
            println!("{} [{}] {}: {}", prefix, created, role, content);
        }
        if once {
            if iteration == 1 && messages.is_empty() {
                println!("(no messages yet)");
            }
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }
}

async fn cloud_whoami(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
) -> Result<(), AgentError> {
    let Some(token) = token else {
        println!("Not signed in. Run `hermes cloud login` first.");
        return Ok(());
    };
    let res = client
        .get(format!("{}/api/v1/auth/me", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AgentError::Io(format!("cloud whoami request failed: {}", e)))?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(AgentError::Io(format!(
            "cloud whoami failed ({}): {}",
            status, text
        )));
    }
    let data: serde_json::Value = res
        .json()
        .await
        .map_err(|e| AgentError::Io(format!("cloud whoami parse failed: {}", e)))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
    );
    Ok(())
}
