# MCP 双向集成（Client + Server）

> 一句话主旨：Codex 既是 **MCP 客户端**（连接外部 MCP server，把它们的工具暴露给模型调用），又是 **MCP 服务器**（把整个 Codex agent 当作一个工具暴露给上层 host），两个方向共享 `codex-mcp` 这套底层连接/协议基建，并通过一条多层审批链把"模型想调外部工具"这件事约束在用户授权范围内。

本文聚焦 MCP（Model Context Protocol）在 Codex 中的两个方向、工具发现与调用链路、审批授权机制，以及与核心工具系统（见 [doc09 阅读指南](../0_导览/09_codex_core_reading_guide.md)）、生命周期（见 [doc11](../2_运行时核心/11_thread_session_turn_lifecycle.md)）、沙箱（见 [doc13](../3_执行与安全/13_sandbox_mechanism.md)）的衔接。

---

## 目录

1. [架构概览](#1-架构概览)
2. [双向角色分工](#2-双向角色分工)
3. [客户端方向：MCP 工具发现与调用链路](#3-客户端方向mcp-工具发现与调用链路)
4. [服务端方向：Codex 工具暴露与配置](#4-服务端方向codex-工具暴露与配置)
5. [工具审批与授权机制](#5-工具审批与授权机制)
6. [配置与依赖管理](#6-配置与依赖管理)
7. [关键数据结构与流程](#7-关键数据结构与流程)
8. [与核心工具系统的关系](#8-与核心工具系统的关系)
9. [常见问题](#9-常见问题)

---

## 1. 架构概览

MCP 是一个基于 JSON-RPC 的协议，约定了"工具列表（tools/list）、工具调用（tools/call）、资源（resources）、引导（elicitation）"等消息。Codex 围绕它拆成几个 crate：

| Crate / 模块 | 角色 | 关键文件 |
|------|------|---------|
| `codex-rs/codex-mcp/` | MCP **客户端**底层：连接池、RMCP 客户端封装、配置解析、引导请求 | `connection_manager.rs`、`rmcp_client.rs`、`mcp/mod.rs`、`elicitation.rs` |
| `codex-rs/core/src/mcp*.rs` | 客户端方向在 core 内的**业务编排**：工具调用入口、审批、暴露策略、技能依赖 | `mcp_tool_call.rs`、`mcp_tool_exposure.rs`、`mcp_tool_approval_templates.rs`、`mcp_skill_dependencies.rs`、`mcp.rs` |
| `codex-rs/core/src/session/mcp.rs` | Session 层的 MCP 原语：`call_tool`、引导请求、热刷新 | `session/mcp.rs` |
| `codex-rs/mcp-server/` | MCP **服务器**：把 Codex 自己当工具暴露给上层 host | `lib.rs`、`codex_tool_config.rs`、`message_processor.rs` |

整体数据流（客户端方向，简化）：

```
 配置(config.toml + 插件) ──► McpConfig ──► effective_mcp_servers()
                                                │  （含运行时注入的 codex_apps）
                                                ▼
                                    McpConnectionManager::new()
                                    并发启动每个 server (AsyncManagedClient)
                                                │
        ┌───────────────────────────────────────┼─────────────────────────┐
        ▼                                        ▼                         ▼
  list_all_tools()                         call_tool()              引导(elicitation)
   聚合所有 server 工具                    路由到具体 server          双向请求用户输入
        │                                        ▲
        ▼                                        │
 build_mcp_tool_exposure()              handle_mcp_tool_call()
  直接暴露 / 延迟暴露                     审批 → 执行 → 发事件
        │                                        ▲
        ▼                                        │
   工具列表注入模型 ────────────► 模型发起 tool call
```

> 命名约定：`codex-mcp` 把"对外暴露给模型的工具名"做了 sanitize（Responses API 要求 `^[a-zA-Z0-9_-]+$`），并可选加 `mcp__<server>__` 前缀。见 `codex-mcp/src/mcp/mod.rs:393-411` 的 `sanitize_responses_api_tool_name` 与 `:63-67` 的 `qualified_mcp_tool_name_prefix`。

---

## 2. 双向角色分工

Codex 与 MCP 的关系是**对称的双向**，很容易混淆，先用一张表钉死：

| 方向 | Codex 扮演 | 对端 | 谁发起 tools/call | 入口文件 |
|------|-----------|------|------------------|---------|
| **客户端**（consumer） | MCP **client** | 外部 MCP server（GitHub、文件系统、第三方 app…） | Codex 内的模型 | `core/src/mcp_tool_call.rs`、`codex-mcp/` |
| **服务端**（provider） | MCP **server** | 上层 host（IDE 插件、其他 agent…） | 外部 host | `mcp-server/src/lib.rs` |

- **客户端方向**是日常主线：模型在一次 turn 中想用某个外部能力（比如查 GitHub issue），Codex 通过 `McpConnectionManager` 把请求转给对应 MCP server，拿到结果再喂回模型。
- **服务端方向**让"整个 Codex agent"变成别人的一个工具：上层 host 通过 stdin/stdout 跑 `codex mcp` 子命令，发一个 `codex` 工具调用（带 prompt、approval policy、sandbox 模式等参数），Codex 在内部跑完一整轮 agent loop 再把结果作为 MCP 响应返回。

两个方向都复用 `codex-mcp` crate 暴露的核心类型（`McpConnectionManager`、`ToolInfo`、`EffectiveMcpServer` 等，见 `codex-mcp/src/lib.rs:1-64`），但服务端方向额外用 `mcp-server` crate 跑 JSON-RPC 主循环。

---

## 3. 客户端方向：MCP 工具发现与调用链路

### 3.1 连接管理：`McpConnectionManager`

`McpConnectionManager`（`codex-mcp/src/connection_manager.rs:104-113`）是 MCP server 连接池，结构体本身很薄：

```rust
pub struct McpConnectionManager {
    clients: HashMap<String, AsyncManagedClient>,   // 每个 server 一个异步客户端
    server_metadata: HashMap<String, McpServerMetadata>,
    tool_plugin_provenance: Arc<ToolPluginProvenance>,  // 工具来源（哪个插件提供）
    host_owned_codex_apps_enabled: bool,
    prefix_mcp_tool_names: bool,
    elicitation_requests: ElicitationRequestManager,   // 引导请求的 responder 表
    startup_cancellation_token: CancellationToken,
}
```

它对外暴露聚合 API：`list_all_tools`（`:416`）、`list_all_resources`（`:552`）、`call_tool`（`:685`）——把"分散在 N 个 server"的能力收拢成统一视图。

**并发启动**在 `McpConnectionManager::new`（`:213-365`）中完成：

1. 遍历所有 enabled 的 server，对每个先发 `McpStartupUpdateEvent { status: Starting }`（`:252-260`）。
2. 把启动任务塞进 `JoinSet`，并发拉起（不阻塞会话启动）。
3. 启动结束后 spawn 一个收尾任务（`:342-363`），把每个 server 的结果归类为 `ready` / `cancelled` / `failed`，汇总成一个 `McpStartupCompleteEvent` 发出去。

> 设计取舍：启动是**并发 + 异步事件驱动**的，会话不会卡在"等所有 MCP server 就绪"。代价是工具列表可能"迟到"——见 §3.3 缓存机制和 §9 常见问题。

构造函数还接收两个可选项：`auth: Option<&CodexAuth>`（用于 codex_apps 的后端鉴权，`:228`、`:242-244`）和 `elicitation_reviewer: Option<ElicitationReviewerHandle>`（`:229`，用于自动复核引导请求）。

### 3.2 单个 server 的客户端：`AsyncManagedClient`

每个 MCP server 对应一个 `AsyncManagedClient`（`codex-mcp/src/rmcp_client.rs:127-134`）：

```rust
pub(crate) struct AsyncManagedClient {
    client: Shared<BoxFuture<'static, Result<ManagedClient, StartupOutcomeError>>>, // 启动 future（可共享 await）
    cached_tool_info_snapshot: Option<Vec<ToolInfo>>,  // 启动时的工具快照缓存
    cached_server_info: Option<McpServerInfo>,
    startup_complete: Arc<AtomicBool>,                  // 启动是否完成
    tool_plugin_provenance: Arc<ToolPluginProvenance>,
    cancel_token: CancellationToken,
}
```

它把"RMCP 客户端的启动过程"包装成一个**可共享的 future**（`Shared<BoxFuture<...>>`）。启动 future 完成时把 `startup_complete` 置为 `true`（`:215`）。

**缓存读取策略**（`cached_tool_info_snapshot_while_initializing`，`:251-253`）：

```rust
fn cached_tool_info_snapshot_while_initializing(&self) -> Option<Vec<ToolInfo>> {
    if !self.startup_complete.load(Ordering::Acquire) {
        return self.cached_tool_info_snapshot.clone();  // 启动未完成 → 用缓存快照
    }
    // 启动完成 → 不再用快照，走真实 list
    ...
}
```

含义：**启动中**返回缓存快照（让早期请求拿到"上次的"工具列表，不至于空白）；**启动完成后**改用真实数据。这是个微妙的平衡（见 §9）。

工具列表的真实来源是 `list_tools_for_client_uncached`（`:342-400`）：从 `RmcpClient` 拉取后，对 codex_apps 连接器元数据做清理（`sanitize_tool_connector_metadata`，`:357`），并规范化工具名（`normalize_codex_apps_callable_name`，`:364`）和标题。对 codex_apps server 还会额外过滤掉不允许的工具（`filter_disallowed_codex_apps_tools`，`:401-402`）。

### 3.3 工具暴露策略：直接 vs 延迟

模型并不是无条件看到所有 MCP 工具。`build_mcp_tool_exposure`（`core/src/mcp_tool_exposure.rs:18-50`）决定**直接暴露**还是**延迟暴露**：

```rust
pub(crate) const DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD: usize = 100;

let should_defer = search_tool_enabled
    && (config.features.enabled(Feature::ToolSearchAlwaysDeferMcpTools)
        || deferred_tools.len() >= DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD);
```

- 工具数 **< 100** 且未强制延迟 → 全部放进 `direct_tools`，直接出现在模型的工具列表里。
- 工具数 **≥ 100**，或启用了 `ToolSearchAlwaysDeferMcpTools` 特性 → 转入 `deferred_tools`，`direct_tools` 清空。延迟的工具不会直接出现在工具列表，模型必须先用"搜索工具"或显式选择才能调用。

返回结构是二元的（`McpToolExposure { direct_tools, deferred_tools: Option<...> }`，`:13-16`）。

> 为什么要延迟：Responses API 的工具列表有大小上限，一个大型 MCP 生态（几百个工具）会把工具描述塞爆 token 预算 / 撞 API 限制。延迟模式用"搜索 + 按需加载"换取列表瘦身。代价：超过 100 个工具时部分工具会"隐身"，模型不知道它们存在（见 §9）。

注意 codex_apps 工具的过滤更严（`filter_codex_apps_mcp_tools`，`:62-88`）：除了"模型可见"，还要求 `connector_id` 在允许列表里、且 `codex_app_tool_is_enabled`。

### 3.4 工具调用主入口：`handle_mcp_tool_call`

模型发起一个 MCP 工具调用后，最终落到 `handle_mcp_tool_call`（`core/src/mcp_tool_call.rs:107-291`）。它负责完整的"审批 → 执行 → 发事件"流程，返回 `HandledMcpToolCall { result: CallToolResult, tool_input }`（`:293-296`）。

主线逻辑：

1. **解析参数**（`:116-131`）：空字符串 OK，非法 JSON 直接返回错误结果。
2. **查元数据**（`:139-140`）：`lookup_mcp_tool_metadata` 聚合工具注释/连接器信息（见 §7）。
3. **确定审批模式**（`:166-171`）：codex_apps 用 `app_tool_policy.approval`，自定义 MCP 用 `custom_mcp_tool_approval_mode`。codex_apps 工具若被配置禁用，直接跳过（`:173-195`）。
4. **发 started 事件**（`:203-210`）：`notify_mcp_tool_call_started`。
5. **请求审批**（`:212-280`）：`maybe_request_mcp_tool_approval` 返回 `Option<McpToolApprovalDecision>`。
   - 返回 `None` → 无需审批，直接执行（`:282-290`）。
   - 返回 `Some(决策)` → 按决策映射（见下）。

**审批决策 → 行为映射**（`:212-280`）：

| 决策 | 行为 |
|------|------|
| `Accept` / `AcceptForSession` / `AcceptAndRemember` | 执行（`handle_approved_mcp_tool_call`，`:227-235`） |
| `Decline { message }` | 跳过并通知，message 缺省为 "user rejected MCP tool call"（`:237-249`） |
| `Cancel` | 跳过并通知 "user cancelled MCP tool call"（`:250-262`） |

执行后（含跳过/拒绝路径）会上报指标：`status = "ok" | "error"`（`:265-273`）。

实际执行在 `handle_approved_mcp_tool_call`（`:304-`）里：先按需重写参数（OpenAI 文件上传，`:327-333`）、构造 request meta、调 `execute_mcp_tool_call`，全程包在 tracing span 里（`:357-369`），最后发 completed 事件（`:374-383`）。

### 3.5 引导（Elicitation）：MCP server 反向问用户

MCP server 可以在工具调用中途**反向请求用户输入**（elicitation）——比如"请确认/请填表/请去这个 URL 授权"。Codex 用**两层**承接：

- **Session 层**：`Session::request_mcp_server_elicitation`（`core/src/session/mcp.rs:85-198`）把进行中的请求存进 `turn_state` 的 `pending_elicitations`（`:152-157`），发 `ElicitationRequestEvent`（`:174-179`），然后 `await rx_response`（`:195`）等客户端回应。
  - 若管理器处于 `elicitations_auto_deny` 状态（`:91-106`），直接返回一个自动应答而不真正发问。
  - `request_id` 的类型（`String` vs `Number`）会被显式映射到协议层的 `RequestId`（`:166-173`）——类型不匹配会导致响应送不回去（见 §9）。
- **连接管理层**：`ElicitationRequestManager`（`codex-mcp/src/elicitation.rs:52-58`）用一个 `ResponderMap = HashMap<(String, RequestId), oneshot::Sender<...>>`（`:244`）保存每个引导请求的应答通道，键是 `(server_name, request_id)`。响应靠 `resolve`（`:88`）按键派发。

---

## 4. 服务端方向：Codex 工具暴露与配置

### 4.1 把 Codex 当成一个工具

`mcp-server` crate 让 Codex 反过来当 MCP server：上层 host 跑 `codex mcp`，通过 stdin/stdout 用 JSON-RPC 跟它说话，可以发一个名为 `codex` 的工具调用，让 Codex 在内部跑完一整轮 agent loop。

工具的输入参数 schema 由 `CodexToolCallParam`（`mcp-server/src/codex_tool_config.rs:21-63`）定义：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CodexToolCallParam {
    pub prompt: String,                                  // 初始 prompt（必填）
    pub model: Option<String>,                           // 模型覆盖
    pub cwd: Option<String>,                             // 工作目录
    pub approval_policy: Option<CodexToolCallApprovalPolicy>,  // 审批策略
    pub sandbox: Option<CodexToolCallSandboxMode>,       // 沙箱模式
    pub config: Option<HashMap<String, serde_json::Value>>,   // config.toml 覆盖
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub compact_prompt: Option<String>,
}
```

注意 `approval_policy` 和 `sandbox` 这里用的是**专门镜像的枚举**（`CodexToolCallApprovalPolicy`，`:67-85`；`CodexToolCallSandboxMode`，`:89-105`），原因是它们要派生 `JsonSchema`（生成 MCP 工具的 JSON Schema），不能直接复用 core 里的 `AskForApproval` / `SandboxMode`，所以各写了一个 `From` 转换。`CodexToolCallSandboxMode` 只暴露三种：`ReadOnly` / `WorkspaceWrite` / `DangerFullAccess`（沙箱语义见 [doc13 §2](../3_执行与安全/13_sandbox_mechanism.md)）。

### 4.2 JSON-RPC 消息循环

`mcp-server` 的主循环在 `run_main`（`mcp-server/src/lib.rs:59-203`），是经典的三任务 + channel 架构：

```
            stdin                                          stdout
              │                                              ▲
              ▼                                              │
 ┌────────────────────┐   incoming   ┌─────────────────┐  outgoing  ┌──────────────────┐
 │ stdin reader task   │ ───────────► │ MessageProcessor │ ─────────► │ stdout writer task │
 │ (反序列化每一行)     │   channel    │ process_request  │  channel   │ (序列化 + 写行)     │
 └────────────────────┘              │ /response/...     │            └──────────────────┘
                                     └─────────────────┘
```

- **stdin reader**（`:126-146`）：逐行读 stdin，`serde_json` 反序列化成 `IncomingMessage`，丢进 `incoming_tx`。
- **processor**（`:149-172`）：从 `incoming_rx` 取消息，按类型分派给 `MessageProcessor`：`Request` / `Response` / `Notification` / `Error`（`:162-167`）。
- **stdout writer**（`:175-195`）：从 `outgoing_rx` 取消息，序列化成一行 JSON 写 stdout。

所有消息都是 MCP 协议消息（requests / responses / notifications）。退出路径很自然：stdin EOF → drop `incoming_tx` → processor 退出 → stdout 任务退出，最后 `tokio::join!` 三者（`:200`）。

---

## 5. 工具审批与授权机制

> 这是 MCP 集成里最复杂、最容易出 bug 的部分，单独成节。

### 5.1 审批决策树：`maybe_request_mcp_tool_approval`

核心函数 `maybe_request_mcp_tool_approval`（`core/src/mcp_tool_call.rs:1156-1329`）返回 `Option<McpToolApprovalDecision>`，`None` 表示"无需问，直接放行"。它是一棵多层决策树，顺序如下：

```
maybe_request_mcp_tool_approval
  │
  ├─① 自动放行？mcp_permission_prompt_is_auto_approved → 是则 return None  (:1165-1174)
  │
  ├─② 不需要审批 且 模式 != Prompt → return None                          (:1176-1180)
  │
  ├─③ 会话级已记住？session key 命中 → return Accept                       (:1185-1189)
  │
  ├─④ permission_request_hooks 钩子                                        (:1191-1214)
  │      Allow → Accept ;  Deny{msg} → Decline ;  None → 继续
  │
  ├─⑤ 路由到 Guardian？routes_approval_to_guardian                         (:1221-1241)
  │      review_approval_request → Guardian 决策 → apply_*持久化 → return
  │
  ├─⑥ ToolCallMcpElicitation 特性开 → 走 MCP elicitation 提示             (:1270-1308)
  │
  └─⑦ 降级到 RequestUserInput（兼容旧路径）                                (:1310-1328)
         request_user_input → 解析 → normalize → apply_*持久化 → return
```

**关键点：决策必须经过 `apply_mcp_tool_approval_decision` 才会被记住。** 注意 ⑤⑥⑦ 三条返回前都调了 `apply_mcp_tool_approval_decision`（`:1232-1239`、`:1299-1306`、`:1320-1327`）。这个调用是"记住"的唯一落点——漏掉它，"本会话内允许"就不会生效（见 §9）。

### 5.2 审批选项：能否"记住"

`McpToolApprovalPromptOptions`（`:1104-1108`）控制提示里出现哪些选项：

```rust
struct McpToolApprovalPromptOptions {
    allow_session_remember: bool,    // 显示"本会话内允许"
    allow_persistent_approval: bool, // 显示"以后都允许"（持久化）
}
```

由 `mcp_tool_approval_prompt_options`（`:1144-1154`）计算：

- `allow_session_remember` = `session_approval_key.is_some()`——只有 `AppToolApproval::Auto` 模式才有 session key（见 `session_mcp_tool_approval_key`，`:1331-1350`）。
- `allow_persistent_approval` = `tool_call_mcp_elicitation_enabled && persistent_approval_key.is_some()`——需要启用 `ToolCallMcpElicitation` 特性。

### 5.3 审批缓存键：`McpToolApprovalKey`

记住决策用 `McpToolApprovalKey`（`:1137-1142`）：

```rust
struct McpToolApprovalKey {
    server: String,
    connector_id: Option<String>,
    tool_name: String,
}
```

`session_mcp_tool_approval_key`（`:1331-1350`）的两个早退很关键：

```rust
if approval_mode != AppToolApproval::Auto {
    return None;   // 只有 Auto 模式能记住
}
let connector_id = metadata.and_then(|m| m.connector_id.clone());
if invocation.server == CODEX_APPS_MCP_SERVER_NAME && connector_id.is_none() {
    return None;   // codex_apps 但缺 connector_id → 无法记住
}
```

即 **codex_apps 工具若 `connector_id` 为 `None`，无法被记住**（见 §9）。

### 5.4 三个去向：会话 / 持久 / Guardian

`apply_mcp_tool_approval_decision`（`:1873-1897`）把决策落地：

- `AcceptForSession` → `remember_mcp_tool_approval`（`:1868-1871`）：写进会话内的 `sess.services.tool_approvals` 表（`ReviewDecision::ApprovedForSession`），仅本会话有效。
- `AcceptAndRemember` → `maybe_persist_mcp_tool_approval`（`:1899-1929`）：尝试持久化到 config（codex_apps 走 `persist_codex_app_tool_approval`，普通 MCP 走 `persist_non_app_mcp_tool_approval`），成功后 `reload_user_config_layer`；若缺 connector_id 或持久化失败，降级为会话级记住。
- `Accept` / `Decline` / `Cancel` → 不记。

会话级命中检查在 `mcp_tool_approval_is_remembered`（`:1863-1866`）：从 `tool_approvals` 取，命中 `ApprovedForSession` 才算。

**Guardian 路径**（`:1221-1241`）：当 `routes_approval_to_guardian` 为真，把审批委托给 Guardian 模型（见 [doc14 多 agent](./14_multi_agent_system.md)），`mcp_tool_approval_decision_from_guardian`（`:1385-1403`）把 Guardian 的 `ReviewDecision` 翻译成 `McpToolApprovalDecision`（`Approved → Accept`、`ApprovedForSession → AcceptForSession`、`Denied/TimedOut → Decline`、`Abort → Decline{None}`）。

### 5.5 后验提示模板

为让审批提示更友好，Codex 内嵌了一套 JSON 模板库（`consequential_tool_message_templates.json`）。`render_mcp_tool_approval_template`（`core/src/mcp_tool_approval_templates.rs:53-69`）从中加载并渲染：

- 模板按 `(server_name, connector_id, tool_title)` 三元组精确匹配（`:104-108`），任一缺失则不渲染、回退到通用展示。
- 支持 `{connector_name}` 变量替换（`mcp_tool_approval_templates.rs:11` 定义 `CONNECTOR_NAME_TEMPLATE_VAR`，例：`"Allow {connector_name} to create an event?"`，`:205`）。
- 参数标签化由 `render_tool_params`（`:110-116`）完成。

模板文件有 schema 版本校验（`:82-89`），版本对不上直接放弃模板（不致命）。

---

## 6. 配置与依赖管理

### 6.1 配置中心：`McpConfig` 与 `McpManager`

`McpConfig`（`codex-mcp/src/mcp/mod.rs:108-149`）是 `codex-mcp` crate 需要的"长生命周期配置"切片，从 root config 提炼而来。文件头注释（`:98-106`）特别强调：**只放长生命周期配置，请求级/鉴权级状态不要塞进来**（避免 auth 变化时配置变陈旧），这类状态要显式 thread 进 `effective_mcp_servers` 等入口。

core 侧用 `McpManager`（`core/src/mcp.rs:14-42`）作为门面，持有 `PluginsManager`，暴露三个核心方法：

```rust
pub struct McpManager { plugins_manager: Arc<PluginsManager> }

impl McpManager {
    // 已配置的 MCP server（不含运行时注入）
    pub async fn configured_servers(&self, config) -> HashMap<String, McpServerConfig>;
    // 生效的 server（含运行时注入的 codex_apps）
    pub async fn effective_servers(&self, config, auth) -> HashMap<String, EffectiveMcpServer>;
    // 工具来源元数据
    pub async fn tool_plugin_provenance(&self, config) -> ToolPluginProvenance;
}
```

它们分别委托给 `codex-mcp` 的 `configured_mcp_servers`（`mcp/mod.rs:238-240`）、`effective_mcp_servers`（`:242-247`）、`tool_plugin_provenance`（`:261-263`）。

### 6.2 codex_apps：运行时注入的 host 端 MCP

`effective_mcp_servers` 比 `configured_mcp_servers` 多了一个**运行时注入**的特殊 server：`codex_apps`（`CODEX_APPS_MCP_SERVER_NAME = "codex_apps"`，`mcp/mod.rs:45`）。注入逻辑在 `with_codex_apps_mcp`（`:218-232`）：

```rust
pub fn host_owned_codex_apps_enabled(config: &McpConfig, auth: Option<&CodexAuth>) -> bool {
    config.apps_enabled && auth.is_some_and(CodexAuth::uses_codex_backend)
}
```

即同时满足 `config.apps_enabled` 且 auth 走 Codex 后端时，才注入 `codex_apps`；否则移除（`:228-230`）。

它的端点 URL 由 `codex_apps_mcp_url`（`:386-446`）根据 base_url（ChatGPT / API Codex）和 `path_override` 拼出来：

- `chatgpt.com` / `chat.openai.com` → 补 `/backend-api`，默认路径 `wham/apps`（`:435-436`）。
- 含 `/api/codex` → 默认路径 `apps`（`:437-438`）。
- 否则 → 拼 `/api/codex`，默认 `apps`（`:440`）。

**鉴权用环境变量 bearer token**，不走 OAuth：`codex_apps_mcp_bearer_token_env_var`（`:413-420`）从 `CODEX_CONNECTORS_TOKEN`（`:48`）读取（见 §9 关于换 token 的注意）。

### 6.3 插件归属：`ToolPluginProvenance`

`ToolPluginProvenance`（`mcp/mod.rs:151-216`）记录"哪个工具/连接器是哪个插件提供的"，按两个维度索引：

```rust
pub struct ToolPluginProvenance {
    plugin_display_names_by_connector_id: HashMap<String, Vec<String>>,
    plugin_display_names_by_mcp_server_name: HashMap<String, Vec<String>>,
    plugin_ids_by_mcp_server_name: HashMap<String, String>,
}
```

由 `from_config`（`:179-215`）从 `config.plugin_capability_summaries` 构建：遍历每个插件的 `app_connector_ids` 和 `mcp_server_names`，把插件 `display_name` 挂到对应索引（最后排序去重）。

> 注意：它是**静态快照**，config 加载后不再更新（见 §9）。新装插件需走会话级刷新。

### 6.4 技能依赖自动安装

技能（skill）可以声明它依赖某个 MCP server。`maybe_prompt_and_install_mcp_dependencies`（`core/src/mcp_skill_dependencies.rs:34-83`）在技能被提及时检测并提示安装缺失依赖：

1. 仅支持 first-party 客户端（`:42-45`）。
2. 必须有 mentioned skills 且启用 `SkillMcpDependencyInstall` 特性（`:48-54`）。
3. 对比 `configured_servers` 找出缺失依赖（`collect_missing_mcp_dependencies`，`:61`）。
4. 过滤掉已提示过的（`filter_prompted_mcp_dependencies`，`:66`）。
5. 询问用户后安装（`should_install_mcp_dependencies` → `maybe_install_mcp_dependencies`，`:71-82`）。

支持两种传输：`streamable_http`（默认）和 `stdio`（`:320-343`，按 transport 字符串分派，缺省 `"streamable_http"`，`:330`）。安装后需要热刷新连接管理器才能发现新 server（见 §6.5）。

### 6.5 热刷新：`refresh_mcp_servers_inner`

新装了 MCP 依赖、或完成 OAuth 后，需要在不重启会话的前提下重建连接池。`Session::refresh_mcp_servers_inner`（`core/src/session/mcp.rs:305-380`）实现热刷新：

1. 重新算 `effective_mcp_servers` + auth statuses（`:322-327`）。
2. 取消旧的 startup cancellation token（`:339-343`）。
3. `McpConnectionManager::new` 建新管理器（`:344-362`）。
4. **转移 `elicitations_auto_deny` 状态**到新管理器（`:363-366`）——保证刷新不丢失"自动拒绝引导"的开关。
5. `std::mem::replace` 换上新管理器（`:375-378`），再 `shutdown` 旧的（`:379`）。

这条 Op 在协议层对应 `Op::RefreshMcpServers { config: McpServerRefreshConfig }`（`protocol/src/protocol.rs:657`，元数据标签 `"refresh_mcp_servers"`，`:794`）。Op 的整体派发见 [doc11 §10 协议 Op](../2_运行时核心/11_thread_session_turn_lifecycle.md)。

---

## 7. 关键数据结构与流程

### 7.1 `ToolInfo`：统一工具视图

`ToolInfo`（`codex-mcp/src/tools.rs:31-`）是连接池对外暴露的工具描述，把"原始 MCP 工具定义"和"Codex 侧的路由/可见性元数据"合在一起：

```rust
pub struct ToolInfo {
    pub server_name: String,            // 路由用的原始 server 名
    pub supports_parallel_tool_calls: bool,
    pub server_origin: Option<String>,
    pub callable_name: String,          // 模型可见的工具名（sanitize 过）
    pub callable_namespace: String,     // 延迟加载用的命名空间
    pub namespace_description: Option<String>,
    pub tool: Tool,                     // 原始 MCP 工具定义；tool.name 发回 server
    pub connector_id: Option<String>,
    pub connector_name: Option<String>,
    pub plugin_display_names: Vec<String>,
}
```

注意 `callable_name` 用 serde `rename = "tool_name", alias = "callable_name"`，`callable_namespace` 同理 `rename = "tool_namespace"`——为了兼容缓存里的旧字段名。

### 7.2 `lookup_mcp_tool_metadata`：审批/分析元数据聚合

`lookup_mcp_tool_metadata`（`core/src/mcp_tool_call.rs:1409-1469`）从 `mcp_connection_manager` 聚合一个工具的全部元数据，喂给审批提示和埋点：

```rust
Some(McpToolApprovalMetadata {
    annotations,                  // 工具注释（destructive/read_only/open_world hint）
    connector_id, connector_name, connector_description,
    plugin_id,                    // 来源插件
    tool_title, tool_description,
    mcp_app_resource_uri,         // app UI 资源
    codex_apps_meta,              // codex_apps 特有元数据
    openai_file_input_params,     // OpenAI 文件参数（仅 codex_apps）
})
```

它先取连接管理器读锁、列出全部工具、按 `(server, tool_name)` 找到目标（`:1419-1422`）。codex_apps 工具会额外查连接器描述（`:1423-1445`）。

**OpenAI 文件参数只放给 codex_apps**：`openai_file_input_params_for_server`（`:1471-1478`）：

```rust
(server == CODEX_APPS_MCP_SERVER_NAME)
    .then_some(declared_openai_file_input_param_names(meta))
    .filter(|params| !params.is_empty())
```

注释明说"Disallow custom MCPs from uploading files via fileParams"（`:1463`）——自定义 MCP 被禁止用 `fileParams` 上传文件（见 §9）。

### 7.3 一次完整 MCP 工具调用的时序

```
模型 tool call
   │
   ▼
handle_mcp_tool_call (mcp_tool_call.rs:107)
   │  解析参数
   │  lookup_mcp_tool_metadata ──► 注释/连接器/文件参数
   │  确定 approval_mode
   │  notify_mcp_tool_call_started ──► McpToolCallBeginEvent
   ▼
maybe_request_mcp_tool_approval (:1156)
   │  ① 自动放行? ② 不需审批? ③ 会话已记住?
   │  ④ permission hooks  ⑤ Guardian  ⑥/⑦ elicitation/RequestUserInput
   │  apply_mcp_tool_approval_decision ──► 记住(会话/持久)
   ▼
[Accept] handle_approved_mcp_tool_call (:304)
   │  rewrite_mcp_tool_arguments_for_openai_files (文件上传重写)
   │  execute_mcp_tool_call ──► Session::call_tool ──► McpConnectionManager::call_tool
   │       └─► 路由到对应 server，可能触发 elicitation（反向问用户）
   │  notify_mcp_tool_call_completed ──► McpToolCallEndEvent
   ▼
HandledMcpToolCall { result, tool_input } ──► 回喂模型
```

> `McpToolCallBegin/End`、`McpStartupUpdate/Complete` 等事件定义在 `protocol/src/protocol.rs`（`:1294-1301`），`McpInvocation` 在 `:2273`。事件如何被 TUI 渲染见 [doc06](../5_前端_集成_协议/06_tui_design.md)。

---

## 8. 与核心工具系统的关系

MCP 工具不是独立于 Codex 工具系统的旁支，而是被统一编排：

- **工具注册/路由**：MCP 工具经 `build_mcp_tool_exposure` 转成 `direct_tools` / `deferred_tools` 后，与内建工具（shell、apply_patch 等）一起注入模型的工具列表。内建工具的注册/路由/编排见 [doc09 阅读指南](../0_导览/09_codex_core_reading_guide.md) 对 `tools/` 目录的拆解（`tools/registry.rs`、`tools/router.rs`、`tools/orchestrator.rs`）。
- **审批复用**：MCP 的审批最终汇入与 shell/apply_patch 同一套 `ReviewDecision` / Guardian 复核体系（`review_approval_request`、`AskForApproval`、`tool_approvals` 表）。即"模型想跑 shell 命令"和"模型想调 GitHub MCP 工具"走的是同源的审批基础设施。
- **沙箱衔接**：MCP 工具执行也可以带沙箱状态（`SandboxState`、`MCP_SANDBOX_STATE_META_CAPABILITY`，见 `codex-mcp/src/lib.rs:6-8`、`runtime.rs`）。沙箱本身的三种权限模式与跨平台实现见 [doc13](../3_执行与安全/13_sandbox_mechanism.md)。
- **生命周期**：MCP server 的启动/刷新嵌在 Session/Turn 生命周期中（启动事件、`Op::RefreshMcpServers`、引导请求存进 turn_state），整体生命周期见 [doc11](../2_运行时核心/11_thread_session_turn_lifecycle.md)。
- **协议层**：MCP 相关的 `Op` 与 `EventMsg` 是 Codex 统一协议的一部分，协议层全景见 [doc15](../5_前端_集成_协议/15_api_protocol_layer.md)。

一句话：**MCP 是"外部能力"接入 Codex 工具系统的标准管道，复用了同一套注册、审批、沙箱、事件基建，只是多了一个连接池和协议适配层。**

---

## 9. 常见问题

**Q1：我点了"本会话内允许"，为什么下次调用还问我？**
A：会话级记住的唯一落点是 `apply_mcp_tool_approval_decision`（`mcp_tool_call.rs:1320-1327` 等）。如果某条审批路径返回决策前没调它，记忆就不生效。另外 `mcp_tool_approval_is_remembered` 只认 `ApprovedForSession`（`:1863-1866`），其它决策不缓存。

**Q2：codex_apps 的某个工具点"以后都允许"没用？**
A：审批缓存键 `McpToolApprovalKey` 需要 `connector_id`。`session_mcp_tool_approval_key`（`:1341-1343`）对 "codex_apps server 且 `connector_id` 为 `None`" 直接返回 `None`，于是无法记住。

**Q3：MCP 工具列表为什么"迟到"或时有时无？**
A：连接是**并发异步**启动的。`AsyncManagedClient` 在 `startup_complete` 为 false 时返回缓存快照（`rmcp_client.rs:251-253`），完成后才用真实数据。如果某 server 启动慢（默认超时 `DEFAULT_STARTUP_TIMEOUT = 30s`，`rmcp_client.rs:76`），其工具会在启动完成后才出现——快照里可能还是上次的、甚至是空的。

**Q4：超过 100 个 MCP 工具后，模型"看不到"部分工具？**
A：`DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD = 100`（`mcp_tool_exposure.rs:11`）。工具数 ≥ 100（或启用 `ToolSearchAlwaysDeferMcpTools`）时全部转为延迟暴露，模型必须先用搜索工具或显式选择才能调用。大型 MCP 生态下确实会"隐藏"工具——这是用列表瘦身换 token 预算的有意取舍。

**Q5：为什么我的自定义 MCP server 不能上传文件？**
A：`openai_file_input_params_for_server`（`mcp_tool_call.rs:1471-1478`）只对 `server == CODEX_APPS_MCP_SERVER_NAME` 返回文件参数。`lookup_mcp_tool_metadata` 处的注释明说禁止自定义 MCP 用 `fileParams` 上传（`:1463`）。文件上传是 codex_apps 专属能力。

**Q6：MCP server 启动失败会让会话起不来吗？**
A：默认不会——失败的 server 只是进入 `McpStartupCompleteEvent` 的 `failed` 列表（`connection_manager.rs:349-354`），会话照常启动，只是工具列表不完整。例外是标了 `required: true`（`config/src/mcp_types.rs:143`）的 server，会被 `required_startup_failures`（`connection_manager.rs:389-399`）单独检查并上报为致命失败。

**Q7：新装的插件 / 改了 codex_apps 的 token，怎么让 Codex 重新发现？**
A：`ToolPluginProvenance` 和 `plugin_capability_summaries` 是静态快照，config 加载后不更新。codex_apps 的 bearer token 从 `CODEX_CONNECTORS_TOKEN` 环境变量读取（`mcp/mod.rs:413-420`），不走 OAuth。换 token 需要重设环境变量并触发热刷新 `refresh_mcp_servers_inner`（`session/mcp.rs:305-380`）重建连接池。

**Q8：引导（elicitation）响应送不回去？**
A：引导是两层架构——Session 侧把 `pending_elicitations` 存进 turn_state（`session/mcp.rs:152-157`），连接侧 `ElicitationRequestManager` 用 `ResponderMap`（键 `(server, request_id)`，`elicitation.rs:244`）保存应答通道。响应必须匹配 `request_id` 的类型（`String` vs `Number`，`session/mcp.rs:166-173`）——类型对不上就路由不到对应通道。

---

> 校对说明：本文所有 `文件:行号` 均基于 `feature-learn-v2` 分支当前源码用 grep/Read 核对。`refresh_mcp_servers_inner` 函数实际跨 `session/mcp.rs:305-380`（大纲标注为 305-362，此处以真实范围为准）。`ElicitationRequestManager` 在连接侧保存的是 responder/应答通道映射（`ResponderMap`），大纲中"重试状态"的表述据真实结构修正为"应答派发表"。
