//! 【文件职责】MCP server 的 JSON-RPC 消息分派核心：把客户端发来的每条
//! request / response / notification / error 路由到对应处理逻辑，
//! 并把 `codex` / `codex-reply` 工具调用接驳到真正的 Codex 会话执行。
//!
//! 【架构位置】
//!   层级：MCP server 消息处理层
//!   上游：lib.rs 的处理任务（按消息种类调 process_request/response/notification/error）
//!   下游：codex_tool_runner（在独立 task 中跑会话）、codex-core 的 ThreadManager、
//!         outgoing_message（回送响应/错误）
//!
//! 【数据流】
//!   IncomingMessage → process_request 大 match → 具体 handle_*：
//!   读类请求（list/read/...）多数只记日志；真正干活的是 handle_call_tool，
//!   它把工具会话 spawn 到后台 task，避免阻塞这条单线程的消息处理循环。
//!
//! 【阅读建议】先看 `process_request` 的大 match（一眼掌握支持哪些方法），
//!   再深入 `handle_call_tool` → `handle_tool_call_codex` /
//!   `handle_tool_call_codex_session_reply`（工具调用主线），
//!   最后看 `handle_cancelled_notification`（中断已运行会话的逻辑）。

use std::collections::HashMap;
use std::sync::Arc;

use codex_arg0::Arg0DispatchPaths;
use codex_core::StateDbHandle;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::empty_extension_registry;
use codex_login::AuthManager;
use codex_login::default_client::USER_AGENT_SUFFIX;
use codex_login::default_client::get_codex_user_agent;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::Submission;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::ErrorCode;
use rmcp::model::ErrorData;
use rmcp::model::Implementation;
use rmcp::model::InitializeResult;
use rmcp::model::JsonRpcError;
use rmcp::model::JsonRpcNotification;
use rmcp::model::JsonRpcRequest;
use rmcp::model::JsonRpcResponse;
use rmcp::model::RequestId;
use rmcp::model::ServerCapabilities;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::task;

use crate::codex_tool_config::CodexToolCallParam;
use crate::codex_tool_config::CodexToolCallReplyParam;
use crate::codex_tool_config::create_tool_for_codex_tool_call_param;
use crate::codex_tool_config::create_tool_for_codex_tool_call_reply_param;
use crate::outgoing_message::OutgoingMessageSender;

/// 单条 MCP 连接的消息处理器，持有出站发送端、线程管理器等共享状态，
/// 由 lib.rs 在处理任务里独占持有（`&mut self`，串行处理消息）。
pub(crate) struct MessageProcessor {
    // 出站消息收口（回响应/错误/通知）。用 Arc 是为了能克隆给后台会话 task。
    outgoing: Arc<OutgoingMessageSender>,
    // MCP 握手标志：是否已完成 initialize。用于拒绝重复 initialize。
    initialized: bool,
    // 当前可执行文件/沙箱程序路径，构建工具会话 Config 时透传（arg0 重 exec 机制）。
    arg0_paths: Arg0DispatchPaths,
    // Codex 线程（会话）管理器：负责开新会话、按 ThreadId 取回已有会话。
    thread_manager: Arc<ThreadManager>,
    // [引用范围] 在途请求 ID → 其对应的 Codex 线程 ID 的映射。
    // 写入：会话 task 启动时登记；读取/删除：收到 cancel 通知或会话结束时。
    // 用 Arc<Mutex<>> 是因为后台会话 task 和消息处理循环会并发读写它。
    running_requests_id_to_codex_uuid: Arc<Mutex<HashMap<RequestId, ThreadId>>>,
}

