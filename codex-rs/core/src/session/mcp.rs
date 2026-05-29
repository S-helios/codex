//! 【文件职责】`Session` 层的 MCP 原语集合，是 `Session` 与底层
//! `McpConnectionManager` 之间的一层薄代理 + 两类有状态逻辑：
//!   1. 引导（elicitation）：MCP server 在工具调用中途反向问用户（确认/填表/
//!      去 URL 授权）。`request_mcp_server_elicitation` 把请求存进 turn_state
//!      的 pending 表、发事件、等回应；`resolve_elicitation` 派发回应。
//!   2. 资源/工具调用代理：`list_resources` / `read_resource` / `call_tool`
//!      等转发给连接管理器（统一通过 session 持有的读锁串行化）。
//!   3. 热刷新：`refresh_mcp_servers_inner` 在不重启会话的前提下重建连接池
//!      （新装插件 / 完成 OAuth 后用）。
//!
//! 另含 Guardian 引导复核器 `GuardianMcpElicitationReviewer`：把符合特定元数据
//! 约定的 MCP 引导请求转成 Guardian 审批，由 Guardian 模型自动放行/拒绝。
//!
//! 【架构位置】
//!   层级：Agent 核心层（Session 内的 MCP 客户端方向原语）
//!   上游：`mcp_tool_call.rs`（调 `call_tool` / `request_mcp_server_elicitation`）、
//!         协议 Op（`Op::RefreshMcpServers` → `refresh_mcp_servers_*`）
//!   下游：`codex-mcp` 的 `McpConnectionManager`、`guardian` 模块
//!
//! 【阅读建议】先看 `request_mcp_server_elicitation` 与 `resolve_elicitation`
//!   这对「发问/收答」，再看 `refresh_mcp_servers_inner` 的连接池替换；
//!   底部 `review_guardian_mcp_elicitation` 及一串 `mcp_elicitation_*` 辅助
//!   函数是 Guardian 复核路径，可作为整体一次性理解。
//!
//! 注：多处 `#[expect(clippy::await_holding_invalid_type)]` 是有意为之——
//!   这些操作必须在持有 `mcp_connection_manager` / active_turn 锁期间串行完成，
//!   reason 字段已说明原因，勿照搬 clippy 建议改写。

use super::*;
use codex_mcp::ElicitationReviewRequest;
use codex_mcp::ElicitationReviewer;
use codex_mcp::ElicitationReviewerHandle;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY as MCP_ELICITATION_APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL as MCP_ELICITATION_APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_TOOL_SUGGESTION as MCP_ELICITATION_APPROVAL_KIND_TOOL_SUGGESTION;
use codex_protocol::mcp_approval_meta::APPROVALS_REVIEWER_KEY as MCP_ELICITATION_APPROVALS_REVIEWER_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_DESCRIPTION_KEY as MCP_ELICITATION_CONNECTOR_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_ID_KEY as MCP_ELICITATION_CONNECTOR_ID_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_NAME_KEY as MCP_ELICITATION_CONNECTOR_NAME_KEY;
use codex_protocol::mcp_approval_meta::REQUEST_TYPE_APPROVAL_REQUEST as MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST;
use codex_protocol::mcp_approval_meta::REQUEST_TYPE_KEY as MCP_ELICITATION_REQUEST_TYPE_KEY;
use codex_protocol::mcp_approval_meta::TOOL_DESCRIPTION_KEY as MCP_ELICITATION_TOOL_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY as MCP_ELICITATION_TOOL_NAME_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY as MCP_ELICITATION_TOOL_PARAMS_KEY;
use codex_protocol::mcp_approval_meta::TOOL_TITLE_KEY as MCP_ELICITATION_TOOL_TITLE_KEY;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::ElicitationAction;
use rmcp::model::Meta;
use serde_json::Map;

const MCP_ELICITATION_DECLINE_MESSAGE_KEY: &str = "message";
const TOOL_SUGGESTION_ACTION_INSTALL: &str = "install";
const TOOL_SUGGESTION_ACTION_KEY: &str = "suggest_type";
const TOOL_SUGGESTION_TOOL_ID_KEY: &str = "tool_id";
const TOOL_SUGGESTION_TOOL_TYPE_KEY: &str = "tool_type";

