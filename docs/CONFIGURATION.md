# Hermes Agent (Rust) — Configuration Guide

This guide documents how to install, configure, and run the Hermes Agent Rust port
(`hermes-agent-rs`, Lumio fork). Everything below is grounded in the actual code —
config keys, environment variables, and gateway behavior are taken from
`hermes-config` (loader/paths/platform), `hermes-cli` (setup and gateway wizards),
`hermes-gateway` (allowlists, Telegram adapter), and `hermes-agent`
(model-string resolution).

> **Note on `hermes setup`**: the setup wizard documented in §2 is the *new*
> behavior. The legacy wizard (OpenAI key only, model presets 1–4) still exists in
> the codebase while the new one rolls out; the new wizard replaces it.

---

## 1. Installation

Prerequisites: a working Rust toolchain (stable) and `cargo`.

```bash
git clone https://github.com/Mala980/hermes-agent-rs.git
cd hermes-agent-rs
git checkout port-native-providers

# Build (debug)
cargo build

# Build (release, recommended for daily use)
cargo build --release

# Binary locations
./target/debug/hermes
./target/release/hermes
```

You can copy or symlink the binary into your `PATH`:

```bash
cp target/release/hermes ~/.local/bin/hermes   # or /usr/local/bin
```

Verify the build and environment:

```bash
hermes --version
hermes doctor     # checks config dir, config.yaml, API keys, external tools
```

---

## 2. `hermes setup` — the setup wizard

`hermes setup` is the interactive first-run wizard. It:

1. Creates the config directory (`~/.hermes`, or `$HERMES_HOME` when set) with
   `profiles/`, `sessions/`, `logs/`, `skills/` subdirectories.
2. Walks you through provider, credentials, model, and personality prompts
   (see below).
3. Writes `config.yaml` into the config directory (asks before overwriting an
   existing one) and a `profiles/default.yaml` when none exists.
4. Prints the next steps (`hermes` to start, `hermes doctor` to check health).

### Prompt sequence

**a) LLM provider.** The wizard asks which LLM provider you use — `openai` or
`anthropic`. The default is `openai` (press Enter to accept).

**b) API key.** You are prompted for that provider's API key. Leaving it blank
skips it — useful when you plan to supply the key via an environment variable
(see §3) instead of writing it to disk.

**c) Custom `base_url` (optional).** You are prompted for an optional custom API
base URL. Blank means "use the provider's default" (e.g. `https://api.openai.com/v1`
for OpenAI). This is how you point a provider at an OpenAI-compatible proxy or
gateway such as AtmoRouter:

```
base_url: https://atmorouter.dev/v1
```

**d) Model choice.** The wizard offers four presets plus a custom option:

| # | Value | Notes |
|---|-------|-------|
| 1 | `openai:gpt-4o` | default |
| 2 | `openai:gpt-4o-mini` | fast & cheap |
| 3 | `anthropic:claude-3-5-sonnet` | |
| 4 | `openrouter:auto` | multi-provider routing |
| 5 | `custom` | free-form input |

The `custom` option accepts any free-form model string in `provider:model`
format. It is validated to contain **exactly one `:` with non-empty parts on
both sides** — for example `atmo:deepseek-v4.1-flash` is accepted, while
`deepseek-v4.1-flash` (no provider) or `foo:` (empty model) are rejected and
re-prompted (up to 3 attempts, then the wizard falls back to `openai:gpt-4o`).

**e) Personality.** Finally the wizard asks for a personality prompt
(e.g. `concise`, `technical`, `creative`).

### Where credentials land

Credentials (`api_key` and `base_url`) are written under
`llm_providers.<provider>` — where `<provider>` is the **provider prefix of the
final model string**, i.e. the part before the first `:`. This is exactly how the
runtime resolves credentials for a request: it splits the model string
(`agent_builder.rs::parse_model_string`) and looks up
`llm_providers[<provider>]`. So if you pick `atmo:deepseek-v4.1-flash`, the key
and base URL go under `llm_providers.atmo` — not under the provider you named at
step (a).

