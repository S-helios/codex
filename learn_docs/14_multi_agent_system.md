# Codex 多 Agent 与子 Agent 系统深度解析

## 1. 设计理念与整体架构

Codex 的多 Agent 系统允许一个"根 Agent"（root agent）在执行任务过程中**动态派生子 Agent**，每个子 Agent 独立拥有自己的对话历史、工具调用能力和沙箱环境，子 Agent 完成任务后将结果返回给父 Agent。

### 核心设计目标

- **任务分解**：将复杂任务拆分给多个专职子 Agent 并行处理
- **隔离执行**：每个 Agent 拥有独立的会话（Session），互不干扰
- **权限继承**：子 Agent 继承父 Agent 的权限，但不能超越父 Agent 的权限边界
- **有界递归**：通过深度限制和数量限制防止 Agent 无限增殖

### 系统层级概览

```
ThreadManager（全局注册表）
  └─ AgentControl（根会话的控制平面，所有子 Agent 共享）
       ├─ AgentRegistry（活跃 Agent 注册表，含数量/深度限制）
       └─ Mailbox（每个 Agent 的收件箱）

线程层级（Thread Hierarchy）：
  /root（根 Agent，ThreadId=A）
    ├─ /root/researcher（子 Agent，ThreadId=B，depth=1）
    │    └─ /root/researcher/inspector（孙 Agent，ThreadId=C，depth=2）
    └─ /root/writer（子 Agent，ThreadId=D，depth=1）
```

---

## 2. Agent 的身份标识体系

每个 Agent 有三种不同维度的标识符，定义在 `codex-rs/protocol/src/agent_path.rs` 和 `protocol.rs`：

### 2.1 ThreadId（内部唯一标识）

- 类型：UUID 格式的字符串
- 用途：内部路由，跨系统唯一标识
- 不对用户直接展示

### 2.2 AgentPath（层级路径标识）

```rust
// codex-rs/protocol/src/agent_path.rs:15
pub struct AgentPath(String);

// 内置特殊路径
const ROOT:    &str = "/root";      // 根 Agent
const MORPHEUS: &str = "/morpheus"; // 系统保留 Agent
```

路径规则：
- 必须以 `/root` 开头（或为 `/morpheus`）
- 路径段只能包含**小写字母、数字、下划线**
- 保留名：`root`、`.`、`..`
- 支持层级组合：`/root/researcher/inspector`

示例：
```
/root                        → 根 Agent
/root/researcher             → depth=1 子 Agent
/root/researcher/inspector   → depth=2 孙 Agent
```

### 2.3 AgentNickname（人类可读昵称）

- 从内置名字列表（`agent_names.txt`）随机选取
- 同一会话内唯一，用于日志和 UI 展示
- 名字池耗尽时自动加后缀（"2nd"、"3rd"）
- 支持角色（role）自定义候选名字列表

---

## 3. 数据结构：核心类型

### AgentStatus（Agent 状态枚举）

`codex-rs/protocol/src/protocol.rs:1672`

```rust
pub enum AgentStatus {
    PendingInit,               // 等待初始化
    Running,                   // 正在执行 Turn
    Interrupted,               // Turn 被中断，可接收新输入
    Completed(Option<String>), // 正常完成，携带最后一条消息
    Errored(String),           // 执行出错
    Shutdown,                  // 已永久关闭
    NotFound,                  // Agent 不存在
}
```

状态转换由事件驱动（`codex-rs/core/src/agent/status.rs:6`）：
```
EventMsg::TurnStarted   → Running
EventMsg::TurnComplete  → Completed(last_message)
EventMsg::TurnAborted   → Interrupted 或 Errored（取决于原因）
EventMsg::Error         → Errored(message)
EventMsg::ShutdownComplete → Shutdown（终态）
```

**终态**（is_final）：`Completed | Errored | Shutdown`——不会再转换。

### SubAgentSource（子 Agent 来源标识）

`codex-rs/protocol/src/protocol.rs:2564`