/// 对一条 MCP 引导请求做 Guardian 复核分诊的三种结论：
/// - `NotRequested`：这条引导不需要 Guardian 介入（让正常流程处理）。
/// - `Decline(reason)`：元数据不符合复核约定，复核前直接拒绝（带静态原因）。
/// - `ApprovalRequest`：可转成一次 Guardian 审批请求。
#[derive(Debug, PartialEq)]
enum GuardianElicitationReview {
    NotRequested,
    Decline(&'static str),
    ApprovalRequest(Box<crate::guardian::GuardianApprovalRequest>),
}

/// 注册到连接管理器的 Guardian 引导复核器。持 `Weak<Session>` 而非强引用，
/// 避免和 `Session` 形成循环引用导致泄漏；复核时再 `upgrade`，会话已销毁则放弃。
struct GuardianMcpElicitationReviewer {
    session: std::sync::Weak<Session>,
}

/// 一次引导请求的结果。`sent` 区分「真的发给了用户」还是「被自动应答短路」
/// （如 `elicitations_auto_deny`）——调用方据此决定是否记录 telemetry。
pub(crate) struct McpServerElicitationOutcome {
    pub(crate) response: Option<ElicitationResponse>,
    pub(crate) sent: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PluginInstallElicitationTelemetryMetadata {
    tool_type: String,
    tool_id: String,
    tool_name: String,
}

impl GuardianMcpElicitationReviewer {
    fn new(session: &Arc<Session>) -> Self {
        Self {
            session: Arc::downgrade(session),
        }
    }
}

impl ElicitationReviewer for GuardianMcpElicitationReviewer {
    fn review(
        &self,
        request: ElicitationReviewRequest,
    ) -> BoxFuture<'static, anyhow::Result<Option<ElicitationResponse>>> {
        let session = self.session.clone();
        Box::pin(async move {
            let Some(session) = session.upgrade() else {
                return Ok(None);
            };
            review_guardian_mcp_elicitation(session, request).await
        })
    }
}

impl Session {
    /// 构造一个绑定本会话的 Guardian 引导复核器句柄，注册进连接管理器。
    pub(crate) fn mcp_elicitation_reviewer(self: &Arc<Self>) -> ElicitationReviewerHandle {
        Arc::new(GuardianMcpElicitationReviewer::new(self))
    }

