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
       └─ AgentRegistry（活跃 Agent 注册表，含数量/深度限制）
  每个 Agent 的 Session 各自持有一个 InputQueue（含子 Agent 邮箱），见 §6.1

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

impl AgentPath {
    // 内置特殊路径（关联常量）
    pub const ROOT: &str = "/root";          // 根 Agent
    pub const MORPHEUS: &str = "/morpheus";  // 保留路径常量 [推测：用途未在 core 进一步展开]
    const ROOT_SEGMENT: &str = "root";
}
```

路径规则（见 `validate_absolute_path` / `validate_agent_name`）：
- 绝对路径必须以 `/root` 开头，或恰好等于 `/morpheus`
- 路径段（agent_name）只能包含**小写 ASCII 字母、数字、下划线**
- 保留段名：`root`、`.`、`..`（不可作为子段名）
- 不得以 `/` 结尾、段名不得含 `/`
- 支持层级组合：`/root/researcher/inspector`

示例：
```
/root                        → 根 Agent
/root/researcher             → depth=1 子 Agent
/root/researcher/inspector   → depth=2 孙 Agent
```

### 2.3 Agent 昵称（人类可读昵称）

- 从内置名字列表（`core/src/agent/agent_names.txt`，经 `AGENT_NAMES = include_str!`）选取
- 同一会话内唯一，用于日志和 UI 展示
- 名字重复时按 `format_agent_nickname`（registry.rs:44）加序数后缀，形如
  "Name the 2nd" / "Name the 3rd"（处理了 11/12/13 → th 的特例）
- 注意：当前代码里没有名为 `AgentNickname` 的独立类型，昵称就是 `String`，
  通过 `reserve_agent_nickname` 在注册表中占位去重 [推测]

---

## 3. 数据结构：核心类型

### AgentStatus（Agent 状态枚举）

`codex-rs/protocol/src/protocol.rs:1632`

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

状态转换由事件驱动，集中在 `agent_status_from_event()`（`codex-rs/core/src/agent/status.rs:6`）：
```
EventMsg::TurnStarted   → Running
EventMsg::TurnComplete  → Completed(last_agent_message)
EventMsg::TurnAborted   → Interrupted（reason 为 Interrupted / BudgetLimited）或 Errored（其余原因）
EventMsg::Error         → Errored(message)
EventMsg::ShutdownComplete → Shutdown（终态）
```

**终态**（`is_final`，status.rs:23）：除 `PendingInit | Running | Interrupted` 之外都算终态，
即 `Completed | Errored | Shutdown | NotFound`——不会再转换。

### SubAgentSource（子 Agent 来源标识）

`codex-rs/protocol/src/protocol.rs:2584`

```rust
pub enum SubAgentSource {
    Review,               // /review 命令触发
    Compact,              // 上下文压缩触发
    ThreadSpawn {
        parent_thread_id: ThreadId,  // 父 Agent 的 ThreadId
        depth: i32,                  // 当前层级深度
        #[serde(default)]
        agent_path: Option<AgentPath>,
        #[serde(default)]
        agent_nickname: Option<String>,
        // serde 仍兼容旧字段名 agent_type
        #[serde(default, alias = "agent_type")]
        agent_role: Option<String>,  // 角色名（explorer 等）
    },
    MemoryConsolidation,  // 记忆整合触发
    Other(String),        // 其他来源
}
```

> `SubAgentSource` 是 `SessionSource::SubAgent(_)` 的载荷。`SessionSource` 上的
> `get_nickname/get_agent_role/get_agent_path` 都是从 `ThreadSpawn` 变体里取出对应字段。

### InterAgentCommunication（Agent 间消息）

`codex-rs/protocol/src/protocol.rs:725`

```rust
pub struct InterAgentCommunication {
    pub author: AgentPath,                 // 发送方
    pub recipient: AgentPath,              // 主接收方
    #[serde(default)]
    pub other_recipients: Vec<AgentPath>,  // 额外接收方（广播）
    pub content: String,                   // 消息内容
    pub trigger_turn: bool,                // 是否立即唤醒接收方开始新 Turn
}
```

---

## 4. Session 管理

### 4.1 核心组件关系

```
ThreadManager（thread_manager.rs:197）
  ├─ ThreadManagerState（核心状态，Arc 共享，thread_manager.rs:226）
  │    ├─ threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>
  │    ├─ thread_created_tx: broadcast::Sender<ThreadId>（通知新线程创建）
  │    ├─ thread_store（本地/远程/内存 持久化）
  │    └─ 各种服务（auth、models、MCP）
  └─ CodexThread（每个 Agent 的句柄，codex_thread.rs:133）
       ├─ codex: Arc<Codex>（双向通道入口）
       └─ thread_id: ThreadId

