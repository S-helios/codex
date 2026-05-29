//! 【文件职责】app-server 的「请求分发中枢」。`MessageProcessor` 聚合了按领域切分的
//! 全部 Processor（thread/turn/fs/account/...），把每条入站 JSON-RPC 请求经「反序列化 →
//! 初始化握手网关 → 序列化范围(scope)排队 → 巨型 match 分发到对应 Processor」一路送达，
//! 并把结果回写为 response/error。
//!
//! 【架构位置】
//!   层级：前端集成层（app-server）· 入站方向核心
//!   上游：`lib.rs` 处理器循环（收到 `TransportEvent::IncomingMessage` 后调 `process_request`）
//!   下游：各 `request_processors::*`（领域处理器）→ 内核 `ThreadManager` / 本地资源；
//!         出站经 `OutgoingMessageSender`
//!
//! 【数据流】
//!   JSONRPCRequest → process_request（反序列化为 ClientRequest）→ handle_client_request
//!   → (Initialize 走握手 | 其余) dispatch_initialized_client_request（初始化检查 + 实验网关 +
//!   scope 排队）→ handle_initialized_client_request（约 400 行 match 委派）→ Processor → 回写
//!
//! 【阅读建议】请求生命周期的四段式依次是：`process_request`（§8.1 入口）→
//! `handle_client_request`（§8.2 Initialize 网关）→ `dispatch_initialized_client_request`
//! （§8.2 守门 + 入队）→ `handle_initialized_client_request`（§8.3 巨型分发）。
//! 顶部 `MessageProcessor::new` 是一次性装配（把上百个依赖织进各 Processor），可略读；
//! `ConnectionSessionState` 是「连接是否已握手」的真相源。`ExternalAuthRefreshBridge` 是
//! 一个反向调用案例（内核要刷新 token → 反过来向客户端发 server-request）。
//! 对照 learn_docs/5_前端_集成_协议/20_app_server_layer.md §8、§9、§10。

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use crate::attestation::app_server_attestation_provider;
use crate::config_manager::ConfigManager;
use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::error_code::invalid_request;
use crate::extensions::app_server_extension_event_sink;
use crate::extensions::guardian_agent_spawner;
use crate::extensions::thread_extensions;
use crate::fs_watch::FsWatchManager;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::RequestContext;
use crate::request_processors::AccountRequestProcessor;
use crate::request_processors::AppsRequestProcessor;
use crate::request_processors::CatalogRequestProcessor;
use crate::request_processors::CommandExecRequestProcessor;
use crate::request_processors::ConfigRequestProcessor;
use crate::request_processors::EnvironmentRequestProcessor;
use crate::request_processors::ExternalAgentConfigRequestProcessor;
use crate::request_processors::FeedbackRequestProcessor;
use crate::request_processors::FsRequestProcessor;
use crate::request_processors::GitRequestProcessor;
use crate::request_processors::InitializeRequestProcessor;
use crate::request_processors::MarketplaceRequestProcessor;
use crate::request_processors::McpRequestProcessor;
use crate::request_processors::PluginRequestProcessor;
use crate::request_processors::ProcessExecRequestProcessor;
use crate::request_processors::RemoteControlRequestProcessor;
use crate::request_processors::SearchRequestProcessor;
use crate::request_processors::ThreadGoalRequestProcessor;
use crate::request_processors::ThreadRequestProcessor;
use crate::request_processors::TurnRequestProcessor;
use crate::request_processors::WindowsSandboxRequestProcessor;
use crate::request_serialization::QueuedInitializedRequest;
use crate::request_serialization::RequestSerializationQueueKey;
use crate::request_serialization::RequestSerializationQueues;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::ThreadStateManager;
use crate::transport::AppServerTransport;
use crate::transport::RemoteControlHandle;
use async_trait::async_trait;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::AuthMode as LoginAuthMode;
use codex_app_server_protocol::ChatgptAuthTokensRefreshParams;
use codex_app_server_protocol::ChatgptAuthTokensRefreshReason;
use codex_app_server_protocol::ChatgptAuthTokensRefreshResponse;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::ExperimentalApi;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::experimental_required_message;
use codex_arg0::Arg0DispatchPaths;
use codex_chatgpt::workspace_settings;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_login::AuthManager;
use codex_login::auth::ExternalAuth;
use codex_login::auth::ExternalAuthRefreshContext;
use codex_login::auth::ExternalAuthRefreshReason;
use codex_login::auth::ExternalAuthTokens;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::W3cTraceContext;
use codex_rollout::StateDbHandle;
use codex_state::log_db::LogDbLayer;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

// 反向请求「刷新 ChatGPT token」的等待上限。超时即取消该 server-request 并报错，
// 避免内核侧的 auth 刷新调用被无回复的客户端无限期挂住。
const EXTERNAL_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

/// 把内核 `AuthManager` 的「外部 token 刷新」需求桥接成一次「服务端 → 客户端」反向请求。
/// 这是 app-server 少见的反向调用方向：当内核发现 token 失效时，不直接刷新，而是通过本桥
/// 向客户端发 `ChatgptAuthTokensRefresh` 请求、等其回传新 token。体现了「凭据归客户端持有」
/// 的设计——app-server 不掌握刷新逻辑，只做转发与超时兜底。
#[derive(Clone)]
struct ExternalAuthRefreshBridge {
    outgoing: Arc<OutgoingMessageSender>,
}

impl ExternalAuthRefreshBridge {
    fn map_reason(reason: ExternalAuthRefreshReason) -> ChatgptAuthTokensRefreshReason {
        match reason {
            ExternalAuthRefreshReason::Unauthorized => ChatgptAuthTokensRefreshReason::Unauthorized,
        }
    }
}

#[async_trait]
impl ExternalAuth for ExternalAuthRefreshBridge {
    fn auth_mode(&self) -> LoginAuthMode {
        LoginAuthMode::Chatgpt
    }

