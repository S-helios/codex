# Codex 追问、打断与进度汇报机制详解

> 对应源文件（branch: feature-learn，已注释）：
> - `codex-rs/core/src/codex_thread.rs` — 线程门面层
> - `codex-rs/core/src/session/turn_context.rs` — Turn 上下文工厂
> - `codex-rs/core/src/session/turn.rs` — Turn 采样主循环（核心）
> - `codex-rs/core/src/state/turn.rs` — TurnState / ActiveTurn 状态结构
> - `codex-rs/core/src/context_manager/history.rs` — ContextManager / 对话历史
> - `codex-rs/core/src/thread_manager.rs` — 线程生命周期管理
> - `codex-rs/protocol/src/protocol.rs` — Op/Event 协议定义

---

## 目录

1. [整体架构：消息流模型](#1-整体架构消息流模型)
2. [Turn 采样主循环：工程核心](#2-turn-采样主循环工程核心)
3. [用户追问（steer_input）](#3-用户追问steer_input)
4. [打断（Interrupt）](#4-打断interrupt)
5. [进度汇报（Event 流）](#5-进度汇报event-流)
6. [三机制协调图](#6-三机制协调图)
7. [关键文件速查](#7-关键文件速查)

---

## 1. 整体架构：两条单向异步 channel

"双向异步消息通道"是一种简化说法，实际上是**两条方向相反的独立 channel**，合起来构成双向通信。理解这个是理解整个系统的基础。

### 1.1 两条 channel 的本质

```rust
// session/mod.rs:606  ── Codex::spawn_internal 中创建
let (tx_sub, rx_sub) = async_channel::bounded(512);    // A：前端 → Session
let (tx_event, rx_event) = async_channel::unbounded(); // B：Session → 前端
```

这是来自 `async_channel` crate 的通道（不是 `tokio::sync::mpsc`），关键特性：**Sender 和 Receiver 都可以 Clone**，即支持多生产者多消费者（MPMC）。

```
前端 / app-server
  │                                          ↑
  │  tx_sub.send(Submission { op, id })      │ rx_event.recv() → Event
  ↓                                          │
─────────────────── 进程内内存 ───────────────────
  A: async_channel::bounded(512)    B: async_channel::unbounded()
  ─────────────────────────────     ──────────────────────────────
  容量上限 512 条，超出后 send()     无容量上限，send() 永不阻塞，
  会 .await 阻塞（背压）            消费者慢时内存积压
─────────────────── 进程内内存 ───────────────────
  ↓                                          ↑
  rx_sub.recv() → Submission                 │ tx_event.send(Event)
  │                                          │
  └──── submission_loop（后台 tokio task） ──┘
              Arc<Session>
```

### 1.2 Channel A：前端 → Session（有界，512）

**类型**：`async_channel::bounded(512)`，Sender 可 Clone

**流向**：前端调用 `submit(Op)` → `tx_sub.send(Submission)` → `submission_loop` 的 `rx_sub.recv()`

```rust
// Codex::submit_with_id（session/mod.rs:850）
pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
    self.tx_sub
        .send(sub)
        .await
        // send().await：有界 channel 满了就在这里阻塞（背压）
        // send() 失败 = rx_sub 已关闭 = session_loop 已退出
        .map_err(|_| CodexErr::InternalAgentDied)?;
    Ok(())
}
```

**submission_loop 的消费**（handlers.rs:711）：

```rust
pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    while let Ok(sub) = rx_sub.recv().await {
        // 串行处理每一条 Op
        match sub.op {
            Op::Interrupt          => interrupt(&sess).await,
            Op::UserInput { .. }   => handle_user_input(&sess, sub).await,
            Op::Shutdown           => break,
            // ...
        }
    }
}
```

`submission_loop` 是**串行**的——同一时刻只处理一条 Op，但 Turn 内的工具调用是并行的（在 Turn 内部 spawn 子 task）。

**为什么有界（512）？**

背压控制。若 Agent 执行很慢，前端继续疯狂 submit，channel 满后 `send().await` 阻塞，天然限流。512 的容量足以应对 multi-agent 并发 spawn 等突发批量提交。

**每条 Submission 的结构**：

```rust
pub struct Submission {
    pub id: String,  // UUIDv7（时间有序），用于将后续 Event 关联回这次提交
    pub op: Op,
    pub trace: Option<W3cTraceContext>,  // W3C 分布式追踪头，透传到模型 API
}
```

提交 ID 用 UUIDv7（时间单调递增），可按提交时序排序，方便调试。

### 1.3 Channel B：Session → 前端（无界）

**类型**：`async_channel::unbounded()`，Receiver 可 Clone

**流向**：`Session::send_event_raw` → `tx_event.send(Event)` → 前端 `rx_event.recv()`

```rust
// Codex::next_event（session/mod.rs:896）
pub async fn next_event(&self) -> CodexResult<Event> {
    let event = self
        .rx_event
        .recv()
        .await
        // recv() 失败 = tx_event 全部 Drop = session_loop 已退出
        .map_err(|_| CodexErr::InternalAgentDied)?;
    Ok(event)
}
```

**为什么无界？**

事件绝对不能丢。如果有界且满了，`send()` 阻塞会卡住 Session 的事件发送，连锁阻塞整个 Turn 执行。无界的代价是：消费者慢时内存会积压，但这在正常场景下不成问题。

### 1.4 Channel C：AgentStatus watch（多播状态）

除了 A/B 两条主通道，还有第三条：`tokio::sync::watch` 用于 AgentStatus 多播。

```rust
// session/mod.rs:768
let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);
```

**watch channel 的特性**：

- 单生产者，多消费者（`Receiver` 可 Clone）
- 只保留**最新值**，不排队历史值
- `borrow()` 非阻塞读最新值；`changed().await` 阻塞等下次变化
- 适合"状态指示器"场景：关心的是当前状态，不关心中间过渡

```rust
// Session 侧写入（deliver_event_raw）
if let Some(status) = agent_status_from_event(&event.msg) {
    self.agent_status.send_replace(status);  // 覆盖旧值
}

// 前端侧读取（可多个 task 并发订阅）
let mut rx = thread.subscribe_status();
loop {
    rx.changed().await.ok();                 // 等状态变化
    let s = rx.borrow().clone();             // 非阻塞读最新值
}
```

### 1.5 内部工具调用：oneshot channel

工具调用的"请求审批"也用 channel，但用的是 `tokio::sync::oneshot`（一次性，发送端/接收端各一个）：

```rust
// 审批请求的工程模式（session/mod.rs:2091）
let (tx_approve, rx_approve) = oneshot::channel();

// Turn 执行侧：把 tx 存进 TurnState，发事件告诉前端需要审批
pending_approvals.insert(call_id, tx_approve);
sess.send_event(turn_context, EventMsg::ExecApprovalRequest(...)).await;

// 然后 Turn 这里阻塞等待
let decision = rx_approve.await;  // 等前端通过 Op::ExecApproval 响应

// 前端 submit Op::ExecApproval 后
// submission_loop 取出 tx，通过它发送 decision
tx_approve.send(decision)
```

Turn 在等待审批时，主 loop 是**挂起**的（`.await` 在 oneshot 上），不消耗 CPU，也不阻塞 submission_loop（因为 submission_loop 是一个独立 task）。

### 1.6 三种 channel 汇总

| channel | 类型 | 方向 | 容量 | 用途 |
|---------|------|------|------|------|
| `tx_sub/rx_sub` | `async_channel::bounded` | 前端 → Session | 512 | 传递 Op（用户意图） |
| `tx_event/rx_event` | `async_channel::unbounded` | Session → 前端 | 无限 | 传递 Event（进度/输出） |
| `agent_status` | `tokio::sync::watch` | Session → 前端（多播） | 只保留最新 | 传递 AgentStatus |
| `tx_approve/rx_approve` | `tokio::sync::oneshot` | 前端 → Turn | 1 | 审批/用户输入回调 |

### 1.7 后台 task 的启动

`Codex::spawn` 最终调用 `tokio::spawn` 启动 `submission_loop`（session/mod.rs:801）：

```rust
let session_loop_handle = tokio::spawn(async move {
    submission_loop(session_for_loop, config, rx_sub)
        .instrument(info_span!("session_loop", thread_id = %thread_id))
        .await
});
```

`Codex` 持有的是这个 handle 的 `Shared<JoinHandle>`（可被多个 caller 并发 await），`shutdown_and_wait()` 调用时：

```rust
pub async fn shutdown_and_wait(&self) -> CodexResult<()> {
    let termination = self.session_loop_termination.clone(); // clone Shared Future
    self.submit(Op::Shutdown).await.ok();                    // 发送关闭信号
    termination.await;                                       // 等后台 task 真正退出
    Ok(())
}
```

**核心类型层次**：

```
ThreadManager           管理所有线程生命周期（thread_manager.rs）
  └─ CodexThread        单条线程门面，持有 Codex 句柄（codex_thread.rs）
        └─ Codex        持有两条 channel 端点 + watch Receiver（session/mod.rs）
              │  tx_sub（发给 loop）  rx_event（收来自 loop）  agent_status（watch）
              └─ [tokio::spawn] submission_loop（handlers.rs）
                    │ rx_sub（收）  tx_event（发）  agent_status_tx（发）
                    └─ Arc<Session>  核心业务逻辑
                          └─ run_turn()  单次 Turn 采样主循环（session/turn.rs）
```

---

## 2. Turn 采样主循环：工程核心

理解追问和打断之前，必须先理解 Turn 内部的 **sampling loop**，因为追问/打断都围绕这个 loop 运作。

### 2.1 一个 Turn ≠ 一次 LLM 调用

Turn 内部是一个**无限 loop**，每轮对应一次 LLM API 调用（`session/turn.rs:436`）：

```
run_turn()
  └─ loop {
        1. 排空 pending_input    ← steer_input 追加的追问消息
        2. 构造 prompt           ← clone_history().for_prompt(...)
                                   = 发给 LLM 的完整对话历史
        3. 调 LLM API（流式）   ← run_sampling_request()
        4. 处理响应：
           ├─ 工具调用 → 执行工具 → 结果写历史 → continue
           ├─ 有 pending_input → continue（不 break）
           └─ 纯文本 + 无 pending → break，Turn 结束
     }
```

每次 LLM 调用收到的 prompt 是**整个对话历史**（`Vec<ResponseItem>`），不是增量。

### 2.2 对话历史是唯一真相来源

```rust
// context_manager/history.rs:34
pub(crate) struct ContextManager {
    items: Vec<ResponseItem>,  // 最老在前，最新在后
    history_version: u64,      // 压缩/回滚时递增
}
```

每次采样前构造 prompt（`session/turn.rs:492`）：

```rust
let prompt_input: Vec<ResponseItem> = sess
    .clone_history()
    .await
    .for_prompt(&turn_context.model_info.input_modalities);
// for_prompt() = normalize_history（归一化）+ 过滤模型不支持的模态（如图片）

let prompt = build_prompt(prompt_input, tools, turn_context, base_instructions);
// Prompt { input: Vec<ResponseItem>, tools, system_instructions, ... }
// → 整体发给 LLM API
```

所有写入历史的操作都通过 `record_conversation_items()`：

```rust
// session/mod.rs:2576
pub(crate) async fn record_conversation_items(
    &self, turn_context: &TurnContext, items: &[ResponseItem],
) {
    self.record_into_history(items, turn_context).await;  // append 到 Vec
    self.persist_rollout_response_items(items).await;      // 持久化到 rollout 文件
    self.send_raw_response_items(turn_context, items).await; // 发事件给前端
}
```

---

## 3. 用户追问（steer_input）

### 3.1 本质：append 到历史，等下轮 loop 消费

追问**不会打断**当前正在进行的 LLM 流式输出，而是：

1. 把用户输入 push 到 `TurnState::pending_input`（暂存 Vec）
2. 等采样 loop 的下一轮迭代开头，取出并写入对话历史
3. 再次调用 LLM，prompt = 完整历史（含追问消息）

```
steer_input("换个方向")
    │
    └─ push 到 pending_input（暂存，非阻塞）
                │
                │ 等待当前 LLM 调用 / 工具执行完毕
                ↓
    loop 顶部：take_pending_input()
                │
                └─ record_conversation_items()  → append 到 Vec<ResponseItem>
                                │
                                ↓
                下一次 LLM 调用：prompt = 全量历史（含追问）
```

### 3.2 pending_input 的数据结构

```rust
// state/turn.rs:110
pub(crate) struct TurnState {
    pending_input: Vec<ResponseInputItem>,  // 暂存队列
    mailbox_delivery_phase: MailboxDeliveryPhase,
    // 其他：审批/工具回调的 oneshot channel...
}

// push（steer_input 调用）
pub(crate) fn push_pending_input(&mut self, input: ResponseInputItem) {
    self.pending_input.push(input);
}

// take（loop 顶部消费，原地清空，避免拷贝）
pub(crate) fn take_pending_input(&mut self) -> Vec<ResponseInputItem> {
    let mut ret = Vec::new();
    std::mem::swap(&mut ret, &mut self.pending_input);
    ret  // pending_input 现在为空
}
```

### 3.3 loop 顶部的消费逻辑（session/turn.rs:442）

```rust
// 控制何时允许排空 pending_input：
// - Turn 有初始用户输入（input 非空）：先跑一轮模型，之后才排空
// - Turn 无初始输入（纯 pending 驱动）：可立即排空
let mut can_drain_pending_input = input.is_empty();

loop {
    let pending_input = if can_drain_pending_input {
        sess.get_pending_input().await  // take_pending_input() + mailbox drain
    } else {
        Vec::new()
    };

    // pending_input 过 hook 检查后写入历史
    for item in accepted_pending_input {
        record_pending_input(&sess, &turn_context, item).await;
        // → record_user_prompt_and_emit_turn_item()
        //   → record_conversation_items()   ← append 到 Vec<ResponseItem>
        //   → emit UserMessage 事件给前端   ← 前端看到追问消息气泡
    }

    // prompt = 全量历史（含刚写入的追问）
    let prompt_input = sess.clone_history().await
        .for_prompt(&turn_context.model_info.input_modalities);

    run_sampling_request(prompt_input, ...).await;

    // 采样完成后判断是否继续
    can_drain_pending_input = true;
    let has_pending_input = sess.has_pending_input().await;
    let needs_follow_up = model_needs_follow_up || has_pending_input;
    //   ↑ 只要还有 pending_input，就不 break，下轮消费
    if !needs_follow_up { break; }
}
```

### 3.4 追问的实际时机：三种情况

**情况一：模型正在执行工具调用（最典型）**

```
LLM 返回 tool_call[shell: "npm run build"]
    → 工具在执行中（可能跑几十秒）
    → 用户发来 steer_input("加上 --verbose 参数")
        → push 进 pending_input（非阻塞，立即返回）
    → 工具执行完毕，结果写入历史（tool_result）
    → loop 顶部排空 pending_input，写入历史
    → 下一次 LLM 收到完整上下文：
        [..., tool_call, tool_result, user: "加上 --verbose 参数"]
```

**情况二：模型正在流式生成文本**

追问不中断当前 HTTP 流式读取，等当前响应全部读完后，loop 检测 `has_pending_input=true`，不 break，进入下一轮消费。

**情况三：model↔tool 多轮循环中**

`can_drain_pending_input` 保护：auto-compact 或工具调用未完成时（`model_needs_follow_up=true`），下轮**不立即排空** pending，等模型-工具循环完整结束再消费，避免上下文错乱。

```rust
// auto-compact 后，若模型还需 follow_up，不立即排空
can_drain_pending_input = !model_needs_follow_up;
```

### 3.5 接口层（codex_thread.rs:372）

```rust
pub async fn steer_input(
    &self,
    input: Vec<UserInput>,
    expected_turn_id: Option<&str>,     // 乐观并发控制：防止追问错 Turn
    responsesapi_client_metadata: Option<HashMap<String, String>>,
) -> Result<String, SteerInputError> {
    self.codex.steer_input(input, expected_turn_id, ...).await
}
```

`Session::steer_input` 的校验流程（session/mod.rs:3161）：

```
1. 空输入 → EmptyInput
2. 无活跃 Turn → NoActiveTurn
3. Turn ID 不匹配 → ExpectedTurnMismatch（乐观锁）
4. Turn 类型为 Review/Compact → ActiveTurnNotSteerable
5. 通过 → push_pending_input + accept_mailbox_delivery_for_current_turn
```

### 3.6 错误类型

```rust
pub enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),               // 没有进行中的 Turn
    ExpectedTurnMismatch { expected, actual },  // Turn ID 不匹配（乐观锁失败）
    ActiveTurnNotSteerable { turn_kind },       // Review/Compact 不支持追问
    EmptyInput,                                 // 输入为空
}
```

### 3.7 `expected_turn_id` 乐观并发控制

前端在追问时传入当前 Turn 的 ID。若 Agent 刚好自然完成这个 Turn 并开始了新 Turn，ID 不匹配 → `ExpectedTurnMismatch`，前端可决定是否重新追问新 Turn。

### 3.8 追问 vs 新 Turn

| 场景 | 操作 | 效果 |
|------|------|------|
| Agent 正在执行 Turn | `steer_input()` | append 到 pending_input，下轮 loop 消费，同一个 Turn 内 LLM 看到追问 |
| Agent 空闲 | `submit(Op::UserInput)` | 开启新 Turn，产生 TurnStarted 事件 |

---

## 4. 打断（Interrupt）

### 4.1 本质：CancellationToken 广播取消信号

打断通过 `CancellationToken::cancel()` 向所有正在运行的异步任务广播取消信号，令当前 LLM HTTP 流式读取和工具执行 Future 立即终止。

**不关闭**后台终端进程（shell 会话仍保持）。

### 4.2 协议定义（protocol.rs:403）

```rust
pub enum Op {
    /// 终止当前 Turn，不关闭后台终端进程。
    /// Server 响应：EventMsg::TurnAborted
    Interrupt,

    /// 终止本线程的所有后台终端进程（长期运行的 shell）。
    CleanBackgroundTerminals,
}
```

### 4.3 打断的内部机制

每次 LLM 调用都传入 CancellationToken 的子 token：

```rust
// session/turn.rs:516
run_sampling_request(
    ...
    cancellation_token.child_token(),  // 每次都是父 token 的子节点
)
```

`try_run_sampling_request` 内部用 `tokio::select!` 竞争：

```rust
loop {
    tokio::select! {
        event = model_stream.next() => {
            // 正常处理流式 token
        }
        _ = cancellation_token.cancelled() => {
            // 立即退出，不等模型继续输出
            return;
        }
    }
}
```

完整打断流程：

```
submit(Op::Interrupt)
    │
    ↓ submission_loop 收到
    │
    ├─ cancellation_token.cancel()
    │       ↓
    │   所有 child_token.cancelled() 立即触发
    │       ↓
    │   LLM 流式读取退出 / 工具执行 Future 取消
    │
    └─ send_event(EventMsg::TurnAborted {
           reason: TurnAbortReason::Interrupted,
           turn_id, completed_at, duration_ms
       })
```

### 4.4 打断响应事件

```rust
// protocol.rs:3675
pub struct TurnAbortedEvent {
    pub turn_id: Option<String>,
    pub reason: TurnAbortReason,
    pub completed_at: Option<i64>,  // Unix 时间戳（秒）
    pub duration_ms: Option<i64>,   // Turn 持续时间（毫秒）
}

pub enum TurnAbortReason {
    Interrupted,   // 用户显式中断
    Replaced,      // 被新 Turn 替换（并发提交）
    ReviewEnded,   // Review 模式结束
    BudgetLimited, // 预算限制
}
```

### 4.5 持久化处理（thread_manager.rs:1207）

若 rollout 历史末尾处于 mid-turn 状态（有 UserMessage 但无 TurnComplete），恢复线程时自动追加中断边界：

```rust
let aborted_event = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
    turn_id: ...,
    reason: TurnAbortReason::Interrupted,
    completed_at: ...,
    duration_ms: ...,
}));
```

确保 resume/fork 时历史能正确重建，不出现"孤悬的用户消息"。

### 4.6 Agent 状态转换

```
PendingInit
    │ (首次 Turn)
    ↓
Running
    │ (Op::Interrupt)
    ↓
Interrupted        ← 同时发送 TurnAborted 事件
    │ (Op::UserInput)
    ↓
Running
    │ (Turn 完成)
    ↓
Completed(Option<String>)
```

完整枚举（protocol.rs:1672）：

```rust
pub enum AgentStatus {
    PendingInit,               // 等待初始化
    Running,                   // 正在执行 Turn
    Interrupted,               // Turn 已被中断，可接收新输入
    Completed(Option<String>), // 完成，含最终消息
    Errored(String),           // 错误
    Shutdown,                  // 已关闭
    NotFound,                  // 线程不存在
}
```

AgentStatus 通过 `watch::Receiver<AgentStatus>` 多播，多个消费者可同时订阅（`codex_thread.rs:499`）。

### 4.7 打断后的行为

打断后可立即开启新 Turn：

```
Op::Interrupt → TurnAborted → AgentStatus::Interrupted
                                     │
                                     └─ Op::UserInput → 新 Turn 开始
```

`NonSteerableTurnKind` 约束只在 `steer_input` 时有效；打断后用 `submit(Op::UserInput)` 完全不受限制。

---

## 5. 进度汇报（Event 流）

### 5.1 Event 结构

```rust
pub struct Event {
    pub id: String,   // 对应触发此 Turn 的 Op 的 submission_id（用于关联）
    pub msg: EventMsg,
}
```

### 5.2 事件发送机制（三层）

```rust
// session/mod.rs

// 高级接口：带 TurnContext
pub async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
    let event = Event { id: turn_context.sub_id.clone(), msg };
    self.send_event_raw(event).await;
}

// 原始接口：三层处理
pub async fn send_event_raw(&self, event: Event) {
    // 层1：持久化到 rollout（供 resume/fork 时重放）
    self.persist_rollout_items(&[RolloutItem::EventMsg(event.msg.clone())]).await;

    // 层2：记录到 OpenTelemetry tracing
    self.services.rollout_thread_trace.record_protocol_event(&event.msg);

    // 层3：传递到前端
    self.deliver_event_raw(event).await;
}

async fn deliver_event_raw(&self, event: Event) {
    // 更新 watch channel（多消费者立即感知状态变化）
    if let Some(status) = agent_status_from_event(&event.msg) {
        self.agent_status.send_replace(status);
    }
    // 发到无界 event channel
    self.tx_event.send(event).await.ok();
}
```

### 5.3 完整 EventMsg 枚举（protocol.rs:1262）

#### Turn 生命周期

| 事件 | 含义 |
|------|------|
| `TurnStarted` | Turn 开始，含 turn_id、turn_context 快照 |
| `TurnComplete` | Turn 完成，含最终消息、用量统计 |
| `TurnAborted` | Turn 被中断/替换，含原因和时长 |
| `TokenCount` | Token 用量更新（输入/输出/推理） |

#### 模型输出（流式）

| 事件 | 含义 |
|------|------|
| `AgentMessage` | 完整助手消息（非流式） |
| `AgentMessageContentDelta` | 助手消息增量片段（流式） |
| `AgentReasoning` | 推理摘要（o 系列模型） |
| `AgentReasoningRawContent` | 原始推理链（chain-of-thought） |
| `ReasoningContentDelta` | 推理摘要增量 |

#### 工具执行

| 事件 | 含义 |
|------|------|
| `ExecCommandBegin` | Shell 命令开始执行 |
| `ExecCommandOutputDelta` | 命令输出增量（每行） |
| `ExecCommandEnd` | 命令执行结束（含退出码） |
| `TerminalInteraction` | 终端交互（stdin/stdout） |
| `McpToolCallBegin/End` | MCP 工具调用开始/结束 |
| `WebSearchBegin/End` | Web 搜索开始/结束 |
| `ImageGenerationBegin/End` | 图片生成开始/结束 |

#### 审批交互

| 事件 | 含义 |
|------|------|
| `ExecApprovalRequest` | 需要用户审批的命令 |
| `ApplyPatchApprovalRequest` | 需要用户审批的补丁 |
| `RequestUserInput` | 需要用户回答的问题 |
| `ElicitationRequest` | MCP elicitation 请求 |
| `GuardianAssessment` | Guardian 自动审批评估结果 |

#### Patch 生命周期

| 事件 | 含义 |
|------|------|
| `PatchApplyBegin` | 补丁应用开始 |
| `PatchApplyUpdated` | 补丁内容更新（流式生成） |
| `PatchApplyEnd` | 补丁应用结束 |

#### 系统/元数据

| 事件 | 含义 |
|------|------|
| `SessionConfigured` | 线程配置完成（首个事件） |
| `McpStartupUpdate/Complete` | MCP 服务器启动进度 |
| `ContextCompacted` | 上下文压缩完成 |
| `ThreadRolledBack` | 历史回滚完成 |
| `Error` | 执行错误（含错误码） |
| `Warning` | 警告（Turn 继续） |
| `ShutdownComplete` | Agent 已关闭 |

#### 多 Agent 协作（Multi-Agent v2）

| 事件 | 含义 |
|------|------|
| `CollabAgentSpawnBegin/End` | 子 Agent 创建 |
| `CollabAgentInteractionBegin/End` | Agent 间通信 |
| `CollabWaitingBegin/End` | 等待子 Agent 响应 |

### 5.4 典型事件时序

```
Agent（Session）                        前端（app-server）
────────────────                        ──────────────────
TurnStarted          ──────────────→   显示"执行中"
AgentMessage         ──────────────→   显示模型文字
ExecCommandBegin     ──────────────→   显示命令，等待审批
                     ←────────────     ExecApproval(Allow)
ExecCommandOutputDelta ────────────→   流式显示命令输出
ExecCommandEnd       ──────────────→   显示退出码
AgentMessage         ──────────────→   显示结果分析
TurnComplete         ──────────────→   隐藏旋转图标
TokenCount           ──────────────→   更新 token 计数
```

### 5.5 事件消费方式

**方式一：next_event() 完整事件流**（codex_thread.rs:484）

```rust
loop {
    let event = thread.next_event().await?;
    // rx_event.recv()，无事件时异步等待
    match event.msg {
        EventMsg::TurnStarted(_) => ui.show_spinner(),
        EventMsg::AgentMessageContentDelta(d) => ui.append_text(&d.text),
        EventMsg::ExecCommandOutputDelta(o) => ui.append_output(&o.data),
        EventMsg::TurnComplete(_) => ui.hide_spinner(),
        EventMsg::TurnAborted(e) => ui.show_aborted(&e.reason),
        EventMsg::Error(e) => ui.show_error(&e.message),
        _ => {}
    }
}
```

**方式二：subscribe_status() 粗粒度状态订阅**（codex_thread.rs:499）

```rust
let mut status_rx = thread.subscribe_status();  // watch::Receiver<AgentStatus>
loop {
    status_rx.changed().await.ok();
    match *status_rx.borrow() {
        AgentStatus::Running     => send_to_ui("running"),
        AgentStatus::Interrupted => send_to_ui("interrupted"),
        AgentStatus::Completed(_)=> { send_to_ui("done"); break; }
        _ => {}
    }
}
```

| 维度 | `next_event()` | `subscribe_status()` |
|------|---------------|----------------------|
| 数据粒度 | 所有事件（含详细内容） | 仅状态枚举 |
| 缓冲 | 无界 channel，不丢事件 | watch，只保留最新值 |
| 多消费者 | 每事件只能被一个消费者收到 | 多消费者同时订阅 |
| 适用 | 完整 UI 渲染 | 状态指示器 |

---

## 6. 三机制协调图

```
                    ┌────────────────────────────────────────────┐
                    │           前端 / app-server                │
                    │  submit(Op::UserInput) → 新 Turn           │
                    │  steer_input(input)    → 追问当前 Turn     │
                    │  submit(Op::Interrupt) → 打断当前 Turn     │
                    │  next_event()          ← 接收所有事件      │
                    │  subscribe_status()    ← 订阅状态变化      │
                    └────────────┬───────────────────┬───────────┘
                                 │ Op                │ Event
                                 ↓                   ↑
                    ┌────────────────────────────────────────────┐
                    │      Codex（通道封装层）                    │
                    │  tx_sub: Sender<Submission>（容量 512）    │
                    │  rx_event: Receiver<Event>（无界）         │
                    │  agent_status: watch::Sender<AgentStatus>  │
                    └────────────┬───────────────────┬───────────┘
                                 │                   │
                                 ↓                   ↑
                    ┌────────────────────────────────────────────┐
                    │      Session（核心业务逻辑）                │
                    │                                            │
                    │  submission_loop()                         │
                    │  ├─ Op::UserInput  → 新 Turn               │
                    │  ├─ Op::Interrupt  → cancel token          │
                    │  └─ Op::Shutdown   → 关闭                  │
                    │                                            │
                    │  active_turn: Mutex<Option<ActiveTurn>>    │
                    │  ├─ TurnState::pending_input  ← steer      │
                    │  └─ RunningTask::cancellation_token ← int  │
                    │                                            │
                    │  run_turn() 采样主循环：                   │
                    │  loop {                                     │
                    │    take_pending_input()  → 写入历史        │
                    │    clone_history().for_prompt() → prompt   │
                    │    run_sampling_request(prompt)            │
                    │    if !needs_follow_up { break }           │
                    │  }                                         │
                    │                                            │
                    │  send_event_raw():                         │
                    │  ├─ 持久化到 rollout                       │
                    │  ├─ 记录到 OTEL tracing                   │
                    │  └─ 推送到 tx_event                       │
                    └────────────────────────────────────────────┘
```

### 6.1 竞态保护

| 保护点 | 机制 |
|--------|------|
| steer 防止追问错 Turn | `expected_turn_id` 乐观锁 + `Mutex<ActiveTurn>` |
| interrupt 精准取消 | `CancellationToken::child_token()`，父取消子全部取消 |
| 事件不丢 | 无界 event channel，消费者慢时内存积压而非丢弃 |
| 历史完整性 | 事件先写 rollout 文件，再发到 channel |

### 6.2 out_of_band_elicitation 与追问

MCP elicitation（带外引导）是特殊的追问场景，用计数器控制 Session 暂停：

```rust
// codex_thread.rs:793
// 引导开始：计数 0→1 时暂停新 Turn 接收
increment_out_of_band_elicitation_count()
    → set_out_of_band_elicitation_pause_state(true)

// 引导结束：计数归零时恢复
decrement_out_of_band_elicitation_count()
    → set_out_of_band_elicitation_pause_state(false)
```

计数 > 0 期间，Session 暂停接收新 Turn（`Op::UserInput`），但 `steer_input` 仍可正常使用。

---

## 7. 关键文件速查

| 文件 | 关键内容 |
|------|---------|
| `codex-rs/protocol/src/protocol.rs` | `Op`、`EventMsg`、`TurnAbortReason`、`AgentStatus` 定义 |
| `codex-rs/core/src/session/turn.rs` | `run_turn`（采样主循环）、`build_prompt`、`run_sampling_request` |
| `codex-rs/core/src/state/turn.rs` | `TurnState`（`pending_input` Vec）、`ActiveTurn`、`MailboxDeliveryPhase` |
| `codex-rs/core/src/context_manager/history.rs` | `ContextManager`（对话历史 Vec）、`for_prompt` |
| `codex-rs/core/src/session/mod.rs` | `Session::steer_input`、`get_pending_input`、`send_event_raw`、`record_conversation_items` |
| `codex-rs/core/src/codex_thread.rs` | `steer_input`、`submit`、`next_event`、`subscribe_status`、elicitation 计数 |
| `codex-rs/core/src/hook_runtime.rs` | `record_pending_input`（pending→历史写入） |
| `codex-rs/core/src/thread_manager.rs` | 线程生命周期、中断边界持久化 |

### 关键行号

| 位置 | 说明 |
|------|------|
| `session/turn.rs:191` | `run_turn` 入口 |
| `session/turn.rs:436` | 采样主循环 `loop {` |
| `session/turn.rs:444` | `pending_input` 排空逻辑 |
| `session/turn.rs:492` | `clone_history().for_prompt()` |
| `session/turn.rs:507` | `run_sampling_request` 调用 |
| `session/turn.rs:526` | `has_pending_input` 判断 |
| `state/turn.rs:110` | `TurnState` 结构体 |
| `state/turn.rs:221` | `push_pending_input` |
| `state/turn.rs:234` | `take_pending_input`（`mem::swap`） |
| `context_manager/history.rs:34` | `ContextManager` 结构体 |
| `context_manager/history.rs:119` | `for_prompt` |
| `session/mod.rs:2576` | `record_conversation_items` |
| `session/mod.rs:3161` | `Session::steer_input` |
| `session/mod.rs:3321` | `get_pending_input` |
| `protocol.rs:403` | `pub enum Op` |
| `protocol.rs:1262` | `pub enum EventMsg` |
| `protocol.rs:1672` | `pub enum AgentStatus` |
| `protocol.rs:3691` | `pub enum TurnAbortReason` |
| `codex_thread.rs:372` | `steer_input` |
| `codex_thread.rs:484` | `next_event` |

---

## 附录：UserInput 类型

`UserInput` 是用户输入的多态枚举，用于 `Op::UserInput`、`steer_input` 等：

```rust
pub enum UserInput {
    Text { text: String },
    Image { url: String },
    LocalImage { path: String },
    Audio { data: Vec<u8> },
    // ...
}
```

`steer_input` 接受 `Vec<UserInput>`，支持多模态输入。