Codex（每个 Agent 的通信接口，session/mod.rs:392）
  ├─ tx_sub: Sender<Submission>（向 Agent 发送 Op）
  ├─ rx_event: Receiver<Event>（接收 Agent 事件）
  ├─ agent_status: watch（订阅状态变更；Session 持有 watch::Sender 侧）
  └─ session: Arc<Session>（Session 核心）
```

### 4.2 Session 的创建流程

`codex-rs/core/src/session/mod.rs:487`（注意是 `Codex::spawn_internal`，挂在 `impl Codex` 上）

```
Codex::spawn_internal()
  │
  ├─ 创建双向异步通道
  │    tx_sub/rx_sub: bounded(SUBMISSION_CHANNEL_CAPACITY=512)   ← Op 提交通道
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

`codex-rs/core/src/session/handlers.rs:744`

```
submission_loop（无限异步循环）：
  等待 Submission（Op）
    ├─ Op::UserInput                → 触发新 Turn
    ├─ Op::InterAgentCommunication  → 投递到目标 Agent 的邮箱（input_queue）
    ├─ Op::ExecApproval             → 继续等待审批的 shell 命令
    ├─ Op::UserInputAnswer          → 回填 request_user_input 的结果
    ├─ Op::Compact                  → 触发上下文压缩
    ├─ Op::Shutdown                 → 清理所有资源，退出循环
    └─ 其他 Op...
```

> 没有独立的 `Op::UserTurn` 变体——发起回合统一走 `Op::UserInput`。

---

## 5. 子 Agent 的创建（Spawn）

### 5.1 触发方式

子 Agent 通过 `spawn_agent` 工具由 AI 主动调用触发。工具定义在：
`codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`

```
AI 生成 spawn_agent 工具调用
    ↓
Handler 解析参数（message, task_name, agent_type, model, reasoning_effort,
                 service_tier, fork_turns；fork_context 在 V2 已被拒绝）
    ↓
计算 child_depth = next_thread_spawn_depth(session_source)
    ↓
发出 CollabAgentSpawnBeginEvent（通知 UI）
    ↓
组装子 Agent config（套用 role、模型/推理强度覆盖）
    ↓
apply_spawn_agent_overrides(config, child_depth)
（multi_agents_common.rs:283：仅在「非 MultiAgentV2」且 child_depth >= agent_max_depth 时，
 关掉子 Agent 的 SpawnCsv / Collab 能力，使其无法再向下派生）
    ↓
AgentControl::spawn_agent_with_metadata()
（数量上限在 spawn_agent_internal 内由 reserve_spawn_slot 强制校验）
    ↓
发出 CollabAgentSpawnEndEvent（含 new_thread_id、nickname、role 和 status）
```

> 深度限制的执行点因版本而异：V1（`multi_agents/spawn.rs:65`、`agent_jobs.rs:117`、
> `resume_agent.rs:50`）会直接调 `exceeds_thread_spawn_depth_limit` 拒绝超深 spawn；
> MultiAgentV2 路径不在此处硬拒，而是通过上面的 `apply_spawn_agent_overrides`
> 在「非 V2」分支收敛能力——也就是说该收敛分支对 V2 实际不生效，V2 下深度主要靠
> 收紧权限/提示约束。[待确认：V2 是否另有专门的深度硬限制点]