### Resulting `config.yaml`

A typical result of the wizard:

```yaml
# Hermes Agent Configuration
model: atmo:deepseek-v4.1-flash
personality: concise
max_turns: 30

llm_providers:
  atmo:
    api_key: YOUR_ATMO_API_KEY
    base_url: https://atmorouter.dev/v1
```

Or, with the defaults (OpenAI):

```yaml
model: openai:gpt-4o
personality: default
max_turns: 30

llm_providers:
  openai:
    api_key: sk-...
```

---

## 3. Providers

### The `llm_providers` map

Provider credentials live in `config.yaml` under the **`llm_providers`** key
(note: the YAML key is `llm_providers`, **not** `llm`):

```yaml
llm_providers:
  openai:
    api_key: sk-...
    base_url: https://api.openai.com/v1
    model: gpt-4o
  anthropic:
    api_key: sk-ant-...
  openrouter:
    api_key: sk-or-...
    base_url: https://openrouter.ai/api/v1
    provider_order: ["DeepInfra", "Together"]   # OpenRouter-specific
```

Per-provider fields available (`LlmProviderConfig` in `config.rs`):

| Field | Meaning |
|-------|---------|
| `api_key` | API key, **or an env-var reference like `${MY_API_KEY}`** |
| `base_url` | Endpoint override (OpenAI-compatible). Trailing slashes are trimmed by the runtime. |
| `model` | Default model slug for this provider |
| `max_tokens` | Output-token ceiling (clamped to the per-model ceiling) |
| `temperature` | Sampling temperature |
| `extra_body` | Extra JSON fields forwarded in the request body |
| `rate_limit` | Requests-per-minute |
| `credential_pool` | Pool of credential identifiers for rotation |
| `oauth_token_url` / `oauth_client_id` | OAuth2 refresh flows (e.g. openai-codex, qwen-oauth) |
| `command` / `args` | External-process provider (e.g. `copilot-language-server --stdio`) |
| `provider_order` | Preferred upstream order (OpenRouter) |

You can also manage these without editing YAML by hand:

```bash
hermes config set llm.openai.api_key sk-...
hermes config set llm.openai.base_url https://api.openai.com/v1
hermes config get llm.openai.api_key      # shown masked, e.g. ***abcd
hermes config unset llm.openai.api_key
```

(`hermes config get` masks secrets: full keys are never printed.)

### Environment variables

Variables are applied **after** the config files, so env always wins over
`config.yaml`. The mapping below is verified in
`hermes-config/src/loader.rs` (`apply_env_overrides`):

| Env var | Effect |
|---------|--------|
| `HERMES_MODEL` | Overrides top-level `model` |
| `HERMES_PERSONALITY` | Overrides `personality` |
| `HERMES_SYSTEM_PROMPT` | Overrides `system_prompt` |
| `HERMES_MAX_TURNS` | Overrides `max_turns` |
| `HERMES_HOME` | Overrides the config directory (`~/.hermes` default) |
| `HERMES_PROXY_HTTP` / `HERMES_PROXY_SOCKS` | `proxy.http_proxy` / `proxy.socks_proxy` |
| `HERMES_BUDGET_MAX_RESULT_CHARS` / `HERMES_BUDGET_MAX_AGGREGATE_CHARS` | Output-budget limits |
| `HERMES_LLM_API_KEY` | Sets `api_key` on **every** configured provider |
| `HERMES_BASE_URL` | Sets `base_url` on **every** configured provider |
| `OPENAI_API_KEY` | `llm_providers.openai.api_key` |
| `ANTHROPIC_API_KEY` | `llm_providers.anthropic.api_key` |
| `OPENROUTER_API_KEY` | `llm_providers.openrouter.api_key` |
| `OPENROUTER_BASE_URL` | `llm_providers.openrouter.base_url` |
| `DASHSCOPE_API_KEY` | `llm_providers.qwen.api_key` |
| `MOONSHOT_API_KEY` | `llm_providers.kimi.api_key` |
| `MINIMAX_API_KEY` | `llm_providers.minimax.api_key` |
| `NOUS_API_KEY` | `llm_providers.nous.api_key` |
| `GITHUB_COPILOT_TOKEN` | `llm_providers.copilot.api_key` |

