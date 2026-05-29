# 文档编号：01 | 仓库结构与 Crate 说明

> **文档描述**：本文详细介绍 Codex 仓库的顶层目录组织、`codex-rs` Cargo workspace 内全部 crate 的分组与职责，以及关键配置文件的作用，帮助读者快速定位代码、理解模块边界。

---

## 目录

1. [顶层目录树](#1-顶层目录树)
2. [codex-rs Cargo Workspace 结构](#2-codex-rs-cargo-workspace-结构)
3. [Crate 分组详解](#3-crate-分组详解)
   - 3.1 [入口层](#31-入口层)
   - 3.2 [核心业务层](#32-核心业务层)
   - 3.3 [扩展系统层（ext/）](#33-扩展系统层ext)
   - 3.4 [AI 通信层](#34-ai-通信层)
   - 3.5 [工具与 MCP 层](#35-工具与-mcp-层)
   - 3.6 [配置层](#36-配置层)
   - 3.7 [持久化层](#37-持久化层)
   - 3.8 [认证与账户层](#38-认证与账户层)
   - 3.9 [app-server 层](#39-app-server-层)
   - 3.10 [工具函数 utils/ 层](#310-工具函数-utils-层)
   - 3.11 [平台适配层](#311-平台适配层)
   - 3.12 [外部 Agent 与云服务层](#312-外部-agent-与云服务层)
   - 3.13 [可观测性与开发辅助层](#313-可观测性与开发辅助层)
4. [Crate 层次架构图](#4-crate-层次架构图)
5. [关键配置文件说明](#5-关键配置文件说明)
6. [各 crate 分组对应的深潜文档](#6-各-crate-分组对应的深潜文档)

---

## 1. 顶层目录树

```
codex/                              # 仓库根目录
├── codex-cli/                      # TypeScript/Node.js 客户端层（npm 包）
│   └── src/                        # TS 源码，封装 Rust 二进制调用
├── codex-rs/                       # Rust 实现主体（Cargo workspace）
│   ├── Cargo.toml                  # workspace manifest，所有依赖版本集中管理
│   ├── Cargo.lock                  # 锁定全部依赖版本（提交到 git）
│   ├── rust-toolchain.toml         # 固定 Rust 工具链版本（channel = 1.95.0）
│   ├── clippy.toml                 # Clippy lint 规则配置
│   ├── rustfmt.toml                # rustfmt 代码格式化配置
│   ├── .cargo/config.toml          # cargo 构建标志（Windows 栈大小等）
│   │
│   ├── cli/                        # 二进制入口 crate（codex 命令）
│   ├── tui/                        # 全屏 TUI 界面 crate
│   ├── exec/                       # 无头 exec 模式 crate
│   ├── exec-server/                # exec 内部服务器 crate
│   │
│   ├── core/                       # 核心业务逻辑 crate（Agent 主循环）
│   ├── core-api/                   # core 对外公共 API crate
│   ├── core-plugins/               # 核心内置插件实现 crate
│   ├── core-skills/                # 核心内置技能实现 crate
│   ├── protocol/                   # 共享数据类型与协议 crate
│   ├── sandboxing/                 # 跨平台沙箱策略 crate
│   │
│   ├── ext/                        # 扩展系统子树（NEW，可插拔工具/能力）
│   │   ├── extension-api/          #   扩展框架（注册表 + 能力契约）
│   │   ├── goal/                   #   /goal 长任务扩展（sketch，尚未接入 host）
│   │   ├── guardian/               #   Guardian 守护 Agent 扩展
│   │   ├── image-generation/       #   图像生成工具扩展（imagegen）
│   │   ├── memories/               #   记忆工具扩展（memories 工具族）
│   │   └── web-search/             #   网页搜索工具扩展
│   │
│   ├── backend-client/             # OpenAI Responses API HTTP 客户端 crate
│   ├── codex-api/                  # API 类型定义 crate
│   ├── codex-client/               # 底层 HTTP client 抽象 crate
│   ├── model-provider/             # 多模型后端路由 crate
│   ├── model-provider-info/        # 模型元信息 crate
│   ├── models-manager/             # 模型列表管理 crate
│   │
│   ├── codex-mcp/                  # MCP 连接管理 crate
│   ├── mcp-server/                 # MCP 服务器入口 crate
│   ├── rmcp-client/                # MCP 客户端（连接外部 MCP 服务器）crate
│   ├── tools/                      # 工具定义注册表 crate
│   ├── apply-patch/                # apply_patch 工具实现 crate
│   ├── execpolicy/                 # 命令执行策略引擎 crate
│   ├── file-system/                # 文件系统操作抽象 crate
│   ├── file-search/                # 模糊文件搜索 crate
│   ├── file-watcher/               # 文件/目录变更监听 crate（NEW）
│   │
│   ├── config/                     # TOML 配置加载与验证 crate
│   ├── features/                   # 功能开关（Feature flags）crate
│   ├── hooks/                      # 钩子系统 crate
│   ├── skills/                     # 技能（Skill）系统 crate
│   ├── plugin/                     # 插件系统核心 crate
│   │
│   ├── state/                      # SQLite 会话状态持久化 crate
│   ├── thread-store/               # 线程持久化接口抽象 crate
│   ├── rollout/                    # 会话 rollout（JSONL 转录 + 发现）crate
│   ├── rollout-trace/              # rollout trace bundle 模式与回放 crate
│   ├── message-history/            # 全局消息历史（history.jsonl）crate（NEW）
│   ├── memories/                   # 记忆存储 crate（read + write 两个子 crate）
│   │
│   ├── login/                      # 认证流程实现 crate
│   ├── keyring-store/              # 系统密钥链抽象 crate
│   ├── aws-auth/                   # AWS 签名认证 crate
│   ├── secrets/                    # 密钥加密存储 crate
│   ├── agent-identity/             # Agent 身份标识 crate
│   ├── chatgpt/                    # ChatGPT 账户 API crate
│   │
│   ├── app-server/                 # IDE 插件 JSON-RPC 服务 crate
│   ├── app-server-protocol/        # app-server 协议类型定义 crate
│   ├── app-server-client/          # app-server 客户端库 crate
│   ├── app-server-transport/       # app-server 传输层 crate（NEW）
│   ├── app-server-daemon/          # app-server 守护进程管理 crate（NEW）
│   │
│   ├── external-agent-migration/   # 外部 Agent 配置迁移 crate（NEW）
│   ├── external-agent-sessions/    # 外部 Agent 会话解析/导出 crate（NEW）
│   ├── cloud-tasks/                # 云端任务 crate
│   │
│   ├── sandboxing/                 # 跨平台沙箱入口 crate
│   ├── linux-sandbox/              # Linux Landlock+bwrap 实现 crate
│   ├── bwrap/                      # bubblewrap 包装二进制 crate（NEW）
│   ├── windows-sandbox-rs/         # Windows 受限令牌实现 crate
│   ├── process-hardening/          # 进程加固工具 crate
│   │
│   ├── utils/                      # 通用工具函数 crate 集合（22 个子 crate）
│   │   ├── absolute-path/   approval-presets/   cache/   cargo-bin/
│   │   ├── cli/   elapsed/   fuzzy-match/   home-dir/   image/
│   │   ├── json-to-toml/   oss/   output-truncation/   path-utils/
│   │   ├── plugins/   pty/   readiness/   rustls-provider/
│   │   └── sandbox-summary/   sleep-inhibitor/   stream-parser/   string/   template/
│   │
│   └── ...（otel / analytics / git-utils / ansi-escape 等辅助 crate 见 §3.13）
│
├── docs/                           # 用户文档（Markdown）
│   ├── authentication.md  config.md  sandbox.md  exec.md  ...
├── scripts/                        # 构建/发布辅助脚本（Shell/Python）
├── sdk/                            # 公开 SDK 相关内容
├── third_party/                    # 第三方依赖快照
├── justfile                        # just 命令任务定义（工作目录 codex-rs）
├── MODULE.bazel                    # Bazel 模块定义
├── MODULE.bazel.lock               # Bazel 依赖锁文件
├── CHANGELOG.md                    # 变更日志
└── README.md                       # 项目简介与快速入门
```

> **说明**：上面的目录树为「代表性」展示，按职责分组列出主要 crate，并非逐一穷举。
> 完整 crate 清单见下文 §3 各分组表格；权威来源是 `codex-rs/Cargo.toml` 的 `members` 列表。

---

## 2. codex-rs Cargo Workspace 结构

`codex-rs/Cargo.toml` 中声明了完整的 workspace members。全部 crate 按功能分层，共享统一的：

- **版本策略**：`version.workspace = true`（单一版本 `0.0.0`，由 CI/发布脚本注入）
- **Edition**：`edition = "2024"`（Rust 2024 Edition）
- **Lint 规则**：`lints.workspace = true`（统一 clippy deny 列表）
- **依赖版本**：`[workspace.dependencies]` 集中声明所有外部依赖与内部 path 依赖

> **规模**：workspace 的 `members` 数组列出约 **114** 个 crate；另有 `chatgpt`、
> `windows-sandbox-rs`、`message-history` 等几个 crate 仅以 `[workspace.dependencies]`
> 内的 `path = "..."` 形式被引用（位于 workspace 根目录下，仍属于同一 workspace）。
> 综合计算，整个 workspace 约含 **115+** 个 crate。

```
codex-rs Cargo Workspace（约 115+ crates）
│
├── [入口层]          cli, tui, exec, exec-server
├── [核心业务层]      core, core-api, core-plugins, core-skills, protocol, sandboxing
├── [扩展系统 ext/]   extension-api, goal, guardian, image-generation, memories, web-search
├── [AI 通信层]       backend-client, codex-api, codex-client, model-provider,
│                     model-provider-info, models-manager, codex-backend-openapi-models,
│                     lmstudio, ollama, realtime-webrtc, responses-api-proxy,
│                     response-debug-context, network-proxy
├── [工具与 MCP 层]   codex-mcp, mcp-server, rmcp-client, tools, apply-patch,
│                     execpolicy, execpolicy-legacy, shell-command, shell-escalation,
│                     arg0, file-system, file-search, file-watcher, code-mode
├── [配置层]          config, features, hooks, skills, connectors, plugin,
│                     collaboration-mode-templates
├── [持久化层]        state, thread-store, rollout, rollout-trace, message-history,
│                     memories/read, memories/write, agent-graph-store
├── [认证与账户层]    login, keyring-store, aws-auth, secrets, agent-identity, chatgpt
├── [app-server 层]   app-server, app-server-protocol, app-server-client,
│                     app-server-transport, app-server-daemon, app-server-test-client,
│                     debug-client, codex-experimental-api-macros, uds, stdio-to-uds
├── [外部 Agent/云]   external-agent-migration, external-agent-sessions,
│                     cloud-tasks, cloud-tasks-client, cloud-tasks-mock-client,
│                     cloud-requirements
├── [utils/ 层]       22 个小型通用工具 crate（见 §3.10）
├── [平台适配层]      sandboxing, linux-sandbox, bwrap, windows-sandbox-rs, process-hardening
└── [可观测性/辅助]   otel, analytics, git-utils, ansi-escape, async-utils,
                      terminal-detection, install-context, feedback,
                      test-binary-support, thread-manager-sample, v8-poc
```

---

## 3. Crate 分组详解

### 3.1 入口层

这些 crate 包含 `main.rs` 二进制入口或顶层编排逻辑，是用户与系统交互的起点。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `cli` | `codex-cli` | 唯一对外发布的二进制 `codex`，解析顶层子命令（tui/exec/app-server/mcp-server 等），分发到对应子系统 |
| `tui` | `codex-tui` | 基于 ratatui 的全屏终端 UI，实现聊天界面、diff 预览、审批交互、Markdown 渲染等 |
| `exec` | `codex-exec` | 无头（headless）exec 模式，读取命令行 prompt 驱动 Agent 循环，完成后退出 |
| `exec-server` | `codex-exec-server` | exec 内部服务器抽象层，提供接口供 TUI 和 app-server 调用，管理 PTY 进程生命周期 |

```
用户
 │
 ├──> codex (cli crate)
 │         │
 │         ├──> codex tui       ─── ratatui 全屏界面
 │         ├──> codex exec      ─── 无头脚本模式
 │         ├──> codex app-server─── IDE JSON-RPC 服务
 │         └──> codex mcp-server─── MCP 工具服务
 │
 └──> exec-server (内部，供 tui/app-server 复用)
```

### 3.2 核心业务层

这是整个系统的"大脑"，`codex-core` 实现了 Agent 的完整运行循环。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `core` | `codex-core` | 核心业务逻辑：Agent 循环、工具调度、上下文窗口管理、沙箱策略执行、系统提示词生成 |
| `core-api` | `codex-core-api` | `codex-core` 对外暴露的公共 API 类型与 trait，减少直接依赖 core 的耦合 |
| `core-plugins` | `codex-core-plugins` | 核心内置插件实现：把可插拔能力注册到 plugin/扩展系统 |
| `core-skills` | `codex-core-skills` | 内置技能（Skill）的具体实现集合 |
| `protocol` | `codex-protocol` | 跨 crate 共享的数据类型（Op、Event、ThreadId、SandboxPolicy 等），无业务逻辑 |
| `sandboxing` | `codex-sandboxing` | 跨平台沙箱策略抽象：将 `SandboxPolicy` 转化为平台命令前缀（详见 §3.11） |

> **重要**：`codex-core` 是最大的 crate，应尽量避免向其中添加新代码。新功能优先考虑新建独立 crate 或放入已有专用 crate（近年新增的 `ext/` 扩展子树正是这一思路的体现）。

### 3.3 扩展系统层（ext/）

`ext/` 子树是较新的 **扩展（Extension）架构**：在 `codex-extension-api` 框架之上，把可插拔的工具与能力实现成独立 crate，由 host 在线程生命周期中按需注册（`install`）。这避免了把每个新工具都塞进 `codex-core`。

| Crate 路径 | 包名 | 职责说明 |
|------------|------|----------|
| `ext/extension-api` | `codex-extension-api` | 扩展框架本身：定义注册表（`ExtensionRegistryBuilder`）、能力契约（`AgentSpawner`、`ExtensionEventSink`、`ResponseItemInjector`）、线程生命周期贡献者（`ThreadLifecycleContributor`）等 |
| `ext/goal` | `codex-goal-extension` | `/goal` 长任务模式的扩展草图：包含 goal 工具规格、扩展注册形态、运行时计量。注释明确标注「尚未接入 host」（intentionally not wired in yet） |
| `ext/guardian` | `codex-guardian` | Guardian 守护扩展：通过 `AgentSpawner` + 线程生命周期钩子，在会话期间派生「守护 Agent」做监督/校验 |
| `ext/image-generation` | `codex-image-generation-extension` | 图像生成工具扩展（工具名 `imagegen`，命名空间 `image_gen`） |
| `ext/memories` | `codex-memories-extension` | 记忆工具扩展：向模型暴露 `memories` 工具族（list/search/read，带 token 上限），是 Agent 主动读写长期记忆的入口 |
| `ext/web-search` | `codex-web-search-extension` | 网页搜索工具扩展（schema + 调用 + 历史记录） |

> **辨析**：`ext/memories`（codex-memories-extension）是 **面向模型的工具入口**；
> 而 §3.7 的 `memories/read`、`memories/write` 是 **底层存储读写实现**。两者协作完成记忆系统。

### 3.4 AI 通信层

负责与各类 LLM 后端通信，将 `codex-core` 的请求编码为 API 调用，解析响应流。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `backend-client` | `codex-backend-client` | OpenAI Responses API 的高层 HTTP 客户端，封装 SSE 流解析与重试逻辑 |
| `codex-api` | `codex-api` | OpenAI API 的 Rust 类型定义（request/response 结构体），与 API 规范一一对应 |
| `codex-client` | `codex-client` | 底层 HTTP 客户端抽象，统一 reqwest 配置（TLS、代理、超时、认证头） |
| `model-provider` | `codex-model-provider` | 多模型后端路由：根据配置选择 OpenAI / Ollama / LM Studio / AWS Bedrock 等 |
| `model-provider-info` | `codex-model-provider-info` | 模型元信息（上下文长度、定价、能力集等），用于 UI 展示和策略决策 |
| `models-manager` | `codex-models-manager` | 模型列表管理：从 API 获取可用模型列表、缓存与刷新 |
| `codex-backend-openapi-models` | `codex-backend-openapi-models` | 由 OpenAPI 规范自动生成的 Rust 类型定义 |
| `lmstudio` | `codex-lmstudio` | LM Studio 本地模型服务器的适配器 |
| `ollama` | `codex-ollama` | Ollama 本地模型服务器的适配器 |
| `realtime-webrtc` | `codex-realtime-webrtc` | 实验性 WebRTC 实时语音/视频通道支持 |
| `responses-api-proxy` | `codex-responses-api-proxy` | Responses API 代理，用于开发调试场景 |
| `response-debug-context` | `codex-response-debug-context` | Responses API 调试上下文记录（开发调试用） |
| `network-proxy` | `codex-network-proxy` | HTTP/SOCKS 代理配置读取与应用 |

```
model-provider (路由器)
    │
    ├──> OpenAI Responses API ←── backend-client + codex-api
    ├──> Ollama               ←── ollama crate
    ├──> LM Studio            ←── lmstudio crate
    └──> AWS Bedrock          ←── aws-auth crate
```

### 3.5 工具与 MCP 层

实现 AI 可调用的"工具"（tool calls），以及将这些工具暴露给外部系统的 MCP 协议支持。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `tools` | `codex-tools` | 工具定义注册表：声明 Agent 可调用工具的 schema 与 handler 入口，并提供 `ConversationHistory`、`ExtensionTurnItem` 等扩展契约类型 |
| `apply-patch` | `codex-apply-patch` | `apply_patch` 工具实现：解析并应用 Codex 专有补丁格式（parse → verify → apply，独立于 codex-core） |
| `execpolicy` | `codex-execpolicy` | 命令执行策略引擎：根据规则判断命令是否被允许执行 |
| `execpolicy-legacy` | `codex-execpolicy-legacy` | 兼容旧版 execpolicy 格式的解析器 |
| `codex-mcp` | `codex-mcp` | MCP 连接管理器：管理外部 MCP 服务器的生命周期与工具调用转发 |
| `mcp-server` | `codex-mcp-server` | MCP 服务器入口：将 Codex 作为 MCP 工具服务器对外暴露 |
| `rmcp-client` | `codex-rmcp-client` | 作为 MCP 客户端连接外部 MCP 服务器（基于 `rmcp` SDK），获取外部工具 |
| `shell-command` | `codex-shell-command` | Shell 命令构建与执行的底层封装 |
| `shell-escalation` | `codex-shell-escalation` | 权限提升（sudo/UAC）的 shell 命令辅助 |
| `arg0` | `codex-arg0` | 通过 `argv[0]` 多路复用：同一二进制根据进程名切换行为（如 `codex-linux-sandbox`）|
| `file-system` | `codex-file-system` | 文件系统操作抽象（读写文件、目录遍历等工具实现基础） |
| `file-search` | `codex-file-search` | 模糊文件搜索（结合 nucleo 排序），支持 TUI 文件选择器 |
| `file-watcher` | `codex-file-watcher` | 监听订阅的文件/目录，把粗粒度变更通知路由给持有匹配路径的订阅者 |
| `code-mode` | `codex-code-mode` | 代码模式配置：特定于编码任务的 prompt 调整与工具选择 |

> **注意**：网页搜索、图像生成、记忆等「工具」如今实现在 `ext/` 扩展子树（§3.3），
> 而非本层。本层聚焦 shell/补丁/文件/MCP 这类基础工具与执行策略。

### 3.6 配置层

处理用户配置、功能开关、钩子系统、技能注册等运行时可定制化内容。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `config` | `codex-config` | 加载并验证 `~/.codex/config.toml`，提供运行时配置结构体 |
| `features` | `codex-features` | 功能开关（Feature flags）的声明与读取，控制实验性功能的可见性 |
| `hooks` | `codex-hooks` | 钩子系统：在 Agent 工具调用前后触发用户自定义命令或函数 |
| `skills` | `codex-skills` | 技能（Skill）系统：可复用的 prompt 模板与工具预设组合，由 TOML 配置定义 |
| `connectors` | `codex-connectors` | 外部服务连接器配置（GitHub、Jira 等集成的配置结构） |
| `plugin` | `codex-plugin` | 插件系统核心：插件的发现、加载、生命周期管理 |
| `collaboration-mode-templates` | `codex-collaboration-mode-templates` | 协作模式的提示词模板 |

### 3.7 持久化层

负责将对话历史、会话状态、用户记忆等数据持久化到本地存储。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `state` | `codex-state` | 基于 SQLite + sqlx 的状态存储，持久化 Agent 事件日志和会话记录 |
| `thread-store` | `codex-thread-store` | 存储中立（storage-neutral）的线程持久化接口：把 `ThreadId` 解析到本地 rollout 文件、RPC 请求或其他后端，提供 in-memory 等多种实现 |
| `rollout` | `codex-rollout` | **会话 rollout 持久化与发现**：以 JSONL 形式记录会话转录到 `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`，并提供历史会话的列举/搜索能力（actor 模型 + 后台写入任务）|
| `rollout-trace` | `codex-rollout-trace` | rollout **trace bundle** 的格式、写入器与 reducer：拥有 trace schema，供语义回放与查看器投影使用（与 hot-path 解耦） |
| `message-history` | `codex-message-history` | 全局追加式 **消息历史** 持久化，存于 `~/.codex/history.jsonl`（每行一个 `{session_id, ts, text}` JSON 对象） |
| `memories/read` | `codex-memories-read` | 读取用户记忆（长期偏好、项目知识）的底层实现 |
| `memories/write` | `codex-memories-write` | 写入/更新用户记忆的底层实现 |
| `agent-graph-store` | `codex-agent-graph-store` | Agent 决策图（多步推理可视化/调试）的存储 |

> **重要勘误**：`codex-rollout` 与 `codex-rollout-trace` 是 **会话转录（rollout）持久化与回放**
> 相关组件，**不是** "灰度发布 / feature flag" 系统。功能开关由 §3.6 的 `codex-features` 负责。
> 「rollout」一词在 Codex 语境中指「会话回放记录」，对应文档 00 架构图中的
> `RolloutRecorder → JSONL 文件 ~/.codex/sessions/`。

### 3.8 认证与账户层

管理用户身份认证、API 凭据，以及平台账户信息。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `login` | `codex-login` | 完整认证流程实现：OAuth Browser Flow、Device Code Flow、API Key 验证 |
| `keyring-store` | `codex-keyring-store` | 系统密钥链抽象（macOS Keychain / Linux SecretService / Windows DPAPI） |
| `aws-auth` | `codex-aws-auth` | AWS SigV4 签名认证，支持 AWS Bedrock 模型访问 |
| `secrets` | `codex-secrets` | 机密数据的加密存储 |
| `agent-identity` | `codex-agent-identity` | Agent 身份标识管理（用于多 Agent 场景的身份区分与机器身份认证） |
| `chatgpt` | `codex-chatgpt` | ChatGPT 账户 API 客户端（获取账户信息、计划类型、Rate Limit 数据等） |

### 3.9 app-server 层

实现 IDE 插件（VS Code、Cursor、Windsurf 等）与 Codex 核心之间的通信协议。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `app-server` | `codex-app-server` | app-server 主体实现：JSON-RPC 2.0，处理 thread/turn/auth 等 RPC 方法 |
| `app-server-protocol` | `codex-app-server-protocol` | app-server 协议类型定义（Request/Response/Notification），同时生成 TypeScript 类型 |
| `app-server-client` | `codex-app-server-client` | app-server 的 Rust 客户端库，供 TUI 等内部消费者使用 |
| `app-server-transport` | `codex-app-server-transport` | app-server 传输层：连接抽象（`AppServerTransport`）、出站消息队列、远程控制句柄、启动锁等，从主体中拆出传输关注点 |
| `app-server-daemon` | `codex-app-server-daemon` | app-server 守护进程管理：后端选择、托管安装（managed_install）、远程控制客户端、自动更新循环 |
| `app-server-test-client` | `codex-app-server-test-client` | 用于集成测试的 app-server 客户端，模拟 IDE 行为 |
| `debug-client` | `codex-debug-client` | 调试用 app-server 客户端，提供命令行交互接口以手动测试 RPC |
| `codex-experimental-api-macros` | `codex-experimental-api-macros` | 过程宏：`#[experimental("...")]` 注解，用于标记实验性 API 字段 |
| `uds` | `codex-uds` | Unix Domain Socket 服务端/客户端封装 |
| `stdio-to-uds` | `codex-stdio-to-uds` | 将 stdin/stdout 桥接到 Unix Domain Socket（MCP stdio 模式使用） |

```
IDE Extension (TypeScript)
    │
    │  JSON-RPC 2.0（经 app-server-transport）
    ▼
app-server (Rust)
    │  解析 RPC 方法
    │
    ├──> thread/* 方法  ──> thread-store + codex-core
    ├──> turn/* 方法    ──> exec-server (Agent 执行)
    ├──> auth/* 方法    ──> login + chatgpt
    ├──> skill/* 方法   ──> skills + hooks
    └──> app/* 方法     ──> connectors
```

### 3.10 工具函数 utils/ 层

22 个细粒度的通用工具 crate，职责单一，被上层 crate 按需依赖。

| Crate 路径 | 包名 | 职责说明 |
|------------|------|----------|
| `utils/absolute-path` | `codex-utils-absolute-path` | 将相对路径解析为绝对路径的安全封装 |
| `utils/approval-presets` | `codex-utils-approval-presets` | 审批策略预设（suggest/auto-edit/full-auto）的类型定义 |
| `utils/cache` | `codex-utils-cache` | 通用内存缓存实现（LRU 等） |
| `utils/cargo-bin` | `codex-utils-cargo-bin` | 在测试中定位 cargo 编译产物二进制路径（兼容 Bazel runfiles）|
| `utils/cli` | `codex-utils-cli` | CLI 公共工具：颜色输出、进度提示、用户确认提示等 |
| `utils/elapsed` | `codex-utils-elapsed` | 人性化耗时显示（"3s ago"、"2m 15s" 等格式） |
| `utils/fuzzy-match` | `codex-utils-fuzzy-match` | 基于 nucleo 的模糊匹配算法封装（文件搜索/命令历史） |
| `utils/home-dir` | `codex-utils-home-dir` | 跨平台获取用户主目录（`~`）路径 |
| `utils/image` | `codex-utils-image` | 图像处理工具：格式转换、base64 编解码（用于多模态输入） |
| `utils/json-to-toml` | `codex-utils-json-to-toml` | JSON 与 TOML 格式互转工具 |
| `utils/oss` | `codex-utils-oss` | 开源版本特有功能标志（区分 OSS 与内部构建的功能开关） |
| `utils/output-truncation` | `codex-utils-output-truncation` | 命令输出截断策略（防止超大输出淹没上下文窗口） |
| `utils/path-utils` | `codex-utils-path` | 路径操作辅助函数（规范化、相对化、跨平台分隔符处理） |
| `utils/plugins` | `codex-utils-plugins` | 插件发现与加载的底层工具函数 |
| `utils/pty` | `codex-utils-pty` | 伪终端（PTY）读写封装，用于交互式命令执行 |
| `utils/readiness` | `codex-utils-readiness` | 服务就绪检测（等待 UDS/TCP 端口可连接） |
| `utils/rustls-provider` | `codex-utils-rustls-provider` | 统一配置 rustls TLS 提供者（ring 后端） |
| `utils/sandbox-summary` | `codex-utils-sandbox-summary` | 将 SandboxPolicy 格式化为人类可读摘要文本 |
| `utils/sleep-inhibitor` | `codex-utils-sleep-inhibitor` | 阻止系统进入睡眠（长任务运行期间保持唤醒） |
| `utils/stream-parser` | `codex-utils-stream-parser` | SSE/流式数据的增量解析工具 |
| `utils/string` | `codex-utils-string` | 字符串处理辅助函数（截断、转义、Unicode 操作等） |
| `utils/template` | `codex-utils-template` | 简单文本模板引擎（用于系统提示词渲染） |

### 3.11 平台适配层

封装各操作系统特有的安全机制，向上层提供统一接口。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `sandboxing` | `codex-sandboxing` | 跨平台沙箱入口：根据编译目标把命令包装成 Seatbelt / Landlock+bwrap / Windows 沙箱前缀 |
| `linux-sandbox` | `codex-linux-sandbox` | Linux 沙箱实现：Landlock（文件系统 LSM）+ Bubblewrap（用户命名空间容器），两段式执行 |
| `bwrap` | `codex-bwrap` | Bubblewrap 包装二进制：为 Linux 沙箱提供 bwrap 调用支持（vendored/封装） |
| `windows-sandbox-rs` | `codex-windows-sandbox` | Windows 沙箱实现：受限令牌（Restricted Token）+ Job Object 资源限制 |
| `process-hardening` | `codex-process-hardening` | 进程加固工具：禁用 core dump、降低特权、防 ptrace 等 |

```
codex-sandboxing（统一接口）
    │
    ├──[cfg(target_os = "macos")]──> /usr/bin/sandbox-exec (Seatbelt)
    ├──[cfg(target_os = "linux")]──> linux-sandbox + bwrap
    │                                    ├── landlock（文件系统 LSM）
    │                                    └── bubblewrap（用户命名空间）
    └──[cfg(target_os = "windows")]──> windows-sandbox-rs
                                           ├── Restricted Token
                                           └── Job Object
```

### 3.12 外部 Agent 与云服务层

支持导入/对接外部 Agent，以及云端任务能力。

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `external-agent-migration` | `codex-external-agent-migration` | 把外部 Agent 配置迁移/导入 Codex 的辅助工具 |
| `external-agent-sessions` | `codex-external-agent-sessions` | 解析与导出外部 Agent 会话历史的辅助工具 |
| `cloud-tasks` | `codex-cloud-tasks` | 云端任务能力（与云侧任务系统交互） |
| `cloud-tasks-client` | `codex-cloud-tasks-client` | 云端任务的客户端库 |
| `cloud-tasks-mock-client` | `codex-cloud-tasks-mock-client` | 云端任务的 mock 客户端（测试/本地开发用） |
| `cloud-requirements` | `codex-cloud-requirements` | 云端运行环境的需求/前置条件检查 |

### 3.13 可观测性与开发辅助层

| Crate 名称 | 包名 | 职责说明 |
|------------|------|----------|
| `otel` | `codex-otel` | OpenTelemetry 初始化与配置：tracing → OTLP 导出 |
| `analytics` | `codex-analytics` | 匿名使用统计数据的采集与上报 |
| `git-utils` | `codex-git-utils` | Git 仓库操作工具：获取分支名、commit SHA、remote URL 等 |
| `ansi-escape` | `codex-ansi-escape` | ANSI 转义序列的解析与剥离工具 |
| `async-utils` | `codex-async-utils` | 异步编程辅助：`select!` 辅助宏、channel 工具等 |
| `terminal-detection` | `codex-terminal-detection` | 检测当前终端类型与能力（是否支持颜色、是否为 TTY 等） |
| `install-context` | `codex-install-context` | 检测 Codex 的安装方式（npm/brew/binary）并暴露给运行时 |
| `feedback` | `codex-feedback` | 用户反馈提交功能 |
| `test-binary-support` | `codex-test-binary-support` | 集成测试中的二进制启动辅助（spawn 测试用子进程） |
| `thread-manager-sample` | — | 线程管理的参考实现示例 |
| `v8-poc` | `codex-v8-poc` | V8 JavaScript 引擎集成的概念验证（实验性） |

---

## 4. Crate 层次架构图

下图以自底向上的方式展示 crate 间的依赖层次关系（箭头方向为"依赖于"）：

```
╔═══════════════════════════════════════════════════════════════════╗
║  第 5 层：用户入口层                                               ║
║                                                                   ║
║   codex-cli ─────────────────────────────────────────────────┐   ║
║       │ (二进制入口，分发所有子命令)                           │   ║
╚═══════╪═══════════════════════════════════════════════════════╪═══╝
        │ depends on                                            │
╔═══════▼═══════════════════════════════════════════════════════▼═══╗
║  第 4 层：前端 / 运行模式层                                        ║
║                                                                   ║
║  codex-tui          codex-exec        codex-mcp-server           ║
║  (ratatui 全屏)     (无头模式)         (MCP 工具服务器)           ║
╚═══════╪═══════════════╪═══════════════════════╪═══════════════════╝
        │               │                       │
        └───────┬────────┘                       │
                │ depends on                     │
╔═══════════════▼═════════════════════════════════▼═══════════════╗
║  第 3 层：app-server / exec-server 服务层                        ║
║                                                                  ║
║  codex-app-server    codex-exec-server    codex-mcp              ║
║  (+ transport/daemon) (PTY 管理/工具执行)  (MCP 连接管理)        ║
╚═══════════════════════════╪══════════════════════════════════════╝
                            │ depends on
╔═══════════════════════════▼══════════════════════════════════════╗
║  第 2 层：核心业务层 + 扩展系统                                   ║
║                                                                  ║
║  ┌─────────────────────────────────────────────────────────┐    ║
║  │              codex-core (Agent 循环)                    │    ║
║  │   depends on:                                           │    ║
║  │   codex-protocol  codex-config  codex-state             │    ║
║  │   codex-sandboxing codex-tools  codex-execpolicy        │    ║
║  │   codex-rollout   codex-hooks   codex-skills            │    ║
║  └─────────────────────────────────────────────────────────┘    ║
║  ┌─────────────────────────────────────────────────────────┐    ║
║  │   ext/ 扩展（基于 codex-extension-api）                 │    ║
║  │   goal · guardian · image-generation · memories ·       │    ║
║  │   web-search                                            │    ║
║  └─────────────────────────────────────────────────────────┘    ║
╚═══════════════════════════╪══════════════════════════════════════╝
                            │ depends on
╔═══════════════════════════▼══════════════════════════════════════╗
║  第 1 层：基础设施层                                              ║
║                                                                  ║
║  ┌──────────────────┐  ┌────────────────┐  ┌──────────────────┐ ║
║  │  AI 通信         │  │  认证           │  │  持久化          │ ║
║  │  backend-client  │  │  login          │  │  state (SQLite)  │ ║
║  │  codex-api       │  │  keyring-store  │  │  thread-store    │ ║
║  │  model-provider  │  │  chatgpt        │  │  rollout         │ ║
║  └──────────────────┘  └────────────────┘  │  message-history │ ║
║                                             │  memories/*      │ ║
║                                             └──────────────────┘ ║
║  ┌──────────────────┐  ┌────────────────┐  ┌──────────────────┐ ║
║  │  平台适配         │  │  可观测性       │  │  工具函数 utils/  │ ║
║  │  sandboxing      │  │  otel           │  │  (22 个小 crate) │ ║
║  │  linux-sandbox   │  │  analytics      │  │  path-utils      │ ║
║  │  bwrap / windows │  │  git-utils      │  │  string, cli...  │ ║
║  └──────────────────┘  └────────────────┘  └──────────────────┘ ║
╚══════════════════════════════════════════════════════════════════╝
```

---

## 5. 关键配置文件说明

### 5.1 `codex-rs/Cargo.toml` — Workspace Manifest

```toml
[workspace]
members = ["cli", "tui", "exec", ...]   # 所有 crate 成员列表（约 114 个）
resolver = "2"                           # 使用 Cargo resolver v2

[workspace.package]
version = "0.0.0"                        # 统一版本号（由 CI/发布脚本设置）
edition = "2024"                         # Rust 2024 Edition，所有 crate 继承

[workspace.dependencies]
# 所有外部依赖 + 内部 path 依赖集中声明，子 crate 通过 { workspace = true } 引用
tokio = "1"
ratatui = "0.29.0"
sqlx = { version = "0.9.0", ... }
rmcp = { version = "1.7.0", default-features = false }
codex-chatgpt = { path = "chatgpt" }     # 内部 path 依赖也在此声明
...

[workspace.lints.clippy]
# 全局 deny 的 Clippy lint 规则
uninlined_format_args = "deny"
unwrap_used = "deny"
...

[profile.release]
lto = "fat"                              # 全程序链接时优化
strip = "symbols"                        # 剥离符号表
codegen-units = 1                        # 单一 codegen unit，更好的优化

[patch.crates-io]
# 使用内部 fork 版本替代 crates.io 版本
ratatui = { git = "...", rev = "..." }   # ratatui 补丁 fork（nornagon）
crossterm = { git = "...", rev = "..." } # crossterm 补丁 fork（nornagon）
tokio-tungstenite = { git = "..." }      # openai-oss-forks
```

**核心作用**：

- 所有依赖版本在此**集中管理**，避免不同 crate 使用不同版本导致的编译膨胀
- `[workspace.lints]` 统一执行严格的 Clippy 规则，防止低质量代码进入 codebase
- `[patch.crates-io]` 允许在上游未合入补丁时使用 fork 版本

---

### 5.2 `codex-rs/rust-toolchain.toml` — 工具链锁定

```toml
[toolchain]
channel = "1.95.0"                       # 固定 Rust 版本
components = ["clippy", "rustfmt", "rust-src"]
```

**核心作用**：

- 确保所有开发者和 CI 环境使用**完全相同的编译器版本**，保证构建可复现
- `rust-src` 组件支持 IDE 代码补全和 `std` 源码跳转
- 版本升级需显式修改此文件并经过测试验证

---

### 5.3 `codex-rs/clippy.toml` — Clippy 规则配置

```toml
allow-expect-in-tests = true             # 测试代码允许使用 .expect()
allow-unwrap-in-tests = true             # 测试代码允许使用 .unwrap()

await-holding-invalid-types = [          # 跨 await 点不允许持有的类型
    "tokio::sync::MutexGuard",
    "tokio::sync::RwLockReadGuard",
    ...
]

disallowed-methods = [
    # 禁止使用 RGB/Indexed 颜色（影响终端主题兼容性）
    { path = "ratatui::style::Color::Rgb", reason = "..." },
    # 禁止使用 .white() / .yellow()（TUI 样式规范）
    { path = "ratatui::style::Stylize::white", reason = "..." },
    ...
]

large-error-threshold = 256              # Result<T, E> 中 E 的最大内存大小
```

**核心作用**：

- 在 workspace 级别的 `deny` 之外提供**更细粒度的配置**
- 强制 TUI 使用 ANSI 标准颜色，确保在各类终端主题下视觉效果一致
- 防止持锁跨 await 点的异步死锁风险

---

### 5.4 `codex-rs/rustfmt.toml` — 代码格式化配置

```toml
edition = "2024"                         # 与 workspace edition 保持一致
imports_granularity = "Item"             # 每个 use item 独占一行（便于 git diff）
```

**核心作用**：

- `imports_granularity = "Item"` 使每个 `use` 声明单独一行，避免合并冲突
- 与 `just fmt` 命令配合使用
- 所有代码提交前**必须通过** `just fmt` 格式化

---

### 5.5 `codex/justfile` — 快捷任务定义

工作目录设置为 `codex-rs/`，常用任务：

| 命令 | 等效操作 | 说明 |
|------|----------|------|
| `just fmt` | `cargo fmt ...` | 格式化所有 Rust 代码 |
| `just fix -p <crate>` | `cargo clippy --fix --tests ...` | 修复指定 crate 的 lint 问题 |
| `just clippy` | `cargo clippy --tests` | 运行 Clippy 检查 |
| `just test` | `cargo nextest run --no-fail-fast` | 运行全量测试（需安装 cargo-nextest） |
| `just codex` | `cargo run --bin codex` | 从源码运行 codex |
| `just exec "..."` | `cargo run --bin codex -- exec "..."` | 从源码运行 exec 模式 |
| `just write-config-schema` | `cargo run -p codex-core --bin ...` | 重新生成 config.schema.json |
| `just write-app-server-schema` | `cargo run -p codex-app-server-protocol ...` | 重新生成 app-server 协议 Schema |
| `just bazel-lock-update` | `bazel mod deps --lockfile_mode=update` | 更新 Bazel 锁文件 |
| `just bazel-lock-check` | `scripts/check-module-bazel-lock.sh` | 验证 Bazel 锁文件是否漂移 |

**核心作用**：统一团队的开发工作流，避免每人记忆不同的长命令。

---

### 5.6 `codex-rs/.cargo/config.toml` — Cargo 构建配置

```toml
# Windows MSVC 目标：设置 8 MiB 栈大小
[target.'cfg(all(windows, target_env = "msvc"))']
rustflags = ["-C", "link-arg=/STACK:8388608"]

# Windows arm64 MSVC：额外链接参数
[target.aarch64-pc-windows-msvc]
rustflags = ["-C", "link-arg=/STACK:8388608", "-C", "link-arg=/arm64hazardfree"]

# Windows MinGW 目标：设置 8 MiB 栈大小
[target.'cfg(all(windows, target_env = "gnu"))']
rustflags = ["-C", "link-arg=-Wl,--stack,8388608"]
```

**核心作用**：

- 将 Windows 默认栈大小从 1 MiB 扩展到 **8 MiB**（与 `justfile` 中 `RUST_MIN_STACK=8388608` 呼应）
- 避免深度递归（如 AST 分析、大型 JSON 解析）导致 Windows 上的栈溢出
- `justfile` 中 Linux/macOS 通过 `RUST_MIN_STACK` 环境变量实现相同效果

---

## 附录：crate 数量统计

```
入口层                    4 crates
核心业务层                6 crates
扩展系统层（ext/）        6 crates
AI 通信层                13 crates
工具与 MCP 层            14 crates
配置层                    7 crates
持久化层                  8 crates
认证与账户层              6 crates
app-server 层            10 crates
工具函数 utils/           22 crates
平台适配层                5 crates
外部 Agent 与云服务层      6 crates
可观测性与开发辅助        11 crates
─────────────────────────────────
合计                    ~115 crates（members ≈ 114 + 若干 path 依赖 crate）
```

> **统计口径**：以上为按职责的近似归类，个别 crate（如 `sandboxing` 同时属核心与平台适配）
> 会跨组，故分组求和与 workspace `members` 精确计数（约 114）存在小幅出入；权威清单始终以
> `codex-rs/Cargo.toml` 的 `members` 数组为准。

---

## 6. 各 crate 分组对应的深潜文档

本文给出的是"地图"——crate 在哪、归哪组、做什么。若想纵向钻进某一组里看清「这组 crate
具体是怎么协作运转的」，下列子系统专题文档（17 ~ 23）配套了 `feature-learn` 分支上的中文源码注释，
按本文的 crate 分组对照阅读最为顺手：

| 深潜文档 | 主题 | 关联本文 crate 分组 / 代表 crate |
|----------|------|----------------------------------|
| [17 - apply-patch 与 V4A 编辑](../3_执行与安全/17_apply_patch_editing.md) | 补丁解析、模糊定位、落盘与跨回合追踪 | §3.5 工具与 MCP 层（`apply-patch`） |
| [18 - 命令执行与安全](../3_执行与安全/18_exec_and_safety.md) | execpolicy / safety / 审批 / 沙箱 / Guardian | §3.5（`execpolicy`）+ §3.3（`ext/guardian`）+ §3.11 平台适配 |
| [19 - MCP 双向集成](../4_工具与多Agent/19_mcp_integration.md) | 既当 MCP 客户端又当 MCP 服务器 | §3.5（`codex-mcp`、`mcp-server`、`rmcp-client`） |
| [20 - app-server 集成层](../5_前端_集成_协议/20_app_server_layer.md) | IDE / 远控经 JSON-RPC 落到内核 `Op` | §3.9 app-server 层 |
| [21 - 配置系统](../6_数据与配置/21_config_system.md) | 分层配置、Profile、优先级与不可覆盖约束 | §3.6 配置层（`config`、`features`） |
| [22 - 认证与登录](../5_前端_集成_协议/22_auth_and_login.md) | 四种认证模式：API Key / ChatGPT OAuth / Agent Identity | §3.8 认证与账户层（`login`、`keyring-store`） |
| [23 - 网络代理](../3_执行与安全/23_network_proxy.md) | 本地 HTTP/SOCKS5 代理 + 域名白名单出网管控 | §3.4 AI 通信层（`network-proxy`） |

---

*文档最后更新：基于 `codex-rs/Cargo.toml` workspace 成员列表，工具链版本 `1.95.0`*

---

> **上一篇**：[00 - Codex 项目总览](./00_overview.md)
> **下一篇**：[02 - 核心架构](../1_架构/02_core_architecture.md)