> V2 的参数结构体是 `SpawnAgentArgs`（spawn.rs:244，`#[serde(deny_unknown_fields)]`）：
> `message` 与 `task_name` 必填，其余可选。`fork_context` 一旦出现会直接报错
> “fork_context is not supported in MultiAgentV2; use fork_turns instead”。

### 5.2 核心创建流程

`codex-rs/core/src/agent/control.rs:213`

```
spawn_agent_internal()：
  1. self.upgrade()                ← 从 Weak<ThreadManagerState> 获取强引用
  2. reserve_spawn_slot()          ← 检查/占用 agent_max_threads 配额
  3. inherited_shell_snapshot_for_source()  ← 继承父 Agent 的 shell 环境
  4. inherited_exec_policy_for_source()     ← 继承父 Agent 的执行策略
  5. prepare_thread_spawn()        ← 分配 AgentPath、nickname，注册到 Registry
                                      （仅 SubAgentSource::ThreadSpawn 走此分支）
  6. 创建线程（三种模式）：
     │  有 source + fork_mode=Some → spawn_forked_thread()（复制父 Agent 历史）
     │  有 source + fork_mode=None → spawn_new_thread_with_source()（全新带 source）
     │  无 source                  → spawn_new_thread()（顶层/普通线程）
  7. agent_metadata.agent_id = Some(new_thread.thread_id)
  8. reservation.commit(agent_metadata.clone()) ← 正式注册到 AgentRegistry
  9. notify_thread_created()        ← 广播新线程创建通知
  10. send_input(initial_operation) ← 发送初始提示词
  11. 返回 LiveAgent { thread_id, metadata, status }
```

> `LiveAgent`（control.rs:61）字段：`thread_id` / `metadata` / `status`。

### 5.3 创建模式：由 `fork_turns` 控制

V2 通过字符串参数 `fork_turns` 决定 fork 行为，由 `SpawnAgentArgs::fork_mode()`
（spawn.rs:256）解析为 `Option<SpawnAgentForkMode>`：

| `fork_turns` 取值 | fork_mode | 含义 |
|------|------|------|
| `"none"` | `None` | Fresh：全新线程，不复制历史 |
| `"all"`（缺省默认） | `Some(FullHistory)` | Forked：复制父 Agent 完整历史 |
| 正整数 `N` | `Some(LastNTurns(N))` | Forked：只复制最近 N 轮 |
| 其他/0 | 报错 | 必须是 `none`/`all`/正整数 |

> 旧的布尔参数 `fork_context` 在 V2 已废弃；若出现则直接报错并提示改用 `fork_turns`。

#### Fresh（全新，`fork_turns="none"`）
```
→ 子 Agent 从空白状态开始
→ 只继承：shell 环境变量快照、执行策略、权限配置
→ 不继承：父 Agent 的对话历史、工具调用记录
```

#### Forked（分叉，`fork_turns="all"` 或 N）
```
→ 子 Agent 继承父 Agent 的历史对话快照（spawn_forked_thread，control.rs:363）
→ FullHistory 复制全部，LastNTurns(N) 只保留最近 N 轮
→ 截断位置：截至父 Agent 发起 spawn_agent 调用的时间点
→ 过滤规则（keep_forked_rollout_item，control.rs:99）：
    ✓ 保留：system/developer/user 消息；phase==FinalAnswer 的 assistant 消息；
           Compacted / EventMsg / SessionMeta；TurnContext 仅 FullHistory 时保留
    ✗ 丢弃：Reasoning（推理）、各类 FunctionCall/ToolSearch/CustomTool 调用及其输出、
           WebSearch、ImageGeneration、Compaction/ContextCompaction 等
```
> 注：`FullHistory` 模式下不允许再叠加 model / reasoning_effort / role 覆盖
> （`reject_full_fork_spawn_overrides`，spawn.rs:88）。