impl MessageProcessor {
    /// Create a new `MessageProcessor`, retaining a handle to the outgoing
    /// `Sender` so handlers can enqueue messages to be written to stdout.
    pub(crate) async fn new(
        outgoing: OutgoingMessageSender,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        environment_manager: Arc<EnvironmentManager>,
        state_db: Option<StateDbHandle>,
        installation_id: String,
    ) -> Self {
        let outgoing = Arc::new(outgoing);
        let auth_manager = AuthManager::shared_from_config(
            config.as_ref(),
            /*enable_codex_api_key_env*/ false,
        )
        .await;
        let thread_manager = Arc::new(ThreadManager::new(
            config.as_ref(),
            auth_manager,
            // 标记会话来源为 MCP：core 据此区分调用渠道（影响遥测、行为开关等）。
            SessionSource::Mcp,
            environment_manager,
            empty_extension_registry(),
            /*analytics_events_client*/ None,
            codex_core::thread_store_from_config(config.as_ref(), state_db.clone()),
            state_db.clone(),
            installation_id,
            /*attestation_provider*/ None,
        ));
        Self {
            outgoing,
            initialized: false,
            arg0_paths,
            thread_manager,
            running_requests_id_to_codex_uuid: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 分派客户端请求：按 MCP 方法类型路由到对应 handle_*。
    /// 多数只读类请求（resources/prompts 等）当前仅记日志、不真正实现；
    /// 实质功能集中在 initialize、tools/list、tools/call 三条。
    pub(crate) async fn process_request(&mut self, request: JsonRpcRequest<ClientRequest>) {
        let request_id = request.id.clone();
        let client_request = request.request;

        match client_request {
            ClientRequest::InitializeRequest(params) => {
                self.handle_initialize(request_id, params.params).await;
            }
            ClientRequest::PingRequest(_params) => {
                self.handle_ping(request_id).await;
            }
            ClientRequest::ListResourcesRequest(params) => {
                self.handle_list_resources(params.params);
            }
            ClientRequest::ListResourceTemplatesRequest(params) => {
                self.handle_list_resource_templates(params.params);
            }
            ClientRequest::ReadResourceRequest(params) => {
                self.handle_read_resource(params.params);
            }
            ClientRequest::SubscribeRequest(params) => {
                self.handle_subscribe(params.params);
            }
            ClientRequest::UnsubscribeRequest(params) => {
                self.handle_unsubscribe(params.params);
            }
            ClientRequest::ListPromptsRequest(params) => {
                self.handle_list_prompts(params.params);
            }
            ClientRequest::GetPromptRequest(params) => {
                self.handle_get_prompt(params.params);
            }
            ClientRequest::ListToolsRequest(params) => {
                self.handle_list_tools(request_id, params.params).await;
            }
            ClientRequest::CallToolRequest(params) => {
                self.handle_call_tool(request_id, params.params).await;
            }
            ClientRequest::SetLevelRequest(params) => {
                self.handle_set_level(params.params);
            }
            ClientRequest::CompleteRequest(params) => {
                self.handle_complete(params.params);
            }
            // tasks/* 是 MCP 的异步任务管理方法族；本 server 不支持，统一回
            // METHOD_NOT_FOUND（Codex 的会话生命周期不走 MCP task 模型）。
            ClientRequest::GetTaskInfoRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/get_info")
                    .await;
            }
            ClientRequest::ListTasksRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/list")
                    .await;
            }
            ClientRequest::GetTaskResultRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/get_result")
                    .await;
            }
            ClientRequest::CancelTaskRequest(_) => {
                self.handle_unsupported_request(request_id, "tasks/cancel")
                    .await;
            }
            ClientRequest::CustomRequest(custom) => {
                let method = custom.method.clone();
                self.outgoing
                    .send_error(
                        request_id,
                        ErrorData::new(
                            ErrorCode::METHOD_NOT_FOUND,
                            format!("method not found: {method}"),
                            Some(json!({ "method": method })),
                        ),
                    )
                    .await;
            }
        }
    }

    /// 处理客户端对「server 主动发起的请求」的应答：按 ID 唤醒等待中的回调。
    /// 典型场景：server 发出审批 elicitation 后，客户端把用户决定作为 response 回来。
    pub(crate) async fn process_response(&mut self, response: JsonRpcResponse<serde_json::Value>) {
        tracing::info!("<- response: {:?}", response);
        let JsonRpcResponse { id, result, .. } = response;
        self.outgoing.notify_client_response(id, result).await
    }

    pub(crate) async fn process_notification(
        &mut self,
        notification: JsonRpcNotification<ClientNotification>,
    ) {
        match notification.notification {
            ClientNotification::CancelledNotification(params) => {
                self.handle_cancelled_notification(params.params).await;
            }
            ClientNotification::ProgressNotification(params) => {
                self.handle_progress_notification(params.params);
            }
            ClientNotification::RootsListChangedNotification(_params) => {
                self.handle_roots_list_changed();
            }
            ClientNotification::InitializedNotification(_) => {
                self.handle_initialized_notification();
            }
            ClientNotification::CustomNotification(_) => {
                tracing::warn!("ignoring custom client notification");
            }
        }
    }

    pub(crate) fn process_error(&mut self, err: JsonRpcError) {
        tracing::error!("<- error: {:?}", err);
    }

    /// 处理 MCP 握手 `initialize`：记录客户端身份用于 user-agent、回送 server
    /// 能力与信息。重复 initialize 视为协议错误并返回 invalid_request。
    async fn handle_initialize(
        &mut self,
        id: RequestId,
        params: rmcp::model::InitializeRequestParams,
    ) {
        tracing::info!("initialize -> params: {:?}", params);

        if self.initialized {
            self.outgoing
                .send_error(
                    id,
                    ErrorData::invalid_request("initialize called more than once", None),
                )
                .await;
            return;
        }

        // 用客户端自报的名称+版本拼成 user-agent 后缀，写入全局，使后续 Codex
        // 发出的网络请求 UA 能体现"是哪个 MCP 客户端在调用"。锁失败则静默跳过
        // （后缀只是观测信息，丢了不影响功能）。
        let client_info = params.client_info;
        let name = client_info.name;
        let version = client_info.version;
        let user_agent_suffix = format!("{name}; {version}");
        if let Ok(mut suffix) = USER_AGENT_SUFFIX.lock() {
            *suffix = Some(user_agent_suffix);
        }

        let server_info =
            Implementation::new("codex-mcp-server", env!("CARGO_PKG_VERSION")).with_title("Codex");

        // Preserve Codex's existing non-spec `serverInfo.user_agent` field.
        let mut server_info_value = match serde_json::to_value(&server_info) {
            Ok(value) => value,
            Err(err) => {
                self.outgoing
                    .send_error(
                        id,
                        ErrorData::internal_error(
                            format!("failed to serialize server info: {err}"),
                            None,
                        ),
                    )
                    .await;
                return;
            }
        };
        if let serde_json::Value::Object(ref mut obj) = server_info_value {
            obj.insert("user_agent".to_string(), json!(get_codex_user_agent()));
        }

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        let result = InitializeResult::new(capabilities)
            .with_protocol_version(params.protocol_version.clone())
            .with_server_info(server_info);
        let mut result_value = match serde_json::to_value(result) {
            Ok(value) => value,
            Err(err) => {
                self.outgoing
                    .send_error(
                        id,
                        ErrorData::internal_error(
                            format!("failed to serialize initialize response: {err}"),
                            None,
                        ),
                    )
                    .await;
                return;
            }
        };

        // 用注入了非标准 `user_agent` 字段的 server_info_value 覆盖结果里的
        // serverInfo（rmcp 默认序列化不含该字段，这里手动补回以兼容 Codex 旧约定）。
        if let serde_json::Value::Object(ref mut obj) = result_value {
            obj.insert("serverInfo".to_string(), server_info_value);
        }

        self.initialized = true;
        self.outgoing.send_response(id, result_value).await;
    }

    async fn handle_ping(&self, id: RequestId) {
        tracing::info!("ping");
        self.outgoing.send_response(id, json!({})).await;
    }

    fn handle_list_resources(&self, params: Option<rmcp::model::PaginatedRequestParams>) {
        tracing::info!("resources/list -> params: {:?}", params);
    }

    fn handle_list_resource_templates(&self, params: Option<rmcp::model::PaginatedRequestParams>) {
        tracing::info!("resources/templates/list -> params: {:?}", params);
    }

    fn handle_read_resource(&self, params: rmcp::model::ReadResourceRequestParams) {
        tracing::info!("resources/read -> params: {:?}", params);
    }

    fn handle_subscribe(&self, params: rmcp::model::SubscribeRequestParams) {
        tracing::info!("resources/subscribe -> params: {:?}", params);
    }

    fn handle_unsubscribe(&self, params: rmcp::model::UnsubscribeRequestParams) {
        tracing::info!("resources/unsubscribe -> params: {:?}", params);
    }

    fn handle_list_prompts(&self, params: Option<rmcp::model::PaginatedRequestParams>) {
        tracing::info!("prompts/list -> params: {:?}", params);
    }

    fn handle_get_prompt(&self, params: rmcp::model::GetPromptRequestParams) {
        tracing::info!("prompts/get -> params: {:?}", params);
    }

    async fn handle_list_tools(
        &self,
        id: RequestId,
        params: Option<rmcp::model::PaginatedRequestParams>,
    ) {
        tracing::trace!("tools/list -> {params:?}");
        let result = rmcp::model::ListToolsResult {
            meta: None,
            tools: vec![
                create_tool_for_codex_tool_call_param(),
                create_tool_for_codex_tool_call_reply_param(),
            ],
            next_cursor: None,
        };

        self.outgoing.send_response(id, result).await;
    }

    /// 处理 `tools/call`：按工具名分派到 `codex`（开新会话）或 `codex-reply`
    /// （续聊已有会话）；未知工具名回送 error 结果。
    async fn handle_call_tool(&self, id: RequestId, params: CallToolRequestParams) {
        tracing::info!("tools/call -> params: {:?}", params);
        let CallToolRequestParams {
            name, arguments, ..
        } = params;

        match name.as_ref() {
            "codex" => self.handle_tool_call_codex(id, arguments).await,
            "codex-reply" => {
                self.handle_tool_call_codex_session_reply(id, arguments)
                    .await
            }
            _ => {
                let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                    "Unknown tool '{name}'"
                ))]);
                self.outgoing.send_response(id, result).await;
            }
        }
    }

    /// 处理 `codex` 工具调用：解析入参 → 构建 Config → 在后台 task 中开新会话。
    /// 参数解析或 Config 构建的任何失败都就地回送 error 结果并提前返回，
    /// 不会进入后台 task。
    async fn handle_tool_call_codex(
        &self,
        id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        // 三层校验：有无 arguments → 能否反序列化为 CodexToolCallParam → 能否转成 Config；
        // 每层失败各自回送一条带具体原因的 error 结果。
        let arguments = arguments.map(serde_json::Value::Object);
        let (initial_prompt, config): (String, Config) = match arguments {
            Some(json_val) => match serde_json::from_value::<CodexToolCallParam>(json_val) {
                Ok(tool_cfg) => match tool_cfg.into_config(self.arg0_paths.clone()).await {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        let result = CallToolResult::error(vec![rmcp::model::Content::text(
                            format!("Failed to load Codex configuration from overrides: {e}"),
                        )]);
                        self.outgoing.send_response(id, result).await;
                        return;
                    }
                },
                Err(e) => {
                    let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                        "Failed to parse configuration for Codex tool: {e}"
                    ))]);
                    self.outgoing.send_response(id, result).await;
                    return;
                }
            },
            None => {
                let result = CallToolResult::error(vec![rmcp::model::Content::text(
                    "Missing arguments for codex tool-call; the `prompt` field is required.",
                )]);
                self.outgoing.send_response(id, result).await;
                return;
            }
        };

        // Clone outgoing and server to move into async task.
        // 克隆共享句柄以便 move 进后台 task（Arc 克隆是廉价的引用计数 +1）。
        let outgoing = self.outgoing.clone();
        let thread_manager = self.thread_manager.clone();
        let running_requests_id_to_codex_uuid = self.running_requests_id_to_codex_uuid.clone();

        // Spawn an async task to handle the Codex session so that we do not
        // block the synchronous message-processing loop.
        // 把会话放到独立 task 跑：会话可能长时间运行（多轮 LLM 调用），绝不能阻塞
        // 这条单线程串行的消息处理循环，否则期间无法处理 cancel 等其他消息。
        task::spawn(async move {
            // Run the Codex session and stream events back to the client.
            crate::codex_tool_runner::run_codex_tool_session(
                id,
                initial_prompt,
                config,
                outgoing,
                thread_manager,
                running_requests_id_to_codex_uuid,
            )
            .await;
        });
    }

    /// 处理 `codex-reply` 工具调用：解析入参与 threadId → 从 ThreadManager 取回
    /// 对应会话 → 在后台 task 中投递续聊 prompt。会话不存在时回送 error 结果。
    async fn handle_tool_call_codex_session_reply(
        &self,
        request_id: RequestId,
        arguments: Option<rmcp::model::JsonObject>,
    ) {
        let arguments = arguments.map(serde_json::Value::Object);
        tracing::info!("tools/call -> params: {:?}", arguments);

        // parse arguments
        let codex_tool_call_reply_param: CodexToolCallReplyParam = match arguments {
            Some(json_val) => match serde_json::from_value::<CodexToolCallReplyParam>(json_val) {
                Ok(params) => params,
                Err(e) => {
                    tracing::error!("Failed to parse Codex tool call reply parameters: {e}");
                    let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                        "Failed to parse configuration for Codex tool: {e}"
                    ))]);
                    self.outgoing.send_response(request_id, result).await;
                    return;
                }
            },
            None => {
                tracing::error!(
                    "Missing arguments for codex-reply tool-call; the `thread_id` and `prompt` fields are required."
                );
                let result = CallToolResult::error(vec![rmcp::model::Content::text(
                    "Missing arguments for codex-reply tool-call; the `thread_id` and `prompt` fields are required.",
                )]);
                self.outgoing.send_response(request_id, result).await;
                return;
            }
        };

        let thread_id = match codex_tool_call_reply_param.get_thread_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to parse thread_id: {e}");
                let result = CallToolResult::error(vec![rmcp::model::Content::text(format!(
                    "Failed to parse thread_id: {e}"
                ))]);
                self.outgoing.send_response(request_id, result).await;
                return;
            }
        };

        // Clone outgoing to move into async task.
        let outgoing = self.outgoing.clone();
        let running_requests_id_to_codex_uuid = self.running_requests_id_to_codex_uuid.clone();

        // 按 threadId 取回会话句柄；取不到（已结束/无效 ID）则回送带 threadId 的
        // error 结果，方便客户端知道是哪个线程失败。
        let codex = match self.thread_manager.get_thread(thread_id).await {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("Session not found for thread_id: {thread_id}");
                let result = crate::codex_tool_runner::create_call_tool_result_with_thread_id(
                    thread_id,
                    format!("Session not found for thread_id: {thread_id}"),
                    Some(true),
                );
                outgoing.send_response(request_id, result).await;
                return;
            }
        };

        // Spawn the long-running reply handler.
        let prompt = codex_tool_call_reply_param.prompt.clone();
        tokio::spawn({
            let outgoing = outgoing.clone();
            let running_requests_id_to_codex_uuid = running_requests_id_to_codex_uuid.clone();

            async move {
                crate::codex_tool_runner::run_codex_tool_session_reply(
                    thread_id,
                    codex,
                    outgoing,
                    request_id,
                    prompt,
                    running_requests_id_to_codex_uuid,
                )
                .await;
            }
        });
    }

    fn handle_set_level(&self, params: rmcp::model::SetLevelRequestParams) {
        tracing::info!("logging/setLevel -> params: {:?}", params);
    }

    fn handle_complete(&self, params: rmcp::model::CompleteRequestParams) {
        tracing::info!("completion/complete -> params: {:?}", params);
    }

    async fn handle_unsupported_request(&self, id: RequestId, method: &str) {
        self.outgoing
            .send_error(
                id,
                ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                    Some(json!({ "method": method })),
                ),
            )
            .await;
    }

    // ---------------------------------------------------------------------
    // Notification handlers
    // ---------------------------------------------------------------------

    /// 处理 `notifications/cancelled`：把客户端的取消请求翻译成对正在运行的
    /// Codex 会话提交一个 `Interrupt` 操作，并从在途映射表中注销该请求 ID。
    /// 找不到对应会话（已结束/未登记）则只记日志、静默返回。
    async fn handle_cancelled_notification(&self, params: rmcp::model::CancelledNotificationParam) {
        let request_id = params.request_id;
        // Create a stable string form early for logging and submission id.
        // 提前固定下字符串形式：既用于日志，也用作下面 Interrupt 提交的 id。
        let request_id_string = request_id.to_string();

        // Obtain the thread id while holding the first lock, then release.
        // 仅在持锁期间查出 thread_id 后立即释放锁（花括号收紧作用域）：避免在随后
        // 的 await 点（get_thread / submit）上仍持有锁，否则可能与会话 task 争用同
        // 一把锁而造成阻塞甚至死锁。
        let thread_id = {
            let map_guard = self.running_requests_id_to_codex_uuid.lock().await;
            match map_guard.get(&request_id) {
                Some(id) => *id,
                None => {
                    tracing::warn!("Session not found for request_id: {request_id_string}");
                    return;
                }
            }
        };
        tracing::info!("thread_id: {thread_id}");

        // Obtain the Codex thread from the server.
        // 拿 thread_id 去 ThreadManager 取回会话句柄（此时已不持有上面的锁）。
        let codex_arc = match self.thread_manager.get_thread(thread_id).await {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("Session not found for thread_id: {thread_id}");
                return;
            }
        };

        // Submit interrupt to Codex.
        // 用与原请求相同的 id 提交 Interrupt：保证中断操作能和原始 tools/call 关联起来。
        if let Err(e) = codex_arc
            .submit_with_id(Submission {
                id: request_id_string,
                op: codex_protocol::protocol::Op::Interrupt,
                trace: None,
            })
            .await
        {
            tracing::error!("Failed to submit interrupt to Codex: {e}");
            return;
        }
        // unregister the id so we don't keep it in the map
        // 中断已提交，注销该 ID（重新取锁）：避免映射表无限增长，也防止后续误处理。
        self.running_requests_id_to_codex_uuid
            .lock()
            .await
            .remove(&request_id);
    }

    fn handle_progress_notification(&self, params: rmcp::model::ProgressNotificationParam) {
        tracing::info!("notifications/progress -> params: {:?}", params);
    }

    fn handle_roots_list_changed(&self) {
        tracing::info!("notifications/roots/list_changed");
    }

    fn handle_initialized_notification(&self) {
        tracing::info!("notifications/initialized");
    }
}
