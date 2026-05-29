# 09 - codex-rs/core 阅读指南
> 文档编号：09 | 类型：代码导读 | 适合读者：想深入阅读 core crate 源码的开发者

---

## 目录

1. [为什么 core 是最难啃的地方](#1-为什么-core-是最难啃的地方)
2. [core 的目录全貌](#2-core-的目录全貌)
3. [模块功能速查表](#3-模块功能速查表)
4. [推荐阅读顺序（主线）](#4-推荐阅读顺序主线)
5. [第一阶段：入口与骨架（2 个文件）](#5-第一阶段入口与骨架2-个文件)
6. [第二阶段：线程管理（2 个文件）](#6-第二阶段线程管理2-个文件)
7. [第三阶段：Session 执行引擎（4 个文件）](#7-第三阶段session-执行引擎4-个文件)
8. [第四阶段：工具系统（5 个目录/文件）](#8-第四阶段工具系统5-个目录文件)
9. [第五阶段：命令执行与沙箱（3 个模块）](#9-第五阶段命令执行与沙箱3-个模块)
10. [第六阶段：AI 通信层（2 个文件）](#10-第六阶段ai-通信层2-个文件)
11. [第七阶段：辅助功能模块（按需阅读）](#11-第七阶段辅助功能模块按需阅读)
12. [各模块关系总图](#12-各模块关系总图)
13. [阅读时的关键问题清单](#13-阅读时的关键问题清单)
14. [避坑提示](#14-避坑提示)

---

## 1. 为什么 core 是最难啃的地方

`codex-rs/core` 是整个 Codex 系统的**中枢神经**，约 **15.4 万行 Rust 代码**，涵盖：

- AI Turn 执行的完整生命周期
- 工具路由、审批、沙箱执行
- 多线程/多 Agent 管理
- Goal 续期、Context Compaction
- MCP 集成、Guardian 审核
- 配置系统、Context 注入

**直接从头开始读会迷失**，因为：
1. 模块之间循环引用（通过 `Arc<Session>`）
2. 核心流程横跨 5+ 个文件
3. 大量 `impl Session` 方法散落在多个子模块
4. 测试文件（`*_tests.rs`）比实现文件还大

本文给你一条**有序的阅读主线**，让每一步都有明确目标。

---

## 2. core 的目录全貌

```
codex-rs/core/src/
│
├── lib.rs                          ← ★ 入口：模块索引 + 公开 API
│
├── ── 核心骨架 ──
├── thread_manager.rs               ← ★ 全局线程注册表
├── codex_thread.rs                 ← ★ 单个 Thread 的双向通道
│
├── ── Session 执行引擎 ──
├── session/
│   ├── mod.rs                      ← ★ Session struct + 公共方法
│   ├── session.rs                  ← ★ Session 详细 struct 定义 + 构造
│   ├── turn.rs                     ← ★ run_turn() 核心循环
│   ├── turn_context.rs             ← Turn 上下文（当前轮次状态）
│   ├── handlers.rs                 ← Op 处理（用户消息/审批/中断）
│   ├── mcp.rs                      ← MCP 相关 Session 方法
│   ├── multi_agents.rs             ← 子 Agent 管理 Session 方法
│   ├── review.rs                   ← Guardian 相关 Session 方法
│   ├── rollout_reconstruction.rs   ← 从 JSONL 恢复会话历史
│   └── tests.rs                    ← 大型集成测试
│
├── ── 工具系统 ──
├── tools/
│   ├── mod.rs                      ← 工具系统入口 + 截断工具
│   ├── router.rs                   ← ★ ToolRouter：工具分发主逻辑
│   ├── registry.rs                 ← ★ ToolRegistry：工具注册表
│   ├── orchestrator.rs             ← 审批 + 沙箱选择 + 执行编排
│   ├── parallel.rs                 ← 并发工具调用管理
│   ├── spec.rs                     ← 工具规格（ToolSpec / ConfiguredToolSpec）
│   ├── context.rs                  ← 工具调用上下文（ToolInvocation）
│   ├── events.rs                   ← 工具事件（开始/完成）
│   ├── sandboxing.rs               ← 工具沙箱权限选择
│   ├── handlers/                   ← ★ 各工具处理器
│   │   ├── mod.rs                  ← 处理器公共工具函数
│   │   ├── shell.rs                ← shell 工具处理器
│   │   ├── apply_patch.rs          ← apply_patch 工具处理器
│   │   ├── goal.rs                 ← goal 工具处理器（get/create/update）
│   │   ├── mcp.rs                  ← MCP 工具处理器
│   │   ├── multi_agents/           ← 多 Agent 工具处理器（spawn/wait/send）
│   │   ├── multi_agents_v2/        ← 多 Agent v2 工具处理器
│   │   ├── agent_jobs.rs           ← 批处理任务工具处理器
│   │   ├── plan.rs                 ← Plan 模式工具处理器
│   │   ├── request_user_input.rs   ← 用户输入请求处理器
│   │   └── unified_exec.rs         ← 统一命令执行处理器
│   ├── runtimes/                   ← 工具运行时
│   │   ├── shell.rs                ← Shell 运行时（沙箱执行）
│   │   ├── apply_patch.rs          ← Patch 运行时
│   │   └── unified_exec.rs         ← 统一执行运行时
│   └── code_mode/                  ← Code 模式（交互式进程）
│
├── ── 命令执行引擎 ──
├── exec.rs                         ← ★ 低层命令执行（spawn + 输出收集）
├── exec_policy.rs                  ← 执行策略（沙箱规则 + 权限路径）
├── exec_env.rs                     ← 执行环境（env vars + cwd）
├── unified_exec/                   ← 交互式进程执行引擎
│   ├── mod.rs                      ← 模块说明 + 入口
│   ├── process.rs                  ← PTY 进程生命周期
│   ├── process_manager.rs          ← 审批 + 沙箱 + 进程复用
│   ├── process_state.rs            ← 进程共享状态
│   ├── async_watcher.rs            ← 异步进程输出监控
│   └── head_tail_buffer.rs         ← 输出头尾截断缓冲
│
├── ── AI 通信层 ──
├── client.rs                       ← ★ ModelClient（Responses API 通信）
├── client_common.rs                ← 通用 Prompt/ResponseEvent 类型
├── stream_events_utils.rs          ← 流式响应事件处理
├── compact.rs                      ← Context Compaction（本地）
├── compact_remote.rs               ← Context Compaction（远程 v1）
├── compact_remote_v2.rs            ← Context Compaction（远程 v2）
│
├── ── Goal 系统 ──
├── goals.rs                        ← ★ Goal 生命周期/续期/预算
│
├── ── Guardian 审核系统 ──
├── guardian/
│   ├── mod.rs                      ← Guardian 入口 + 公开类型
│   ├── approval_request.rs         ← 审批请求构建
│   ├── prompt.rs                   ← Guardian 提示词
│   ├── review.rs                   ← Guardian 审核决策
│   └── review_session.rs           ← Guardian 专用 Session
│
├── ── MCP 系统 ──
├── mcp.rs                          ← McpManager（MCP 服务器管理）
├── mcp_tool_call.rs                ← MCP 工具调用执行
├── mcp_tool_exposure.rs            ← MCP 工具暴露给 AI
├── mcp_openai_file.rs              ← MCP 文件类型适配
│
├── ── 配置系统 ──
├── config/
│   ├── mod.rs                      ← ★ Config struct（主配置）
│   ├── schema.rs                   ← TOML schema 定义
│   ├── permissions.rs              ← 权限配置解析
│   ├── agent_roles.rs              ← Agent 角色配置
│   ├── edit.rs                     ← 配置动态编辑
│   └── managed_features.rs         ← 受管 Feature Flag
│
├── ── Context 注入系统 ──
├── context/                        ← 系统提示片段（~20 个 Fragment 类型）
├── context_manager/                ← 对话历史管理（压缩/截断/规范化）
│
├── ── Agent 系统 ──
├── agent/
│   ├── mod.rs                      ← Agent 模块入口
│   ├── control.rs                  ← AgentControl（子 Agent 控制）
│   ├── mailbox.rs                  ← Agent 消息邮箱
│   ├── registry.rs                 ← Agent 注册表（父子关系）
│   ├── role.rs                     ← Agent 角色（主/子/Side）
│   └── status.rs                   ← AgentStatus 状态机
│
├── ── Skills/Plugins 系统 ──
├── skills.rs                       ← Skill 系统（技能注入）
├── skills_watcher.rs               ← Skill 文件监控
├── plugins/                        ← Plugin 系统
│
├── ── 其他辅助模块 ──
├── exec_policy.rs                  ← 执行策略
├── hook_runtime.rs                 ← Hook 系统（pre/post 工具钩子）
├── rollout.rs                      ← Rollout JSONL 写入
├── state/                          ← Session 内部状态机
├── tasks/                          ← 任务类型（Regular/Compact/Review）
├── safety.rs                       ← 命令安全检查
├── shell.rs                        ← Shell 环境检测
└── sandboxing/                     ← 沙箱抽象接口
```

---

## 3. 模块功能速查表

| 模块 | 主要职责 | 行数（约） | 优先级 |
|------|---------|-----------|--------|
| `lib.rs` | 模块导出索引，公开 API 声明 | 196 | ★★★ 必读 |
| `thread_manager.rs` | Thread 全局注册、路由 Op/Event | 1606 | ★★★ 必读 |
| `codex_thread.rs` | 单 Thread 生命周期、双向通道 | ~600 | ★★★ 必读 |
| `session/mod.rs` | Session struct、公共方法集合 | 3341 | ★★★ 必读 |
| `session/session.rs` | Session 详细字段、构造逻辑 | ~1286 | ★★★ 必读 |
| `session/turn.rs` | `run_turn()` 核心 AI 循环 | 2213 | ★★★ 必读 |
| `session/handlers.rs` | Op 事件路由（用户消息/审批） | ~963 | ★★ 重要 |
| `tools/router.rs` | ToolRouter 工具分发 | ~266 | ★★★ 必读 |
| `tools/registry.rs` | ToolRegistry 工具注册表 | ~780 | ★★ 重要 |
| `tools/orchestrator.rs` | 审批+沙箱+执行编排 | ~500 | ★★ 重要 |
| `tools/handlers/shell.rs` | shell 工具处理器 | ~244 | ★★ 重要 |
| `tools/handlers/goal.rs` | goal 工具处理器 | ~158 | ★ 按需 |
| `exec.rs` | 低层命令执行 | 1597 | ★★ 重要 |
| `exec_policy.rs` | 沙箱权限规则 | ~1047 | ★★ 重要 |
| `unified_exec/process_manager.rs` | 交互式进程管理 | 1293 | ★ 按需 |
| `client.rs` | Responses API 通信 | 2292 | ★★ 重要 |
| `stream_events_utils.rs` | 流式事件处理 | ~600 | ★★ 重要 |
| `goals.rs` | Goal 续期/预算/状态 | 1850 | ★★ 重要 |
| `guardian/review_session.rs` | Guardian AI 审核 | 1661 | ★ 按需 |
| `config/mod.rs` | Config struct | 3810 | ★★ 重要 |
| `context_manager/history.rs` | 对话历史管理 | ~800 | ★ 按需 |
| `compact.rs` | Context Compaction | ~620 | ★ 按需 |
| `mcp_tool_call.rs` | MCP 工具调用 | 2138 | ★ 按需 |
| `agent/control.rs` | 子 Agent 控制 | 1318 | ★ 按需 |

---

## 4. 推荐阅读顺序（主线）

```
阶段 1（骨架）          阶段 2（线程）         阶段 3（会话）
lib.rs              thread_manager.rs      session/mod.rs
                    codex_thread.rs        session/session.rs
                                           session/turn.rs
                                           session/handlers.rs
        │                  │                      │
        └──────────────────┴──────────────────────┘
                                │
                                ▼
阶段 4（工具）          阶段 5（执行）         阶段 6（通信）
tools/router.rs     exec.rs                client.rs
tools/registry.rs   exec_policy.rs         stream_events_utils.rs
tools/orchestrator  unified_exec/          compact.rs
tools/handlers/

        │
        ▼
阶段 7（专项，按需）
goals.rs  │  guardian/  │  mcp_tool_call.rs  │  agent/  │  config/
```

**每个阶段的核心问题**：

| 阶段 | 核心问题 |
|------|---------|
| 1 | core 对外暴露什么 API？模块之间是什么关系？ |
| 2 | Thread 是什么？Op 和 Event 如何流动？ |
| 3 | Session 里有哪些状态？一个 Turn 是怎么跑的？ |
| 4 | 工具如何被注册、路由、执行、审批？ |
| 5 | 命令如何进入沙箱？输出如何被收集？ |
| 6 | 如何与 Responses API 通信？流式事件怎么处理？ |
| 7 | Goal 怎么续期？Guardian 怎么审核？MCP 怎么调用？ |

---

## 5. 第一阶段：入口与骨架（2 个文件）

### 5.1 `src/lib.rs`（196 行，必读）

**目的**：了解 core 对外暴露哪些东西。

```
阅读重点：
  - pub use xxx → 哪些 struct/fn 是公开 API
  - mod xxx → 了解模块全貌
  - pub(crate) mod xxx → 内部共享模块

关键公开类型：
  pub use codex_thread::CodexThread        ← 外界创建 Thread 用这个
  pub use thread_manager::ThreadManager    ← 全局 Thread 管理器
  pub use goals::ExternalGoalSet           ← 外部设置 Goal 的接口
  pub use mcp::McpManager                  ← MCP 管理器
  pub use network_proxy_loader::*          ← 网络代理
```

**阅读后能回答**：core 的"门面"是什么？哪些 struct 是外部使用的入口？

---

## 6. 第二阶段：线程管理（2 个文件）

### 6.1 `src/thread_manager.rs`（1606 行）

**目的**：理解"Thread"是什么，多 Thread 如何管理。

```
核心 struct：
  ThreadManager
  ├── threads: HashMap<ThreadId, CodexThread>  ← 所有活跃 Thread
  ├── tx_events: Sender<Event>                 ← 对外广播事件
  └── config: Arc<Config>                      ← 全局配置

核心方法（按阅读顺序）：
  1. ThreadManager::new()         ← 了解初始化
  2. create_thread()              ← 线程创建流程
  3. handle_op()                  ← Op 路由分发
  4. broadcast_event()            ← 事件广播机制

关键 Op 类型（来自 codex-protocol）：
  Op::UserTurnInput  → 用户发消息
  Op::SubmitApproval → 用户审批工具
  Op::Interrupt      → 中断当前任务
  Op::SetGoal        → 设置 Goal
  Op::ResumeSideConversation → 恢复 Side 对话
```

**阅读策略**：先看 struct 定义 → `new()` → `create_thread()` → `handle_op()`，
不需要逐行读完，聚焦数据流方向。

---

### 6.2 `src/codex_thread.rs`（~600 行）

**目的**：理解单个 Thread 的生命周期和通信机制。

```
核心 struct：
  CodexThread                    ← Thread 句柄（在 ThreadManager 里持有）
  ├── tx_sub: Sender<Op>         ← 向 Session 发送操作
  ├── rx_event: Receiver<Event>  ← 从 Session 接收事件
  └── thread_id: ThreadId

CodexThread vs Session：
  CodexThread = 句柄（handle），是通道的一端
  Session     = 执行引擎，在独立 Tokio task 中运行

关键方法：
  CodexThread::spawn()    ← 创建并启动 Session
  CodexThread::submit()   ← 发送 Op 给 Session
  CodexThread::receive()  ← 接收 Session 发出的 Event

生命周期：
  spawn() → [Session 在独立 task 运行] → submit(Op) → 处理 → receive(Event)
```

---

## 7. 第三阶段：Session 执行引擎（4 个文件）

这是整个 core 最复杂的部分，Session 是**执行 AI Turn 的核心 struct**。

### 7.1 `src/session/session.rs`（~1286 行）

**目的**：了解 Session 有哪些字段，初始化流程。

```rust
// 重点阅读的 struct 定义：
pub(crate) struct Session {
    pub(crate) conversation_id: ThreadId,   // 当前对话 ID
    pub(super) tx_event: Sender<Event>,     // 发事件给外部
    pub(super) state: Mutex<SessionState>,  // 主状态（配置+历史）
    pub(super) features: ManagedFeatures,   // Feature Flag 集合
    pub(crate) goal_runtime: GoalRuntimeState, // Goal 续期运行时
    pub(crate) services: SessionServices,   // 外部服务（MCP/Auth/...）
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>, // 当前活跃 Turn
    pub(crate) mailbox: Mailbox,            // 等待回复的消息邮箱
    // ... 还有 ~10 个字段
}

pub(crate) struct SessionConfiguration {
    pub(super) collaboration_mode: CollaborationMode, // Plan/Default/Auto
    pub(super) model_reasoning_summary: ...,
    pub(super) approval_policy: ApprovalPolicy,       // 审批策略
    pub(super) developer_instructions: Option<String>,
    pub(super) cwd: PathBuf,                          // 工作目录
    // ... 还有 ~15 个字段
}
```

**阅读策略**：
1. 先读 `Session` struct 的每个字段，理解用途
2. 再读 `SessionConfiguration` 字段
3. 最后读 `Session::new()` 初始化过程（跳过细节）

---

### 7.2 `src/session/mod.rs`（3341 行，核心！）

**目的**：这是 Session 最主要的实现文件，包含所有关键方法。

```
重点阅读块（按顺序）：

Block 1：公开类型定义（前 200 行）
  Codex struct               ← Session 的"外观"，持有 Arc<Session>
  CodexSpawnArgs             ← 创建 Session 的参数
  SessionState               ← Session 内部可变状态

Block 2：Codex 公开方法（200-500 行）
  Codex::submit()            ← ★ 接收 Op，路由给 Session
  Codex::is_idle()           ← 判断 Session 是否空闲

Block 3：Session 核心方法（500-1500 行）
  Session::start_turn()      ← ★ 启动一个新 Turn
  Session::build_prompt()    ← ★ 构建发给 AI 的 Prompt
  Session::inject_context()  ← 注入系统 context 片段
  Session::get_tool_specs()  ← 获取当前可用工具列表

Block 4：状态更新方法（1500+ 行）
  Session::update_config()   ← 热更新配置
  Session::get_thread_goal() ← Goal 相关
  Session::create_thread_goal()
  Session::set_thread_goal()
  ...
```

**阅读策略**：
- 先 `Cmd+F` 搜索 `pub(crate) fn` 列出所有方法
- 重点读 `start_turn`、`build_prompt`、`get_tool_specs`
- 其他方法看函数签名和注释即可

---

### 7.3 `src/session/turn.rs`（2213 行，最核心！）

**目的**：理解一个完整的 AI Turn 是如何执行的。

```
这是整个 core 最重要的文件之一。一个 Turn 的执行流程：

Session::run_turn() 主函数：
  │
  ├── 1. 构建 input（用户消息 + 历史 + context 片段）
  │      build_turn_input()
  │
  ├── 2. 调用 Responses API
  │      client.stream_response(prompt)
  │
  ├── 3. 流式处理响应
  │      loop {
  │        match event {
  │          ResponseEvent::TextDelta → 实时输出文本
  │          ResponseEvent::ToolCall  → 收集工具调用
  │          ResponseEvent::Done      → 进入工具执行
  │        }
  │      }
  │
  ├── 4. 执行工具调用（ToolRouter）
  │      tool_router.dispatch_tool_calls(tool_calls)
  │      ← 审批 → 沙箱 → 执行 → 收集结果
  │
  ├── 5. 提交工具结果
  │      追加 function_output 到 input
  │      再次调用 API（loop 继续）
  │
  └── 6. Turn 结束
         emit TurnFinished event
         GoalRuntimeEvent::TurnFinished（触发续期检查）

关键函数（按重要性）：
  run_turn()                 ← ★ 主循环
  build_turn_input()         ← ★ 构建 Prompt input
  handle_response_stream()   ← ★ 处理 SSE 流
  dispatch_tool_calls()      ← 在 ToolRouter 中
```

**阅读策略**：
1. 先读 `run_turn()` 的整体结构（跳过细节，看 `match`/`if let`/注释）
2. 再读 `build_turn_input()` 理解 Prompt 是怎么组装的
3. 最后读 `handle_response_stream()` 理解流式响应处理

---

### 7.4 `src/session/handlers.rs`（~963 行）

**目的**：理解 Op 消息如何被处理（用户消息/审批/中断）。

```
关键 Op 处理函数：
  handle_user_turn_input()   ← 用户发消息
    └── 检查是否 idle → 启动 run_turn()

  handle_submit_approval()   ← 用户审批工具调用
    └── 将审批结果写入 mailbox → 解除等待中的 Turn

  handle_interrupt()         ← 用户中断
    └── 取消 CancellationToken → Turn 中止

  handle_steer()             ← 用户在 Turn 运行中发消息（Plan 模式）
    └── 将消息加入 idle_pending_input
```

---

## 8. 第四阶段：工具系统（5 个目录/文件）

### 8.1 `src/tools/router.rs` — ToolRouter（最重要）

**目的**：理解工具调用如何被分发给各处理器。

```rust
// ToolRouter 是工具分发的核心
pub struct ToolRouter {
    registry: ToolRegistry,       // 工具注册表（name → handler）
    orchestrator: ToolOrchestrator, // 审批 + 沙箱
}

// 关键方法：
impl ToolRouter {
    pub async fn dispatch_tool_calls(
        &self,
        tool_calls: Vec<FunctionCall>,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
    ) -> Vec<ToolResult>;
    // 内部：
    //   parallel::run_parallel_tool_calls(tool_calls)
    //   → 每个工具：orchestrator.approve_and_run()
    //             → handler.handle(invocation)
}
```

**分发流程**：
```
tool_calls (来自 AI 响应)
    │
    ▼
ToolRouter::dispatch_tool_calls()
    │
    ├── parallel::run_parallel_tool_calls()  ← 并发执行
    │
    └── 对每个 tool_call：
        │
        ├── 1. 查找 handler
        │      registry.get_handler(tool_name)
        │
        ├── 2. 审批检查（orchestrator）
        │      ApprovalPolicy::check() → Auto/Suggest/Full/Guardian
        │
        ├── 3. 选择沙箱
        │      sandboxing::select_sandbox()
        │
        └── 4. 执行
               handler.handle(ToolInvocation)
```

---

### 8.2 `src/tools/registry.rs` — ToolRegistry

**目的**：理解工具是如何被注册的。

```rust
// ToolRegistry 本质是一个 HashMap
pub struct ToolRegistry {
    handlers: HashMap<ToolName, Box<dyn ToolHandler>>,
}

// 注册过程（在 Session 初始化时）：
registry.register(ShellHandler);
registry.register(ApplyPatchHandler);
registry.register(GetGoalHandler);    // 仅 Feature::Goals 启用时
registry.register(CreateGoalHandler); // 仅 Feature::Goals 启用时
registry.register(UpdateGoalHandler); // 仅 Feature::Goals 启用时
registry.register(McpToolHandler);    // 每个 MCP 工具一个
// ... 约 20+ 个工具

// ToolHandler trait：
pub trait ToolHandler: Send + Sync {
    fn tool_name(&self) -> ToolName;
    fn kind(&self) -> ToolKind;
    async fn handle(&self, invocation: ToolInvocation)
        -> Result<Self::Output, FunctionCallError>;
}
```

---

### 8.3 `src/tools/orchestrator.rs` — 审批与沙箱编排

**目的**：理解工具执行前的审批和沙箱选择逻辑。

```
orchestrator.approve_and_run(tool_call) 流程：

  ApprovalPolicy 检查：
  ├── Full/Auto        → 直接执行（无需用户确认）
  ├── Suggest          → 发送 ApprovalRequest 事件 → 等待用户响应
  ├── Guardian         → 启动 Guardian AI 审核 → AI 决定是否批准
  └── MCP Elicitation  → 通过 MCP 协议请求审批

  批准后：
  ├── sandboxing::select_sandbox() → 选择沙箱类型
  └── runtime.execute(request, sandbox) → 实际执行
```

---

### 8.4 `src/tools/handlers/` — 具体工具处理器

**按优先级阅读**：

```
1. handlers/shell.rs        ← shell_exec 工具（最常用）
   ShellHandler::handle()
   → 解析参数 → exec_policy 检查 → exec.rs 执行 → 返回输出

2. handlers/apply_patch.rs  ← apply_patch 工具（文件修改）
   → 解析 patch 格式 → 应用到文件系统

3. handlers/goal.rs         ← goal 工具三件套（已在 07 文档详述）

4. handlers/mcp.rs          ← MCP 工具代理
   → 转发调用给 MCP 服务器 → 返回结果

5. handlers/multi_agents/   ← 子 Agent 管理（spawn/wait/send）
   → 创建子 Thread → 等待结果 → 传递给父 Agent
```

---

### 8.5 `src/tools/spec.rs` — 工具规格

**目的**：理解工具的 JSON Schema 定义如何组装。

```rust
// 工具规格类型：
pub enum ToolSpec {
    Function(ResponsesApiTool), // 普通函数工具
    LocalShell,                 // shell 工具（内置）
    LocalFileSearch,            // 文件搜索（内置）
}

// Session 在每次 Turn 前调用 get_tool_specs() 获取当前可用工具，
// 然后发给 Responses API 的 tools 字段
```

---

## 9. 第五阶段：命令执行与沙箱（3 个模块）

### 9.1 `src/exec.rs`（1597 行）

**目的**：理解实际的命令执行是怎么发生的。

```rust
// 核心函数：
pub async fn exec(
    request: ExecRequest,      // 命令 + 工作目录 + 沙箱参数
    cancellation_token: ...,
    output_tx: Sender<Output>, // 实时输出通道
) -> ExecResult;

// ExecRequest 包含：
struct ExecRequest {
    command: Vec<String>,      // 要执行的命令
    cwd: PathBuf,              // 工作目录
    env: HashMap<String, String>,
    sandbox: SandboxSpec,      // 沙箱参数
    timeout: Duration,
}

// 执行过程：
// 1. spawn_child_async() 创建子进程
// 2. 异步读取 stdout/stderr
// 3. 通过 output_tx 实时发送输出
// 4. 等待进程退出 → 返回 exit_code
```

---

### 9.2 `src/exec_policy.rs`（~1047 行）

**目的**：理解哪些命令被允许，哪些被拒绝，权限路径如何配置。

```
ExecPolicy 主要职责：
  1. 路径白名单检查
     allowed_read_paths  → shell 工具可以读哪些路径
     allowed_write_paths → shell 工具可以写哪些路径

  2. 命令白名单（基于 execpolicy crate）
     危险命令（rm -rf /、格式化磁盘等）→ 直接拒绝

  3. 网络访问策略
     network_policy → 允许/拒绝 shell 命令的网络访问

重要类型：
  SandboxPermissions {
      allowed_read_paths: Vec<PathBuf>,
      allowed_write_paths: Vec<PathBuf>,
      allow_network: bool,
  }
```

---

### 9.3 `src/unified_exec/` — 交互式进程

**目的**：理解 Code Mode 下的交互式进程管理。

```
与普通 exec 的区别：
  exec.rs          → 一次性命令（run and done）
  unified_exec/    → 长期运行的交互式进程（PTY）

unified_exec/process_manager.rs：
  ProcessManager
  ├── open()    ← 创建/复用交互式进程
  ├── send()    ← 发送输入到进程
  ├── read()    ← 读取进程输出
  └── close()   ← 关闭进程

适合先读 mod.rs 的注释，理解整体设计后再看 process_manager.rs。
```

---

## 10. 第六阶段：AI 通信层（2 个文件）

### 10.1 `src/client.rs`（2292 行）

**目的**：理解 Codex 如何与 Responses API 通信。

```
文件开头的注释（第 1-25 行）非常重要，请完整阅读。

核心 struct：
  ModelClient           ← Session 级别，持久化配置
  ModelClientSession    ← Turn 级别，每 Turn 创建一个

核心方法（按阅读顺序）：
  1. ModelClient::new()           ← 初始化
  2. ModelClientSession::new()    ← 创建 Turn 级别客户端
  3. ModelClientSession::stream() ← ★ 发起流式请求
     → 构建 HTTP 请求
     → POST /v1/responses
     → 解析 SSE 流 → 发送 ResponseEvent
  4. WebSocket prewarm 逻辑       ← 性能优化，可跳过

关键类型（来自 client_common.rs）：
  Prompt {
      model: String,
      input: Vec<ResponseInputItem>,  // 对话历史 + 工具结果
      tools: Vec<ToolSpec>,           // 可用工具
      instructions: String,           // 系统提示
      ...
  }

  ResponseEvent {
      TextDelta(String),     // 流式文本输出
      ToolCall(FunctionCall), // AI 要调用的工具
      Done,                  // 本次响应结束
      Error(ApiError),
  }
```

---

### 10.2 `src/stream_events_utils.rs`（~600 行）

**目的**：理解 SSE 流式响应如何被解析为结构化事件。

```
关键函数：
  handle_output_item_done()     ← ★ 处理一个完整的输出项
    ├── text item → emit AgentMessage event
    ├── function_call item → collect for tool dispatch
    └── reasoning item → 处理思维链

  handle_non_tool_response_item() ← 处理非工具响应项

  last_assistant_message_from_item() ← 提取最后的 AI 消息
```

---

## 11. 第七阶段：辅助功能模块（按需阅读）

按需阅读，不需要全部掌握：

### 11.1 `src/goals.rs`（已在文档 07 详述）

关注：`GoalRuntimeState` struct → `goal_runtime_apply()` → 续期触发逻辑

---

### 11.2 `src/guardian/` — AI 辅助审核

适合场景：想理解"Guardian 模式如何用 AI 替代人工审批"

```
阅读顺序：
  1. mod.rs        ← 了解模块结构和公开 API
  2. approval_request.rs ← 审批请求如何构建
  3. review.rs     ← 审核决策流程
  4. review_session.rs   ← Guardian 专用 Session（独立 AI 实例）
  5. prompt.rs     ← Guardian 系统提示词

Guardian 核心逻辑：
  每次工具调用审批时（on-request 策略）：
    1. 构建审批上下文（用户意图 + 当前工具调用）
    2. 创建独立 Guardian Session（调用另一个 AI）
    3. Guardian AI 返回 allow/deny JSON
    4. 根据决策放行或拒绝
```

---

### 11.3 `src/config/mod.rs`（3810 行）

**不需要全读，只需理解 Config 的核心字段**：

```rust
pub struct Config {
    pub model: String,                    // 模型名
    pub approval_policy: ApprovalPolicy,  // 审批策略
    pub sandbox_policy: SandboxPolicy,    // 沙箱策略
    pub mcp_servers: HashMap<String, McpServerConfig>,
    pub instructions: Option<String>,     // 开发者指令
    pub cwd: PathBuf,                    // 工作目录
    // ... 约 30 个字段
}
```

关注 `config/permissions.rs` 理解权限配置的解析。

---

### 11.4 `src/mcp_tool_call.rs`（2138 行）

适合场景：理解 MCP 工具调用全流程

```
关键函数：
  call_mcp_tool()        ← ★ 调用 MCP 服务器工具
    → 查找 MCP 服务器
    → 序列化参数
    → 发送 JSON-RPC 请求
    → 反序列化响应
    → 处理错误/超时
```

---

### 11.5 `src/agent/control.rs`（1318 行）

适合场景：理解多 Agent 协作机制

```
AgentControl 是父 Agent 控制子 Agent 的句柄：
  spawn()       ← 创建子 Agent
  wait()        ← 等待子 Agent 完成
  send_input()  ← 发送输入给子 Agent
  close()       ← 关闭子 Agent

关联数据库表：thread_spawn_edges（父子关系记录）
```

---

### 11.6 `src/context/`（~20 个 Fragment 模块）

**按需阅读，不必全读**：

```
每个 Fragment 都代表一类系统提示片段：
  user_instructions.rs       ← 用户自定义指令注入
  permissions_instructions.rs ← 权限提示注入
  collaboration_mode_instructions.rs ← 协作模式提示
  environment_context.rs     ← 环境信息（OS/shell/git）
  ...

阅读规律：每个 Fragment 都实现 ToResponseInputItem trait，
将自身序列化为 ResponseInputItem 注入到 Prompt 中。
```

---

## 12. 各模块关系总图

```
外部（TUI/exec）
      │ Op
      ▼
ThreadManager ────────────────────────────────────────────┐
      │ 路由 Op                                           │
      ▼                                                   │ Event
CodexThread (通道句柄)                                    │
      │ tx_sub                                           │
      ▼                                                   │
  [Tokio task]                                           │
      │                                                   │
      ▼                                                   │
    Codex ──── Arc<Session> ────────────────────────────────
                    │
          ┌─────────┼──────────┐──────────────────────┐
          │         │          │                       │
          ▼         ▼          ▼                       ▼
    SessionState  features  GoalRuntime          SessionServices
    (config+hist) (flags)   (续期/预算)           (MCP/Auth/...)
          │
          ▼
    run_turn()  ←─── session/turn.rs ─────────────────────────┐
          │                                                     │
          ├── build_prompt()                                    │
          │   ├── context fragments → instructions              │
          │   └── history → input items                        │
          │                                                     │
          ├── client.stream()  ←── client.rs                   │
          │   └── Responses API                                 │
          │                                                     │
          ├── handle_response_stream()                          │
          │   ├── TextDelta → emit AgentMessage event           │
          │   └── ToolCall  → collect                           │
          │                                                     │
          └── ToolRouter::dispatch_tool_calls() ←── tools/     │
              │                                                 │
              ├── orchestrator.approve()                        │
              │   ├── Auto → skip                               │
              │   ├── Suggest → wait mailbox                    │
              │   └── Guardian → review_session                 │
              │                                                 │
              ├── sandboxing::select_sandbox()                  │
              │                                                 │
              └── handler.handle(invocation)                    │
                  ├── ShellHandler → exec.rs → sandbox         │
                  ├── ApplyPatchHandler → file system          │
                  ├── GoalHandler → goals.rs                    │
                  ├── McpHandler → mcp_tool_call.rs             │
                  └── MultiAgentHandler → agent/               │
                                                               │
              工具结果 → 追加到 input → 再次 stream() ──────────┘
```

---

## 13. 阅读时的关键问题清单

阅读每个模块时，思考这些问题：

**骨架层**
- [ ] `CodexThread` 和 `Session` 的区别是什么？谁持有谁？
- [ ] `Op` 和 `Event` 各自的流动方向是什么？

**Session 层**
- [ ] `SessionState` 里的 history 是怎么维护的？
- [ ] `run_turn()` 什么时候会再次调用（递归/循环）？
- [ ] 工具调用结果如何被追加回 input？
- [ ] `CancellationToken` 是如何传递给各个异步操作的？

**工具系统**
- [ ] 工具处理器是如何注册到 `ToolRegistry` 的？
- [ ] `ApprovalPolicy::check()` 的四种结果分别做什么？
- [ ] 并发工具调用如何保证顺序返回结果？

**执行层**
- [ ] `exec.rs` 中的沙箱参数是如何从 `ExecPolicy` 来的？
- [ ] 命令超时是如何实现的？

**通信层**
- [ ] WebSocket prewarm 是什么，为什么做？
- [ ] 流式响应的 `TextDelta` 和 `ToolCall` 如何区分？

---

## 14. 避坑提示

### 坑 1：`impl Session` 分散在多个文件
Session 的方法实现分布在：
- `session/mod.rs`（最多）
- `session/session.rs`（struct 定义）
- `session/turn.rs`（turn 执行）
- `session/handlers.rs`（Op 处理）
- `session/mcp.rs`（MCP 相关）
- `session/multi_agents.rs`（子 Agent 相关）
- `session/review.rs`（Guardian 相关）
- `goals.rs`（Goal 相关，通过 `impl Session`）

**建议**：用 IDE 的"查找所有 `impl Session`"功能。

---

### 坑 2：`*_tests.rs` 文件巨大，不要迷失
`session/tests.rs` 有 **10496 行**，`config/config_tests.rs` 有 **10658 行**。
**不要从这里入手**，测试文件作为验证手段，在理解实现后再回来参考。

---

### 坑 3：`session/mod.rs` 和 `session/session.rs` 容易混淆
- `session/mod.rs` = 模块入口，包含大部分方法实现
- `session/session.rs` = Session struct 的详细字段定义和构造逻辑

---

### 坑 4：`Arc<Session>` 无处不在
几乎所有函数都接收 `Arc<Session>` 或 `&Session`，
因为 Session 需要在多个 Tokio task 间共享。
不需要担心它，把它当普通引用读就好。

---

### 坑 5：工具注册不在一个地方
工具注册散落在：
- `tools/registry.rs` → ToolRegistry 基础注册机制
- `session/mod.rs` → Session 初始化时注册所有工具
- `tools/spec.rs` → 动态工具（MCP）的规格

用 `grep -r "register\|ToolRegistry" src/tools/ src/session/` 找所有注册点。

---

### 坑 6：两套 multi_agents 实现
`handlers/multi_agents/`（v1）和 `handlers/multi_agents_v2/`（v2）同时存在，
读 v2 即可（新功能在 v2 实现）。

---

## 延伸阅读：专题深潜文档（10 ~ 23）

读完本指南、对 core 的整体地图心中有数之后，下列专题文档按子系统纵向钻取，
每篇都配套了 `feature-learn` 分支上的中文源码注释，建议对照源码一起读：

| 文档 | 主题 | 对应 core 子系统 / 关键文件 |
|------|------|------------------------------|
| [10 - 追问 / 打断 / 进度](../2_运行时核心/10_followup_interrupt_progress.md) | 运行中追问、打断、进度汇报 | `session/turn.rs`、`codex_thread.rs` |
| [11 - 三层生命周期](../2_运行时核心/11_thread_session_turn_lifecycle.md) | Thread / Session / Turn 的创建与流转 | `thread_manager.rs`、`session/`、`state/turn.rs` |
| [12 - 长期记忆系统](../6_数据与配置/12_memory_system.md) | 记忆注入、维护与用量统计 | `memory_usage.rs`、SQLite `0016_memory_usage` |
| [13 - 沙箱机制深度解析](../3_执行与安全/13_sandbox_mechanism.md) | 三平台沙箱实现 | `sandboxing/`、`linux-sandbox`、Seatbelt |
| [14 - 多 Agent 系统](../4_工具与多Agent/14_multi_agent_system.md) | 子 Agent 树形协作与权限继承 | `agent/`、`tools/handlers/multi_agents_v2/` |
| [15 - API 与协议层](../5_前端_集成_协议/15_api_protocol_layer.md) | SQ/EQ 双队列、跨语言协议契约 | `protocol/`、`core/src/client.rs` |
| [16 - Agent 优化](../2_运行时核心/16_agent_optimization.md) | 上下文管理与自动压缩 | `context_manager/history.rs`、`compact*.rs` |
| [17 - apply-patch 与 V4A 编辑](../3_执行与安全/17_apply_patch_editing.md) | 补丁解析、模糊定位、落盘与跨回合追踪 | `apply-patch/src/lib.rs`、`tools/handlers/apply_patch.rs`、`tools/runtimes/apply_patch.rs` |
| [18 - 命令执行与安全](../3_执行与安全/18_exec_and_safety.md) | execpolicy / safety / 审批 / 沙箱 / Guardian 自动审查 | `exec.rs`、`exec_policy.rs`、`safety.rs`、`guardian/` |
| [19 - MCP 双向集成](../4_工具与多Agent/19_mcp_integration.md) | 既当 MCP 客户端又当 MCP 服务器，及多层审批链 | `mcp.rs`、`mcp_tool_call.rs`、`codex-mcp`、`mcp-server` |
| [20 - app-server 集成层](../5_前端_集成_协议/20_app_server_layer.md) | IDE / 远控经 JSON-RPC 落到内核 `Op` | `app-server/`、`app-server-protocol/` |
| [21 - 配置系统](../6_数据与配置/21_config_system.md) | 分层配置、Profile、优先级与不可覆盖约束 | `core/src/config/`、`codex-config` |
| [22 - 认证与登录](../5_前端_集成_协议/22_auth_and_login.md) | `CodexAuth` / `AuthManager`：四种认证模式 | `codex-login`、`keyring-store` |
| [23 - 网络代理](../3_执行与安全/23_network_proxy.md) | 本地 HTTP/SOCKS5 代理 + 域名白名单出网管控 | `network_proxy_loader.rs`、`network-proxy` |