### 5.4 深度与数量限制

**深度限制**（`codex-rs/core/src/agent/registry.rs:75`）：
```rust
pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}
// 子 Agent 的 depth 由 next_thread_spawn_depth() = 父 depth + 1 计算（registry.rs:71）
```

**数量限制**（`codex-rs/core/src/agent/registry.rs:80`）：
```rust
pub(crate) fn reserve_spawn_slot(self: &Arc<Self>, max_threads: Option<usize>)
    -> Result<SpawnReservation>
// 配置项：agent_max_threads
// 当 max_threads 为 Some 时走 try_increment_spawned（CAS 自旋，registry.rs:275），
// 超额返回 CodexErr::AgentLimitReached { max_threads }；为 None 时直接 fetch_add 计数
```

---

## 6. Agent 间通信机制

### 6.1 邮箱系统：`InputQueue`（重构说明）

> **重构说明**：早期独立的 `core/src/agent/mailbox.rs`（带 `next_seq: AtomicU64` +
> `seq_tx: watch::Sender<u64>` 序列号的 `Mailbox` 结构）已不复存在。当前邮箱机制
> 与「回合待处理输入」合并进了 **`InputQueue`**，位于
> `codex-rs/core/src/session/input_queue.rs`。每个 `Session` 持有唯一一个 `InputQueue`，
> 子 Agent 邮箱是其中的会话级共享状态。

```rust
// core/src/session/input_queue.rs:52
pub(crate) struct InputQueue {
    mailbox_tx: watch::Sender<()>,                              // 仅作"有新邮件"信号，不带序列号
    mailbox_pending_mails: Mutex<VecDeque<InterAgentCommunication>>, // 待处理邮件队列（保持入队顺序）
}
```

**发送 / 入队**（`enqueue_mailbox_communication`，input_queue.rs:74）：
```
push_back(communication)        ← 追加到 VecDeque 队尾
mailbox_tx.send_replace(())     ← 广播"有新邮件"信号（唤醒订阅者）
```

**订阅 / 接收**：
```
subscribe_mailbox()                ← 返回 watch::Receiver<()>；若已有待处理邮件先 mark_changed
has_trigger_turn_mailbox_items()   ← 是否存在 trigger_turn=true 的邮件
                                     （如有，调度器可启动新 Turn）
drain_mailbox_input_items()        ← drain(..) 取出全部邮件（保持原始顺序），
                                     逐条 to_response_input_item() 转成 ResponseItem
```

> 邮箱与回合输入的汇流由 `get_pending_input` / `has_pending_input` 完成：当本回合
> "接收邮件"开关打开（`MailboxDeliveryPhase`）时，把 drain 出的邮件并入 pending_input。

### 6.2 通信流程：从父 Agent 发消息到子 Agent

```
父 Agent AI 调用 send_message 工具（或 followup_task）
    ↓
Op::InterAgentCommunication { communication } 发往 submission_loop
    ↓
inter_agent_communication(sess, sub_id, communication)（handlers.rs:326）
    ↓
sess.input_queue.enqueue_mailbox_communication(communication)
    → 消息入队到目标 Agent 的邮箱（InputQueue）
    ↓
if trigger_turn == true:
    sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)（tasks/mod.rs:459）
    → 唤醒目标 Agent 开始新 Turn
```

> `send_message` 与 `followup_task` 共用同一条投递路径（message_tool.rs），
> 区别只在 `MessageDeliveryMode`：`TriggerTurn`（立即唤醒）vs `QueueOnly`（仅入队）。

### 6.3 子 Agent 的消息注入方式

消息在 Agent 处理时被转换为 `ResponseInputItem`（对话历史中的 assistant 消息）：