Additionally, the agent runtime (`hermes-agent/src/agent_builder.rs`) honors a
per-provider base-URL override of the form **`<NAME>_BASE_URL`** — the provider
name upper-cased with non-alphanumeric characters turned into `_` separators,
e.g. `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, `OPENAI_CODEX_BASE_URL`. An
explicit `base_url` in `llm_providers.<name>` wins over this env var; both win
over the provider's shipped default.

### `.env` support

`~/.hermes/.env` (or `$HERMES_HOME/.env`) is loaded at startup by
`load_dotenv()` into the process environment. Real environment variables always
win over `.env` values. Format:

```
# comments and blank lines are fine
OPENAI_API_KEY=sk-...
TELEGRAM_ALLOWED_USERS=123456789
```

Values may be single- or double-quoted.

### Config file precedence

The loader merges sources in this order (later wins):

1. built-in defaults
2. `$HERMES_HOME/gateway.json` (lowest-priority file source)
3. `$HERMES_HOME/config.yaml`
4. `$HERMES_HOME/cli-config.yaml` (high-priority overlay)
5. `.env` file, then real environment variables (highest priority)

So: `env vars > .env > cli-config.yaml > config.yaml > gateway.json > defaults`.

`validate_config` rejects configs with `max_turns: 0`, `terminal.timeout: 0`,
or any **empty** (but present) provider `api_key` — a present-but-empty key is an
error, not the same as unset.

---

## 4. Model selection

Models are named in **`provider:model`** format everywhere: in `config.yaml`,
in profiles, and in the ACP config.

The provider is resolved from the model string with `split_once(':')`
(`agent_builder.rs::parse_model_string`):

- `"openai:gpt-4o"` → provider `openai`, model `gpt-4o`
- `"atmo:deepseek-v4.1-flash"` → provider `atmo`, model `deepseek-v4.1-flash`
- `"gpt-4o"` (no colon) → provider defaults to **`openai`**

The provider's credentials are then looked up in `llm_providers.<provider>`
(exactly why the setup wizard writes credentials under the provider prefix of
the chosen model — §2). If the runtime finds no entry for a custom provider
name, check that `llm_providers.<name>` exists in `config.yaml`.

Tip: OpenRouter-style `provider/model` forms are also handled — `model_base_name`
strips both `provider:` and `provider/` prefixes when applying per-model rules
(e.g. which models force `max_completion_tokens` instead of `max_tokens`).

### Smart model routing (optional)

Short/simple turns can be routed to a cheaper model automatically:

```yaml
smart_model_routing:
  enabled: true
  max_simple_chars: 160
  max_simple_words: 28
  cheap_model:
    provider: openai
    model: gpt-4o-mini
```

Disabled by default. CLI equivalents: `hermes config set
smart_model_routing.enabled true`, `smart_model_routing.cheap_model.model`, etc.

---

## 5. Telegram gateway

### Getting a token

Talk to **[@BotFather](https://t.me/BotFather)** on Telegram, send `/newbot`,
follow the prompts, and copy the bot token it returns. You will need it in the
wizard below.

### Configure with the gateway wizard

```bash
hermes gateway setup
```

The wizard lists platform status, then asks which platforms to configure
(comma-separated; empty defaults to `weixin,telegram`). For `telegram` it:

1. Runs the auth flow to capture the bot token.
2. Enables the platform.
3. Asks **"Telegram use polling mode?"** (default yes) and stores `polling` in
   the telegram platform section.
   - Polling = the bot long-polls `getUpdates` — simplest, works anywhere with
     outbound HTTPS. Long-poll timeout is `poll_timeout` seconds (default 30).
   - Webhook = you supply a `webhook_url`; Telegram pushes updates to your
     server instead. Requires a publicly reachable HTTPS endpoint.
4. Optionally records a home channel.

Result in `config.yaml`:

```yaml
platforms:
  telegram:
    enabled: true
    token: 123456:ABC-DEF...
    # polling: true          # default
    # webhook_url: https://my.host/telegram  # webhook mode only
    # bot_username: MyBot   # mention filtering in groups
    # parse_markdown: false
    # parse_html: false