    async fn refresh(
        &self,
        context: ExternalAuthRefreshContext,
    ) -> std::io::Result<ExternalAuthTokens> {
        let params = ChatgptAuthTokensRefreshParams {
            reason: Self::map_reason(context.reason),
            previous_account_id: context.previous_account_id,
        };

        let (request_id, rx) = self
            .outgoing
            .send_request(ServerRequestPayload::ChatgptAuthTokensRefresh(params))
            .await;

        let result = match timeout(EXTERNAL_AUTH_REFRESH_TIMEOUT, rx).await {
            Ok(result) => {
                // Two failure scenarios:
                // 1) `oneshot::Receiver` failed (sender dropped) => request canceled/channel closed.
                // 2) client answered with JSON-RPC error payload => propagate code/message.
                let result = result.map_err(|err| {
                    std::io::Error::other(format!("auth refresh request canceled: {err}"))
                })?;
                result.map_err(|err| {
                    std::io::Error::other(format!(
                        "auth refresh request failed: code={} message={}",
                        err.code, err.message
                    ))
                })?
            }
            Err(_) => {
                let _canceled = self.outgoing.cancel_request(&request_id).await;
                return Err(std::io::Error::other(format!(
                    "auth refresh request timed out after {}s",
                    EXTERNAL_AUTH_REFRESH_TIMEOUT.as_secs()
                )));
            }
        };

        let response: ChatgptAuthTokensRefreshResponse =
            serde_json::from_value(result).map_err(std::io::Error::other)?;

        Ok(ExternalAuthTokens::chatgpt(
            response.access_token,
            response.chatgpt_account_id,
            response.chatgpt_plan_type,
        ))
    }
}

/// 【核心结构体】请求分发中枢。本质是「一个出站发射器 + 一组按领域切分的 Processor +
/// 一套请求序列化队列」的聚合器。`handle_initialized_client_request` 的巨型 match 把每个
/// `ClientRequest` 变体路由到下面对应的 `*_processor`。各 Processor 在 `new()` 中一次性装配，
/// 之间通过 `Arc` 共享 `ThreadManager`/配置/出站发射器等公共依赖。
pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    skills_watcher: Arc<SkillsWatcher>,
    account_processor: AccountRequestProcessor,
    apps_processor: AppsRequestProcessor,
    catalog_processor: CatalogRequestProcessor,
    command_exec_processor: CommandExecRequestProcessor,
    process_exec_processor: ProcessExecRequestProcessor,
    config_processor: ConfigRequestProcessor,
    environment_processor: EnvironmentRequestProcessor,
    external_agent_config_processor: ExternalAgentConfigRequestProcessor,
    feedback_processor: FeedbackRequestProcessor,
    fs_processor: FsRequestProcessor,
    git_processor: GitRequestProcessor,
    initialize_processor: InitializeRequestProcessor,
    marketplace_processor: MarketplaceRequestProcessor,
    mcp_processor: McpRequestProcessor,
    plugin_processor: PluginRequestProcessor,
    remote_control_processor: RemoteControlRequestProcessor,
    search_processor: SearchRequestProcessor,
    thread_goal_processor: ThreadGoalRequestProcessor,
    thread_processor: ThreadRequestProcessor,
    turn_processor: TurnRequestProcessor,
    windows_sandbox_processor: WindowsSandboxRequestProcessor,
    request_serialization_queues: RequestSerializationQueues,
}

/// 单个连接的会话状态。`initialized` 用 `OnceLock`——握手完成后「写一次、此后只读」，
/// 天然契合「一个连接只初始化一次」的约束，且无需加锁即可被多任务并发读。
/// 未初始化时 `initialized.get()` 为 `None`，正是 §8.2 守门检查「是否已 Initialize」的依据。
#[derive(Debug)]
pub(crate) struct ConnectionSessionState {
    pub(crate) rpc_gate: Arc<ConnectionRpcGate>,
    initialized: OnceLock<InitializedConnectionSessionState>,
}

/// 握手完成后才确定的连接级能力与身份：是否启用实验 API（实验字段/方法的运行期闸门）、
/// 退订了哪些通知方法、客户端名称/版本、是否要求 attestation。这些值在 Initialize 时一次性
/// 写入，后续请求据此做网关判定与通知过滤。
#[derive(Debug)]
pub(crate) struct InitializedConnectionSessionState {
    pub(crate) experimental_api_enabled: bool,
    pub(crate) opted_out_notification_methods: HashSet<String>,
    pub(crate) app_server_client_name: String,
    pub(crate) client_version: String,
    pub(crate) request_attestation: bool,
}

impl Default for ConnectionSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionSessionState {
    pub(crate) fn new() -> Self {
        Self {
            rpc_gate: Arc::new(ConnectionRpcGate::new()),
            initialized: OnceLock::new(),
        }
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized.get().is_some()
    }

    pub(crate) fn experimental_api_enabled(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.experimental_api_enabled)
    }

    pub(crate) fn opted_out_notification_methods(&self) -> HashSet<String> {
        self.initialized
            .get()
            .map(|session| session.opted_out_notification_methods.clone())
            .unwrap_or_default()
    }

    pub(crate) fn app_server_client_name(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.app_server_client_name.as_str())
    }

    pub(crate) fn client_version(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.client_version.as_str())
    }

    pub(crate) fn request_attestation(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.request_attestation)
    }

    pub(crate) fn initialize(&self, session: InitializedConnectionSessionState) -> Result<(), ()> {
        self.initialized.set(session).map_err(|_| ())
    }
}

pub(crate) struct MessageProcessorArgs {
    pub(crate) outgoing: Arc<OutgoingMessageSender>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) arg0_paths: Arg0DispatchPaths,
    pub(crate) config: Arc<Config>,
    pub(crate) config_manager: ConfigManager,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) feedback: CodexFeedback,
    pub(crate) log_db: Option<LogDbLayer>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) config_warnings: Vec<ConfigWarningNotification>,
    pub(crate) session_source: SessionSource,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) installation_id: String,
    pub(crate) rpc_transport: AppServerRpcTransport,
    pub(crate) remote_control_handle: Option<RemoteControlHandle>,
    pub(crate) plugin_startup_tasks: crate::PluginStartupTasks,
}