```rust
// protocol.rs:751
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

每个节点通过邮箱（InputQueue）与子节点通信，形成责任链。

### 7.4 角色（Role）系统

通过 `agent_type` 参数（内部映射为 `agent_role`）为子 Agent 指定角色。角色定义见
`core/src/agent/role.rs`，可配置：

- 定制系统 prompt（专职指令，配套 `*.toml`，如 `core/src/agent/builtins/explorer.toml`）
- 定制默认模型
- 定制推理强度

默认角色为 `DEFAULT_ROLE_NAME = "default"`（role.rs:29）。当前实际内置角色是
`explorer`（用于"针对代码库的具体问题"，可并行派生多个）；`awaiter`（等待长命令完成
后报告）在 role.rs 中暂被注释。文档其余示例中出现的 `researcher`/`coder` 等角色名
仅为讲解示意，并非硬编码内置角色。[推测]

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

子 Agent 的命令执行也受批准策略 `AskForApproval` 控制（protocol.rs:829，`#[serde(rename_all = "kebab-case")]`）：

| 变体 | serde 名 | 含义 |
|------|------|------|
| `UnlessTrusted` | `untrusted` | 只自动放行已知安全的只读命令，其余都问 |
| `OnRequest`（默认） | `on-request` | 由模型决定何时向用户征求批准 |
| `Granular(...)` | `granular` | 按类别细粒度开关各审批流 |
| `Never` | `never` | 从不询问；命令失败直接回给模型 |
| `OnFailure` | `on-failure` | **已废弃**（旧：失败时才升级审批） |

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
| `send_message` | 多 Agent | 向其他 Agent 发消息（立即唤醒）|
| `followup_task` | 多 Agent | 向其他 Agent 追加消息（同一投递路径，trigger_turn 行为不同）|
| `close_agent` | 多 Agent | 关闭指定 Agent |
| `wait_agent` | 多 Agent | 等待一个/多个 Agent 完成 |
| `list_agents` | 多 Agent | 列出当前活跃的 Agent |
| `request_user_input` | 交互 | 请求用户输入 |

> 多 Agent V2 工具的 `tool_name()` 实名见 `core/src/tools/handlers/multi_agents_v2/`：
> `spawn_agent` / `send_message` / `followup_task` / `close_agent` / `wait_agent` / `list_agents`。

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
  "task_name": "audit-mem-leaks",
  "agent_type": "explorer",
  "model": "gpt-5.1-codex",
  "reasoning_effort": "high",
  "fork_turns": "none"
}
```
（`message` 与 `task_name` 必填；`#[serde(deny_unknown_fields)]` 不接受未知字段。
用 `fork_turns` 而非旧的 `fork_context`。）

返回（`SpawnAgentResult`，spawn.rs:294，`#[serde(untagged)]`）：
```json
{
  "task_name": "audit-mem-leaks",
  "nickname": "Aurora"
}
```
> 返回里给的是 `task_name` 和（可选）`nickname`，并非 `agent_id`；当昵称被隐藏时
> 只返回 `task_name`（`HiddenMetadata` 变体）。

### 10.2 关闭：三种结束方式

**1. 自然完成（Completed）**

Agent 完成任务后自动进入 `Completed` 状态：
```
Turn 执行完毕
  → EventMsg::TurnComplete { last_agent_message }
  → AgentStatus::Completed(message)
  → 向父 Agent 邮箱（InputQueue）发送结果（trigger_turn=true）
  → 父 Agent 收到结果，继续自己的 Turn
```

**2. 显式关闭（close_agent 工具）**

