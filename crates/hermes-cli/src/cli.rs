//! CLI argument parsing.
//!
//! Defines the command-line interface for the hermes binary.
//!
//! ## Command tree (v0.2)
//!
//! ```text
//! hermes                          # interactive REPL (default)
//! hermes chat                     # single-shot query
//! hermes serve                    # API server + gateway + cron
//! hermes cloud                    # cloud task workflows (list/exec/status)
//! hermes gateway                  # platform gateway only (no API)
//! hermes config                   # configuration management
//! hermes model                    # model / provider management
//! hermes providers                # list configured LLM providers
//! hermes auth                     # authentication (login/logout/status)
//! hermes tools                    # tool management
//! hermes mcp                      # MCP server management
//! hermes skills                   # skill management
//! hermes plugins                  # plugin management
//! hermes sessions                 # session management (incl. export/dump)
//! hermes memory                   # memory provider management (incl. setup)
//! hermes cron                     # cron job management
//! hermes acp                      # Agent Communication Protocol
//! hermes setup                    # interactive setup wizard
//! hermes doctor                   # dependency & config health check
//! hermes status                   # running status
//! hermes logs                     # recent logs
//! hermes profile                  # profile management
//! hermes backup / import          # backup & restore
//! hermes update / version         # update & version info
//! hermes completion               # shell completions
//! hermes uninstall                # uninstall helper
//! ```

use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CliCommand
// ---------------------------------------------------------------------------

