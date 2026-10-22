//! Gateway slash command handler.
//!
//! Processes user-issued slash commands from messaging platforms
//! (Telegram, Discord, Slack, etc.) and returns responses or actions.
//!
//! In-process extensions (Python `register_command` parity) register via
//! [`register_slash_command_extension`]; each handler returns `None` if it
//! does not own the command. Extensions run **before** built-ins so custom
//! commands can be handled without forking core routing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

/// Result of processing a gateway slash command.
#[derive(Debug, Clone)]
pub enum GatewayCommandResult {
    /// Send a text reply to the user.
    Reply(String),
    /// Reset the session and send a reply.
    ResetSession(String),
    /// Switch the model and send a reply.
    SwitchModel { model: String, reply: String },
    /// Switch the personality and send a reply.
    SwitchPersonality { name: String, reply: String },
    /// Stop the currently running agent task.
    StopAgent(String),
    /// Show usage statistics.
    ShowUsage(String),
    /// Compress the conversation context.
    CompressContext(String),
    /// Show insights about the conversation.
    ShowInsights(String),
    /// List, enable, or disable tools.
    ListTools { filter: Option<String> },
    /// Enable a specific tool.
    EnableTool { name: String },
    /// Disable a specific tool.
    DisableTool { name: String },
    /// List or switch sessions.
    ListSessions,
    /// Switch to a specific session.
    SwitchSession { session_id: String },
    /// Show or set usage budget.
    ShowBudget { new_budget: Option<f64> },
    /// Toggle verbose mode.
    ToggleVerbose(String),
    /// Toggle YOLO (auto-approve) mode.
    ToggleYolo(String),
    /// Set the home/working directory.
    SetHome { path: String, reply: String },
    /// Show status information.
    ShowStatus(String),
    /// Show help text.
    ShowHelp(String),
    /// Trigger background task.
    BackgroundTask { prompt: String },
    /// BTW (side conversation) task.
    BtwTask { prompt: String },
    /// Toggle reasoning display.
    ToggleReasoning(String),
    /// Switch to fast model.
    SwitchFast(String),
    /// Retry the last message.
    Retry,
    /// Undo the last exchange.
    Undo,
    /// Approve a user for DM access.
    ApproveUser { user_id: String },
    /// Deny and revoke a user from DM access.
    DenyUser { user_id: String },
    /// Reload MCP server registry/state.
    ReloadMcp,
    /// Switch active provider.
    SwitchProvider { provider: String, reply: String },
    /// Switch active profile.
    SwitchProfile { profile: String, reply: String },
    /// Show or switch current branch.
    SwitchBranch { branch: Option<String> },
    /// Rollback conversation messages.
    Rollback { steps: usize },
    /// Check for updates.
    CheckUpdate,
    /// Unknown command.
    Unknown(String),
    /// No-op (command handled internally).
    Noop,
}

/// Metadata about a slash command.
pub struct CommandInfo {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
}

/// Help line for a dynamically registered slash command (owned strings).
#[derive(Debug, Clone)]
pub struct ExtensionCommandInfo {
    pub usage: String,
    pub description: String,
}

/// Extend gateway slash routing (mirrors Python plugin `register_command`).
pub trait SlashCommandExtension: Send + Sync {
    /// Return [`Some`]`(result)` when this extension handles `cmd` (lowercased token, e.g. `"/foo"`).
    fn try_dispatch(&self, cmd: &str, args: &str) -> Option<GatewayCommandResult>;

    /// Optional extra rows appended after built-in [`all_commands`] in `/help`.
    fn extension_help_entries(&self) -> Vec<ExtensionCommandInfo> {
        vec![]
    }
}

static SLASH_EXTENSIONS: OnceLock<RwLock<Vec<Arc<dyn SlashCommandExtension>>>> = OnceLock::new();

fn slash_extensions() -> &'static RwLock<Vec<Arc<dyn SlashCommandExtension>>> {
    SLASH_EXTENSIONS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register an in-process slash command handler (process-wide, idempotent-safe to call at startup).
pub fn register_slash_command_extension(ext: Arc<dyn SlashCommandExtension>) {
    slash_extensions()
        .write()
        .expect("slash extensions poisoned")
        .push(ext);
}

fn build_help_text() -> String {
    let mut help = String::from("📖 **Available Commands:**\n\n");
    for cmd_info in all_commands() {
        help.push_str(&format!(
            "  `{}` — {}\n",
            cmd_info.usage, cmd_info.description
        ));
    }
    if let Some(lock) = SLASH_EXTENSIONS.get() {
        for ext in lock.read().expect("slash extensions poisoned").iter() {
            for e in ext.extension_help_entries() {
                help.push_str(&format!("  `{}` — {}\n", e.usage, e.description));
            }
        }
    }
    help
}