父 Agent 主动关闭子 Agent（close_agent.rs:27）：
```
父 Agent 调用 close_agent(target=...)   ← target 经 resolve_agent_target 解析为 thread_id
  → emit CollabCloseBeginEvent
  → AgentControl::close_agent(agent_id)（control.rs:773）
       → state.send_op(agent_id, Op::Shutdown {}) 发往目标 Agent 的 submission_loop
       → 清理资源：关闭 shell 进程、断开 MCP 连接
       → state.release_spawned_thread(agent_id)（释放计数）
  → EventMsg::ShutdownComplete → AgentStatus::Shutdown（终态）
  → emit CollabCloseEndEvent
```
> 不能关闭 root：若 target 解析到 `/root` 会报错 “root is not a spawned agent”。

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

这些事件广播给所有监听客户端（UI、外部 API 等），结构定义在 `protocol.rs:3715` 起一段：

```
CollabAgentSpawnBeginEvent（子 Agent 开始创建，protocol.rs:3715）
  ├─ call_id            ← 关联到 spawn_agent 工具调用
  ├─ started_at_ms      ← 起始时间戳
  ├─ sender_thread_id   ← 发起 spawn 的父 Agent
  ├─ prompt             ← 发给子 Agent 的初始提示词
  ├─ model              ← 子 Agent 使用的模型
  └─ reasoning_effort   ← 子 Agent 的推理强度

CollabAgentSpawnEndEvent（子 Agent 创建完成，protocol.rs:3756）
  ├─ call_id / completed_at_ms / sender_thread_id
  ├─ new_thread_id      ← 新 Agent 的 ThreadId（Option<ThreadId>，可能未创建）
  ├─ new_agent_nickname ← 新 Agent 的昵称
  ├─ new_agent_role     ← 新 Agent 的角色
  ├─ prompt / model / reasoning_effort ← 继承+覆盖后的最终值
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
  │    └─ 返回 { task_name, nickname }
  │
  ├─ 父 Agent 继续（或等待）
  │
  ├─ [子 Agent 独立执行]
  │    └─ 子 Agent Turn 完成
  │         → AgentStatus::Completed(message)
  │         → 向父 Agent 邮箱（InputQueue）发消息（trigger_turn=true）
  │
  ├─ 父 Agent 被唤醒（邮箱收到新消息）
  │    emit CollabAgentInteractionBeginEvent
  │    └─ 子 Agent 结果注入父 Agent 对话历史
  │    emit CollabAgentInteractionEndEvent
  │
  └─ 父 Agent 继续处理结果，Turn 完成
```

---

## 12. AgentControl：控制平面详解

`codex-rs/core/src/agent/control.rs:154`

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
- `Weak<ThreadManagerState>` 避免引用循环
  `ThreadManagerState → CodexThread → Session → SessionServices → ThreadManagerState`
  （源码注释里给的就是这条链）

---

## 13. 保护机制与限制

### 13.1 防无限递归（深度限制）

```
深度检查（V1 spawn 路径，每次 spawn 前执行）：
  child_depth = next_thread_spawn_depth(parent) = parent_depth + 1
  if exceeds_thread_spawn_depth_limit(child_depth, config.agent_max_depth):  // child_depth > max
      返回错误（RespondToModel）
```
> 见 §5.1 的版本差异说明：MultiAgentV2 不在此处硬拒，深度更多体现为能力收敛/提示约束。

### 13.2 防资源耗尽（数量限制）

```
数量检查（reserve_spawn_slot，registry.rs:80）：
  若 max_threads = Some(n)：try_increment_spawned 用 CAS 自旋递增；
      若 total_count >= n → 返回 CodexErr::AgentLimitReached { max_threads }
  若 max_threads = None：直接 fetch_add 计数，不设上限
```

### 13.3 SpawnReservation RAII 机制

```rust
// registry.rs:294
struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,  // 预留的昵称（失败时回收）
    reserved_agent_path: Option<AgentPath>,    // 预留的路径（失败时回收）
}
impl Drop for SpawnReservation {            // registry.rs:331
    fn drop(&mut self) {
        if self.active {
            // 创建失败时，先归还已预留的 agent_path，再释放 slot 计数
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
// commit(agent_metadata) 会把 active 置 false，从而跳过 Drop 中的回收逻辑
```