```

Start the gateway afterwards with `hermes gateway start`.

### `TelegramConfig` reference

(From `hermes-gateway/src/platforms/telegram.rs`)

| Key | Default | Meaning |
|-----|---------|---------|
| `token` | — (required) | Bot token from @BotFather |
| `polling` | `true` | `true` = long-poll `getUpdates`; `false` = webhook mode |
| `webhook_url` | unset | Public HTTPS URL Telegram pushes updates to (webhook mode) |
| `poll_timeout` | 30 | Long-poll timeout, seconds |
| `bot_username` | unset | Bot username (without `@`), for mention filtering in groups |
| `parse_markdown` / `parse_html` | `false` | Message formatting mode |
| `allowed_users` | unset | Config-file allowlist of numeric Telegram user IDs (strings); see below |

### Allowlist (who may talk to the bot)

Telegram inbound messages pass a **fail-closed** allowlist gate
(`hermes-gateway/src/allowlist.rs`, plus the adapter-level union in
`telegram.rs`). A user is admitted when **any** of these match:

1. Config-file list — `allowed_users` on the telegram platform config
   (numeric user IDs as strings, e.g. `["123456789"]`).
2. `TELEGRAM_ALLOWED_USERS` — comma-separated IDs.
3. `GATEWAY_ALLOWED_USERS` — comma-separated IDs; the cross-platform fallback
   applied to every gated platform.

`"*" ` in any of the lists admits everyone.

**Fail-closed semantics:** if none of the sources carries entries, **no user is
allowed** — inbound updates are dropped with a warn log. An empty or missing
allowlist never means "open".

**Dev switches** (verified in `allowlist.rs`; intended for local development
only — the code logs a warning when the gate is bypassed this way):

- `TELEGRAM_ALLOW_ALL_USERS=true|1|yes` — opens Telegram.
- `GATEWAY_ALLOW_ALL_USERS=true|1|yes` — opens every gated platform.

To find your numeric Telegram user ID, message
[@userinfobot](https://t.me/userinfobot) on Telegram.

---

## 6. Other gateways

The generic allowlist module (`hermes-gateway/src/allowlist.rs`) gates five
platforms with the same `<PLATFORM>_ALLOWED_USERS ∪ GATEWAY_ALLOWED_USERS`
shape. The per-platform env vars:

| Platform | Allowlist env | Allow-all dev switch |
|----------|---------------|----------------------|
| discord | `DISCORD_ALLOWED_USERS` | `DISCORD_ALLOW_ALL_USERS` |
| slack | `SLACK_ALLOWED_USERS` | `SLACK_ALLOW_ALL_USERS` |
| matrix | `MATRIX_ALLOWED_USERS` | `MATRIX_ALLOW_ALL_USERS` |
| email | `EMAIL_ALLOWED_USERS` | `EMAIL_ALLOW_ALL_USERS` |
| telegram | `TELEGRAM_ALLOWED_USERS` | `TELEGRAM_ALLOW_ALL_USERS` |

(`GATEWAY_ALLOWED_USERS` / `GATEWAY_ALLOW_ALL_USERS` apply to all of them.)

**Signal** is gated differently — by group, not by user
(`signal_group_allowlist` in the same module):

- `SIGNAL_GROUP_ALLOWED_USERS` — comma-separated **group IDs** admitted
  (fail-closed: empty/missing = no group message passes).
- `SIGNAL_GROUP_ALLOW_ALL_USERS=true|1|yes` — opens all groups; the global
  `GATEWAY_ALLOW_ALL_USERS` also opens them.
- `GATEWAY_ALLOWED_USERS` is unioned in as group IDs.
- **Signal DMs are never gated** — this is intentional.

**WhatsApp** (and all other platform names) are **not** user-gated by this
module (`platform_env_prefix("whatsapp")` returns `None`; legacy behavior is
preserved and inbound messages pass). If you need gating there, it must come
from another layer.

Platforms are enabled in `config.yaml` under `platforms:` with a bot token —
`hermes gateway setup` walks you through each platform's token/URL prompts and
saves them; start everything with `hermes gateway start`. Each platform block
also supports `require_mention`, `home_channel`, `admin_users`, and DM handling
via `unauthorized_dm_behavior` (`PlatformConfig` in `hermes-config`).

---

## 7. Cron, TUI, ACP, skills, memory

### Cron (`hermes cron`)

Manages recurring agent jobs persisted in the config home.

```bash
hermes cron create --schedule "0 9 * * *" --prompt "Summarize today's calendar"
hermes cron create --schedule "every 15m" --prompt "Check inbox" --overlap allow
hermes cron list
hermes cron history --id <job-id>
hermes cron pause --id <job-id> / resume / delete / run
```

- **Schedules**: 5/6-field cron (`"0 9 * * *"`), natural-language-ish
  (`"every 15m"`, `"daily"`), or absolute (`"at 2030-01-01T09:00:00+08:00"`).
- **`--timezone`** accepts an IANA name (e.g. `Asia/Makassar`); the default is
  the system-local timezone.
- **`--overlap skip|allow`** (default `skip`): what to do when a tick arrives
  while the previous run is still in flight. Ticks are also guarded by a
  cross-process tick lock, and failed runs follow a retry ladder.

### TUI key bindings

The interactive TUI (`hermes` with no subcommand) — press **F1** (or `?` with an
empty input) for the in-app help overlay. From `tui.rs::help_lines`:

| Key | Action |
|-----|--------|
| `Enter` | send message (`Shift+Enter`: newline) |
| `Esc` | clear input / close popup / exit |
| `Ctrl+C` | interrupt the running turn / exit |
| `F1` / `?` | this help overlay (`?` works when input is empty) |
| `F2` | toggle dark/light theme (persisted to `tui.theme` in `config.yaml`) |
| `F3` | switch session (overlay; `Enter` selects, `Esc`/`F3` closes) |
| `F4` | background-process panel (toggle) |
| `Up` / `Down` | input history / navigate session list |
| `Tab` | accept completion |

Slash commands (also shown in F1): `/goal [text]` (set/show/clear the session
goal shown in the goal bar), `/save [name]`, `/load <name>`, plus `/new`,
`/reset`, `/retry`, `/model`, `/personality`, `/skills`, `/tools`, `/compress`,
`/usage`, `/yolo`, and more (22 total; see `SLASH_COMMANDS` in the CLI).

### ACP (`hermes acp start`)

The Agent Communication Protocol server runs over stdio (JSON-RPC framing):

```bash
hermes acp start     # serve ACP on stdin/stdout (model/max_turns from config)
hermes acp status    # "not running" unless launched by an ACP client
```

### Skills (`hermes skills`)

```bash
hermes skills list
hermes skills search <query> | browse
hermes skills install <name> | uninstall <name> | update <name>
hermes skills inspect <name> | check <name> | audit
```

Enable/disable lists are stored in config: `skills.enabled` / `skills.disabled`.

### Memory (`hermes memory`)

Built-in memory lives in `~/.hermes/memories/` (`MEMORY.md` + `USER.md`); one
external provider plugin can be attached on top.

```bash
hermes memory status          # show memory status
hermes memory setup [redis]   # configure a memory provider
hermes memory off             # disable memory provider
hermes memory remember "prefers dark mode"        # save a note
hermes memory recall "dark mode"                  # search notes
hermes memory forget "dark mode"                  # delete matching notes
hermes memory remember "loves spicy food" --target user   # user-profile store
```

`--target` is `memory` (agent notes, default), `user` (user profile), or `all`
(recall/forget only). Store budgets: `memory` ≈ 2200 chars, `user` ≈ 1375 chars.

### Other useful commands

```bash
hermes doctor                 # config/env/tool health check
hermes status [--full|--deep] # model, platforms, sessions; --deep checks provider credentials
hermes logs [--list] [--follow]   # tail log files
hermes sessions list|show|export|stats   # session management
hermes profile list|create|switch <name> # named profiles
```

---

## 8. Troubleshooting

### Gateway drops every Telegram message ("deny-all" behavior)

This is the fail-closed allowlist working as designed — **not a bug**. If
`TELEGRAM_ALLOWED_USERS`, `GATEWAY_ALLOWED_USERS`, and the config-file
`allowed_users` are all empty/missing, **no user is admitted** and every update
is dropped with a warn log (`"dropping update from user not in env allowlist
nor config allowed_users (fail-closed)"`). Fix: set `TELEGRAM_ALLOWED_USERS` to
your numeric Telegram user ID (or `"*"`).