```rust
pub enum SubAgentSource {
    ThreadSpawn {
        parent_thread_id: ThreadId,  // 父 Agent 的 ThreadId
        depth: i32,                  // 当前层级深度
        agent_path: Option<AgentPath>,
        agent_nickname: Option<String>,
        agent_role: Option<String>,  // 角色名（reviewer、coder 等）
    },
    Review,               // /review 命令触发
    Compact,              // 上下文压缩触发
    MemoryConsolidation,  // 记忆整合触发
    Other(String),        // 其他来源
}
```

### InterAgentCommunication（Agent 间消息）

`codex-rs/protocol/src/protocol.rs:787`

```rust
pub struct InterAgentCommunication {
    pub author: AgentPath,                 // 发送方
    pub recipient: AgentPath,              // 主接收方
    pub other_recipients: Vec<AgentPath>,  // 额外接收方（广播）
    pub content: String,                   // 消息内容
    pub trigger_turn: bool,                // 是否立即唤醒接收方开始新 Turn
}
```

---

## 4. Session 管理

### 4.1 核心组件关系

```
ThreadManager
  ├─ ThreadManagerState（核心状态，Arc 共享）
  │    ├─ threads: HashMap<ThreadId, Arc<CodexThread>>
  │    ├─ thread_created_tx: broadcast::Sender<ThreadId>（通知新线程创建）
  │    ├─ thread_store（本地/远程/内存 持久化）
  │    └─ 各种服务（auth、models、MCP）
  └─ CodexThread（每个 Agent 的句柄）
       ├─ codex: Arc<Codex>（双向通道入口）
       └─ thread_id: ThreadId

Codex（每个 Agent 的通信接口）
  ├─ tx_sub: Sender<Submission>（向 Agent 发送 Op）
  ├─ rx_event: Receiver<Event>（接收 Agent 事件）
  ├─ agent_status: watch::Receiver<AgentStatus>（订阅状态变更）
  └─ session: Arc<Session>（Session 核心）
```

### 4.2 Session 的创建流程

`codex-rs/core/src/session/mod.rs:576`

```
Session::spawn_internal()
  │
  ├─ 创建双向异步通道
  │    tx_sub/rx_sub: bounded(512)   ← Op 提交通道
  │    tx_event/rx_event: unbounded  ← Event 事件通道
  │
  ├─ 加载配置（模型、沙箱策略、工具、插件、技能）
  │
  ├─ 初始化 SessionServices
  │    ├─ model_client（AI API 客户端）
  │    ├─ mcp_manager（MCP 工具服务）
  │    ├─ skills_manager（技能注册）
  │    ├─ auth_manager（鉴权）
  │    ├─ unified_exec_manager（Shell 执行）
  │    └─ agent_control（子 Agent 控制平面）
  │
  ├─ 创建 Session 和 Codex 实例
  │
  └─ spawn submission_loop() 任务（异步事件循环）
```

### 4.3 Session 的核心循环：submission_loop

`codex-rs/core/src/session/handlers.rs:711`

```
submission_loop（无限异步循环）：
  等待 Submission（Op）
    ├─ Op::UserTurn | Op::UserInput → 触发新 Turn
    ├─ Op::InterAgentCommunication  → 路由到目标 Agent 的 Mailbox
    ├─ Op::ExecApproval             → 继续等待审批的 shell 命令
    ├─ Op::Compact                  → 触发上下文压缩
    ├─ Op::Shutdown                 → 清理所有资源，退出循环
    └─ 其他 Op...
```

---

## 5. 子 Agent 的创建（Spawn）

### 5.1 触发方式

子 Agent 通过 `spawn_agent` 工具由 AI 主动调用触发。工具定义在：
`codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`

```
AI 生成 spawn_agent 工具调用
    ↓
Handler 解析参数（message, agent_type, model, reasoning_effort, fork_context）
    ↓
检查深度限制（exceeds_thread_spawn_depth_limit）
    ↓
发出 CollabAgentSpawnBeginEvent（通知 UI）
    ↓
AgentControl::spawn_agent_with_metadata()
    ↓
发出 CollabAgentSpawnEndEvent（含新 thread_id 和 status）
```

