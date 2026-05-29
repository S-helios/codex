# 04 - Agent 主流程链路
> 文档编号：04 | 类型：流程分析 | 适合读者：想深入理解 AI Agent 工作机制的开发者

---

## 目录

1. [全局架构鸟瞰](#1-全局架构鸟瞰)
2. [核心类型速查](#2-核心类型速查)
3. [AgentStatus 状态机](#3-agentstatus-状态机)
4. [exec 模式完整流程](#4-exec-模式完整流程)
5. [一次 AI Turn 的内部执行时序图](#5-一次-ai-turn-的内部执行时序图)
6. [工具调用详细执行流程](#6-工具调用详细执行流程)
7. [审批策略决策流程](#7-审批策略决策流程)
8. [沙箱选择逻辑](#8-沙箱选择逻辑)
9. [多 Agent 父子关系](#9-多-agent-父子关系)
10. [Context Compaction 触发机制](#10-context-compaction-触发机制)
11. [完整数据流总览](#11-完整数据流总览)

---

## 1. 全局架构鸟瞰

Codex 是一个完全用 Rust 实现的 AI 编码 Agent 系统。整个系统可以分成三层：

```
┌─────────────────────────────────────────────────────────────────┐
│                        前端 / 入口层                              │
│                                                                   │
│   ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐   │
│   │ exec CLI │  │  TUI     │  │ MCP 服务  │  │  app-server  │   │
│   │(exec模式)│  │(终端UI)  │  │(工具暴露) │  │(v2 API 层)  │   │
│   └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬───────┘   │
└────────┼─────────────┼─────────────┼────────────────┼───────────┘
         │             │             │                │
         └─────────────┴─────────────┴────────────────┘
                               │
                    ┌──────────▼──────────┐
                    │   app-server 层      │
                    │  (in-process 或 IPC) │
                    └──────────┬──────────┘
                               │
┌──────────────────────────────▼────────────────────────────────────┐
│                        核心调度层                                    │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    ThreadManager                             │   │
│  │  (管理所有 CodexThread，是全局的线程注册表)                   │   │
│  │                                                              │   │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │   │
│  │  │ CodexThread  │  │ CodexThread  │  │ CodexThread  │ ...  │   │
│  │  │  (Thread A)  │  │  (Thread B)  │  │  (Thread C)  │      │   │
│  │  └──────┬───────┘  └──────────────┘  └──────────────┘      │   │
│  │         │                                                    │   │
│  │  ┌──────▼───────┐                                           │   │
│  │  │    Codex     │  ← 双向通道的句柄                          │   │
│  │  │ (tx_sub/     │    tx_sub: Op  → Session                  │   │
│  │  │  rx_event)   │    rx_event: Event ← Session              │   │
│  │  └──────┬───────┘                                           │   │
│  └─────────┼──────────────────────────────────────────────────-┘   │
│            │                                                         │
│  ┌─────────▼────────────────────────────────────────────────────┐   │
│  │                       Session                                 │   │
│  │  (实际执行 AI Turn 的核心 struct)                              │   │
│  │                                                               │   │
│  │  ┌──────────────────┐   ┌─────────────────────────────────┐  │   │
│  │  │  SessionServices  │   │      SessionConfiguration       │  │   │
│  │  │  - model_client   │   │  - approval_policy              │  │   │
│  │  │  - mcp_manager    │   │  - sandbox/permission profile   │  │   │
│  │  │  - skills_manager │   │  - model / reasoning_effort     │  │   │
│  │  │  - auth_manager   │   │  - cwd / collaboration_mode     │  │   │
│  │  │  - rollout_thread │   └─────────────────────────────────┘  │   │
│  │  └──────────────────┘                                          │   │
│  └───────────────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────────────-─┘
                               │
┌──────────────────────────────▼────────────────────────────────────┐
│                       AI 通信层                                      │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │                   ModelClientSession                          │  │
│  │  - stream() → 流式 SSE / WebSocket 请求 OpenAI API            │  │
│  │  - 处理 ResponseEvent 流                                       │  │
│  └──────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────┘
```

**关键设计思想**：Codex 使用 **双向 channel** 解耦前端与后端。
- 前端通过 `submit(Op)` 向 Session 发送操作
- Session 通过 `tx_event` 向前端广播 `Event`
- 两端完全异步，Session 内部有自己的 Tokio 任务循环

---

## 2. 核心类型速查

### 2.1 Op — 操作枚举（用户 → Agent）

`Op` 定义在 `codex-rs/protocol/src/protocol.rs`，是所有用户操作的枚举：

```
Op 枚举（关键变体，完整定义见 protocol.rs:529）
│
├── UserInput { items, environments, final_output_json_schema,
│              thread_settings, ... }
│      └── items: Vec<UserInput>（Text / LocalImage 等）
│          最常用的变体：一次性把「输入项 + 本回合环境 + 输出 JSON Schema
│          约束 + 持久化设置覆盖」打包提交。设置覆盖与回合启动复用同一条
│          提交队列(SQ)，杜绝「设置还没生效就开始回合」的竞态。
│          （注：旧版本独立的 UserTurn 变体已并入 UserInput）
│
├── ThreadSettings { thread_settings }
│      └── 只更新线程级设置、不启动回合（与 UserInput 共用提交队列）
│
├── Interrupt
│      └── 中断当前 Turn，但「不」杀后台终端进程；内核回以 TurnAborted
│
├── CleanBackgroundTerminals
│      └── 终止本线程所有后台 shell 进程
│
├── ExecApproval { id, turn_id, decision }
│      └── 用户对 shell 命令审批请求的回应（decision: ReviewDecision）
│
├── PatchApproval { id, decision }
│      └── 用户对 apply_patch 审批请求的回应
│
├── UserInputAnswer { id, response }
│      └── 用户对 request_user_input 工具的回答
│
├── Compact
│      └── 手动触发上下文压缩（生成对话摘要）
│
├── ThreadRollback { num_turns }
│      └── 从「内存上下文」丢弃最近 N 个用户轮（不碰磁盘，磁盘改动
│          由客户端自行撤销）——replay-to-rebuild 回滚入口（详见 ch.16）
│
├── Review { review_request }
│      └── 请求 AI 做一次代码评审
│
├── RunUserShellCommand { command }
│      └── 执行用户手动输入的一次性 shell 命令（"!cmd" 触发）
│
├── InterAgentCommunication { communication }
│      └── 多 Agent 间通信，记入 assistant 历史
│
└── Shutdown
       └── 优雅关闭整个 Session

（其余变体：Realtime* 实时语音流、ResolveElicitation（MCP 询问）、
  RequestPermissionsResponse、DynamicToolResponse、RefreshMcpServers、
  ReloadUserConfig、SetThreadMemoryMode、ApproveGuardianDeniedAction 等）
```

### 2.2 Event / EventMsg — 事件枚举（Agent → 用户）

`Event` 包含一个 `id` 和一个 `EventMsg`：

```
EventMsg 枚举（关键变体，按流程顺序）
│
├── SessionConfigured(...)     ← Session 初始化完成，携带 session_id / model / 审批策略等
│
├── TurnStarted(...)           ← 新 Turn 开始，携带 turn_id / 时间戳 / 上下文窗口大小
│
├── ReasoningContentDelta(...) ← AI 的推理过程（流式，仅在 reasoning 模型中）
│
├── AgentMessageContentDelta(...) ← AI 文字输出（流式增量）
│
├── AgentMessage(...)          ← AI 完整消息（Turn 结束时）
│
├── ExecCommandBegin(...)      ← shell 工具开始执行，携带命令 / cwd / call_id
│
├── ExecCommandOutputDelta(...)← shell 命令的 stdout/stderr 流式输出
│
├── ExecCommandEnd(...)        ← shell 工具执行完毕，携带退出码 / 完整输出
│
├── PatchApplyBegin(...)       ← apply_patch 工具开始，携带文件变更列表
│
├── PatchApplyEnd(...)         ← apply_patch 完毕，success/failed
│
├── ExecApprovalRequest(...)   ← 需要用户审批 shell 命令
│
├── McpToolCallBegin(...)      ← MCP 工具调用开始
│
├── McpToolCallEnd(...)        ← MCP 工具调用结束
│
├── TokenCount(...)            ← token 用量快照（每轮结束后）
│
├── TurnComplete(...)          ← Turn 正常完成，携带最后一条 AI 消息 / 时长
│
├── TurnAborted(...)           ← Turn 被中断或异常终止
│
├── Error(...)                 ← 错误事件
│
└── ShutdownComplete           ← Session 已关闭
```

### 2.3 核心 struct 关系图

```
ThreadManager
│  管理所有活跃的 thread，通过 HashMap<ThreadId, Arc<CodexThread>> 存储
│
└──▶ CodexThread
     │  一个对话线程的高层句柄
     │  - submit(op) → 向 Session 发送 Op
     │  - next_event() → 从 Session 接收 Event
     │  - agent_status() → 查询当前状态
     │
     └──▶ Codex  (struct，包含通道的两端)
          │  - tx_sub: Sender<Submission>   (发送 Op)
          │  - rx_event: Receiver<Event>    (接收 Event)
          │  - agent_status: watch::Receiver<AgentStatus>
          │  - session: Arc<Session>
          │
          └──▶ Session  (在独立 Tokio task 中运行)
               │  核心状态机，处理所有 Op，驱动 AI Turn
               │  - conversation: Arc<Mutex<ConversationHistory>>
               │  - active_turn: Mutex<Option<ActiveTurn>>
               │  - services: SessionServices
               │
               └──▶ ModelClientSession  (per-Turn 创建)
                    │  负责与 AI API 通信
                    │  - stream(prompt) → impl Stream<ResponseEvent>
                    │  支持 WebSocket 和 HTTPS 两种传输
                    │
                    └──▶ OpenAI Responses API (外部)
```

---

## 3. AgentStatus 状态机

`AgentStatus` 枚举定义在 `codex-rs/protocol/src/protocol.rs`，
状态转换逻辑在 `codex-rs/core/src/agent/status.rs` 的 `agent_status_from_event()` 函数中实现。

```
                    ┌─────────────────┐
                    │                 │
       Session 初始化│  PendingInit    │
            时的初态  │                 │
                    └────────┬────────┘
                             │
                    TurnStarted 事件
                             │
                             ▼
                    ┌─────────────────┐
                    │                 │◀─────────────────────────────┐
                    │    Running      │                              │
                    │                 │──────── TurnStarted ─────────┘
                    └────────┬────────┘  (下一次 Turn 开始时回到 Running)
                             │
              ┌──────────────┼────────────────────┐
              │              │                     │
   TurnComplete 事件   TurnAborted 事件        Error 事件
   (正常完成)           (被中断)                (发生错误)
              │              │                     │
              ▼              ▼                     ▼
   ┌──────────────┐  ┌──────────────────┐  ┌──────────────┐
   │              │  │  Interrupted     │  │              │
   │  Completed   │  │                  │  │   Errored    │
   │(含最后消息)  │  │ (可以被重新触发) │  │  (含错误信息)│
   └──────────────┘  └──────────────────┘  └──────────────┘
                                                    │
                             Shutdown 完成           │
                                    │               │
                                    ▼               │
                           ┌────────────────┐       │
                           │   Shutdown     │◀──────┘
                           │  (终态，不可恢复)│  ShutdownComplete 事件
                           └────────────────┘
```

**状态说明**：

| 状态 | 含义 | 是否终态 |
|---|---|---|
| `PendingInit` | Session 刚创建，等待初始化完成 | 否 |
| `Running` | 当前有活跃的 Turn 正在执行 | 否 |
| `Completed(msg)` | 最近一次 Turn 正常完成，携带最后一条 AI 消息 | 否（可被新 Turn 重置） |
| `Interrupted` | Turn 被用户中断 | 否（可被新 Turn 重置） |
| `Errored(msg)` | Turn 因错误终止 | 否（可被新 Turn 重置） |
| `Shutdown` | Session 已关闭 | **是** |

> **注意**：`Completed`、`Interrupted`、`Errored` 并非真正的终态——当用户提交新的 `UserInput` 时，Session 会重新进入 `Running`。只有 `Shutdown` 是不可恢复的真正终态。

---

## 4. exec 模式完整流程

exec 模式（`codex-rs/exec/src/lib.rs`）是最简洁的入口，适合理解 Agent 的整体工作流程。它通过命令行接受一个 prompt，驱动一次完整的 AI 对话，然后退出。

### 4.1 完整流程图

```
用户运行: codex exec "帮我修复这个 bug"
          │
          ▼
┌─────────────────────────────────────────────────────────────────┐
│  run_main() → 解析 CLI 参数                                      │
│  - 读取 ~/.codex/config.toml 与项目 .codex/config.toml           │
│  - 合并配置，确定 model / approval_policy / sandbox_policy        │
│  - 检测 stdin（是否管道输入），处理 prompt 编码（UTF-8 / UTF-16） │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  run_exec_session()                                              │
│                                                                  │
│  Step 1: 创建 EventProcessor                                     │
│  ├── json_mode=true  → EventProcessorWithJsonOutput             │
│  └── json_mode=false → EventProcessorWithHumanOutput            │
│      (负责把 Event 渲染成人类可读的终端输出)                       │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 2: 安全检查                                                 │
│  ├── 检查当前目录是否在 git 仓库内                                 │
│  │   (非 git 目录 + 未设 --skip-git-repo-check 则退出)            │
│  └── 若指定 --yolo 则跳过检查（用户自行承担风险）                  │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 3: 启动 InProcessAppServerClient                           │
│  ├── 在进程内启动 app-server（无需独立进程）                       │
│  └── 建立与 app-server 的通信通道                                 │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 4: 发送 ThreadStart / ThreadResume 请求                    │
│                                                                  │
│  ┌─────────────────────┐       ┌──────────────────────────────┐ │
│  │  新会话（默认）       │       │  恢复会话（--resume 子命令）   │ │
│  │  ClientRequest::    │       │  ClientRequest::ThreadResume  │ │
│  │  ThreadStart { ... }│       │  { thread_id, ... }          │ │
│  └──────────┬──────────┘       └───────────────┬──────────────┘ │
│             └──────────────┬──────────────────-┘                │
│                            │                                     │
│                            ▼                                     │
│              app-server 同步返回 SessionConfiguredEvent           │
│              ├── session_id（此次会话的唯一 ID）                   │
│              ├── model（实际使用的模型名）                         │
│              ├── approval_policy（审批策略）                       │
│              ├── cwd（工作目录）                                   │
│              └── rollout_path（会话持久化路径）                    │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 5: 打印配置摘要                                             │
│  EventProcessor::print_config_summary()                         │
│  输出: model / approval policy / sandbox mode / cwd / ...       │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 6: 注册 Ctrl+C 中断处理器                                  │
│  tokio::spawn(async { signal::ctrl_c().await → send interrupt }) │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 7: 发送 TurnStart 请求（提交用户 prompt）                   │
│                                                                  │
│  ClientRequest::TurnStart {                                      │
│      thread_id,                                                  │
│      params: TurnStartParams {                                   │
│          input: [UserInput::Text { text: prompt }],             │
│          cwd, approval_policy, effort, ...                       │
│      }                                                           │
│  }                                                               │
│                                                                  │
│  → app-server 返回 TurnStartResponse { turn.id }                 │
│    这个 task_id 用于后续过滤事件                                  │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 8: 主事件循环 loop { select! { ... } }                     │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  tokio::select!                                          │    │
│  │  ├── interrupt_rx.recv()   ← Ctrl+C 信号                │    │
│  │  │   → 发送 TurnInterrupt 请求                           │    │
│  │  └── client.next_event()  ← 来自 app-server 的事件       │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  收到 ServerNotification 后，按事件类型处理：                      │
│                                                                  │
│  事件类型                    处理动作                             │
│  ─────────────────────────  ──────────────────────────────────  │
│  TurnStarted                打印 "Codex is thinking..."          │
│  AgentMessageDelta          流式打印 AI 文字（增量）              │
│  AgentMessage               打印 AI 完整消息                      │
│  ExecCommandBegin           打印 "Running: <command>"            │
│  ExecCommandOutputDelta     流式打印命令输出                      │
│  ExecCommandEnd             打印执行结果（退出码）                 │
│  ExecApprovalRequest        弹出审批提示，等待用户输入             │
│  PatchApplyBegin            打印 "Applying patch..."             │
│  PatchApplyEnd              打印 patch 结果                       │
│  TokenCount                 更新 token 计数（内部）               │
│  TurnComplete               ← 跳出循环                           │
│  TurnAborted                ← 设置 error_seen=true，跳出循环     │
│  Error                      ← 设置 error_seen=true              │
└─────────────────────────┬───────────────────────────────────────┘
                          │  TurnComplete / TurnAborted
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│  Step 9: 关闭 client，打印最终摘要                                │
│  event_processor.print_final_output()                           │
│  输出:                                                           │
│  ├── Token 用量（输入 / 缓存 / 输出 / 总计）                      │
│  ├── 会话 ID（用于后续 --resume）                                 │
│  └── 恢复命令提示: codex exec --resume <session_id> "..."        │
│                                                                  │
│  若 error_seen=true → std::process::exit(1)                     │
│  否则              → exit(0)                                     │
└─────────────────────────────────────────────────────────────────┘
```

---

## 5. 一次 AI Turn 的内部执行时序图

当 `TurnStart` 请求到达 Session 后，Session 内部会调用 `run_turn()`（`codex-rs/core/src/session/turn.rs`）。下面是完整的内部时序图：

```
用户/前端          Session(主循环)      run_turn()函数     ModelClientSession    OpenAI API
    │                   │                   │                    │                 │
    │  Op::UserInput     │                   │                    │                 │
    │──────────────────▶│                   │                    │                 │
    │                   │  spawn async task │                    │                 │
    │                   │──────────────────▶│                    │                 │
    │                   │                   │                    │                 │
    │  Event::TurnStarted│                  │                    │                 │
    │◀──────────────────│                   │                    │                 │
    │                   │                   │                    │                 │
    │                   │    ┌──────────────────────────────────────────────────┐  │
    │                   │    │  Phase 1: Pre-Turn 准备阶段                      │  │
    │                   │    │                                                  │  │
    │                   │    │  1.1 run_pre_sampling_compact()                  │  │
    │                   │    │      检查 token 是否超阈值，若超则先压缩           │  │
    │                   │    │                                                  │  │
    │                   │    │  1.2 record_context_updates()                    │  │
    │                   │    │      将 TurnContextItem 写入对话历史              │  │
    │                   │    │      (记录 cwd / 审批策略 / 模型 / 时间等元数据)  │  │
    │                   │    │                                                  │  │
    │                   │    │  1.3 收集上下文                                  │  │
    │                   │    │      - 插件 (plugins)                            │  │
    │                   │    │      - MCP 工具列表                              │  │
    │                   │    │      - Connectors (App 集成)                     │  │
    │                   │    │      - 解析 @mention (skill / plugin / app)      │  │
    │                   │    │                                                  │  │
    │                   │    │  1.4 run_user_prompt_submit_hooks()              │  │
    │                   │    │      执行 pre-turn Hooks（user_prompt_submit）    │  │
    │                   │    │      若 Hook 返回 should_stop → 取消此 Turn      │  │
    │                   │    │                                                  │  │
    │                   │    │  1.5 record_user_prompt_and_emit_turn_item()     │  │
    │                   │    │      将用户消息写入对话历史                        │  │
    │                   │    │      发出 Event::UserMessage                     │  │
    │                   │    │                                                  │  │
    │                   │    │  1.6 注入 Skill / Plugin 内容                   │  │
    │                   │    │      build_skill_injections()                    │  │
    │                   │    │      将 skill 指令插入对话历史（对 AI 可见）       │  │
    │                   │    └──────────────────────────────────────────────────┘  │
    │                   │                   │                    │                 │
    │                   │    ┌──────────────────────────────────────────────────┐  │
    │                   │    │  Phase 2: 采样循环 (Sampling Loop)               │  │
    │                   │    │  loop {                                          │  │
    │                   │    │                                                  │  │
    │                   │    │  2.1 构建 ToolRouter                             │  │
    │                   │    │      built_tools() → ToolRouter                  │  │
    │                   │    │      注册所有可用工具:                            │  │
    │                   │    │        shell / apply_patch / view_image / ...    │  │
    │                   │    │        + MCP 工具 + Dynamic 工具                │  │
    │                   │    │                                                  │  │
    │                   │    │  2.2 构建 Prompt                                 │  │
    │                   │    │      build_prompt(history, tool_specs, ...)      │  │
    │                   │    │      Prompt {                                    │  │
    │                   │    │        input: 完整对话历史,                       │  │
    │                   │    │        base_instructions: 系统提示,               │  │
    │                   │    │        personality: 人格设定,                     │  │
    │                   │    │        tools: 工具定义列表,                       │  │
    │                   │    │        ...                                       │  │
    │                   │    │      }                                           │  │
    │                   │    └──────────────────────────────────────────────────┘  │
    │                   │                   │                    │                 │
    │                   │    ┌─────────────────────────────────────────────────┐   │
    │                   │    │  2.3 发起流式请求                               │   │
    │                   │    └─────────────────────────────────────────────────┘   │
    │                   │                   │  client_session.stream(prompt)  │    │
    │                   │                   │───────────────────────────────▶│    │
    │                   │                   │                    │  POST /responses│
    │                   │                   │                    │───────────────▶│
    │                   │                   │                    │                 │
    │                   │                   │                    │  SSE 流开始     │
    │                   │                   │                    │◀───────────────│
    │                   │                   │                    │                 │
    │                   │    ┌─────────────────────────────────────────────────┐   │
    │                   │    │  2.4 ResponseEvent 处理循环                     │   │
    │                   │    │  loop { stream.next() }                         │   │
    │                   │    └─────────────────────────────────────────────────┘   │
    │                   │                   │                    │                 │
    │  AgentReasoningDelta                  │◀── OutputItemAdded(Reasoning)──│    │
    │◀──────────────────────────────────────│    (AI 思维过程)               │    │
    │                   │                   │                    │                 │
    │  AgentMessageDelta │                  │◀── OutputTextDelta ────────────│    │
    │◀──────────────────────────────────────│    (AI 文字增量)               │    │
    │                   │                   │                    │                 │
    │                   │                   │◀── OutputItemDone(FunctionCall)─│   │
    │                   │                   │    AI 请求调用工具               │   │
    │                   │                   │                    │                 │
    │                   │    ┌─────────────────────────────────────────────────┐   │
    │                   │    │  2.5 工具调用处理（见第6节详细流程）              │   │
    │                   │    │  handle_output_item_done()                      │   │
    │                   │    │  → ToolRouter::build_tool_call()                │   │
    │                   │    │  → ToolCallRuntime::handle_tool_call()          │   │
    │                   │    │     (异步，放入 FuturesOrdered in_flight)        │   │
    │                   │    └─────────────────────────────────────────────────┘   │
    │                   │                   │                    │                 │
    │                   │                   │◀── ResponseDone ───────────────│    │
    │                   │                   │    流结束                        │    │
    │                   │                   │                    │                 │
    │                   │    ┌─────────────────────────────────────────────────┐   │
    │                   │    │  2.6 判断是否需要继续                            │   │
    │                   │    │  needs_follow_up = 有工具调用结果需要回传给 AI   │   │
    │                   │    │                                                  │   │
    │                   │    │  if needs_follow_up:                             │   │
    │                   │    │      等待工具执行完成 (drain in_flight)           │   │
    │                   │    │      将工具输出追加到对话历史                     │   │
    │                   │    │      → 继续 Sampling Loop (下一轮 AI 请求)        │   │
    │                   │    │  else:                                           │   │
    │                   │    │      → 退出 Sampling Loop                        │   │
    │                   │    └─────────────────────────────────────────────────┘   │
    │                   │                   │                    │                 │
    │                   │    ┌──────────────────────────────────────────────────┐  │
    │                   │    │  Phase 3: Post-Turn 清理阶段                     │  │
    │                   │    │                                                  │  │
    │                   │    │  3.1 运行 Stop Hooks                             │  │
    │                   │    │      hooks.run_stop(stop_request)                │  │
    │                   │    │      若 Hook 要求继续 → 注入 Hook prompt          │  │
    │                   │    │                      → 继续 Sampling Loop        │  │
    │                   │    │                                                  │  │
    │                   │    │  3.2 运行 after_agent Hooks                      │  │
    │                   │    │      hooks.dispatch(HookEvent::AfterAgent)       │  │
    │                   │    │      若 Hook 失败且设置 abort → 返回 Error        │  │
    │                   │    │                                                  │  │
    │                   │    │  3.3 检查 token 用量                             │  │
    │                   │    │      token_limit_reached && needs_follow_up?     │  │
    │                   │    │      → 触发 mid-turn auto compact                │  │
    │                   │    │                                                  │  │
    │                   │    │  3.4 Rollout 持久化                              │  │
    │                   │    │      将此 Turn 的所有 ResponseItem 写入磁盘       │  │
    │                   │    └──────────────────────────────────────────────────┘  │
    │                   │                   │                    │                 │
    │  Event::TurnComplete                  │                    │                 │
    │◀──────────────────────────────────────│                    │                 │
    │                   │                   │                    │                 │
```

---

## 6. 工具调用详细执行流程

当 AI 返回一个 `FunctionCall` 响应项时，Codex 会经过一套完整的审批+执行+沙箱流程。

### 6.1 工具调用总流程图

```
AI 返回 OutputItemDone(FunctionCall)
            │
            ▼
┌─────────────────────────────────┐
│  ToolRouter::build_tool_call()  │
│  ResponseItem → ToolCall：      │
│                                 │
│  FunctionCall  → ToolPayload::  │
│    Function（含 MCP 命名空间）  │
│                                 │
│  ToolSearchCall→ ToolPayload::  │
│    ToolSearch                   │
│                                 │
│  CustomToolCall→ ToolPayload::  │
│    Custom（Dynamic 动态工具）   │
└──────────────┬──────────────────┘
               │
               ▼
┌──────────────────────────────────────────────────┐
│  ToolCallRuntime::handle_tool_call()             │
│  (放入 FuturesOrdered 并发执行)                  │
│                                                  │
│  按 ToolName 分派到对应 Handler / Runtime：      │
│                                                  │
│  工具名(ToolName)     处理器                     │
│  ──────────────────  ───────────────────────     │
│  shell               ShellRuntime                │
│  unified_exec        UnifiedExecRuntime          │
│  apply_patch         ApplyPatchHandler           │
│  update_plan         PlanHandler                 │
│  view_image          ViewImageHandler            │
│  request_user_input  RequestUserInputHandler     │
│  request_permissions RequestPermissionsHandler   │
│  tool_search         ToolSearchHandler           │
│  <mcp__*>            McpHandler                  │
│  <dynamic>           DynamicToolHandler          │
└──────────────┬───────────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────────────────────┐
│              ToolOrchestrator::run()                        │
│  (codex-rs/core/src/tools/orchestrator.rs)                  │
│                                                             │
│  Step 1: 判断 ExecApprovalRequirement                       │
│  tool.exec_approval_requirement(req) 或默认规则              │
│  ├── Skip { bypass_sandbox }   → 无需审批，直接执行          │
│  ├── NeedsApproval { reason }  → 需要审批                    │
│  └── Forbidden { reason }      → 直接拒绝                    │
│                                                             │
│  Step 2: 如需审批，调用 request_approval()                   │
│  (见第7节：审批策略决策流程)                                   │
│                                                             │
│  Step 3: 选择沙箱类型并执行                                  │
│  sandbox.select_initial(...) → SandboxType                  │
│  ├── None                   (无沙箱)                        │
│  ├── MacosSeatbelt          (macOS sandbox-exec)            │
│  ├── LinuxSeccomp           (Linux seccomp + Landlock)      │
│  └── WindowsRestrictedToken (Windows 受限令牌)              │
│                                                             │
│  Step 4: run_attempt() → 执行工具                           │
│                                                             │
│  Step 5: 若沙箱拒绝(SandboxErr::Denied)                     │
│  └── 若 escalate_on_failure=true && 审批策略允许              │
│      → 再次请求用户审批（带 retry_reason）                    │
│      → 无沙箱重试                                             │
└──────────────────────────┬──────────────────────────────────┘
                           │ OrchestratorRunResult { output }
                           ▼
                ┌──────────────────────┐
                │  工具执行结果          │
                │  ResponseInputItem:: │
                │  FunctionCallOutput  │
                │  { call_id, output } │
                └──────────┬───────────┘
                           │
                           ▼
            追加到对话历史 → 触发下一轮 AI 采样
```

### 6.2 可用工具一览

```
┌──────────────────────────────────────────────────────────────┐
│                  Codex 内置工具（节选）                      │
│                                                              │
│  工具名              分类    描述                            │
│  ──────────────────  ────  ────────────────────────────────  │
│  shell               执行   执行 shell 命令（核心工具）      │
│  unified_exec        执行   交互式/长驻 shell 会话           │
│  apply_patch         文件   用 unified diff 修改文件         │
│  view_image          媒体   把本地图片加入上下文             │
│  update_plan         规划   维护多步骤执行计划               │
│  web_search          网络   联网搜索（Responses 托管工具）   │
│  request_user_input  交互   向用户提问并等待回答             │
│  request_permissions 权限   运行时请求额外权限               │
│  tool_search         发现   按需检索 / 加载工具              │
│                                                              │
│  + MCP 工具(mcp__*)         经 MCP 协议连接的服务器工具      │
│  + Dynamic 工具             运行时注册的自定义工具           │
│  + 多 Agent / Goal / 插件安装 等扩展类工具                   │
│                                                              │
│  注：无独立 read_file/list_dir/grep_files 工具——读文件 /     │
│      列目录 / 搜索均经 shell（cat / ls / grep 等）完成。     │
└──────────────────────────────────────────────────────────────┘
```

### 6.3 工具调用审批决策树

```
AI 发出工具调用请求
        │
        ▼
┌────────────────────────────────────────────────────────────┐
│  exec_approval_requirement(req)                             │
│  是否有工具自定义的 ExecApprovalRequirement?                  │
└────────────────┬──────────────┬────────────────────────────┘
                 │              │
         有自定义   │         无自定义│
                 │              │
                 │              ▼
                 │   default_exec_approval_requirement()
                 │   根据 AskForApproval 策略 + 沙箱类型决定
                 │
                 ▼
      ┌──────────────────────────────────────────────────┐
      │               ExecApprovalRequirement             │
      └──────┬────────────────┬────────────────┬─────────┘
             │                │                │
         Skip              NeedsApproval    Forbidden
             │                │                │
             ▼                ▼                ▼
      无需审批，          需要审批          直接拒绝
      直接执行               │             返回错误给 AI
             │               │
             │               ▼
             │   ┌───────────────────────────────────┐
             │   │  request_approval() 审批流程:       │
             │   │                                   │
             │   │  1. 先检查 PermissionRequest Hooks  │
             │   │     若 Hook 返回 Allow → 通过       │
             │   │     若 Hook 返回 Deny  → 拒绝       │
             │   │     若 Hook 无响应     → 下一步     │
             │   │                                   │
             │   │  2. Guardian 模式? (strict auto    │
             │   │     review 或 Guardian 守卫启用)    │
             │   │     是 → Guardian AI 自动审核       │
             │   │     否 → 向用户发送 Event::         │
             │   │           ExecApprovalRequest      │
             │   │           等待 Op::ExecApproval     │
             │   └──────────────┬────────────────────┘
             │                  │
             │                  ▼
             │        ReviewDecision 枚举
             │        │
             │        ├── Approved               → 通过
             │        ├── ApprovedForSession      → 通过 + 记忆
             │        ├── ApprovedExecpolicyAmendment → 通过 + 更新策略
             │        ├── NetworkPolicyAmendment  → 网络策略调整
             │        ├── Denied                 → 拒绝
             │        ├── TimedOut               → 超时拒绝
             │        └── Abort                  → 中止整个 Turn
             │
             └──────────────────┐
                                ▼
                        选择沙箱 + 执行工具
```

---

## 7. 审批策略决策流程

审批策略（`AskForApproval`）控制什么情况下需要用户同意工具调用。

### 7.1 策略枚举说明

```
AskForApproval 枚举（默认 = OnRequest，见 protocol.rs:829）
│
├── UnlessTrusted
│       只有"已知安全且仅读文件"的命令(is_safe_command)自动通过，
│       其余一律询问用户。最保守，wire 名 "untrusted"。
│
├── OnFailure（已废弃 DEPRECATED）
│       先在沙箱中运行；沙箱拒绝（SandboxErr::Denied）时再询问
│       用户是否允许无沙箱重试。官方建议改用 OnRequest / Never。
│
├── OnRequest（#[default] 默认）
│       由模型自行决定何时请求审批。实际效果：沙箱策略为受限
│       （Restricted）时询问用户；无限制（Full Access）时跳过。
│
├── Granular(GranularApprovalConfig)
│       与 OnRequest 同策略，但用布尔字段细粒度开关各类审批
│       （sandbox_approval / rules / skill_approval /
│        request_permissions / mcp_elicitations）；
│       关闭的类别直接拒绝、不弹给用户。
│
└── Never
        从不询问；失败立即返回给模型，绝不升级给用户（即 --yolo）。
        ⚠️ 危险：AI 可执行任何命令。

注：本枚举无 Always 变体；"总是询问"由 UnlessTrusted 表达，
    而 ReviewDecision::ApprovedForSession 的会话级记忆让用户
    不必重复批准相同命令。
```

### 7.2 审批策略决策矩阵

```
┌──────────────────┬─────────────────────────────────────────────────────┐
│ AskForApproval   │  文件系统沙箱 = Restricted  │  文件系统 = Full Access  │
├──────────────────┼─────────────────────────────┼─────────────────────────┤
│ Never            │  无审批，无沙箱（危险）        │  无审批，无沙箱（危险）   │
├──────────────────┼─────────────────────────────┼─────────────────────────┤
│ OnFailure        │  先跑沙箱，失败时询问          │  先跑沙箱，失败时询问     │
├──────────────────┼─────────────────────────────┼─────────────────────────┤
│ OnRequest        │  询问用户                     │  不询问（信任）           │
├──────────────────┼─────────────────────────────┼─────────────────────────┤
│ Granular         │  按工具类型细粒度决定          │  不询问（信任）           │
├──────────────────┼─────────────────────────────┼─────────────────────────┤
│ UnlessTrusted    │  总是询问（除非已记住）        │  总是询问（除非已记住）   │
└──────────────────┴─────────────────────────────┴─────────────────────────┘
```

### 7.3 审批记忆机制

```
用户回应 ReviewDecision::ApprovedForSession
          │
          ▼
    ApprovalStore (存在 SessionServices 中)
    HashMap<SerializedKey, ReviewDecision>
          │
          │  下次相同命令来临时
          ▼
    with_cached_approval() 检查缓存
    all(keys) 都是 ApprovedForSession?
    ├── 是 → 直接通过，不弹提示
    └── 否 → 正常走审批流程
```

---

## 8. 沙箱选择逻辑

沙箱保护用户系统不被 AI 错误地修改。`SandboxManager::select_initial()` 根据当前平台和策略选择沙箱类型。

### 8.1 沙箱选择决策树

```
SandboxManager::select_initial()
         │
         ▼
┌─────────────────────────────────────────────────┐
│  FileSystemSandboxPolicy (来自 PermissionProfile)│
└─────────┬──────────────────────┬────────────────┘
          │                      │
   kind = Restricted        kind = Unrestricted
          │                      │
          ▼                      ▼
 ┌────────────────────────────────┐  ┌──────────────────┐
 │ 选择平台对应的受限沙箱:        │  │  SandboxType::   │
 │   macOS   → MacosSeatbelt      │  │  None            │
 │             (sandbox-exec)     │  │  (无沙箱限制)    │
 │   Linux   → LinuxSeccomp       │  └──────────────────┘
 │             (seccomp+Landlock) │
 │   Windows → WindowsRestricted- │
 │             Token (受限令牌)   │
 │                                │
 └────────────────────────────────┘
```

### 8.2 沙箱策略（SandboxPolicy）说明

```
SandboxPolicy 枚举

┌─────────────────────────────────────────────────────────────────┐
│  ReadOnly (默认，最严格)                                         │
│  ├── 允许：读取整个文件系统                                       │
│  ├── 禁止：写入任何文件                                          │
│  └── 网络：可配置 NetworkAccess::Restricted / Enabled            │
│                                                                 │
│  WorkspaceWrite (常用)                                          │
│  ├── 允许：读取整个文件系统                                       │
│  ├── 允许：写入 writable_roots 中列出的目录（通常是 cwd）         │
│  ├── 保护：Git 元数据（.git/）不可写                             │
│  └── 可排除：/tmp 目录（可选）                                   │
│                                                                 │
│  DangerFullAccess (危险)                                        │
│  ├── 允许：读取 + 写入整个文件系统                                │
│  └── 完全没有沙箱限制                                            │
│                                                                 │
│  ExternalSandbox (特殊)                                         │
│  └── 由外部 sandbox 机制保护（如 devcontainer）                   │
└─────────────────────────────────────────────────────────────────┘
```

### 8.3 沙箱失败后的升级流程

```
                    工具在沙箱中运行
                          │
                          ▼
              SandboxErr::Denied? ──── 否 ──▶ 返回正常结果
                          │
                          是
                          ▼
              ┌───────────────────────────────┐
              │  tool.escalate_on_failure()   │
              │  该工具是否支持沙箱升级?        │
              └────────────┬──────────────────┘
                           │
                  是        │       否
                           │
                           ▼
              ┌────────────────────────────┐
              │  tool.wants_no_sandbox_    │
              │  approval(approval_policy) │
              │  审批策略是否允许无沙箱?     │
              │  - OnFailure  → 是         │
              │  - UnlessTrusted → 是      │
              │  - Never      → 否         │
              │  - OnRequest  → 否         │
              └──────────┬─────────────────┘
                         │
               是（允许升级）│  否（不允许升级）
                         │
                         ▼
              再次请求用户审批       返回沙箱拒绝错误
              retry_reason =         给 AI
              "沙箱拒绝原因..."
                         │
              用户批准?   │
                  是      │   否
                         │
                         ▼
              无沙箱重试执行工具
```

---

## 9. 多 Agent 父子关系

Codex 支持多 Agent 协作，一个 parent thread 可以 spawn 出 child threads，形成树状结构。

### 9.1 父子关系图

```
┌──────────────────────────────────────────────────────────────────┐
│                        ThreadManager                              │
│                                                                  │
│   ┌──────────────────────────────────────────────────────────┐   │
│   │  Root Thread (来自用户输入)                               │   │
│   │  SessionSource::Exec / Cli / VSCode                       │   │
│   │  thread_id: "thread-abc-001"                             │   │
│   │                                                           │   │
│   │  工具调用: spawn_thread(agent_path, prompt, ...)          │   │
│   │      │                                                    │   │
│   │      ▼                                                    │   │
│   │  ┌─────────────────────────────────────────────────────┐  │   │
│   │  │  Child Thread A                                     │  │   │
│   │  │  SessionSource::SubAgent(ThreadSpawn {              │  │   │
│   │  │    parent_thread_id: "thread-abc-001",             │  │   │
│   │  │    depth: 1,                                        │  │   │
│   │  │    agent_path: Some("./agents/reviewer"),           │  │   │
│   │  │    agent_nickname: Some("Alice"),                   │  │   │
│   │  │    agent_role: Some("code-reviewer"),               │  │   │
│   │  │  })                                                 │  │   │
│   │  │  thread_id: "thread-xyz-002"                        │  │   │
│   │  │                                                     │  │   │
│   │  │  可以继续 spawn:                                     │  │   │
│   │  │      │                                              │  │   │
│   │  │      ▼                                              │  │   │
│   │  │  ┌──────────────────────────────────────────────┐  │  │   │
│   │  │  │  Grandchild Thread (depth: 2)                │  │  │   │
│   │  │  │  受 thread_spawn_depth_limit 限制             │  │  │   │
│   │  │  │  超过深度限制时拒绝 spawn                      │  │  │   │
│   │  │  └──────────────────────────────────────────────┘  │  │   │
│   │  └─────────────────────────────────────────────────────┘  │   │
│   │                                                           │   │
│   │      ▼                                                    │   │
│   │  ┌─────────────────────────────────────────────────────┐  │   │
│   │  │  Child Thread B (另一个子 Agent)                    │  │   │
│   │  │  depth: 1                                           │  │   │
│   │  │  thread_id: "thread-pqr-003"                        │  │   │
│   │  └─────────────────────────────────────────────────────┘  │   │
│   └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### 9.2 多 Agent 消息传递流程

```
Parent Thread (Agent A)               Child Thread (Agent B)
      │                                       │
      │  发现需要子任务                         │
      │  (通过 spawn_thread 工具)              │
      │                                       │
      │  CollabAgentSpawnBegin ──────────────▶│
      │  (Event: 新 Agent 正在启动)            │  初始化 Session
      │                                       │  加载 agent_path 的 AGENTS.md
      │  CollabAgentSpawnEnd ◀────────────────│
      │  (Event: 新 Agent 已就绪)             │
      │                                       │
      │  发送任务描述 (InterAgentCommunication) │
      │  ─────────────────────────────────────▶│
      │                                       │  执行 AI Turn
      │                                       │  调用工具
      │  CollabAgentInteractionBegin ─────────▶│  生成结果
      │  (等待子 Agent 响应)                   │
      │                                       │
      │◀──────────────────────────────────────│
      │  CollabAgentInteractionEnd             │  通过 forward_child_completion_to_parent()
      │  (子 Agent 返回结果)                   │  将结果回传给父 Agent
      │                                       │
      │  将结果作为工具调用输出                 │
      │  继续自己的 AI Turn                    │
```

### 9.3 多 Agent 安全限制

```
多 Agent 安全机制

1. 深度限制
   thread_spawn_depth_limit (来自 Config)
   超过限制时 spawn 请求被拒绝
   防止无限递归

2. 权限继承
   子 Agent 的权限 ≤ 父 Agent 的权限
   子 Agent 的 approval_policy / sandbox_policy
   不能比父 Agent 更宽松

3. 会话隔离
   每个 thread 有独立的:
   - ConversationHistory（对话历史）
   - SessionServices（工具 / MCP 连接）
   - RolloutRecorder（持久化）

4. 资源追踪
   agent_graph_store 记录 thread 的父子关系图
   用于 UI 展示和安全审计
```

---

## 10. Context Compaction 触发机制

当对话历史超过模型上下文窗口限制时，Codex 会自动压缩历史以节省 token 并继续对话。

### 10.1 Compaction 触发时机图

```
                  Turn 生命周期中的 Compaction 检查点
                                │
          ┌─────────────────────┼─────────────────────┐
          │                     │                     │
          ▼                     ▼                     ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│  Pre-Turn        │  │  Mid-Turn        │  │  Manual          │
│  (每次 Turn 开始 │  │  (采样循环中)    │  │  (用户主动触发)  │
│   前检查)        │  │                  │  │  Op::Compact     │
│                  │  │                  │  │                  │
│ run_pre_sampling │  │ 采样完成后检查:  │  │ run_compact_task │
│ _compact()       │  │ token_limit_     │  │ ()               │
│                  │  │ reached &&       │  │                  │
│ 检查:            │  │ needs_follow_up  │  │                  │
│ total_tokens ≥   │  │                  │  │                  │
│ auto_compact_    │  │ run_auto_compact │  │                  │
│ limit?           │  │ (mid-turn)       │  │                  │
└────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘
         │                     │                     │
         └─────────────────────┴─────────────────────┘
                               │
                               ▼
                   触发 Compaction 流程
```

### 10.2 Compaction 内部执行流程

```
Compaction 开始
      │
      ▼
┌─────────────────────────────────────────────────────────────┐
│  should_use_remote_compact_task(provider)?                   │
│  ├── 是（支持远端压缩的 Provider）                            │
│  │   → run_inline_remote_auto_compact_task()                 │
│  └── 否（本地压缩，默认）                                     │
│      → run_inline_auto_compact_task()                        │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼ (本地压缩流程)
┌─────────────────────────────────────────────────────────────┐
│  Step 1: 收集当前对话历史                                     │
│  sess.clone_history()                                        │
│  → 克隆所有 ResponseItem                                     │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 2: 构建压缩 Prompt                                      │
│  Prompt {                                                    │
│    input: 完整历史 + 压缩指令,                                │
│    base_instructions: 系统提示(不含工具),                     │
│    ...                                                       │
│  }                                                           │
│  注：压缩请求不包含工具定义（让 AI 专注于摘要）                │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 3: 发起压缩请求到 AI API                                │
│  drain_to_completed() → client_session.stream(prompt)       │
│  AI 读取全部历史 → 生成摘要文本                               │
│                                                             │
│  若 ContextWindowExceeded:                                   │
│  history.remove_first_item() → 重试                         │
│  (最多删除到只剩 1 条消息)                                    │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 4: 构建压缩后的历史                                     │
│                                                             │
│  summary_text = SUMMARY_PREFIX + AI生成的摘要               │
│  user_messages = 从历史中收集的所有用户消息                    │
│                                                             │
│  build_compacted_history():                                  │
│  ┌──────────────────────────────────────────────────┐       │
│  │  新历史结构：                                      │       │
│  │  [CompactedItem { summary_text }]                 │       │
│  │  + [最近的用户消息]                                │       │
│  │  （丢弃所有中间的工具调用、AI 回复等）               │       │
│  └──────────────────────────────────────────────────┘       │
│                                                             │
│  若 BeforeLastUserMessage:                                   │
│  → 在最后一条用户消息前插入新的 TurnContextItem             │
│  （确保 cwd / model / 时间戳等元数据是最新的）               │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Step 5: 替换历史 + 持久化                                    │
│  sess.replace_compacted_history(new_history, ...)            │
│                                                             │
│  Rollout 文件中写入 RolloutItem::Compacted                   │
│  发出 EventMsg::ContextCompacted 事件                        │
│  发出警告："长线程可能降低 AI 准确性，建议开新线程"            │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
         Compaction 完成，继续执行原 Turn
```

### 10.3 压缩前后历史对比

```
压缩前 (可能有数百条记录):
┌────────────────────────────────────────────────────────┐
│  TurnContextItem (Turn 1 的元数据)                      │
│  UserMessage: "帮我重构这个函数"                         │
│  AssistantMessage: "好的，我来分析..."                   │
│  FunctionCall: shell("cat src/main.rs")                 │
│  FunctionCallOutput: "fn main() { ... }"               │
│  AssistantMessage: "我发现了几个问题..."                 │
│  FunctionCall: apply_patch("...")                       │
│  FunctionCallOutput: "Patch applied"                   │
│  AssistantMessage: "重构完成"                           │
│  ...（数百条中间记录）                                  │
│  TurnContextItem (Turn N 的元数据)                      │
│  UserMessage: "现在帮我写测试"                          │
└────────────────────────────────────────────────────────┘

压缩后（大幅精简）:
┌────────────────────────────────────────────────────────┐
│  CompactedItem:                                        │
│  "## 对话摘要                                           │
│   用户要求重构 src/main.rs。AI 分析了文件结构，发现      │
│   了 3 个问题：... 通过 apply_patch 完成了重构。          │
│   修改了 foo()、bar()、baz() 三个函数..."               │
│                                                        │
│  UserMessage: "帮我写测试"  ← 保留最近的用户消息         │
│  TurnContextItem (最新的元数据)                         │
└────────────────────────────────────────────────────────┘
```

---

## 11. 完整数据流总览

将所有流程整合到一张图中，展示从用户输入到 AI 输出的完整数据流：

```
用户输入
   │  "帮我修复 src/main.rs 中的 bug"
   │
   ▼
┌──────────────────────────────────────────────────────────────────┐
│  入口层 (exec / TUI / app-server)                                 │
│  Op::UserInput { items: [Text("帮我修复...")], ... }              │
└───────────────────────────┬──────────────────────────────────────┘
                            │  通过 channel 发送
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│  Session 主循环 (spawn_internal 中的 async loop)                  │
│                                                                  │
│  收到 Op::UserInput                                              │
│  → 创建 TurnContext（携带此次 Turn 的所有配置快照）               │
│  → spawn run_turn(sess, turn_context, input, ...)                │
└───────────────────────────┬──────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│  run_turn() 函数（在独立 Tokio Task 中）                           │
│                                                                  │
│  ①  Pre-turn Compaction（若需要）                                │
│  ②  记录上下文元数据（TurnContextItem）到历史                     │
│  ③  收集 Skill / Plugin / Connector 注入内容                     │
│  ④  运行 pre-turn Hooks（user_prompt_submit）                    │
│  ⑤  将用户消息写入历史                                           │
│  ⑥  注入 Skill / Plugin 内容到历史                              │
│                                                                  │
│  ↓ 进入 Sampling Loop                                           │
│                                                                  │
│  ⑦  built_tools() → 构建 ToolRouter                            │
│  ⑧  build_prompt(history, tools, ...) → Prompt struct           │
│  ⑨  client_session.stream(prompt) → Stream<ResponseEvent>       │
│                                                                  │
│  ↓ ResponseEvent 处理循环                                        │
│                                                                  │
│  ResponseEvent::OutputTextDelta                                  │
│  → 发出 EventMsg::AgentMessageDelta（流式）                       │
│                                                                  │
│  ResponseEvent::OutputItemDone(FunctionCall)                     │
│  → ToolRouter::build_tool_call()                                │
│  → ToolCallRuntime::handle_tool_call() (async, FuturesOrdered)  │
│       ↓                                                          │
│  ToolOrchestrator::run()                                         │
│       ① 判断审批需求                                            │
│       ② 如需审批 → 发出 ExecApprovalRequest → 等待用户回应       │
│       ③ SandboxManager 选择沙箱类型                              │
│       ④ 执行工具（shell / apply_patch / MCP / ...）              │
│       ⑤ 若沙箱拒绝 → 可升级重试                                 │
│       ↓                                                          │
│  返回 FunctionCallOutput（工具结果）→ 写入历史                    │
│                                                                  │
│  ResponseEvent::ResponseDone → 检查 needs_follow_up             │
│  有工具结果需要回传 → 继续 Sampling Loop（⑦）                   │
│  无需要继续        → 退出 Sampling Loop                          │
│                                                                  │
│  ⑩  运行 Stop Hook（after_turn）                                │
│  ⑪  运行 after_agent Hooks                                      │
│  ⑫  检查 token → 可能触发 mid-turn Compaction                   │
│  ⑬  发出 EventMsg::TurnComplete                                 │
└───────────────────────────┬──────────────────────────────────────┘
                            │  通过 tx_event channel 广播
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│  前端 / UI 层                                                     │
│                                                                  │
│  EventProcessor 接收并渲染事件：                                  │
│  AgentMessageDelta → 流式打印到终端                               │
│  ExecCommandBegin  → 打印 "Running: ..."                         │
│  ExecCommandEnd    → 打印执行结果                                 │
│  TurnComplete      → 退出事件循环                                 │
│                                                                  │
│  print_final_output():                                           │
│  Token 用量 / 会话 ID / resume 命令                              │
└──────────────────────────────────────────────────────────────────┘
```

---

## 附录：关键文件索引

| 概念 | 文件路径 |
|---|---|
| 核心类型定义 (Op/Event/EventMsg) | `codex-rs/protocol/src/protocol.rs` |
| exec 模式入口 | `codex-rs/exec/src/lib.rs` |
| ThreadManager（线程注册表） | `codex-rs/core/src/thread_manager.rs` |
| CodexThread（线程句柄） | `codex-rs/core/src/codex_thread.rs` |
| Codex struct（双向通道） | `codex-rs/core/src/session/mod.rs` |
| Session（核心状态机） | `codex-rs/core/src/session/session.rs` |
| run_turn（Turn 执行主函数） | `codex-rs/core/src/session/turn.rs` |
| TurnContext（Turn 配置快照） | `codex-rs/core/src/session/turn_context.rs` |
| ToolRouter（工具路由） | `codex-rs/core/src/tools/router.rs` |
| ToolOrchestrator（审批+沙箱） | `codex-rs/core/src/tools/orchestrator.rs` |
| AgentStatus 状态转换 | `codex-rs/core/src/agent/status.rs` |
| Context Compaction | `codex-rs/core/src/compact.rs` |
| 沙箱策略 | `codex-rs/sandboxing/src/` |
| SandboxPolicy 类型 | `codex-rs/protocol/src/protocol.rs` |
| AskForApproval 类型 | `codex-rs/protocol/src/protocol.rs` |
| ModelClientSession（AI 通信） | `codex-rs/core/src/client.rs` |

---

> **延伸阅读**：
> - `00_overview.md` — 项目背景与整体定位
> - `02_core_architecture.md` — 核心架构设计原则
> - `03_database_design.md` — Rollout 持久化存储设计
> - `codex-rs/config.md` — 配置项完整参考
> - `codex-rs/tui/styles.md` — TUI 样式规范