### `hermes doctor` shows API keys missing but they're in `config.yaml`

`hermes doctor` checks **environment variables only** for its API-key table
(`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `EXA_API_KEY`,
`FIRECRAWL_API_KEY`). Keys in `config.yaml` are fine at runtime — the check just
doesn't look there. Use `hermes status --deep` for a runtime view of
`llm_providers` credentials (masked).

### Webhook 403 `invalid webhook signature`

The inbound webhook platform verifies **HMAC-SHA256** of the raw request body
with the configured secret, compared in constant time against the
`X-Hub-Signature-256` header in `sha256=<hex>` form (legacy header name
`X-Signature` is also accepted; header names are case-insensitive). A 403 means
the signature didn't match — check that the sender signs with the *same*
secret, over the *exact raw bytes* of the body, and uses the `sha256=<hex>`
format. (The old behavior — accepting any non-empty signature — was removed as
a security fix; clients must use HMAC.)

Additionally the request carries a **5-minute replay window**: it must include
either an `X-Webhook-Timestamp` header (unix seconds) or a numeric `timestamp`
field in the JSON body, with `|now - ts| <= 300`s. Stale or far-future
timestamps are rejected (403) even with a valid signature — clock skew between
sender and receiver is the usual cause.

A **503 `webhook not configured`** means the secret itself is missing or empty —
also fail-closed.

### Webhook never fires / provider 403s

- **Telegram 403 on startup**: the bot token is invalid or revoked — regenerate
  via @BotFather and re-run `hermes gateway setup`.
- **Telegram 429**: the poll loop honors `retry_after` once (capped at 60s)
  and backs off. Sustained 429s usually mean another instance is polling with
  the same token.
- **Discord 403s**: check the bot token and that the privileged *message
  content intent* is enabled in the Discord developer portal if DMs/intents are
  used.

### `base_url` trailing slashes

Don't worry about them: the runtime trims trailing slashes from provider
`base_url` values (`agent_builder.rs::resolve_provider_base_url`). A value like
`https://atmorouter.dev/v1/` works identically to
`https://atmorouter.dev/v1`. (If a *specific* third-party proxy misbehaves,
prefer the no-trailing-slash form.)

### "llm_providers.<name>.api_key must not be empty"

Config validation fails when a provider key is **present but empty**. An empty
string is not the same as unset — remove the key entirely (or use
`hermes config unset llm.<name>.api_key`) if you want to rely on env vars.

### `config.yaml` ignored / old values sticking

Remember the precedence chain: **env vars > `.env` > `cli-config.yaml` >
`config.yaml` > `gateway.json` > defaults**. A stale value in
`cli-config.yaml` or an env var (including one set in `~/.hermes/.env`) silently
overrides `config.yaml`. Run `hermes config get <key>` to see the effective
value after merging.

### Rate limits (`429`) on provider calls

Provider responses with HTTP 429 surface as a rate-limit error carrying the
server's `retry_after_secs`; the agent loop backs off accordingly. If you hit
this persistently, lower concurrency, switch to a cheaper model via
`smart_model_routing`, or check the provider dashboard for quota.