### 5.2 核心创建流程

`codex-rs/core/src/agent/control.rs:195`

```
spawn_agent_internal()：
  1. state.upgrade()               ← 从 Weak<ThreadManagerState> 获取强引用
  2. reserve_spawn_slot()          ← 检查/占用 agent_max_threads 配额
  3. inherited_shell_snapshot_for_source()  ← 继承父 Agent 的 shell 环境
  4. inherited_exec_policy_for_source()     ← 继承父 Agent 的执行策略
  5. prepare_thread_spawn()        ← 分配 AgentPath、nickname，注册到 Registry
  6. 创建线程（两种模式）：
     │  fork_mode=None   → spawn_new_thread_with_source()（全新线程）
     │  fork_mode=Some   → spawn_forked_thread()（复制父 Agent 历史）
  7. agent_metadata.agent_id = new_thread.thread_id
  8. reservation.commit(agent_metadata) ← 正式注册到 AgentRegistry
  9. notify_thread_created()        ← 广播新线程创建通知
  10. send_input(initial_operation) ← 发送初始提示词
  11. 返回 LiveAgent { thread_id, metadata, status }
```

### 5.3 两种创建模式

#### Fresh（全新）模式
```
fork_context: false（默认）
→ 子 Agent 从空白状态开始
→ 只继承：shell 环境变量、执行策略、权限配置
→ 不继承：父 Agent 的对话历史、工具调用记录
```

#### Forked（分叉）模式
```
fork_context: true
→ 子 Agent 继承父 Agent 的历史对话快照
→ 过滤规则（keep_forked_rollout_item）：
    ✓ 保留：system/developer/user 消息、FinalAnswer 阶段的 assistant 消息
    ✗ 丢弃：工具调用记录、推理过程、上下文压缩记录
→ 可选截断：LastNTurns(n) 只保留最近 N 轮
→ 截断位置：截至父 Agent 发起 spawn_agent 调用的时间点
```

### 5.4 深度与数量限制

**深度限制**（`codex-rs/core/src/agent/registry.rs:75`）：
```rust
pub fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}
// 子 Agent 的 depth = 父 Agent 的 depth + 1
```

**数量限制**（`codex-rs/core/src/agent/registry.rs:80`）：
```rust
pub fn reserve_spawn_slot(max_threads: Option<usize>) -> Result<SpawnReservation>
// 配置项：agent_max_threads
// 使用 CAS（compare-and-swap）原子操作保证并发安全
```

---

## 6. Agent 间通信机制

### 6.1 Mailbox（邮箱系统）

每个 Agent 拥有独立的 Mailbox，实现在 `codex-rs/core/src/agent/mailbox.rs`：

```rust
pub(crate) struct Mailbox {
    tx: mpsc::UnboundedSender<InterAgentCommunication>,
    next_seq: AtomicU64,       // 单调递增序列号
    seq_tx: watch::Sender<u64> // 广播序列号变更（用于等待通知）
}
```

**发送**：
```
Mailbox::send(communication)
  → 原子递增 next_seq
  → tx.send(communication)（放入无界队列）
  → seq_tx.send_replace(seq)（广播"有新消息"通知）
  → 返回序列号
```

**接收**：
```
MailboxReceiver::has_pending_trigger_turn()
  → 检查是否有 trigger_turn=true 的消息
  → 如有，submission_loop 会启动新 Turn

MailboxReceiver::drain()
  → 取出所有消息（保持原始顺序）
  → 消息作为 ResponseInputItem 注入到 AI 上下文
```

### 6.2 通信流程：从父 Agent 发消息到子 Agent