/// Top-level subcommands for the hermes CLI.
#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    // ── Core ────────────────────────────────────────────────────────
    /// Start an interactive session (default when no subcommand is given).
    #[command(name = "hermes")]
    Hermes,

    /// Start an interactive chat session.
    Chat {
        /// Single-shot query (non-interactive).
        #[arg(short, long)]
        query: Option<String>,
        /// Preload a skill before chatting.
        #[arg(long)]
        preload_skill: Option<String>,
        /// Skip confirmation for dangerous tools.
        #[arg(long)]
        yolo: bool,
    },

    /// Start or manage unified runtime (API server + gateway + cron).
    ///
    /// Examples:
    ///   hermes serve                    — start all subsystems
    ///   hermes serve --no-gateway       — API + cron only
    ///   hermes serve --no-cron          — API + gateway only
    ///   hermes serve stop               — stop the running server

    /// Interact with cloud tasks from the terminal.
    ///
    /// Examples:
    ///   hermes cloud login --email me@example.com    — store cloud bearer
    ///   hermes cloud whoami                          — show signed-in identity
    ///   hermes cloud logout                          — drop stored bearer
    ///   hermes cloud                                 — list recent cloud agents
    ///   hermes cloud list --limit 20                 — list recent cloud agents
    ///   hermes cloud exec --agent-id <id> "Fix lint errors"
    ///   hermes cloud status --agent-id <id>          — get cloud agent status
    ///   hermes cloud logs --agent-id <id>            — tail messages (Ctrl-C to stop)
    ///   hermes cloud logs --agent-id <id> --once     — print existing messages and exit

    /// Start or manage the platform gateway (no API server).
    ///
    /// Examples:
    ///   hermes gateway start            — start the gateway
    ///   hermes gateway status           — check gateway status
    ///   hermes gateway setup            — interactive platform setup
    ///   hermes gateway setup whatsapp   — WhatsApp-specific setup

    // ── Configuration & Model ───────────────────────────────────────
    /// Configuration management.
    ///
    /// Examples:
    ///   hermes config                   — show full configuration
    ///   hermes config get model         — get a specific config key
    ///   hermes config set model gpt-4o  — set a config key
    ///   hermes config unset model       — reset a config key to its default
    ///   hermes config set region us-east-1 — set API region

    /// Model and provider management.
    ///
    /// Examples:
    ///   hermes model                    — show current model
    ///   hermes model openai:gpt-4o      — switch model
    ///   hermes model provider list      — list available providers
    ///   hermes model provider set openai — switch provider
    Model {
        /// Provider:model identifier, or sub-action ("provider").
        provider_model: Option<String>,
        /// Second positional for sub-actions (e.g. "list", "set").
        sub_action: Option<String>,
        /// Third positional for sub-action arguments.
        sub_arg: Option<String>,
    },

    /// List configured LLM providers (top-level alias for
    /// `hermes model provider list`).
    ///
    /// Examples:
    ///   hermes providers                — list providers
    ///   hermes providers list           — list providers
    Providers {
        /// Action: "list" (default), "set <name>", or "status".
        action: Option<String>,
    },

    /// Authentication management (login, logout, status).
    ///
    /// Examples:
    ///   hermes auth login               — login to default provider
    ///   hermes auth login lumio          — login to Lumio
    ///   hermes auth logout openai       — logout from OpenAI
    ///   hermes auth status              — show auth status

    // ── Tools & Extensions ──────────────────────────────────────────
    /// List or manage available tools.
    ///
    /// Examples:
    ///   hermes tools                    — list tools
    ///   hermes tools disable web_search — disable a tool (persisted in config)
    ///   hermes tools enable web_search  — re-enable a tool

    /// MCP server management.
    Mcp {
        /// Action: serve/add/remove/list/test/configure.
        action: Option<String>,
        /// Server name or URL.
        #[arg(long)]
        server: Option<String>,
    },

    /// Skills management.
    Skills {
        /// Action: browse/search/install/inspect/list/check/update/audit/uninstall/publish/snapshot/tap/config.
        action: Option<String>,
        /// Skill name or search query.
        name: Option<String>,
        /// Additional argument (e.g. tap URL, snapshot path).
        #[arg(long)]
        extra: Option<String>,
    },

    /// Plugin management.
    Plugins {
        /// Action: install/update/remove/list/enable/disable.
        action: Option<String>,
        /// Plugin name.
        name: Option<String>,
        /// Git branch, tag, or commit to checkout after clone.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Allow clone from hosts outside the default allowlist.
        #[arg(long)]
        allow_untrusted_git_host: bool,
    },

    // ── Data & Sessions ─────────────────────────────────────────────
    /// Session management (list, export, delete, stats, dump).
    ///
    /// Examples:
    ///   hermes sessions                 — list sessions
    ///   hermes sessions show --id abc   — show a session's summary + messages
    ///   hermes sessions export --id abc — export a session
    ///   hermes sessions stats           — usage analytics
    Sessions {
        /// Action: list/show/export/delete/prune/stats/rename/browse/dump.
        action: Option<String>,
        /// Session ID.
        #[arg(long)]
        id: Option<String>,
        /// New name (for rename) or output path (for export/dump).
        #[arg(long)]
        name: Option<String>,
        /// Output path (for export/dump).
        #[arg(long)]
        output: Option<String>,
    },

    /// Memory provider management.
    ///
    /// Examples:
    ///   hermes memory                   — show memory status
    ///   hermes memory setup             — run setup wizard
    ///   hermes memory setup redis       — configure redis
    ///   hermes memory off               — disable memory provider
    ///   hermes memory remember "prefers dark mode" — save a memory note
    ///   hermes memory recall "dark mode" — search memory notes
    ///   hermes memory forget "dark mode" — delete matching memory notes
    Memory {
        /// Action: setup/status/off/remember/recall/forget.
        action: Option<String>,
        /// Action argument: provider name (setup), or query text
        /// (remember/recall/forget).
        arg: Option<String>,
        /// Memory store for remember/recall/forget: "memory" (agent notes,
        /// default), "user" (user profile), or "all" (recall/forget only).
        #[arg(long, default_value = "memory")]
        target: String,
    },

    /// Cron job management.

    /// ACP (Agent Communication Protocol) server.
    Acp {
        /// Action: start/status.
        action: Option<String>,
    },

    // ── System ──────────────────────────────────────────────────────
    /// Run the interactive setup wizard.

    /// Check dependencies and configuration health.

    /// Show running status (active sessions, model, uptime).

    /// Show recent logs.
    Logs(crate::cmd::logs::LogsArgs),

    /// Profile management (list, switch, create).
    Profile(crate::cmd::profile::ProfileArgs),

    /// Backup configuration and sessions.
    Backup {
        /// Output path for backup archive.
        output: Option<String>,
    },

    /// Import configuration from backup.
    Import {
        /// Path to backup archive.
        path: String,
    },

    /// Check for updates.

    /// Show version information.
    Version,

    /// Interactive setup wizard.
    Setup,

    /// Generate shell completion scripts.

    /// Uninstall helper (removes ~/.hermes by default).

    // ── Ported from hermes-rs ───────────────────────────────────────
    /// Text-to-speech and speech-to-text utilities.
    Voice {
        #[command(subcommand)]
        cmd: VoiceCmd,
    },

    /// Run prompts from a JSONL file non-interactively.
    ///
    /// Each input line: {"prompt": "...", "session": "optional-id"}.
    /// Output: JSONL records with response/usage/duration.

    /// Run evaluation suites.

    /// Standalone incoming webhook server (POST /hook → agent → JSON).
    Webhook {
        /// Port to listen on.
        #[arg(long, default_value = "8080")]
        port: u16,
    },

    /// One-shot non-interactive run (like `chat --query`).

    /// Launch the TUI directly.

    // ── Quick-wins port (NousResearch/hermes-agent parity) ─────────────
    /// OpenClaw migration helpers (ported from the original).
    ///
    /// Examples:
    ///   hermes claw               — check for legacy OpenClaw artefacts
    ///   hermes claw migrate       — import OpenClaw settings into Hermes
    ///   hermes claw cleanup       — remove legacy OpenClaw directories
    Claw {
        /// Action: "migrate", "cleanup", or "status" (defaults to "status").
        action: Option<String>,
    },

    /// Device pairing management (ported from the original).
    ///
    /// Examples:
    ///   hermes pairing                          — list paired devices
    ///   hermes pairing approve --device-id <id> — approve a device
    ///   hermes pairing revoke --device-id <id>  — revoke a device
    ///   hermes pairing clear-pending             — drop pending requests
    Pairing {
        /// Action: "list", "approve", "revoke", or "clear-pending" (defaults to "list").
        action: Option<String>,
        /// Device id for "approve" / "revoke".
        #[arg(long)]
        device_id: Option<String>,
    },

    /// List, switch, and tweak skins (ported from the original).
    ///
    /// `set` changes ONE color of the active skin in place; editing a
    /// built-in skin forks it into `<name>-custom.yaml` so the built-in
    /// stays intact.
    ///
    /// Examples:
    ///   hermes skin                  — list skins
    ///   hermes skin use ares         — switch to the ares skin
    ///   hermes skin set ui_accent #FF0000

    /// Emergency stop: pause cron/kanban dispatch and new gateway turns.
    ///
    /// Writes the ESTOP sentinel at `$HERMES_HOME/ESTOP`; in-flight work is
    /// never killed. Run `hermes resume` to lift the pause.
    Pause {
        /// Optional reason stored in the sentinel and shown to users.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Lift the emergency stop set by `hermes pause`.
    Resume,

    /// Real-profile browsing helpers (ported from the original).
    ///
    /// `close-profile` terminates the browser process tree holding your
    /// default profile so Hermes can copy it — DESTRUCTIVE (unsaved tabs
    /// in that browser are lost). Run only with the user's explicit OK.
    Browser {
        /// Action: "close-profile" (defaults to showing help).
        action: Option<String>,
        /// Override the detected default browser
        /// (chrome/edge/brave/brave-origin/chromium).
        #[arg(long)]
        browser: Option<String>,
    },

    /// Manage the fallback provider chain (ported from the original).
    ///
    /// Fallbacks are tried in order when the primary route fails
    /// (rate-limit, 5xx, connection errors).
    ///
    /// Examples:
    ///   hermes fallback                                    — list the chain
    ///   hermes fallback add --provider openai --model gpt-4o
    ///   hermes fallback remove 1
    ///   hermes fallback clear

    /// Manage Mixture-of-Agents model presets (ported from the original).
    ///
    /// Examples:
    ///   hermes moa                                             — list presets
    ///   hermes moa configure --reference openai:gpt-4o --aggregator anthropic:claude-opus-4-6
    ///   hermes moa delete my-preset

    /// Slack helpers (ported from the original).
    ///
    /// Examples:
    ///   hermes slack manifest                  — print the app manifest JSON
    ///   hermes slack manifest --write           — write to ~/.hermes/slack-manifest.json
    ///   hermes slack manifest --slashes-only    — only the slash_commands section
    Slack {
        /// Action: "manifest" (defaults to "manifest").
        action: Option<String>,
        /// App name for the manifest (default: "Hermes").
        #[arg(long)]
        name: Option<String>,
        /// Short description for the manifest.
        #[arg(long)]
        description: Option<String>,
        /// Long description text.
        #[arg(long)]
        long_description: Option<String>,
        /// Read the long description from a file.
        #[arg(long)]
        long_description_file: Option<String>,
        /// Print only the `slash_commands` section.
        #[arg(long)]
        slashes_only: bool,
        /// Use the agent messaging experience instead of assistant.
        #[arg(long)]
        agent_view: bool,
        /// No assistant/agent messaging experience.
        #[arg(long)]
        no_assistant: bool,
        /// Write the manifest to a file: `--write [path]`
        /// (bare `--write` uses `~/.hermes/slack-manifest.json`).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        write: Option<String>,
    },
    // ── Phase 5: session & system utilities ─────────────────────────
    /// Kanban board management.
    Kanban(crate::cmd::kanban::KanbanArgs),
    /// Checkpoint management.
    Checkpoints(crate::cmd::checkpoints::CheckpointsArgs),
    /// Token usage analytics.
    Usage(crate::cmd::usage_cmd::UsageArgs),
    /// Measure prompt component sizes.
    PromptSize(crate::cmd::prompt_size::PromptSizeArgs),
    /// Git worktree management.
    Worktree(crate::cmd::worktree::WorktreeArgs),
    /// System monitoring.
    Monitoring(crate::cmd::monitoring::MonitoringArgs),
    /// Session insights.
    Insights(crate::cmd::insights::InsightsArgs),
    /// Debug report generation.
    Debug(crate::cmd::debug_cmd::DebugArgs),
    /// Diagnostic dump.
    Dump(crate::cmd::dump_cmd::DumpArgs),
    /// Dashboard HTTP status page.
    Dashboard(crate::cmd::advanced::DashboardArgs),
    /// Hook management.
    Hooks(crate::cmd::advanced::HooksArgs),
    /// Bundle management.
    Bundles(crate::cmd::advanced::BundlesArgs),
    /// Vault status.
    Vault(crate::cmd::advanced::VaultArgs),
    /// Security audit.
    Security(crate::cmd::advanced::SecurityArgs),
    /// Connector management.
    Connectors(crate::cmd::advanced::ConnectorsArgs),
    /// Verify installation.
    Verify(crate::cmd::advanced::VerifyArgs),
    /// Interactive console.
    Console(crate::cmd::advanced::ConsoleArgs),

    // ── Restored fixed modules ──
    /// Authentication and OAuth management.
    Auth {
        /// Action: login, logout, status.
        action: Option<String>,
        /// Provider name.
        provider: Option<String>,
    },
    /// Batch JSONL processing.
    Batch {
        /// Input JSONL file.
        input: String,
        /// Output file (optional).
        #[arg(short, long)]
        out: Option<String>,
    },
    /// Cloud agent management.
    Cloud {
        /// Action to perform.
        action: Option<String>,
    },
    /// Configuration management.
    Config {
        /// Action: get, set, list, reset.
        action: Option<String>,
        /// Config key.
        key: Option<String>,
        /// Config value (for set action).
        value: Option<String>,
    },
    /// Cron job management.
    Cron {
        /// Action: list, add, remove.
        action: Option<String>,
    },
    /// Run evaluation suites.
    #[command(subcommand)]
    Eval(crate::cli::EvalCmd),
    /// Fallback provider chain management.
    Fallback {
        /// Action: list, add, remove, clear.
        action: Option<String>,
    },
    /// Gateway management.
    Gateway {
        /// Action to perform.
        action: Option<String>,
    },
    /// Interactive session (explicit).
    Interactive,
    /// Miscellaneous utilities.
    Misc {
        /// Action to perform.
        action: Option<String>,
    },
    /// Mixture-of-Agents configuration.
    Moa {
        /// Action: list, configure, delete.
        action: Option<String>,
    },
    /// HTTP server mode.
    Serve {
        /// Action to perform.
        action: Option<String>,
    },
    /// Skin/theme management.
    Skin {
        /// Action: list, use, set.
        action: Option<String>,
    },
    /// Tool configuration.
    Tools {
        /// Action: list, enable, disable.
        action: Option<String>,
    },
    /// Launch the TUI.
    Tui,
    /// Run a one-shot prompt.
    Run {
        /// The prompt to run.
        prompt: String,
    },
}