    /// 向用户发起一次 MCP server 引导请求并等待回应。
    ///
    /// 这是 elicitation「发问」的 Session 侧落点：把请求登记进当前 turn 的
    /// pending 表（用 `oneshot` 通道接回应），发出 `ElicitationRequestEvent`，
    /// 然后阻塞 await 通道直到 `resolve_elicitation` 派发回应（或通道被丢弃）。
    ///
    /// @returns - `response` 为用户回应（通道关闭则 `None`）；`sent` 标记是否
    ///            真的发给了用户。
    ///
    /// 副作用：修改 turn_state 的 pending_elicitations；发送会话事件；记录
    ///   插件安装 telemetry。
    /// 短路：管理器处于 `elicitations_auto_deny` 时不发问，直接返回一个
    ///   Accept 自动应答且 `sent = false`。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_mcp_server_elicitation(
        &self,
        turn_context: &TurnContext,
        request_id: RequestId,
        params: McpServerElicitationRequestParams,
    ) -> McpServerElicitationOutcome {
        // 自动拒绝模式：不打扰用户，直接给一个空 Accept 应答（sent=false）。
        if self
            .services
            .mcp_connection_manager
            .read()
            .await
            .elicitations_auto_deny()
        {
            return McpServerElicitationOutcome {
                response: Some(ElicitationResponse {
                    action: codex_rmcp_client::ElicitationAction::Accept,
                    content: Some(serde_json::json!({})),
                    meta: None,
                }),
                sent: false,
            };
        }

        // Step 1：把 app-server 协议的引导请求转成内部协议的 `ElicitationRequest`。
        // Form 类型的 `requested_schema` 需序列化为 JSON；序列化失败则直接放弃
        // 此次引导（返回 None / sent=false），不向上抛错。
        let server_name = params.server_name.clone();
        let request = match params.request {
            McpServerElicitationRequest::Form {
                meta,
                message,
                requested_schema,
            } => {
                let requested_schema = match serde_json::to_value(requested_schema) {
                    Ok(requested_schema) => requested_schema,
                    Err(err) => {
                        warn!(
                            "failed to serialize MCP elicitation schema for server_name: {server_name}, request_id: {request_id}: {err:#}"
                        );
                        return McpServerElicitationOutcome {
                            response: None,
                            sent: false,
                        };
                    }
                };
                codex_protocol::approvals::ElicitationRequest::Form {
                    meta,
                    message,
                    requested_schema,
                }
            }
            McpServerElicitationRequest::Url {
                meta,
                message,
                url,
                elicitation_id,
            } => codex_protocol::approvals::ElicitationRequest::Url {
                meta,
                message,
                url,
                elicitation_id,
            },
        };

        // Step 2：建 oneshot 通道并把发送端登记进当前 turn 的 pending 表，
        // 键为 (server_name, request_id)。无 active turn 时不登记（prev=None）。
        // 同键已存在则覆盖并告警——正常不应发生。
        let (tx_response, rx_response) = oneshot::channel();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_elicitation(
                        server_name.clone(),
                        request_id.clone(),
                        tx_response,
                    )
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!(
                "Overwriting existing pending elicitation for server_name: {server_name}, request_id: {request_id}"
            );
        }
        // Step 3：把 rmcp 的 `NumberOrString` 显式映射到内部协议 `RequestId`
        // 的 String / Integer 变体。两层类型必须对齐，否则回应 `resolve` 时
        // 按键查不到通道，响应送不回去（参见 19_mcp_integration §9 Q8）。
        let id = match request_id {
            rmcp::model::NumberOrString::String(value) => {
                codex_protocol::mcp::RequestId::String(value.to_string())
            }
            rmcp::model::NumberOrString::Number(value) => {
                codex_protocol::mcp::RequestId::Integer(value)
            }
        };
        let event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
            turn_id: params.turn_id,
            server_name,
            id,
            request,
        });
        // Step 4：发事件给前端请用户输入。先标记「本 turn 期间请求过用户输入」
        // （turn 完成判定会用到），若是插件安装类引导再补一条 telemetry。
        let plugin_install_telemetry = plugin_install_elicitation_telemetry_metadata(&event);
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        if let Some(plugin_install_telemetry) = plugin_install_telemetry {
            turn_context
                .session_telemetry
                .record_plugin_install_elicitation_sent(
                    plugin_install_telemetry.tool_type.as_str(),
                    plugin_install_telemetry.tool_id.as_str(),
                    plugin_install_telemetry.tool_name.as_str(),
                );
        }
        // Step 5：阻塞等待回应。发送端被丢弃（如 turn 取消）则 `ok()` 得 None。
        McpServerElicitationOutcome {
            response: rx_response.await.ok(),
            sent: true,
        }
    }

    /// 派发一条引导回应给等待中的请求方，是 elicitation「收答」的落点。
    ///
    /// 先在当前 turn 的 pending 表里按 (server_name, id) 找发送端：命中则直接
    /// 通过 oneshot 通道发回（唤醒 `request_mcp_server_elicitation` 的 await）；
    /// 未命中（请求可能由管理器自身发起）则回退给连接管理器处理。
    ///
    /// 副作用：从 pending 表移除对应条目。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and manager fallback must stay serialized"
    )]
    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_elicitation(&server_name, &id)
                }
                None => None,
            }
        };
        if let Some(tx_response) = entry {
            tx_response
                .send(response)
                .map_err(|e| anyhow::anyhow!("failed to send elicitation response: {e:?}"))?;
            return Ok(());
        }

        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    // ── MCP 资源/工具调用代理 ────────────────────────────────────────────
    // 下面四个方法（list_resources / list_resource_templates / read_resource /
    // call_tool）都是对 `McpConnectionManager` 的薄转发：取读锁 → 调同名方法。
    // 统一经 session 持有的管理器读锁，保证这些 MCP 调用串行化（见上 #[expect]）。

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resources(server, params)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resource_templates(server, params)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP resource calls are serialized through the session-owned manager guard"
    )]
    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .read_resource(server, params)
            .await
    }

    /// 把一次工具调用路由到指定 MCP server 并返回结果。是 MCP 工具实际执行的
    /// 最底层落点（`mcp_tool_call.rs` 审批通过后经此调用），仅转发给连接管理器。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "MCP tool calls are serialized through the session-owned manager guard"
    )]
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .call_tool(server, tool, arguments, meta)
            .await
    }

    /// 热刷新 MCP 连接池：在不重启会话的前提下用新配置重建
    /// `McpConnectionManager`，替换旧管理器并关停旧的。新装 MCP 依赖、完成
    /// OAuth、或换了 codex_apps token 后调用（经 `Op::RefreshMcpServers`）。
    ///
    /// 副作用：取消并重置 startup cancellation token；替换
    ///   `self.services.mcp_connection_manager`；shutdown 旧管理器（含其连接）。
    async fn refresh_mcp_servers_inner(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        // Step 1：用最新 auth + config 重算建池所需的全部输入：生效 server 列表
        // （注入/移除 codex_apps）、工具来源快照、各 server 的 auth 状态、运行时
        // 上下文（优先用 turn 环境的 cwd）。
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        let mcp_config = config
            .to_mcp_config(self.services.plugins_manager.as_ref())
            .await;
        let tool_plugin_provenance = self
            .services
            .mcp_manager
            .tool_plugin_provenance(config.as_ref())
            .await;
        let mcp_servers =
            effective_mcp_servers_from_configured(mcp_servers, &mcp_config, auth.as_ref());
        let host_owned_codex_apps_enabled =
            host_owned_codex_apps_enabled(&mcp_config, auth.as_ref());
        let auth_statuses =
            compute_auth_statuses(mcp_servers.iter(), store_mode, auth.as_ref()).await;
        let mcp_runtime_context = match turn_context.environments.primary() {
            Some(turn_environment) => McpRuntimeContext::new(
                Arc::clone(&self.services.environment_manager),
                turn_environment.cwd.to_path_buf(),
            ),
            None => McpRuntimeContext::new(
                Arc::clone(&self.services.environment_manager),
                #[allow(deprecated)]
                turn_context.cwd.to_path_buf(),
            ),
        };
        // Step 2：取消上一次（可能仍在进行的）启动，并换一个新 token。避免旧
        // 启动任务和即将创建的新管理器并发抢占。
        {
            let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
            guard.cancel();
            *guard = CancellationToken::new();
        }
        // Step 3：用新输入并发启动新管理器（不阻塞，内部用 JoinSet 拉起每个 server）。
        let (refreshed_manager, cancel_token) = McpConnectionManager::new(
            &mcp_servers,
            store_mode,
            auth_statuses,
            &turn_context.approval_policy,
            turn_context.sub_id.clone(),
            self.get_tx_event(),
            turn_context.permission_profile(),
            mcp_runtime_context,
            config.codex_home.to_path_buf(),
            codex_apps_tools_cache_key(auth.as_ref()),
            host_owned_codex_apps_enabled,
            mcp_config.prefix_mcp_tool_names,
            mcp_config.client_elicitation_capability,
            tool_plugin_provenance,
            auth.as_ref(),
            elicitation_reviewer,
        )
        .await;
        // Step 4：把旧管理器的 `elicitations_auto_deny` 开关转移到新管理器，
        // 保证刷新不丢失「自动拒绝引导」这一运行时状态。
        {
            let current_manager = self.services.mcp_connection_manager.read().await;
            refreshed_manager.set_elicitations_auto_deny(current_manager.elicitations_auto_deny());
        }
        // Step 5：把新启动 token 存回去；若期间共享 token 已被取消（有人请求停止
        // 启动），则立刻取消新 token，避免错过取消信号。
        {
            let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
            if guard.is_cancelled() {
                cancel_token.cancel();
            }
            *guard = cancel_token;
        }

        // Step 6：原子替换连接管理器（取写锁 + mem::replace），拿到旧的后释放
        // 写锁再 shutdown——shutdown 可能耗时，不在持写锁期间做以免阻塞其它访问。
        let mut old_manager = {
            let mut manager = self.services.mcp_connection_manager.write().await;
            std::mem::replace(&mut *manager, refreshed_manager)
        };
        old_manager.shutdown().await;
    }

    /// 若存在「待处理的刷新请求」（由 `Op::RefreshMcpServers` 暂存到
    /// `pending_mcp_server_refresh_config`），则消费它并执行热刷新。
    /// 在 turn 边界等安全点调用：把刷新延后到不会打断进行中工作的时机。
    /// 配置以 JSON 暂存，解析失败仅告警跳过、不刷新。
    pub(crate) async fn refresh_mcp_servers_if_requested(
        &self,
        turn_context: &TurnContext,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        // take() 取出并清空待处理配置；无则直接返回（无 pending 刷新）。
        let refresh_config = { self.pending_mcp_server_refresh_config.lock().await.take() };
        let Some(refresh_config) = refresh_config else {
            return;
        };

        let McpServerRefreshConfig {
            mcp_servers,
            mcp_oauth_credentials_store_mode,
        } = refresh_config;

        let mcp_servers =
            match serde_json::from_value::<HashMap<String, McpServerConfig>>(mcp_servers) {
                Ok(servers) => servers,
                Err(err) => {
                    warn!("failed to parse MCP server refresh config: {err}");
                    return;
                }
            };
        let store_mode = match serde_json::from_value::<OAuthCredentialsStoreMode>(
            mcp_oauth_credentials_store_mode,
        ) {
            Ok(mode) => mode,
            Err(err) => {
                warn!("failed to parse MCP OAuth refresh config: {err}");
                return;
            }
        };

        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode, elicitation_reviewer)
            .await;
    }

    /// 立即用给定配置热刷新（绕过 pending 暂存，直接走 inner）。供已持有 server
    /// 配置、需同步刷新的调用方使用。
    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
    ) {
        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode, elicitation_reviewer)
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn mcp_startup_cancellation_token(&self) -> CancellationToken {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .clone()
    }

    pub(crate) async fn cancel_mcp_startup(&self) {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .cancel();
    }
}

