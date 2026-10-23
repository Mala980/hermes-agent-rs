//! Advanced CLI commands ported from the original Hermes CLI.
//!
//! Fully implemented: `dashboard`, `hooks`, `bundles`, `vault`, `security`,
//! `connectors`, `verify`, `console`.
//!
//! Honest stubs (the backend infrastructure for these was never ported, so
//! they print a clear "not yet implemented" message and exit cleanly):
//! `computer-use`, `secrets`, `proxy`, `peer`, `project`, `lsp`, `journey`,
//! `curator`, `egress`, `approvals`, `import-agent`, `portal`,
//! `codex-runtime`, `whatsapp-cloud`, `desktop`.
//!
//! Every command exposes `#[derive(clap::Args, Clone)] pub struct <Name>Args` and
//! `pub async fn run_<name>(&<Name>Args) -> anyhow::Result<()>`; the parent
//! wires these into `cli.rs` / `mod.rs`.

use std::path::{Path, PathBuf};

use clap::Args;
use hermes_config::{hermes_home, load_user_config_file, validate_config, GatewayConfig};
use hermes_gateway::hooks::{HookRegistry, LoadedHookInfo};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// dashboard — minimal HTTP status page (loopback only)
// ---------------------------------------------------------------------------

/// `hermes dashboard` — serve a minimal read-only status page.
///
/// The original shipped a full dashboard SPA; this build serves a small
/// status page instead: HTML at `/`, JSON at `/status`. Binds the loopback
/// interface only.
#[derive(Args, Debug, Clone, Default)]
pub struct DashboardArgs {
    /// Port to listen on (loopback interface only).
    #[arg(long, default_value_t = 8383)]
    pub port: u16,
}