/// All registered gateway slash commands.
pub fn all_commands() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/new",
            aliases: &[],
            description: "Start a new conversation",
            usage: "/new",
        },
        CommandInfo {
            name: "/reset",
            aliases: &["/clear"],
            description: "Reset the current session",
            usage: "/reset",
        },
        CommandInfo {
            name: "/model",
            aliases: &[],
            description: "Show or switch the LLM model",
            usage: "/model [name]",
        },
        CommandInfo {
            name: "/personality",
            aliases: &["/persona"],
            description: "Show or switch personality",
            usage: "/personality [name]",
        },
        CommandInfo {
            name: "/retry",
            aliases: &[],
            description: "Retry the last message",
            usage: "/retry",
        },
        CommandInfo {
            name: "/undo",
            aliases: &[],
            description: "Undo the last exchange",
            usage: "/undo",
        },
        CommandInfo {
            name: "/compress",
            aliases: &[],
            description: "Compress conversation context",
            usage: "/compress",
        },
        CommandInfo {
            name: "/usage",
            aliases: &["/cost"],
            description: "Show token usage and cost",
            usage: "/usage",
        },
        CommandInfo {
            name: "/stop",
            aliases: &["/cancel"],
            description: "Stop the running agent",
            usage: "/stop",
        },
        CommandInfo {
            name: "/background",
            aliases: &["/bg"],
            description: "Run a task in the background",
            usage: "/background <prompt>",
        },
        CommandInfo {
            name: "/btw",
            aliases: &[],
            description: "Side conversation without context",
            usage: "/btw <prompt>",
        },
        CommandInfo {
            name: "/reasoning",
            aliases: &["/think"],
            description: "Toggle reasoning display",
            usage: "/reasoning",
        },
        CommandInfo {
            name: "/fast",
            aliases: &[],
            description: "Switch to fast model",
            usage: "/fast",
        },
        CommandInfo {
            name: "/verbose",
            aliases: &[],
            description: "Toggle verbose output",
            usage: "/verbose",
        },
        CommandInfo {
            name: "/yolo",
            aliases: &[],
            description: "Toggle auto-approve mode",
            usage: "/yolo",
        },
        CommandInfo {
            name: "/sethome",
            aliases: &[],
            description: "Set the working directory",
            usage: "/sethome <path>",
        },
        CommandInfo {
            name: "/status",
            aliases: &[],
            description: "Show current status",
            usage: "/status",
        },
        CommandInfo {
            name: "/approve",
            aliases: &[],
            description: "Authorize a user id for DM access (admin only)",
            usage: "/approve <user_id>",
        },
        CommandInfo {
            name: "/deny",
            aliases: &[],
            description: "Revoke a user id from DM access (admin only)",
            usage: "/deny <user_id>",
        },
        CommandInfo {
            name: "/reload_mcp",
            aliases: &["/mcp_reload"],
            description: "Reload MCP tool/server registrations",
            usage: "/reload_mcp",
        },
        CommandInfo {
            name: "/provider",
            aliases: &[],
            description: "Show or switch provider",
            usage: "/provider [name]",
        },
        CommandInfo {
            name: "/profile",
            aliases: &[],
            description: "Show or switch profile",
            usage: "/profile [name]",
        },
        CommandInfo {
            name: "/branch",
            aliases: &[],
            description: "Show or switch branch context",
            usage: "/branch [name]",
        },
        CommandInfo {
            name: "/rollback",
            aliases: &[],
            description: "Rollback N latest messages (default 2)",
            usage: "/rollback [steps]",
        },
        CommandInfo {
            name: "/update",
            aliases: &[],
            description: "Check for updates",
            usage: "/update",
        },
        CommandInfo {
            name: "/tools",
            aliases: &[],
            description: "List, enable, or disable tools",
            usage: "/tools [list|enable|disable] [name]",
        },
        CommandInfo {
            name: "/sessions",
            aliases: &[],
            description: "List or switch sessions",
            usage: "/sessions [id]",
        },
        CommandInfo {
            name: "/budget",
            aliases: &[],
            description: "Show or set usage budget",
            usage: "/budget [amount]",
        },
        CommandInfo {
            name: "/insights",
            aliases: &[],
            description: "Show conversation insights",
            usage: "/insights",
        },
        CommandInfo {
            name: "/help",
            aliases: &["/commands"],
            description: "Show this help message",
            usage: "/help",
        },
    ]
}

/// Priority order for the Telegram `setMyCommands` menu, mirroring the
/// original's menu tiers. When the menu cap trims the list, these commands
/// survive first, in this order; any remaining commands fill the menu
/// alphabetically.
pub const TELEGRAM_MENU_PRIORITY: &[&str] = &[
    "help",
    "new",
    "stop",
    "status",
    "sessions",
    "model",
    "provider",
    "retry",
    "undo",
    "verbose",
    "usage",
    "commands",
    "approve",
    "deny",
    "yolo",
    "personality",
    "profile",
    "compress",
    "reasoning",
    "tools",
    "background",
    "btw",
    "fast",
    "reset",
    "branch",
    "rollback",
    "budget",
    "sethome",
    "insights",
    "update",
];

/// Default cap for the Telegram command menu. The original caps at 60 to keep
/// the `setMyCommands` payload under Telegram's undocumented ~4KB limit.
pub const TELEGRAM_MENU_DEFAULT_MAX: usize = 60;

