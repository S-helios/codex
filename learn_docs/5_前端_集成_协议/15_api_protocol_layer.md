# 15 - API 与协议层详解

> 文档编号：15 | 类型：深度解析 | 适合读者：API/协议开发、Agent 集成、core 贡献者
>
> 本文档回答：客户端（TUI / app-server / 扩展）与 agent 内核到底"说什么语言"？
> 一次用户输入是如何变成一串事件流回到界面的？审批这种"问一句答一句"的交互
> 在异步世界里怎么实现？codex 如何与 OpenAI Responses API 通信？

---

## 目录

1. [为什么需要一个独立的协议层](#1-为什么需要一个独立的协议层)
2. [SQ/EQ 双队列模型：核心抽象](#2-sqeq-双队列模型核心抽象)
3. [Codex 句柄与 actor 模型](#3-codex-句柄与-actor-模型)
4. [Submission 与 Op：输入侧全解](#4-submission-与-op输入侧全解)
5. [Event 与 EventMsg：输出侧全解](#5-event-与-eventmsg输出侧全解)
6. [id 关联：把一次请求的多个事件串起来](#6-id-关联把一次请求的多个事件串起来)
7. [审批的"事件出 + oneshot 回"往返模式](#7-审批的事件出--oneshot-回往返模式)
8. [ResponseItem：与模型对话的数据单元](#8-responseitem与模型对话的数据单元)
9. [ModelClient：与 Responses API 通信](#9-modelclient与-responses-api-通信)
10. [跨语言契约：serde / JsonSchema / TS 代码生成](#10-跨语言契约serde--jsonschema--ts-代码生成)
11. [核心代码路径索引](#11-核心代码路径索引)

---

## 1. 为什么需要一个独立的协议层

Codex 不是一个单体程序，而是一套"内核 + 多前端"的架构：

- **内核**（`codex-core`）：运行 agent 循环、调用模型、执行工具、管沙箱。
- **前端**：终端界面 TUI、`app-server`（给 IDE/桌面端的 JSON-RPC 服务）、扩展层、
  甚至 TypeScript 写的客户端。

这些前端用不同语言、跑在不同进程，却都要驱动同一个内核。于是必须有一份**语言无关、
进程无关的消息契约**——这就是 `codex-protocol` crate（核心文件
[`protocol/src/protocol.rs`](../codex-rs/protocol/src/protocol.rs)）。

它定义了双方往来的全部消息类型，并通过 serde（JSON 序列化）、`JsonSchema`、`TS`
（TypeScript 类型生成）等派生宏，让同一份 Rust 定义自动产出各语言可用的 schema。
**改协议 = 改所有前端的契约**，所以这一层格外强调稳定性与显式版本兼容。

---

## 2. SQ/EQ 双队列模型：核心抽象

协议层最顶层的设计是**双队列**（见文件头注释）：

```
        客户端 (TUI / app-server / 扩展)
              │  ▲
   Submission │  │ Event
  (提交队列SQ) │  │ (事件队列EQ)
              ▼  │
            codex-core 内核 (agent 循环)
```

- **SQ（Submission Queue，提交队列）**：客户端 → 内核的**输入**。每个条目是一个
  `Submission { id, op }`，`op: Op` 描述要做什么。
- **EQ（Event Queue，事件队列）**：内核 → 客户端的**输出**。每个条目是一个
  `Event { id, msg }`，`msg: EventMsg` 描述发生了什么。

为什么用"队列"而不是"函数调用"？因为 agent 的工作天然是**异步、流式、一对多**的：

| 同步函数调用 | SQ/EQ 异步队列 |
|------------|---------------|
| 一次调用一个返回值 | 一次提交 → 任意多个事件 |
| 调用方阻塞等待 | 提交后立刻返回，事件陆续到来 |
| 难以表达"流式增量" | 天然支持 `*Delta` 增量事件 |
| 难以中途打断 | 随时可投 `Op::Interrupt` |

一次"用户发一句话"在 EQ 上可能展开成几十个事件：回合开始 → 推理增量 → 工具调用开始
→ 命令输出流 → 工具调用结束 → 助手消息增量 → 回合完成 → token 计量更新……

---

## 3. Codex 句柄与 actor 模型

协议是数据契约，**驱动它流动的是 actor 模型**。内核侧的实现核心是 `Codex` 句柄
（[`core/src/session/mod.rs`](../codex-rs/core/src/session/mod.rs)）：

```rust
// 概念示意（非逐字）
pub struct Codex {
    tx_sub: Sender<Submission>,   // 提交队列入口：往里投 Op
    rx_event: Receiver<Event>,    // 事件队列出口：从这里读 Event
    // ...
}
```

`Codex::spawn` 在后台启动一个 **`submission_loop`** 任务
（[`core/src/session/handlers.rs`](../codex-rs/core/src/session/handlers.rs)）：

```
Codex::spawn → spawn_internal
   ├─ 构造 Session（God-object，持有全部会话状态）
   └─ tokio::spawn(submission_loop):
         while let Ok(sub) = rx_sub.recv().await {   // 串行取出下一个提交
             match sub.op.clone() {
                 Op::UserInput { .. }    => user_input_or_turn(..),
                 Op::ExecApproval { .. } => exec_approval(..),   // 内部调 notify_approval
                 Op::Interrupt           => interrupt(..),
                 Op::Shutdown            => shutdown(..),  // 返回 true → 退出循环
                 // ... 其余 Op
             }
         }
```

**关键设计：单消费者串行循环。** 所有提交由同一个 loop 顺序处理，这意味着会话状态
的修改天然串行化——不需要给会话状态加额外的锁就线程安全。这是 codex 并发模型的基石，
也是为什么 `Session` 这个"上帝对象"能持有大量可变状态而不出数据竞争。

> 详见 [11_thread_session_turn_lifecycle.md](../2_运行时核心/11_thread_session_turn_lifecycle.md) 对
> Thread/Session/Turn 三层生命周期的展开。

---

## 4. Submission 与 Op：输入侧全解

### 4.1 Submission 信封

```rust
pub struct Submission {
    pub id: String,              // 关联键：后续所有相关 Event 都带它
    pub op: Op,                  // 动作载荷
    pub trace: Option<W3cTraceContext>,  // 可选的分布式追踪上下文
}
```

`id` 由客户端生成，是把"一次提交"和"它引发的一串事件"绑定的纽带（见 §6）。

### 4.2 Op 操作分类

`Op` 是 codex 对外暴露的**全部动作类型**（`#[non_exhaustive]`，未来可扩展）。按用途
分为几族：

| 族 | 代表变体 | 作用 |
|----|---------|------|
| **启动/驱动回合** | `UserInput`、`Review`、`RunUserShellCommand`、`Compact` | 让 agent 干活；`UserInput` 最常用，可顺带改线程设置 |
| **审批/交互回程** | `ExecApproval`、`PatchApproval`、`ResolveElicitation`、`UserInputAnswer`、`RequestPermissionsResponse`、`DynamicToolResponse` | "事件问一句 → 用户答一句"的回程（见 §7） |
| **线程设置/生命周期** | `ThreadSettings`、`SetThreadMemoryMode`、`ThreadRollback`、`ReloadUserConfig`、`Shutdown` | 改配置、按轮回滚、重载、关停 |
| **实时语音流** | `RealtimeConversationStart/Audio/Text/Close/ListVoices` | realtime 语音会话 |
| **控制** | `Interrupt`、`CleanBackgroundTerminals` | 中断当前任务 / 清理后台 shell |

几个值得展开的变体：

**`UserInput`** —— 不仅是"发消息"，它把**多件事打包**进一次提交：
```rust
Op::UserInput {
    items,                          // 用户输入（文本、图片等）
    environments,                   // 可选：本回合的工作环境
    final_output_json_schema,       // 可选：约束本回合最终输出的 JSON Schema
    responsesapi_client_metadata,   // 可选：透传给 Responses API 的元数据
    additional_context,             // 客户端附带的上下文片段
    thread_settings,                // 在输入前先应用的持久化设置覆盖
}
```
这种"设置 + 输入"合并提交的设计，是为了让 app-server 能**保持调用方的顺序**——
设置变更和回合启动走同一条提交队列，不会出现"设置还没生效就开始回合"的竞态。

**`ThreadRollback { num_turns }`** —— 丢弃内存上下文里最近 N 个用户轮，但**不动磁盘**
（本地文件改动需客户端自己撤销）。这是"replay-to-rebuild"语义，详见 §8 与 ch.16。

**`Interrupt`** —— 中断当前任务但**不**杀后台终端进程；内核以 `EventMsg::TurnAborted`
回应。要终止后台 shell 得用 `CleanBackgroundTerminals`。

---

## 5. Event 与 EventMsg：输出侧全解

### 5.1 Event 信封

```rust
pub struct Event {
    pub id: String,      // 回指引发它的那次 Submission
    pub msg: EventMsg,   // 事件载荷
}
```

### 5.2 EventMsg 分类

`EventMsg` 变体极多，但按用途归类就清晰了：

| 族 | 代表变体 | 说明 |
|----|---------|------|
| **回合生命周期** | `TurnStarted`、`TurnComplete`、`TurnAborted`、`SessionConfigured` | 划定一个回合的边界 |
| **模型产出（多为流式）** | `AgentMessage(+Delta)`、`AgentReasoning*`、`UserMessage` | 助手文本、推理/思维链；`UserMessage` 回显发给模型的输入 |
| **工具执行进度** | `ExecCommandBegin/End`、`McpToolCallBegin/End`、`WebSearchBegin/End`、`PatchApply*`、`ImageGeneration*` | 多成对出现，便于前端画进度条 |
| **审批/交互请求** | `ExecApprovalRequest`、`ApplyPatchApprovalRequest`、elicitation 类 | 需要用户回话，对应 `Op` 的回程变体 |
| **上下文与计量** | `ContextCompacted`、`ThreadRolledBack`、`TokenCount` | 压缩发生、回滚发生、token 用量更新 |
| **诊断** | `Error`、`Warning`、`GuardianWarning`、`ModelReroute`、`ModelVerification` | 错误/警告/模型改路 |

**一条重要约束**（源码顶部英文警告）：`EventMsg` 各变体内嵌的字段**不可用 `Option`**，
否则会打乱扩展层的跨语言代码生成。这是协议演进时的隐形雷区。

**v1/v2 线格式兼容**：部分变体用 `#[serde(rename = "task_started", alias = "turn_started")]`
同时接受新旧名字。例如 `TurnStarted` 在 v1 写作 `task_started`，v2 写作 `turn_started`，
内核两者都认。这是"只增不破"协议演进的典型手法。

---

## 6. id 关联：把一次请求的多个事件串起来

SQ/EQ 是两条独立的流，靠 `id` 把它们"缝"在一起：

```
客户端                                内核
  │                                    │
  │  Submission{ id="abc", op=UserInput }
  │ ─────────────────────────────────▶ │  submission_loop 取出，spawn 一个 turn
  │                                    │
  │      Event{ id="abc", TurnStarted }│
  │ ◀───────────────────────────────── │
  │      Event{ id="abc", AgentMessageDelta }
  │ ◀───────────────────────────────── │  （流式，可能几十条）
  │      Event{ id="abc", ExecCommandBegin }
  │ ◀───────────────────────────────── │
  │      Event{ id="abc", TurnComplete }│
  │ ◀───────────────────────────────── │
```

前端据此把同一 `id` 的所有事件归并到同一个逻辑请求下渲染。多个回合可以并发（各有各的
`id`），事件交错到达也不会混淆。内部产生的事件（非用户发起，如后台 goal 续期）则用
`next_internal_sub_id()` 生成的内部 id。

---

## 7. 审批的"事件出 + oneshot 回"往返模式

这是协议层最精巧的设计之一，专门解决"异步流里如何实现同步问答"。

**场景**：agent 想执行一条危险命令，需要用户点"允许/拒绝"才能继续。但 agent 循环跑在
内核后台，用户的点击要经过 EQ → 前端 → SQ 绕一大圈回来。怎么让那条 `await` 等到答案？

**解法**（[`core/src/session/mod.rs`](../codex-rs/core/src/session/mod.rs) 的
`request_command_approval`）：

```
agent 执行到危险命令
   │
   ├─ request_command_approval():
   │     1. 取有效审批键 effective_approval_id = approval_id.unwrap_or(call_id)
   │     2. 建一个 oneshot 通道，用 insert_pending_approval 把 tx 端
   │        按 effective_approval_id 存进当前回合的 turn_state.pending_approvals
   │     3. send_event(ExecApprovalRequest{ call_id, approval_id, turn_id, .. })  ← 事件出 (EQ)
   │     4. rx_approve.await                              ← 在这里挂起等答案
   │
   ▼ （时间流逝，用户在界面上点了"允许"）
   │
前端把决定包成 Op::ExecApproval{ id, turn_id, decision } 投回 SQ
   │
submission_loop 取出 → exec_approval handler → notify_approval():
        按 approval_id 找到那个 oneshot 的 tx，把 decision 发进去
   │
   ▼
第 4 步的 rx_approve.await 被唤醒，拿到 decision，agent 继续
```

**关键安全细节**：如果 oneshot 通道意外断开（前端崩了、连接断了），`rx.await` 会得到
`Err`，此时**默认 `Abort`（拒绝）**，绝不会"出错就放行"。安全设计里这叫 fail-closed
（失败时关闭/拒绝），和沙箱网络策略"无可用端点就返回空（断网）"是同一种思路。

补丁审批 `request_patch_approval` 略有不同：它**返回 receiver** 而非自己 `await`，把
"何时等"的控制权交给调用方，便于批量收集多个补丁的审批。

---

## 8. ResponseItem：与模型对话的数据单元

`Op`/`EventMsg` 是 codex **自己**的协议；而和**模型**对话用的是另一套类型
`ResponseItem`（定义在 `codex-protocol` 的 `models` 模块）。它对齐 OpenAI Responses API：

| ResponseItem 变体 | 含义 |
|------------------|------|
| `Message { role, content }` | 用户/助手/开发者/系统消息 |
| `Reasoning { encrypted_content }` | 模型推理（思维链），常加密 |
| `FunctionCall` / `FunctionCallOutput` | 工具调用与其输出（成对） |
| `CustomToolCall` / `CustomToolCallOutput` | 自定义（freeform）工具调用 |
| `LocalShellCall` / `WebSearchCall` / `ImageGenerationCall` | 内建工具调用 |
| `Compaction` / `ContextCompaction` | 压缩产生的摘要项（见 ch.16） |

历史就是一串 `ResponseItem`，由 `ContextManager` 管理
（[`core/src/context_manager/history.rs`](../codex-rs/core/src/context_manager/history.rs)）。
发给模型前要满足 Responses API 的硬约束：**每个 call 必须有配对的 output、每个 output
必须有配对的 call**——否则服务端拒收。这条不变量由 `normalize_history` 兜底维护。

`RolloutItem`（持久化层，见 [`rollout/src/recorder.rs`](../codex-rs/rollout/src/recorder.rs)）
则在 `ResponseItem` 之外又包了 `SessionMeta`、`TurnContext`、`EventMsg` 等，是写进
JSONL 流水账的条目类型。

---

## 9. ModelClient：与 Responses API 通信

协议层定义"说什么"，[`core/src/client.rs`](../codex-rs/core/src/client.rs) 负责"怎么把话
发给模型"。两个核心类型：

- **`ModelClient`**：会话级，持有 provider、auth、模型信息。
- **`ModelClientSession`**：**回合级**——每个回合派生一个。它懒加载一条 **Responses
  over WebSocket** 连接并跨同一回合的多次请求复用，还缓存 `x-codex-turn-state` 做粘性
  路由（让同一回合的请求落到同一后端）。

```
ModelClient (会话级)
   └─ 每回合 derive ─▶ ModelClientSession (回合级)
                          ├─ 懒连 Responses WebSocket（首次请求才建）
                          ├─ 缓存 turn-state 粘性路由头
                          └─ stream(prompt) ─▶ 流式产出 ResponseEvent
```

**为什么回合级而非会话级连接？** 因为模型、推理强度、工具集等都可能逐轮变化
（见 [`turn_context.rs`](../codex-rs/core/src/session/turn_context.rs)），把连接绑定到
回合便于隔离这些差异，也便于回合结束时干净地释放资源。

模型的流式输出（文本增量、推理增量、工具调用）被翻译成内核内部事件，再映射成
`EventMsg` 经 EQ 发给前端——这就接回了 §5。

---

## 10. 跨语言契约：serde / JsonSchema / TS 代码生成

协议类型上密集的派生宏不是装饰，每一个都服务于"一份定义、多端共享"：

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Display, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
#[strum(serialize_all = "snake_case")]
pub enum EventMsg { /* ... */ }
```

| 派生/属性 | 作用 |
|----------|------|
| `Serialize` / `Deserialize` | serde JSON 编解码——跨进程传输的基础 |
| `#[serde(tag = "type")]` | **内部标签**：JSON 里带 `"type":"user_input"` 字段自描述，前端无需猜类型 |
| `rename_all = "snake_case"` | Rust 的 `UserInput` ↔ JSON 的 `user_input` |
| `JsonSchema` | 生成 JSON Schema，供校验与文档 |
| `TS` | 生成 TypeScript 类型定义，给 TS 端客户端直接用 |
| `Display`（strum） | 把变体名转字符串，便于日志/telemetry |

**演进纪律**：因为一处定义牵动全链路，协议改动遵循"只增不破"：
- 加变体用 `#[non_exhaustive]` 预留（强制外部 match 写 `_`）。
- 改名用 `#[serde(alias = "...")]` 同时认新旧（如 v1/v2 的 `task_started`/`turn_started`）。
- 删字段前先确认所有前端不再依赖。

---

## 11. 核心代码路径索引

| 主题 | 文件 | 关键符号 |
|------|------|---------|
| 协议总定义 | [`protocol/src/protocol.rs`](../codex-rs/protocol/src/protocol.rs) | `Submission`、`Op`、`Event`、`EventMsg` |
| 与模型对话的数据单元 | `protocol/src/models.rs` | `ResponseItem`、`ContentItem` |
| Codex 句柄 / actor | [`core/src/session/mod.rs`](../codex-rs/core/src/session/mod.rs) | `Codex`、`request_command_approval` |
| 提交分派循环 | [`core/src/session/handlers.rs`](../codex-rs/core/src/session/handlers.rs) | `submission_loop`、`exec_approval` |
| 线程门面 | [`core/src/codex_thread.rs`](../codex-rs/core/src/codex_thread.rs) | `CodexThread::submit`、`next_event` |
| 与 Responses API 通信 | [`core/src/client.rs`](../codex-rs/core/src/client.rs) | `ModelClient`、`ModelClientSession` |
| 历史规范化 | [`core/src/context_manager/history.rs`](../codex-rs/core/src/context_manager/history.rs) | `normalize_history`、`for_prompt` |
| 持久化条目 | [`rollout/src/recorder.rs`](../codex-rs/rollout/src/recorder.rs) | `RolloutItem`、`RolloutRecorder` |

---

> **上一篇**：[14 - 多 Agent 系统](../4_工具与多Agent/14_multi_agent_system.md)
> **下一篇**：[16 - Agent 优化：上下文管理与压缩](../2_运行时核心/16_agent_optimization.md)