// ---------------------------------------------------------------------------
// Voice subcommands (ported from hermes-rs)
// ---------------------------------------------------------------------------

/// Voice utilities: text-to-speech and speech-to-text.
#[derive(Debug, Clone, Subcommand)]
pub enum VoiceCmd {
    /// Synthesize speech from text (writes an audio file).
    Say {
        /// Text to synthesize.
        text: String,
        /// Output path (default: speech-<unix_ts>.mp3).
        #[arg(short, long)]
        out: Option<String>,
        /// Voice name (provider-specific).
        #[arg(long)]
        voice: Option<String>,
    },
    /// Transcribe an audio file to text.
    Listen {
        /// Audio file path.
        file: String,
        /// Language hint (e.g. "en", "id").
        #[arg(short, long)]
        language: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Eval subcommands (ported from hermes-rs)
// ---------------------------------------------------------------------------

/// Evaluation suites.
#[derive(Debug, Clone, Subcommand)]
pub enum EvalCmd {
    /// List available suites without running them.
    List,
    /// Run a suite (path to JSONL file or `builtin:tools`).
    Run {
        /// Suite: JSONL file path or `builtin:tools`.
        suite: String,
        /// Write JSON report to this path.
        #[arg(short, long)]
        out: Option<String>,
        /// Pass threshold percent 0–100 (overrides config).
        #[arg(long)]
        threshold: Option<f64>,
    },
}

// ---------------------------------------------------------------------------
// Cli
// ---------------------------------------------------------------------------

/// Hermes Agent CLI.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "hermes",
    version,
    about = "Hermes Agent — autonomous AI agent with tool use",
    long_about = "Hermes Agent is an autonomous AI agent that can use tools, execute code, \
                  and interact with various platforms. Start an interactive session with `hermes` \
                  or use subcommands for specific tasks."
)]
pub struct Cli {
    /// The subcommand to execute. Defaults to starting an interactive session.
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Enable verbose / debug logging.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Override the configuration directory path.
    #[arg(short = 'C', long, global = true)]
    pub config_dir: Option<String>,