/// Build the (command, description) list for Telegram's `setMyCommands` menu.
///
/// Dynamic: combines builtin [`all_commands`] with any registered
/// [`SlashCommandExtension`] entries and the available skills (see
/// [`discover_menu_skills`]). Names are stripped of the leading `/`
/// and sanitized for Telegram (lowercase alphanumeric + underscore).
///
/// Ordering: commands in [`TELEGRAM_MENU_PRIORITY`] come first, in
/// priority-list order (only those actually present); the remaining commands
/// follow alphabetically. Skills always land in the alphabetical bucket (the
/// priority names are all builtin commands, and skill names colliding with
/// them are skipped). The result is truncated to `max_commands`.
pub fn telegram_menu_commands(max_commands: usize) -> Vec<(String, String)> {
    telegram_menu_commands_with_skills(max_commands, true)
}

/// [`telegram_menu_commands`] with control over skill inclusion (M-C2). Set
/// `include_skills` to false to leave skills out of the menu — wired to
/// `TelegramConfig::command_menu_skills` by the Telegram adapter.
pub fn telegram_menu_commands_with_skills(
    max_commands: usize,
    include_skills: bool,
) -> Vec<(String, String)> {
    let skills = if include_skills {
        discover_menu_skills()
    } else {
        Vec::new()
    };
    build_menu(max_commands, skills)
}

/// Sanitize a command/skill name for Telegram's menu: lowercase
/// alphanumeric + underscore, stripped of the leading `/`, max 32 chars.
fn sanitize_menu_name(name: &str) -> Option<String> {
    let clean: String = name
        .trim_start_matches('/')
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let clean = clean.trim_matches('_').to_string();
    if clean.is_empty() || clean.len() > 32 {
        None
    } else {
        Some(clean)
    }
}

fn build_menu(max_commands: usize, skills: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    // Builtins first (canonical names only, no aliases).
    for info in all_commands() {
        if let Some(name) = sanitize_menu_name(info.name) {
            if seen.insert(name.clone()) {
                out.push((name, info.description.to_string()));
            }
        }
    }

    // Then extension-registered commands.
    if let Some(lock) = SLASH_EXTENSIONS.get() {
        for ext in lock.read().expect("slash extensions poisoned").iter() {
            for entry in ext.extension_help_entries() {
                // usage is like "/cmd args" — take the first token as the name.
                let raw_name = entry.usage.split_whitespace().next().unwrap_or("");
                if let Some(name) = sanitize_menu_name(raw_name) {
                    if seen.insert(name.clone()) {
                        out.push((name, entry.description.clone()));
                    }
                }
            }
        }
    }

    // Then skills (M-C2): already sanitized and deduplicated by
    // `discover_menu_skills`; the `seen` check skips any skill colliding
    // with a builtin or extension command.
    for (name, description) in skills {
        if seen.insert(name.clone()) {
            out.push((name, description));
        }
    }

    out.truncate(100);

    // Priority tiers (M-C3): priority commands first, in priority-list
    // order; the remaining commands fill the menu alphabetically. Then
    // truncate to the caller's cap so the important commands always survive.
    let (priority, mut rest): (Vec<_>, Vec<_>) = out
        .into_iter()
        .partition(|(name, _)| TELEGRAM_MENU_PRIORITY.contains(&name.as_str()));
    let mut priority = priority;
    priority.sort_by_key(|(name, _)| {
        TELEGRAM_MENU_PRIORITY
            .iter()
            .position(|p| *p == name.as_str())
    });
    rest.sort_by(|a, b| a.0.cmp(&b.0));

    let mut menu = priority;
    menu.extend(rest);
    menu.truncate(max_commands);
    menu
}

// ---------------------------------------------------------------------------
// Skill entries in the Telegram command menu (M-C2)
// ---------------------------------------------------------------------------

/// Built-in skill names surfaced in the command menu. Mirrors
/// `hermes_skills::SkillManager::BUILTIN_SKILLS`; kept in sync manually so
/// `hermes-gateway` does not need to depend on `hermes-skills`.
const MENU_BUILTIN_SKILLS: [&str; 26] = [
    "planning",
    "debugging",
    "refactoring",
    "testing",
    "docs",
    "git",
    "review",
    "web-research",
    "terminal",
    "file-edit",
    "security",
    "performance",
    "api-design",
    "db-migrations",
    "incident-response",
    "release",
    "prompting",
    "agent-orchestration",
    "mcp",
    "gateway",
    "voice-mode",
    "cron",
    "memory",
    "session-search",
    "tool-authoring",
    "skill-authoring",
];

/// Skills directory for the menu, mirroring
/// `FileSkillStore::default_dir()` in `hermes-skills` (the platform data dir,
/// e.g. `~/.local/share/hermes/skills` on Linux).
fn menu_skills_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "hermes", "hermes")
        .map(|dirs| dirs.data_dir().join("skills"))
        .unwrap_or_else(|| PathBuf::from(".hermes/skills"))
}

/// Strip a single pair of surrounding single/double quotes from a
/// frontmatter scalar value.
fn unquote_frontmatter_value(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    if value.len() >= 2
        && ((bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Parse the YAML frontmatter of a `SKILL.md` file into
/// `(name, description)`. Tiny and forgiving: the file must start with a
/// `---` line and scanning stops at the closing `---` line; only the first
/// `name:` and `description:` lines are used. Returns `None` when the file
/// cannot be read or has no usable `name:`.
fn parse_skill_frontmatter(path: &Path) -> Option<(String, Option<String>)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            if name.is_none() {
                name = Some(unquote_frontmatter_value(rest));
            }
        } else if let Some(rest) = line.strip_prefix("description:") {
            if description.is_none() {
                description = Some(unquote_frontmatter_value(rest));
            }
        }
    }
    let name = name.filter(|n| !n.is_empty())?;
    let description = description.filter(|d| !d.is_empty());
    Some((name, description))
}

