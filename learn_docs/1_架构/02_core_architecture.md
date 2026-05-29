# Codex 核心架构设计文档

**文档编号：02**
**版本：1.0**
**适用代码库：`codex/codex-rs/`（Cargo workspace）**
**主语言：Rust（异步运行时：Tokio）**

---

## 目录

1. [项目概述](#1-项目概述)
2. [分层架构图](#2-分层架构图)
3. [组件关系图](#3-组件关系图)
4. [核心数据流时序图](#4-核心数据流时序图)
5. [AgentStatus 状态机](#5-agentstatus-状态机)
6. [工具调用流程图](#6-工具调用流程图)
7. [沙箱策略架构图](#7-沙箱策略架构图)
8. [各架构层详细说明](#8-各架构层详细说明)
   - [8.1 CLI 入口层](#81-cli-入口层)
   - [8.2 会话管理层](#82-会话管理层)
   - [8.3 Session 执行层](#83-session-执行层)
   - [8.4 AI 通信层](#84-ai-通信层)
   - [8.5 工具调用层](#85-工具调用层)
   - [8.6 沙箱层](#86-沙箱层)
   - [8.7 持久化层](#87-持久化层)
   - [8.8 协议层](#88-协议层)
   - [8.9 TUI 层](#89-tui-层)
   - [8.10 app-server 层](#810-app-server-层)
9. [异步通信机制](#9-异步通信机制)
10. [Crate 依赖拓扑](#10-crate-依赖拓扑)

---

## 1. 项目概述

Codex 是 OpenAI 开源的本地 AI 编码代理（Local AI Coding Agent）。
用户可以通过交互式终端（TUI）、无头脚本（exec）或 IDE 插件（app-server）
三种方式驱动 AI 在本地完成编码任务。

核心设计原则：

- **安全隔离**：所有 shell 命令均在沙箱中执行，支持三大平台差异化策略
- **异步解耦**：UI 层与 AI 会话层通过消息队列完全解耦，零阻塞
- **可持久化**：每次 session 的完整事件流持久化为 JSONL，支持回放与恢复
- **可扩展工具**：通过 MCP 协议动态注册外部工具，无需改动核心代码
- **多会话并发**：`ThreadManager` 同时管理多个独立 AI 对话线程

---

## 2. 分层架构图

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           用户交互前端层                                      │
│                                                                             │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────────────┐  │
│  │   TUI 层         │  │   exec 层        │  │   app-server 层          │  │
│  │  codex-tui       │  │  codex-exec      │  │  codex-app-server        │  │
│  │                  │  │                  │  │                          │  │
│  │ Ratatui 全屏 UI  │  │ 非交互批处理模式 │  │ JSON-RPC over WS/UDS     │  │
│  │ App struct       │  │ EventProcessor   │  │ MessageProcessor         │  │
│  │ 键盘/鼠标事件    │  │ JSONL/human 输出 │  │ IDE 插件连接点           │  │
│  └────────┬─────────┘  └────────┬─────────┘  └──────────┬───────────────┘  │
└───────────┼──────────────────────┼─────────────────────────┼────────────────┘
            │  async_channel       │  async_channel          │  async_channel
            │  (Op↓ / Event↑)     │  (Op↓ / Event↑)        │  (Op↓ / Event↑)
            ▼                      ▼                         ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           CLI 入口 & 调度层                                   │
│                          codex-cli / main.rs                                │
│                                                                             │
│   clap 命令解析 → 子命令分发（tui | exec | app-server | login | mcp | …）   │
└────────────────────────────────┬────────────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           会话管理层                                          │
│                    codex-core / ThreadManager                               │
│                                                                             │
│   ThreadManager                                                             │
│   ├── CodexThread #1  (thread_id, SessionSource, rollout_path)             │
│   ├── CodexThread #2                                                        │
│   └── CodexThread #N                                                        │
│        └── Codex (submit/next_event/agent_status)                          │
└────────────────────────────────┬────────────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          Session 执行层                                       │
│               codex-core / session/ {mod, session, turn, handlers}          │
│                                                                             │
│   Session (主 actor)                                                        │
│   ├── 配置管理（Config, ExecPolicyManager）                                 │
│   ├── 消息历史（MessageHistory, ContextManager）                            │
│   ├── Turn 驱动（turn.rs: 构建 prompt → 流式推理 → 工具调用循环）           │
│   ├── MCP 管理（McpConnectionManager）                                      │
│   ├── Hook 运行时（HookRuntime）                                            │
│   └── 协作模式（multi_agents.rs: CollaborationMode）                        │
└────────────────────────────────┬────────────────────────────────────────────┘
                                  │
                     ┌────────────┴────────────┐
                     ▼                         ▼
┌────────────────────────────┐   ┌─────────────────────────────────────────┐
│       AI 通信层             │   │              工具调用层                  │
│  codex-core / client.rs    │   │   codex-core / tools/                   │
│                            │   │                                         │
│  ModelClient               │   │  ToolRouter                             │
│  ├── HTTP SSE 传输          │   │  ├── ToolRegistry（内置工具注册表）      │
│  ├── WebSocket 传输         │   │  ├── McpToolHandler（外部 MCP 工具）     │
│  ├── 连接预热（prewarm）    │   │  └── DynamicTool（动态工具）            │
│  └── ModelClientSession    │   │                                         │
│      (per-turn 状态)       │   │  ToolOrchestrator                       │
│                            │   │  ├── 审批检查（AskForApproval policy）   │
│  OpenAI Responses API      │   │  ├── 沙箱选择（SandboxManager）         │
│  (SSE / WebSocket)         │   │  └── 重试逻辑（escalated sandbox）      │
└────────────────────────────┘   └──────────────────┬──────────────────────┘
                                                      │
                                                      ▼
                                  ┌───────────────────────────────────────┐
                                  │              沙箱层                    │
                                  │       codex-sandboxing/               │
                                  │                                       │
                                  │  SandboxManager                       │
                                  │  ├── macOS: Seatbelt (sandbox-exec)  │
                                  │  ├── Linux: Landlock + Bubblewrap    │
                                  │  └── Windows: Restricted Token       │
                                  └───────────────────────────────────────┘
                                                      │
                     ┌────────────────────────────────┘
                     ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            持久化层                                           │
│                    codex-rollout / {recorder, state_db}                     │
│                                                                             │
│  RolloutRecorder → JSONL（~/.codex/sessions/<Y/M/D>/rollout-<uuid>.jsonl）   │
│  StateDbHandle   → SQLite（state_5.sqlite：状态索引、线程元数据）            │
└─────────────────────────────────────────────────────────────────────────────┘

                         ══════════════════════
                         │    协议层（跨层共享）│
                         │  codex-protocol/    │
                         │  Op, Event, EventMsg│
                         │  SandboxPolicy      │
                         │  AgentStatus        │
                         │  RolloutItem        │
                         ══════════════════════
```

---

## 3. 组件关系图

```
                    ┌──────────────────────────────────────────────┐
                    │              codex-protocol (共享类型)         │
                    │  Op │ Event │ EventMsg │ AgentStatus          │
                    │  SandboxPolicy │ RolloutItem │ SessionMeta    │
                    └──────────────────────────────────────────────┘
                              ▲ 所有层均依赖此 crate

┌─────────────────────────────────────────────────────────────────────────────┐
│                         ThreadManager (codex-core)                          │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │  CodexThread                                                          │  │
│  │  ┌──────────────────────────────────────────────────────────────┐   │  │
│  │  │  Codex                                                        │   │  │
│  │  │  ├── submit(Op) ──────────────────────────────────────────┐  │   │  │
│  │  │  ├── next_event() → Event                                  │  │   │  │
│  │  │  ├── agent_status: watch::Sender<AgentStatus>             │  │   │  │
│  │  │  └── session: Arc<Session>                                │  │   │  │
│  │  │        │                                                  │  │   │  │
│  │  │        ▼                                                  │  │   │  │
│  │  │  ┌─────────────────────────────────────────┐             │  │   │  │
│  │  │  │  Session                                │             │  │   │  │
│  │  │  │  ├── mailbox: Mailbox<Op>               │             │  │   │  │
│  │  │  │  ├── event_tx: Sender<Event>            │─────────────┘  │   │  │
│  │  │  │  ├── config: Arc<Config>                │                │   │  │
│  │  │  │  ├── message_history: MessageHistory    │                │   │  │
│  │  │  │  ├── mcp: McpConnectionManager         │                │   │  │
│  │  │  │  ├── rollout: RolloutRecorder          │                │   │  │
│  │  │  │  └── run_turn() ──────────────────────────►turn.rs      │   │  │
│  │  │  └─────────────────────────────────────────┘             │   │  │
│  │  └──────────────────────────────────────────────────────────┘   │  │
│  └──────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│  turn.rs（Turn 执行引擎）                                          │
│                                                                  │
│  1. 构建 Prompt（系统提示 + ContextManager 注入 + 消息历史）       │
│  2. ModelClient::stream() ──────────────────────────────────────►│
│           │                                                      │
│           ▼  ResponseEvent（流式）                                │
│  3. handle_output_item_done() ─────────────────────────────────► │
│           │                                                      │
│           ├── 文本消息 → emit AgentMessage / AgentMessageDelta   │
│           ├── 推理摘要 → emit AgentReasoning                     │
│           └── FunctionCall → ToolRouter::route()                │
│                    │                                             │
│                    ▼                                             │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  ToolOrchestrator                                        │    │
│  │  ├── ExecApprovalRequirement 检查                        │    │
│  │  ├── AskForApproval 策略判断                             │    │
│  │  │    ├── 自动批准 → 直接执行                            │    │
│  │  │    └── 需要用户 → emit ExecApprovalRequest            │    │
│  │  │         └── 等待 Op::ExecApproval                    │    │
│  │  └── SandboxManager::transform() → SandboxCommand       │    │
│  │       └── 执行 → ToolRuntime::run()                     │    │
│  └─────────────────────────────────────────────────────────┘    │
│           │                                                      │
│           ▼ tool_output                                          │
│  4. 追加 function_call_output 到历史 → 下一轮推理               │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│  ModelClient（codex-core/client.rs）                              │
│                                                                  │
│  ModelClient（session 生命周期）                                  │
│  ├── auth: SharedAuthProvider                                    │
│  ├── provider_config: ApiProvider                                │
│  ├── ws_state: tokio::sync::watch （WebSocket 连接状态）          │
│  └── new_turn_session() → ModelClientSession                     │
│                                                                  │
│  ModelClientSession（per-turn）                                   │
│  ├── ws_connection: Option<ApiWebSocketConnection>               │
│  ├── turn_state_token: Option<String> （粘性路由）                │
│  ├── prewarm()    → 提前建立 WS 连接                             │
│  └── stream(req)  → AsyncStream<ResponseEvent>                   │
│         ├── 优先尝试 WebSocket                                    │
│         └── 降级 HTTP SSE（fallback）                            │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│  RolloutRecorder（codex-rollout/recorder.rs）                     │
│                                                                  │
│  ┌──────────────────────────────┐                               │
│  │  tokio::sync::mpsc channel   │                               │
│  │  (Sender 在 Session 侧)      │                               │
│  └──────────┬───────────────────┘                               │
│             ▼                                                    │
│  后台 tokio task（异步写磁盘）                                    │
│  ├── 序列化为 RolloutLine（timestamp + RolloutItem）             │
│  ├── append to .jsonl 文件                                       │
│  └── StateDbHandle → SQLite（线程索引、元数据）                  │
└──────────────────────────────────────────────────────────────────┘
```

---

## 4. 核心数据流时序图

```
用户         TUI/exec      CodexThread     Session/turn    ModelClient    OpenAI API    ToolRuntime    RolloutRecorder
 │             │               │               │               │              │               │               │
 │  键入消息   │               │               │               │              │               │               │
 │────────────►│               │               │               │              │               │               │
 │             │  submit(      │               │               │              │               │               │
 │             │  Op::UserTurn)│               │               │              │               │               │
 │             │──────────────►│               │               │              │               │               │
 │             │               │ mailbox.send  │               │               │               │              │
 │             │               │──────────────►│               │               │               │              │
 │             │               │               │ 构建 Prompt   │               │               │              │
 │             │               │               │ (系统提示+历史)│               │               │              │
 │             │               │               │───────────────►               │               │              │
 │             │               │               │               │ POST /responses│              │              │
 │             │               │               │               │──────────────►│               │              │
 │             │               │               │               │               │               │              │
 │             │               │               │  ◄── SSE/WS 流式响应 ─────────│               │              │
 │             │               │               │               │               │               │              │
 │             │◄── Event:AgentMessageDelta ───│               │               │               │              │
 │◄────────────│               │               │               │               │               │              │
 │  实时显示文字│               │               │               │               │               │              │
 │             │               │               │ FunctionCall  │               │               │              │
 │             │               │               │ (tool_name,   │               │               │              │
 │             │               │               │  arguments)   │               │               │              │
 │             │               │               │               │               │               │              │
 │             │◄── Event:ExecCommandBegin ────│               │               │               │              │
 │◄────────────│               │               │               │               │               │              │
 │  展示工具调用│               │               │               │               │               │              │
 │             │               │               │ 审批检查       │               │               │              │
 │             │               │               │──────────────────────────────────────────────►│              │
 │             │               │               │               │               │ SandboxManager│              │
 │             │               │               │               │               │ .transform()  │              │
 │             │               │               │               │               │──────────────►│              │
 │             │               │               │               │               │               │ 执行命令     │
 │             │               │               │               │               │               │──────►       │
 │             │               │               │               │               │               │◄──────       │
 │             │               │               │◄─ tool_output ────────────────────────────────│              │
 │             │               │               │               │               │               │              │
 │             │               │               │ 追加结果到历史  │               │               │              │
 │             │               │               │ → 下一轮推理  │               │               │              │
 │             │               │               │───────────────►               │               │              │
 │             │               │               │               │               │               │  写 JSONL    │
 │             │               │               │───────────────────────────────────────────────────────────►  │
 │             │               │               │               │               │               │              │
 │             │               │               │ TurnComplete  │               │               │              │
 │             │◄─── Event:TurnComplete ───────│               │               │               │              │
 │◄────────────│               │               │               │               │               │              │
 │  turn 结束  │               │               │               │               │               │              │
```

---

## 5. AgentStatus 状态机

```
                        ┌─────────────────┐
                        │   PendingInit   │  ← 初始状态（Session 刚创建）
                        └────────┬────────┘
                                 │
                    EventMsg::TurnStarted
                                 │
                                 ▼
              ┌──────────────────────────────────────┐
              │              Running                  │
              │  （正在进行 AI 推理 / 工具调用）        │
              └──┬──────────────┬───────────┬─────────┘
                 │              │           │
          TurnComplete    TurnAborted   Error Event
          (正常完成)      Interrupted  (推理/网络异常)
          BudgetLimited
                 │              │           │
                 ▼              ▼           ▼
         ┌───────────┐  ┌────────────┐  ┌─────────┐
         │ Completed │  │ Interrupted│  │ Errored │
         │ (含最后   │  │ (用户中断  │  │ (含错误 │
         │  消息摘要)│  │  或预算耗尽)│  │  消息)  │
         └───────────┘  └────────────┘  └─────────┘
                 ▲              │
                 │    下一轮可继续推理（非最终态）
                 └──────────────┘

         ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
         任意状态 + ShutdownComplete → Shutdown
         ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

                     ┌──────────┐
                     │ Shutdown │  ← 最终态（Session 已销毁）
                     └──────────┘

                     ┌──────────┐
                     │ NotFound │  ← 查询不存在的 thread_id 时返回
                     └──────────┘

状态转换规则（来自 core/src/agent/status.rs）：
  TurnStarted    → Running
  TurnComplete   → Completed(last_agent_message)
  TurnAborted    →
    reason=Interrupted    → Interrupted
    reason=BudgetLimited  → Interrupted
    reason=其他           → Errored(reason_str)
  Error          → Errored(message)
  ShutdownComplete → Shutdown

最终态（is_final）：Completed | Errored | Shutdown
非最终态：PendingInit | Running | Interrupted
```

---

## 6. 工具调用流程图

```
AI 模型输出 FunctionCall(tool_name, call_id, arguments_json)
                              │
                              ▼
                    ┌──────────────────┐
                    │   ToolRouter     │
                    │  router.rs       │
                    └────────┬─────────┘
                             │ 按 tool_name 路由
              ┌──────────────┼──────────────────┐
              ▼              ▼                  ▼
      ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
      │ 内置工具      │ │  MCP 工具     │ │  动态工具    │
      │ ToolRegistry │ │ McpToolHandler│ │ DynamicTool  │
      │              │ │              │ │              │
      │ shell        │ │ 调用外部 MCP  │ │ 用户/插件注入│
      │ apply_patch  │ │ server 进程  │ │ 的自定义工具  │
      │ list_dir     │ │              │ │              │
      │ read_file    │ │              │ │              │
      │ grep_files   │ │              │ │              │
      │ view_image   │ │              │ │              │
      │ web_search   │ │              │ │              │
      └──────┬───────┘ └──────┬───────┘ └──────┬───────┘
             │                │                │
             └────────────────┼────────────────┘
                              ▼
                  ┌────────────────────────┐
                  │   ToolOrchestrator     │
                  │   orchestrator.rs      │
                  └──────────┬─────────────┘
                             │
              ┌──────────────▼──────────────────┐
              │  Step 1: ExecApprovalRequirement  │
              │  判断是否需要执行审批              │
              │  ├── Never         → 无需审批     │
              │  ├── OnFailure     → 失败后才问   │
              │  ├── UnlessTrusted → 非受信则问   │
              │  ├── OnRequest     → 明确请求时问 │
              │  └── Granular      → 精细控制     │
              └──────────────┬──────────────────┘
                             │
              ┌──────────────▼──────────────────┐
              │  Step 2: ExecPolicy 规则评估      │
              │  execpolicy.rs                   │
              │  ├── 命令前缀白名单匹配           │
              │  ├── 危险命令检测                 │
              │  └── Decision::Allow / Deny /     │
              │       AskForApproval              │
              └──────────────┬──────────────────┘
                             │
           ┌─────────────────▼─────────────────┐
           │  Step 3: 需要审批 → emit Event      │
           │  ExecApprovalRequest               │
           │  ├── 等待 Op::ExecApproval          │
           │  │    ├── Approved → 继续           │
           │  │    ├── ApprovedForSession → 缓存 │
           │  │    └── Denied  → 拒绝执行        │
           │  └── Guardian 审批模式              │
           │       (管理员策略额外拦截层)          │
           └─────────────────┬─────────────────┘
                             │
              ┌──────────────▼──────────────────┐
              │  Step 4: SandboxManager          │
              │  选择沙箱类型                    │
              │  ├── SandboxType::MacosSeatbelt  │
              │  ├── SandboxType::LinuxSeccomp   │
              │  ├── SandboxType::WindowsRestrictedToken│
              │  └── SandboxType::None           │
              │  transform(request) → SandboxCommand│
              └──────────────┬──────────────────┘
                             │
              ┌──────────────▼──────────────────┐
              │  Step 5: ToolRuntime::run()      │
              │  spawn 子进程 / 调用 MCP         │
              │  收集 stdout/stderr/exit_code    │
              └──────────────┬──────────────────┘
                             │
              ┌──────────────▼──────────────────┐
              │  Step 6: 生成 function_call_output│
              │  → 追加到 ResponseItem 历史      │
              │  → emit ExecCommandEnd Event     │
              │  → RolloutRecorder 写盘          │
              └─────────────────────────────────┘
                             │
                             ▼
                   继续下一轮 AI 推理（loop）

─────────────────────────────────────────────────
Hook 系统（横切所有工具调用）：
  pre-tool-use hook   → 可修改/阻止工具调用
  permission-request  → 权限请求钩子
  post-tool-use hook  → 记录调用结果
─────────────────────────────────────────────────
```

---

## 7. 沙箱策略架构图（三平台）

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         SandboxPolicy（协议层定义）                           │
│                         codex-protocol/src/protocol.rs                      │
│                                                                             │
│  ┌──────────────────┐  ┌─────────────────┐  ┌────────────────────────────┐ │
│  │ DangerFullAccess │  │    ReadOnly      │  │     WorkspaceWrite         │ │
│  │ 无任何限制        │  │ 仅读，无写权限   │  │ 指定目录可写               │ │
│  │                  │  │ network_access:  │  │ writable_roots: Vec<...>   │ │
│  │ ⚠ 仅调试使用     │  │ Restricted|Enabled│  │ network_access: Restricted│ │
│  └──────────────────┘  └─────────────────┘  │ exclude_tmpdir_env_var     │ │
│                                             │ exclude_slash_tmp          │ │
│  ┌──────────────────┐                       └────────────────────────────┘ │
│  │ ExternalSandbox  │                                                       │
│  │ 由外部工具管控    │                                                       │
│  └──────────────────┘                                                       │
└───────────────────────────────┬────────────────────────────────────────────┘
                                │  SandboxManager::transform()
                                │  (codex-sandboxing/src/manager.rs)
                ┌───────────────┼───────────────────┐
                ▼               ▼                   ▼
┌───────────────────┐ ┌─────────────────┐ ┌─────────────────────────────────┐
│   macOS Seatbelt  │ │  Linux Sandbox  │ │       Windows Sandbox           │
│  (seatbelt.rs)    │ │  (landlock.rs   │ │   (windows-sandbox-rs/)         │
│                   │ │   + bwrap.rs)   │ │                                 │
│ /usr/bin/sandbox  │ │                 │ │                                 │
│ -exec -f policy   │ │                 │ │                                 │
│                   │ │                 │ │                                 │
│ .sbpl 策略文件:    │ │ 两层机制：       │ │ CreateRestrictedToken API       │
│ ┌───────────────┐ │ │ ┌─────────────┐ │ │ ├── 令牌降权（移除高权限 SID）   │
│ │seatbelt_base  │ │ │ │  Landlock   │ │ │ ├── 强制性完整性级别（Low/Med） │
│ │_policy.sbpl   │ │ │ │  内核 LSM   │ │ │ └── Job Object 限制            │
│ │               │ │ │ │  文件访问   │ │ │                                 │
│ │ 禁止:         │ │ │ │  路径规则   │ │ │ WindowsSandboxLevel:            │
│ │ • 网络（可选）│ │ │ └─────────────┘ │ │ ├── None  (无限制)               │
│ │ • 写系统目录  │ │ │ ┌─────────────┐ │ │ ├── Low   (低权限)               │
│ │ • 进程注入    │ │ │ │ Bubblewrap  │ │ │ └── High  (最严格)               │
│ │               │ │ │ │ (bwrap)     │ │ │                                 │
│ │seatbelt_      │ │ │ │ 用户命名空间 │ │ │ WSL1 注意：                     │
│ │network_policy │ │ │ │ 挂载命名空间 │ │ │ WSL1 下 Bubblewrap 不可用，     │
│ │.sbpl          │ │ │ │ 网络命名空间 │ │ │ 仅使用 Landlock                 │
│ └───────────────┘ │ │ └─────────────┘ │ │                                 │
│                   │ │                 │ │                                 │
│ 关键参数：         │ │ codex-linux-    │ │                                 │
│ • 可写根目录列表   │ │ sandbox (arg0)  │ │                                 │
│ • 受保护元数据名   │ │ 独立进程执行    │ │                                 │
│ • 网络代理端口     │ │                 │ │                                 │
└───────────────────┘ └─────────────────┘ └─────────────────────────────────┘

SandboxType 枚举：
  None                   → 无沙箱（DangerFullAccess 模式）
  MacosSeatbelt          → /usr/bin/sandbox-exec
  LinuxSeccomp           → codex-linux-sandbox（Landlock + Bubblewrap）
  WindowsRestrictedToken → CreateRestrictedToken + Job Object

PermissionProfile → SandboxPolicy 转换路径：
  PermissionProfile
  ├── file_system_sandbox_policy: FileSystemSandboxPolicy
  │    ├── ReadOnly（全局只读）
  │    ├── WorkspaceWrite（工作区可写）
  │    └── DangerFullAccess
  └── network_sandbox_policy: NetworkSandboxPolicy
       ├── Restricted（仅允许代理流量）
       └── Enabled（完整网络访问）
  → compatibility_sandbox_policy_for_permission_profile() → SandboxPolicy
```

---

## 8. 各架构层详细说明

### 8.1 CLI 入口层

**位置**：`codex-rs/cli/src/main.rs`
**crate**：`codex-cli`

CLI 层是整个系统的启动和分发入口，使用 `clap` 库解析命令行参数。

#### 主要子命令

| 子命令 | 描述 | 委托目标 |
|--------|------|----------|
| （默认）| 交互式 TUI 模式 | `codex-tui` |
| `exec`  | 非交互式批处理 | `codex-exec` |
| `app-server` | IDE 插件服务端 | `codex-app-server` |
| `login` | 认证管理 | `codex-login` |
| `mcp`  | MCP 工具调试 | `codex-mcp` |
| `marketplace` | 技能市场 | `codex-skills` |
| `landlock` / `seatbelt` / `windows` | 沙箱调试 | `codex-sandboxing` |

#### 关键设计

```
MultitoolCli（clap Parser）
├── config_overrides: CliConfigOverrides  ← --config / --set 参数
├── feature_toggles: FeatureToggles      ← --enable-feature / --disable-feature
├── interactive: TuiCli                  ← TUI 专属参数
└── subcommand: Option<Subcommand>
      ├── Exec(ExecCli)
      ├── AppServer(AppServerArgs)
      ├── Login(LoginCli)
      └── ...
```

`arg0_dispatch_or_else` 允许同一个二进制文件通过不同的进程名（argv[0]）
被调用为不同的工具，例如 `codex-linux-sandbox`，这是 Linux 沙箱的关键机制。

---

### 8.2 会话管理层

**位置**：`codex-rs/core/src/thread_manager.rs`
**主要类型**：`ThreadManager`、`CodexThread`

`ThreadManager` 是多会话并发的核心。它维护一个 `thread_id → CodexThread` 的映射表，
支持同时运行多个独立的 AI 对话线程。

#### ThreadManager 职责

- **创建线程**：`create_thread()` 初始化 `CodexThread`，分配唯一 `ThreadId`（UUID）
- **线程路由**：按 `thread_id` 将 `Op` 转发到对应线程
- **事件广播**：收集所有线程的 `Event`，通过 `broadcast::Sender` 向上层分发
- **会话恢复**：从 JSONL rollout 文件重建历史消息，恢复之前的对话上下文
- **Skills 监控**：`SkillsWatcher` 监听技能文件变化，热更新技能注入

#### CodexThread 职责

`CodexThread` 是单个 AI 对话线程的门面（Facade），封装了：

- `Codex`：双向通信的核心结构（submit Op / receive Event）
- `WatchRegistration`：文件监控注册（skills/config 变化）
- `out_of_band_elicitation_count`：跟踪并发 elicitation 请求数量
- Goal 运行时管理（暂停/继续/预算追踪）

```rust
pub struct CodexThread {
    pub(crate) codex: Codex,           // 核心通信结构
    pub(crate) session_source: SessionSource,
    rollout_path: Option<PathBuf>,     // JSONL 文件路径
    out_of_band_elicitation_count: Mutex<u64>,
    _watch_registration: WatchRegistration,
}
```

---

### 8.3 Session 执行层

**位置**：`codex-rs/core/src/session/`
**主要文件**：`session.rs`、`turn.rs`、`turn_context.rs`、`handlers.rs`

Session 执行层是 AI 推理循环的核心驱动。

#### Session 结构

`Session` 是一个长生命周期的 actor，通过内部 `Mailbox` 接收 `Op` 并串行处理：

```
Session（主循环）
├── mailbox: Mailbox<Submission>      ← 接收来自 Codex.submit() 的 Op
├── event_tx: Sender<Event>           → 向上层推送 Event
├── config: Arc<Config>              ← 当前配置（支持热更新）
├── message_history: MessageHistory  ← 完整对话历史（ResponseItem 列表）
├── context_manager: ContextManager  ← 系统提示片段注入
├── mcp: McpConnectionManager        ← MCP 服务器连接池
├── rollout: RolloutRecorder         ← 持久化写入器
├── exec_policy: ExecPolicyManager   ← 命令执行策略
├── hooks: Hooks                     ← 生命周期钩子
└── guardian_review_session          ← 管理员审批代理
```

#### Turn 执行流程（turn.rs）

每次用户提交 `Op::UserTurn` 都触发一次 **Turn**，其完整生命周期：

1. **Prompt 构建**：合并系统提示、环境上下文、技能注入、用户消息
2. **ModelClient::stream()**：向 OpenAI Responses API 发起请求
3. **流式处理循环**：
   - 文本 token → emit `AgentMessageDelta`
   - 推理内容 → emit `AgentReasoningDelta`
   - 工具调用完成 → 转交 `ToolRouter`
4. **工具结果汇总**：所有并行工具执行完毕后，将结果追加为 `function_call_output`
5. **继续推理**：带有工具结果的历史发送给模型，重复步骤 2-4
6. **Turn 结束**：emit `TurnComplete` / `TurnAborted`

#### 关键 Op 类型处理

| Op | 处理方式 |
|----|---------|
| `UserTurn` | 触发新的 AI turn |
| `ExecApproval` | 解除对应工具调用的阻塞等待 |
| `PatchApproval` | 解除 apply_patch 的审批等待 |
| `Interrupt` | 取消当前 turn（CancellationToken） |
| `Compact` | 触发上下文压缩 |
| `Undo` | 回滚最近 N 次文件变更 |
| `Shutdown` | 优雅关闭 Session |

---

### 8.4 AI 通信层

**位置**：`codex-rs/core/src/client.rs`
**主要类型**：`ModelClient`、`ModelClientSession`

AI 通信层负责与 OpenAI Responses API 建立连接并流式获取模型输出。

#### 双传输模式

```
ModelClientSession
├── 主传输：WebSocket（低延迟，支持 prewarm）
│    ├── ResponsesWebsocketClient
│    ├── 连接预热（prewarm）：turn 开始前建立连接
│    └── turn_state_token：粘性路由 header（x-codex-turn-state）
└── 降级传输：HTTP SSE（连接失败时自动切换）
     └── ReqwestTransport → POST /v1/responses
```

#### 关键设计

- **连接预热（Prewarm）**：在 Turn 开始前异步建立 WebSocket，减少首 token 延迟（TTFT）
- **粘性路由**：`x-codex-turn-state` header 确保同一 Turn 的多轮请求路由到同一后端
- **WS 状态管理**：`tokio::sync::watch` 在 session 级别广播 WebSocket 连接状态
- **认证注入**：`SharedAuthProvider` 自动处理 Bearer token 刷新
- **请求元数据**：`x-codex-turn-id`、`x-codex-session-id` 等 header 用于服务端追踪

#### ResponseEvent 类型

从模型流式返回的事件映射为 `ResponseEvent`：

| 事件 | 描述 |
|------|------|
| `output_item.done` | 完整输出项完成（文本/工具调用/推理） |
| `response.output_item.added` | 新输出项开始 |
| `response.created` | 新的 response 对象创建 |
| `response.completed` | 当前 response 完成（可能还有后续） |
| `error` | API 错误 |

---

### 8.5 工具调用层

**位置**：`codex-rs/core/src/tools/`
**主要文件**：`registry.rs`、`router.rs`、`orchestrator.rs`、`sandboxing.rs`

工具调用层负责解析 AI 的 FunctionCall 请求，路由到对应处理器，
执行审批和沙箱封装，最终返回结果。

#### ToolRegistry — 内置工具注册表

```
ToolRegistry
├── shell            → ShellToolHandler（bash 命令执行）
├── apply_patch      → ApplyPatchToolHandler（文件差异应用）
├── list_dir         → ListDirToolHandler（目录列表）
├── read_file        → ReadFileToolHandler（文件读取）
├── grep_files       → GrepFilesToolHandler（正则搜索）
├── view_image       → ViewImageToolHandler（图像查看）
├── web_search       → WebSearchToolHandler（联网搜索）
├── spawn_agent      → AgentToolHandler（多代理协作）
├── create_goal      → GoalToolHandler（目标追踪）
└── request_user_input → RequestUserInputToolHandler（请求用户输入）
```

#### ToolRouter — 路由分发

```rust
pub struct ToolRouter {
    registry: ToolRegistry,              // 内置工具
    specs: Vec<ConfiguredToolSpec>,      // 所有工具配置
    model_visible_specs: Vec<ToolSpec>,  // 暴露给模型的工具列表
    parallel_mcp_server_names: HashSet<String>, // 可并行的 MCP 服务器
}
```

路由逻辑：
1. 按 `tool_name` 在 `registry` 中查找内置处理器
2. 若未找到，在 `mcp_tools` 中查找对应 MCP 工具
3. 若为 `dynamic_tool`，使用动态工具处理器

#### ToolOrchestrator — 审批与沙箱编排

`ToolOrchestrator` 实现了工具执行的完整生命周期管理：

```
approval → sandbox_selection → attempt → retry_on_denial
```

关键特性：
- **审批缓存**：`ApprovalStore` 缓存已批准的命令前缀，避免重复询问
- **沙箱升级**：首次执行失败时可以用更严格的沙箱重试
- **网络审批**：支持延迟网络审批（`DeferredNetworkApproval`）
- **并行工具**：多个 MCP 工具调用可以并行执行

#### ToolRuntime Trait

所有工具处理器实现 `ToolRuntime<Req, Out>` trait：

```rust
trait ToolRuntime<Rq, Out> {
    fn network_approval_spec(&self, req: &Rq, ctx: &ToolCtx) -> NetworkApprovalSpec;
    async fn run(&mut self, req: &Rq, ctx: &ToolCtx, attempt: &SandboxAttempt) -> Result<Out, ToolError>;
}
```

---

### 8.6 沙箱层

**位置**：`codex-rs/sandboxing/`
**主要类型**：`SandboxManager`、`SandboxType`、`SandboxCommand`

沙箱层为所有 shell 命令提供操作系统级隔离，防止 AI 在未经授权的情况下
修改系统文件或访问网络。

#### SandboxManager

`SandboxManager` 将 `SandboxTransformRequest` 转换为 `SandboxCommand`：

```
SandboxTransformRequest
├── command: Vec<String>       ← 原始命令
├── cwd: AbsolutePathBuf      ← 工作目录
├── env: HashMap<String, String>
├── sandbox: SandboxType      ← 沙箱类型
└── permission_profile: PermissionProfile

    ↓ transform()

SandboxCommand
├── program: OsString          ← 可能被替换为 sandbox-exec 等
├── args: Vec<String>
├── cwd: AbsolutePathBuf
└── env: HashMap<String, String>
```

#### 各平台实现

**macOS Seatbelt**（`seatbelt.rs`）：
- 使用 Apple Sandbox `/usr/bin/sandbox-exec -f <policy>`
- 策略文件：`seatbelt_base_policy.sbpl` + `seatbelt_network_policy.sbpl`
- 支持精细化路径白名单：`(allow file-write* (subpath "/path/to/workspace"))`
- 受保护元数据路径（`.git`、`.codex` 等）独立控制

**Linux（Landlock + Bubblewrap）**（`landlock.rs` + `bwrap.rs`）：
- 将命令包装给 `codex-linux-sandbox` 进程（arg0 dispatch 机制）
- Bubblewrap（`bwrap`）：用户命名空间 + 挂载命名空间隔离
- Landlock LSM：内核级文件系统访问控制（路径+读写权限矩阵）
- WSL1 兼容：检测到 WSL1 时降级为仅 Landlock

**Windows Restricted Token**（`windows-sandbox-rs/`）：
- 调用 `CreateRestrictedToken` API 创建降权令牌
- 配合 Job Object 限制子进程权限
- `WindowsSandboxLevel`: None / Low / High 三档

#### 网络策略

```
NetworkSandboxPolicy
├── Restricted → 仅允许代理服务器流量（CODEX_PROXY_URL）
└── Enabled    → 完整网络访问

NetworkAccess（SandboxPolicy 内嵌）
├── Restricted（默认）
└── Enabled
```

---

### 8.7 持久化层

**位置**：`codex-rs/rollout/`
**主要类型**：`RolloutRecorder`、`StateDbHandle`

持久化层确保每次 AI session 的完整历史可被回放、恢复和审计。

#### 存储结构

```
~/.codex/
├── sessions/                                 ← 会话 rollout 根目录（按日期分层）
│   └── 2024/01/15/
│       └── rollout-2024-01-15T10-30-00-<uuid>.jsonl   ← 单次会话转录
├── archived_sessions/                        ← 归档会话
├── history.jsonl                             ← 全局消息历史（codex-message-history）
├── state_5.sqlite                            ← 主状态库（codex-state）
├── logs_2.sqlite                             ← 日志运行时 DB
├── goals_1.sqlite                            ← Goal 运行时 DB
└── memories_1.sqlite                         ← 记忆运行时 DB
```

> **路径说明**：rollout 文件位于 `~/.codex/sessions/YYYY/MM/DD/` 日期层级下，
> 文件名形如 `rollout-<ISO 时间戳>-<uuid>.jsonl`；归档会话在 `~/.codex/archived_sessions/`。
> SQLite 侧由 `codex-state` 管理 4 个运行时数据库（state/logs/goals/memories）。

#### JSONL 格式（RolloutLine）

每行是一个 `RolloutLine`，包含时间戳和 `RolloutItem`：

```
RolloutItem
├── SessionMeta  ← 第一行：session 元数据（model、cwd、source 等）
├── TurnContext  ← 每个 turn 开始时：配置快照
├── ResponseItem ← AI 响应项（消息、工具调用、工具结果）
├── EventMsg     ← 重要事件（TurnComplete 等）
└── Compacted    ← 上下文压缩记录
```

#### RolloutRecorder 内部机制

```
Session
  │ rollout_tx: tokio::sync::mpsc::Sender<RolloutCmd>
  │
  ▼
RolloutRecorder（后台 tokio task）
  ├── 接收 RolloutCmd
  ├── 序列化为 JSON（serde_json）
  ├── 追加写入 .jsonl 文件（tokio::io::AsyncWriteExt）
  └── StateDbHandle → SQLite
       ├── 线程索引（thread_id → file_path）
       ├── 最后活跃时间
       └── 线程名称
```

#### StateDbHandle — SQLite 状态数据库

`StateDbHandle` 提供快速线程查找和元数据存储，避免每次都扫描 JSONL 文件。

---

### 8.8 协议层

**位置**：`codex-rs/protocol/src/`
**主要文件**：`protocol.rs`

协议层定义了整个系统的公共类型，是各层之间的共享语言。

#### 核心类型

**Op（用户/系统 → Session 的消息）**

```
Op
├── UserTurn        ← 用户提交新的对话轮次（含消息、配置覆盖）
├── UserInput       ← 简化版用户输入（无配置覆盖）
├── ExecApproval    ← 用户对工具调用的审批决定
├── PatchApproval   ← 用户对 patch 的审批决定
├── Interrupt       ← 中断当前 turn
├── Compact         ← 触发上下文压缩
├── Shutdown        ← 关闭 Session
├── Undo            ← 撤销最近文件变更
├── AddToHistory    ← 注入历史消息
└── ...（共 30+ 种 Op）
```

**Event / EventMsg（Session → 上层 UI 的消息）**

```
EventMsg
├── TurnStarted           ← turn 开始
├── TurnComplete          ← turn 正常完成
├── AgentMessage          ← AI 完整消息
├── AgentMessageDelta     ← AI 流式 token
├── AgentReasoning        ← AI 推理内容
├── ExecCommandBegin      ← 工具调用开始
├── ExecCommandEnd        ← 工具调用结束
├── ExecCommandOutputDelta← 命令流式输出
├── ExecApprovalRequest   ← 请求用户审批
├── PatchApplyBegin/End   ← patch 应用事件
├── McpToolCallBegin/End  ← MCP 工具事件
├── TokenCount            ← token 用量统计
├── SessionConfigured     ← session 初始化完成
├── Error / Warning       ← 错误/警告
└── ShutdownComplete      ← 关闭完成
```

**SandboxPolicy（沙箱策略）**

见 [8.6 沙箱层](#86-沙箱层)。

**AgentStatus（代理状态）**

见 [第 5 节](#5-agentstatus-状态机)。

#### 协议层设计原则

- **Wire 格式稳定**：使用 `#[serde(rename_all = "camelCase")]` 保证跨版本兼容
- **零拷贝友好**：大量使用 `Arc<>` 避免克隆
- **向后兼容**：旧版字段使用 `#[serde(default)]` 处理缺失

---

### 8.9 TUI 层

**位置**：`codex-rs/tui/src/`
**主要类型**：`App`、`ChatWidget`

TUI 层基于 [Ratatui](https://github.com/ratatui-org/ratatui) 库实现全屏终端 UI，
提供流畅的实时 AI 交互体验。

#### 核心结构

```
App（主事件循环 actor）
├── chatwidget: ChatWidget        ← 聊天区域（主视图）
│    ├── history_cells: Vec<Cell> ← 历史消息渲染
│    ├── streaming: StreamingCell ← 当前流式输出
│    └── exec_cells: Vec<ExecCell>← 工具调用展示
├── bottom_pane: BottomPane       ← 底部输入区
│    ├── composer: ChatComposer   ← 文本输入框（支持多行）
│    └── footer: Footer           ← 状态栏/快捷键提示
├── app_server_session: AppServerSession ← 与 app-server 连接
└── notifications: NotificationsWidget  ← 通知浮层
```

#### 事件处理

TUI 层运行两个并发事件源：

1. **终端事件**：键盘/鼠标输入（crossterm）
2. **Codex 事件**：从 `CodexThread::next_event()` 接收

```
AppEvent
├── CrosstermEvent(KeyEvent | MouseEvent | Resize)
├── CodexEvent(Event)          ← 来自 Session 的 Event
├── AppCommand(AppCommand)     ← 内部命令（定时器、审批等）
└── Tick                       ← 动画帧更新
```

#### 渲染特性

- **Markdown 渲染**：支持代码块语法高亮、表格、列表
- **Diff 展示**：apply_patch 时显示 unified diff
- **流式更新**：token by token 渲染，带光标动画
- **Shimmer 效果**：AI 推理期间的加载动画
- **Snapshot 测试**：使用 `insta` 进行 UI 快照测试

---

### 8.10 app-server 层

**位置**：`codex-rs/app-server/src/`
**主要类型**：`MessageProcessor`、`TransportEvent`

app-server 层提供 IDE 插件（VSCode 扩展等）连接 Codex 的 RPC 接口，
基于 JSON-RPC 2.0 协议，支持 WebSocket 和 Unix Domain Socket 两种传输。

#### 通信架构

```
IDE 插件（VSCode Extension）
        │
        │  JSON-RPC 2.0
        │  WebSocket / Unix Socket
        ▼
app-server（codex-app-server）
├── transport/
│    ├── start_websocket_acceptor()    ← WebSocket 服务端
│    ├── start_control_socket_acceptor()← Unix Socket 服务端
│    └── start_stdio_connection()      ← stdio 模式（进程内）
│
├── MessageProcessor（主处理器）
│    ├── 解析 JSON-RPC Request
│    ├── 路由到对应 Handler
│    └── 序列化 JSON-RPC Response/Notification
│
├── codex_message_processor/
│    ├── thread/create        ← 创建新对话线程
│    ├── thread/read          ← 读取线程历史
│    ├── thread/submit        ← 提交用户输入
│    ├── thread/interrupt     ← 中断当前 turn
│    ├── config/read|write    ← 配置读写
│    └── app/list             ← 应用列表
│
└── ThreadManager（内嵌，复用 core 层）
     └── 管理所有 IDE 会话的 CodexThread
```

#### 协议版本

- **v1**：原始协议（维护中，不再新增 API）
- **v2**：当前活跃版本，所有新功能在此扩展
  - 请求字段：`camelCase`
  - 配置字段：`snake_case`（与 config.toml 保持一致）
  - 实验性 API：使用 `#[experimental]` 宏标注

#### 安全机制

`connection_rpc_gate.rs` 实现访问控制：
- Unix Socket 模式：同用户进程间可信通信
- WebSocket 模式：支持 Bearer token 验证
- 远程控制：`start_remote_control()` 限制远程访问权限

---

## 9. 异步通信机制

Codex 的异步架构基于 Tokio 运行时，使用多种通信原语实现层间解耦。

### 通信原语汇总

| 通信路径 | 原语 | 说明 |
|---------|------|------|
| UI → CodexThread | `async_channel::Sender<Submission>` | MPSC，支持多发送方 |
| CodexThread → UI | `async_channel::Receiver<Event>` | 事件消费 |
| Session 内部 mailbox | `async_channel` | Op 队列化处理 |
| ModelClient WS 状态 | `tokio::sync::watch` | 广播 WS 连接状态给多个观察者 |
| RolloutRecorder 写盘 | `tokio::sync::mpsc` | 异步写磁盘，不阻塞主循环 |
| ThreadManager 事件广播 | `tokio::sync::broadcast` | 多订阅者事件分发 |
| AgentStatus 订阅 | `tokio::sync::watch::Receiver` | app-server 订阅状态变化 |
| Turn 取消 | `tokio_util::sync::CancellationToken` | 级联取消所有子任务 |
| 审批等待 | `tokio::sync::oneshot` | 一次性响应（审批结果） |

### 背压与流控

- `async_channel` 通道设置容量上限，防止内存无限增长
- `RolloutRecorder` 的 mpsc 通道：写盘慢时在通道中积压，Session 不感知
- WebSocket prewarm 在 turn 开始前的空闲期执行，不占用推理窗口

### 并发模型

```
Tokio 多线程运行时（work-stealing scheduler）

主要 async 任务：
├── Session 主循环（per CodexThread）
├── ModelClient WebSocket 接收器
├── RolloutRecorder 写盘后台任务
├── MCP server 进程监控任务
├── Skills/Config 文件监控任务（notify）
├── TUI 事件循环
└── app-server connection handlers（per connection）
```

---

## 10. Crate 依赖拓扑

```
codex-cli
  ├── codex-tui
  │    ├── codex-core
  │    └── codex-app-server-client
  ├── codex-exec
  │    └── codex-core
  └── codex-app-server
       └── codex-core

codex-core（核心，应控制其膨胀）
  ├── codex-protocol（公共类型，无业务逻辑）
  ├── codex-sandboxing（沙箱）
  ├── codex-rollout（持久化）
  ├── codex-mcp（MCP 客户端）
  ├── codex-tools（工具定义）
  ├── codex-execpolicy（执行策略规则引擎）
  ├── codex-login（认证）
  ├── codex-config（配置加载）
  ├── codex-hooks（生命周期钩子）
  └── codex-api（HTTP/WS 客户端）

codex-protocol（零依赖的公共类型库）
  └── serde / uuid / chrono（仅外部库）

codex-sandboxing
  ├── codex-protocol
  └── codex-network-proxy

codex-rollout
  ├── codex-protocol
  └── codex-state（SQLite 状态层）

codex-app-server-protocol
  ├── codex-protocol
  └── typescript-type-def（TS 类型生成）
```

> **架构约束**：`codex-core` 已是最大的 crate，新功能应优先考虑放入其他现有 crate
> 或新建专用 crate，避免进一步膨胀 `codex-core`。

---

*文档由对源码逐层分析后生成，覆盖 `codex-rs/` workspace 下的核心 crate。*
*如需了解具体 API 细节，请参考各 crate 的 Rust 文档注释或 `docs/` 目录。*