/// Guardian 复核一条 MCP 引导请求的主流程（被 `GuardianMcpElicitationReviewer`
/// 调用）。返回 `Ok(None)` 表示「不复核、交回正常引导流程」；返回
/// `Ok(Some(resp))` 表示 Guardian 已给出自动应答（接受/拒绝）。
///
/// 仅当存在 active turn、且该 turn 把审批路由给 Guardian 时才介入；随后按元数据
/// 分诊（见 `guardian_elicitation_review_request`），可复核的转成 Guardian 审批，
/// 把 Guardian 决策翻译回引导应答。
async fn review_guardian_mcp_elicitation(
    session: Arc<Session>,
    request: ElicitationReviewRequest,
) -> anyhow::Result<Option<ElicitationResponse>> {
    // 无进行中的 turn → 不复核。
    let Some((turn_context, _cancellation_token)) =
        session.active_turn_context_and_cancellation_token().await
    else {
        return Ok(None);
    };

    // 当前 turn 未把审批路由给 Guardian → 不复核。
    if !crate::guardian::routes_approval_to_guardian(turn_context.as_ref()) {
        return Ok(None);
    }

    let guardian_request = match guardian_elicitation_review_request(&request) {
        GuardianElicitationReview::NotRequested => return Ok(None),
        GuardianElicitationReview::Decline(reason) => {
            warn!(
                server_name = %request.server_name,
                request_id = %mcp_elicitation_request_id(&request.request_id),
                reason,
                "declining Guardian MCP elicitation before review"
            );
            return Ok(Some(mcp_elicitation_decline_without_message()));
        }
        GuardianElicitationReview::ApprovalRequest(guardian_request) => *guardian_request,
    };

    let review_id = crate::guardian::new_guardian_review_id();
    let decision = crate::guardian::review_approval_request(
        &session,
        &turn_context,
        review_id.clone(),
        guardian_request,
        /*retry_reason*/ None,
    )
    .await;
    Ok(Some(
        mcp_elicitation_response_from_guardian_decision(session.as_ref(), &review_id, decision)
            .await,
    ))
}