```
父 Agent AI 调用 send_agent_message 工具
    ↓
Op::InterAgentCommunication { communication } 发往 submission_loop
    ↓
inter_agent_communication()（handlers.rs:310）
    ↓
通过 AgentPath 查找目标 ThreadId
    ↓
sess.enqueue_mailbox_communication(communication)
    → 消息入队到目标 Agent 的 Mailbox
    ↓
if trigger_turn == true:
    maybe_start_turn_for_pending_work_with_sub_id()
    → 唤醒目标 Agent 开始新 Turn
```

### 6.3 子 Agent 的消息注入方式

消息在 Agent 处理时被转换为 `ResponseInputItem`（对话历史中的 assistant 消息）：

```rust
// protocol.rs:813
pub fn to_response_input_item(&self) -> ResponseInputItem {
    ResponseInputItem::Message {
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::to_string(self).unwrap_or_default(),
        }],
        phase: Some(MessagePhase::Commentary), // 标记为注释阶段（不影响 CoT）
    }
}
```

### 6.4 trigger_turn 机制

`trigger_turn` 是控制 Agent 是否立即响应的关键开关：

| 场景 | trigger_turn 值 | 效果 |
|------|----------------|------|
| 发送任务分配 | `true` | 子 Agent 立即开始执行 |
| 发送附加信息 | `false` | 消息入队，等待下次 Turn |
| 广播通知多个 Agent | 视情况 | 灵活控制唤醒时机 |

---

## 7. 协作方式

### 7.1 父子协作（串行）

最基本的协作模式：父 Agent 派生子 Agent，等待子 Agent 完成后继续。

```
父 Agent
  → spawn_agent("请分析这段代码的安全漏洞")
  → [等待子 Agent 完成]
  ←── 子 Agent 完成，结果注入父 Agent 上下文
  → 继续处理结果
```

### 7.2 多子 Agent 并行

父 Agent 同时派生多个子 Agent 并行工作：

```
父 Agent
  → spawn_agent("搜索相关文档", agent_type="researcher")  → 子 Agent A（running）
  → spawn_agent("编写测试用例", agent_type="tester")       → 子 Agent B（running）
  → [等待所有子 Agent 完成]
  ←── 子 Agent A 完成
  ←── 子 Agent B 完成
  → 汇总结果
```

### 7.3 Agent 树（嵌套协作）

子 Agent 本身也可以派生孙 Agent：

```
/root（协调者）
  ├─ /root/architect（设计方案）
  │    └─ /root/architect/researcher（调研技术选型）
  └─ /root/implementer（实现代码）
       ├─ /root/implementer/coder（写代码）
       └─ /root/implementer/tester（写测试）
```

每个节点通过 Mailbox 与子节点通信，形成责任链。

### 7.4 角色（Role）系统

通过 `agent_type`/`agent_role` 参数为子 Agent 指定角色，角色可配置：

- 定制系统 prompt（专职指令）
- 定制候选昵称列表
- 定制默认模型
- 定制推理强度

常见内置角色：`reviewer`、`coder`、`researcher`、`tester`

---

## 8. 权限设计

### 8.1 权限继承原则

**子 Agent 的权限 ≤ 父 Agent 的权限**，继承在以下两个层面发生：

**Shell 环境继承**（`inherited_shell_snapshot_for_source`）：
- 继承父 Agent 的 shell 环境变量快照
- 包含 PATH、环境变量、工作目录状态

**执行策略继承**（`inherited_exec_policy_for_source`）：
- 继承父 Agent 的命令审批规则（哪些命令自动通过，哪些需要用户确认）
- 子 Agent 无法绕过父 Agent 已建立的审批规则

### 8.2 权限传播流程

```
父 Agent（PermissionProfile: WorkspaceWrite）
    ↓
spawn_agent_internal() 创建子 Agent
    ↓
子 Agent 配置：
  - file_system_policy: WorkspaceWrite（继承，不能是 DangerFullAccess）
  - network_policy: 继承父 Agent 配置
  - writable_roots: 继承父 Agent 的可写路径集合
  - approval_policy: 继承父 Agent 配置
    ↓
子 Agent 沙箱启动，受相同或更严格的沙箱策略约束
```

