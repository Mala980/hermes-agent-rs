//! `hermes browser close-profile` — kill the browser locking your real profile.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/subcommands/browser.py`
//! + `hermes_cli/browser_connect.py::close_browser_holding_profile`.
//!
//! CONSENTED, DESTRUCTIVE (unsaved tab/form state is lost): only call after
//! the user agreed. Implemented on Unix via `/proc` cmdline scanning and
//! SIGTERM/SIGKILL of the holding process tree (the original used psutil);
//! Windows is reported as unsupported by this port.

use std::path::PathBuf;

use hermes_cli::cli::Cli;
use hermes_core::AgentError;

/// (cli flag key, binary names to probe, linux config subdir, mac support
/// subdir, windows profile subdir)
const BROWSERS: &[(&str, &[&str], &str, &str, &str)] = &[
    (
        "chrome",
        &["google-chrome", "google-chrome-stable", "chrome"],
        "google-chrome",
        "Google/Chrome",
        "Google\\Chrome\\User Data",
    ),
    (
        "chromium",
        &["chromium", "chromium-browser"],
        "chromium",
        "Chromium",
        "Chromium\\User Data",
    ),
    (
        "brave",
        &["brave", "brave-browser"],
        "BraveSoftware/Brave-Browser",
        "BraveSoftware/Brave-Browser",
        "BraveSoftware\\Brave-Browser\\User Data",
    ),
    (
        "edge",
        &["microsoft-edge", "microsoft-edge-stable", "msedge"],
        "microsoft-edge",
        "Microsoft/Edge",
        "Microsoft\\Edge\\User Data",
    ),
];

/// Detect the default Chromium-family browser: the first with a binary on
/// PATH, else the first with an existing profile dir.
fn detect_default_chromium() -> Option<&'static str> {
    for (key, bins, _, _, _) in BROWSERS {
        if bins.iter().any(|b| which_binary(b).is_some()) {
            return Some(key);
        }
    }
    None
}

fn which_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// Default user-data-dir for a browser key on this OS (first existing wins on
/// Linux, mirroring the original's native/snap/flatpak candidate order).
fn real_profile_data_dir(browser: &str) -> Option<PathBuf> {
    let entry = BROWSERS.iter().find(|(k, _, _, _, _)| *k == browser)?;
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        return Some(home.join("Library/Application Support").join(entry.3));
    }
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        return Some(local.join(entry.4));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let native = config_home.join(entry.2);
        let snap = home.join("snap").join(browser).join("common").join(entry.2);
        let flatpak = home
            .join(".var/app")
            .join(format!("com.{}.Browser", capitalize(browser)))
            .join("config")
            .join(entry.2);
        for cand in [&native, &snap, &flatpak] {
            if cand.is_dir() {
                return Some(cand.clone());
            }
        }
        Some(native)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// One scanned process: pid, ppid, argv[0], full cmdline.
struct ProcInfo {
    pid: i32,
    ppid: i32,
    argv0: String,
    cmdline: String,
}

/// Scan /proc for processes (Unix only).
#[cfg(unix)]
fn scan_procs() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let pid: i32 = match name.to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let parts: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let ppid = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|s| {
                // comm is parenthesized; ppid is the field right after it.
                let after = s.rfind(')')?;
                s[after + 1..]
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(0);
        out.push(ProcInfo {
            pid,
            ppid,
            argv0: parts[0].clone(),
            cmdline: parts.join(" "),
        });
    }
    out
}

/// Pids of Chromium-family processes whose cmdline references the profile dir.
#[cfg(unix)]
fn processes_holding_profile(src: &std::path::Path) -> Vec<i32> {
    const BROWSER_BINS: &[&str] = &[
        "chrome",
        "chromium",
        "chrome_crashpad",
        "brave",
        "msedge",
        "google chrome",
    ];
    let norm = src.to_string_lossy().to_lowercase();
    scan_procs()
        .into_iter()
        .filter(|p| {
            let name = p.argv0.to_lowercase();
            if !BROWSER_BINS.iter().any(|b| name.contains(b)) {
                return false;
            }
            let joined = p.cmdline.to_lowercase();
            joined.contains(&norm)
        })
        .map(|p| p.pid)
        .collect()
}

/// Full process tree rooted at `roots` (roots + all descendants).
#[cfg(unix)]
fn process_tree(roots: &[i32]) -> Vec<i32> {
    let procs = scan_procs();
    let mut by_ppid: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    for p in &procs {
        by_ppid.entry(p.ppid).or_default().push(p.pid);
    }
    let mut seen: std::collections::HashSet<i32> = roots.iter().copied().collect();
    let mut stack: Vec<i32> = roots.to_vec();
    while let Some(pid) = stack.pop() {
        if let Some(children) = by_ppid.get(&pid) {
            for &c in children {
                if seen.insert(c) {
                    stack.push(c);
                }
            }
        }
    }
    seen.into_iter().collect()
}

/// Terminate the tree holding the profile and wait for release.
#[cfg(unix)]
fn close_browser_holding_profile(src: &std::path::Path) -> (bool, String) {
    use std::time::{Duration, Instant};

    let holding = processes_holding_profile(src);
    if holding.is_empty() {
        return (
            false,
            "no matching browser process found holding the profile.".to_string(),
        );
    }
    let targets = process_tree(&holding);
    for &pid in &targets {
        // SIGTERM the whole tree.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alive = targets.clone();
    while Instant::now() < deadline && !alive.is_empty() {
        std::thread::sleep(Duration::from_millis(250));
        alive = processes_holding_profile(src);
        if alive.is_empty() {
            break;
        }
        // Escalate survivors past the 8s mark to SIGKILL.
        if deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(7) {
            for &pid in &process_tree(&alive) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    if processes_holding_profile(src).is_empty() {
        (
            true,
            "closed the browser and the profile lock released.".to_string(),
        )
    } else {
        (
            false,
            "closed the browser processes but the profile is still locked — \
             another instance may have relaunched (background/tray mode)."
                .to_string(),
        )
    }
}

/// Handle `hermes browser [close-profile]`.
pub(crate) async fn cmd_browser(
    _cli: Cli,
    action: Option<String>,
    browser: Option<String>,
) -> Result<(), AgentError> {
    match action.as_deref().unwrap_or("close-profile") {
        "close-profile" => {
            let browser: &str = browser
                .as_deref()
                .or(detect_default_chromium())
                .ok_or_else(|| {
                    AgentError::Config(
                        "No supported Chromium default browser detected.".to_string(),
                    )
                })?;
            if !BROWSERS.iter().any(|(k, _, _, _, _)| *k == browser) {
                return Err(AgentError::Config(format!(
                    "Unsupported browser '{}'. Supported: chrome, edge, brave, chromium.",
                    browser
                )));
            }
            let src = real_profile_data_dir(browser).ok_or_else(|| {
                AgentError::Config(format!(
                    "Could not resolve the {} profile directory.",
                    browser
                ))
            })?;
            #[cfg(unix)]
            {
                let (closed, msg) = close_browser_holding_profile(&src);
                if closed {
                    println!("✓ {}", msg);
                    Ok(())
                } else {
                    Err(AgentError::Io(format!("✗ {}", msg)))
                }
            }
            #[cfg(not(unix))]
            {
                let _ = src;
                Err(AgentError::Config(
                    "browser close-profile is only implemented on Unix in this port.".to_string(),
                ))
            }
        }
        other => Err(AgentError::Config(format!(
            "Unknown browser action: '{}'. Available: close-profile",
            other
        ))),
    }
}
