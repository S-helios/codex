# Thread / Session / Turn 生命周期详解

> 本文档回答：为什么"一次 Turn 不等于一次对话"？多伦、间隔很长时间、断点续跑是怎么实现的？
> Thread / Session / Turn 三者的边界、循环依据、以及所有相关 Op 操作枚举。

---

## 目录

1. [三个概念的本质区别](#1-三个概念的本质区别)
2. [Thread 生命周期](#2-thread-生命周期)
3. [Session 生命周期](#3-session-生命周期)
4. [Turn 生命周期](#4-turn-生命周期)
5. [为什么一次 Turn ≠ 一次对话](#5-为什么一次-turn--一次对话)
6. [多轮对话：跨 Turn 的历史延续](#6-多轮对话跨-turn-的历史延续)
7. [断点续跑：跨 Session / 进程重启的历史恢复](#7-断点续跑跨-session--进程重启的历史恢复)
8. [InitialHistory 四种变体详解](#8-initialhistory-四种变体详解)
9. [Rollout 文件：持久化引擎](#9-rollout-文件持久化引擎)
10. [Op 全枚举：所有操作及其对生命周期的影响](#10-op-全枚举所有操作及其对生命周期的影响)
11. [生命周期状态机总图](#11-生命周期状态机总图)
12. [核心代码路径索引](#12-核心代码路径索引)

---

## 1. 三个概念的本质区别

| 维度 | Thread（线程/会话） | Session（运行时） | Turn（回合） |
|------|-------------------|------------------|-------------|
| **是什么** | 持久化的对话身份标识 | Thread 的运行时实例 | 一次 LLM 交互循环 |
| **标识符** | `ThreadId`（UUID，不可变） | 无独立 ID，绑定 ThreadId | `turn_id`（UUID，每次新建） |
| **生存周期** | 跨进程，理论上永久 | 进程内，直到 `Op::Shutdown` | 单次任务，直到模型不再需要 follow-up |
| **持久化** | rollout 文件（磁盘） | 内存（`ContextManager`） | 临时状态（`TurnState`） |
| **数量关系** | 一个 Thread | 一个 Thread 可对应多个 Session（跨进程） | 一个 Session 内，一次只能有一个活跃 Turn |
| **管理者** | `ThreadManager`（注册表） | `Session` struct + `submission_loop` | `ActiveTurn` + `run_turn()` |

**核心原则**：
- **Thread** = 对话的"身份证"，谁来管这段历史
- **Session** = 进程中实际运行的对话实例，负责接收 Op、执行 Turn
- **Turn** = 用户按一次发送后，模型从接收到最终回复的完整周期（包括多轮工具调用）

---

## 2. Thread 生命周期

### 2.1 Thread 的数据结构

```rust
// codex-rs/core/src/thread_manager.rs
pub(crate) struct ThreadManagerState {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>,  // 全局注册表
    thread_created_tx: broadcast::Sender<ThreadId>,              // 新建通知广播
    thread_store: Arc<dyn ThreadStore>,                           // 持久化后端
    // ...
}
```

`ThreadId` 是一个 UUID。`CodexThread` 是 Thread 的 Rust 封装，持有 `Codex`（即 Session 的句柄）。

### 2.2 Thread 的创建

所有创建路径最终汇聚到 `ThreadManagerState::spawn_thread_with_source`：

```
start_thread()                          → InitialHistory::New
start_thread_with_tools()              → InitialHistory::New
start_thread_with_options()            → InitialHistory::New / Resumed / Forked / Cleared
resume_thread_from_rollout(path)       → InitialHistory::Resumed（从文件读历史）
resume_thread_with_history(history)    → InitialHistory::Resumed（已加载历史）
fork_thread(snapshot)                  → InitialHistory::Forked（拷贝历史分叉）
```

创建流程（`spawn_thread_with_source`，线：`thread_manager.rs:902`）：

```
1. 幂等检查：若 Resumed 且 Thread 已在运行 → 直接返回（不重复创建）
2. 若已停止的同 ID 线程 → 从注册表移除
3. 解析环境选择（exec 环境）
4. 注册技能/插件文件监听
5. 调用 Codex::spawn → 创建 Session（含 submission_loop）
6. 等待首个 SessionConfigured 事件 → 注册到 threads HashMap
7. 若是 Resumed → 调用 apply_goal_resume_runtime_effects()
```

### 2.3 Thread 的结束

| 触发方式 | 行为 |
|---------|------|
| `Op::Shutdown` 被 submission_loop 接收 | Session 的 submission_loop 退出，channels 关闭 |
| `ThreadManager::remove_thread(id)` | 从注册表移除（不自动 shutdown Session） |
| `shutdown_all_threads_bounded(timeout)` | 并发发送 `Op::Shutdown` 给所有 Thread，等待完成 |
| 进程退出 | Session Tokio 任务被 OS 终止 |

**注意**：Thread 从注册表移除 ≠ Thread 历史丢失。只要 rollout 文件还在磁盘上，历史永远可以恢复。

---

## 3. Session 生命周期

### 3.1 Session 的数据结构

```rust
// codex-rs/core/src/session/session.rs:14
pub(crate) struct Session {
    pub(crate) conversation_id: ThreadId,            // 绑定的 Thread ID（不可变）
    pub(super) tx_event: Sender<Event>,              // Session → 前端事件通道（Channel B）
    pub(super) agent_status: watch::Sender<AgentStatus>, // 状态广播（Channel C）
    pub(super) state: Mutex<SessionState>,           // 会话状态（含 ContextManager）
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>, // 当前活跃 Turn（最多一个）
    pub(super) idle_pending_input: Mutex<Vec<ResponseInputItem>>, // 空闲时缓存的输入
    pub(crate) services: SessionServices,            // 各种服务引用
}
```

### 3.2 Session 的启动

`Codex::spawn` 是唯一的 Session 工厂函数（`session/mod.rs:~580`）：

```
Codex::spawn(CodexSpawnArgs { conversation_history: InitialHistory, ... })
↓
1. 创建四条通信 Channel（见文档10）
2. 构建 SessionConfiguration（模型、权限、基础指令等）
3. 调用 Session::new() → 构造 ContextManager、rollout recorder 等
4. 调用 record_initial_history(conversation_history)
   - New/Cleared: ContextManager 为空，等待第一个 Turn 时注入初始 context
   - Resumed: 从 rollout items 重建 ContextManager（完整历史）
   - Forked: 从 rollout items 重建 + 立刻持久化到新文件
5. tokio::spawn → submission_loop (后台 async 任务)
6. 发送 SessionConfigured 事件（首事件）
```

**Session 启动后**：`submission_loop` 在后台串行消费 `Op` 队列，等待用户输入。此时 Session 处于 `AgentStatus::Completed`（无活跃 Turn）。

### 3.3 Session 内的状态转移

```
AgentStatus::PendingInit  (spawn 刚完成，channels 刚建立)
    ↓ SessionConfigured 事件发送
AgentStatus::Completed    (等待第一个 Op::UserInput)
    ↓ Op::UserInput / Op::UserTurn / Op::UserInputWithTurnContext
AgentStatus::Running      (Turn 运行中)
    ↓ Turn 正常完成
AgentStatus::Completed    (可接受下一次用户输入)
    ↓ Op::Interrupt（Turn 运行中）
AgentStatus::Interrupted  (Turn 被打断)
    ↓ Op::UserInput（用户继续）
AgentStatus::Running      (新 Turn 开始)
    ↓ Op::Shutdown
AgentStatus::Shutdown     (Session 终止)
```

### 3.4 一个 Thread 的多个 Session

一个 Thread 在其生命周期内可能经历多个 Session：

```
进程第1次运行:
  Codex::spawn(InitialHistory::New)
  → Session #1 (空历史)
  → 用户进行了10轮对话
  → 历史写入 rollout 文件
  → 进程退出 (Session #1 结束)

进程第2次运行 (断点续跑):
  读取 rollout 文件
  Codex::spawn(InitialHistory::Resumed { history: [...10轮历史...] })
  → Session #2 (从历史重建 ContextManager)
  → 用户继续第11轮对话，LLM 看到完整的1-10轮历史
```

**一个 Thread 对应多个 Session，但同一时刻最多只有一个活跃 Session。**

---

## 4. Turn 生命周期

### 4.1 Turn 的触发

`submission_loop` 接收到用户输入 Op 时（`handlers.rs:805`）：

```rust
Op::UserInput { .. }
| Op::UserInputWithTurnContext { .. }
| Op::UserTurn { .. } => {
    user_input_or_turn(&sess, sub.id, sub.op).await;
}
```

`user_input_or_turn` 的执行路径（`handlers.rs:109`）：

```
1. 解析 Op → (items, SessionSettingsUpdate)
2. sess.new_turn_with_sub_id(sub_id, updates)
   → 创建 TurnContext 快照（模型、权限、cwd 等不可变视图）
   → 更新 SessionState（model、cwd 等新设置生效）
3. sess.steer_input(items)
   → 检查是否有活跃 Turn:
     a. 有活跃 Turn → push 到 active_turn.turn_state.pending_input（追问模式）
     b. 无活跃 Turn → Err::NoActiveTurn → 走下面的路径
4. 若 NoActiveTurn:
   → sess.spawn_task(turn_context, items, RegularTask::new())
   → 创建 ActiveTurn，tokio::spawn run_turn()
```

### 4.2 Turn 的内部结构（sampling loop）

```rust
// session/turn.rs ~ line 436
async fn run_turn(sess, turn_context, items) {
    let mut can_drain_pending_input = items.is_empty();

    loop {
        // ① 获取追问内容（仅在第一次工具调用完成后才允许）
        let pending_input = if can_drain_pending_input {
            sess.get_pending_input().await  // 从 TurnState.pending_input 取出
        } else {
            Vec::new()
        };

        // ② 将追问内容写入对话历史
        if !pending_input.is_empty() {
            sess.record_conversation_items(&pending_input).await;
        }

        // ③ 从内存历史构建完整 prompt
        let history = sess.clone_history().await;
        let prompt_input = history.for_prompt(&input_modalities);

        // ④ 调用 LLM API（可被 CancellationToken 打断）
        tokio::select! {
            result = run_sampling_request(prompt_input, ...) => { /* 处理结果 */ }
            _ = cancellation_token.cancelled() => { /* 被打断 */ break; }
        }

        can_drain_pending_input = true;  // 首次工具调用完成后解锁

        // ⑤ 判断是否继续
        let has_pending_input = sess.has_pending_input().await;
        let needs_follow_up = model_needs_follow_up || has_pending_input;
        if !needs_follow_up { break; }
    }
}
```

每次循环 = 一次 LLM API 调用。模型若返回工具调用，执行工具后继续循环；若返回 final assistant message，循环结束。

### 4.3 Turn 的结束条件

| 结束原因 | `TurnAbortReason` | 触发方式 |
|---------|-------------------|---------|
| 模型输出 final message | 无（正常完成） | `needs_follow_up = false`，loop break |
| 用户主动打断 | `Interrupted` | `Op::Interrupt` → `CancellationToken::cancel()` |
| 另一个 Turn 抢占 | `Replaced` | 新 `Op::UserInput` 在活跃 Turn 期间提交（抢占模式） |
| Token/预算超限 | `BudgetLimited` | 模型返回的停止原因为 budget |
| 审批流程结束 | `ReviewEnded` | Review Task 完成 |

Turn 结束时发送 `EventMsg::TurnCompleted` 或 `EventMsg::TurnAborted`，并清除 `active_turn`。

### 4.4 Turn 的 ActiveTurn 结构

```rust
// codex-rs/core/src/state/turn.rs:29
pub(crate) struct ActiveTurn {
    pub(crate) turn_id: String,
    pub(crate) tasks: IndexMap<String, RunningTask>,  // 并发工具任务
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
}

pub(crate) struct TurnState {
    pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>,
    pending_input: Vec<ResponseInputItem>,  // steer_input 写入，sampling loop 顶部读取
    mailbox_delivery_phase: MailboxDeliveryPhase,
}
```

---

## 5. 为什么一次 Turn ≠ 一次对话

这是最容易产生混淆的概念。澄清：

### "对话" vs "Turn" 的层级差异

```
Thread（整个对话历史）
├── Session #1（第一次进程运行）
│   ├── Turn 1（用户: "帮我写一个排序算法"）
│   │   ├── LLM call 1: 生成代码
│   │   └── 结束（最终回复）
│   ├── Turn 2（用户: "改成快速排序"）
│   │   ├── LLM call 1: 分析
│   │   ├── LLM call 2: 执行 write_file 工具
│   │   ├── LLM call 3: 验证结果
│   │   └── 结束（最终回复）
│   └── Turn 3（用户: "运行一下"）
│       ├── LLM call 1: 执行 shell 工具
│       └── 结束
└── Session #2（进程重启后恢复）
    ├── Turn 4（用户: "上次的代码在哪"）
    │   ├── LLM 看到 Turn 1-3 的完整历史
    │   └── 结束
    └── Turn 5 ...
```

### Turn 包含多次 LLM 调用

一次 Turn 的时间范围可能很长：

```
用户发送 "帮我重构整个项目"
  ↓ Turn 开始
  LLM call 1: "我来分析代码结构"
  → 调用 read_file 工具
  → 等待工具执行（可能几秒）
  LLM call 2: "根据分析，我来重构第一个文件"
  → 调用 write_file 工具
  LLM call 3: "继续第二个文件..."
  → 调用 write_file 工具
  ... (可能几十次 LLM call)
  LLM call N: "重构完成，以下是总结..."
  ↓ Turn 结束
```

这整个过程是**一个 Turn**，因为它响应的是**一次用户输入**。

### Turn 内接受追问

在 Turn 运行期间，用户可以再次发送消息（追问）：

```
用户: "帮我重构整个项目"
  → Turn 开始，LLM 正在执行工具
  
用户: "先重构认证模块，其他的等一下"
  → steer_input() → pending_input 队列
  → 当前 LLM call 完成后，下一次迭代顶部读取 pending_input
  → 写入历史：追问作为 user message
  → LLM call 看到"先重构认证模块"的指令
  
  → 最终结束 Turn（响应了最后的追问）
```

**追问不开启新 Turn，而是注入当前 Turn 的下一次 LLM call。**

---

## 6. 多轮对话：跨 Turn 的历史延续

### 机制

每次 Turn 开始时，`run_turn` 通过 `sess.clone_history().for_prompt()` 获取**完整历史**作为 LLM 输入：

```rust
// 每次采样循环的 ③ 步
let history = sess.clone_history().await;     // ContextManager 的完整快照
let prompt_input = history.for_prompt(&modalities);  // 全量历史 → LLM 输入
```

`ContextManager` 始终持有该 Session 内所有历史：
- 每次工具调用结果、模型输出都追加到 `ContextManager`
- 每次新 Turn 从 `ContextManager` 的完整视图开始

### 跨 Turn 的间隔时间

Session 在两个 Turn 之间处于 **idle 状态**，`submission_loop` 阻塞在 `rx_sub.recv().await`，等待下一个 Op。间隔时间**可以任意长**（分钟、小时、天），Session 持有内存不释放，历史保持完整。

限制：
- 若进程退出，Session（内存历史）丢失 → 需要通过 rollout 文件恢复（断点续跑）
- 若 Session 长时间不活跃，上层逻辑可能主动 Shutdown（由调用方决定，框架不强制）

---

## 7. 断点续跑：跨 Session / 进程重启的历史恢复

### 触发依据

断点续跑由**调用方显式触发**，框架不自动恢复。触发方式：

```rust
// 方式1：从 rollout 文件路径恢复（最常用）
thread_manager.resume_thread_from_rollout(config, rollout_path, auth, trace).await

// 方式2：已有历史数据的恢复（如从远程加载）
thread_manager.resume_thread_with_history(config, initial_history, auth, false, trace).await

// 方式3：通过 StartThreadOptions
thread_manager.start_thread_with_options(StartThreadOptions {
    initial_history: InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: rollout_items,
        rollout_path: Some(path),
    }),
    ...
}).await
```

### 恢复流程

```
1. 读取 rollout 文件
   RolloutRecorder::get_rollout_history(&rollout_path)
   → Vec<RolloutItem>（包含历史所有 EventMsg、SessionMeta、TurnContext）

2. 构造 InitialHistory::Resumed(ResumedHistory {
       conversation_id: <原 ThreadId>,
       history: Vec<RolloutItem>,
       rollout_path: Some(path),
   })

3. 幂等检查（spawn_thread_with_source:919）
   if thread 已在运行 → 直接返回（不重复创建）
   if thread 已停止   → 从注册表移除旧记录

4. Codex::spawn → Session::new → record_initial_history(Resumed)
   → apply_rollout_reconstruction(rollout_items)
      → reconstruct_history_from_rollout()
      → 逐条重放 EventMsg，重建 ContextManager
   → 恢复 token 使用量（从 rollout 读最后一条 token 信息）
   → flush_rollout()（确保 rollout 文件与内存一致）

5. Session 就绪，发送 SessionConfigured 事件
   AgentStatus → Completed（等待用户输入）

6. 用户发送 Op::UserInput
   → new_turn_with_sub_id → run_turn
   → clone_history().for_prompt() 返回完整的恢复历史
   → LLM 看到之前所有对话内容
```

### 幂等性保证

`spawn_thread_with_source` 在处理 `Resumed` 时有幂等检查：

```rust
if let InitialHistory::Resumed(resumed) = &initial_history {
    if let Some(thread) = threads.get(&resumed.conversation_id) {
        if thread.is_running() {
            // 线程还在运行 → 直接返回现有线程（不重复创建）
            return Ok(NewThread { thread_id: resumed.conversation_id, ... });
        }
        // 线程已停止 → 移除旧记录，重新创建
        threads.remove(&resumed.conversation_id);
    }
}
```

这保证：多次调用 resume（如网络重试）不会产生重复 Session。

---

## 8. InitialHistory 四种变体详解

```rust
// codex-rs/protocol/src/protocol.rs:2374
pub enum InitialHistory {
    New,                    // 全新对话，空历史
    Cleared,                // 清空历史但保留 ThreadId（重置对话）
    Resumed(ResumedHistory), // 从已有历史恢复
    Forked(Vec<RolloutItem>), // 从另一 Thread 分叉（带历史副本）
}

pub struct ResumedHistory {
    pub conversation_id: ThreadId,    // 必须与原 Thread 的 ID 一致
    pub history: Vec<RolloutItem>,    // 完整历史条目（从 rollout 文件读取）
    pub rollout_path: Option<PathBuf>, // rollout 文件路径（可选，用于冲突检测）
}
```

### 各变体的行为

| 变体 | ThreadId | 历史 | rollout 文件 | 适用场景 |
|------|---------|------|------------|---------|
| `New` | 新生成 UUID | 空 | 新建（可选） | 开始全新对话 |
| `Cleared` | 新生成 UUID | 空 | 新建（可选） | 重置对话但保持其他配置 |
| `Resumed` | 沿用原 ThreadId | 从 rollout 重建 | 沿用原文件（追加模式） | 断点续跑、进程重启 |
| `Forked` | 新生成 UUID | 从源 Thread 拷贝 | 新建（立即 materialize） | 分支对话、A/B 测试 |

### New vs Cleared 的区别

两者都是空历史，但语义不同：
- `New`：全新出发，没有任何先验知识（用于首次启动）
- `Cleared`：显式清除历史，表达"我知道这是重置"（用于 `Op::Compact` 之后的极端场景，或测试）

在 `record_initial_history` 中，两者行为完全相同：

```rust
InitialHistory::New | InitialHistory::Cleared => {
    // ContextManager 保持空状态，等待第一个 Turn 注入初始 context
    self.set_previous_turn_settings(None).await;
}
```

### Forked 的特殊处理

`Forked` 会立刻将历史 materialize 到新文件：

```rust
InitialHistory::Forked(rollout_items) => {
    self.apply_rollout_reconstruction(&turn_context, &rollout_items).await;
    if !rollout_items.is_empty() {
        self.persist_rollout_items(&rollout_items).await;  // 写入新 rollout 文件
    }
    self.ensure_rollout_materialized().await;  // 确保文件存在
}
```

这保证 fork 出的 Thread 有独立的历史文件，不会影响源 Thread。

---

## 9. Rollout 文件：持久化引擎

### 文件格式

rollout 文件是 NDJSON（每行一个 JSON 对象）：

```jsonl
{"SessionMeta": {"id": "uuid-xxx", "model": "codex-mini", ...}}
{"TurnContext": {"turn_id": "turn-1", "cwd": "/home/user", ...}}
{"EventMsg": {"AssistantMessage": {"content": "你好！", ...}}}
{"EventMsg": {"ToolCall": {"name": "write_file", "input": {...}}}}
{"EventMsg": {"ToolResult": {"id": "call-1", "output": "ok"}}}
{"EventMsg": {"AssistantMessage": {"content": "文件已写入。", ...}}}
```

### RolloutItem 类型

```rust
pub enum RolloutItem {
    SessionMeta(SessionMetaLine),  // 会话元数据（thread_id, model, forked_from_id 等）
    TurnContext(TurnContextItem),  // Turn 级元数据（turn_id, cwd, model 等）
    EventMsg(EventMsg),            // 实际对话内容（模型输出、工具调用结果等）
    Compacted(CompactedItem),      // Compact 操作的压缩摘要
}
```

### 写入时机

每次有新内容产生时，`persist_rollout_items()` 被调用：

| 写入时机 | 内容 |
|---------|------|
| Session 初始化完成 | `SessionMeta` |
| 每次 Turn 开始 | `TurnContext` |
| 模型输出 token | `EventMsg::AssistantMessage` |
| 工具调用发起 | `EventMsg::ToolCall` |
| 工具执行结果 | `EventMsg::ToolResult` |
| `Op::Compact` | `RolloutItem::Compacted` |
| Turn 结束 | `EventMsg::TurnCompleted / TurnAborted` |

### 读取时机（resume 时）

```
RolloutRecorder::get_rollout_history(path)
→ 读取文件所有行
→ 反序列化为 Vec<RolloutItem>
→ 包装为 InitialHistory::Resumed(ResumedHistory { history: items, ... })
```

---

## 10. Op 全枚举：所有操作及其对生命周期的影响

### 10.1 用户输入 Op（触发新 Turn 或追问当前 Turn）

| Op | 说明 | 生命周期影响 |
|----|------|------------|
| `Op::UserInput { items, environments, final_output_json_schema, ... }` | 传统用户输入（legacy） | 启动新 Turn 或追问当前 Turn |
| `Op::UserInputWithTurnContext { items, cwd, model, effort, ... }` | 带完整上下文的输入（推荐） | 同上，还更新 cwd/model/权限 |
| `Op::UserTurn { items, cwd, approval_policy, sandbox_policy, ... }` | 完整 Turn 上下文设置 | 同上，更新所有 turn-level 设置 |

**三者的区别**：
- `UserInput`：最简，不更新 session 设置
- `UserInputWithTurnContext`：更新 session 设置，携带完整元数据（preferred）
- `UserTurn`：语义更强，明确表达"这是一个新 Turn 的开始"，包含所有配置

**对 Turn 的影响**：
- 若无活跃 Turn → 创建新 Turn（`spawn_task → run_turn`）
- 若有活跃 Turn → items 注入 `pending_input`（追问，不开新 Turn）

### 10.2 控制 Op（影响 Turn / Session 生命周期）

| Op | 说明 | 生命周期影响 |
|----|------|------------|
| `Op::Interrupt` | 取消当前 Turn | `CancellationToken::cancel()` → `TurnAborted(Interrupted)` |
| `Op::Shutdown` | 终止 Session | `submission_loop` 退出 → Session 关闭 |
| `Op::Compact` | 压缩对话历史 | 截断 ContextManager，写入 CompactedItem |
| `Op::ThreadRollback { num_turns }` | 回滚 N 个 Turn | 从历史中移除最后 N 个 Turn 的内容 |
| `Op::CleanBackgroundTerminals` | 清理后台终端进程 | 不影响 Turn/Session 生命周期 |

### 10.3 Turn 上下文覆盖 Op

| Op | 说明 | 生命周期影响 |
|----|------|------------|
| `Op::OverrideTurnContext { cwd, model, effort, ... }` | 修改 session 设置 | 更新 SessionState，但不开新 Turn |

### 10.4 审批/回调 Op（Turn 执行中的双向通信）

| Op | 说明 | 生命周期影响 |
|----|------|------------|
| `Op::ExecApproval { id, turn_id, decision }` | 工具执行审批响应 | 解除 Turn 中等待审批的阻塞 |
| `Op::PatchApproval { id, decision }` | 代码修改审批响应 | 解除 Turn 中等待 patch 审批的阻塞 |
| `Op::UserInputAnswer { id, response }` | 响应 LLM 发起的用户提问 | 解除 Turn 中 UserInputRequest 的等待 |
| `Op::RequestPermissionsResponse { id, response }` | 权限申请响应 | 解除权限等待 |
| `Op::DynamicToolResponse { id, response }` | 动态工具响应 | 解除动态工具等待 |

这些 Op 通过 `oneshot::Sender` 将结果传递给 Turn 中的等待 Future，不影响 Turn/Session 的整体生命周期。

### 10.5 配置/维护 Op

| Op | 说明 | 生命周期影响 |
|----|------|------------|
| `Op::RefreshMcpServers { config }` | 重载 MCP 服务器配置 | 不影响 Turn/Session |
| `Op::ReloadUserConfig` | 重载用户配置 | 不影响 Turn/Session |
| `Op::SetThreadMemoryMode { mode }` | 设置内存模式 | 不影响 Turn/Session |
| `Op::RunUserShellCommand { command }` | 在 session 环境中执行 shell | 不影响 Turn/Session |
| `Op::InterAgentCommunication { communication }` | 子 Agent 间通信 | 可能触发新 Turn（若 `trigger_turn = true`） |

### 10.6 实时对话 Op（Realtime Voice/Text）

| Op | 说明 |
|----|------|
| `Op::RealtimeConversationStart(params)` | 开始实时（语音/文字）对话流 |
| `Op::RealtimeConversationAudio(params)` | 发送音频数据 |
| `Op::RealtimeConversationText(params)` | 发送文字输入（实时模式） |
| `Op::RealtimeConversationClose` | 关闭实时对话流 |
| `Op::RealtimeConversationListVoices` | 查询支持的语音列表 |

实时对话是独立路径，通过 Realtime API 驱动，不经过普通的 Turn sampling loop。

---

## 11. 生命周期状态机总图

```
┌─────────────────────────────────────────────────────────────────────┐
│                         THREAD（持久标识）                           │
│  ThreadId = UUID（跨进程不变）                                        │
│  rollout file = 磁盘历史（跨进程不变）                                 │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │              SESSION #1（进程第1次运行）                       │   │
│  │                                                              │   │
│  │  [启动] Codex::spawn(InitialHistory::New)                    │   │
│  │     → ContextManager = 空                                    │   │
│  │     → AgentStatus::PendingInit → Completed                   │   │
│  │                                                              │   │
│  │  ┌──────────────────────────────────────────────────────┐    │   │
│  │  │  TURN 1（用户首次输入）                                │    │   │
│  │  │  触发: Op::UserInput                                  │    │   │
│  │  │  sampling loop: LLM call 1 → tool → LLM call 2 → end │    │   │
│  │  │  结束: TurnCompleted，ContextManager 增长              │    │   │
│  │  └──────────────────────────────────────────────────────┘    │   │
│  │                                                              │   │
│  │  [idle 任意时间] AgentStatus::Completed                      │   │
│  │                                                              │   │
│  │  ┌──────────────────────────────────────────────────────┐    │   │
│  │  │  TURN 2（用户第二次输入）                              │    │   │
│  │  │  触发: Op::UserInputWithTurnContext                   │    │   │
│  │  │  LLM 看到 TURN 1 的完整历史 + 新输入                  │    │   │
│  │  │  （中途用户追问 → pending_input → 下一 LLM call 包含）  │    │   │
│  │  │  结束: TurnCompleted                                  │    │   │
│  │  └──────────────────────────────────────────────────────┘    │   │
│  │                                                              │   │
│  │  [进程退出] rollout 文件保留 TURN 1 + TURN 2 历史            │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │              SESSION #2（进程重启，断点续跑）                  │   │
│  │                                                              │   │
│  │  [恢复] Codex::spawn(InitialHistory::Resumed {               │   │
│  │           conversation_id: <原 ThreadId>,                    │   │
│  │           history: [TURN1历史 + TURN2历史],                   │   │
│  │         })                                                   │   │
│  │     → ContextManager 从 rollout 重建                         │   │
│  │     → AgentStatus::Completed（等待输入）                      │   │
│  │                                                              │   │
│  │  ┌──────────────────────────────────────────────────────┐    │   │
│  │  │  TURN 3（继续对话）                                   │    │   │
│  │  │  LLM 看到 TURN 1 + TURN 2 + 新输入（完整历史）         │    │   │
│  │  │  用户随时可 Op::Interrupt 打断                         │    │   │
│  │  └──────────────────────────────────────────────────────┘    │   │
│  │                                                              │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘

状态转移触发条件汇总：

  SESSION 开始: ThreadManager::start_thread* / resume_thread* / fork_thread
  SESSION 结束: Op::Shutdown（submission_loop 退出）
  TURN 开始:    Op::UserInput / UserInputWithTurnContext / UserTurn
                （无活跃 Turn 时，调用 spawn_task → run_turn）
  TURN 结束:    模型无需 follow-up（正常） / Op::Interrupt（打断） /
                Budget 超限 / 被新 Turn 替换
  追问(steer):  Op::UserInput 到达时有活跃 Turn → push pending_input
                → 当前 LLM call 完成后，下次迭代前消费
```

---

## 12. 核心代码路径索引

| 概念 | 文件 | 行号 | 说明 |
|------|------|------|------|
| `Thread` 注册表 | `core/src/thread_manager.rs` | 277 | `ThreadManagerState.threads: HashMap<ThreadId, Arc<CodexThread>>` |
| `Session` 结构体 | `core/src/session/session.rs` | 14 | `Session` struct 定义 |
| Session 创建入口 | `core/src/session/mod.rs` | ~580 | `Codex::spawn()` |
| 历史初始化 | `core/src/session/mod.rs` | 1338 | `record_initial_history()` |
| rollout 重建 | `core/src/session/mod.rs` | 1430 | `apply_rollout_reconstruction()` |
| submission_loop | `core/src/session/handlers.rs` | 711 | 串行消费 Op 的后台任务 |
| Turn 触发 | `core/src/session/handlers.rs` | 109 | `user_input_or_turn_inner()` |
| Turn 入口 | `core/src/session/turn.rs` | 191 | `run_turn()` |
| sampling loop | `core/src/session/turn.rs` | ~436 | `loop { ... }` 内的 LLM 调用循环 |
| `ActiveTurn` 结构 | `core/src/state/turn.rs` | 29 | `ActiveTurn`, `TurnState` |
| `pending_input` 写入 | `core/src/session/mod.rs` | 3161 | `Session::steer_input()` |
| `pending_input` 读取 | `core/src/session/mod.rs` | 3321 | `Session::get_pending_input()` |
| `InitialHistory` 定义 | `codex-rs/protocol/src/protocol.rs` | 2374 | 四种变体 |
| `ResumedHistory` 定义 | `codex-rs/protocol/src/protocol.rs` | 2367 | 含 conversation_id, history |
| Thread 创建汇聚点 | `core/src/thread_manager.rs` | 902 | `spawn_thread_with_source()` |
| resume_from_rollout | `core/src/thread_manager.rs` | 669 | `resume_thread_from_rollout()` |
| fork_thread | `core/src/thread_manager.rs` | 762 | `fork_thread()` |
| Op 全枚举 | `codex-rs/protocol/src/protocol.rs` | 403 | `pub enum Op { ... }` |
| `TurnAbortReason` | `codex-rs/protocol/src/protocol.rs` | 3691 | 四种 Turn 终止原因 |
| `AgentStatus` | `codex-rs/protocol/src/protocol.rs` | 1672 | Session 状态枚举 |
| rollout 写入 | `core/src/session/mod.rs` | 2971 | `persist_rollout_items()` |
| 历史写入内存 | `core/src/session/mod.rs` | 2603 | `record_conversation_items()` |

---

## 常见问题

**Q: 用户关闭 App 再重开，对话历史还在吗？**

取决于是否有 rollout 文件。若 Session 在 rollout 文件开启的情况下运行（`thread_store` = `Local`），历史会持续写入磁盘。重开 App 后，调用 `resume_thread_from_rollout(path)` 即可恢复完整历史。

**Q: 为什么 `submission_loop` 必须是串行的？**

防止并发竞争：Turn 的启停、`pending_input` 的推送/消费、Session 状态更新都必须有序。串行队列天然保证了 Op 的执行顺序，避免了复杂的锁争用。

**Q: 如果用户在 Turn 运行中发送新消息，是追问还是等待？**

取决于当前 Turn 的状态和配置。默认：若当前 Turn 活跃 → 追问（steer_input → pending_input）。若配置了 "Replaced" 模式 → 打断旧 Turn，开启新 Turn（`TurnAbortReason::Replaced`）。

**Q: `Compact` 之后，LLM 还能看到之前的历史吗？**

不能。`Op::Compact` 截断 ContextManager，用一个摘要替代所有历史。截断点之前的内容对 LLM 不再可见，但 rollout 文件中仍保留（可供人工审查或 rollback）。