/// Discover skills for the command menu from the default skills directory:
/// builtin skill names merged with `SKILL.md` files found at
/// `<dir>/*/SKILL.md` and `<dir>/*/*/SKILL.md` (categorized skills).
/// Returns `(command_name, description)` pairs — sanitized, deduplicated,
/// sorted alphabetically. Filesystem skills win over builtins when two
/// names sanitize to the same command.
fn discover_menu_skills() -> Vec<(String, String)> {
    discover_menu_skills_in(&menu_skills_dir())
}

/// If `skill_dir` contains a `SKILL.md` with usable frontmatter, push
/// `(name, description)` onto `found` (description falls back to
/// `Skill: <name>`).
fn collect_skill_dir(skill_dir: &Path, found: &mut Vec<(String, String)>) {
    if let Some((name, desc)) = parse_skill_frontmatter(&skill_dir.join("SKILL.md")) {
        let description = desc.unwrap_or_else(|| format!("Skill: {name}"));
        found.push((name, description));
    }
}

fn discover_menu_skills_in(dir: &Path) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();

    // `<dir>/*/SKILL.md`, plus one extra level for categorized skills
    // (`<dir>/<category>/*/SKILL.md`).
    let mut dirs_to_scan = vec![dir.to_path_buf()];
    if let Ok(top) = std::fs::read_dir(dir) {
        dirs_to_scan.extend(top.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    for scan_dir in dirs_to_scan {
        if let Ok(entries) = std::fs::read_dir(&scan_dir) {
            for entry in entries.flatten() {
                collect_skill_dir(&entry.path(), &mut found);
            }
        }
    }

    for builtin in MENU_BUILTIN_SKILLS {
        found.push((builtin.to_string(), format!("Skill: {builtin}")));
    }

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, String)> = found
        .into_iter()
        .filter_map(|(name, description)| {
            let sanitized = sanitize_menu_name(&name)?;
            if seen.insert(sanitized.clone()) {
                Some((sanitized, description))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Parse and dispatch a gateway slash command.
pub fn handle_command(input: &str) -> GatewayCommandResult {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return GatewayCommandResult::Unknown(format!("Not a command: {}", trimmed));
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let args = parts.get(1).map(|s| s.trim()).unwrap_or("");

    if let Some(lock) = SLASH_EXTENSIONS.get() {
        for ext in lock.read().expect("slash extensions poisoned").iter() {
            if let Some(res) = ext.try_dispatch(cmd.as_str(), args) {
                return res;
            }
        }
    }

    dispatch_builtin_command(cmd.as_str(), args)
}

fn dispatch_builtin_command(cmd: &str, args: &str) -> GatewayCommandResult {
    match cmd {
        "/new" => GatewayCommandResult::ResetSession("🆕 New conversation started.".to_string()),
        "/reset" | "/clear" => GatewayCommandResult::ResetSession("🔄 Session reset.".to_string()),
        "/model" => {
            if args.is_empty() {
                GatewayCommandResult::Reply(
                    "Current model shown in status. Use /model <name> to switch.".to_string(),
                )
            } else {
                GatewayCommandResult::SwitchModel {
                    model: args.to_string(),
                    reply: format!("🔀 Model switched to: {}", args),
                }
            }
        }
        "/personality" | "/persona" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Use /personality <name> to switch.".to_string())
            } else {
                GatewayCommandResult::SwitchPersonality {
                    name: args.to_string(),
                    reply: format!("🎭 Personality switched to: {}", args),
                }
            }
        }
        "/retry" => GatewayCommandResult::Retry,
        "/undo" => GatewayCommandResult::Undo,
        "/approve" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Usage: /approve <user_id>".to_string())
            } else {
                GatewayCommandResult::ApproveUser {
                    user_id: args.to_string(),
                }
            }
        }
        "/deny" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Usage: /deny <user_id>".to_string())
            } else {
                GatewayCommandResult::DenyUser {
                    user_id: args.to_string(),
                }
            }
        }
        "/compress" => GatewayCommandResult::CompressContext("📦 Context compressed.".to_string()),
        "/usage" | "/cost" => {
            GatewayCommandResult::ShowUsage("Usage statistics will be shown.".to_string())
        }
        "/stop" | "/cancel" => GatewayCommandResult::StopAgent("⏹ Agent stopped.".to_string()),
        "/background" | "/bg" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Usage: /background <prompt>".to_string())
            } else {
                GatewayCommandResult::BackgroundTask {
                    prompt: args.to_string(),
                }
            }
        }
        "/btw" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Usage: /btw <prompt>".to_string())
            } else {
                GatewayCommandResult::BtwTask {
                    prompt: args.to_string(),
                }
            }
        }
        "/reasoning" | "/think" => {
            GatewayCommandResult::ToggleReasoning("🧠 Reasoning display toggled.".to_string())
        }
        "/fast" => GatewayCommandResult::SwitchFast("⚡ Switched to fast model.".to_string()),
        "/verbose" => GatewayCommandResult::ToggleVerbose("📝 Verbose mode toggled.".to_string()),
        "/yolo" => GatewayCommandResult::ToggleYolo(
            "🤠 YOLO mode toggled. Auto-approving all actions.".to_string(),
        ),
        "/sethome" => {
            if args.is_empty() {
                GatewayCommandResult::Reply("Usage: /sethome <path>".to_string())
            } else {
                GatewayCommandResult::SetHome {
                    path: args.to_string(),
                    reply: format!("🏠 Home directory set to: {}", args),
                }
            }
        }
        "/status" => {
            GatewayCommandResult::ShowStatus("Status information will be shown.".to_string())
        }
        "/reload_mcp" | "/mcp_reload" => GatewayCommandResult::ReloadMcp,
        "/provider" => {
            if args.is_empty() {
                GatewayCommandResult::Reply(
                    "Current provider shown in status. Use /provider <name> to switch.".to_string(),
                )
            } else {
                GatewayCommandResult::SwitchProvider {
                    provider: args.to_string(),
                    reply: format!("🔌 Provider switched to: {}", args),
                }
            }
        }
        "/profile" => {
            if args.is_empty() {
                GatewayCommandResult::Reply(
                    "Current profile shown in status. Use /profile <name> to switch.".to_string(),
                )
            } else {
                GatewayCommandResult::SwitchProfile {
                    profile: args.to_string(),
                    reply: format!("👤 Profile switched to: {}", args),
                }
            }
        }
        "/branch" => {
            if args.is_empty() {
                GatewayCommandResult::SwitchBranch { branch: None }
            } else {
                GatewayCommandResult::SwitchBranch {
                    branch: Some(args.to_string()),
                }
            }
        }
        "/rollback" => {
            if args.is_empty() {
                GatewayCommandResult::Rollback { steps: 2 }
            } else {
                let parsed = args.parse::<usize>().unwrap_or(2).max(1);
                GatewayCommandResult::Rollback { steps: parsed }
            }
        }
        "/update" => GatewayCommandResult::CheckUpdate,
        "/tools" => {
            let tokens: Vec<&str> = args.split_whitespace().collect();
            match tokens.as_slice() {
                [] => GatewayCommandResult::ListTools { filter: None },
                ["list", rest @ ..] => GatewayCommandResult::ListTools {
                    filter: if rest.is_empty() {
                        None
                    } else {
                        Some(rest.join(" "))
                    },
                },
                ["enable", rest @ ..] if !rest.is_empty() => GatewayCommandResult::EnableTool {
                    name: rest.join(" "),
                },
                ["disable", rest @ ..] if !rest.is_empty() => GatewayCommandResult::DisableTool {
                    name: rest.join(" "),
                },
                ["enable"] | ["disable"] => GatewayCommandResult::Reply(
                    "Usage: /tools [list] [filter] | /tools enable <name> | /tools disable <name>"
                        .to_string(),
                ),
                [only] => GatewayCommandResult::ListTools {
                    filter: Some((*only).to_string()),
                },
                _ => GatewayCommandResult::Reply(
                    "Usage: /tools [list] [filter] | /tools enable <name> | /tools disable <name>"
                        .to_string(),
                ),
            }
        }
        "/sessions" => {
            if args.is_empty() {
                GatewayCommandResult::ListSessions
            } else {
                GatewayCommandResult::SwitchSession {
                    session_id: args.to_string(),
                }
            }
        }
        "/budget" => {
            if args.is_empty() {
                GatewayCommandResult::ShowBudget { new_budget: None }
            } else {
                match args.trim().parse::<f64>() {
                    Ok(v) if v.is_finite() && v >= 0.0 => GatewayCommandResult::ShowBudget {
                        new_budget: Some(v),
                    },
                    _ => GatewayCommandResult::Reply(
                        "Usage: /budget [amount] — amount must be a non-negative number."
                            .to_string(),
                    ),
                }
            }
        }
        "/insights" => GatewayCommandResult::ShowInsights(
            "📌 Conversation insights will be shown here.".to_string(),
        ),
        "/help" | "/commands" => GatewayCommandResult::ShowHelp(build_help_text()),
        _ => GatewayCommandResult::Unknown(format!(
            "Unknown command: {}. Type /help for available commands.",
            cmd
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SLASH_EXT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn slash_extension_dispatches_before_builtin_and_merges_help() {
        let _g = SLASH_EXT_TEST_LOCK
            .lock()
            .expect("slash ext test lock poisoned");
        struct E;
        impl SlashCommandExtension for E {
            fn try_dispatch(&self, cmd: &str, args: &str) -> Option<GatewayCommandResult> {
                if cmd == "/__hermes_ext_ping__" {
                    Some(GatewayCommandResult::Reply(format!("pong:{args}")))
                } else {
                    None
                }
            }

            fn extension_help_entries(&self) -> Vec<ExtensionCommandInfo> {
                vec![ExtensionCommandInfo {
                    usage: "/__hermes_ext_ping__".into(),
                    description: "test extension".into(),
                }]
            }
        }
        register_slash_command_extension(Arc::new(E));

        match handle_command("/__hermes_ext_ping__ hi") {
            GatewayCommandResult::Reply(s) => assert_eq!(s, "pong:hi"),
            other => panic!("expected Reply, got {:?}", other),
        }
        match handle_command("/new") {
            GatewayCommandResult::ResetSession(_) => {}
            other => panic!("expected builtin ResetSession, got {:?}", other),
        }
        match handle_command("/help") {
            GatewayCommandResult::ShowHelp(h) => {
                assert!(h.contains("/__hermes_ext_ping__"));
                assert!(h.contains("test extension"));
            }
            other => panic!("expected ShowHelp, got {:?}", other),
        }
    }

    #[test]
    fn test_new_command() {
        match handle_command("/new") {
            GatewayCommandResult::ResetSession(_) => {}
            other => panic!("Expected ResetSession, got {:?}", other),
        }
    }

    #[test]
    fn test_model_switch() {
        match handle_command("/model gpt-4o") {
            GatewayCommandResult::SwitchModel { model, .. } => {
                assert_eq!(model, "gpt-4o");
            }
            other => panic!("Expected SwitchModel, got {:?}", other),
        }
    }

    #[test]
    fn test_help_command() {
        match handle_command("/help") {
            GatewayCommandResult::ShowHelp(text) => {
                assert!(text.contains("Available Commands"));
            }
            other => panic!("Expected ShowHelp, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_command() {
        match handle_command("/xyz123") {
            GatewayCommandResult::Unknown(_) => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn test_background_without_args() {
        match handle_command("/background") {
            GatewayCommandResult::Reply(msg) => {
                assert!(msg.contains("Usage"));
            }
            other => panic!("Expected Reply, got {:?}", other),
        }
    }

    #[test]
    fn test_background_with_args() {
        match handle_command("/background check disk space") {
            GatewayCommandResult::BackgroundTask { prompt } => {
                assert_eq!(prompt, "check disk space");
            }
            other => panic!("Expected BackgroundTask, got {:?}", other),
        }
    }

    #[test]
    fn test_case_insensitive() {
        match handle_command("/NEW") {
            GatewayCommandResult::ResetSession(_) => {}
            other => panic!("Expected ResetSession, got {:?}", other),
        }
    }

    #[test]
    fn test_alias_commands() {
        match handle_command("/clear") {
            GatewayCommandResult::ResetSession(_) => {}
            other => panic!("Expected ResetSession for /clear, got {:?}", other),
        }
        match handle_command("/cancel") {
            GatewayCommandResult::StopAgent(_) => {}
            other => panic!("Expected StopAgent for /cancel, got {:?}", other),
        }
    }

    #[test]
    fn test_admin_commands() {
        match handle_command("/approve alice") {
            GatewayCommandResult::ApproveUser { user_id } => assert_eq!(user_id, "alice"),
            other => panic!("Expected ApproveUser, got {:?}", other),
        }
        match handle_command("/deny bob") {
            GatewayCommandResult::DenyUser { user_id } => assert_eq!(user_id, "bob"),
            other => panic!("Expected DenyUser, got {:?}", other),
        }
    }

    #[test]
    fn test_profile_provider_and_rollback_commands() {
        match handle_command("/provider openai") {
            GatewayCommandResult::SwitchProvider { provider, .. } => assert_eq!(provider, "openai"),
            other => panic!("Expected SwitchProvider, got {:?}", other),
        }
        match handle_command("/profile prod") {
            GatewayCommandResult::SwitchProfile { profile, .. } => assert_eq!(profile, "prod"),
            other => panic!("Expected SwitchProfile, got {:?}", other),
        }
        match handle_command("/rollback 5") {
            GatewayCommandResult::Rollback { steps } => assert_eq!(steps, 5),
            other => panic!("Expected Rollback, got {:?}", other),
        }
    }

    #[test]
    fn test_tools_list_enable_disable() {
        match handle_command("/tools") {
            GatewayCommandResult::ListTools { filter } => assert!(filter.is_none()),
            other => panic!("Expected ListTools, got {:?}", other),
        }
        match handle_command("/tools list") {
            GatewayCommandResult::ListTools { filter } => assert!(filter.is_none()),
            other => panic!("Expected ListTools, got {:?}", other),
        }
        match handle_command("/tools list grep") {
            GatewayCommandResult::ListTools { filter } => {
                assert_eq!(filter.as_deref(), Some("grep"))
            }
            other => panic!("Expected ListTools with filter, got {:?}", other),
        }
        match handle_command("/tools enable fs_read") {
            GatewayCommandResult::EnableTool { name } => assert_eq!(name, "fs_read"),
            other => panic!("Expected EnableTool, got {:?}", other),
        }
        match handle_command("/tools disable shell") {
            GatewayCommandResult::DisableTool { name } => assert_eq!(name, "shell"),
            other => panic!("Expected DisableTool, got {:?}", other),
        }
        match handle_command("/tools enable") {
            GatewayCommandResult::Reply(msg) => assert!(msg.contains("Usage")),
            other => panic!("Expected Reply for bare enable, got {:?}", other),
        }
        match handle_command("/tools my_filter") {
            GatewayCommandResult::ListTools { filter } => {
                assert_eq!(filter.as_deref(), Some("my_filter"))
            }
            other => panic!("Expected ListTools with shorthand filter, got {:?}", other),
        }
    }

    #[test]
    fn test_sessions_and_budget_and_insights() {
        match handle_command("/sessions") {
            GatewayCommandResult::ListSessions => {}
            other => panic!("Expected ListSessions, got {:?}", other),
        }
        match handle_command("/sessions abc-123") {
            GatewayCommandResult::SwitchSession { session_id } => assert_eq!(session_id, "abc-123"),
            other => panic!("Expected SwitchSession, got {:?}", other),
        }
        match handle_command("/sessions multi word id") {
            GatewayCommandResult::SwitchSession { session_id } => {
                assert_eq!(session_id, "multi word id")
            }
            other => panic!("Expected SwitchSession, got {:?}", other),
        }
        match handle_command("/budget") {
            GatewayCommandResult::ShowBudget { new_budget } => assert!(new_budget.is_none()),
            other => panic!("Expected ShowBudget (show), got {:?}", other),
        }
        match handle_command("/budget 12.5") {
            GatewayCommandResult::ShowBudget { new_budget } => assert_eq!(new_budget, Some(12.5)),
            other => panic!("Expected ShowBudget (set), got {:?}", other),
        }
        match handle_command("/budget -1") {
            GatewayCommandResult::Reply(msg) => assert!(msg.contains("Usage")),
            other => panic!("Expected Reply for invalid budget, got {:?}", other),
        }
        match handle_command("/budget nan") {
            GatewayCommandResult::Reply(msg) => assert!(msg.contains("Usage")),
            other => panic!("Expected Reply for invalid budget, got {:?}", other),
        }
        match handle_command("/insights") {
            GatewayCommandResult::ShowInsights(s) => assert!(!s.is_empty()),
            other => panic!("Expected ShowInsights, got {:?}", other),
        }
    }

    #[test]
    fn telegram_menu_commands_is_dynamic_and_sanitized() {
        let menu = telegram_menu_commands(100);
        assert!(
            menu.len() >= 20,
            "expected 20+ commands, got {}",
            menu.len()
        );
        assert!(menu.iter().any(|(n, _)| n == "status"));
        assert!(menu.iter().any(|(n, _)| n == "new"));
        assert!(menu.iter().any(|(n, _)| n == "help"));
        assert!(menu.iter().all(|(n, _)| !n.starts_with('/')));
        let mut names: Vec<&str> = menu.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), menu.len(), "duplicate command names in menu");
        assert!(menu.len() <= 100);
        assert!(menu.iter().all(|(n, d)| n.len() <= 32 && !d.is_empty()));
    }

    #[test]
    fn telegram_menu_commands_priority_ordering() {
        let menu = telegram_menu_commands(100);
        let present: Vec<&str> = TELEGRAM_MENU_PRIORITY
            .iter()
            .filter(|p| menu.iter().any(|(n, _)| n == **p))
            .copied()
            .collect();
        assert!(
            present.len() >= 20,
            "expected 20+ priority commands present, got {}",
            present.len()
        );
        let head: Vec<&str> = menu
            .iter()
            .take(present.len())
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(
            head, present,
            "priority commands must lead the menu in priority-list order"
        );
    }

    #[test]
    fn telegram_menu_commands_cap_respected() {
        // Cap below the number of priority commands: only the top-N
        // priority entries survive, in priority order.
        let menu = telegram_menu_commands(5);
        assert_eq!(menu.len(), 5);
        let names: Vec<&str> = menu.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["help", "new", "stop", "status", "sessions"]);

        // Cap of zero yields an empty menu.
        assert!(telegram_menu_commands(0).is_empty());
    }

    #[test]
    fn telegram_menu_commands_alphabetical_fill() {
        let menu = telegram_menu_commands(100);
        let tail_start = TELEGRAM_MENU_PRIORITY
            .iter()
            .filter(|p| menu.iter().any(|(n, _)| n == **p))
            .count();
        let tail: Vec<&str> = menu
            .iter()
            .skip(tail_start)
            .map(|(n, _)| n.as_str())
            .collect();
        let mut sorted = tail.clone();
        sorted.sort_unstable();
        assert_eq!(tail, sorted, "non-priority commands must be alphabetical");
        assert!(
            tail.iter().all(|n| !TELEGRAM_MENU_PRIORITY.contains(n)),
            "no priority command may appear in the alphabetical tail"
        );
    }

    #[test]
    fn telegram_menu_commands_default_cap_is_60() {
        // The sending path reads TelegramConfig::command_menu_max with a
        // fallback of TELEGRAM_MENU_DEFAULT_MAX (60, mirroring the original);
        // assert the menu builder honors that cap.
        assert_eq!(TELEGRAM_MENU_DEFAULT_MAX, 60);
        let menu = telegram_menu_commands(TELEGRAM_MENU_DEFAULT_MAX);
        assert!(menu.len() <= 60);
        assert!(
            menu.len() >= 20,
            "expected 20+ commands under the default cap, got {}",
            menu.len()
        );
    }

    // -- M-C2: skill entries in the command menu -------------------------

    fn write_skill_md(dir: &std::path::Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("skill md has parent"))
            .expect("create skill dir");
        std::fs::write(&path, contents).expect("write SKILL.md");
    }

    fn fake_skills_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Top-level skill with a name needing sanitization + a description.
        write_skill_md(
            tmp.path(),
            "cooler/SKILL.md",
            "---\nname: \"My Cool Skill!\"\ndescription: Does cool things\n---\n# My Cool Skill\n",
        );
        // Categorized skill colliding with a builtin command name.
        write_skill_md(
            tmp.path(),
            "cat/statusskill/SKILL.md",
            "---\nname: status\ndescription: A skill, not the /status command\n---\nbody\n",
        );
        // Skill without a description gets the fallback.
        write_skill_md(
            tmp.path(),
            "nodesc/SKILL.md",
            "---\nname: no-desc-skill\n---\nbody\n",
        );
        // No frontmatter at all — ignored.
        write_skill_md(tmp.path(), "junk/SKILL.md", "# just markdown\n");
        tmp
    }

    #[test]
    fn menu_skill_frontmatter_parsing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_skill_md(
            tmp.path(),
            "quoted/SKILL.md",
            "---\nname: 'Quoted Name'\ndescription: \"Quoted desc\"\n---\nbody\n",
        );
        assert_eq!(
            parse_skill_frontmatter(&tmp.path().join("quoted/SKILL.md")),
            Some(("Quoted Name".to_string(), Some("Quoted desc".to_string())))
        );
        // Missing name: unusable.
        write_skill_md(
            tmp.path(),
            "noname/SKILL.md",
            "---\ndescription: x\n---\nbody\n",
        );
        assert_eq!(
            parse_skill_frontmatter(&tmp.path().join("noname/SKILL.md")),
            None
        );
        // Missing file: unusable.
        assert_eq!(
            parse_skill_frontmatter(&tmp.path().join("missing/SKILL.md")),
            None
        );
    }

    #[test]
    fn menu_skill_discovery_sanitizes_and_dedupes() {
        let tmp = fake_skills_dir();
        let skills = discover_menu_skills_in(tmp.path());
        let names: Vec<&str> = skills.iter().map(|(n, _)| n.as_str()).collect();
        // Sanitized name lands in the list with its description.
        assert!(skills
            .iter()
            .any(|(n, d)| n == "my_cool_skill" && d == "Does cool things"));
        // Missing description falls back to "Skill: <name>".
        assert!(skills
            .iter()
            .any(|(n, d)| n == "no_desc_skill" && d == "Skill: no-desc-skill"));
        // No-frontmatter file ignored; names unique.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate skill names");
        // Builtin skills are merged in.
        assert!(names.contains(&"planning"));
    }

    #[test]
    fn menu_skill_collisions_with_builtins_are_skipped() {
        let tmp = fake_skills_dir();
        let skills = discover_menu_skills_in(tmp.path());
        let menu = build_menu(100, skills);
        let status_entries: Vec<&(String, String)> =
            menu.iter().filter(|(n, _)| n == "status").collect();
        assert_eq!(status_entries.len(), 1, "status must appear exactly once");
        assert!(
            !status_entries[0]
                .1
                .contains("A skill, not the /status command"),
            "builtin /status description must win over the colliding skill"
        );
    }

    #[test]
    fn menu_skills_land_in_alphabetical_tail() {
        let tmp = fake_skills_dir();
        let skills = discover_menu_skills_in(tmp.path());
        let menu = build_menu(100, skills);
        let tail_start = TELEGRAM_MENU_PRIORITY
            .iter()
            .filter(|p| menu.iter().any(|(n, _)| n == **p))
            .count();
        let tail: Vec<&str> = menu
            .iter()
            .skip(tail_start)
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(tail.contains(&"my_cool_skill"));
        assert!(tail.contains(&"planning"));
        let mut sorted = tail.clone();
        sorted.sort_unstable();
        assert_eq!(tail, sorted, "non-priority tail must be alphabetical");
        assert!(
            tail.iter().all(|n| !TELEGRAM_MENU_PRIORITY.contains(n)),
            "no priority command may appear in the tail"
        );
    }

    #[test]
    fn menu_skills_flag_off_excludes_them() {
        let tmp = fake_skills_dir();
        let skills = discover_menu_skills_in(tmp.path());
        let with = build_menu(100, skills);
        let without = build_menu(100, Vec::new());
        assert!(with.iter().any(|(n, _)| n == "my_cool_skill"));
        assert!(without.iter().all(|(n, _)| n != "my_cool_skill"));
        assert!(without.iter().all(|(n, _)| n != "planning"));
        // The flag-off path of the public entry point is hermetic for names
        // that cannot exist in the real environment.
        assert!(telegram_menu_commands_with_skills(100, false)
            .iter()
            .all(|(n, _)| n != "my_cool_skill"));
        // Backward compatibility: the original entry point includes skills.
        assert!(telegram_menu_commands(100)
            .iter()
            .any(|(n, _)| n == "planning"));
    }

    #[test]
    fn menu_skills_cap_respected() {
        let tmp = fake_skills_dir();
        let skills = discover_menu_skills_in(tmp.path());
        let menu = build_menu(3, skills);
        assert_eq!(menu.len(), 3);
        assert!(menu
            .iter()
            .all(|(n, _)| TELEGRAM_MENU_PRIORITY.contains(&n.as_str())));
    }
}