### 8.3 子 Agent 无法做什么

- 无法突破父 Agent 的文件系统权限边界
- 无法绕过审批策略（execApproval）
- 无法访问父 Agent 沙箱以外的路径
- 无法修改 Codex 自身的配置（`.codex` 目录受保护）
- 无法超过父 Agent 的网络访问权限

### 8.4 批准策略（Approval Policy）

子 Agent 的命令执行也受批准策略控制：

| 策略 | 含义 |
|------|------|
| `Never` | 所有命令自动批准（全自动模式） |
| `OnRequest` | 工具执行前总要求用户确认 |
| `OnFailure` | 命令失败时才请求审批 |
| `UnlessTrusted` | 可信命令自动通过，其余需审批 |

---

## 9. 职责边界

### 9.1 内置工具的可用性

每个子 Agent 拥有与根 Agent 相同的工具集（受配置控制）：

| 工具 | 类别 | 说明 |
|------|------|------|
| `shell` | 执行 | 在沙箱内运行 shell 命令 |
| `apply_patch` | 文件 | 修改文件（受权限约束） |
| `list_dir` | 文件 | 列出目录内容 |
| `read_file` | 文件 | 读取文件 |
| `grep_files` | 搜索 | 搜索文件内容 |
| `web_search` | 网络 | 网络搜索（受网络策略约束）|
| `spawn_agent` | 多 Agent | 派生子 Agent（受深度限制）|
| `send_message` | 多 Agent | 向其他 Agent 发消息 |
| `close_agent` | 多 Agent | 关闭指定 Agent |
| `wait_for_agents` | 多 Agent | 等待多个 Agent 完成 |
| `list_agents` | 多 Agent | 列出当前活跃的 Agent |
| `request_user_input` | 交互 | 请求用户输入 |

### 9.2 MCP 工具

MCP（Model Context Protocol）工具服务器可以为每个 Agent 提供额外能力：

```
SessionServices
  └─ mcp_manager
       ├─ MCP Server A（如 database 工具）
       ├─ MCP Server B（如 web scraping 工具）
       └─ MCP Server C（如 code interpreter）
```

MCP 工具在会话初始化时加载，可以通过 `tool_search` 动态发现。

### 9.3 Agent 的隔离边界

- **对话历史**：每个 Agent 独立维护，互不可见（除非 fork 时复制）
- **MCP 连接**：每个 Agent 独立持有 MCP 连接
- **Shell 进程**：通过 `unified_exec_manager` 隔离
- **上下文窗口**：每个 Agent 独立管理 token 用量

---

## 10. Agent 的开启与结束

### 10.1 开启：spawn_agent 工具参数

```json
{
  "message": "请分析 src/ 目录下的代码，找出所有内存泄漏风险",
  "agent_type": "security_reviewer",
  "model": "claude-opus-4-7",
  "reasoning_effort": "high",
  "fork_context": false
}
```

返回：
```json
{
  "agent_id": "thread-uuid-xxxx",
  "nickname": "Aurora"
}
```

### 10.2 关闭：三种结束方式

**1. 自然完成（Completed）**

Agent 完成任务后自动进入 `Completed` 状态：
```
Turn 执行完毕
  → EventMsg::TurnComplete { last_agent_message }
  → AgentStatus::Completed(message)
  → 向父 Agent Mailbox 发送结果（trigger_turn=true）
  → 父 Agent 收到结果，继续自己的 Turn
```

**2. 显式关闭（close_agent 工具）**

父 Agent 主动关闭子 Agent：
```
父 Agent 调用 close_agent(agent_id="thread-uuid-xxxx")
  → Op::Shutdown 发往目标 Agent 的 submission_loop
  → 清理资源：关闭 shell 进程、断开 MCP 连接
  → EventMsg::ShutdownComplete
  → AgentStatus::Shutdown（终态）
  → AgentRegistry::release_spawned_thread()（释放计数）
```

**3. 错误终止（Errored）**

