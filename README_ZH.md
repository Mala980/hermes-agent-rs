<div align="center">

# ⚡ Hermes Agent

**自我进化的 AI Agent。一个二进制文件，全平台运行。**

[Nous Research](https://nousresearch.com) 出品的 [Hermes Agent](https://github.com/NousResearch/hermes-agent) Rust 重写版。

`110,000+ 行 Rust 代码` · `2,150+ 个测试` · `18 个 crate` · `~16MB 二进制`

**[English](./README.md)** · **[中文](./README_ZH.md)** · **[日本語](./README_JA.md)** · **[한국어](./README_KO.md)**

</div>

> **v0.2：** 新增原生 Gemini（`GEMINI_API_KEY`）、Azure OpenAI（`AZURE_OPENAI_API_KEY`）与 AWS Bedrock（Converse + SigV4）提供商——文档中记录的 C1 Bedrock 兼容性缺口现已补齐，共 13 个提供商。
> **v0.2（批次 1–6，2026 年 10 月已移植）：**（1）安全加固——执行密钥黑名单、输出脱敏、WhatsApp/webhook HMAC-SHA256 默认关闭式验证、SSRF 防护、各平台白名单、`--readonly`/`--yes`；（2）网关可靠性——Discord 重连/去重、Matrix since-token 持久化、Signal 群组门控、WhatsApp 去重、429/`retry_after` 处理、围栏感知消息切分、Telegram 真实轮询循环；（3）Agent 主循环——工具调用屏障、先持久化再执行、严格校验、压缩接线、预算宽限、不可信数据分隔符；（4）提供商——请求形态、Gemini 多轮对话、Bedrock 真实流式 + IAM 凭证链、Anthropic OAuth/bearer + 用量统计、Azure Foundry、SSE 加固；（5）工具——18 工具看板家族、加密 `browser_vault`（HMAC-SHA256-CTR）、exec/edit/read/write 语义、`x_search` 切换至 xAI API；（6）CLI/ACP/TUI/cron——CLI 分级命令、12 个真实 ACP 方法、TUI 会话切换器（F3）/智能体面板（F4）/目标栏、统一 cron（多格式、时区、重试阶梯、tick 锁）。
>
> **配置：** YAML 键为 `llm_providers`（如 `llm_providers.openai.api_key`），环境变量包括 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`HERMES_LLM_API_KEY`、`<PROVIDER>_BASE_URL`、`HERMES_MODEL`。
>
> **Telegram 白名单（默认关闭，fail-closed）：** `TELEGRAM_ALLOWED_USERS`（逗号分隔 ID，`*`=所有人，未设置=全部拒绝）∪ 配置文件 telegram 平台的 `allowed_users`；`GATEWAY_ALLOWED_USERS` 为跨平台兜底。

---

## 为什么选择 Hermes？

🚀 **零依赖** — 单个静态二进制文件。不需要 Python、pip、Docker。复制到树莓派、$3/月 VPS 或断网服务器，直接运行。

🧠 **自进化引擎** — 多臂老虎机模型选择、长任务规划、Prompt 与记忆自动塑形。用得越多，Agent 越聪明。

🔌 **17 个平台 · 90+ 工具 · 8 个记忆后端** — Telegram、Discord、Slack、微信、钉钉、飞书、企业微信等 17 个平台。文件操作、浏览器、代码执行、视觉、语音、网页搜索、Home Assistant、看板任务等 90+ 工具。

⚡ **真并发** — Rust tokio 运行时将工具调用分发到 OS 线程。30 秒的浏览器抓取不会阻塞 50ms 的文件读取。没有 GIL。

## 快速开始

```bash
# 安装
curl -fsSL https://raw.githubusercontent.com/Lumio-Research/hermes-agent-rs/main/scripts/install.sh | bash

# 设置 API 密钥
echo "ANTHROPIC_API_KEY=sk-..." >> ~/.hermes/.env

# 运行
hermes
```

就这样。你已经进入了一个带工具、记忆和流式输出的交互式会话。

📖 完整配置指南：[docs/CONFIGURATION.md](docs/CONFIGURATION.md)

## 能做什么？

**与任意 LLM 对话** — 对话中随时切换模型：
```
hermes
> /model gpt-4o
> 分析这个仓库，找出安全问题
```

**命令行一次性任务**：
```bash
hermes chat --query "把 auth.rs 重构为新的错误类型"
```

**多平台网关** — 同时连接 Telegram、Discord、Slack、微信等：
```bash
hermes gateway start
```

**随处运行** — Docker、SSH 或远程沙箱：
```yaml
# ~/.hermes/config.yaml
terminal:
  backend: docker
  image: ubuntu:24.04
```

**MCP + ACP** — 连接外部工具服务器，或将 Hermes 暴露为工具服务：
```yaml
mcp:
  servers:
    - name: my-tools
      command: npx my-mcp-server
```

**语音模式** — VAD + STT + TTS 流水线，解放双手。

## 架构

```
hermes-cli                    # 二进制入口、TUI、斜杠命令
├── hermes-agent              # Agent 循环、LLM 提供商、记忆插件
│   ├── hermes-core           # 共享类型、trait、错误层级
│   ├── hermes-intelligence   # 模型路由、Prompt 构建、自进化
│   └── hermes-config         # 配置加载、YAML/环境变量合并
├── hermes-tools              # 90+ 工具后端、审批引擎
├── hermes-gateway            # 17 个平台适配器、会话管理
├── hermes-environments       # 终端：Local/Docker/SSH/Daytona/Modal/Singularity
├── hermes-mcp                # Model Context Protocol 客户端/服务端
├── hermes-acp                # Agent Communication Protocol
├── hermes-skills             # 技能管理与 Hub
├── hermes-cron               # 定时任务调度
├── hermes-server             # HTTP/WebSocket API 服务
├── hermes-eval               # SWE-bench、Terminal-Bench、YC Bench
└── hermes-telemetry          # OpenTelemetry + Prometheus
```

**核心 trait：** `LlmProvider`（13 个提供商）· `ToolHandler`（90+ 后端）· `PlatformAdapter`（17 个平台）· `TerminalBackend`（6 个后端）· `MemoryProvider`（8 个插件）

**工具调用解析器：** Hermes、Anthropic、OpenAI、Qwen、Llama、DeepSeek、Auto

## 安装

**一键安装**（自动识别系统与架构）：
```bash
curl -fsSL https://raw.githubusercontent.com/Lumio-Research/hermes-agent-rs/main/scripts/install.sh | bash
```

**从源码安装：**
```bash
cargo install --git https://github.com/Lumio-Research/hermes-agent-rs hermes-cli --locked
```

**手动下载：** 从 [Releases](https://github.com/Lumio-Research/hermes-agent-rs/releases) 下载对应平台的二进制文件。

**Docker：**
```bash
docker run --rm -it -v ~/.hermes:/root/.hermes ghcr.io/lumio-research/hermes-agent-rs
```

## 贡献

欢迎贡献。提交前请运行测试：

```bash
cargo test --workspace        # 2,150+ 个测试
cargo clippy --workspace      # 代码检查
cargo fmt --all --check       # 格式检查
```

架构细节和编码规范见 [AGENTS.md](AGENTS.md)。

## 许可证

MIT — 见 [LICENSE](LICENSE)。

基于 [Nous Research](https://nousresearch.com) 的 [Hermes Agent](https://github.com/NousResearch/hermes-agent)。
