# App-Server 集成层与 JSON-RPC 协议架构

> 本文档回答：IDE / 桌面端 / 远程控制是怎么跟 Codex 内核对话的？
> 它们说的"协议"是什么？一条 `thread/start` 请求从客户端落到内核的 `Op::UserInput`、
> 再把内核事件流回客户端，中间经过哪些层？以及这套 JSON-RPC 为什么"不太标准"。

App-server 是 Codex 内核（`codex-rs/core`，见 doc02 / doc04）之上的**集成层**：它把内核暴露的
`Op`（提交）/ `Event`（事件）模型，包装成一套基于 JSON-RPC 的、可跨进程跨网络的协议，供
TUI、桌面应用、远程控制等多种前端复用。理解它的关键是把握一条主线——
**协议方法（`thread/start`）→ `ClientRequest` 变体 → Processor → 核心 `Op` → 内核事件 → `ServerNotification` → 推回客户端**。

---

## 目录

1. [整体分层与 crate 地图](#1-整体分层与-crate-地图)
2. [协议基础与消息格式](#2-协议基础与消息格式)
3. [JSON-RPC 请求与响应](#3-json-rpc-请求与响应)
4. [JSON-RPC 通知](#4-json-rpc-通知)
5. [JSON-RPC 方法映射到核心 Op](#5-json-rpc-方法映射到核心-op)
6. [传输层架构](#6-传输层架构)
7. [进程模型与生命周期（daemon）](#7-进程模型与生命周期daemon)
8. [请求分发与处理流程](#8-请求分发与处理流程)
9. [请求序列化（Serialization Scope）](#9-请求序列化serialization-scope)
10. [协议版本设计与实验性 API](#10-协议版本设计与实验性-api)
11. [端到端流程图](#11-端到端流程图)
12. [易误解点与设计取舍](#12-易误解点与设计取舍)
13. [源码路径索引](#13-源码路径索引)

---

## 1. 整体分层与 crate 地图

App-server 被拆成多个 crate，职责分离清晰：

| crate | 职责 |
|-------|------|
| `app-server-protocol` | **协议定义**：JSON-RPC 消息壳、`ClientRequest` / `ServerRequest` / `ServerNotification` / `ClientNotification` 全部方法与类型；ts-rs / JSON Schema 导出 |
| `app-server-transport` | **传输层**：`stdio:// / unix:// / ws://` 三种接入方式，把字节流解析成 `JSONRPCMessage`，再封成 `TransportEvent` |
| `app-server` | **核心服务**：`MessageProcessor` 接收请求、分发给各 Processor、把内核事件映射成通知 |
| `app-server-daemon` | **后台进程生命周期**：`Start/Restart/Stop/Version`，管理 pid / socket 文件 |
| `app-server-client` | 客户端 SDK（连内核用） |
| `app-server-test-client` | 集成测试用客户端 |

数据流的两个方向：

```
                       ┌───────────────────────────────────────────┐
  客户端                │              app-server                     │      内核 core
 (IDE/TUI)             │                                            │   (ThreadManager)
    │  JSONRPCRequest   │  Transport     MessageProcessor   Processor│      Op
    ├──────────────────►│  ───────────► (反序列化为 ────► (TurnProc ─┼──────────►
    │ 'thread/start'    │  TransportEvent  ClientRequest)    等)      │   UserInput
    │                   │                                            │
    │  JSONRPCNotif     │  OutgoingMsg   bespoke_event_      映射     │  EventMsg
    ◄───────────────────┤  ◄─────────── handling.rs    ◄──── 事件流 ◄─┼──────────
    │ 'item/started'    │  (推送队列)                                 │  (Channel B)
                       └───────────────────────────────────────────┘
```

> 内核侧的 `Op` / `Event` / `ThreadManager` / Channel B 详见 doc02、doc04、doc11。
> 本文聚焦"内核之上"的协议与集成层。

---

## 2. 协议基础与消息格式

### 2.1 "不太标准"的 JSON-RPC

第一个会让人困惑的点：**Codex 的 JSON-RPC 并不完全遵循 JSON-RPC 2.0**。
文件开头第一句话就直说了（`app-server-protocol/src/jsonrpc_lite.rs:1-2`）：

```rust
//! We do not do true JSON-RPC 2.0, as we neither send nor expect the
//! "jsonrpc": "2.0" field.
```

也就是说：消息体里**既不发送、也不期望** `"jsonrpc": "2.0"` 字段。
文件里虽然定义了 `pub const JSONRPC_VERSION: &str = "2.0";`（`jsonrpc_lite.rs:11`），
但它只是个常量，并不会被序列化进消息。这是早期决策的历史包袱——客户端协议已经成型，
强行补字段反而会破坏兼容性，于是干脆把这层"lite"化。文件名 `jsonrpc_lite.rs` 也点明了这一点。

### 2.2 四种线上消息：`JSONRPCMessage`

线上传输的最外层是一个 `#[serde(untagged)]` 的枚举（`jsonrpc_lite.rs:35-42`）：

```rust
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),        // 期望响应
    Notification(JSONRPCNotification), // 不期望响应
    Response(JSONRPCResponse),      // 成功响应
    Error(JSONRPCError),            // 错误响应
}
```

`untagged` 意味着没有显式判别字段，serde 靠"哪个变体能成功反序列化"来区分：
有 `method` 且有 `id` → Request；有 `method` 无 `id` → Notification；
有 `result` → Response；有 `error` → Error。

### 2.3 `RequestId`：字符串或整数

请求 ID 可以是两种类型之一（`jsonrpc_lite.rs:13-21`）：

```rust
#[serde(untagged)]
pub enum RequestId {
    String(String),
    #[ts(type = "number")]
    Integer(i64),
}
```

这是 JSON-RPC 规范本身允许的（id 可为 string 或 number）。但它的后果是：
**全局反序列化必须同时容纳两种变体，所有处理 id 的业务逻辑都得能应付 string 和 integer**。
为统一打印，它实现了 `Display`（`jsonrpc_lite.rs:23-30`）把两种都格式化成文本。

---

## 3. JSON-RPC 请求与响应

### 3.1 四个壳结构体

| 结构体 | 字段 | 说明 |
|--------|------|------|
| `JSONRPCRequest` | `id` + `method` + `params?` + `trace?` | `jsonrpc_lite.rs:46-56` |
| `JSONRPCNotification` | `method` + `params?` | `jsonrpc_lite.rs:60-65`，**无 id** |
| `JSONRPCResponse` | `id` + `result` | `jsonrpc_lite.rs:69-72` |
| `JSONRPCError` | `id` + `error` | `jsonrpc_lite.rs:76-79` |

几个值得注意的细节：

- **`result` 类型是 `serde_json::Value`**（`jsonrpc_lite.rs:32`：`pub type Result = serde_json::Value;`）。
  也就是说协议壳层对成功响应的负载不做强类型约束——强类型在更上层的 `ClientRequest` 响应类型里。
- **`Response` 和 `Error` 共享 `id`**：客户端靠 `id` 把响应匹配回原请求，无论成功失败。
- **`JSONRPCRequest` 多了一个 `trace` 字段**（`jsonrpc_lite.rs:52-55`）：可选的 W3C Trace Context，
  用于分布式链路追踪。这是大纲未提及但确实存在的字段——`process_request()` 会把它读出来塞进
  `RequestContext`（见 §8）。
- `JSONRPCErrorError`（`jsonrpc_lite.rs:81-88`）携带 `code: i64` + `message` + 可选 `data`。

### 3.2 强类型层：`ClientRequest`

壳层只管搬运 JSON，真正的类型安全在 `ClientRequest` 枚举——它由宏 `client_request_definitions!`
展开生成（`app-server-protocol/src/protocol/common.rs:435` 起）。每个变体绑定：
**一个 JSON-RPC 方法字符串 + 参数类型 + 响应类型 + 序列化范围**。例如：

```rust
// common.rs:445-450
ThreadStart => "thread/start" {
    params: v2::ThreadStartParams,
    inspect_params: true,           // 仅部分字段实验性，需逐字段网关检查
    serialization: None,            // 不进序列化队列
    response: v2::ThreadStartResponse,
},
// common.rs:451-456
ThreadResume => "thread/resume" {
    params: v2::ThreadResumeParams,
    inspect_params: true,
    serialization: thread_or_path(params.thread_id, params.path),
    response: v2::ThreadResumeResponse,
},
```

宏负责把这些声明展开成：枚举变体、`method()` / `id()` / `params()` 访问器、
`serialization_scope()`、`experimental_reason()` 等方法，以及 ts-rs / JSON Schema 导出代码。
这种"声明式协议表"的好处是**单一事实来源**——方法名、参数、响应、序列化策略全写在一处，
不会出现"方法表和实现对不上"的漂移。

### 3.3 `ThreadStart` 的参数与响应

`ThreadStartParams`（`app-server-protocol/src/protocol/v2/thread.rs:90-173`）是个大结构体，
承载创建线程时的全部可选覆盖：

| 字段（节选） | 含义 |
|------|------|
| `model` / `model_provider` / `service_tier` | 模型与供应商 |
| `cwd` | 工作目录 |
| `approval_policy` / `approvals_reviewer` | 审批策略与审批路由（见 doc13 审批） |
| `sandbox` / `permissions` | 沙箱模式 / 命名权限档（见 doc13） |
| `environments` | 粘性环境选择（实验性） |
| `dynamic_tools` | 动态工具声明（实验性） |
| `base_instructions` / `developer_instructions` / `personality` | 指令与人格 |

`ThreadStartResponse`（`thread.rs:192-222`）返回的是 `thread: Thread` **加上一组线程级状态快照**——
注意这里大纲说的"线程级 metadata"并不是一个泛化的 metadata 对象，而是若干扁平字段：

```rust
// thread.rs:195-222（节选）
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub cwd: AbsolutePathBuf,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub sandbox: SandboxPolicy,
    pub reasoning_effort: Option<ReasoningEffort>,
    // ... instruction_sources / runtime_workspace_roots / active_permission_profile 等
}
```

**关键认知**：`thread/start` 返回 `ThreadStartResponse` 只代表"线程身份已建立、运行时配置已解析"，
**并不保证内核已经"就绪"或开始干活**。真正驱动内核执行的是后续的 `turn/start`（见 §5）。
这是个常见误解，§12 会展开。

---

## 4. JSON-RPC 通知

### 4.1 通知的两个方向

- **`ServerNotification`**（服务端 → 客户端）：单向广播，不期望回复。这是事件流的主力。
- **`ClientNotification`**（客户端 → 服务端）：单向，由 `client_notification_definitions!` 宏定义
  （`common.rs:1286`）。

此外还有一类**双向请求**：`ServerRequest`（服务端 → 客户端，**期望回复**），
由 `server_request_definitions!` 宏定义（`common.rs:1321` 起，约 8 个方法）。
它用于"内核反过来问客户端"的场景，比如审批：
`item/commandExecution/requestApproval`（`common.rs:1325`）、
`item/fileChange/requestApproval`（`common.rs:1332`）、
`mcpServer/elicitation/request`（`common.rs:1344`）等。
注意区分：`ServerNotification` 是广播无需回复，`ServerRequest` 是请求需回复。

### 4.2 `ServerNotification`：70+ 通知类型

`ServerNotification` 由 `server_notification_definitions!` 宏展开（调用在 `common.rs:1469`，
枚举本体定义在 `common.rs:1251-1257`），变体超过 60 个（实测 1469-1561 区间约 64 个 `=> "wire/name"` 条目）。
每个变体 `=>` 一个线上方法名，并绑定一个 payload 结构体：

```rust
// common.rs:1471-1493（节选）
Error                 => "error"              (v2::ErrorNotification),
ThreadStarted         => "thread/started"     (v2::ThreadStartedNotification),
ThreadStatusChanged   => "thread/status/changed" (...),
TurnStarted           => "turn/started"       (v2::TurnStartedNotification),
TurnCompleted         => "turn/completed"     (v2::TurnCompletedNotification),
ItemStarted           => "item/started"       (v2::ItemStartedNotification),
ItemCompleted         => "item/completed"     (v2::ItemCompletedNotification),
AgentMessageDelta     => "item/agentMessage/delta" (...),
PlanDelta             => "item/plan/delta"    (v2::PlanDeltaNotification),  // 实验性
// ... 还有 reasoning / mcp / account / fs / remoteControl 等大量类别
```

通知大致分几类（按线上前缀）：
- `thread/*`：线程生命周期（started / archived / closed / status/changed …）
- `turn/*`：回合生命周期（started / completed / diff/updated / plan/updated）
- `item/*`：回合内"条目"的流式更新（started / completed / agentMessage/delta / reasoning/* …）
- `command/*`、`process/*`：命令与进程输出流
- `mcpServer/*`、`account/*`、`fs/*`、`model/*`、`remoteControl/*` 等横切类别

宏同时为每个变体生成 `to_params()`（转 JSON）和 `TryFrom<JSONRPCNotification>`（从 JSON 解回），
以及 JSON Schema 导出。

### 4.3 `ErrorNotification.will_retry`：一个重要语义

`ErrorNotification`（`app-server-protocol/src/protocol/v2/notification.rs:41-48`）携带一个布尔
`will_retry`，源码注释把语义说得很清楚：

```rust
pub struct ErrorNotification {
    pub error: TurnError,
    // Set to true if the error is transient and the app-server process will automatically retry.
    // If true, this will not interrupt a turn.
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}
```

**`will_retry == true`** = 这是个瞬时错误，app-server 会自动重试，**不会中断当前 turn**；
客户端应把它当"提示"而非"终态"。`will_retry == false` 才意味着 turn 真的失败了。
客户端如果不区分这个标志，就会在每次可恢复的网络抖动上误报"任务失败"。

---

## 5. JSON-RPC 方法映射到核心 Op

这是 app-server 的核心职责：把协议方法翻译成内核 `Op`。

### 5.1 核心 `Op` 枚举回顾

`Op` 定义在 `protocol/src/protocol.rs:529-702`（`#[non_exhaustive]`，`#[serde(tag = "type")]`）。
完整枚举与生命周期影响见 **doc11 §10**，这里只点出与 app-server 映射最相关的几个：

| `Op` 变体 | 触发它的典型协议方法 |
|-----------|---------------------|
| `UserInput { items, environments, final_output_json_schema, thread_settings, … }` | `turn/start`、`thread/start`（带输入时） |
| `ThreadSettings { thread_settings }` | `thread/settings/update`（仅改设置不发起 turn） |
| `Interrupt` | `turn/interrupt` |
| `ExecApproval` / `PatchApproval` | 审批 `ServerRequest` 的回复 |
| `Compact` 等 | `thread/compact/start` |

`Op::UserInput`（`protocol.rs:559-578`）的设计很关键。源码内联中文注释已点明（`protocol.rs:556-558`）：

> 不只是「发消息」：它把输入项 + 本回合环境 + 输出 JSON Schema 约束 + 透传元数据 + **持久化设置覆盖**
> 「打包」进一次提交。设置与回合启动共用同一条 SQ，从而保持调用方顺序、杜绝「设置还没生效就开始回合」的竞态。

这里的 `thread_settings: ThreadSettingsOverrides`（`protocol.rs:576-577`，`#[serde(flatten)]`）
就是把"改 cwd / model / approval_policy"和"发起 turn"合并到**同一个提交**里——
保证内核按客户端提交顺序处理、避免设置和 turn 启动之间出现竞态窗口。

### 5.2 `turn/start` → `Op::UserInput` 的完整翻译

落点在 `app-server/src/request_processors/turn_processor.rs:380-487` 的 `turn_start_inner()`：

```
turn_start_inner(params: TurnStartParams)
  1. validate_v2_input_limit(&params.input)          // 输入长度上限校验（turn_processor.rs:387）
  2. load_thread(&params.thread_id)                  // 取出 CodexThread（:395）
  3. set_app_server_client_info(...)                 // 记录客户端身份（:401）
  4. parse_environment_selections(params.environments) // 解析环境（:411）
  5. V2UserInput::into_core 逐项映射输入             // v2 输入 → 核心输入（:414-418）
  6. build_thread_settings_overrides(...)            // 组装设置覆盖（:421-440）
  7. let turn_op = Op::UserInput { items, environments,
        final_output_json_schema, responsesapi_client_metadata,
        additional_context, thread_settings }        // 组装 Op（:443-450）
  8. submit_core_op(&request_id, thread, turn_op)    // 提交到内核，拿回 turn_id（:451-458）
  9. record_request_turn_id(&request_id, &turn_id)   // 记录 request↔turn 映射（:472-474）
 10. return TurnStartResponse { turn: Turn { id: turn_id, status: InProgress, … } }
```

`submit_core_op()`（`turn_processor.rs:349-358`）很薄——本质就是
`thread.submit_with_trace(op, trace)`，把 `Op` 投递到内核的提交队列（SQ），
返回值就是内核分配的提交 id，被用作 `turn_id`。

**注意映射的方向性**：`thread/start` 也能携带输入（`turn_has_input` 路径，`turn_processor.rs:460`），
但只有 `turn/start` 是"纯粹的发起回合"。线程级配置（`thread/start` 的 model/cwd/sandbox）和
回合级输入是分两条协议方法、但最终都可能汇成 `Op::UserInput` / `Op::ThreadSettings`。

---

## 6. 传输层架构

### 6.1 三种接入方式 + Off

`AppServerTransport`（`app-server-transport/src/transport/mod.rs:66-72`）枚举了支持的传输：

```rust
pub enum AppServerTransport {
    Stdio,
    UnixSocket { socket_path: AbsolutePathBuf },
    WebSocket { bind_address: SocketAddr },
    Off,
}
```

`from_listen_url()`（`mod.rs:108-152`）负责把 `--listen` 字符串解析成上述变体：

| `--listen` 形式 | 结果 | 典型用途 |
|-----------------|------|---------|
| `stdio://`（默认，`mod.rs:106`） | `Stdio` | TUI / 桌面端通过子进程 stdin/stdout 对话 |
| `unix://`（空路径） | `UnixSocket`，路径取 `app_server_control_socket_path(CODEX_HOME)` | 本机多客户端共享一个 app-server |
| `unix://PATH` | `UnixSocket { PATH }` | 指定 socket 路径 |
| `ws://IP:PORT` | `WebSocket { bind_address }` | 远程控制（Remote Control），配合认证 |
| `off` | `Off` | 不监听（嵌入式 / 仅 in-process） |

> [推测] 大纲把传输列为 `stdio:// / unix://PATH / ws://IP:PORT` 三种，源码里其实还有第四个 `Off`，
> 表示"不对外监听"。

### 6.2 `TransportEvent`：统一事件入口

不管哪种传输，底层都把字节流解析成 `JSONRPCMessage`（`forward_incoming_message`，`mod.rs:194-209`），
再封装成 `TransportEvent` 投递给 `MessageProcessor`。`TransportEvent` 只有三个变体
（`mod.rs:163-178`）：

```rust
pub enum TransportEvent {
    ConnectionOpened  { connection_id, origin, writer, disconnect_sender },
    ConnectionClosed  { connection_id },
    IncomingMessage   { connection_id, message: JSONRPCMessage },
}
```

`ConnectionOrigin`（`mod.rs:180-186`：`Stdio / InProcess / WebSocket / RemoteControl`）标记连接来源，
每个连接被分配一个全局递增的 `ConnectionId`（`mod.rs:188-192`）。这种"先归一成 `JSONRPCMessage`、
再封 `TransportEvent`、统一打上 `connection_id`"的设计，让 `MessageProcessor` **完全不必关心传输细节**——
无论 stdio 还是 WebSocket，进到处理器的都是同一种事件。

---

## 7. 进程模型与生命周期（daemon）

### 7.1 daemon 是独立的"管家"进程

`app-server-daemon`（`app-server-daemon/src/lib.rs`）不是 app-server 本身，而是**管理 app-server 进程生命周期**
的后台守护。它对外提供 `LifecycleCommand`（`lib.rs:36-42`）：

```rust
pub enum LifecycleCommand { Start, Restart, Stop, Version }
```

> 大纲写的是"Start/Stop/Version"，源码里还有 `Restart`（`lib.rs:39`）。

`run()`（`lib.rs:276-292`）对每个命令的处理：`Start/Restart/Stop` 都先 `acquire_operation_lock()`
拿一把操作锁（避免并发启停打架），`Version` 则直接探测。这套设计独立于主 app-server 进程，
通过 socket / pid 文件协调。

### 7.2 文件全部集中在 `CODEX_HOME` 下

`Daemon` 结构体（`lib.rs:250-257`）持有一组路径，全部在 `from_environment()`（`lib.rs:260-274`）里
基于 `find_codex_home()` 解析出来：

```rust
struct Daemon {
    socket_path: PathBuf,         // CODEX_HOME/app-server-control/app-server-control.sock
    pid_file: PathBuf,            // CODEX_HOME/app-server-daemon/app-server.pid
    update_pid_file: PathBuf,     // .../app-server-updater.pid
    operation_lock_file: PathBuf, // .../daemon.lock
    settings_file: PathBuf,       // .../settings.json
    managed_codex_bin: PathBuf,
}
```

常量见 `lib.rs:30-34`（`PID_FILE_NAME` 等）、socket 路径见 `transport/mod.rs:46-56`。
`start()`（`lib.rs:294`）会先 `client::probe(socket)` 探测是否已有实例在跑，
有则返回 `AlreadyRunning`（`lib.rs:296-304`），否则才真正拉起——保证幂等。

**关键依赖**：socket 路径强依赖 `CODEX_HOME`（`transport/mod.rs:115`）。
不同 `CODEX_HOME` 会落到不同 socket，从而是相互隔离的 app-server 实例。

---

## 8. 请求分发与处理流程

### 8.1 `process_request()`：JSON-RPC 入口

`app-server/src/message_processor.rs:521-578` 的 `process_request()` 是处理 `JSONRPCRequest` 的入口：

```
process_request(connection_id, request: JSONRPCRequest, transport, session)
  1. 读 request.method / request.id                    (:528-537)
  2. 提取 request.trace → W3cTraceContext              (:540-543)
  3. 组装 RequestContext（id + span + trace）           (:544)
  4. to_value(&request) → from_value::<ClientRequest>  // 壳 → 强类型（:549-554）
        反序列化失败 → invalid_request 错误
  5. handle_client_request(request_id, codex_request, session, …) (:561-568)
  6. 出错 → outgoing.send_error(...)                    (:572-574)
```

第 4 步是承上启下的关键：先把强类型 `JSONRPCRequest` 序列化回 `Value`、再反序列化成 `ClientRequest`。
这一步把"方法名 + params"转成具体枚举变体，**协议与处理逻辑在此交接**。
（另有 `process_client_request()`，`message_processor.rs:584` 起，给 in-process 嵌入方走类型直通路径，
跳过 JSON 反序列化，但语义一致。）

### 8.2 `handle_client_request()`：初始化握手网关

`handle_client_request()`（`message_processor.rs:749-792`）先特判 `Initialize`：

```
if let ClientRequest::Initialize { .. } = codex_request {
    initialize_processor.initialize(...)              // 握手（:761-771）
    if connection_initialized {
        thread_processor.connection_initialized(...)  // 标记连接就绪（:772-781）
    }
    return;
}
dispatch_initialized_client_request(...)              // 其余请求走这里（:785-791）
```

`dispatch_initialized_client_request()`（`message_processor.rs:794-855`）做三件守门的事：

1. **初始化检查**：`if !session.initialized() { return Err(invalid_request("Not initialized")) }`
   （`:801-803`）——没握手就发其它请求会被拒。
2. **实验性 API 检查**：`if codex_request.experimental_reason().is_some() && !session.experimental_api_enabled()`
   → 拒绝（`:805-809`），见 §10。
3. **计算序列化范围** `serialization_scope()`（`:817`），据此决定入队还是直接 spawn（见 §9）。

### 8.3 `handle_initialized_client_request()`：巨型分发匹配块

`message_processor.rs:857-1248` 是一个超大的 `match codex_request { … }`（约 400 行），
把每个 `ClientRequest` 变体委托给对应的 Processor：

```rust
// message_processor.rs:871-986（节选）
let result = match codex_request {
    ClientRequest::Initialize { .. } => panic!("…handled before dispatch"),
    ClientRequest::ConfigRead { params, .. }      => self.config_processor.read(params).await…,
    ClientRequest::FsReadFile { params, .. }      => self.fs_processor.read_file(params).await…,
    ClientRequest::ThreadStart { params, .. }     => self.thread_processor.thread_start(…).await,
    // … TurnProcessor / EnvironmentProcessor / RemoteControlProcessor / …
};
```

Processor 是按领域切分的（节选）：`config_processor`、`thread_processor`、`turn_processor`（§5）、
`fs_processor`、`environment_processor`、`remote_control_processor`、
`external_agent_config_processor`、`windows_sandbox_processor` 等。每个 Processor 内部再调内核或本地资源，
返回 `Result<Option<ClientResponsePayload>, JSONRPCErrorError>`。这种"入口宏 + 分发匹配 + 领域 Processor"
的三段式，让协议表（`common.rs`）和实现（各 Processor）解耦。

### 8.4 出站：内核事件 → 通知

反方向上，内核的 `EventMsg` 流（Channel B，见 doc11 §3）被
`app-server/src/bespoke_event_handling.rs`（约 146 KB 的大文件）逐条映射成 `ServerNotification`。
例如 `ItemStartedNotification`（`bespoke_event_handling.rs:801` / `:954` / `:1013` / `:1335`）、
`ItemCompletedNotification`（`:963` / `:1022` / `:1395`）、
`RawResponseItemCompletedNotification`（`:1412`）。

> 大纲称这个映射文件为 `event_mapping.rs`，源码里实际叫 `bespoke_event_handling.rs`
> （"bespoke" = 定制化，因为不是机械 1:1 映射，而是逐事件定制）。

映射出的通知交给 `OutgoingMessageSender::send_server_notification()`
（`outgoing_message.rs:553`），它把通知封成 `JSONRPCNotification` 排进出站队列，
再由 transport 推回对应连接。

---

## 9. 请求序列化（Serialization Scope）

### 9.1 为什么需要

App-server 是多连接、异步的：同一个 thread 可能被多个请求并发触达
（比如客户端连发 `thread/settings/update` + `turn/start`）。如果不加约束，
内核状态会被并发改动撕裂。**序列化范围（serialization scope）就是把"同一资源"的变更请求串行化**。

### 9.2 scope 在协议表里声明，运行期映射成队列键

每个 `ClientRequest` 在宏里声明 `serialization:`（`common.rs:90-120` 的 `serialization_scope_expr!`）。
可能的 scope（`ClientRequestSerializationScope`，`common.rs:77-88`）：

| scope | 含义 |
|-------|------|
| `None` | 不序列化，直接并发（如 `thread/start`，因为还没有 thread_id） |
| `thread_id(params.thread_id)` | 按线程串行（大多数 `thread/*`、`turn/*`） |
| `thread_or_path(thread_id, path)` | 有 thread_id 按线程、否则按路径（`thread/resume`、`thread/fork`） |
| `global("key")` | 全局互斥（如 `memory/reset` → `global("memory")`，`common.rs:533`） |
| `global_shared_read("key")` | 全局但读共享 |
| 其它 | `CommandExecProcess` / `Process` / `FsWatch` / `McpOauth` / `FuzzyFileSearchSession` 等细分资源 |

运行期 `RequestSerializationQueueKey::from_scope()`（`app-server/src/request_serialization.rs:53-104`）
把 scope 映射成 `(队列键, 访问模式)`，访问模式只有两种（`request_serialization.rs:47-51`）：
`Exclusive`（独占，写）和 `SharedRead`（共享读）。大多数 scope 都是 `Exclusive`；
只有 `GlobalSharedRead` 映射为 `SharedRead`（`request_serialization.rs:62-64`）——
即"全局读可并发，全局写互斥"的读写锁语义。

回到 §8.2 第 3 步：`dispatch_initialized_client_request()` 拿到 scope 后，
有 scope 就 `request_serialization_queues.enqueue(key, access, request)`（`message_processor.rs:844-848`）；
scope 为 `None` 则直接 `tokio::spawn` 并发执行（`:849-852`）。

**与内核 SQ 的关系**：这里的"序列化队列"是 **app-server 层**的，作用是保证**到达内核之前**的请求顺序；
它和内核自己的提交队列（SQ）是两层不同的串行机制。`thread_id` scope 保证同一线程的变更请求
按客户端提交顺序进入内核，这与 §5.1 `Op::UserInput` "设置与回合共用同一 SQ"是互补的两道保险。

---

## 10. 协议版本设计与实验性 API

### 10.1 v1 / v2 双版本并存

协议类型按版本分目录：`protocol/v1/`（旧）与 `protocol/v2/`（新）。
`ClientRequest` 里 `Initialize` 仍用 `v1::InitializeParams`（`common.rs:437`），
而几乎所有新方法都用 `v2::*`（`ThreadStart` → `v2::ThreadStartParams` 等）。
v2 类型统一带 `#[ts(export_to = "v2/")]`（如 `thread.rs:94`），TS 类型导出到 `v2/` 子目录。

### 10.2 ts-rs：协议类型导出为 TypeScript

所有协议类型都 `#[derive(..., TS)]`（ts-rs），用于**生成 TypeScript 定义**给 JS/TS 客户端
（桌面端等）。这是"协议即代码"的体现——Rust 改了类型，重新导出就能让 TS 端同步，
避免手写两套定义漂移。

### 10.3 实验性字段的网关：`#[experimental(...)]`

新功能不会一上来就稳定。Codex 用 `#[experimental("reason")]` 标注**字段级或方法级**的实验性
（由 `codex_experimental_api_macros::ExperimentalApi` 派生宏驱动，`common.rs:11`）。例如：

```rust
// thread.rs:151-156
#[experimental("thread/start.environments")]
#[ts(optional = nullable)]
pub environments: Option<Vec<TurnEnvironmentParams>>,
#[experimental("thread/start.dynamicTools")]
pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
```

两种粒度：
- **方法级**：整个方法标 `#[experimental("…")]`（如 `ThreadSettingsUpdate`，`common.rs:517`）。
- **字段级**：方法本身稳定，但个别字段实验性 → 宏里用 `inspect_params: true`
  （`common.rs:445-449` 的 `ThreadStart`），运行期逐字段检查
  （`experimental_reason_expr!`，`common.rs:41-54`）。

运行期闸门在 §8.2 第 2 步：`experimental_reason()` 返回 `Some(reason)` 且连接未启用实验 API（
`session.experimental_api_enabled()` 为假）→ 直接 `invalid_request`（`message_processor.rs:805-808`）。
ts-rs 导出时也会据此过滤，未启用实验的客户端看不到这些字段。

---

## 11. 端到端流程图

### 11.1 入站：`thread/start` 落到内核

```
IDE/客户端
   │ JSONRPCRequest { method:"thread/start", id, params }
   ▼
AppServerTransport (stdio:// / unix:// / ws://)
   │ 解析字节 → JSONRPCMessage::Request
   │ 封装 TransportEvent::IncomingMessage { connection_id, message }
   ▼
MessageProcessor::process_request()                      (message_processor.rs:521)
   │ to_value → from_value::<ClientRequest::ThreadStart>  (:549-554)
   ▼
handle_client_request() → 非 Initialize → dispatch_…()   (:749 / :794)
   │ ① session.initialized() 检查      (:801)
   │ ② experimental 网关检查           (:805)
   │ ③ serialization_scope = None      (thread/start 无 thread_id)
   ▼
handle_initialized_client_request()                      (:857)
   │ match → ThreadProcessor::thread_start(params, …)    (:976)
   ▼
ThreadProcessor → ThreadManager（内核）创建线程 / 解析配置
   │ 返回 ThreadStartResponse { thread, model, cwd, sandbox, … }
   ▼
OutgoingMessageSender.send_response(id, response) → 客户端
   （注意：线程"已建立"≠ 内核已就绪干活，真正执行靠后续 turn/start）
```

### 11.2 `turn/start` → `Op::UserInput`（核心翻译）

```
TurnStart request
   │ serialization_scope = thread_id(params.thread_id)   (common.rs)
   ▼
RequestSerializationQueues.enqueue(Thread{id}, Exclusive, req)   (message_processor.rs:844)
   │ 同一 thread 的变更请求串行执行，顺序 = 客户端提交顺序
   ▼
TurnProcessor::turn_start_inner()                        (turn_processor.rs:380)
   │ validate → load_thread → map input → build settings
   │ Op::UserInput { items, environments, schema, thread_settings }  (:443)
   ▼
submit_core_op() → thread.submit_with_trace(op, trace)   (:349)
   ▼
内核 SQ → run_turn() → 产出 EventMsg 流（Channel B）
   │ TurnStarted / ItemStarted / AgentMessageDelta / ItemCompleted / TurnCompleted …
   ▼
bespoke_event_handling.rs：EventMsg → ServerNotification  (:801 起逐事件映射)
   ▼
OutgoingMessageSender::send_server_notification()        (outgoing_message.rs:553)
   │ → JSONRPCNotification { method:"item/started", params }
   ▼
Transport → 推回客户端（流式）
```

### 11.3 daemon 生命周期（unix 平台）

```
codex CLI: app-server-daemon start
   ▼
Daemon::from_environment()  →  解析 CODEX_HOME 下 socket/pid/lock 路径  (lib.rs:260)
   ▼
Daemon::run(Start)
   │ acquire_operation_lock()（daemon.lock，防并发启停）        (lib.rs:279)
   ▼
start()
   │ client::probe(socket)？已在跑 → AlreadyRunning             (lib.rs:296)
   │ 否则拉起 app-server 子进程，写 pid_file，监听 socket_path
   ▼
其它客户端通过 unix:// + 同一 CODEX_HOME 的 socket 连入
```

---

## 12. 易误解点与设计取舍

| 误区 / 取舍 | 真相 | 源码锚点 |
|------|------|---------|
| "它是标准 JSON-RPC 2.0" | **不是**。不发也不期望 `"jsonrpc":"2.0"` 字段，是早期决策的兼容包袱 | `jsonrpc_lite.rs:1-2` |
| "RequestId 总是数字" | 可为 `String` 或 `Integer` 两个变体，反序列化与业务都要兼容两种 | `jsonrpc_lite.rs:13-21` |
| "`thread/start` 成功就能直接干活了" | 只代表线程身份建立、配置解析完成，**无"内核就绪"保证**；真正驱动执行靠 `turn/start` | `thread.rs:192-222`、`turn_processor.rs:380` |
| "改设置和发 turn 是两次独立提交" | `Op::UserInput` 把设置覆盖与回合输入**打包进一次提交**，共用 SQ 保顺序、防竞态 | `protocol.rs:556-577` |
| "`ServerNotification` 需要客户端回复" | 不需要——它是单向广播。需要回复的是 `ServerRequest`（如审批） | `common.rs:1251`、`common.rs:1321` |
| "`error` 通知 = turn 失败" | 看 `will_retry`：`true` 表示 app-server 会自动重试、**不中断 turn** | `notification.rs:43-45` |
| "序列化队列就是内核的 SQ" | 是 **app-server 层**的请求串行（按 thread/path/global），与内核 SQ 是两层；`thread_id` scope → `Exclusive` | `request_serialization.rs:53-104` |
| "传输只有 stdio/unix/ws" | 还有 `Off`（不监听）；且 socket 路径强依赖 `CODEX_HOME` | `transport/mod.rs:66-72`、`:115` |
| "daemon 就是 app-server" | daemon 是独立"管家"进程，靠 socket/pid/lock 文件协调 app-server 生命周期 | `app-server-daemon/src/lib.rs:250-292` |
| "TUI / 桌面 / 远程控制走同一通道" | TUI/桌面多用 `stdio://` 或 `unix://`；远程控制走 `ws://` + 认证，架构上分离 | `transport/mod.rs:108-152` |

### 设计取舍小结

- **"协议即声明表"**：`client_request_definitions!` / `server_notification_definitions!` 等宏把
  方法名、参数、响应、序列化策略、实验性标注全收敛到 `common.rs` 一处。代价是宏可读性差，
  收益是杜绝"协议表与实现漂移"，并能机械导出 TS / JSON Schema。
- **"传输无关"**：transport 统一归一成 `JSONRPCMessage` + `TransportEvent` + `ConnectionId`，
  让 `MessageProcessor` 不感知 stdio / socket / ws 差异。新增传输只需实现 acceptor。
- **"两层串行"**：app-server 序列化队列（到达内核前的顺序）+ 内核 SQ（提交后的顺序），
  共同保证客户端提交顺序在并发下不被打乱。
- **"实验性闸门"**：`#[experimental]` 让新功能能在协议里"先声明、后开放"，
  未启用实验的客户端在 ts-rs 导出和运行期网关两道都看不到 / 用不到，降低了 API 稳定性承诺的压力。

---

## 13. 源码路径索引

> 所有行号基于 `feature-learn-v2` 当前快照，已逐一 Read / grep 核对。

| 主题 | 文件:行 |
|------|---------|
| JSON-RPC lite 说明（不发 jsonrpc 字段） | `app-server-protocol/src/jsonrpc_lite.rs:1-11` |
| `RequestId`（String / Integer） | `app-server-protocol/src/jsonrpc_lite.rs:13-30` |
| `JSONRPCMessage` 四变体 | `app-server-protocol/src/jsonrpc_lite.rs:35-42` |
| 四个壳结构体 + `Result=Value` | `app-server-protocol/src/jsonrpc_lite.rs:32, 46-88` |
| `ClientRequestSerializationScope` 枚举 | `app-server-protocol/src/protocol/common.rs:77-88` |
| `client_request_definitions!` 调用（ThreadStart 等） | `app-server-protocol/src/protocol/common.rs:435-549` |
| `ServerNotification` 枚举展开 | `app-server-protocol/src/protocol/common.rs:1251-1257` |
| `server_notification_definitions!` 调用（~64 通知） | `app-server-protocol/src/protocol/common.rs:1469-1561` |
| `server_request_definitions!`（审批等 ~8 个） | `app-server-protocol/src/protocol/common.rs:1321-1359` |
| `ThreadStartParams` | `app-server-protocol/src/protocol/v2/thread.rs:90-173` |
| `ThreadStartResponse` | `app-server-protocol/src/protocol/v2/thread.rs:192-222` |
| `ErrorNotification.will_retry` | `app-server-protocol/src/protocol/v2/notification.rs:41-48` |
| `Op` 枚举 / `Op::UserInput` | `protocol/src/protocol.rs:529-578` |
| `process_request()`（JSON-RPC 入口） | `app-server/src/message_processor.rs:521-578` |
| `handle_client_request()`（Initialize 网关） | `app-server/src/message_processor.rs:749-792` |
| `dispatch_initialized_client_request()`（守门 + 入队） | `app-server/src/message_processor.rs:794-855` |
| `handle_initialized_client_request()`（巨型分发） | `app-server/src/message_processor.rs:857-1248` |
| `turn_start_inner()`（→ Op::UserInput） | `app-server/src/request_processors/turn_processor.rs:380-487` |
| `submit_core_op()` | `app-server/src/request_processors/turn_processor.rs:349-358` |
| `from_scope()`（scope → 队列键 + 访问模式） | `app-server/src/request_serialization.rs:47-104` |
| `OutgoingMessageSender::send_server_notification()` | `app-server/src/outgoing_message.rs:553` |
| 事件 → 通知映射（定制化） | `app-server/src/bespoke_event_handling.rs:801, 954, 1013, 1335, 1395` |
| `AppServerTransport` / `from_listen_url()` | `app-server-transport/src/transport/mod.rs:66-152` |
| `TransportEvent` / `ConnectionOrigin` | `app-server-transport/src/transport/mod.rs:163-186` |
| socket 路径（依赖 CODEX_HOME） | `app-server-transport/src/transport/mod.rs:46-56, 115` |
| daemon `LifecycleCommand` / `Daemon` / `run` | `app-server-daemon/src/lib.rs:36-42, 250-292` |

### 交叉引用

- 内核架构、`Op`/`Event` 模型：**doc02**、**doc04**
- Thread / Session / Turn 生命周期 与 `Op` 全枚举：**doc11**（尤其 §10）
- 沙箱、审批策略、权限档：**doc13**