Agent 遇到无法恢复的错误：
```
EventMsg::Error { message }
  → AgentStatus::Errored(message)
  → 不自动向父 Agent 发送通知（需父 Agent 轮询或等待）
```

### 10.3 资源释放

Agent 关闭时的清理顺序：
1. 向 submission_loop 发送 `Op::Shutdown`
2. unified_exec_manager 终止所有子进程
3. MCP 管理器断开所有连接
4. AgentRegistry 调用 `release_spawned_thread(thread_id)` 递减计数
5. ThreadManager 从 HashMap 中移除 CodexThread

---

## 11. 关键事件流

### 11.1 CollabAgent 事件系列

这些事件广播给所有监听客户端（UI、外部 API 等）：

```
CollabAgentSpawnBeginEvent（子 Agent 开始创建）
  ├─ call_id            ← 关联到 spawn_agent 工具调用
  ├─ sender_thread_id   ← 发起 spawn 的父 Agent
  ├─ prompt             ← 发给子 Agent 的初始提示词
  ├─ model              ← 子 Agent 使用的模型
  └─ reasoning_effort   ← 子 Agent 的推理强度

CollabAgentSpawnEndEvent（子 Agent 创建完成）
  ├─ new_thread_id      ← 新 Agent 的 ThreadId
  ├─ new_agent_nickname ← 新 Agent 的昵称
  ├─ new_agent_role     ← 新 Agent 的角色
  └─ status             ← 初始状态（通常是 PendingInit）

CollabAgentInteractionBeginEvent（父子 Agent 通信开始）
  ├─ sender_thread_id   ← 发送方
  ├─ receiver_thread_id ← 接收方
  └─ prompt             ← 通信内容

CollabAgentInteractionEndEvent（父子 Agent 通信结束）
  ├─ receiver_agent_nickname ← 接收方昵称
  └─ status             ← 接收方当前状态

CollabWaitingBeginEvent（父 Agent 等待多个子 Agent）
  ├─ receiver_thread_ids     ← 等待的所有子 Agent
  └─ receiver_agents         ← 含 nickname/role 的详细信息
```

### 11.2 完整的子 Agent 协作时序图

```
父 Agent Turn 开始
  │
  ├─ AI 决策：派生子 Agent
  │    emit CollabAgentSpawnBeginEvent
  │    │
  │    ├─ 创建子 Agent Session
  │    ├─ 注册到 AgentRegistry
  │    ├─ 通知 ThreadManager（notify_thread_created）
  │    ├─ 发送初始 Op（send_input）
  │    │
  │    emit CollabAgentSpawnEndEvent
  │    └─ 返回 { agent_id, nickname }
  │
  ├─ 父 Agent 继续（或等待）
  │
  ├─ [子 Agent 独立执行]
  │    └─ 子 Agent Turn 完成
  │         → AgentStatus::Completed(message)
  │         → 向父 Agent Mailbox 发消息（trigger_turn=true）
  │
  ├─ 父 Agent 被唤醒（Mailbox 收到新消息）
  │    emit CollabAgentInteractionBeginEvent
  │    └─ 子 Agent 结果注入父 Agent 对话历史
  │    emit CollabAgentInteractionEndEvent
  │
  └─ 父 Agent 继续处理结果，Turn 完成
```

---

## 12. AgentControl：控制平面详解

`codex-rs/core/src/agent/control.rs:136`

```rust
pub(crate) struct AgentControl {
    session_id: SessionId,          // 整棵 Agent 树共享同一 session_id
    manager: Weak<ThreadManagerState>, // 弱引用，避免循环引用
    state: Arc<AgentRegistry>,      // 共享注册表
}
```

**关键设计：整棵 Agent 树共享同一个 AgentControl 实例**——这意味着：
- 所有子 Agent 共享同一 `session_id`（便于审计追踪）
- 所有子 Agent 共享同一 `AgentRegistry`（统一的数量限制）
- `Weak<ThreadManagerState>` 避免 `ThreadManagerState→CodexThread→Session→AgentControl→ThreadManagerState` 的引用循环