/// Build the JSON status snapshot shown by the dashboard.
pub fn status_snapshot() -> serde_json::Value {
    let home = hermes_home();
    let config_path = home.join("config.yaml");
    let config_state = match load_user_config_file(&config_path) {
        Ok(cfg) => {
            if validate_config(&cfg).is_ok() {
                "ok"
            } else {
                "invalid"
            }
        }
        Err(_) => "missing",
    };
    let mut hooks = HookRegistry::new();
    hooks.register_builtins();
    hooks.discover_and_load(&home.join("hooks"));
    serde_json::json!({
        "service": "hermes",
        "version": env!("CARGO_PKG_VERSION"),
        "hermes_home": home.display().to_string(),
        "estop_engaged": super::estop::is_engaged(),
        "config": config_state,
        "hooks": hooks.loaded_hooks().len(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// Render the snapshot as a small HTML status page.
pub fn render_status_html(snapshot: &serde_json::Value) -> String {
    let mut rows = String::new();
    if let Some(obj) = snapshot.as_object() {
        for (key, value) in obj {
            let display = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            rows.push_str(&format!(
                "<tr><th>{}</th><td>{}</td></tr>\n",
                html_escape(key),
                html_escape(&display)
            ));
        }
    }
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <title>hermes status</title></head>\n<body>\n\
         <h1>hermes status</h1>\n<table border=\"1\" cellpadding=\"6\">\n\
         {}\
         </table>\n<p><a href=\"/status\">JSON</a></p>\n</body></html>\n",
        rows
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Map a raw HTTP request to `(status_code, content_type, body)`.
pub fn route_request(request: &str, snapshot: &serde_json::Value) -> (u16, &'static str, String) {
    let path = request
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("/");
    match path {
        "/" | "/index.html" => (
            200,
            "text/html; charset=utf-8",
            render_status_html(snapshot),
        ),
        "/status" => (
            200,
            "application/json",
            serde_json::to_string_pretty(snapshot).unwrap_or_else(|_| "{}".to_string()),
        ),
        _ => (404, "text/plain; charset=utf-8", "not found".to_string()),
    }
}

/// Accept connections on `listener` and answer with the status page.
///
/// `max_requests` caps the number of accepted connections (`None` = serve
/// forever); the cap exists so tests and automation can run a bounded server.
pub async fn serve_dashboard(
    listener: tokio::net::TcpListener,
    max_requests: Option<u64>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut served: u64 = 0;
    loop {
        if max_requests.is_some_and(|max| served >= max) {
            break;
        }
        let (mut socket, _) = listener.accept().await?;
        served += 1;
        let snapshot = status_snapshot();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let (code, content_type, body) = route_request(&request, &snapshot);
            let reason = if code == 200 { "OK" } else { "Not Found" };
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\n\
                 Content-Type: {content_type}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n\
                 {body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
    Ok(())
}

/// Run `hermes dashboard`: serve the status page until Ctrl-C.
pub async fn run_dashboard(args: &DashboardArgs) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    let addr = listener.local_addr()?;
    println!("Hermes status dashboard: http://{addr}/");
    println!("  /         HTML status page");
    println!("  /status   JSON status");
    println!("Loopback only. Press Ctrl-C to stop.");
    tokio::select! {
        result = serve_dashboard(listener, None) => result?,
        _ = tokio::signal::ctrl_c() => println!("\nDashboard stopped."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hooks — list / doctor shell hooks backed by the real HookRegistry
// ---------------------------------------------------------------------------

/// `hermes hooks` — inspect gateway event hooks.
///
/// Backed by `hermes_gateway::hooks::HookRegistry`: built-in hooks plus
/// `HOOK.yaml` manifests discovered under `<HERMES_HOME>/hooks/`.
#[derive(Args, Debug, Clone)]
pub struct HooksArgs {
    /// Action: `list` or `doctor`.
    #[arg(default_value = "list")]
    pub action: String,
}

impl Default for HooksArgs {
    fn default() -> Self {
        Self {
            action: "list".to_string(),
        }
    }
}

/// Discover hooks under `hooks_dir` (built-ins + on-disk manifests).
pub fn discover_hooks(hooks_dir: &Path) -> Vec<LoadedHookInfo> {
    let mut registry = HookRegistry::new();
    registry.register_builtins();
    registry.discover_and_load(hooks_dir);
    registry.loaded_hooks().to_vec()
}

/// On-disk `HOOK.yaml` shape (mirrors the private manifest in
/// `hermes-gateway`, re-declared here for validation).
#[derive(Debug, Deserialize, Default)]
struct HookManifestCheck {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

/// One doctor finding for a hook directory.
#[derive(Debug)]
pub struct HookDoctorIssue {
    pub hook: String,
    pub ok: bool,
    pub detail: String,
}

/// Validate every `HOOK.yaml` under `hooks_dir`; returns one entry per
/// directory that contains a manifest.
pub fn doctor_hooks(hooks_dir: &Path) -> Vec<HookDoctorIssue> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(hooks_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let manifest_path = dir.join("HOOK.yaml");
        if !manifest_path.exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unnamed>")
            .to_string();
        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                out.push(HookDoctorIssue {
                    hook: name,
                    ok: false,
                    detail: format!("cannot read HOOK.yaml: {e}"),
                });
                continue;
            }
        };
        let manifest: HookManifestCheck = match serde_yaml::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                out.push(HookDoctorIssue {
                    hook: name,
                    ok: false,
                    detail: format!("invalid YAML: {e}"),
                });
                continue;
            }
        };
        let mut problems = Vec::new();
        if manifest.events.is_empty() {
            problems.push("no events declared".to_string());
        }
        if manifest.command.is_empty() {
            problems.push("empty `command:` (handlers are spawned as subprocesses)".to_string());
        }
        if manifest.timeout_secs == Some(0) {
            problems.push("timeout_secs is 0".to_string());
        }
        if let Some(cwd) = &manifest.cwd {
            if !Path::new(cwd).exists() {
                problems.push(format!("cwd {cwd:?} does not exist"));
            }
        }
        let _ = (manifest.name, manifest.description);
        if problems.is_empty() {
            out.push(HookDoctorIssue {
                hook: name,
                ok: true,
                detail: format!(
                    "{} event(s), command argv len {}",
                    manifest.events.len(),
                    manifest.command.len()
                ),
            });
        } else {
            out.push(HookDoctorIssue {
                hook: name,
                ok: false,
                detail: problems.join("; "),
            });
        }
    }
    out
}

/// Run `hermes hooks <list|doctor>`.
pub async fn run_hooks(args: &HooksArgs) -> anyhow::Result<()> {
    let hooks_dir = hermes_home().join("hooks");
    match args.action.as_str() {
        "list" => {
            let hooks = discover_hooks(&hooks_dir);
            if hooks.is_empty() {
                println!("No hooks configured.");
                println!(
                    "Drop a directory with a HOOK.yaml into {} to add one.",
                    hooks_dir.display()
                );
                return Ok(());
            }
            println!("Hooks ({}):\n", hooks.len());
            for hook in &hooks {
                println!("  {} — {}", hook.name, hook.description);
                println!("    events: {}", hook.events.join(", "));
                println!("    path:   {}", hook.path);
            }
        }
        "doctor" => {
            let issues = doctor_hooks(&hooks_dir);
            if issues.is_empty() {
                println!("No hook manifests found under {}.", hooks_dir.display());
                return Ok(());
            }
            let mut bad = 0;
            for issue in &issues {
                let mark = if issue.ok { "✓" } else { "✗" };
                println!("{mark} {} — {}", issue.hook, issue.detail);
                if !issue.ok {
                    bad += 1;
                }
            }
            if bad > 0 {
                anyhow::bail!("{bad} hook(s) have problems");
            }
            println!("All hook manifests look good.");
        }
        other => anyhow::bail!("Unknown hooks action {other:?}. Usage: hermes hooks [list|doctor]"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// bundles — list / show skill bundles from <HERMES_HOME>/skill-bundles
// ---------------------------------------------------------------------------

/// `hermes bundles` — list and inspect skill bundles.
///
/// Bundles are `<slug>.yaml` files under `<HERMES_HOME>/skill-bundles/`
/// (override with `HERMES_BUNDLES_DIR`), each naming a set of skills.
#[derive(Args, Debug, Clone)]
pub struct BundlesArgs {
    /// Action: `list` or `show`.
    #[arg(default_value = "list")]
    pub action: String,
    /// Bundle name or slug (for `show`).
    pub name: Option<String>,
}

impl Default for BundlesArgs {
    fn default() -> Self {
        Self {
            action: "list".to_string(),
            name: None,
        }
    }
}

/// One parsed skill bundle.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct BundleInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub instruction: String,
    #[serde(skip)]
    pub slug: String,
    #[serde(skip)]
    pub path: PathBuf,
}

/// Directory bundles are read from (`HERMES_BUNDLES_DIR` or
/// `<HERMES_HOME>/skill-bundles`).
pub fn bundles_dir() -> PathBuf {
    std::env::var("HERMES_BUNDLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| hermes_home().join("skill-bundles"))
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Parse every `*.yaml`/`*.yml` bundle in `dir`; broken files are skipped.
pub fn list_bundles_at(dir: &Path) -> Vec<BundleInfo> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut info: BundleInfo = match serde_yaml::from_str(&raw) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let name = if info.name.trim().is_empty() {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string()
        } else {
            info.name.trim().to_string()
        };
        let slug = slugify(&name);
        if slug.is_empty() || info.skills.is_empty() {
            continue;
        }
        if info.description.trim().is_empty() {
            info.description = format!("Load {} skills as a bundle", info.skills.len());
        }
        info.name = name;
        info.slug = slug;
        info.path = path;
        out.push(info);
    }
    out
}

/// Run `hermes bundles <list|show> [name]`.
pub async fn run_bundles(args: &BundlesArgs) -> anyhow::Result<()> {
    let dir = bundles_dir();
    match args.action.as_str() {
        "list" => {
            let bundles = list_bundles_at(&dir);
            if bundles.is_empty() {
                println!("No bundles installed yet.");
                println!(
                    "Create one as {}/<slug>.yaml with a `skills:` list.",
                    dir.display()
                );
                return Ok(());
            }
            println!("Skill bundles ({}):\n", bundles.len());
            for b in &bundles {
                println!("  /{} — {} ({} skills)", b.slug, b.name, b.skills.len());
            }
        }
        "show" => {
            let want = args.name.as_deref().ok_or_else(|| {
                anyhow::anyhow!("Missing bundle name. Usage: hermes bundles show <name>")
            })?;
            let bundles = list_bundles_at(&dir);
            let found = bundles.iter().find(|b| b.slug == want || b.name == want);
            match found {
                Some(b) => {
                    println!(
                        "/{}\n  name:        {}\n  description: {}",
                        b.slug, b.name, b.description
                    );
                    println!("  skills ({}): {}", b.skills.len(), b.skills.join(", "));
                    if !b.instruction.trim().is_empty() {
                        println!("  instruction: {}", b.instruction.trim());
                    }
                    println!("  path:        {}", b.path.display());
                }
                None => anyhow::bail!("Bundle {want:?} not found under {}", dir.display()),
            }
        }
        other => anyhow::bail!(
            "Unknown bundles action {other:?}. Usage: hermes bundles [list|show] [name]"
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// vault — basic status of the encrypted autofill vault (no crypto here)
// ---------------------------------------------------------------------------

/// `hermes vault` — report on the local encrypted autofill vault.
///
/// Only `status` is implemented: the encrypted store
/// (`<HERMES_HOME>/vault/vault.json.enc`) is never decrypted by this
/// command, so it reports presence, size and key material only.
#[derive(Args, Debug, Clone)]
pub struct VaultArgs {
    /// Action: `status`.
    #[arg(default_value = "status")]
    pub action: String,
}

impl Default for VaultArgs {
    fn default() -> Self {
        Self {
            action: "status".to_string(),
        }
    }
}

/// Observable vault state (no decryption).
#[derive(Debug)]
pub struct VaultStatus {
    pub dir: PathBuf,
    pub initialized: bool,
    pub store_bytes: Option<u64>,
    pub store_mode: Option<String>,
    pub key_present: bool,
}

pub fn vault_status_at(home: &Path) -> VaultStatus {
    let dir = home.join("vault");
    let store = dir.join("vault.json.enc");
    let key = dir.join("vault.key");
    let meta = std::fs::metadata(&store).ok();
    VaultStatus {
        initialized: store.exists(),
        store_bytes: meta.as_ref().map(|m| m.len()),
        store_mode: store_mode(&store),
        key_present: key.exists(),
        dir,
    }
}

#[cfg(unix)]
fn store_mode(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| format!("{:o}", m.permissions().mode() & 0o777))
}

#[cfg(not(unix))]
fn store_mode(_path: &Path) -> Option<String> {
    None
}

/// Run `hermes vault <action>`.
pub async fn run_vault(args: &VaultArgs) -> anyhow::Result<()> {
    match args.action.as_str() {
        "status" => {
            let st = vault_status_at(&hermes_home());
            println!("Vault: {}", st.dir.display());
            if st.initialized {
                println!(
                    "  store: vault.json.enc ({} bytes{})",
                    st.store_bytes.unwrap_or(0),
                    st.store_mode
                        .map(|m| format!(", mode {m}"))
                        .unwrap_or_default()
                );
                println!(
                    "  key:   {}",
                    if st.key_present {
                        "present (vault.key)"
                    } else {
                        "MISSING (vault.key)"
                    }
                );
                println!("  items: not readable without unlocking (encrypted at rest)");
            } else {
                println!("  not initialized (no vault.json.enc)");
            }
            println!("Note: add/remove/list-items need the encrypted vault backend,");
            println!("which is not yet implemented in this build.");
        }
        "add" | "rm" | "list" => {
            println!(
                "`hermes vault {}` is not yet implemented in this build.",
                args.action
            );
            println!("The encrypted vault backend (crypto) has not been ported yet.");
        }
        other => anyhow::bail!("Unknown vault action {other:?}. Usage: hermes vault [status]"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// security — basic config security audit
// ---------------------------------------------------------------------------

/// `hermes security` — audit the local configuration for common issues.
///
/// Checks the parsed `config.yaml` and the secrets file; this is a
/// config-level audit, not a dependency vulnerability scan.
#[derive(Args, Debug, Clone, Default)]
pub struct SecurityArgs {}

/// Severity of one audit finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Pass,
    Warn,
    Fail,
}

/// One audit finding.
#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    pub check: &'static str,
    pub detail: String,
}

fn looks_like_env_ref(value: &str) -> bool {
    let t = value.trim();
    (t.starts_with("${") && t.ends_with('}')) || (t.starts_with('$') && t.len() > 1)
}

/// Audit the config under `home`; never fails on missing files, reports them.
pub fn audit_config(home: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let cfg_path = home.join("config.yaml");

    let cfg: Option<GatewayConfig> = match load_user_config_file(&cfg_path) {
        Ok(c) => {
            let detail = if cfg_path.exists() {
                format!("parsed {}", cfg_path.display())
            } else {
                "no config.yaml; using built-in defaults".to_string()
            };
            findings.push(Finding {
                severity: Severity::Pass,
                check: "config-parse",
                detail,
            });
            Some(c)
        }
        Err(e) => {
            findings.push(Finding {
                severity: Severity::Fail,
                check: "config-parse",
                detail: format!("cannot parse {}: {e}", cfg_path.display()),
            });
            None
        }
    };

    if let Some(cfg) = &cfg {
        match validate_config(cfg) {
            Ok(()) => findings.push(Finding {
                severity: Severity::Pass,
                check: "config-validate",
                detail: "config passes validation".to_string(),
            }),
            Err(e) => findings.push(Finding {
                severity: Severity::Fail,
                check: "config-validate",
                detail: format!("config validation failed: {e}"),
            }),
        }

        for (name, provider) in &cfg.llm_providers {
            if let Some(key) = &provider.api_key {
                if !key.trim().is_empty() && !looks_like_env_ref(key) {
                    findings.push(Finding {
                        severity: Severity::Warn,
                        check: "provider-api-key",
                        detail: format!(
                            "llm_providers.{name}.api_key holds a literal secret; \
                             prefer ${{ENV_VAR}} and keep secrets in ~/.hermes/.env"
                        ),
                    });
                }
            }
        }

        for (platform, pcfg) in &cfg.platforms {
            if !pcfg.enabled {
                continue;
            }
            if pcfg.allowed_users.is_empty() {
                findings.push(Finding {
                    severity: Severity::Warn,
                    check: "platform-allowlist",
                    detail: format!(
                        "platform '{platform}' is enabled with an empty allowed_users \
                         list (fail-closed: every inbound message is rejected)"
                    ),
                });
            }
            if let Some(token) = &pcfg.token {
                if !token.trim().is_empty() && !looks_like_env_ref(token) {
                    findings.push(Finding {
                        severity: Severity::Warn,
                        check: "platform-token",
                        detail: format!(
                            "platforms.{platform}.token holds a literal secret; \
                             prefer ${{ENV_VAR}} and keep secrets in ~/.hermes/.env"
                        ),
                    });
                }
            }
        }
    }

    let env_path = home.join(".env");
    if env_path.exists() {
        let mut detail = format!("found {}", env_path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(&env_path) {
                let mode = m.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    findings.push(Finding {
                        severity: Severity::Warn,
                        check: "dotenv-perms",
                        detail: format!(
                            ".env is readable by group/others (mode {mode:o}); \
                             run: chmod 600 {}",
                            env_path.display()
                        ),
                    });
                } else {
                    detail.push_str(&format!(" (mode {mode:o})"));
                }
            }
        }
        findings.push(Finding {
            severity: Severity::Pass,
            check: "dotenv",
            detail,
        });
    } else {
        findings.push(Finding {
            severity: Severity::Warn,
            check: "dotenv",
            detail: "no .env file; provider secrets must come from the environment".to_string(),
        });
    }

    findings
}

/// Run `hermes security`: print the audit report, exit 1 on failures.
pub async fn run_security(_args: &SecurityArgs) -> anyhow::Result<()> {
    let findings = audit_config(&hermes_home());
    let mut fails = 0;
    let mut warns = 0;
    println!("Security audit:\n");
    for f in &findings {
        let (mark, sev) = match f.severity {
            Severity::Pass => ("✓", "PASS"),
            Severity::Warn => {
                warns += 1;
                ("⚠", "WARN")
            }
            Severity::Fail => {
                fails += 1;
                ("✗", "FAIL")
            }
        };
        println!("{mark} [{sev}] {} — {}", f.check, f.detail);
    }
    println!(
        "\n{} passed, {warns} warning(s), {fails} failure(s).",
        findings
            .iter()
            .filter(|f| f.severity == Severity::Pass)
            .count()
    );
    if fails > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// connectors — list configured connectors (MCP servers)
// ---------------------------------------------------------------------------

/// `hermes connectors` — list the connectors this build can see.
///
/// Hosted connector accounts (OAuth via the Nous portal) are not ported;
/// this lists the locally configured MCP servers from `config.yaml`.
#[derive(Args, Debug, Clone, Default)]
pub struct ConnectorsArgs {}

/// One listed connector.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorInfo {
    pub name: String,
    pub kind: String,
    pub detail: String,
}

/// Collect connectors visible to this build from the loaded config.
pub fn list_connectors(cfg: &GatewayConfig) -> Vec<ConnectorInfo> {
    cfg.mcp_servers
        .iter()
        .map(|s| ConnectorInfo {
            name: s.name.clone(),
            kind: "mcp".to_string(),
            detail: s
                .command
                .clone()
                .or_else(|| s.url.clone())
                .unwrap_or_default(),
        })
        .collect()
}

/// Run `hermes connectors`.
pub async fn run_connectors(_args: &ConnectorsArgs) -> anyhow::Result<()> {
    let home = hermes_home();
    let connectors = load_user_config_file(&home.join("config.yaml"))
        .ok()
        .map(|cfg| list_connectors(&cfg))
        .unwrap_or_default();
    if connectors.is_empty() {
        println!("No connectors configured.");
        println!("Add MCP servers under `mcp_servers:` in ~/.hermes/config.yaml.");
    } else {
        println!("Connectors ({}):\n", connectors.len());
        for c in &connectors {
            let detail = if c.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", c.detail)
            };
            println!("  [{}] {}{}", c.kind, c.name, detail);
        }
    }
    println!("\nNote: hosted connector accounts (Gmail, Notion, …) need the");
    println!("Nous portal / desktop app, which are not ported in this build.");
    Ok(())
}

// ---------------------------------------------------------------------------
// verify — verify the installation and configuration
// ---------------------------------------------------------------------------

/// `hermes verify` — smoke-test the installation and configuration.
#[derive(Args, Debug, Clone, Default)]
pub struct VerifyArgs {
    /// Print machine-readable JSON instead of the human report.
    #[arg(long)]
    pub json: bool,
}

/// One verification check.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyCheck {
    pub name: &'static str,
    pub ok: bool,
    /// Failing a fatal check makes `hermes verify` exit 1.
    pub fatal: bool,
    pub detail: String,
}

/// Run the verification checks against `home`.
pub fn verify_install(home: &Path) -> Vec<VerifyCheck> {
    let mut checks = Vec::new();

    let home_ok = home.is_dir();
    checks.push(VerifyCheck {
        name: "hermes-home",
        ok: home_ok,
        fatal: true,
        detail: if home_ok {
            format!("{} exists", home.display())
        } else {
            format!("{} missing or not a directory", home.display())
        },
    });

    let cfg_path = home.join("config.yaml");
    match load_user_config_file(&cfg_path) {
        Ok(cfg) => {
            let detail = if cfg_path.exists() {
                format!("parsed {}", cfg_path.display())
            } else {
                "no config.yaml; using built-in defaults".to_string()
            };
            checks.push(VerifyCheck {
                name: "config-parse",
                ok: true,
                fatal: true,
                detail,
            });
            match validate_config(&cfg) {
                Ok(()) => checks.push(VerifyCheck {
                    name: "config-validate",
                    ok: true,
                    fatal: true,
                    detail: "config passes validation".to_string(),
                }),
                Err(e) => checks.push(VerifyCheck {
                    name: "config-validate",
                    ok: false,
                    fatal: true,
                    detail: format!("validation failed: {e}"),
                }),
            }
        }
        Err(e) => checks.push(VerifyCheck {
            name: "config-parse",
            ok: false,
            fatal: true,
            detail: format!("cannot parse {}: {e}", cfg_path.display()),
        }),
    }

    let env_path = home.join(".env");
    checks.push(VerifyCheck {
        name: "dotenv",
        ok: env_path.exists(),
        fatal: false,
        detail: if env_path.exists() {
            format!("found {}", env_path.display())
        } else {
            "no .env; secrets must come from the environment".to_string()
        },
    });

    for (name, sub) in [("sessions-dir", "sessions"), ("skills-dir", "skills")] {
        let dir = home.join(sub);
        let ok = dir.exists() || std::fs::create_dir_all(&dir).is_ok();
        checks.push(VerifyCheck {
            name,
            ok,
            fatal: false,
            detail: format!(
                "{} {}",
                dir.display(),
                if ok { "ready" } else { "not writable" }
            ),
        });
    }

    let hook_issues: Vec<_> = doctor_hooks(&home.join("hooks"))
        .into_iter()
        .filter(|i| !i.ok)
        .collect();
    checks.push(VerifyCheck {
        name: "hooks",
        ok: hook_issues.is_empty(),
        fatal: false,
        detail: if hook_issues.is_empty() {
            "all HOOK.yaml manifests valid".to_string()
        } else {
            format!("{} hook(s) with problems", hook_issues.len())
        },
    });

    checks.push(VerifyCheck {
        name: "binary",
        ok: true,
        fatal: false,
        detail: format!("hermes {}", env!("CARGO_PKG_VERSION")),
    });

    checks
}

/// Run `hermes verify`.
pub async fn run_verify(args: &VerifyArgs) -> anyhow::Result<()> {
    let checks = verify_install(&hermes_home());
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&checks).unwrap_or_default()
        );
    } else {
        println!("Verify:\n");
        for c in &checks {
            let mark = if c.ok { "✓" } else { "✗" };
            let fatal = if !c.ok && c.fatal { " (fatal)" } else { "" };
            println!("{mark} {}{} — {}", c.name, fatal, c.detail);
        }
    }
    let failed_fatal = checks.iter().any(|c| c.fatal && !c.ok);
    if failed_fatal {
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// console — a safe, curated REPL (status/info only, not a shell)
// ---------------------------------------------------------------------------

/// `hermes console` — open a safe, curated command console.
///
/// This is not a raw shell and does not expose the full CLI: a small set of
/// read-only info commands (`help`, `status`, `config`, `hooks`, `exit`).
#[derive(Args, Debug, Clone, Default)]
pub struct ConsoleArgs {}

#[derive(Debug, PartialEq, Eq)]
enum ConsoleControl {
    Continue,
    Exit,
}

const CONSOLE_HELP: &str = "\
Available commands:
  help    show this help
  status  show agent status (JSON snapshot)
  config  show config file location and parse state
  hooks   list discovered hooks
  exit    leave the console (Ctrl-D also works)";

/// Dispatch one console line; returns the control flow and output text.
fn dispatch_console_line(line: &str) -> (ConsoleControl, String) {
    match line.trim() {
        "" => (ConsoleControl::Continue, String::new()),
        "help" => (ConsoleControl::Continue, CONSOLE_HELP.to_string()),
        "status" => (
            ConsoleControl::Continue,
            serde_json::to_string_pretty(&status_snapshot()).unwrap_or_default(),
        ),
        "config" => {
            let path = hermes_home().join("config.yaml");
            let state = match load_user_config_file(&path) {
                Ok(cfg) => {
                    if validate_config(&cfg).is_ok() {
                        "ok"
                    } else {
                        "invalid"
                    }
                }
                Err(e) => return (ConsoleControl::Continue, format!("config error: {e}")),
            };
            (
                ConsoleControl::Continue,
                format!("{} [{state}]", path.display()),
            )
        }
        "hooks" => {
            let hooks = discover_hooks(&hermes_home().join("hooks"));
            let mut out = format!("{} hook(s):\n", hooks.len());
            for h in &hooks {
                out.push_str(&format!("  {} ({})\n", h.name, h.events.join(", ")));
            }
            (ConsoleControl::Continue, out)
        }
        "exit" | "quit" => (ConsoleControl::Exit, "Bye.".to_string()),
        other => (
            ConsoleControl::Continue,
            format!("Unknown command {other:?}. Type 'help'."),
        ),
    }
}

/// Run `hermes console`.
pub async fn run_console(_args: &ConsoleArgs) -> anyhow::Result<()> {
    println!("Hermes console — a safe, curated REPL (not a shell). Type 'help'.");
    let stdin = std::io::stdin();
    loop {
        use std::io::Write;
        print!("hermes> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line)?;
        if n == 0 {
            println!();
            break;
        }
        let (control, output) = dispatch_console_line(&line);
        if !output.is_empty() {
            println!("{output}");
        }
        if control == ConsoleControl::Exit {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Honest stubs — backend infrastructure not ported yet.
// ---------------------------------------------------------------------------

/// Declare a stub command: prints what it would do, then a clear
/// "not yet implemented" message, and exits cleanly (never fakes work).
macro_rules! define_stub {
    ($cmd:literal, $struct:ident, $run:ident, $about:literal) => {
        #[doc = concat!("`hermes ", $cmd, "` — ", $about, " (stub: not ported yet).")]
        #[derive(Args, Debug, Clone, Default)]
        pub struct $struct {}

        #[doc = concat!("Run `hermes ", $cmd, "` (stub).")]
        pub async fn $run(_args: &$struct) -> anyhow::Result<()> {
            println!("`hermes {}` — {}.", $cmd, $about);
            println!("Not yet implemented in this build: the backend for this command has not been ported.");
            Ok(())
        }
    };
}

define_stub!(
    "computer-use",
    ComputerUseArgs,
    run_computer_use,
    "drive the local desktop via computer-use tools (screenshots, mouse, keyboard)"
);
define_stub!(
    "secrets",
    SecretsArgs,
    run_secrets,
    "manage provider secrets via 1Password / system keychain"
);
define_stub!(
    "proxy",
    ProxyArgs,
    run_proxy,
    "run the local LLM proxy server"
);
define_stub!("peer", PeerArgs, run_peer, "peer-to-peer agent networking");
define_stub!(
    "project",
    ProjectArgs,
    run_project,
    "project workspace management"
);
define_stub!(
    "lsp",
    LspArgs,
    run_lsp,
    "language-server backed code intelligence"
);
define_stub!(
    "journey",
    JourneyArgs,
    run_journey,
    "onboarding journey state"
);
define_stub!(
    "curator",
    CuratorArgs,
    run_curator,
    "curate and prune skills and memory"
);
define_stub!(
    "egress",
    EgressArgs,
    run_egress,
    "managed network egress policy"
);
define_stub!(
    "approvals",
    ApprovalsArgs,
    run_approvals,
    "pending approval queue"
);
define_stub!(
    "import-agent",
    ImportAgentArgs,
    run_import_agent,
    "import an agent definition from another system"
);
define_stub!("portal", PortalArgs, run_portal, "Nous portal integration");
define_stub!(
    "codex-runtime",
    CodexRuntimeArgs,
    run_codex_runtime,
    "Codex runtime switching"
);
define_stub!(
    "whatsapp-cloud",
    WhatsappCloudArgs,
    run_whatsapp_cloud,
    "WhatsApp Cloud API management"
);
define_stub!(
    "desktop",
    DesktopArgs,
    run_desktop,
    "desktop app (GUI) management"
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    // --- dashboard ---

    #[test]
    fn dashboard_snapshot_shape() {
        let snap = status_snapshot();
        assert_eq!(snap["service"], serde_json::json!("hermes"));
        assert!(snap.get("version").is_some());
        assert!(snap.get("timestamp").is_some());
        let html = render_status_html(&snap);
        assert!(html.contains("<html"));
        assert!(html.contains("hermes"));
    }

    #[test]
    fn dashboard_routes() {
        let snap = status_snapshot();
        let (code, ctype, body) = route_request("GET /status HTTP/1.1\r\n\r\n", &snap);
        assert_eq!(code, 200);
        assert!(ctype.contains("json"));
        assert!(body.contains("hermes"));

        let (code, ctype, _) = route_request("GET / HTTP/1.1", &snap);
        assert_eq!(code, 200);
        assert!(ctype.contains("html"));

        let (code, _, _) = route_request("GET /nope HTTP/1.1", &snap);
        assert_eq!(code, 404);

        // Garbage request line still yields a response, never a panic.
        let (code, _, _) = route_request("", &snap);
        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn dashboard_serves_bounded_requests() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { serve_dashboard(listener, Some(1)).await });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /status HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("200 OK"));
        assert!(text.contains("application/json"));

        server.await.unwrap().unwrap();
    }

    // --- hooks ---

    #[test]
    fn hooks_list_discovers_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let hook_dir = dir.path().join("my-hook");
        std::fs::create_dir_all(&hook_dir).unwrap();
        write(
            &hook_dir,
            "HOOK.yaml",
            "name: my-hook\ndescription: test hook\nevents:\n  - agent:start\ncommand:\n  - echo\n  - hi\n",
        );
        let hooks = discover_hooks(dir.path());
        // One on-disk hook plus the built-in boot-md hook.
        assert!(hooks.iter().any(|h| h.name == "my-hook"));
        let mine = hooks.iter().find(|h| h.name == "my-hook").unwrap();
        assert!(mine.events.contains(&"agent:start".to_string()));
    }

    #[test]
    fn hooks_doctor_flags_bad_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        std::fs::create_dir_all(&good).unwrap();
        write(
            &good,
            "HOOK.yaml",
            "events:\n  - agent:start\ncommand:\n  - echo\n",
        );
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        write(&bad, "HOOK.yaml", "events: []\ncommand: []\n");
        let broken = dir.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        write(&broken, "HOOK.yaml", "name: [unclosed\n");

        let issues = doctor_hooks(dir.path());
        assert_eq!(issues.len(), 3);
        assert!(issues.iter().find(|i| i.hook == "good").unwrap().ok);
        assert!(!issues.iter().find(|i| i.hook == "bad").unwrap().ok);
        assert!(!issues.iter().find(|i| i.hook == "broken").unwrap().ok);
    }

    // --- bundles ---

    #[test]
    fn bundles_list_parses_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "dev.yaml",
            "name: Dev Tools\ndescription: stuff\nskills:\n  - git\n  - gh\n",
        );
        write(dir.path(), "broken.yaml", "name: [unclosed\n");
        write(dir.path(), "empty.yaml", "name: Empty\nskills: []\n");

        let bundles = list_bundles_at(dir.path());
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].slug, "dev-tools");
        assert_eq!(bundles[0].name, "Dev Tools");
        assert_eq!(bundles[0].skills, vec!["git", "gh"]);
    }

    #[test]
    fn bundles_slugify() {
        assert_eq!(slugify("Dev Tools!"), "dev-tools");
        assert_eq!(slugify("  --x--  "), "x");
        assert_eq!(slugify("!!!"), "");
    }

    // --- vault ---

    #[test]
    fn vault_status_uninitialized() {
        let dir = tempfile::tempdir().unwrap();
        let st = vault_status_at(dir.path());
        assert!(!st.initialized);
        assert!(!st.key_present);
        assert_eq!(st.dir, dir.path().join("vault"));
    }

    #[test]
    fn vault_status_initialized() {
        let dir = tempfile::tempdir().unwrap();
        let vdir = dir.path().join("vault");
        std::fs::create_dir_all(&vdir).unwrap();
        write(&vdir, "vault.json.enc", "encrypted-blob");
        write(&vdir, "vault.key", "key-material");
        let st = vault_status_at(dir.path());
        assert!(st.initialized);
        assert!(st.key_present);
        assert_eq!(st.store_bytes, Some(14));
    }

    // --- security ---

    #[test]
    fn security_flags_literal_api_key() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config.yaml",
            "llm_providers:\n  openai:\n    api_key: sk-live-secret\n",
        );
        let findings = audit_config(dir.path());
        assert!(findings
            .iter()
            .any(|f| f.check == "provider-api-key" && f.severity == Severity::Warn));
    }

    #[test]
    fn security_accepts_env_ref_api_key() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config.yaml",
            "llm_providers:\n  openai:\n    api_key: ${OPENAI_API_KEY}\n",
        );
        let findings = audit_config(dir.path());
        assert!(!findings.iter().any(|f| f.check == "provider-api-key"));
    }

    #[test]
    fn security_broken_config_fails() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config.yaml", "llm_providers: [unclosed\n");
        let findings = audit_config(dir.path());
        assert!(findings
            .iter()
            .any(|f| f.check == "config-parse" && f.severity == Severity::Fail));
    }

    #[test]
    fn security_missing_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let findings = audit_config(dir.path());
        let parse = findings.iter().find(|f| f.check == "config-parse").unwrap();
        assert_eq!(parse.severity, Severity::Pass);
        assert!(parse.detail.contains("defaults"));
    }

    #[test]
    fn security_flags_enabled_platform_without_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config.yaml",
            "platforms:\n  telegram:\n    enabled: true\n    token: ${TELEGRAM_BOT_TOKEN}\n",
        );
        let findings = audit_config(dir.path());
        assert!(findings
            .iter()
            .any(|f| f.check == "platform-allowlist" && f.severity == Severity::Warn));
    }

    // --- connectors ---

    #[test]
    fn connectors_lists_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config.yaml",
            "mcp_servers:\n  - name: fs\n    command: npx -y mcp-fs\n  - name: web\n    url: http://localhost:9000/mcp\n",
        );
        let cfg = load_user_config_file(&dir.path().join("config.yaml")).unwrap();
        let conns = list_connectors(&cfg);
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].name, "fs");
        assert_eq!(conns[0].kind, "mcp");
        assert!(conns[0].detail.contains("npx"));
        assert_eq!(conns[1].detail, "http://localhost:9000/mcp");
    }

    // --- verify ---

    #[test]
    fn verify_reports_home_and_config() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config.yaml", "model: test-model\n");
        let checks = verify_install(dir.path());
        assert!(checks
            .iter()
            .any(|c| c.name == "hermes-home" && c.ok && c.fatal));
        assert!(checks
            .iter()
            .any(|c| c.name == "config-parse" && c.ok && c.fatal));
        assert!(checks.iter().any(|c| c.name == "binary" && c.ok));
    }

    #[test]
    fn verify_broken_config_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config.yaml", "max_turns: [unclosed\n");
        let checks = verify_install(dir.path());
        let parse = checks.iter().find(|c| c.name == "config-parse").unwrap();
        assert!(!parse.ok);
        assert!(parse.fatal);
    }

    #[test]
    fn verify_missing_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let checks = verify_install(dir.path());
        let parse = checks.iter().find(|c| c.name == "config-parse").unwrap();
        assert!(parse.ok);
    }

    // --- console ---

    #[test]
    fn console_dispatches_commands() {
        let (ctl, out) = dispatch_console_line("help");
        assert_eq!(ctl, ConsoleControl::Continue);
        assert!(out.contains("status"));

        let (ctl, out) = dispatch_console_line("bogus-cmd");
        assert_eq!(ctl, ConsoleControl::Continue);
        assert!(out.contains("Unknown command"));

        let (ctl, out) = dispatch_console_line("quit");
        assert_eq!(ctl, ConsoleControl::Exit);
        assert!(!out.is_empty());

        let (ctl, out) = dispatch_console_line("");
        assert_eq!(ctl, ConsoleControl::Continue);
        assert!(out.is_empty());
    }

    // --- stubs: every one exits cleanly without doing anything ---

    #[tokio::test]
    async fn stubs_exit_cleanly() {
        run_computer_use(&ComputerUseArgs::default()).await.unwrap();
        run_secrets(&SecretsArgs::default()).await.unwrap();
        run_proxy(&ProxyArgs::default()).await.unwrap();
        run_peer(&PeerArgs::default()).await.unwrap();
        run_project(&ProjectArgs::default()).await.unwrap();
        run_lsp(&LspArgs::default()).await.unwrap();
        run_journey(&JourneyArgs::default()).await.unwrap();
        run_curator(&CuratorArgs::default()).await.unwrap();
        run_egress(&EgressArgs::default()).await.unwrap();
        run_approvals(&ApprovalsArgs::default()).await.unwrap();
        run_import_agent(&ImportAgentArgs::default()).await.unwrap();
        run_portal(&PortalArgs::default()).await.unwrap();
        run_codex_runtime(&CodexRuntimeArgs::default())
            .await
            .unwrap();
        run_whatsapp_cloud(&WhatsappCloudArgs::default())
            .await
            .unwrap();
        run_desktop(&DesktopArgs::default()).await.unwrap();
    }

    #[tokio::test]
    async fn functional_commands_run_without_crashing() {
        // Functional commands that only read local state must not crash
        // even with an empty hermes home. (Dashboard binds a port, console
        // needs stdin, and security/verify exit the process on failure —
        // those are covered by their pure helpers above.)
        run_hooks(&HooksArgs::default()).await.unwrap();
        run_bundles(&BundlesArgs::default()).await.unwrap();
        run_vault(&VaultArgs::default()).await.unwrap();
        run_connectors(&ConnectorsArgs::default()).await.unwrap();
    }
}
