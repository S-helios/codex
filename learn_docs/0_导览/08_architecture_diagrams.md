# 08 - 架构设计综合图谱
> 文档编号：08 | 类型：架构设计 | 适合读者：系统设计者、贡献者

---

## 目录

1. [Crate 依赖全景图](#1-crate-依赖全景图)
2. [分层架构图（垂直）](#2-分层架构图垂直)
3. [核心数据流图](#3-核心数据流图)
4. [多运行模式对比](#4-多运行模式对比)
5. [MCP 集成架构图](#5-mcp-集成架构图)
6. [沙箱安全架构图](#6-沙箱安全架构图)
7. [Multi-Agent 父子树结构](#7-multi-agent-父子树结构)
8. [认证系统架构图](#8-认证系统架构图)
9. [Memory/Compaction 数据流](#9-memorycompaction-数据流)
10. [Goal 模式与 Thread 状态全景](#10-goal-模式与-thread-状态全景)

---

## 1. Crate 依赖全景图

```
codex-rs 工作区包含 100+ 个 crate（baseline 快照为 114 个工作区成员），按职责分为以下层次：

┌──────────────────────── 入口/前端层 ─────────────────────────────────┐
│ codex-cli        │ 主 CLI 入口，路由到各子命令                        │
│ codex-tui        │ 全屏 TUI（Ratatui），用户交互主界面                │
│ codex-exec       │ exec 非交互模式（脚本/CI 使用）                    │
│ codex-mcp-server │ 作为 MCP 服务端，暴露工具给外部 Agent              │
│ codex-app-server │ v2 API 服务层（IDE 插件/远程控制）                 │
└──────────────────────────────────────────────────────────────────────┘
         │                │              │               │
         ▼                ▼              ▼               ▼
┌──────────────────────── 协议/通信层 ─────────────────────────────────┐
│ app-server-protocol    │ v2 API 请求/响应类型定义                    │
│ app-server-transport   │ HTTP/WebSocket 传输实现                     │
│ app-server-client      │ TUI 与 app-server 的本地 RPC 客户端          │
│ codex-protocol         │ 核心协议类型（Op/Event/ThreadGoal 等）       │
│ rollout-trace          │ Rollout JSONL 序列化/反序列化               │
└──────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────── 核心 Agent 层 ─────────────────────────────┐
│                       codex-core                                   │
│  ┌────────────────┐  ┌───────────────┐  ┌──────────────────────┐  │
│  │ Session         │  │ ThreadManager  │  │ GoalRuntime          │  │
│  │ (AI Turn 执行)  │  │ (Thread 注册表)│  │ (续期/预算管理)      │  │
│  └────────────────┘  └───────────────┘  └──────────────────────┘  │
│  ┌────────────────┐  ┌───────────────┐  ┌──────────────────────┐  │
│  │ ToolRegistry    │  │ ModelClient   │  │ ApprovalPolicy       │  │
│  │ (工具注册管理)  │  │ (AI 通信)     │  │ (权限审批策略)       │  │
│  └────────────────┘  └───────────────┘  └──────────────────────┘  │
└────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────── 服务/功能层 ─────────────────────────────────┐
│ codex-tools      │ 工具规格定义（shell/file/goal 等）                │
│ codex-memories   │ Memory 管理（Stage1/Stage2 摘要）                 │
│ codex-rollout    │ Rollout JSONL 写入/读取/Compaction                │
│ codex-skills     │ Skill（技能）系统，MCP 工具扩展                   │
│ codex-features   │ Feature Flag 系统（开关实验性功能）               │
│ codex-config     │ 配置文件解析（TOML）                              │
│ codex-sandboxing │ 沙箱抽象接口（统一跨平台）                         │
└──────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────── 平台/基础层 ─────────────────────────────────┐
│ codex-state      │ SQLite 状态持久化（thread/goal/job 等）            │
│ codex-linux-sandbox │ Linux bwrap 沙箱实现                           │
│ codex-windows-sandbox │ Windows Restricted Token 沙箱               │
│ codex-otel       │ OpenTelemetry 指标上报                            │
│ codex-login      │ 认证（ChatGPT OAuth/Device Code/API Key）         │
│ codex-keyring    │ 系统密钥链存储（令牌持久化）                        │
│ codex-file-search│ 文件搜索能力                                       │
│ codex-model-provider │ 多 AI 提供商支持（OpenAI/LM Studio/Ollama）   │
└──────────────────────────────────────────────────────────────────────┘
```

> 注：上图按职责分层，crate 名多为简称示意。实际工作区中密钥链 crate 名为
> `codex-keyring-store`（目录 `keyring-store/`）；Memory 拆成 `codex-memories-read`
> 与 `codex-memories-write` 两个 crate（目录 `memories/{read,write}/`）。完整成员
> 清单见根 `Cargo.toml` 的 `members`（baseline 共 114 个）。

---

## 2. 分层架构图（垂直）

```
════════════════════════════════════════════════════════════
                     用户/外部系统
════════════════════════════════════════════════════════════
         │                │               │
  终端 TUI           IDE 插件         其他 Agent
(codex-tui)      (app-server-client)  (MCP Client)
         │                │               │
════════════════════════════════════════════════════════════
                      入口/前端层
════════════════════════════════════════════════════════════
  ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌────────┐
  │  exec    │   │  TUI     │   │ app-srv  │   │  mcp   │
  │ （非交互）│   │ （全屏）  │   │ （v2 API）│   │ server │
  └────┬─────┘   └────┬─────┘   └────┬─────┘   └───┬────┘
       └───────────────┴──────────────┴─────────────┘
                              │
              App Server 协议（本地 IPC/HTTP）
                              │
════════════════════════════════════════════════════════════
                     核心 Agent 层（codex-core）
════════════════════════════════════════════════════════════
                              │
              ┌───────────────┴───────────────┐
              │        ThreadManager           │
              │    （全局线程注册/路由）         │
              └───────────────┬───────────────┘
                              │
         ┌────────────────────┼────────────────────┐
         │                    │                    │
    CodexThread A        CodexThread B        CodexThread C
    （主对话线程）         （子 Agent 线程）     （Side 线程）
         │
    ┌────┴──────────────────────────────┐
    │            Session                │
    │  ┌──────────┐  ┌──────────────┐  │
    │  │ModelClient│  │ToolRegistry  │  │
    │  │(AI 通信)  │  │(工具管理)    │  │
    │  └──────────┘  └──────────────┘  │
    │  ┌──────────┐  ┌──────────────┐  │
    │  │McpManager│  │ApprovalPolicy│  │
    │  │(MCP客户端)│  │(权限控制)    │  │
    │  └──────────┘  └──────────────┘  │
    │  ┌──────────────────────────────┐ │
    │  │       GoalRuntime             │ │
    │  │  (Goal 续期/预算追踪)         │ │
    │  └──────────────────────────────┘ │
    └───────────────────────────────────┘
                       │
════════════════════════════════════════════════════════════
                   AI 通信层
════════════════════════════════════════════════════════════
                       │
          ┌────────────┴────────────┐
          │     Responses API        │
          │    (HTTP/WebSocket)      │
          │   reqwest + tokio        │
          └────────────┬────────────┘
                       │
               OpenAI / ChatGPT
               GPT-4o / o3 / ...

════════════════════════════════════════════════════════════
                   持久化层
════════════════════════════════════════════════════════════
    ┌──────────────┐  ┌───────────┐  ┌──────────────────┐
    │  state.db    │  │  logs.db  │  │  rollout-*.jsonl  │
    │ (SQLite 主库)│  │ (日志库)  │  │  (完整对话历史)    │
    └──────────────┘  └───────────┘  └──────────────────┘
```

---

## 3. 核心数据流图

### 用户消息 → AI 响应 → 工具执行

```
用户输入文本
    │
    ▼
TUI::ChatWidget::submit_message()
    │  (AppEvent::SubmitMessage)
    ▼
App::handle_submit_message()
    │  (AppServerRequest::SubmitInput)
    ▼
AppServer::handle_submit_input()
    │
    ▼
CodexThread::submit(Op::UserTurnInput)
    │  (Tokio channel tx_sub)
    ▼
Session::run_turn()
    │
    ├── 1. 构建 input 数组（用户消息 + 历史 context）
    │
    ├── 2. 调用 Responses API
    │      POST /v1/responses
    │      model, tools, input, instructions
    │
    ├── 3. 流式接收 response
    │      Server-Sent Events / WebSocket chunks
    │
    ├── 4. 解析 tool_calls
    │      ┌─────────────────────────────────────┐
    │      │   tool_name = "shell"                │
    │      │   ↓                                  │
    │      │   ApprovalPolicy.check()             │
    │      │   ├── Auto approve → 直接执行         │
    │      │   └── Need approval → 发送事件给用户  │
    │      │       用户审批 → 执行 / 拒绝 → 跳过   │
    │      └─────────────────────────────────────┘
    │
    ├── 5. 执行工具
    │      ToolHandler::handle(invocation)
    │      shell → bwrap/seatbelt 沙箱执行
    │      file  → 受限文件系统访问
    │      goal  → GoalRuntime 状态更新
    │
    ├── 6. 工具结果 → 再次提交给 API（function_output）
    │
    └── 7. 最终文本响应 → 通过 rx_event 发送给前端
         Session → CodexThread → AppServer → TUI
```

### 事件流（双向通道）

```
Session ──────────────────────────────────► TUI / AppServer
   │                                               │
   │  Event 类型：                                  │
   │  - AgentMessage (AI 文本输出)                   │
   │  - TaskStarted/Completed/Aborted              │
   │  - ApprovalRequest (等待用户审批)               │
   │  - ContextLeftWindowEvent (Context 过长警告)   │
   │  - ThreadGoalUpdatedEvent (Goal 状态变更)      │
   │  - TurnFinished / TurnAborted                 │
   │                                               │
   ◄──────────────────────────────────────────────── │
   │                                               │
   │  Op 类型：                                    │
   │  - UserTurnInput (用户消息)                    │
   │  - ApprovalResponse (审批结果)                │
   │  - Interrupt (中断信号)                        │
```

---

## 4. 多运行模式对比

```
┌───────────────┬──────────────┬──────────────┬──────────────┬──────────────┐
│  维度          │  exec 模式   │  TUI 模式    │ app-server   │ mcp-server   │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  入口 crate   │ codex-exec   │ codex-tui    │ codex-app-   │ codex-mcp-   │
│               │              │              │ server       │ server       │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  交互方式     │ 非交互/脚本  │ 全屏终端 UI  │ HTTP/WS API  │ JSON-RPC MCP │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  使用场景     │ CI/CD 管道   │ 开发者日常   │ IDE 插件     │ 被其他 AI    │
│               │ 自动化脚本   │ 终端工作流   │ Web 前端     │ 调用工具     │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  审批处理     │ 自动（按策略）│ 交互弹窗     │ API 回调     │ MCP elicit   │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  会话持久化   │ 有（rollout）│ 有（rollout）│ 有（rollout）│ 有（rollout）│
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  Goal 支持    │ 有限         │ 完整         │ 完整         │ 有限         │
├───────────────┼──────────────┼──────────────┼──────────────┼──────────────┤
│  沙箱         │ 完整         │ 完整         │ 完整         │ 完整         │
└───────────────┴──────────────┴──────────────┴──────────────┴──────────────┘

TUI 与 app-server 的关系：
  TUI 进程内嵌 app-server（in-process），通过本地通道通信；
  IDE 插件连接外部 app-server（out-of-process），通过 Unix Domain Socket 或 HTTP。
```

---

## 5. MCP 集成架构图

```
Codex 在 MCP 生态中扮演双重角色：

╔═══════════════════════╗        ╔═══════════════════════╗
║   Codex as MCP Client  ║        ║  Codex as MCP Server   ║
╠═══════════════════════╣        ╠═══════════════════════╣
║                        ║        ║                        ║
║  Session               ║        ║  codex-mcp-server      ║
║  │                     ║        ║  │                     ║
║  └─ McpManager         ║        ║  ├─ 暴露 shell_exec   ║
║     │                  ║        ║  ├─ 暴露 file_read    ║
║     ├─ MCP Server A    ║        ║  ├─ 暴露 codex_chat   ║
║     │  (外部工具服务)   ║        ║  └─ 暴露 ...          ║
║     ├─ MCP Server B    ║        ║                        ║
║     └─ MCP Server C    ║        ║  接受来自其他 AI 的    ║
║                        ║        ║  工具调用（JSON-RPC）  ║
║  将 MCP 工具注入到      ║        ║                        ║
║  Responses API 的       ║        ╚═══════════════════════╝
║  tools 数组             ║                   ▲
╚═══════════════════════╝                    │
           │                         外部 AI Agent
           ▼                         (Claude / GPT 等)
    外部 MCP 工具服务
    (Filesystem/GitHub/...)

MCP 通信协议：
  JSON-RPC 2.0
  Transport: stdio / SSE / WebSocket
  Schema: 工具名 + inputSchema (JSON Schema)

Codex MCP Client 配置：
  ~/.codex/config.toml
  [mcp_servers]
  [mcp_servers.github]
  command = "npx"
  args = ["-y", "@modelcontextprotocol/server-github"]
  env = { GITHUB_TOKEN = "..." }

MCP Elicitation（工具审批路由）：
  Feature::ToolCallMcpElicitation 启用时 →
  MCP 工具的审批请求通过 MCP elicitation API 路由给用户，
  而非走内部审批弹窗。
```

---

## 6. 沙箱安全架构图

```
Codex 执行工具时使用平台特定沙箱：

┌─────────────────────────────────────────────────────────────────┐
│                    ApprovalPolicy 层                             │
│                                                                   │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ Suggest 模式 │  │ Auto 模式   │  │ Full Auto 模式          │  │
│  │（逐步确认）  │  │（安全自动）  │  │（全自动，限制解除）     │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    沙箱执行层（codex-sandboxing）                 │
│                                                                   │
│  macOS Seatbelt        Linux bwrap         Windows Restricted    │
│  (sandbox-exec)        (Bubblewrap)        Token                 │
│  │                     │                  │                      │
│  ├─ 允许读取当前目录    ├─ 独立 Mount NS   ├─ 受限 Token 执行     │
│  ├─ 禁止网络（可选）   ├─ 禁止网络        ├─ 禁止高权限操作      │
│  ├─ 禁止进程 fork      ├─ Tmpfs /tmp     └─ 仅白名单路径写入    │
│  └─ 禁止危险 syscall   └─ PID/UTS NS                            │
│                                                                   │
│  权限白名单示例：                                                  │
│  - read: ["${CWD}", "~/.config"]                                 │
│  - write: ["${CWD}"]                                             │
│  - exec: ["git", "node", "python3", ...]                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    实际命令执行                                    │
│                                                                   │
│  shell_exec: bash -c "<command>"                                 │
│  受沙箱保护，结果（stdout/stderr/exit_code）返回给 Session        │
│                                                                   │
│  超时控制: ApprovalPolicy 中配置 timeout_seconds                  │
│  并行限制: 同时最多 N 个工具调用                                   │
└─────────────────────────────────────────────────────────────────┘
```

---

## 7. Multi-Agent 父子树结构

```
Codex 支持 Agent 创建子 Agent（AgentControl 工具）：

                    ┌─────────────────┐
                    │   主 Thread A    │
                    │  (用户直接对话)  │
                    └────────┬────────┘
                             │ spawn_child()
                    ┌────────┴────────┐
                    │                 │
           ┌────────▼────────┐  ┌────▼──────────┐
           │  子 Thread B    │  │  子 Thread C   │
           │  (子任务 1)     │  │  (子任务 2)    │
           └────────┬────────┘  └───────────────┘
                    │ spawn_child()
           ┌────────┴────────┐
           │  子 Thread D    │
           │  (子子任务)     │
           └────────────────┘

数据库记录（thread_spawn_edges）：
  parent_thread_id → child_thread_id
  status: pending / running / complete / failed

子 Agent 特性：
  - 独立的 Session 和 ApprovalPolicy
  - 可继承父 Agent 的沙箱策略
  - 结果通过 AgentControl 工具返回给父 Agent
  - 支持并发（多个子 Agent 同时运行）
  - 支持 agent_jobs 批处理（CSV 输入 → 多行并发）
```

---

## 8. 认证系统架构图

```
Codex 支持四种认证方式：

┌─────────────────────────────────────────────────────────────────┐
│                      AuthManager                                 │
│                                                                   │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────┐ ┌────────────┐  │
│  │ ChatGPT OAuth│ │ Device Code  │ │ API Key  │ │  Agent     │  │
│  │    Flow      │ │   Flow       │ │  (直接)  │ │  Identity  │  │
│  └──────┬───────┘ └──────┬───────┘ └────┬─────┘ └─────┬──────┘  │
│         │                │              │              │          │
│         └────────────────┴──────────────┴──────────────┘          │
│                                  │                                │
│                    ┌─────────────▼──────────────┐                │
│                    │      Token 管理              │                │
│                    │  keyring-store (系统密钥链)   │                │
│                    │  ~/.codex/auth              │                │
│                    └──────────────────────────────┘                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                  Bearer Token / API Key
                              │
                              ▼
                    OpenAI Responses API

认证流程：
  1. ChatGPT OAuth：浏览器重定向 → 授权 → access_token/refresh_token
  2. Device Code：设备激活码 → 轮询 → token
  3. API Key：直接配置 OPENAI_API_KEY 环境变量
  4. Agent Identity：平台管理身份（Cloud 部署场景）

Token 刷新：
  access_token 过期时，AuthManager 使用 refresh_token 自动续期。
  chatgpt crate 处理具体的 OAuth 令牌刷新逻辑。
```

---

## 9. Memory/Compaction 数据流

```
Codex 的 Memory 系统分为两个阶段：

阶段一：Rollout Summarization (Stage1)
─────────────────────────────────────
  JSONL Rollout 文件（完整对话）
    │
    ▼
  BackgroundJob: stage1_summary
    │  （异步，不阻塞主流程）
    ▼
  LLM 生成摘要 (raw_memory + rollout_summary)
    │
    ▼
  state.db → stage1_outputs 表
    │  (thread_id → raw_memory, rollout_summary)
    ▼
  供下次会话启动时注入 context

阶段二：Context Compaction
──────────────────────────
  当前 context token 数接近模型上限
    │  ContextLeftWindowEvent 触发
    ▼
  Compaction 策略评估：
    ├── Local Compaction：
    │   在客户端裁剪 context（删除旧消息）
    └── Remote Compaction (RemoteCompactionV2)：
        发送完整 context 到服务端压缩
        返回压缩后的 summary 替换旧 context

Goal 与 Memory 的关系：
  Goal 的 continuation.md 模板会包含：
  - 当前目标（objective）
  - 已用 Token 和时间
  - "Avoid repeating work that is already done"
  这相当于一种轻量 Memory，让续期 Turn 不重复已完成的工作。
```

---

## 10. Goal 模式与 Thread 状态全景

```
Thread 生命周期与 Goal 状态的交互：

Thread 状态：
  Created → Active → Archived / Deleted

Goal 状态（thread_goals 表）：
  (无 Goal) → Active → Paused ↔ Active
                     → BudgetLimited → Active（增加预算后）
                     → Complete（终态）

Thread + Goal 全景图：

  ┌─────────────────────────────────────────────────────────┐
  │                       Thread                             │
  │                                                          │
  │  ● 用户消息 Turn 1                                       │
  │  ● AI 响应 Turn 1                                        │
  │  ● /goal 设置目标                                        │
  │  │                                                       │
  │  ▼  [Goal: Active]                                       │
  │                                                          │
  │  ● AI 自动续期 Turn 2 (continuation.md 注入)             │
  │  ● AI 自动续期 Turn 3                                     │
  │  ● AI 自动续期 Turn 4                                     │
  │  │                                                       │
  │  ▼  tokens_used → 接近 token_budget                      │
  │                                                          │
  │  ● 系统注入 budget_limit.md → [Goal: BudgetLimited]     │
  │  ● AI 收尾当前 Turn，报告进度                             │
  │  │                                                       │
  │  ▼  用户: /goal resume (或增加预算)                       │
  │                                                          │
  │  ▼  [Goal: Active]                                       │
  │                                                          │
  │  ● AI 续期 Turn 5，6，7...                                │
  │  ● AI 调用 update_goal(status="complete")                │
  │  │                                                       │
  │  ▼  [Goal: Complete]                                     │
  │                                                          │
  │  ● 正常对话继续（无续期）                                  │
  │                                                          │
  └─────────────────────────────────────────────────────────┘

  OTel 指标记录点：
  ├── Goal 创建：GOAL_CREATED_METRIC (+1)
  ├── Goal 完成：GOAL_COMPLETED_METRIC (+1)
  │              GOAL_TOKEN_COUNT_METRIC (tokens_used)
  │              GOAL_DURATION_SECONDS_METRIC (time_used_seconds)
  └── 预算限制：GOAL_BUDGET_LIMITED_METRIC (+1)
```

---

## 附：关键 Rust 文件索引

| 功能 | 文件路径 |
|------|---------|
| Goal 核心逻辑 | `codex-rs/core/src/goals.rs` |
| Goal 工具规格 | `codex-rs/core/src/tools/handlers/goal_spec.rs` |
| Goal 工具处理器 | `codex-rs/core/src/tools/handlers/goal.rs` |
| Goal TUI 操作 | `codex-rs/tui/src/app/thread_goal_actions.rs` |
| Goal 显示格式化 | `codex-rs/tui/src/goal_display.rs` |
| Goal 数据库模型 | `codex-rs/state/src/model/thread_goal.rs` |
| Goal 数据库迁移（现行库）| `codex-rs/state/goals_migrations/0001_thread_goals.sql` |
| Goal Feature 定义 | `codex-rs/features/src/lib.rs` (Feature::Goals) |
| Goal 续期模板 | `codex-rs/core/templates/goals/continuation.md` |
| Goal 预算限制模板 | `codex-rs/core/templates/goals/budget_limit.md` |
| Session 主循环 | `codex-rs/core/src/session/session.rs` |
| ThreadManager | `codex-rs/core/src/thread_manager.rs` |
| 斜杠命令解析 | `codex-rs/tui/src/slash_command.rs` |
| Feature Flag 系统 | `codex-rs/features/src/lib.rs` |