    /// Override the default model (e.g. "openai:gpt-4o").
    #[arg(short = 'm', long, global = true)]
    pub model: Option<String>,

    /// Override the personality / persona.
    #[arg(short = 'p', long, global = true)]
    pub personality: Option<String>,
}

impl Cli {
    /// Return the effective command, defaulting to `CliCommand::Hermes`.
    pub fn effective_command(&self) -> CliCommand {
        self.command.clone().unwrap_or(CliCommand::Hermes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn cli_parse_default() {
        let cli = Cli::try_parse_from(vec!["hermes"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.verbose);
    }

    #[test]
    fn cli_parse_model() {
        let cli = Cli::try_parse_from(vec!["hermes", "model", "openai:gpt-4o"]).unwrap();
        match cli.command {
            Some(CliCommand::Model { provider_model, .. }) => {
                assert_eq!(provider_model.as_deref(), Some("openai:gpt-4o"));
            }
            _ => panic!("Expected Model command"),
        }
    }

    #[test]
    fn cli_parse_providers_list() {
        let cli = Cli::try_parse_from(vec!["hermes", "providers", "list"]).unwrap();
        match cli.command {
            Some(CliCommand::Providers { action }) => {
                assert_eq!(action.as_deref(), Some("list"));
            }
            _ => panic!("Expected Providers command"),
        }
    }

    #[test]
    fn cli_parse_providers_default_action() {
        let cli = Cli::try_parse_from(vec!["hermes", "providers"]).unwrap();
        match cli.command {
            Some(CliCommand::Providers { action }) => {
                assert!(action.is_none());
            }
            _ => panic!("Expected Providers command"),
        }
    }

    #[test]
    fn cli_parse_logs_list_and_name() {
        let cli = Cli::try_parse_from(vec!["hermes", "logs", "--lines", "50"]).unwrap();
        match cli.command {
            Some(CliCommand::Logs(args)) => {
                assert_eq!(args.lines, 50);
            }
            _ => panic!("Expected Logs command"),
        }
    }

    #[test]
    fn cli_parse_memory_recall_target() {
        let cli = Cli::try_parse_from(vec![
            "hermes",
            "memory",
            "recall",
            "dark mode",
            "--target",
            "user",
        ])
        .unwrap();
        match cli.command {
            Some(CliCommand::Memory {
                action,
                arg,
                target,
            }) => {
                assert_eq!(action.as_deref(), Some("recall"));
                assert_eq!(arg.as_deref(), Some("dark mode"));
                assert_eq!(target, "user");
            }
            _ => panic!("Expected Memory command"),
        }
    }

    #[test]
    fn cli_parse_sessions_show() {
        let cli = Cli::try_parse_from(vec!["hermes", "sessions", "show", "--id", "abc"]).unwrap();
        match cli.command {
            Some(CliCommand::Sessions { action, id, .. }) => {
                assert_eq!(action.as_deref(), Some("show"));
                assert_eq!(id.as_deref(), Some("abc"));
            }
            _ => panic!("Expected Sessions command"),
        }
    }

    #[test]
    fn cli_parse_memory_with_provider() {
        let cli = Cli::try_parse_from(vec!["hermes", "memory", "setup", "redis"]).unwrap();
        match cli.command {
            Some(CliCommand::Memory { action, arg, .. }) => {
                assert_eq!(action.as_deref(), Some("setup"));
                assert_eq!(arg.as_deref(), Some("redis"));
            }
            _ => panic!("Expected Memory command"),
        }
    }

    #[test]
    fn cli_parse_sessions_export() {
        let cli = Cli::try_parse_from(vec!["hermes", "sessions", "export", "--id", "abc"]).unwrap();
        match cli.command {
            Some(CliCommand::Sessions { action, id, .. }) => {
                assert_eq!(action.as_deref(), Some("export"));
                assert_eq!(id.as_deref(), Some("abc"));
            }
            _ => panic!("Expected Sessions command"),
        }
    }

    #[test]
    fn removed_commands_are_rejected() {
        assert!(Cli::try_parse_from(vec!["hermes", "login"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "logout"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "clipboard"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "whatsapp"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "lumio"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "region"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "subscription"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "codex-models"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "runtime-provider"]).is_err());
        assert!(Cli::try_parse_from(vec!["hermes", "memory-setup"]).is_err());
    }

    #[test]
    fn ported_commands_parse() {
        // Commands ported from hermes-rs must parse.
        assert!(Cli::try_parse_from(vec!["hermes", "voice", "say", "hello"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "voice", "listen", "a.mp3"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "batch", "in.jsonl"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "eval", "list"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "eval", "run", "builtin:tools"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "webhook"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "webhook", "--port", "9000"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "run", "hello"]).is_ok());
        assert!(Cli::try_parse_from(vec!["hermes", "tui"]).is_ok());
    }
}