---

## 13. 保护机制与限制

### 13.1 防无限递归

```
深度检查（每次 spawn 前执行）：
  child_depth = parent_depth + 1
  if child_depth > config.agent_max_depth:
      返回错误："Agent depth limit reached"
```

### 13.2 防资源耗尽

```
数量检查（使用 CAS 原子操作）：
  if total_count >= config.agent_max_threads:
      返回 CodexErr::AgentLimitReached { max_threads }
```

### 13.3 SpawnReservation RAII 机制

```rust
struct SpawnReservation {
    active: bool,
    // ...
}
impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            // 创建失败时，自动释放已预留的 slot
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
```

若 spawn 过程中任何步骤失败，RAII 保证计数器自动归还，不会出现"幽灵 Agent"占用配额。

### 13.4 AgentPath 冲突检测

同一路径的 Agent 只能存在一个：
```rust
fn reserve_agent_path(agent_path: &AgentPath) -> Result<()> {
    match active_agents.agent_tree.entry(agent_path.to_string()) {
        Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(
            format!("agent path `{agent_path}` already exists")
        )),
        Entry::Vacant(entry) => { entry.insert(...); Ok(()) }
    }
}
```

---

## 14. 代码位置速查

| 功能 | 文件 | 关键行 |
|------|------|--------|
| AgentPath 类型定义 | `protocol/src/agent_path.rs` | 1-241 |
| AgentStatus 枚举 | `protocol/src/protocol.rs` | 1672 |
| SubAgentSource | `protocol/src/protocol.rs` | 2564 |
| InterAgentCommunication | `protocol/src/protocol.rs` | 787 |
| CollabAgent 事件系列 | `protocol/src/protocol.rs` | 3699-3818 |
| AgentControl（控制平面）| `core/src/agent/control.rs` | 136 |
| spawn_agent_internal | `core/src/agent/control.rs` | 195 |
| spawn_forked_thread | `core/src/agent/control.rs` | 343 |
| AgentRegistry（注册表）| `core/src/agent/registry.rs` | 23 |
| 深度/数量限制 | `core/src/agent/registry.rs` | 71-97 |
| SpawnReservation RAII | `core/src/agent/registry.rs` | 294-340 |
| Mailbox（邮箱）| `core/src/agent/mailbox.rs` | 11 |
| Agent 状态转换 | `core/src/agent/status.rs` | 6 |
| Session 创建 | `core/src/session/mod.rs` | 576 |
| submission_loop | `core/src/session/handlers.rs` | 711 |
| inter_agent_communication | `core/src/session/handlers.rs` | 310 |
| spawn_agent 工具 | `core/src/tools/handlers/multi_agents_v2/spawn.rs` | 全文 |
| close_agent 工具 | `core/src/tools/handlers/multi_agents_v2/close_agent.rs` | 全文 |
| ThreadManager | `core/src/thread_manager.rs` | 239 |
| CodexThread | `core/src/codex_thread.rs` | 181 |

---

## 15. 总结

Codex 多 Agent 系统的核心设计哲学：

> **树形结构 + 邮箱通信 + 权限继承 + RAII 保护**

1. **树形层级**：Agent 以路径层级组织（`/root/worker/inspector`），清晰表达从属关系
2. **邮箱通信**：异步无界队列 + 序列号 + `trigger_turn` 标志，实现松耦合的 Agent 间协作
3. **权限继承**：子 Agent 权限严格不超过父 Agent，沙箱策略自动向下传播
4. **RAII 保护**：SpawnReservation 确保资源计数绝对准确，防止幽灵 Agent 累积
5. **双重限制**：深度限制（防止无限嵌套）+ 数量限制（防止资源耗尽）
6. **事件驱动**：Collab 事件系列让 UI 实时感知 Agent 树的变化状态

---

> **上一篇**：[13 - 沙箱机制深度解析](./13_sandbox_mechanism.md)　**下一篇**：[15 - API 与协议层详解](./15_api_protocol_layer.md)