/// 把一条引导请求分诊为三种 Guardian 复核结论（见 `GuardianElicitationReview`）。
/// 这里是一串严格的元数据校验：只接受「Form 类型 + 空 schema + 声明为
/// mcp_tool_call 审批类型 + 带非空 tool_name」的引导转成 Guardian 审批；任何
/// 偏离约定的（如 URL 引导声明了审批、schema 非空、缺 tool_name）要么不复核、
/// 要么直接拒绝，避免把不受控的请求当成可自动审批的工具调用。
fn guardian_elicitation_review_request(
    request: &ElicitationReviewRequest,
) -> GuardianElicitationReview {
    let (meta, requested_schema) = match &request.elicitation {
        CreateElicitationRequestParams::FormElicitationParams {
            meta,
            requested_schema,
            ..
        } => (meta, Some(requested_schema)),
        CreateElicitationRequestParams::UrlElicitationParams { meta, .. } => {
            return if meta_requests_approval_request(meta) {
                GuardianElicitationReview::Decline(
                    "guardian MCP elicitation review only supports form elicitations",
                )
            } else {
                GuardianElicitationReview::NotRequested
            };
        }
    };

    let Some(meta) = meta.as_ref().map(|meta| &meta.0) else {
        return GuardianElicitationReview::NotRequested;
    };
    if metadata_str(meta, MCP_ELICITATION_REQUEST_TYPE_KEY)
        != Some(MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST)
    {
        return GuardianElicitationReview::NotRequested;
    }
    if metadata_str(meta, MCP_ELICITATION_APPROVAL_KIND_KEY)
        != Some(MCP_ELICITATION_APPROVAL_KIND_MCP_TOOL_CALL)
    {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation metadata must declare mcp_tool_call approval kind",
        );
    }
    if requested_schema.is_some_and(|schema| !schema.properties.is_empty()) {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation review only supports empty form schemas",
        );
    }

    let Some(tool_name) = metadata_owned_string(meta, MCP_ELICITATION_TOOL_NAME_KEY) else {
        return GuardianElicitationReview::Decline(
            "guardian MCP elicitation metadata must include a non-empty tool_name",
        );
    };
    let arguments = match meta.get(MCP_ELICITATION_TOOL_PARAMS_KEY) {
        Some(value @ Value::Object(_)) => Some(value.clone()),
        Some(_) => {
            return GuardianElicitationReview::Decline(
                "guardian MCP elicitation tool_params must be an object",
            );
        }
        None => Some(Value::Object(Map::new())),
    };

    GuardianElicitationReview::ApprovalRequest(Box::new(
        crate::guardian::GuardianApprovalRequest::McpToolCall {
            id: format!(
                "mcp_elicitation:{}:{}",
                request.server_name,
                mcp_elicitation_request_id(&request.request_id)
            ),
            server: request.server_name.clone(),
            tool_name,
            arguments,
            connector_id: metadata_owned_string(meta, MCP_ELICITATION_CONNECTOR_ID_KEY),
            connector_name: metadata_owned_string(meta, MCP_ELICITATION_CONNECTOR_NAME_KEY),
            connector_description: metadata_owned_string(
                meta,
                MCP_ELICITATION_CONNECTOR_DESCRIPTION_KEY,
            ),
            tool_title: metadata_owned_string(meta, MCP_ELICITATION_TOOL_TITLE_KEY),
            tool_description: metadata_owned_string(meta, MCP_ELICITATION_TOOL_DESCRIPTION_KEY),
            annotations: None,
        },
    ))
}