若 spawn 过程中任何步骤失败，RAII 保证计数器自动归还，不会出现"幽灵 Agent"占用配额。

### 13.4 AgentPath 冲突检测

同一路径的 Agent 只能存在一个（`AgentRegistry::reserve_agent_path`，registry.rs:242）：
```rust
fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
    // active_agents 取自 self.active_agents.lock()
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
| AgentPath 类型定义 | `protocol/src/agent_path.rs` | 1-241（结构体 15）|
| AgentStatus 枚举 | `protocol/src/protocol.rs` | 1632 |
| SubAgentSource | `protocol/src/protocol.rs` | 2584 |
| InterAgentCommunication | `protocol/src/protocol.rs` | 725 |
| to_response_input_item | `protocol/src/protocol.rs` | 751 |
| CollabAgent 事件系列 | `protocol/src/protocol.rs` | 3715 起 |
| AgentControl（控制平面）| `core/src/agent/control.rs` | 154 |
| spawn_agent_internal | `core/src/agent/control.rs` | 213 |
| spawn_forked_thread | `core/src/agent/control.rs` | 363 |
| AgentControl::close_agent | `core/src/agent/control.rs` | 773 |
| AgentRegistry（注册表）| `core/src/agent/registry.rs` | 23 |
| exceeds_thread_spawn_depth_limit / reserve_spawn_slot | `core/src/agent/registry.rs` | 75 / 80 |
| SpawnReservation + Drop | `core/src/agent/registry.rs` | 294 / 331 |
| reserve_agent_path（路径冲突检测）| `core/src/agent/registry.rs` | 242 |
| 邮箱（InputQueue）| `core/src/session/input_queue.rs` | 52 |
| agent_status_from_event（状态转换）| `core/src/agent/status.rs` | 6 |
| Codex::spawn_internal（会话创建）| `core/src/session/mod.rs` | 487 |
| submission_loop | `core/src/session/handlers.rs` | 744 |
| inter_agent_communication | `core/src/session/handlers.rs` | 326 |
| maybe_start_turn_for_pending_work_with_sub_id | `core/src/tasks/mod.rs` | 459 |
| spawn_agent 工具（含 SpawnAgentArgs:244）| `core/src/tools/handlers/multi_agents_v2/spawn.rs` | 全文 |
| close_agent 工具 | `core/src/tools/handlers/multi_agents_v2/close_agent.rs` | 全文 |
| send_message / followup_task 共享投递 | `core/src/tools/handlers/multi_agents_v2/message_tool.rs` | 全文 |
| ThreadManager / ThreadManagerState | `core/src/thread_manager.rs` | 197 / 226 |
| CodexThread | `core/src/codex_thread.rs` | 133 |

---

## 15. 总结

Codex 多 Agent 系统的核心设计哲学：

> **树形结构 + 邮箱通信 + 权限继承 + RAII 保护**

1. **树形层级**：Agent 以路径层级组织（`/root/worker/inspector`），清晰表达从属关系
2. **邮箱通信**：`InputQueue` 内的待处理邮件队列（`VecDeque`）+ `watch` 通知信号 + `trigger_turn` 标志，实现松耦合的 Agent 间协作
3. **权限继承**：子 Agent 权限严格不超过父 Agent，沙箱策略自动向下传播
4. **RAII 保护**：SpawnReservation 确保资源计数绝对准确，防止幽灵 Agent 累积
5. **双重限制**：深度限制（防止无限嵌套）+ 数量限制（防止资源耗尽）
6. **事件驱动**：Collab 事件系列让 UI 实时感知 Agent 树的变化状态

---

> **上一篇**：[13 - 沙箱机制深度解析](../3_执行与安全/13_sandbox_mechanism.md)　**下一篇**：[15 - API 与协议层详解](../5_前端_集成_协议/15_api_protocol_layer.md)
