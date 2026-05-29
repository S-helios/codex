# 05 - 核心组件设计
> 文档编号：05 | 类型：组件设计 | 适合读者：需要深入理解各核心 struct 设计的开发者

---

## 目录

1. [ThreadManager](#1-threadmanager)
2. [CodexThread](#2-codexthread)
3. [Codex](#3-codex)
4. [Session](#4-session)
5. [ModelClient / ModelClientSession](#5-modelclient--modelclientsession)
6. [ToolRouter](#6-toolrouter)
7. [ToolOrchestrator](#7-toolorchestrator)
8. [RolloutRecorder](#8-rolloutrecorder)
9. [StateDbHandle / StateRuntime](#9-statedbhandle--stateruntime)
10. [完整组件关系图](#10-完整组件关系图)

---

## 1. ThreadManager

**源文件：** `codex-rs/core/src/thread_manager.rs`

### 职责

`ThreadManager` 是整个 Codex 系统的**全局线程注册表**，负责管理所有 `CodexThread` 的生命周期。它是所有外部调用方（TUI、exec、app-server）创建、恢复、分支、关闭会话的唯一入口。它持有所有共享资源管理器的 `Arc` 引用，并在新线程创建时通过广播通道通知订阅方。

### 结构体层次

```
ThreadManager
└── state: Arc<ThreadManagerState>          ← 所有可共享状态
     ├── threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>
     ├── thread_created_tx: broadcast::Sender<ThreadId>
     ├── auth_manager: Arc<AuthManager>
     ├── models_manager: SharedModelsManager
     ├── environment_manager: Arc<EnvironmentManager>
     ├── skills_manager: Arc<SkillsManager>
     ├── plugins_manager: Arc<PluginsManager>
     ├── mcp_manager: Arc<McpManager>
     ├── skills_watcher: Arc<SkillsWatcher>
     ├── session_source: SessionSource
     ├── analytics_events_client: Option<AnalyticsEventsClient>
     └── ops_log: Option<SharedCapturedOps>  ← 测试模式专用
```

### 关键字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `threads` | `Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>` | 所有活跃线程的注册表，支持并发读写 |
| `thread_created_tx` | `broadcast::Sender<ThreadId>` | 新线程创建时广播通知，容量由 `THREAD_CREATED_CHANNEL_CAPACITY` 控制 |
| `auth_manager` | `Arc<AuthManager>` | 统一认证管理，所有线程共享同一实例 |
| `models_manager` | `SharedModelsManager` | 模型列表及元数据管理，支持在线刷新 |
| `environment_manager` | `Arc<EnvironmentManager>` | 管理 Turn 环境变量选择 |
| `skills_manager` | `Arc<SkillsManager>` | 技能（Skill）加载与管理，含 bundled skills |
| `plugins_manager` | `Arc<PluginsManager>` | MCP 插件生命周期管理 |
| `mcp_manager` | `Arc<McpManager>` | MCP 服务器连接管理 |
| `skills_watcher` | `Arc<SkillsWatcher>` | 监听文件系统上 skill 文件的变更 |
| `ops_log` | `Option<SharedCapturedOps>` | 测试模式下捕获所有提交的 Op，便于断言 |

### 关键方法

```
ThreadManager
├── new()                          → 初始化所有管理器，创建广播通道
├── start_thread()                 → 创建新线程（空白历史）
├── start_thread_with_tools()      → 创建带动态工具的新线程
├── start_thread_with_options()    → 完整选项创建，支持继承 shell/exec policy
├── resume_thread_from_rollout()   → 从 JSONL rollout 文件恢复线程
├── resume_thread_with_history()   → 从已加载历史恢复线程
├── fork_thread()                  → 按 ForkSnapshot 策略分支线程
├── fork_thread_from_history()     → 从已加载历史分支线程
├── get_thread()                   → 根据 ThreadId 查找活跃线程
├── remove_thread()                → 从注册表移除线程
├── subscribe_thread_created()     → 订阅新线程创建事件
├── shutdown_all_threads_bounded() → 有界时间内关闭所有线程
└── list_agent_subtree_thread_ids()→ 列出某线程及其所有子代线程 ID
```

### ForkSnapshot 枚举

```
ForkSnapshot
├── TruncateBeforeNthUserMessage(usize)
│    └── 截断到第 N 个用户消息之前，用于"从某个时间点重新开始"
└── Interrupted
     └── 保留截至最近中断点的历史，带 interrupted 标记边界
```

### 与其他组件的关系图

```
                    ┌─────────────────────────────────┐
                    │         ThreadManager            │
                    │  ┌───────────────────────────┐  │
                    │  │    ThreadManagerState      │  │
                    │  └───────────┬───────────────┘  │
                    └─────────────┼───────────────────┘
          ┌──────────────────────┬┼────────────────────────────┐
          │                      │                             │
          ▼                      ▼                             ▼
  ┌──────────────┐    ┌─────────────────────┐    ┌──────────────────────┐
  │ AuthManager  │    │   SkillsManager      │    │    McpManager        │
  │ (统一认证)   │    │ + PluginsManager     │    │ (MCP 服务器连接)     │
  └──────────────┘    └─────────────────────┘    └──────────────────────┘
          │                      │                             │
          └──────────────────────┴─────────────────────────────┘
                                 │  spawn / resume / fork
                                 ▼
                      ┌───────────────────┐
                      │   CodexThread     │  ← 注册到 threads 表
                      │ (Arc<CodexThread>)│
                      └───────────────────┘
                                 │ 广播
                                 ▼
                      broadcast::Receiver<ThreadId>
                      (TUI / app-server 监听新线程)
```

### 设计亮点

1. **状态内聚**：`ThreadManagerState` 用独立的 `Arc` 包装，可被 `AgentControl` 弱引用降级（`Arc::downgrade`），避免循环引用。
2. **广播通知**：`thread_created_tx` 采用 `broadcast` 语义，允许多个订阅方（TUI、app-server）同时收到新线程事件。
3. **测试模式隔离**：`ops_log` 字段仅在测试模式激活，通过 `set_thread_manager_test_mode_for_tests()` 开关，不影响生产代码路径。
4. **技能热加载**：`skills_watcher` 运行于独立任务，监听文件变更后自动通知 session 重新加载技能，无需重启线程。

---

## 2. CodexThread

**源文件：** `codex-rs/core/src/codex_thread.rs`

### 职责

`CodexThread` 是暴露给所有外部消费方（TUI、exec、app-server）的**高层会话句柄**。它将底层的 `Codex`（双通道包装层）与会话级元数据（rollout 路径、elicitation 计数、文件监听注册）整合在一起，是"一个对话"的完整表示。

### 结构体定义

```
pub struct CodexThread {
    codex: Codex,                                    // 内部双通道包装
    session_source: SessionSource,                   // 会话来源（cli/tui/mcp/exec/...）
    rollout_path: Option<PathBuf>,                   // JSONL 持久化文件路径
    out_of_band_elicitation_count: Mutex<u64>,       // OOB elicitation 并发计数
    _watch_registration: WatchRegistration,          // 文件监听注册（RAII 释放）
}
```

### 关键字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `codex` | `Codex` | 实际的双通道通信层，持有 Session Arc |
| `session_source` | `SessionSource` | 标识创建来源，影响日志和权限策略 |
| `rollout_path` | `Option<PathBuf>` | 指向 `rollout-*.jsonl` 文件，恢复时加载 |
| `out_of_band_elicitation_count` | `Mutex<u64>` | 记录当前挂起的 OOB elicitation 请求数量 |
| `_watch_registration` | `WatchRegistration` | Drop 时自动注销文件监听，防止内存泄漏 |

### 关键方法

| 方法 | 说明 |
|------|------|
| `submit(op)` | 向内部 Session 提交操作（UserTurn、Interrupt 等） |
| `next_event()` | 异步等待下一个 Event（TurnStarted、AgentMessage 等） |
| `shutdown_and_wait()` | 发送 Shutdown Op 并等待 session loop 退出 |
| `steer_input()` | 向当前活跃 turn 注入中途输入（steering），支持实时纠偏 |
| `inject_response_items()` | 直接向会话历史注入 ResponseItem（批处理结果导入等场景） |
| `config_snapshot()` | 获取当前 turn 配置的只读快照（model、审批策略、cwd 等） |
| `increment_out_of_band_elicitation_count()` | 增加 OOB 计数，若从 0→1 则暂停 agent turn |
| `decrement_out_of_band_elicitation_count()` | 减少 OOB 计数，若降至 0 则恢复 agent turn |
| `guardian_trunk_rollout_path()` | 获取 Guardian Review 主干的 rollout 路径 |
| `token_usage_info()` | 读取当前线程缓存的 Token 用量快照 |

### Out-of-Band Elicitation 机制

这是 `CodexThread` 最具特色的设计之一，用于支持 MCP Elicitation 协议：

```
用户触发 MCP Elicitation 请求
           │
           ▼
increment_out_of_band_elicitation_count()
    count: 0 → 1
    当 was_zero == true 时：
           │
           ▼
    session.set_out_of_band_elicitation_pause_state(true)
           │
           ▼
    Session 内部暂停 agent turn 的工具执行
    (通过 watch::Sender<bool> 广播暂停信号)
           │
           ◆ ← 外部系统处理 elicitation（等待用户响应）
           │
decrement_out_of_band_elicitation_count()
    count: 1 → 0
    当 now_zero == true 时：
           │
           ▼
    session.set_out_of_band_elicitation_pause_state(false)
           │
           ▼
    agent turn 恢复执行
```

**设计要点**：
- 支持**多个并发** elicitation 请求（`count > 1` 时不重复暂停）
- 计数溢出时返回 `CodexErr::Fatal`，防止状态不一致
- 使用 `Mutex<u64>` 而非 `AtomicU64`，保证 `was_zero` 检查与计数更新的原子性

---

## 3. Codex

**源文件：** `codex-rs/core/src/session/mod.rs`

### 职责

`Codex` 是 `CodexThread` 与底层 `Session` 之间的**双向通道中间层**。它持有发送 Op 的 `tx_sub` 和接收 Event 的 `rx_event`，是实现"异步提交 → 异步事件流"模式的核心。

### 结构体定义

```
pub struct Codex {
    tx_sub: Sender<Submission>,              // Op 发送通道（→ session loop）
    rx_event: Receiver<Event>,              // Event 接收通道（← session loop）
    agent_status: watch::Receiver<AgentStatus>, // 实时 agent 状态订阅
    session: Arc<Session>,                   // 底层 Session 的直接引用
    session_loop_termination: SessionLoopTermination, // Session loop 退出 Future
}
```

### 关键字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `tx_sub` | `Sender<Submission>` | 有界通道（容量 512），向 session loop 发送 Op |
| `rx_event` | `Receiver<Event>` | 从 session loop 接收输出事件 |
| `agent_status` | `watch::Receiver<AgentStatus>` | 轮询 agent 当前状态，无需订阅事件流 |
| `session` | `Arc<Session>` | 可直接访问 Session 的方法（不通过通道） |
| `session_loop_termination` | `Shared<BoxFuture<'static, ()>>` | 可被多个 caller `.clone().await`，等待 loop 退出 |

### 双向通道架构图

```
┌─────────────────────────────────────────────────────────────┐
│                      外部调用方                               │
│      (CodexThread / app-server / exec / TUI)                 │
└──────────────┬──────────────────────────────┬───────────────┘
               │  submit(Op)                  │  next_event()
               │                              │
               ▼                              ▲
  tx_sub: Sender<Submission>        rx_event: Receiver<Event>
  (capacity = 512)                  (无界，由 session emit)
               │                              │
               │                              │
  ─ ─ ─ ─ ─ ─ ─│─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─│─ ─ ─ ─ ─ ─ ─ ─
               │    Codex 边界                │
  ─ ─ ─ ─ ─ ─ ─│─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─│─ ─ ─ ─ ─ ─ ─ ─
               │                              │
               ▼                              │
  ┌────────────────────────────────────────────────────────┐
  │                  session loop (Tokio task)              │
  │                                                        │
  │  loop {                                                │
  │    sub = rx_sub.recv().await          ← 接收 Submission│
  │    match sub.op {                                      │
  │      Op::UserTurn { .. } → run_turn() → emit Events ──┤
  │      Op::Interrupt       → cancel current turn        │
  │      Op::Shutdown        → break                      │
  │      ...                                              │
  │    }                                                   │
  │  }                                                     │
  └────────────────────────────────────────────────────────┘
               │
               ▼
  session_loop_termination: Shared<BoxFuture>
  (shutdown_and_wait() 等待此 Future 完成)
```

### 关键方法

| 方法 | 行为 |
|------|------|
| `submit(op)` | 生成 UUID v7 作为 submission ID，包装为 `Submission` 后发送 |
| `submit_with_trace(op, trace)` | 附带 W3C Trace Context 的提交，用于分布式追踪 |
| `submit_with_id(sub)` | 使用调用方指定的 ID（稀有情况），自动附加当前 span 的 trace |
| `next_event()` | 等待 `rx_event`，channel 关闭时返回 `CodexErr::InternalAgentDied` |
| `shutdown_and_wait()` | 先提交 `Op::Shutdown`，再等待 `session_loop_termination` Future |
| `steer_input()` | 直接委托给 `session.steer_input()`，绕过 Op 通道 |

---

## 4. Session

**源文件：** `codex-rs/core/src/session/session.rs`、`core/src/session/mod.rs`

### 职责

`Session` 是 AI 对话的**核心执行引擎**，管理会话状态、工具路由、历史记录、MCP 服务器生命周期，以及 Guardian Review 等高级功能。整个 AI turn 的编排都发生在 Session 内部。

### 核心结构体

```
pub(crate) struct Session {
    conversation_id: ThreadId,                          // 全局唯一的对话 ID
    tx_event: Sender<Event>,                            // 向 Codex 发送事件
    agent_status: watch::Sender<AgentStatus>,           // 广播 agent 状态变更
    out_of_band_elicitation_paused: watch::Sender<bool>,// OOB 暂停信号
    state: Mutex<SessionState>,                         // 所有可变会话状态
    managed_network_proxy_refresh_lock: Semaphore,      // 代理重建串行化锁
    features: ManagedFeatures,                          // Feature flag 集合（会话期不变）
    pending_mcp_server_refresh_config: Mutex<Option<McpServerRefreshConfig>>,
    conversation: Arc<RealtimeConversationManager>,     // 实时对话管理
    active_turn: Mutex<Option<ActiveTurn>>,             // 当前活跃 turn 状态
    mailbox: Mailbox,                                   // 子 agent 消息邮箱
    mailbox_rx: Mutex<MailboxReceiver>,
    idle_pending_input: Mutex<Vec<ResponseInputItem>>,  // 无活跃 turn 时的待处理输入
    goal_runtime: GoalRuntimeState,                     // Goal 运行时状态
    guardian_review_session: GuardianReviewSessionManager, // Guardian Review 管理器
    services: SessionServices,                          // 外部服务集合
    next_internal_sub_id: AtomicU64,                    // 内部 submission ID 生成器
}
```

### SessionConfiguration（会话配置）

```
SessionConfiguration
├── provider: ModelProviderInfo          // "openai" / "openrouter" / "ollama" 等
├── collaboration_mode: CollaborationMode // Auto / Code / Ask 等模式
├── model_reasoning_summary: Option<...> // 推理摘要配置
├── service_tier: Option<ServiceTier>    // API 服务等级
├── developer_instructions: Option<String>
├── user_instructions: Option<String>
├── base_instructions: String            // 系统基础 prompt
├── approval_policy: Constrained<AskForApproval>
├── permission_profile: Constrained<PermissionProfile>
├── cwd: AbsolutePathBuf                 // 工作目录（沙箱基准）
├── codex_home: AbsolutePathBuf          // Codex 状态目录
├── environments: Vec<TurnEnvironmentSelection>
└── dynamic_tools: Vec<DynamicToolSpec>  // 动态注册的工具
```

### Session 主循环（session loop）

Session 的驱动通过 `Codex::spawn_internal()` 中的 Tokio task 实现：

```
┌──────────────────────────────────────────────────────────────────┐
│                      session loop (Tokio task)                   │
│                                                                  │
│  loop {                                                          │
│    ┌──────────────────────────────────────────────────────────┐  │
│    │  sub = rx_sub.recv().await   // 等待 Op                  │  │
│    └───────────────────┬──────────────────────────────────────┘  │
│                        │                                         │
│           ┌────────────┼─────────────────┐                       │
│           ▼            ▼                 ▼                       │
│    Op::UserTurn   Op::Interrupt    Op::Shutdown                  │
│         │         取消当前 turn         break                    │
│         │                                                        │
│         ▼                                                        │
│    run_turn(sess, turn_ctx, input, ...)                          │
│         │                                                        │
│         ├─→ build_prompt() 构建完整历史 prompt                   │
│         │                                                        │
│         ├─→ ModelClientSession::stream()  调用 AI API            │
│         │         │                                              │
│         │         ├── SSE 流式返回 ResponseItem                  │
│         │         └── WebSocket 双向连接                         │
│         │                                                        │
│         ├─→ 处理 FunctionCall → ToolRouter::build_tool_call()   │
│         │         │                                              │
│         │         └─→ ToolOrchestrator::run() 执行工具           │
│         │                                                        │
│         └─→ emit Events → tx_event                               │
│  }                                                               │
└──────────────────────────────────────────────────────────────────┘
```

### Guardian Review 机制

`guardian_review_session` 是一个特殊的代码审查子系统：

```
主 Session（agent turn 执行代码操作）
         │
         │  发现需要 Guardian 审查
         ▼
  spawn_review_thread()
         │
         ├─ 创建独立的 TurnContext（使用 review_model）
         ├─ 禁用 WebSearch、ViewImage 等工具
         ├─ 构建 review prompt（描述将要执行的操作）
         │
         ▼
  专用 review session（async task）
         │
         ├─ 调用 AI API 做安全审查
         ├─ 返回 ReviewDecision（Approved / Denied / TimedOut）
         │
         ▼
  主 Session 根据决定继续或中止工具执行
```

**设计意图**：将代码执行的安全审查与主 agent turn 解耦，review 可以使用不同的（更专注的）模型，同时不阻塞主 session 的状态机。

---

## 5. ModelClient / ModelClientSession

**源文件：** `codex-rs/core/src/client.rs`

### 职责

`ModelClient` 负责与 OpenAI Responses API 通信，支持 HTTP SSE 和 WebSocket 两种传输方式。`ModelClientSession` 是每个 AI turn 创建的**一次性流式会话**。

### 结构体层次

```
ModelClient
└── state: Arc<ModelClientState>
     ├── conversation_id: ThreadId           // 当前 thread 的唯一 ID
     ├── window_generation: AtomicU64        // 窗口代数（用于 WebSocket 粘性路由）
     ├── installation_id: String             // 客户端安装 ID（遥测）
     ├── provider: SharedModelProvider       // 实际的 API 提供方
     ├── auth_env_telemetry: AuthEnvTelemetry
     ├── session_source: SessionSource
     ├── model_verbosity: Option<VerbosityConfig>
     ├── enable_request_compression: bool
     ├── include_timing_metrics: bool
     ├── beta_features_header: Option<String>
     ├── disable_websockets: AtomicBool      // WebSocket 降级标志
     └── cached_websocket_session: StdMutex<WebsocketSession>  // WS 复用缓存

ModelClientSession（每 turn 新建）
├── client: ModelClient                      // 共享的 session-scoped 配置
├── websocket_session: WebsocketSession      // 本 turn 的 WS 连接状态
│    ├── connection: Option<ApiWebSocketConnection>
│    ├── last_request: Option<ResponsesApiRequest>  // 用于增量请求对比
│    ├── last_response_rx: Option<oneshot::Receiver<LastResponse>>
│    └── connection_reused: StdMutex<bool>
└── turn_state: Arc<OnceLock<String>>        // x-codex-turn-state 粘性路由令牌
```

### HTTP SSE vs WebSocket 请求流程对比

```
┌──────────────────────────────────────────────────────────────────────┐
│                    HTTP SSE 路径（降级/兜底）                         │
│                                                                      │
│  ModelClientSession::stream()                                        │
│         │                                                            │
│         └─→ stream_responses_api()                                   │
│                  │                                                   │
│                  ├─ 构建 ResponsesApiRequest                         │
│                  ├─ POST /v1/responses                               │
│                  │    (每次 turn 都是全量 HTTP 请求)                  │
│                  │                                                   │
│                  ├─ 服务器返回 SSE 事件流                            │
│                  │  response.created  →  EventStream                │
│                  │  response.delta    →  EventStream                │
│                  │  response.done     →  EventStream                │
│                  │                                                   │
│                  ├─ 若收到 401: handle_unauthorized() → 重试         │
│                  └─ map_response_stream() → ResponseStream           │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────┐
│                    WebSocket 路径（低延迟首选）                       │
│                                                                      │
│  ModelClientSession::stream()                                        │
│    └─[responses_websocket_enabled()?]─→ stream_responses_websocket() │
│                  │                                                   │
│                  ├─ websocket_connection()                           │
│                  │   ├─ 复用已有连接（cached_websocket_session）      │
│                  │   └─ 或 connect_websocket() 新建连接              │
│                  │        └─ WSS wss://api.openai.com/v1/responses   │
│                  │                                                   │
│                  ├─ prepare_websocket_request()                      │
│                  │   ├─ 首次 turn: 完整请求体                        │
│                  │   └─ 连续 turn: 增量差异（last_request 对比）      │
│                  │                                                   │
│                  ├─ 发送 JSON 消息                                   │
│                  │                                                   │
│                  ├─ 接收 WS 消息流                                   │
│                  │  response.created  →  parse → Event              │
│                  │  response.delta    →  parse → Event              │
│                  │  response.done     →  parse → Event              │
│                  │                                                   │
│                  ├─ 若 WS 失败: WebsocketStreamOutcome::FallbackToHttp│
│                  │   └─ try_switch_fallback_transport() 永久降级      │
│                  └─ 返回 ResponseStream                              │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘

关键区别：
  HTTP SSE    ──  每次新建 TCP 连接，全量发送历史
  WebSocket   ──  连接复用，增量发送 delta，延迟更低
```

### WebSocket 粘性路由机制

```
Turn 1: POST WS 连接
  服务器响应头: x-codex-turn-state: <token>
  客户端: turn_state.set(token)  → OnceLock，只写一次
                │
Turn 2: 复用 WS 连接
  请求头: x-codex-turn-state: <token>  ← 从 OnceLock 读取
  服务器: 路由到同一后端实例
```

### 关键方法

| 方法 | 层级 | 说明 |
|------|------|------|
| `ModelClient::new()` | session 级 | 初始化 session 范围的客户端，不做网络 I/O |
| `ModelClient::new_session()` | session 级 | 为当前 turn 创建新的 `ModelClientSession`，复用 WS 缓存 |
| `ModelClientSession::stream()` | turn 级 | 主入口：优先 WS，失败降级 SSE |
| `stream_responses_api()` | turn 级 | HTTP SSE 路径 |
| `stream_responses_websocket()` | turn 级 | WebSocket 路径 |
| `prewarm_websocket()` | turn 级 | 预热 WS 连接（预建连接，减少首次延迟） |
| `try_switch_fallback_transport()` | turn 级 | 永久降级到 HTTP（本 session 剩余所有 turn） |

---

## 6. ToolRouter

**源文件：** `codex-rs/core/src/tools/router.rs`

### 职责

`ToolRouter` 是工具调用的**路由层**，负责解析 AI 返回的 `ResponseItem`（FunctionCall、LocalShellCall、CustomToolCall 等），将其转换为统一的 `ToolCall` 结构，并通过 `ToolRegistry` 分发到对应的处理器。

### 结构体定义

```
pub struct ToolRouter {
    registry: ToolRegistry,                    // 工具 handler 的注册表
    specs: Vec<ConfiguredToolSpec>,            // 所有已注册工具的配置规格
    model_visible_specs: Vec<ToolSpec>,        // 实际发送给模型的工具列表（已过滤）
    parallel_mcp_server_names: HashSet<String>,// 支持并行调用的 MCP 服务器名
}
```

### 工具调用分发流程

```
ResponseItem (AI 返回)
       │
       ▼
ToolRouter::build_tool_call()
       │
       ├── ResponseItem::FunctionCall
       │      │
       │      ├─ 查找 MCP tool info？
       │      │   ├── 是: ToolPayload::Mcp { server, tool, args }
       │      │   └── 否: ToolPayload::Function { arguments }
       │      └── → ToolCall { tool_name, call_id, payload }
       │
       ├── ResponseItem::LocalShellCall
       │      └── → ToolCall { tool_name: "local_shell", payload: LocalShell }
       │
       ├── ResponseItem::CustomToolCall
       │      └── → ToolCall { tool_name, payload: Custom { input } }
       │
       └── ResponseItem::ToolSearchCall (execution == "client")
              └── → ToolCall { tool_name: "tool_search", payload: ToolSearch }
                        │
                        ▼
dispatch_tool_call_with_code_mode_result()
       │
       ▼
ToolRegistry::dispatch_any(invocation)
       │
       ├── 查找对应的 handler
       └── 调用 handler (通过 ToolOrchestrator)
```

### 工具规格过滤逻辑

```
specs（完整工具列表）
   │
   ├─ filter code_mode_only 工具（code mode 嵌套工具不可见）
   ├─ filter deferred dynamic tools（延迟加载工具不在初始列表中）
   └── model_visible_specs（发送给模型的工具定义）
```

### ToolCall 结构

```
pub struct ToolCall {
    tool_name: ToolName {            // 工具标识符
        name: String,               //   工具名（如 "shell"）
        namespace: Option<String>,  //   命名空间（如 MCP server 名）
    },
    call_id: String,                 // AI 分配的 call ID
    payload: ToolPayload,            // 调用载荷
}

pub enum ToolPayload {
    Function { arguments: String },          // 普通函数工具（JSON 字符串）
    Mcp { server, tool, raw_arguments },     // MCP 工具调用
    LocalShell { params: ShellToolCallParams },
    Custom { input: serde_json::Value },     // 自定义工具
    ToolSearch { arguments: SearchToolCallParams },
}
```

### 并行工具调用策略

```
ToolRouter::tool_supports_parallel(call) 决策：
  ┌─ ToolPayload::Mcp    → 检查 parallel_mcp_server_names（按 server 配置）
  └─ 其他类型            → 检查 ConfiguredToolSpec::supports_parallel_tool_calls
```

---

## 7. ToolOrchestrator

**源文件：** `codex-rs/core/src/tools/orchestrator.rs`

### 职责

`ToolOrchestrator` 是**工具执行的统一编排器**，负责处理审批策略、沙箱选择、失败重试的完整流程。它实现了"先请示，再执行，失败后可升级重试"的核心安全模型。

### 结构体定义

```
pub(crate) struct ToolOrchestrator {
    sandbox: SandboxManager,    // 沙箱类型选择与命令构建
}

pub(crate) struct OrchestratorRunResult<Out> {
    pub output: Out,
    pub deferred_network_approval: Option<DeferredNetworkApproval>,
}
```

### 完整执行流程

```
ToolOrchestrator::run(tool, req, tool_ctx, turn_ctx, approval_policy)
│
├─ [步骤 1] 计算审批需求
│   exec_approval_requirement() / default_exec_approval_requirement()
│   │
│   ├── ExecApprovalRequirement::Skip
│   │    ├─ strict_auto_review? → 走 Guardian 审批
│   │    └─ 否则: 直接通过（otel 记录 Config 决策）
│   │
│   ├── ExecApprovalRequirement::Forbidden
│   │    └─ 立即返回 ToolError::Rejected
│   │
│   └── ExecApprovalRequirement::NeedsApproval
│        └─ 走审批流程（下方）
│
├─ [步骤 2] 请求审批 request_approval()
│   │
│   ├─ 评估 PermissionRequest hooks（config 中定义的自动规则）
│   │    ├── Allow → ReviewDecision::Approved
│   │    ├── Deny  → ToolError::Rejected（带说明）
│   │    └── None  → 继续
│   │
│   ├─ use_guardian? → 路由到 Guardian Review session
│   │    └── Guardian AI 审查 → ReviewDecision
│   │
│   └─ 否则 → 发送 ApprovalRequest 事件，等待用户 ReviewDecision
│
├─ [步骤 3] 执行拒绝检查 reject_if_not_approved()
│   Denied / Abort / TimedOut → ToolError::Rejected
│   Approved / ApprovedForSession → 继续
│
├─ [步骤 4] 选择初始沙箱
│   SandboxManager::select_initial(
│       file_system_sandbox_policy,
│       network_sandbox_policy,
│       tool.sandbox_preference(),
│       windows_sandbox_level,
│       managed_network_active
│   ) → SandboxType（Seatbelt / Landlock / None 等）
│
├─ [步骤 5] 执行首次尝试 run_attempt()
│   │
│   ├── begin_network_approval()  ← 网络策略预检
│   ├── tool.run(req, attempt, ctx) ← 实际执行
│   └── finish_network_approval() ← 网络策略后处理
│
├─ [步骤 6] 首次失败处理
│   若 SandboxErr::Denied（沙箱拒绝）
│   │
│   ├─ tool.escalate_on_failure()? → 否则直接返回错误
│   ├─ wants_no_sandbox_approval()? → 否则直接返回错误
│   │
│   ├─ bypass_retry_approval? → 无需再次审批
│   └─ 否则: 再次走审批流程（retry_reason = sandbox 拒绝原因）
│
└─ [步骤 7] 升级重试（SandboxType::None）
    run_attempt(escalated_attempt) ← 无沙箱再次执行
    返回最终结果
```

### 审批决策树（精简版）

```
AskForApproval 策略
       │
       ├── Auto（全自动）
       │    └── 只读操作: Skip，写操作: Skip（有沙箱保护）
       │
       ├── Unless-All-Files-Are-Untrusted
       │    └── 视文件系统策略决定
       │
       ├── OnRequest（仅在工具主动请求时审批）
       │    └── NeedsApproval when tool.wants_approval()
       │
       └── Never（永不自动执行写操作）
            └── 写操作: NeedsApproval
```

### 网络策略（Network Approval）

```
Immediate 模式: 执行前同步等待网络策略批准
Deferred 模式:  先执行，完成后异步处理网络策略
               （适用于支持 deferred 的工具，如 MCP）
```

---

## 8. RolloutRecorder

**源文件：** `codex-rs/rollout/src/recorder.rs`

### 职责

`RolloutRecorder` 将每个 session 的完整事件流**异步持久化为 JSONL 文件**（rollout 文件），同时同步更新 SQLite 数据库。它是 Codex 会话持久化和可恢复性的基础。

### 结构体定义

```
pub struct RolloutRecorder {
    tx: Sender<RolloutCmd>,              // 向后台写入任务发送命令
    writer_task: Arc<RolloutWriterTask>, // 后台写入任务的观察状态
    pub(crate) rollout_path: PathBuf,   // JSONL 文件路径
    state_db: Option<StateDbHandle>,    // SQLite 数据库句柄
    event_persistence_mode: EventPersistenceMode, // 事件持久化级别
}
```

### 内部命令枚举

```
enum RolloutCmd {
    AddItems(Vec<RolloutItem>),         // 追加事件项（主要写入路径）
    Persist { ack: oneshot::Sender },   // 刷盘并 ack
    Flush { ack: oneshot::Sender },     // 确保所有 pending 写入完成
    Shutdown { ack: oneshot::Sender },  // 关闭写入任务
}
```

### 异步写入架构

```
Session（主 Tokio task）
       │
       │ record_items(&[RolloutItem])
       │
       ▼
tx: Sender<RolloutCmd>    ← mpsc 有界通道（capacity = 256）
       │
       │ 不阻塞 session loop！
       │
       ▼
┌──────────────────────────────────────────────────────┐
│         rollout_writer (独立 Tokio task)              │
│                                                      │
│  RolloutWriterState {                                │
│    writer: JsonlWriter,          ← tokio::fs::File   │
│    pending_items: Vec<...>,      ← 待写入缓冲         │
│    state_db_ctx: Option<...>,    ← SQLite 句柄        │
│    state_builder: Option<...>,   ← Thread 元数据构建   │
│    ...                                               │
│  }                                                   │
│                                                      │
│  loop {                                              │
│    cmd = rx.recv().await                             │
│    match cmd {                                       │
│      AddItems(items) → add_items(items)              │
│      Persist { ack } → write_pending() + ack         │
│      Flush { ack }   → write + sync_thread_state()  │
│                           + ack                      │
│      Shutdown { ack } → flush + ack + break          │
│    }                                                 │
│  }                                                   │
└──────────────────────────────────────────────────────┘
       │
       ▼
文件系统: rollout-{RFC3339}-{uuid}.jsonl
SQLite:  threads 表（通过 sync_thread_state_after_write）
```

### 文件命名格式

```
rollout-2025-01-15T10:30:45.123Z-{uuid}.jsonl
         │                        │
         └── RFC3339 时间戳       └── UUID（ThreadId）
```

### RolloutRecorderParams（创建/恢复模式）

```
RolloutRecorderParams
├── Create {                           // 新建会话
│    conversation_id: ThreadId,
│    forked_from_id: Option<ThreadId>, // fork 来源
│    source: SessionSource,
│    base_instructions: BaseInstructions,
│    dynamic_tools: Vec<DynamicToolSpec>,
│    event_persistence_mode,
│  }
└── Resume {                           // 从现有文件恢复
     path: PathBuf,                   // append 模式打开
     event_persistence_mode,
   }
```

### 关键设计亮点

1. **零拷贝解耦**：`tx: Sender<RolloutCmd>` 是有界通道，主 session loop 发送后立即返回，写入任务在后台批量处理，不影响 AI turn 响应延迟。

2. **延迟物化（Deferred Materialization）**：新建会话时不立即创建文件（`deferred_log_file_info`），只有在真正有事件写入时才创建文件，避免创建空 rollout 文件。

3. **写入恢复机制**：`write_pending_with_recovery()` 在首次写入失败时进入 recovery 模式（`enter_recovery_mode()`），将 pending 数据保存，避免丢失。

4. **终止状态传播**：`RolloutWriterTask` 通过 `terminal_failure` 字段记录后台任务的致命错误，后续 `record_items()` 调用会检查并返回该错误。

5. **git 信息异步采集**：writer 任务启动后，异步采集当前工作目录的 git 信息（branch、SHA、origin URL），写入 session meta 头部。

---

## 9. StateDbHandle / StateRuntime

**源文件：** `codex-rs/rollout/src/state_db.rs`（handle 别名）、`codex-rs/state/src/runtime.rs`（实现）

### 职责

`StateDbHandle`（即 `Arc<StateRuntime>`）是 SQLite 数据库的统一操作句柄。通过 `sqlx` 连接池提供线程安全的异步 CRUD 操作，持久化所有 thread 元数据、agent jobs、memories 等状态。

### StateRuntime 结构体

```
pub struct StateRuntime {
    codex_home: PathBuf,                      // ~/.codex 或配置的 home 目录
    default_provider: String,                 // 默认模型提供方 ID
    pool: Arc<SqlitePool>,                    // 主状态 DB 连接池（max 5 连接）
    logs_pool: Arc<SqlitePool>,               // 独立 logs DB 连接池（减少锁竞争）
    thread_updated_at_millis: Arc<AtomicI64>, // 最近 updated_at 的毫秒时间戳（内存缓存）
}
```

### 数据库文件布局

```
~/.codex/
├── state_{VERSION}.sqlite          ← 主状态数据库
│    ├── threads                    ← Thread 元数据
│    ├── thread_spawn_edges         ← 多 Agent 父子关系图
│    ├── agent_jobs                 ← Batch Agent Job 任务
│    ├── agent_job_items            ← Job 子任务
│    ├── memories                   ← AI 生成的记忆
│    └── _sqlx_migrations           ← 迁移记录
├── state_{VERSION}.sqlite-wal      ← WAL 日志（Write-Ahead Logging）
├── state_{VERSION}.sqlite-shm      ← 共享内存（WAL 协调）
└── logs_{VERSION}.sqlite           ← 独立 logs 数据库（结构化日志）
```

### 关键操作方法

#### Thread 管理

| 方法 | 说明 |
|------|------|
| `upsert_thread(metadata)` | 插入或更新 thread 记录（全量 upsert） |
| `insert_thread_if_absent(metadata)` | 仅在不存在时插入（ON CONFLICT DO NOTHING） |
| `list_threads(page_size, filters)` | 分页查询 threads，支持多种过滤器和排序 |
| `list_thread_ids(limit, anchor, ...)` | 轻量级 ID 列表查询 |
| `update_thread_title(id, title)` | 更新 thread 标题（AI 自动生成） |
| `touch_thread_updated_at(id)` | 更新 `updated_at_ms`（活跃性维护） |
| `update_thread_git_info(id, ...)` | 更新 git 元信息（sha、branch、origin） |
| `mark_archived(id)` | 归档 thread |
| `find_rollout_path_by_id(id)` | 根据 ID 查找 JSONL 文件路径 |

#### Thread Spawn 关系图（多 Agent）

| 方法 | 说明 |
|------|------|
| `upsert_thread_spawn_edge(parent, child, status)` | 建立父子关系边 |
| `list_thread_spawn_descendants(id)` | 递归列出所有子代 thread |
| `find_thread_spawn_descendant_by_path(id, path)` | 按 rollout path 查找子代 |

#### Agent Jobs（批量任务）

| 方法 | 说明 |
|------|------|
| `create_agent_job(params)` | 创建 batch job 记录 |
| `get_agent_job(job_id)` | 查询 job 状态 |
| `mark_agent_job_running(job_id)` | 标记 job 为运行中 |
| `mark_agent_job_completed(job_id)` | 标记 job 为完成 |
| `mark_agent_job_item_running_with_thread(item_id, thread_id)` | 关联 job item 到 thread |
| `report_agent_job_item_result(item_id, result)` | 原子性提交 item 结果 |
| `get_agent_job_progress(job_id)` | 聚合查询 job 进度 |

### SQLite 配置

```
SqliteConnectOptions {
    journal_mode: Wal,           // WAL 模式，读写并发
    synchronous: Normal,         // 平衡安全与性能
    auto_vacuum: Incremental,    // 增量 vacuum，避免大 VACUUM 操作
    busy_timeout: 5s,            // 锁等待超时
    max_connections: 5,          // 连接池上限
}
```

### 两个数据库分离设计

```
state.sqlite  ──  持久状态（threads、jobs、memories）
                  高价值数据，WAL + Normal sync 保护

logs.sqlite   ──  结构化日志（每 partition 上限 10MB + 1000 行）
                  频繁写入，独立文件避免与 state.sqlite 争锁
```

---

## 10. 完整组件关系图

```
╔══════════════════════════════════════════════════════════════════════════════╗
║                           外部调用层                                         ║
║   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐   ┌────────────┐  ║
║   │   TUI        │   │  exec CLI    │   │  app-server  │   │  MCP 服务  │  ║
║   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘   └─────┬──────┘  ║
╚══════════╪═══════════════════╪═══════════════════╪═════════════════╪═════════╝
           │                  │                   │                 │
           └──────────────────┴───────────────────┴─────────────────┘
                                        │ get_thread / submit / next_event
                                        ▼
╔══════════════════════════════════════════════════════════════════════════════╗
║                         ThreadManager（全局注册表）                           ║
║  ┌────────────────────────────────────────────────────────────────────────┐ ║
║  │ ThreadManagerState                                                      │ ║
║  │  threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>             │ ║
║  │  thread_created_tx: broadcast::Sender<ThreadId>                        │ ║
║  │                                                                         │ ║
║  │  ┌─────────────┐ ┌──────────────┐ ┌─────────────┐ ┌────────────────┐  │ ║
║  │  │AuthManager  │ │SkillsManager │ │PluginsMgr   │ │EnvironmentMgr  │  │ ║
║  │  └─────────────┘ └──────────────┘ └─────────────┘ └────────────────┘  │ ║
║  │  ┌─────────────┐ ┌──────────────┐                                       │ ║
║  │  │McpManager   │ │ModelsManager │                                       │ ║
║  │  └─────────────┘ └──────────────┘                                       │ ║
║  └────────────────────────────────────────────────────────────────────────┘ ║
╚══════════════════════════════════════╪═════════════════════════════════════╝
                                       │ spawn / resume / fork
                                       ▼
╔══════════════════════════════════════════════════════════════════════════════╗
║                      CodexThread（会话句柄层）                               ║
║  ┌────────────────────────────────────────────────────────────────────────┐ ║
║  │  session_source: SessionSource                                          │ ║
║  │  rollout_path: Option<PathBuf>                                          │ ║
║  │  out_of_band_elicitation_count: Mutex<u64>                              │ ║
║  │  _watch_registration: WatchRegistration                                 │ ║
║  └────────────────────────────────────────────────────────────────────────┘ ║
║                         │ submit / next_event / steer_input                 ║
║                         ▼                                                   ║
║  ┌────────────────────────────────────────────────────────────────────────┐ ║
║  │                   Codex（双向通道层）                                    │ ║
║  │                                                                         │ ║
║  │  tx_sub ─────────────────────────────────────→  session loop           │ ║
║  │  Sender<Submission>           Op::UserTurn                              │ ║
║  │                               Op::Interrupt                             │ ║
║  │                               Op::Shutdown                              │ ║
║  │                                                                         │ ║
║  │  rx_event  ←─────────────────────────────────  session loop            │ ║
║  │  Receiver<Event>              Event::TurnStarted                        │ ║
║  │                               Event::AgentMessage                       │ ║
║  │                               Event::ToolCallStarted                    │ ║
║  │                               Event::TurnComplete                       │ ║
║  │                                                                         │ ║
║  │  agent_status: watch::Receiver<AgentStatus>  ← 实时状态订阅             │ ║
║  └────────────────────────────────────────────────────────────────────────┘ ║
╚══════════════════════════════════════╪═════════════════════════════════════╝
                                       │ Arc<Session>
                                       ▼
╔══════════════════════════════════════════════════════════════════════════════╗
║                        Session（核心执行引擎）                               ║
║  ┌────────────────────────────────────────────────────────────────────────┐ ║
║  │ state: Mutex<SessionState>                                              │ ║
║  │   └─ SessionConfiguration（model、prompt、policy、cwd...）               │ ║
║  │ active_turn: Mutex<Option<ActiveTurn>>                                  │ ║
║  │ guardian_review_session: GuardianReviewSessionManager                   │ ║
║  │ goal_runtime: GoalRuntimeState                                           │ ║
║  │ services: SessionServices                                                │ ║
║  │   ├─ models_manager: SharedModelsManager                                │ ║
║  │   ├─ mcp_connection_manager: Arc<McpConnectionManager>                  │ ║
║  │   ├─ plugins_manager: Arc<PluginsManager>                               │ ║
║  │   └─ auth_manager: Arc<AuthManager>                                      │ ║
║  └────────────────────────────────────────────────────────────────────────┘ ║
║        │ run_turn()          │ tool call           │ persist                 ║
╚════════╪════════════════════╪════════════════════╪════════════════════════╝
         │                    │                    │
         ▼                    ▼                    ▼
╔════════════════╗  ╔══════════════════════════╗  ╔════════════════════════╗
║  ModelClient   ║  ║       ToolRouter         ║  ║   RolloutRecorder      ║
║                ║  ║  registry: ToolRegistry  ║  ║                        ║
║  new_session() ║  ║  specs: Vec<ToolSpec>    ║  ║  tx: Sender<RolloutCmd>║
║      │         ║  ║  model_visible_specs     ║  ║  writer_task (独立)    ║
║      ▼         ║  ║          │               ║  ║  rollout_path: PathBuf ║
║ ModelClient    ║  ║          ▼               ║  ║  state_db: Option<..>  ║
║ Session        ║  ║  ToolOrchestrator        ║  ╚══════════╪═════════════╝
║   │            ║  ║  ┌──────────────────┐   ║             │
║   ├─ WS 路径   ║  ║  │ 1. 审批策略检查  │   ║             │
║   │  stream_   ║  ║  │ 2. Guardian审查  │   ║             ▼
║   │  responses ║  ║  │ 3. 沙箱选择      │   ║  ╔════════════════════════╗
║   │  websocket ║  ║  │ 4. 首次执行      │   ║  ║  StateDbHandle         ║
║   │            ║  ║  │ 5. 升级重试      │   ║  ║ (Arc<StateRuntime>)    ║
║   └─ SSE 路径  ║  ║  └──────────────────┘   ║  ║                        ║
║     stream_    ║  ╚══════════════════════════╝  ║  pool: Arc<SqlitePool> ║
║     responses  ║                               ║  logs_pool: Arc<...>   ║
║     _api       ║                               ╚════════════════════════╝
╚════════════════╝
```

---

## 附录：组件间调用链（典型 UserTurn 场景）

```
用户输入消息
      │
      ▼
CodexThread::submit(Op::UserTurn { input })
      │
      ▼
Codex::submit(op)
  → 生成 submission_id（UUID v7）
  → tx_sub.send(Submission { id, op, trace })
      │
      ▼ (异步 session loop 接收)
Session（主循环）
  → run_turn(sess, turn_ctx, input, ...)
      │
      ├─ record_context_updates...()    记录上下文变更
      ├─ build_prompt()                  构建完整 prompt
      │
      ├─ ModelClientSession::stream()   调用 AI API
      │    ├─ stream_responses_websocket() ─ 优先 WS
      │    └─ stream_responses_api()     ─ 降级 SSE
      │          │ 流式 SSE/WS 事件
      │          ▼
      │  map_response_events() 解析 API 响应
      │          │
      │  ┌────── ResponseItem::FunctionCall ──────┐
      │  │                                        │
      │  ▼                                        ▼
      │ emit TurnItemStarted                 ToolRouter::build_tool_call()
      │                                           │
      │                                           ▼
      │                                      ToolOrchestrator::run()
      │                                           │
      │                                      ┌────┴────────────┐
      │                                      │ 审批 → 沙箱 → 执行 │
      │                                      └────┬────────────┘
      │                                           │ ToolResult
      │                                           ▼
      │                                 record_into_history()
      │                                           │
      │                                 emit TurnItemCompleted
      │
      ├─ 继续下一轮 AI 采样（直到 response.done）
      │
      ├─ emit TurnComplete
      │
      └─ RolloutRecorder::record_items([...])
              │ (mpsc 异步发送，不阻塞)
              ▼
         后台写入任务
              ├─ 写入 rollout-*.jsonl
              └─ sync_thread_state_after_write()
                   └─ StateRuntime::upsert_thread() / touch_thread_updated_at()
```

---

*文档基于源码 `codex-rs/core`、`codex-rs/rollout`、`codex-rs/state` 实际实现撰写，如源码有变动请及时更新本文档。*