impl MessageProcessor {
    /// Create a new `MessageProcessor`, retaining a handle to the outgoing
    /// `Sender` so handlers can enqueue messages to be written to stdout.
    /// 一次性装配：构建 `ThreadManager` 等公共依赖，再把它们以 `Arc` 织进各领域 Processor。
    /// 本函数很长但逻辑线性（几乎全是「new 出一个 Processor 并塞进字段」），无需逐个细读；
    /// 关注点应放在分发逻辑（`handle_initialized_client_request`）而非这里的接线细节。
    pub(crate) fn new(args: MessageProcessorArgs) -> Self {
        let MessageProcessorArgs {
            outgoing,
            analytics_events_client,
            arg0_paths,
            config,
            config_manager,
            environment_manager,
            feedback,
            log_db,
            state_db,
            config_warnings,
            session_source,
            auth_manager,
            installation_id,
            rpc_transport,
            remote_control_handle,
            plugin_startup_tasks,
        } = args;
        auth_manager.set_external_auth(Arc::new(ExternalAuthRefreshBridge {
            outgoing: outgoing.clone(),
        }));
        let thread_state_manager = ThreadStateManager::new();
        // The thread store is intentionally process-scoped. Config reloads can
        // affect per-thread behavior, but they must not move newly started,
        // resumed, or forked threads to a different persistence backend/root.
        let thread_store = codex_core::thread_store_from_config(config.as_ref(), state_db.clone());
        let environment_manager_for_requests = Arc::clone(&environment_manager);
        let thread_manager = Arc::new_cyclic(|thread_manager| {
            ThreadManager::new(
                config.as_ref(),
                auth_manager.clone(),
                session_source,
                environment_manager,
                thread_extensions(
                    guardian_agent_spawner(thread_manager.clone()),
                    app_server_extension_event_sink(outgoing.clone()),
                    auth_manager.clone(),
                ),
                Some(analytics_events_client.clone()),
                Arc::clone(&thread_store),
                state_db.clone(),
                installation_id,
                Some(app_server_attestation_provider(
                    outgoing.clone(),
                    thread_state_manager.clone(),
                )),
            )
        });
        thread_manager
            .plugins_manager()
            .set_analytics_events_client(analytics_events_client.clone());
        let skills_watcher = SkillsWatcher::new(thread_manager.skills_manager(), outgoing.clone());

        let pending_thread_unloads = Arc::new(Mutex::new(HashSet::new()));
        let thread_watch_manager =
            crate::thread_status::ThreadWatchManager::new_with_outgoing(outgoing.clone());
        let thread_list_state_permit = Arc::new(Semaphore::new(/*permits*/ 1));
        let workspace_settings_cache =
            Arc::new(workspace_settings::WorkspaceSettingsCache::default());
        let app_list_shutdown_token = CancellationToken::new();
        let account_processor = AccountRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            Arc::clone(&config),
            config_manager.clone(),
        );
        let apps_processor = AppsRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            config_manager.clone(),
            Arc::clone(&workspace_settings_cache),
            app_list_shutdown_token,
        );
        let catalog_processor = CatalogRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            Arc::clone(&config),
            config_manager.clone(),
            Arc::clone(&workspace_settings_cache),
        );
        let command_exec_processor = CommandExecRequestProcessor::new(
            arg0_paths.clone(),
            Arc::clone(&config),
            outgoing.clone(),
            config_manager.clone(),
            Arc::clone(&environment_manager_for_requests),
        );
        let process_exec_processor = ProcessExecRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&environment_manager_for_requests),
        );
        let feedback_processor = FeedbackRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            Arc::clone(&config),
            feedback,
            log_db,
            state_db.clone(),
        );
        let git_processor = GitRequestProcessor::new();
        let initialize_processor = InitializeRequestProcessor::new(
            outgoing.clone(),
            analytics_events_client.clone(),
            Arc::clone(&config),
            config_warnings,
            rpc_transport,
        );
        let marketplace_processor = MarketplaceRequestProcessor::new(
            Arc::clone(&config),
            config_manager.clone(),
            Arc::clone(&thread_manager),
        );
        let mcp_processor = McpRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            config_manager.clone(),
        );
        let plugin_processor = PluginRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            analytics_events_client.clone(),
            config_manager.clone(),
            workspace_settings_cache,
        );
        let remote_control_processor = RemoteControlRequestProcessor::new(remote_control_handle);
        let search_processor = SearchRequestProcessor::new(outgoing.clone());
        let thread_goal_processor = ThreadGoalRequestProcessor::new(
            Arc::clone(&thread_manager),
            outgoing.clone(),
            Arc::clone(&config),
            thread_state_manager.clone(),
            state_db.clone(),
        );
        let thread_processor = ThreadRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            arg0_paths.clone(),
            Arc::clone(&config),
            config_manager.clone(),
            Arc::clone(&thread_store),
            Arc::clone(&pending_thread_unloads),
            thread_state_manager.clone(),
            thread_watch_manager.clone(),
            Arc::clone(&thread_list_state_permit),
            thread_goal_processor.clone(),
            state_db,
            Arc::clone(&skills_watcher),
        );
        let turn_processor = TurnRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            analytics_events_client.clone(),
            arg0_paths.clone(),
            Arc::clone(&config),
            config_manager.clone(),
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            Arc::clone(&skills_watcher),
        );
        if matches!(plugin_startup_tasks, crate::PluginStartupTasks::Start) {
            // Keep plugin startup warmups aligned at app-server startup.
            let on_effective_plugins_changed =
                plugin_processor.effective_plugins_changed_callback();
            thread_manager
                .plugins_manager()
                .maybe_start_plugin_startup_tasks_for_config(
                    &config.plugins_config_input(),
                    auth_manager.clone(),
                    Some(on_effective_plugins_changed),
                );
        }
        let config_processor = ConfigRequestProcessor::new(
            outgoing.clone(),
            config_manager.clone(),
            auth_manager,
            thread_manager.clone(),
            analytics_events_client,
        );
        let external_agent_config_processor = ExternalAgentConfigRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&thread_manager),
            config_manager.clone(),
            config_processor.clone(),
            arg0_paths,
            config.codex_home.to_path_buf(),
        );
        let environment_processor =
            EnvironmentRequestProcessor::new(thread_manager.environment_manager());
        let fs_processor = FsRequestProcessor::new(
            Arc::clone(&environment_manager_for_requests),
            FsWatchManager::new(outgoing.clone()),
        );
        let windows_sandbox_processor = WindowsSandboxRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&config),
            config_manager,
        );

        Self {
            outgoing,
            skills_watcher,
            account_processor,
            apps_processor,
            catalog_processor,
            command_exec_processor,
            process_exec_processor,
            config_processor,
            environment_processor,
            external_agent_config_processor,
            feedback_processor,
            fs_processor,
            git_processor,
            initialize_processor,
            marketplace_processor,
            mcp_processor,
            plugin_processor,
            remote_control_processor,
            search_processor,
            thread_goal_processor,
            thread_processor,
            turn_processor,
            windows_sandbox_processor,
            request_serialization_queues: RequestSerializationQueues::default(),
        }
    }

    pub(crate) fn clear_runtime_references(&self) {
        self.account_processor.clear_external_auth();
        self.apps_processor.shutdown();
        self.skills_watcher.shutdown();
    }

    /// 【入站 JSON-RPC 入口】处理一条来自 transport 的 `JSONRPCRequest`。
    /// 关键步骤：① 组装 `RequestContext`（id + tracing span + 上游 W3C trace）；
    /// ② 把 `JSONRPCRequest` 先序列化回 `Value`、再反序列化成强类型 `ClientRequest`
    /// （协议壳层 → 强类型的交接点，失败即回 invalid_request）；③ 委派 `handle_client_request`。
    ///
    /// 注意 `outbound_initialized` 传 `None`：websocket 路径下，「标记连接 outbound 就绪」
    /// 由 lib.rs 在镜像会话状态 + 发完连接级初始通知之后收尾，此处提前标记会引发竞态
    /// （见 lib.rs 中「became initialized」一段）。in-process 路径则不同，见
    /// `process_client_request`。
    pub(crate) async fn process_request(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        request: JSONRPCRequest,
        transport: &AppServerTransport,
        session: Arc<ConnectionSessionState>,
    ) {
        let request_method = request.method.as_str();
        tracing::trace!(
            ?connection_id,
            request_id = ?request.id,
            "app-server request: {request_method}"
        );
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: request.id.clone(),
        };
        let request_span =
            crate::app_server_tracing::request_span(&request, transport, connection_id, &session);
        let request_trace = request.trace.as_ref().map(|trace| W3cTraceContext {
            traceparent: trace.traceparent.clone(),
            tracestate: trace.tracestate.clone(),
        });
        let request_context = RequestContext::new(request_id.clone(), request_span, request_trace);
        Self::run_request_with_context(
            Arc::clone(&self.outgoing),
            request_context.clone(),
            async {
                let codex_request = serde_json::to_value(&request)
                    .map_err(|err| invalid_request(format!("Invalid request: {err}")))
                    .and_then(|request_json| {
                        serde_json::from_value::<ClientRequest>(request_json)
                            .map_err(|err| invalid_request(format!("Invalid request: {err}")))
                    });
                let result = match codex_request {
                    Ok(codex_request) => {
                        // Websocket callers finalize outbound readiness in lib.rs after mirroring
                        // session state into outbound state and sending initialize notifications to
                        // this specific connection. Passing `None` avoids marking the connection
                        // ready too early from inside the shared request handler.
                        self.handle_client_request(
                            request_id.clone(),
                            codex_request,
                            Arc::clone(&session),
                            /*outbound_initialized*/ None,
                            request_context.clone(),
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    self.outgoing.send_error(request_id.clone(), error).await;
                }
            },
        )
        .await;
    }

    /// Handles a typed request path used by in-process embedders.
    ///
    /// This bypasses JSON request deserialization but keeps identical request
    /// semantics by delegating to `handle_client_request`.
    /// in-process 嵌入方的「类型直通」入口：直接拿到 `ClientRequest`，跳过 JSON 反序列化，
    /// 但语义与 `process_request` 完全一致（同样委派 `handle_client_request`）。
    /// 与 websocket 路径的唯一差异：in-process 没有 lib.rs 那套握手后收尾，故这里把
    /// `outbound_initialized` 以 `Some(...)` 传入，让共享处理器自己完成「标记就绪」。
    pub(crate) async fn process_client_request(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        outbound_initialized: &AtomicBool,
    ) {
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: request.id().clone(),
        };
        let request_span =
            crate::app_server_tracing::typed_request_span(&request, connection_id, &session);
        let request_context =
            RequestContext::new(request_id.clone(), request_span, /*parent_trace*/ None);
        tracing::trace!(
            ?connection_id,
            request_id = ?request_id.request_id,
            "app-server typed request"
        );
        Self::run_request_with_context(
            Arc::clone(&self.outgoing),
            request_context.clone(),
            async {
                // In-process clients do not have the websocket transport loop that performs
                // post-initialize bookkeeping, so they still finalize outbound readiness in
                // the shared request handler.
                let result = self
                    .handle_client_request(
                        request_id.clone(),
                        request,
                        Arc::clone(&session),
                        Some(outbound_initialized),
                        request_context.clone(),
                    )
                    .await;
                if let Err(error) = result {
                    self.outgoing.send_error(request_id.clone(), error).await;
                }
            },
        )
        .await;
    }

    pub(crate) async fn process_notification(&self, notification: JSONRPCNotification) {
        // Currently, we do not expect to receive any notifications from the
        // client, so we just log them.
        tracing::info!("<- notification: {:?}", notification);
    }

    /// Handles typed notifications from in-process clients.
    pub(crate) async fn process_client_notification(&self, notification: ClientNotification) {
        // Currently, we do not expect to receive any typed notifications from
        // in-process clients, so we just log them.
        tracing::info!("<- typed notification: {:?}", notification);
    }

    async fn run_request_with_context<F>(
        outgoing: Arc<OutgoingMessageSender>,
        request_context: RequestContext,
        request_fut: F,
    ) where
        F: Future<Output = ()>,
    {
        outgoing
            .register_request_context(request_context.clone())
            .await;
        request_fut.instrument(request_context.span()).await;
    }

    pub(crate) fn thread_created_receiver(&self) -> broadcast::Receiver<ThreadId> {
        self.thread_processor.thread_created_receiver()
    }

    pub(crate) async fn send_initialize_notifications_to_connection(
        &self,
        connection_id: ConnectionId,
    ) {
        self.initialize_processor
            .send_initialize_notifications_to_connection(connection_id)
            .await;
    }

    pub(crate) async fn connection_initialized(
        &self,
        connection_id: ConnectionId,
        request_attestation: bool,
    ) {
        self.thread_processor
            .connection_initialized(
                connection_id,
                ConnectionCapabilities {
                    request_attestation,
                },
            )
            .await;
    }

    pub(crate) async fn send_initialize_notifications(&self) {
        self.initialize_processor
            .send_initialize_notifications()
            .await;
    }

    pub(crate) async fn try_attach_thread_listener(
        &self,
        thread_id: ThreadId,
        connection_ids: Vec<ConnectionId>,
    ) {
        self.thread_processor
            .try_attach_thread_listener(thread_id, connection_ids)
            .await;
    }

    pub(crate) async fn drain_background_tasks(&self) {
        self.thread_processor.drain_background_tasks().await;
    }

    pub(crate) async fn cancel_active_login(&self) {
        self.account_processor.cancel_active_login().await;
    }

    pub(crate) async fn clear_all_thread_listeners(&self) {
        self.thread_processor.clear_all_thread_listeners().await;
    }

    pub(crate) async fn shutdown_threads(&self) {
        self.thread_processor.shutdown_threads().await;
    }

    pub(crate) async fn connection_closed(
        &self,
        connection_id: ConnectionId,
        session_state: &ConnectionSessionState,
    ) {
        session_state.rpc_gate.shutdown().await;
        self.outgoing.connection_closed(connection_id).await;
        self.fs_processor.connection_closed(connection_id).await;
        self.command_exec_processor
            .connection_closed(connection_id)
            .await;
        self.process_exec_processor
            .connection_closed(connection_id)
            .await;
        self.thread_processor.connection_closed(connection_id).await;
    }

    pub(crate) fn subscribe_running_assistant_turn_count(&self) -> watch::Receiver<usize> {
        self.thread_processor
            .subscribe_running_assistant_turn_count()
    }

    /// Handle a standalone JSON-RPC response originating from the peer.
    /// 客户端对「服务端反向请求」的回复落点：转交 `OutgoingMessageSender` 按 id 匹配回调，
    /// 唤醒之前在 `send_request*` 处等待的调用方（如审批/token 刷新）。
    pub(crate) async fn process_response(&self, response: JSONRPCResponse) {
        tracing::info!("<- response: {:?}", response);
        let JSONRPCResponse { id, result, .. } = response;
        self.outgoing.notify_client_response(id, result).await
    }

    /// Handle an error object received from the peer.
    pub(crate) async fn process_error(&self, err: JSONRPCError) {
        tracing::error!("<- error: {:?}", err);
        self.outgoing.notify_client_error(err.id, err.error).await;
    }

    /// 【初始化握手网关】所有请求的第一道分叉：`Initialize` 在此就地处理（握手并标记连接
    /// 就绪），其余一律下放给 `dispatch_initialized_client_request`。
    /// 把握手单独前置，是因为它是「连接尚未初始化时唯一允许的请求」——其它请求若先到，
    /// 会在下游守门处被拒（见 `dispatch_initialized_client_request` 第一步）。
    async fn handle_client_request(
        self: &Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        // `Some(...)` means the caller wants initialize to immediately mark the
        // connection outbound-ready. Websocket JSON-RPC calls pass `None` so
        // lib.rs can deliver connection-scoped initialize notifications first.
        outbound_initialized: Option<&AtomicBool>,
        request_context: RequestContext,
    ) -> Result<(), JSONRPCErrorError> {
        let connection_id = connection_request_id.connection_id;
        if let ClientRequest::Initialize { request_id, params } = codex_request {
            let connection_initialized = self
                .initialize_processor
                .initialize(
                    connection_id,
                    request_id,
                    params,
                    &session,
                    outbound_initialized,
                )
                .await?;
            if connection_initialized {
                self.thread_processor
                    .connection_initialized(
                        connection_id,
                        ConnectionCapabilities {
                            request_attestation: session.request_attestation(),
                        },
                    )
                    .await;
            }
            return Ok(());
        }

        self.dispatch_initialized_client_request(
            connection_request_id,
            codex_request,
            session,
            request_context,
        )
        .await
    }

    /// 【守门 + 序列化排队】对「已初始化连接的非 Initialize 请求」做三道把关后，决定如何执行。
    ///
    /// 三道门：① 未握手直接拒（`Not initialized`）；② 请求含实验性字段/方法但连接未启用
    /// 实验 API → 拒（运行期实验闸门，见 §10）；③ 取请求的「序列化范围 scope」决定调度方式。
    ///
    /// 调度：有 scope → 映射成 `(队列键, 访问模式)` 入序列化队列，保证「同一资源」的请求串行
    /// （如同一 thread 的 settings 更新与 turn 启动按提交顺序执行，杜绝竞态）；无 scope（如
    /// `thread/start`，尚无 thread_id）→ 直接 `tokio::spawn` 并发执行。这层串行是 app-server
    /// 自己的、位于「到达内核之前」，与内核 SQ 互补（见 §9）。
    async fn dispatch_initialized_client_request(
        self: &Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        request_context: RequestContext,
    ) -> Result<(), JSONRPCErrorError> {
        // Step 1：初始化检查——没握手就发其它请求一律拒。
        if !session.initialized() {
            return Err(invalid_request("Not initialized"));
        }

        // Step 2：实验性 API 闸门——含实验字段/方法且连接未开实验，则拒。
        if let Some(reason) = codex_request.experimental_reason()
            && !session.experimental_api_enabled()
        {
            return Err(invalid_request(experimental_required_message(reason)));
        }
        let connection_id = connection_request_id.connection_id;
        self.initialize_processor.track_initialized_request(
            connection_id,
            connection_request_id.request_id.clone(),
            &codex_request,
        );

        let serialization_scope = codex_request.serialization_scope();
        let app_server_client_name = session.app_server_client_name().map(str::to_string);
        let client_version = session.client_version().map(str::to_string);
        let error_request_id = connection_request_id.clone();
        let rpc_gate = Arc::clone(&session.rpc_gate);
        let processor = Arc::clone(self);
        let span = request_context.span();
        let request = QueuedInitializedRequest::new(
            rpc_gate,
            async move {
                let processor_for_request = Arc::clone(&processor);
                let result = processor_for_request
                    .handle_initialized_client_request(
                        connection_request_id,
                        codex_request,
                        request_context,
                        app_server_client_name,
                        client_version,
                    )
                    .await;
                if let Err(error) = result {
                    processor.outgoing.send_error(error_request_id, error).await;
                }
            }
            .instrument(span),
        );

        // Step 3：按 scope 调度。有 scope → 入对应队列串行；无 scope → 直接并发 spawn。
        if let Some(scope) = serialization_scope {
            let (key, access) = RequestSerializationQueueKey::from_scope(connection_id, scope);
            self.request_serialization_queues
                .enqueue(key, access, request)
                .await;
        } else {
            tokio::spawn(async move {
                request.run().await;
            });
        }
        Ok(())
    }

    /// 【巨型分发匹配块】请求生命周期的终点：一个约 400 行的 `match codex_request { ... }`，
    /// 把每个 `ClientRequest` 变体委派给对应领域的 Processor 方法。各分支形态高度一致
    /// （`变体 { params } => self.某_processor.某方法(params).await`），故下方不逐臂注释——
    /// 看一两个即明全貌；Processor 返回 `Result<Option<ClientResponsePayload>, _>`，
    /// 函数末尾统一收口：`Ok(Some)` 发响应、`Ok(None)` 静默（结果由 Processor 自行异步回写）、
    /// `Err` 发错误。注意 `Initialize` 分支是 `panic!`——它本应在 `handle_client_request`
    /// 就被截走，走到这里说明分发逻辑被破坏，属编程错误而非运行时输入错误。
    async fn handle_initialized_client_request(
        self: Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        request_context: RequestContext,
        app_server_client_name: Option<String>,
        client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let connection_id = connection_request_id.connection_id;
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: codex_request.id().clone(),
        };

        let result: Result<Option<ClientResponsePayload>, JSONRPCErrorError> = match codex_request {
            ClientRequest::Initialize { .. } => {
                panic!("Initialize should be handled before initialized request dispatch");
            }
            ClientRequest::ConfigRead { params, .. } => self
                .config_processor
                .read(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::WindowsSandboxReadiness { .. } => self
                .windows_sandbox_processor
                .windows_sandbox_readiness()
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ExternalAgentConfigDetect { params, .. } => self
                .external_agent_config_processor
                .detect(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ExternalAgentConfigImport { params, .. } => self
                .external_agent_config_processor
                .import(request_id.clone(), params)
                .await
                .map(|()| None),
            ClientRequest::ConfigValueWrite { params, .. } => {
                self.config_processor.value_write(params).await.map(Some)
            }
            ClientRequest::ConfigBatchWrite { params, .. } => {
                self.config_processor.batch_write(params).await.map(Some)
            }
            ClientRequest::ExperimentalFeatureEnablementSet { params, .. } => {
                self.config_processor
                    .experimental_feature_enablement_set(request_id.clone(), params)
                    .await
            }
            ClientRequest::RemoteControlEnable { .. } => self
                .remote_control_processor
                .enable()
                .map(|response| Some(response.into())),
            ClientRequest::RemoteControlDisable { .. } => self
                .remote_control_processor
                .disable()
                .map(|response| Some(response.into())),
            ClientRequest::RemoteControlStatusRead { .. } => self
                .remote_control_processor
                .status_read()
                .map(|response| Some(response.into())),
            ClientRequest::ConfigRequirementsRead { params: _, .. } => self
                .config_processor
                .config_requirements_read()
                .await
                .map(|response| Some(response.into())),
            ClientRequest::EnvironmentAdd { params, .. } => {
                self.environment_processor.environment_add(params).await
            }
            ClientRequest::FsReadFile { params, .. } => self
                .fs_processor
                .read_file(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsWriteFile { params, .. } => self
                .fs_processor
                .write_file(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsCreateDirectory { params, .. } => self
                .fs_processor
                .create_directory(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsGetMetadata { params, .. } => self
                .fs_processor
                .get_metadata(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsReadDirectory { params, .. } => self
                .fs_processor
                .read_directory(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsRemove { params, .. } => self
                .fs_processor
                .remove(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsCopy { params, .. } => self
                .fs_processor
                .copy(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsWatch { params, .. } => self
                .fs_processor
                .watch(connection_id, params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsUnwatch { params, .. } => self
                .fs_processor
                .unwatch(connection_id, params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ModelProviderCapabilitiesRead { params: _, .. } => self
                .config_processor
                .model_provider_capabilities_read()
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ThreadStart { params, .. } => {
                self.thread_processor
                    .thread_start(
                        request_id.clone(),
                        params,
                        app_server_client_name.clone(),
                        client_version.clone(),
                        request_context,
                    )
                    .await
            }
            ClientRequest::ThreadUnsubscribe { params, .. } => {
                self.thread_processor
                    .thread_unsubscribe(&request_id, params)
                    .await
            }
            ClientRequest::ThreadResume { params, .. } => {
                self.thread_processor
                    .thread_resume(
                        request_id.clone(),
                        params,
                        app_server_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadFork { params, .. } => {
                self.thread_processor
                    .thread_fork(
                        request_id.clone(),
                        params,
                        app_server_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadArchive { params, .. } => {
                self.thread_processor
                    .thread_archive(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadIncrementElicitation { params, .. } => {
                self.thread_processor
                    .thread_increment_elicitation(params)
                    .await
            }
            ClientRequest::ThreadDecrementElicitation { params, .. } => {
                self.thread_processor
                    .thread_decrement_elicitation(params)
                    .await
            }
            ClientRequest::ThreadSetName { params, .. } => {
                self.thread_processor
                    .thread_set_name(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadGoalSet { params, .. } => {
                self.thread_goal_processor
                    .thread_goal_set(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadGoalGet { params, .. } => {
                self.thread_goal_processor.thread_goal_get(params).await
            }
            ClientRequest::ThreadGoalClear { params, .. } => {
                self.thread_goal_processor
                    .thread_goal_clear(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadMetadataUpdate { params, .. } => {
                self.thread_processor.thread_metadata_update(params).await
            }
            ClientRequest::ThreadSettingsUpdate { params, .. } => {
                self.turn_processor
                    .thread_settings_update(&request_id, params)
                    .await
            }
            ClientRequest::ThreadMemoryModeSet { params, .. } => {
                self.thread_processor.thread_memory_mode_set(params).await
            }
            ClientRequest::MemoryReset { .. } => self.thread_processor.memory_reset().await,
            ClientRequest::ThreadUnarchive { params, .. } => {
                self.thread_processor
                    .thread_unarchive(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadCompactStart { params, .. } => {
                self.thread_processor
                    .thread_compact_start(&request_id, params)
                    .await
            }
            ClientRequest::ThreadBackgroundTerminalsClean { params, .. } => {
                self.thread_processor
                    .thread_background_terminals_clean(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRollback { params, .. } => {
                self.thread_processor
                    .thread_rollback(&request_id, params)
                    .await
            }
            ClientRequest::ThreadList { params, .. } => {
                self.thread_processor.thread_list(params).await
            }
            ClientRequest::ThreadSearch { params, .. } => {
                self.thread_processor.thread_search(params).await
            }
            ClientRequest::ThreadLoadedList { params, .. } => {
                self.thread_processor.thread_loaded_list(params).await
            }
            ClientRequest::ThreadRead { params, .. } => {
                self.thread_processor.thread_read(params).await
            }
            ClientRequest::ThreadTurnsList { params, .. } => {
                self.thread_processor.thread_turns_list(params).await
            }
            ClientRequest::ThreadTurnsItemsList { params, .. } => {
                self.thread_processor.thread_turns_items_list(params).await
            }
            ClientRequest::ThreadShellCommand { params, .. } => {
                self.thread_processor
                    .thread_shell_command(&request_id, params)
                    .await
            }
            ClientRequest::ThreadApproveGuardianDeniedAction { params, .. } => {
                self.thread_processor
                    .thread_approve_guardian_denied_action(&request_id, params)
                    .await
            }
            ClientRequest::GetConversationSummary { params, .. } => {
                self.thread_processor.conversation_summary(params).await
            }
            ClientRequest::SkillsList { params, .. } => {
                self.catalog_processor.skills_list(params).await
            }
            ClientRequest::HooksList { params, .. } => {
                self.catalog_processor.hooks_list(params).await
            }
            ClientRequest::MarketplaceAdd { params, .. } => {
                self.marketplace_processor.marketplace_add(params).await
            }
            ClientRequest::MarketplaceRemove { params, .. } => {
                self.marketplace_processor.marketplace_remove(params).await
            }
            ClientRequest::MarketplaceUpgrade { params, .. } => {
                self.marketplace_processor.marketplace_upgrade(params).await
            }
            ClientRequest::PluginList { params, .. } => {
                self.plugin_processor.plugin_list(params).await
            }
            ClientRequest::PluginInstalled { params, .. } => {
                self.plugin_processor.plugin_installed(params).await
            }
            ClientRequest::PluginRead { params, .. } => {
                self.plugin_processor.plugin_read(params).await
            }
            ClientRequest::PluginSkillRead { params, .. } => {
                self.plugin_processor.plugin_skill_read(params).await
            }
            ClientRequest::PluginShareSave { params, .. } => {
                self.plugin_processor.plugin_share_save(params).await
            }
            ClientRequest::PluginShareUpdateTargets { params, .. } => {
                self.plugin_processor
                    .plugin_share_update_targets(params)
                    .await
            }
            ClientRequest::PluginShareList { params, .. } => {
                self.plugin_processor.plugin_share_list(params).await
            }
            ClientRequest::PluginShareCheckout { params, .. } => {
                self.plugin_processor.plugin_share_checkout(params).await
            }
            ClientRequest::PluginShareDelete { params, .. } => {
                self.plugin_processor.plugin_share_delete(params).await
            }
            ClientRequest::AppsList { params, .. } => {
                self.apps_processor.apps_list(&request_id, params).await
            }
            ClientRequest::SkillsConfigWrite { params, .. } => {
                self.catalog_processor.skills_config_write(params).await
            }
            ClientRequest::PluginInstall { params, .. } => {
                self.plugin_processor.plugin_install(params).await
            }
            ClientRequest::PluginUninstall { params, .. } => {
                self.plugin_processor.plugin_uninstall(params).await
            }
            ClientRequest::ModelList { params, .. } => {
                self.catalog_processor.model_list(params).await
            }
            ClientRequest::ExperimentalFeatureList { params, .. } => {
                self.catalog_processor
                    .experimental_feature_list(params)
                    .await
            }
            ClientRequest::PermissionProfileList { params, .. } => {
                self.catalog_processor.permission_profile_list(params).await
            }
            ClientRequest::CollaborationModeList { params, .. } => {
                self.catalog_processor.collaboration_mode_list(params).await
            }
            ClientRequest::MockExperimentalMethod { params, .. } => {
                self.catalog_processor
                    .mock_experimental_method(params)
                    .await
            }
            ClientRequest::TurnStart { params, .. } => {
                self.turn_processor
                    .turn_start(
                        request_id.clone(),
                        params,
                        app_server_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadInjectItems { params, .. } => {
                self.turn_processor.thread_inject_items(params).await
            }
            ClientRequest::TurnSteer { params, .. } => {
                self.turn_processor.turn_steer(&request_id, params).await
            }
            ClientRequest::TurnInterrupt { params, .. } => {
                self.turn_processor
                    .turn_interrupt(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeStart { params, .. } => {
                self.turn_processor
                    .thread_realtime_start(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeAppendAudio { params, .. } => {
                self.turn_processor
                    .thread_realtime_append_audio(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeAppendText { params, .. } => {
                self.turn_processor
                    .thread_realtime_append_text(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeStop { params, .. } => {
                self.turn_processor
                    .thread_realtime_stop(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeListVoices { params: _, .. } => {
                self.turn_processor.thread_realtime_list_voices().await
            }
            ClientRequest::ReviewStart { params, .. } => {
                self.turn_processor.review_start(&request_id, params).await
            }
            ClientRequest::McpServerOauthLogin { params, .. } => {
                self.mcp_processor.mcp_server_oauth_login(params).await
            }
            ClientRequest::McpServerRefresh { params, .. } => {
                self.mcp_processor.mcp_server_refresh(params).await
            }
            ClientRequest::McpServerStatusList { params, .. } => {
                self.mcp_processor
                    .mcp_server_status_list(&request_id, params)
                    .await
            }
            ClientRequest::McpResourceRead { params, .. } => {
                self.mcp_processor
                    .mcp_resource_read(&request_id, params)
                    .await
            }
            ClientRequest::McpServerToolCall { params, .. } => {
                self.mcp_processor
                    .mcp_server_tool_call(&request_id, params)
                    .await
            }
            ClientRequest::WindowsSandboxSetupStart { params, .. } => {
                self.windows_sandbox_processor
                    .windows_sandbox_setup_start(&request_id, params)
                    .await
            }
            ClientRequest::LoginAccount { params, .. } => {
                self.account_processor
                    .login_account(request_id.clone(), params)
                    .await
            }
            ClientRequest::LogoutAccount { .. } => {
                self.account_processor
                    .logout_account(request_id.clone())
                    .await
            }
            ClientRequest::CancelLoginAccount { params, .. } => {
                self.account_processor.cancel_login_account(params).await
            }
            ClientRequest::GetAccount { params, .. } => {
                self.account_processor.get_account(params).await
            }
            ClientRequest::GetAuthStatus { params, .. } => {
                self.account_processor.get_auth_status(params).await
            }
            ClientRequest::GetAccountRateLimits { .. } => {
                self.account_processor.get_account_rate_limits().await
            }
            ClientRequest::SendAddCreditsNudgeEmail { params, .. } => {
                self.account_processor
                    .send_add_credits_nudge_email(params)
                    .await
            }
            ClientRequest::GitDiffToRemote { params, .. } => {
                self.git_processor.git_diff_to_remote(params).await
            }
            ClientRequest::FuzzyFileSearch { params, .. } => self
                .search_processor
                .fuzzy_file_search(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionStart { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_start_response(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionUpdate { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_update_response(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionStop { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_stop(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::OneOffCommandExec { params, .. } => {
                self.command_exec_processor
                    .one_off_command_exec(&request_id, params)
                    .await
            }
            ClientRequest::CommandExecWrite { params, .. } => {
                self.command_exec_processor
                    .command_exec_write(request_id.clone(), params)
                    .await
            }
            ClientRequest::CommandExecResize { params, .. } => {
                self.command_exec_processor
                    .command_exec_resize(request_id.clone(), params)
                    .await
            }
            ClientRequest::CommandExecTerminate { params, .. } => {
                self.command_exec_processor
                    .command_exec_terminate(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessSpawn { params, .. } => self
                .process_exec_processor
                .process_spawn(request_id.clone(), params)
                .await
                .map(|()| None),
            ClientRequest::ProcessWriteStdin { params, .. } => {
                self.process_exec_processor
                    .process_write_stdin(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessKill { params, .. } => {
                self.process_exec_processor
                    .process_kill(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessResizePty { params, .. } => {
                self.process_exec_processor
                    .process_resize_pty(request_id.clone(), params)
                    .await
            }
            ClientRequest::FeedbackUpload { params, .. } => {
                self.feedback_processor.feedback_upload(params).await
            }
        };

        // 统一收口：Some → 立即回响应；None → 不在此回（Processor 已自行异步回写，
        // 如长流式/审批类）；Err → 回错误。外层返回 `Ok(())` 仅表示「分发动作完成」，
        // 不代表业务成功——业务成败已通过上面的 response/error 直接告知客户端。
        match result {
            Ok(Some(response)) => {
                self.outgoing
                    .send_response_as(request_id.clone(), response)
                    .await;
            }
            Ok(None) => {}
            Err(error) => {
                self.outgoing.send_error(request_id.clone(), error).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "message_processor_tracing_tests.rs"]
mod message_processor_tracing_tests;