fn meta_requests_approval_request(meta: &Option<Meta>) -> bool {
    meta.as_ref()
        .and_then(|meta| metadata_str(&meta.0, MCP_ELICITATION_REQUEST_TYPE_KEY))
        == Some(MCP_ELICITATION_REQUEST_TYPE_APPROVAL_REQUEST)
}

fn metadata_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

fn metadata_owned_string(meta: &Map<String, Value>, key: &str) -> Option<String> {
    metadata_str(meta, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn plugin_install_elicitation_telemetry_metadata(
    event: &EventMsg,
) -> Option<PluginInstallElicitationTelemetryMetadata> {
    let EventMsg::ElicitationRequest(ElicitationRequestEvent { request, .. }) = event else {
        return None;
    };
    let codex_protocol::approvals::ElicitationRequest::Form {
        meta: Some(Value::Object(meta)),
        ..
    } = request
    else {
        return None;
    };
    if metadata_str(meta, MCP_ELICITATION_APPROVAL_KIND_KEY)
        != Some(MCP_ELICITATION_APPROVAL_KIND_TOOL_SUGGESTION)
        || metadata_str(meta, TOOL_SUGGESTION_ACTION_KEY) != Some(TOOL_SUGGESTION_ACTION_INSTALL)
    {
        return None;
    }

    Some(PluginInstallElicitationTelemetryMetadata {
        tool_type: metadata_owned_string(meta, TOOL_SUGGESTION_TOOL_TYPE_KEY)?,
        tool_id: metadata_owned_string(meta, TOOL_SUGGESTION_TOOL_ID_KEY)?,
        tool_name: metadata_owned_string(meta, MCP_ELICITATION_TOOL_NAME_KEY)?,
    })
}

fn mcp_elicitation_request_id(id: &RequestId) -> String {
    match id {
        rmcp::model::NumberOrString::String(value) => value.to_string(),
        rmcp::model::NumberOrString::Number(value) => value.to_string(),
    }
}

async fn mcp_elicitation_response_from_guardian_decision(
    session: &Session,
    review_id: &str,
    decision: ReviewDecision,
) -> ElicitationResponse {
    let denial_message = match decision {
        ReviewDecision::Denied => {
            Some(crate::guardian::guardian_rejection_message(session, review_id).await)
        }
        _ => None,
    };
    mcp_elicitation_response_from_guardian_decision_parts(decision, denial_message)
}

// 把 Guardian 的 `ReviewDecision` 翻译成 MCP 引导应答：各类「批准」→ Accept；
// Denied/TimedOut → 带原因的 Decline；Abort → Cancel。所有应答都标注由
// AutoReview 产生，便于下游区分自动复核与真人作答。
fn mcp_elicitation_response_from_guardian_decision_parts(
    decision: ReviewDecision,
    denial_message: Option<String>,
) -> ElicitationResponse {
    match decision {
        ReviewDecision::Approved
        | ReviewDecision::ApprovedForSession
        | ReviewDecision::ApprovedExecpolicyAmendment { .. }
        | ReviewDecision::NetworkPolicyAmendment { .. } => ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({})),
            meta: Some(mcp_elicitation_auto_meta()),
        },
        ReviewDecision::Denied => mcp_elicitation_decline_with_message(
            denial_message.unwrap_or_else(|| "Guardian denied this request.".to_string()),
        ),
        ReviewDecision::TimedOut => {
            mcp_elicitation_decline_with_message(crate::guardian::guardian_timeout_message())
        }
        ReviewDecision::Abort => ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
            meta: Some(mcp_elicitation_auto_meta()),
        },
    }
}

fn mcp_elicitation_decline_with_message(message: String) -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: Some(serde_json::json!({
            MCP_ELICITATION_DECLINE_MESSAGE_KEY: message,
            MCP_ELICITATION_APPROVALS_REVIEWER_KEY: ApprovalsReviewer::AutoReview,
        })),
    }
}

fn mcp_elicitation_decline_without_message() -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: Some(mcp_elicitation_auto_meta()),
    }
}

fn mcp_elicitation_auto_meta() -> serde_json::Value {
    serde_json::json!({
        MCP_ELICITATION_APPROVALS_REVIEWER_KEY: ApprovalsReviewer::AutoReview,
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
