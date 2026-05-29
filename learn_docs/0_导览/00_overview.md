# 00 - Codex 项目总览
> 文档编号：00 | 类型：概览 | 适合读者：所有人

---

## 目录

1. [项目介绍](#1-项目介绍)
2. [技术栈](#2-技术栈)
3. [支持平台](#3-支持平台)
4. [运行模式](#4-运行模式)
5. [认证方式](#5-认证方式)
6. [沙箱机制总览](#6-沙箱机制总览)
7. [整体系统架构图](#7-整体系统架构图)
8. [学习路径建议](#8-学习路径建议)

---

## 1. 项目介绍

**Codex** 是 OpenAI 开源的**本地 AI 编码代理（Coding Agent）**，GitHub 地址：
[https://github.com/openai/codex](https://github.com/openai/codex)

与纯云端方案不同，Codex 直接运行在用户本地终端，通过 **OpenAI Responses API** 与大模型通信，
将 AI 的推理能力与本地执行环境（文件系统、Shell）深度结合，形成一个可自主完成复杂编码任务的智能体。

### 核心特性

| 特性 | 说明 |
|------|------|
| **本地运行** | 完全在用户机器上执行，代码与文件不离开本地环境 |
| **全屏 TUI** | 基于 Ratatui 构建的终端交互界面，支持对话、文件查看、差异审查 |
| **沙箱隔离** | 跨平台沙箱（macOS Seatbelt / Linux bwrap / Windows Restricted Token） |
| **MCP 协议** | 作为 MCP 客户端接入外部工具，也可作为 MCP 服务端被其他智能体调用 |
| **多模式运行** | TUI 交互 / exec 非交互 / app-server IDE 插件 / mcp-server 无头服务 |
| **多认证方式** | ChatGPT OAuth / Device Code / API Key / Agent Identity |
| **持久化会话** | 对话历史以 JSONL Rollout 文件 + SQLite 数据库持久化 |

### 安装方式

```/dev/null/install.sh#L1-9
# 方式一：npm 全局安装（推荐）
npm install -g @openai/codex

# 方式二：Homebrew（macOS）
brew install --cask codex

# 方式三：直接下载二进制（GitHub Releases）
# macOS Apple Silicon: codex-aarch64-apple-darwin.tar.gz
# macOS x86_64:        codex-x86_64-apple-darwin.tar.gz
# Linux x86_64:        codex-x86_64-unknown-linux-musl.tar.gz
# Linux arm64:         codex-aarch64-unknown-linux-musl.tar.gz
```

---

## 2. 技术栈

Codex Rust 实现（`codex-rs/`）采用 **Rust 2024 edition**，涵盖终端 UI、异步 IO、AI 通信、
沙箱隔离、持久化存储等各层，选型均指向成熟的社区库。

| 技术 | 版本/说明 | 用途 |
|------|-----------|------|
| **Rust 2024 edition** | workspace 统一 edition | 语言基础，全部 crate 共享 |
| **Tokio** | 1.x | 异步运行时，驱动网络 IO、定时器、任务调度 |
| **Ratatui** | 0.29（社区 fork） | 终端 UI 框架，构建全屏 TUI 界面 |
| **Crossterm** | 0.28（社区 fork） | 跨平台终端控制，处理键盘/鼠标事件与颜色输出 |
| **SQLite + SQLx** | 0.9.0 | 会话状态（thread、turn 等）持久化存储 |
| **Serde + serde_json** | 1.x | JSON/TOML 序列化，协议消息、配置文件处理 |
| **Clap** | 4.x | CLI 参数解析，子命令路由 |
| **reqwest** | 0.12 | HTTP 客户端，访问 OpenAI Responses API |
| **tokio-tungstenite** | 0.28（社区 fork） | WebSocket，实时通信（Realtime WebRTC 信令等） |
| **tracing + tracing-subscriber** | 0.1.x / 0.3.x | 结构化日志与链路追踪，支持 OTEL 导出 |
| **rmcp** | 1.7.0 | Model Context Protocol（MCP）SDK，工具调用协议 |
| **insta** | 1.46.3 | 快照测试（snapshot testing），用于 TUI 渲染回归测试 |
| **Bazel** | `.bazelversion` 指定 | 企业级构建系统，与 Cargo 并行运行，支持 CI 远端缓存 |
| **axum** | 0.8 | HTTP/WebSocket 服务框架，app-server 与 exec-server |
| **landlock** | 0.4.4 | Linux Landlock 内核沙箱绑定 |
| **pretty_assertions** | 1.4.1 | 测试断言，提供彩色 diff 输出 |
| **textwrap** | 0.16.2 | TUI 中的文本换行处理 |

> **备注**：Ratatui、Crossterm、tokio-tungstenite 均使用 OpenAI 维护的社区 fork，
> 通过 `[patch.crates-io]` 覆盖，以获得尚未合并到上游的修复。

---

## 3. 支持平台

### 平台矩阵

| 平台 | 最低版本 | 沙箱机制 | 备注 |
|------|---------|---------|------|
| **macOS** | 12.0+ | Apple Seatbelt（sandbox-exec） | 主力开发平台，支持全部功能 |
| **Ubuntu / Debian** | 20.04+ / 10+ | Landlock + Bubblewrap（bwrap） | musl 静态链接二进制 |
| **其他 Linux** | 内核 5.13+（Landlock） | Landlock / bwrap fallback | 需用户命名空间支持 |
| **Windows** | 11 via WSL2 | Restricted Token + Job Object | WSL1 不支持 bwrap 沙箱 |

### 各平台沙箱详解

#### macOS — Apple Seatbelt

macOS 平台通过系统内置的 `/usr/bin/sandbox-exec` 实现进程沙箱，使用
**SBPL（Sandbox Profile Language）** 规则文件描述权限策略。

- **read-only 模式**：允许读取全部文件系统，但禁止写入；网络访问默认禁止
- **workspace-write 模式**：在 read-only 基础上，额外放开当前工作区目录的写权限；
  同时允许 `~/.codex/memories` 写入，保证记忆维护无需额外审批
- **保护目标**：`.git` 目录、`gitdir:` 指向目标、`.codex` 配置目录默认保持只读

#### Linux — Landlock + Bubblewrap

Linux 平台采用双机制组合：

1. **Landlock**（内核 5.13+ 特性）：文件系统访问控制 LSM，精细限制 open/mkdir/rename 等系统调用
2. **Bubblewrap（bwrap）**：用户命名空间沙箱，构建隔离的挂载/进程命名空间

路由逻辑：
```/dev/null/sandbox_routing.txt#L1-8
if bwrap 可用 && 策略需要精细控制:
    → 走 bubblewrap 路径（支持 read-only carveout、writable 子树覆盖）
elif 策略等价于 legacy SandboxPolicy:
    → 走 Landlock legacy 路径
else:
    → 拒绝执行（fail closed）

WSL1: bwrap 无法创建用户命名空间 → 拒绝沙箱 shell 命令
```

- **bwrap 版本降级**：若系统 bwrap 过旧（不支持 `--argv0`），自动切换兼容路径
- **bwrap 缺失**：回退到二进制内置的 vendored bwrap，并向用户发出警告

#### Windows — Restricted Token + Job Object

- **受限令牌（Restricted Token）**：降低进程权限，移除不必要的特权
- **Job Object**：限制子进程的资源使用（CPU、内存、句柄继承）
- 支持 `read-only`、`workspace-write` legacy 策略，以及部分 split filesystem 策略
- 不支持的 `none`（显式不可读 carveout）策略会 fail closed，而非降级执行

---

## 4. 运行模式

Codex 通过统一的 `codex-cli` 二进制（`cli` crate）对外提供四种运行模式：

```/dev/null/modes_overview.txt#L1-38
┌────────────────────────────────────────────────────────────────┐
│                       用户 / 客户端                              │
│   终端直接运行     IDE 插件(VSCode/Cursor)    自动化脚本/CI      │
└──────┬──────────────────┬──────────────────────┬───────────────┘
       │                  │                      │
       ▼                  ▼                      ▼
┌──────────────┐  ┌───────────────┐  ┌───────────────────────────┐
│  TUI 模式    │  │  app-server   │  │  exec 模式                 │
│  codex       │  │  模式         │  │  codex exec "prompt"      │
│  (全屏交互)  │  │  codex app    │  │  (非交互，stdout 输出)    │
└──────┬───────┘  └───────┬───────┘  └──────────┬────────────────┘
       │                  │                      │
       │           ┌──────▼──────┐               │
       │           │  MCP server │               │
       │           │  模式       │               │
       │           │  codex      │               │
       │           │  mcp-server │               │
       │           └──────┬──────┘               │
       │                  │                      │
       └──────────┬────────┘                     │
                  ▼                              ▼
         ┌────────────────────────────────────────┐
         │           codex-core                   │
         │  (ThreadManager / Session / Agent)     │
         └────────────────────────────────────────┘
```

### 四种模式详解

#### 模式一：TUI 模式（`codex`）

**命令**：直接运行 `codex`

**适用场景**：日常交互式编码辅助，最常用的模式

**特点**：
- 基于 Ratatui 构建的全屏终端界面
- 支持对话历史浏览、文件差异预览、Markdown 渲染
- 实时显示 AI 工具调用过程（shell 执行、文件修改等）
- 每次操作通过批准（Approve / Deny / Always Allow）控制安全边界
- 支持键盘快捷键、历史搜索、会话恢复

#### 模式二：exec 模式（`codex exec`）

**命令**：`codex exec "完成任务的提示词"` 或 `echo "prompt" | codex exec`

**适用场景**：CI/CD 自动化、脚本集成、批量处理

**特点**：
- 完全非交互，输出打印到 stdout/stderr
- 支持同时接收参数与 stdin（stdin 以 `<stdin>` 块追加到提示词后）
- `--ephemeral` 标志：不持久化 rollout 文件到磁盘
- `RUST_LOG` 环境变量控制日志级别

#### 模式三：app-server 模式（`codex app`）

**命令**：`codex app`

**适用场景**：IDE 插件（VS Code / Cursor / Windsurf）、桌面应用

**特点**：
- 实现 JSON-RPC over Unix Domain Socket（UDS）协议
- 提供完整的 thread/turn/approval/skill 等 API
- 支持多客户端并发连接
- 实验性 API 通过 `experimentalApi` capability 选择启用

#### 模式四：mcp-server 模式（`codex mcp-server`）

**命令**：`codex mcp-server`

**适用场景**：作为工具被其他 MCP 客户端（如另一个 AI 智能体）调用

**特点**：
- 将 Codex 自身作为一个 MCP 工具暴露给外部
- 标准 MCP 协议通过 stdio 通信
- 可通过 `npx @modelcontextprotocol/inspector codex mcp-server` 快速调试

---

## 5. 认证方式

Codex 支持四种认证方式，适应不同的使用场景：

```/dev/null/auth_flow.txt#L1-60
                        ┌─────────────────────────────┐
                        │        用户启动 codex        │
                        └──────────────┬──────────────┘
                                       │
                    ┌──────────────────▼──────────────────┐
                    │         是否已有有效凭证？             │
                    └──┬────────────┬────────────┬─────────┘
                       │ 是         │ 否          │
                       ▼            ▼            ▼
              ┌──────────────┐  选择认证方式  （继续）
              │  直接进入    │       │
              │  主界面      │       │
              └──────────────┘       │
                    ┌────────────────┼────────────────┐
                    │                │                │
                    ▼                ▼                ▼
        ┌─────────────────┐ ┌────────────────┐ ┌──────────────────┐
        │ ChatGPT OAuth   │ │ Device Code    │ │ API Key          │
        │ (浏览器流程)    │ │ Flow           │ │ (OPENAI_API_KEY) │
        └────────┬────────┘ └───────┬────────┘ └────────┬─────────┘
                 │                  │                   │
                 ▼                  ▼                   ▼
        打开浏览器到      显示 verification_url    读取环境变量
        ChatGPT 授权页    和 user_code，用户       或 keychain
                          在手机/另一设备输入
                 │                  │                   │
                 ▼                  ▼                   ▼
        轮询 OAuth 回调   轮询设备授权结果         验证 Key 有效性
        获取 access token 获取 access token
                 │                  │                   │
                 └──────────────────┴───────────────────┘
                                    │
                                    ▼
                        ┌─────────────────────────┐
                        │  凭证存储到系统 keychain  │
                        │  或 ~/.codex/secrets/    │
                        └─────────────────────────┘
```

### 四种认证方式对比

| 认证方式 | 命令/方式 | 适用场景 | 说明 |
|---------|---------|---------|------|
| **ChatGPT OAuth** | `codex` → Sign in with ChatGPT | 个人用户（Plus/Pro/Business） | 浏览器弹出授权页，支持 SSO；凭证存储在系统 keychain |
| **ChatGPT Device Code** | OAuth device_code flow | 无浏览器的服务器、CI 环境 | 显示 URL + 用户码，在另一设备完成授权 |
| **API Key** | `OPENAI_API_KEY=sk-...` 环境变量 | API 用量计费用户、企业集成 | 直接跳过 OAuth 流程，最简单直接 |
| **Agent Identity** | `agent-identity` crate | 企业自动化、多智能体场景 | 基于机器身份的认证，无需人工干预 |

---

## 6. 沙箱机制总览

三平台沙箱对比：

```/dev/null/sandbox_comparison.txt#L1-55
┌──────────────────────┬──────────────────────┬────────────────────────┐
│       macOS          │        Linux          │       Windows          │
│   Apple Seatbelt     │  Landlock + bwrap     │  Restricted Token      │
│                      │                      │  + Job Object          │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 内核机制             │ 内核机制              │ 内核机制               │
│ /usr/bin/sandbox-    │ Landlock LSM          │ Win32 Token API        │
│ exec (SBPL 规则文件) │ (内核 5.13+)          │ CreateRestrictedToken  │
│                      │ Bubblewrap namespace  │ + AssignProcessToJob   │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 策略描述             │ 策略描述              │ 策略描述               │
│ SandboxPolicy →      │ FileSystemSandbox     │ SandboxPolicy →        │
│ SBPL 字符串模板      │ Policy (split FS)     │ Restricted Token 配置  │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 可控维度             │ 可控维度              │ 可控维度               │
│ • 文件读/写          │ • 文件读/写           │ • 文件读/写            │
│ • 网络访问           │ • 网络访问            │ • 网络访问             │
│ • 进程 fork          │ • 挂载命名空间        │ • 资源配额             │
│ • IPC                │ • 进程命名空间        │ • 特权降低             │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 保护项目             │ 保护项目              │ 保护项目               │
│ .git 目录 (只读)     │ 用户命名空间隔离      │ 系统目录保护           │
│ .codex 目录 (只读)   │ 按需挂载 /proc /dev   │ Registry 访问控制      │
│ gitdir 指向 (只读)   │ bwrap pivot_root      │                        │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 沙箱模式             │ 沙箱模式              │ 沙箱模式               │
│ read-only            │ read-only             │ read-only              │
│ workspace-write      │ workspace-write       │ workspace-write        │
│ danger-full-access   │ danger-full-access    │ danger-full-access     │
├──────────────────────┼──────────────────────┼────────────────────────┤
│ 失败行为             │ 失败行为              │ 失败行为               │
│ SBPL deny → 系统     │ bwrap 失败 →          │ Token 创建失败 →       │
│ 调用返回 EPERM       │ vendored bwrap 回退   │ fail closed            │
│                      │ → 用户警告            │                        │
└──────────────────────┴──────────────────────┴────────────────────────┘
```

### 沙箱策略速查

| 策略名 | 文件读 | 文件写 | 网络 | 适用场景 |
|-------|-------|-------|------|---------|
| `read-only` | ✅ 全部 | ❌ 禁止 | ❌ 禁止 | 代码审查、只读分析 |
| `workspace-write` | ✅ 全部 | ✅ 工作区内 | ❌ 禁止 | 日常编码任务（默认推荐） |
| `danger-full-access` | ✅ 全部 | ✅ 全部 | ✅ 允许 | 已在容器内运行时使用 |

---

## 7. 整体系统架构图

```/dev/null/system_architecture.txt#L1-90
╔══════════════════════════════════════════════════════════════════════╗
║                        用户输入层                                    ║
║  ┌─────────────┐  ┌────────────────┐  ┌───────────────────────────┐ ║
║  │  终端直接   │  │  IDE 插件      │  │  自动化脚本 / CI Pipeline │ ║
║  │  运行 codex │  │  VSCode/Cursor │  │  codex exec "prompt"      │ ║
║  └──────┬──────┘  └───────┬────────┘  └────────────┬──────────────┘ ║
╚═════════╪═════════════════╪═══════════════════════╪════════════════╝
          │                 │                        │
╔═════════▼═════════════════▼════════════════════════▼════════════════╗
║                        CLI 层（codex-cli crate）                     ║
║           统一二进制入口，路由子命令到对应运行模式                     ║
║   codex | codex exec | codex app | codex mcp-server | codex sandbox ║
╚═════════╪═════════════════╪════════════════════════╪════════════════╝
          │                 │                        │
╔═════════▼═══╗  ╔══════════▼═══════╗  ╔════════════▼═══════════════╗
║  TUI 模式   ║  ║  app-server 模式 ║  ║  exec 模式                 ║
║ (codex-tui) ║  ║ (codex-app-      ║  ║ (codex-exec)               ║
║             ║  ║  server)         ║  ║                            ║
║ Ratatui 全屏║  ║ JSON-RPC over    ║  ║ 无头运行，stdout 输出      ║
║ 终端界面    ║  ║ Unix Socket      ║  ║ 适合自动化 / CI            ║
╚═════╪═══════╝  ╚══════════╪═══════╝  ╚════════════╪═══════════════╝
      │                     │                        │
      │           ╔══════════▼═══════╗               │
      │           ║  mcp-server 模式 ║               │
      │           ║ (codex-mcp-      ║               │
      │           ║  server)         ║               │
      │           ║ stdio MCP 协议   ║               │
      │           ║ 被其他 Agent 调用║               │
      │           ╚══════════╪═══════╝               │
      └─────────────────────┼───────────────────────┘
                            │
╔═══════════════════════════▼══════════════════════════════════════════╗
║                    核心层（codex-core crate）                         ║
║                                                                      ║
║  ┌─────────────────┐  ┌───────────────────┐  ┌──────────────────┐  ║
║  │  ThreadManager  │  │     Session       │  │      Agent       │  ║
║  │ 管理所有会话线程│  │ 单次对话上下文    │  │ AI 推理主循环    │  ║
║  │ 生命周期调度    │  │ 历史消息构建      │  │ 工具调用编排     │  ║
║  └────────┬────────┘  └────────┬──────────┘  └────────┬─────────┘  ║
║           └─────────────────────┴──────────────────────┘            ║
╚═══════════════════════════╪══════════════════════════════════════════╝
                            │
        ┌───────────────────┼───────────────────┐
        │                   │                   │
╔═══════▼═════════╗  ╔══════▼══════════╗  ╔═════▼══════════════════╗
║  AI 通信层      ║  ║   工具层        ║  ║   持久化层             ║
║                 ║  ║                 ║  ║                        ║
║  ModelClient    ║  ║   ToolRouter    ║  ║  RolloutRecorder       ║
║  backend-client ║  ║   tools crate   ║  ║  → JSONL 文件          ║
║  reqwest HTTP   ║  ║                 ║  ║  ~/.codex/sessions/    ║
║                 ║  ║  ┌──────────┐  ║  ║                        ║
║  ┌───────────┐  ║  ║  │  shell   │  ║  ║  SQLite state_5.sqlite ║
║  │ OpenAI    │  ║  ║  │ 执行命令 │  ║  ║  codex-state crate     ║
║  │ Responses │  ║  ║  ├──────────┤  ║  ║  ~/.codex/state_5.sqlite ║
║  │ API       │◄─║──║  │apply_    │  ║  ║                        ║
║  │ (SSE 流式)│  ║  ║  │patch     │  ║  ║  thread / turn / goal  ║
║  └───────────┘  ║  ║  │ 文件修改 │  ║  ║  等结构化数据          ║
║                 ║  ║  ├──────────┤  ║  ╚════════════════════════╝
╚═════════════════╝  ║  │  MCP 工具│  ║
                     ║  │ 外部工具  │  ║
                     ║  │ rmcp SDK │  ║
                     ║  └──────────┘  ║
                     ╚═══════╪════════╝
                             │
╔════════════════════════════▼═════════════════════════════════════════╗
║                      沙箱层（sandboxing crate）                       ║
║                                                                      ║
║  ┌───────────────────────────────────────────────────────────────┐  ║
║  │                    SandboxManager                             │  ║
║  ├───────────────┬──────────────────────┬────────────────────────┤  ║
║  │    macOS      │        Linux         │       Windows          │  ║
║  │  Seatbelt     │  Landlock + bwrap    │  Restricted Token      │  ║
║  │ sandbox-exec  │  (linux-sandbox      │  + Job Object          │  ║
║  │ SBPL 规则     │   crate)             │  (windows-sandbox-rs)  │  ║
║  └───────────────┴──────────────────────┴────────────────────────┘  ║
╚══════════════════════════════════════════════════════════════════════╝
```

### 关键数据流说明

```/dev/null/data_flow.txt#L1-25
用户输入提示词
    │
    ▼
ThreadManager 创建/恢复 Thread
    │
    ▼
Session 构建消息历史（含工具调用结果）
    │
    ▼
ModelClient 发送 POST /v1/responses → OpenAI API
    │ SSE 流式响应
    ▼
Agent 解析 delta 事件
    ├── text delta → 实时渲染到 TUI
    └── function_call → ToolRouter 分发
                            │
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
           shell          apply_patch   MCP 工具
          (沙箱内执行)    (文件修改)   (外部服务)
              │             │
              └──── 结果写入 RolloutRecorder ────┘
                         │
                    SQLite 持久化 turn 记录
                         │
                    返回工具输出给模型
                    进入下一轮推理循环
```

---

## 8. 学习路径建议

根据读者背景与目标，建议按以下顺序阅读本系列文档：

### 通用学习路径（从入门到深入）

```/dev/null/learning_path.txt#L1-35
┌────────────────────────────────────────────────────────────┐
│  阶段 1：宏观认知（本文）                                   │
│  📄 00_overview.md                                         │
│  目标：理解 Codex 是什么、技术栈、四种模式、整体架构        │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 2：代码组织                                           │
│  📄 01_repository_structure.md                             │
│  目标：掌握仓库目录结构、全部 crate 分组与职责              │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 3：核心架构                                           │
│  📄 02_core_architecture.md                                │
│  目标：深入理解 codex-core 内部：                           │
│        ThreadManager / Session / Agent / ToolRouter         │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 4：数据持久化                                         │
│  📄 03_database_design.md                                  │
│  目标：理解 SQLite schema、Rollout JSONL 格式、状态管理      │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 5：Agent 流程与组件设计                                │
│  📄 04_agent_main_flow.md → Agent Turn 执行时序             │
│  📄 05_component_design.md → 核心组件拆解                   │
│  📄 06_tui_design.md → TUI 渲染与事件处理                   │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 6：架构图谱与新特性深度解析                            │
│  📄 07_goal_mode.md → /goal 模式完整实现解析                │
│      (实验性新功能，长时任务自动续期机制)                    │
│  📄 08_architecture_diagrams.md → 全景架构图集              │
│      (Crate 依赖图、数据流图、沙箱图、MCP 集成等)           │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 7：源码精读（core crate 深潜）                         │
│  📄 09_codex_core_reading_guide.md                         │
│      → 15.4 万行 core 源码完整阅读路线图                    │
│      → 各模块速查表、阅读顺序、关键问题清单、避坑指南        │
└────────────────────────────────┬───────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 8：专题深潜（高级运行时主题，按需选读）                │
│  📄 10_followup_interrupt_progress.md → 追问 / 打断 / 进度  │
│  📄 11_thread_session_turn_lifecycle.md → 三层生命周期      │
│  📄 12_memory_system.md → 长期记忆系统（注入与维护）        │
│  📄 13_sandbox_mechanism.md → 三平台沙箱机制深度解析        │
│  📄 14_multi_agent_system.md → 多 Agent 树形协作            │
│  📄 15_api_protocol_layer.md → SQ/EQ 双队列与协议契约       │
│  📄 16_agent_optimization.md → 上下文管理与自动压缩         │
└────────────────────────────┬───────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────────┐
│  阶段 9：子系统专题（工具 / 执行 / 集成 / 配置 / 认证）       │
│  📄 17_apply_patch_editing.md → apply-patch 与 V4A 编辑     │
│  📄 18_exec_and_safety.md → 命令执行与安全（execpolicy/    │
│      safety/guardian/沙箱）                                  │
│  📄 19_mcp_integration.md → MCP 双向集成（client + server）│
│  📄 20_app_server_layer.md → app-server 集成层与 JSON-RPC  │
│  📄 21_config_system.md → 配置系统（分层/Profile/优先级）  │
│  📄 22_auth_and_login.md → 认证与登录（OAuth/API Key/      │
│      Keyring）                                               │
│  📄 23_network_proxy.md → 网络代理（沙箱网络出口控制）     │
└────────────────────────────────────────────────────────────┘
```

> **📁 目录结构说明**：本系列文档已按主题归入 7 个子目录——`0_导览/`、`1_架构/`、
> `2_运行时核心/`、`3_执行与安全/`、`4_工具与多Agent/`、`5_前端_集成_协议/`、`6_数据与配置/`。
> 上面的"学习路径"是**推荐阅读顺序**（跨目录线性推进，按编号 00→23）；完整的**按目录索引**
> 见根目录 [`../README.md`](../README.md)。

### 按角色推荐

| 角色 | 推荐重点 |
|------|---------|
| **普通用户** | 00 → 安装使用文档 → 配置文档 → 21（配置系统）|
| **贡献者（新手）** | 00 → 01 → 02 → 04 → 05 |
| **TUI 开发** | 00 → 01 → 02 → 06 → 10（追问/打断/进度）|
| **API/协议开发** | 00 → 02 → 04 → 08 → 15（SQ/EQ 与协议契约）→ 20（app-server/JSON-RPC）|
| **平台/安全工程师** | 00 → 08（沙箱架构图）→ 13（沙箱机制深潜）→ 18（执行与安全）→ 23（网络代理）→ 各平台实现 |
| **AI/Agent 集成** | 00 → 08（MCP 集成图）→ 19（MCP 双向集成）→ 14（多 Agent）→ 07（Goal 模式）|
| **core 源码贡献者** | 00 → 02 → 09（精读指南）→ 11（生命周期）→ 12（记忆）→ 17（apply-patch 编辑）→ 逐模块深入 |
| **认证/集成工程师** | 00 → 22（认证与登录）→ 20（app-server）→ 21（配置系统）|
| **长任务/自治 Agent** | 07（Goal 模式）→ 16（上下文管理与压缩）重点阅读 |

---

> **下一篇**：[01 - 仓库结构与 Crate 说明](./01_repository_structure.md)
