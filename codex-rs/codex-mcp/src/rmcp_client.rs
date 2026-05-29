//! RMCP client lifecycle for MCP server connections.
//!
//! This module owns startup of individual RMCP clients: building the transport,
//! initializing the server, listing raw tools, applying per-server tool filters,
//! and exposing cached startup snapshots while a client is still connecting.
//! Higher-level aggregation and resource/tool APIs live in
//! [`crate::connection_manager`].
//!
//! 【文件职责】管理「单个」RMCP 客户端的生命周期：构建传输层（stdio 子进程 /
//!   StreamableHTTP）、initialize 握手、列举原始工具、套用 per-server 工具过滤器，
//!   并在尚未连接完成时对外暴露启动缓存快照。
//!
//! 【架构位置】
//!   层级：工具执行层（MCP 子系统的「单连接」层，位于连接池之下）
//!   上游：`connection_manager`（聚合多个客户端、做跨 server 的列举与路由）
//!   下游：`codex_rmcp_client::RmcpClient`（真正的 MCP 协议客户端）、
//!         `tools`（过滤、schema 整形）、`codex_apps`（apps 工具的缓存读写）
//!
//! 【数据流】
//!   配置 → `make_rmcp_client()` 建传输 → `start_server_task()` 握手+列举工具
//!   → 封装为 `ManagedClient`；这一整段被包成 `Shared` future 存进
//!   `AsyncManagedClient`，多处可并发 await 而只执行一次。
//!
//! 【阅读建议】先看 `AsyncManagedClient`（如何把启动包成共享 future + 缓存快照），
//!   再看 `start_server_task()`（握手与工具列举的主流程），
//!   最后看 `make_rmcp_client()`（两种传输的分支构造）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::codex_apps::CachedCodexAppsToolsLoad;
use crate::codex_apps::CodexAppsToolsCacheContext;
use crate::codex_apps::filter_disallowed_codex_apps_tools;
use crate::codex_apps::load_cached_codex_apps_tools;
use crate::codex_apps::load_startup_cached_codex_apps_server_info;
use crate::codex_apps::load_startup_cached_codex_apps_tools_snapshot;
use crate::codex_apps::normalize_codex_apps_callable_name;
use crate::codex_apps::normalize_codex_apps_callable_namespace;
use crate::codex_apps::normalize_codex_apps_tool_title;
use crate::codex_apps::write_cached_codex_apps_tools_if_needed;
use crate::elicitation::ElicitationRequestManager;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::mcp::ToolPluginProvenance;
use crate::runtime::McpRuntimeContext;
use crate::runtime::emit_duration;
use crate::server::EffectiveMcpServer;
use crate::server::McpServerLaunch;
use crate::tools::ToolFilter;
use crate::tools::ToolInfo;
use crate::tools::filter_tools;
use crate::tools::tool_with_model_visible_input_schema;
use anyhow::Result;
use anyhow::anyhow;
use async_channel::Sender;
use codex_api::SharedAuthProvider;
use codex_async_utils::CancelErr;
use codex_async_utils::OrCancelExt;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::ReqwestHttpClient;
use codex_protocol::mcp::McpServerInfo;
use codex_protocol::protocol::Event;
use codex_rmcp_client::ExecutorStdioServerLauncher;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::StdioServerLauncher;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use rmcp::model::ClientCapabilities;
use rmcp::model::ElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use rmcp::model::Tool as RmcpTool;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// MCP server capability indicating that Codex should include [`SandboxState`]
/// in tool-call request `_meta` under this key.
pub const MCP_SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";

pub(crate) const MCP_TOOLS_LIST_DURATION_METRIC: &str = "codex.mcp.tools.list.duration_ms";
pub(crate) const MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC: &str =
    "codex.mcp.tools.fetch_uncached.duration_ms";
// server 未配置 `startup_timeout_sec` 时的默认握手超时（30s）。
pub(crate) const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
// server 未配置 `tool_timeout_sec` 时单次工具调用的默认超时（120s）：
// 取值较宽以容纳耗时较长的工具，避免误杀正常的慢操作。
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

// 不受信任的连接器（connector）元数据键名清单。
// 仅 `codex_apps` 这一可信 server 的工具允许携带这些键；其余 server 的工具
// 在 `sanitize_tool_connector_metadata()` 里会把它们剥离，防止第三方 server
// 冒充 connector 身份（见下方该函数）。
const UNTRUSTED_CONNECTOR_META_KEYS: &[&str] = &[
    "connector_id",
    "connector_name",
    "connector_display_name",
    "connector_description",
    "connectorDescription",
];

/// 一个「已就绪」的 MCP 客户端及其启动期采集到的全部信息。
/// 由 `start_server_task()` 在握手成功后构造，是 `AsyncManagedClient` 内部
/// 共享 future 的 Ok 结果。
#[derive(Clone)]
pub(crate) struct ManagedClient {
    pub(crate) client: Arc<RmcpClient>,
    pub(crate) server_info: McpServerInfo,
    pub(crate) tools: Vec<ToolInfo>,
    pub(crate) tool_filter: ToolFilter,
    pub(crate) tool_timeout: Option<Duration>,
    pub(crate) server_instructions: Option<String>,
    // server 是否声明了 `codex/sandbox-state-meta` 能力——若是，调用工具时
    // Codex 会在请求 `_meta` 里附带沙箱状态。
    pub(crate) server_supports_sandbox_state_meta_capability: bool,
    // 仅 `codex_apps` server 非 None：用于把列举到的工具落盘缓存，加速下次启动。
    pub(crate) codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
}

impl ManagedClient {
    // 返回本客户端的工具列表。对 `codex_apps` server 优先读磁盘缓存（命中则直接
    // 返回过滤后的缓存工具），缓存未命中或非 apps server 则回退到启动期采集的
    // `self.tools`。两条路径都打 `cache=hit/miss` 埋点用于观测缓存有效性。
    fn listed_tools(&self) -> Vec<ToolInfo> {
        let total_start = Instant::now();
        if let Some(cache_context) = self.codex_apps_tools_cache_context.as_ref()
            && let CachedCodexAppsToolsLoad::Hit(tools) =
                load_cached_codex_apps_tools(cache_context)
        {
            emit_duration(
                MCP_TOOLS_LIST_DURATION_METRIC,
                total_start.elapsed(),
                &[("cache", "hit")],
            );
            return filter_tools(tools, &self.tool_filter);
        }

        if self.codex_apps_tools_cache_context.is_some() {
            emit_duration(
                MCP_TOOLS_LIST_DURATION_METRIC,
                total_start.elapsed(),
                &[("cache", "miss")],
            );
        }

        self.tools.clone()
    }
}

/// 一个「正在/已经」启动的 MCP 客户端句柄。
///
/// 核心设计：把整段启动流程（建传输 + 握手 + 列举工具）包成一个 `Shared` future。
/// 它可被 `clone` 到多处、被并发 await，但底层只执行一次、结果对所有 await 方共享。
/// 这样「管理器持有一份用于路由」与「后台任务持有一份用于上报状态」就能共用同一次启动。
/// 在启动完成前，外部可读 `cached_*` 快照先行展示，避免阻塞 UI。
#[derive(Clone)]
pub(crate) struct AsyncManagedClient {
    // 共享的启动 future：await 它即「等待该 server 就绪」，成功得到 `ManagedClient`。
    pub(crate) client: Shared<BoxFuture<'static, Result<ManagedClient, StartupOutcomeError>>>,
    // 启动期可用的工具快照（来自磁盘缓存）：仅在启动尚未完成时对外提供。
    pub(crate) cached_tool_info_snapshot: Option<Vec<ToolInfo>>,
    // 启动期可用的 server 展示信息快照（同上，供 UI 抢先展示）。
    pub(crate) cached_server_info: Option<McpServerInfo>,
    // 启动是否已结束（无论成败）。供「是否还能用缓存快照」等判断使用。
    pub(crate) startup_complete: Arc<AtomicBool>,
    pub(crate) tool_plugin_provenance: Arc<ToolPluginProvenance>,
    // 取消令牌：`shutdown()` 时 cancel 以中止仍在进行的启动。
    pub(crate) cancel_token: CancellationToken,
}

impl AsyncManagedClient {
    // Keep this constructor flat so the startup inputs remain readable at the
    // single call site instead of introducing a one-off params wrapper.
    // 构造时「不」立即启动 server：仅组装出启动 future 并 `shared()` 化，
    // 真正的握手要等到首个 await（通常发生在 `connection_manager::new` 的
    // 启动任务里）才执行。参数刻意保持扁平，便于唯一调用点直观对照。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        server_name: String,
        server: EffectiveMcpServer,
        store_mode: OAuthCredentialsStoreMode,
        cancel_token: CancellationToken,
        tx_event: Sender<Event>,
        elicitation_requests: ElicitationRequestManager,
        codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
        tool_plugin_provenance: Arc<ToolPluginProvenance>,
        runtime_context: McpRuntimeContext,
        runtime_auth_provider: Option<SharedAuthProvider>,
        client_elicitation_capability: ElicitationCapability,
    ) -> Self {
        let tool_filter = server
            .configured_config()
            .map(ToolFilter::from_config)
            .unwrap_or_default();
        let cached_tool_info_snapshot = load_startup_cached_codex_apps_tools_snapshot(
            &server_name,
            codex_apps_tools_cache_context.as_ref(),
        );
        let cached_tool_info_snapshot =
            cached_tool_info_snapshot.map(|tools| filter_tools(tools, &tool_filter));
        let cached_server_info = load_startup_cached_codex_apps_server_info(
            &server_name,
            codex_apps_tools_cache_context.as_ref(),
        );
        let startup_tool_filter = tool_filter;
        let startup_complete = Arc::new(AtomicBool::new(false));
        let startup_complete_for_fut = Arc::clone(&startup_complete);
        let cancel_token_for_fut = cancel_token.clone();
        // 启动 future 主体：内层 async 块做「校验名→建客户端→start_server_task」，
        // 外层用 `.or_cancel()` 包裹，使取消令牌能在任意 await 点中断启动并归类为
        // Cancelled；无论成败，最后都置位 `startup_complete`。
        let fut = async move {
            let outcome = match async {
                if let Err(error) = validate_mcp_server_name(&server_name) {
                    return Err(error.into());
                }

                let client = Arc::new(
                    make_rmcp_client(
                        &server_name,
                        server.clone(),
                        store_mode,
                        runtime_context,
                        runtime_auth_provider,
                    )
                    .await?,
                );
                start_server_task(
                    server_name,
                    client,
                    StartServerTaskParams {
                        startup_timeout: server
                            .configured_config()
                            .and_then(|config| config.startup_timeout_sec)
                            .or(Some(DEFAULT_STARTUP_TIMEOUT)),
                        tool_timeout: server
                            .configured_config()
                            .and_then(|config| config.tool_timeout_sec)
                            .unwrap_or(DEFAULT_TOOL_TIMEOUT),
                        tool_filter: startup_tool_filter,
                        tx_event,
                        elicitation_requests,
                        codex_apps_tools_cache_context,
                        client_elicitation_capability,
                    },
                )
                .await
            }
            .or_cancel(&cancel_token_for_fut)
            .await
            {
                Ok(result) => result,
                Err(CancelErr::Cancelled) => Err(StartupOutcomeError::Cancelled),
            };

            startup_complete_for_fut.store(true, Ordering::Release);
            outcome
        };
        let client = fut.boxed().shared();
        // 仅当存在缓存快照时，才额外 spawn 一个任务主动驱动启动 future。
        // 原因：有缓存意味着上层可能一直只读缓存、从不 await 真实 future，
        // 那样 server 永远不会真正连接；这里替它「预热」，让真实连接在后台完成，
        // 后续缓存失效时即可无缝切换到实时数据。无缓存时则交由首个真实 await 驱动。
        if cached_tool_info_snapshot.is_some() {
            let startup_task = client.clone();
            tokio::spawn(async move {
                let _ = startup_task.await;
            });
        }

        Self {
            client,
            cached_tool_info_snapshot,
            cached_server_info,
            startup_complete,
            tool_plugin_provenance,
            cancel_token,
        }
    }

    // await 共享启动 future，拿到就绪的 `ManagedClient`（或启动错误）。
    // 多次调用是幂等的——底层 future 只执行一次，后续调用直接复用其结果。
    pub(crate) async fn client(&self) -> Result<ManagedClient, StartupOutcomeError> {
        self.client.clone().await
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel_token.cancel();
        match self.client().await {
            Ok(client) => client.client.shutdown().await,
            Err(StartupOutcomeError::Cancelled) => {}
            Err(error) => {
                warn!("failed to initialize MCP client during shutdown: {error:#}");
            }
        }
    }

    // 仅在「启动尚未完成」时返回缓存工具快照；一旦启动结束就返回 None，
    // 迫使调用方改用实时数据（见 `listed_tools` 的取舍）。
    fn cached_tool_info_snapshot_while_initializing(&self) -> Option<Vec<ToolInfo>> {
        if !self.startup_complete.load(Ordering::Acquire) {
            return self.cached_tool_info_snapshot.clone();
        }
        None
    }

    /// 返回本 server 的工具列表，并为其补充「插件来源」标注。
    ///
    /// 数据来源选择：启动未完成 → 用缓存快照（不阻塞）；启动已完成 → await 真实
    /// 客户端取实时工具，失败再退回缓存。取到后统一经 `annotate_tools` 处理：
    ///   - 对 `codex_apps` 工具做模型可见 schema 整形；
    ///   - 把工具所属插件的展示名追加进描述末尾（便于用户识别工具出处）。
    /// 缓存内容刻意保持「裸」——插件来源是 per-session、在读取时才解析的。
    pub(crate) async fn listed_tools(&self) -> Option<Vec<ToolInfo>> {
        let annotate_tools = |tools: Vec<ToolInfo>| {
            let mut tools = tools;
            for tool in &mut tools {
                if tool.server_name == CODEX_APPS_MCP_SERVER_NAME {
                    tool.tool = tool_with_model_visible_input_schema(&tool.tool);
                }

                let plugin_names = match tool.connector_id.as_deref() {
                    Some(connector_id) => self
                        .tool_plugin_provenance
                        .plugin_display_names_for_connector_id(connector_id),
                    None => self
                        .tool_plugin_provenance
                        .plugin_display_names_for_mcp_server_name(tool.server_name.as_str()),
                };
                tool.plugin_display_names = plugin_names.to_vec();

                if plugin_names.is_empty() {
                    continue;
                }

                let plugin_source_note = if plugin_names.len() == 1 {
                    format!("This tool is part of plugin `{}`.", plugin_names[0])
                } else {
                    format!(
                        "This tool is part of plugins {}.",
                        plugin_names
                            .iter()
                            .map(|plugin_name| format!("`{plugin_name}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let description = tool
                    .tool
                    .description
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("");
                let annotated_description = if description.is_empty() {
                    plugin_source_note
                } else if matches!(description.chars().last(), Some('.' | '!' | '?')) {
                    format!("{description} {plugin_source_note}")
                } else {
                    format!("{description}. {plugin_source_note}")
                };
                tool.tool.description = Some(Cow::Owned(annotated_description));
            }
            tools
        };

        // Keep cache payloads raw; plugin provenance is resolved per-session at read time.
        let tools = if let Some(startup_tools) = self.cached_tool_info_snapshot_while_initializing()
        {
            Some(startup_tools)
        } else {
            match self.client().await {
                Ok(client) => Some(client.listed_tools()),
                Err(_) => self.cached_tool_info_snapshot.clone(),
            }
        };
        tools.map(annotate_tools)
    }
}

/// MCP server 启动的失败结果。
/// 需要 `Clone` 是因为它要随 `Shared` future 分发给多个 await 方。
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum StartupOutcomeError {
    #[error("MCP startup cancelled")]
    Cancelled,
    // We can't store the original error here because anyhow::Error doesn't implement
    // `Clone`.
    // 此处只存错误字符串而非原始 `anyhow::Error`：因为该枚举要求 `Clone`，
    // 而 `anyhow::Error` 不实现 `Clone`，故在转换时即扁平化为字符串。
    #[error("MCP startup failed: {error}")]
    Failed { error: String },
}

impl From<anyhow::Error> for StartupOutcomeError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed {
            error: error.to_string(),
        }
    }
}

/// 直接向 server 发 `tools/list`（绕过任何缓存），把每个原始 MCP 工具映射为
/// 本 crate 的 `ToolInfo`。
///
/// 处理要点：
///   - 对非 `codex_apps` server，剥离不可信的 connector 元数据（防身份冒充）；
///   - 推导 `callable_name` / `callable_namespace`（模型可见名的初步规范化）；
///   - 无 connector 元数据时，用 server 自身的 `instructions` 作为命名空间描述；
///   - `codex_apps` server 额外过滤掉不允许的工具。
pub(crate) async fn list_tools_for_client_uncached(
    server_name: &str,
    client: &Arc<RmcpClient>,
    timeout: Option<Duration>,
    server_instructions: Option<&str>,
) -> Result<Vec<ToolInfo>> {
    let resp = client
        .list_tools_with_connector_ids(/*params*/ None, timeout)
        .await?;
    let tools = resp
        .tools
        .into_iter()
        .map(|tool| {
            let mut tool_def = tool.tool;
            let (connector_id, connector_name, connector_description) =
                sanitize_tool_connector_metadata(
                    server_name,
                    &mut tool_def,
                    tool.connector_id,
                    tool.connector_name,
                    tool.connector_description,
                );
            let callable_name = normalize_codex_apps_callable_name(
                server_name,
                &tool_def.name,
                connector_id.as_deref(),
                connector_name.as_deref(),
            );
            let callable_namespace =
                normalize_codex_apps_callable_namespace(server_name, connector_name.as_deref());
            if let Some(title) = tool_def.title.as_deref() {
                let normalized_title =
                    normalize_codex_apps_tool_title(server_name, connector_name.as_deref(), title);
                if tool_def.title.as_deref() != Some(normalized_title.as_str()) {
                    tool_def.title = Some(normalized_title);
                }
            }
            let has_connector_metadata = connector_id.is_some()
                || connector_name.is_some()
                || connector_description.is_some();
            let namespace_description = if has_connector_metadata {
                connector_description
            } else {
                server_instructions.map(str::to_string)
            };
            ToolInfo {
                server_name: server_name.to_owned(),
                supports_parallel_tool_calls: false,
                server_origin: None,
                callable_name,
                callable_namespace,
                namespace_description,
                tool: tool_def,
                connector_id,
                connector_name,
                plugin_display_names: Vec::new(),
            }
        })
        .collect();
    if server_name == CODEX_APPS_MCP_SERVER_NAME {
        return Ok(filter_disallowed_codex_apps_tools(tools));
    }
    Ok(tools)
}

// 信任边界：只有可信的 `codex_apps` server 才允许携带/保留 connector 身份元数据。
// 对其他（第三方）server，剥离其工具上的 connector 元数据并把三个 connector 字段
// 一律返回 None，防止恶意 server 伪装成某个官方连接器。
fn sanitize_tool_connector_metadata(
    server_name: &str,
    tool: &mut RmcpTool,
    connector_id: Option<String>,
    connector_name: Option<String>,
    connector_description: Option<String>,
) -> (Option<String>, Option<String>, Option<String>) {
    if server_name == CODEX_APPS_MCP_SERVER_NAME {
        return (connector_id, connector_name, connector_description);
    }

    strip_untrusted_connector_meta(tool);
    (None, None, None)
}

fn strip_untrusted_connector_meta(tool: &mut RmcpTool) {
    if let Some(meta) = tool.meta.as_mut() {
        meta.retain(|key, _| !is_untrusted_connector_meta_key(key));
    }
}

fn is_untrusted_connector_meta_key(key: &str) -> bool {
    UNTRUSTED_CONNECTOR_META_KEYS.contains(&key)
}

fn resolve_bearer_token(
    server_name: &str,
    bearer_token_env_var: Option<&str>,
) -> Result<Option<String>> {
    let Some(env_var) = bearer_token_env_var else {
        return Ok(None);
    };

    match env::var(env_var) {
        Ok(value) => {
            if value.is_empty() {
                Err(anyhow!(
                    "Environment variable {env_var} for MCP server '{server_name}' is empty"
                ))
            } else {
                Ok(Some(value))
            }
        }
        Err(env::VarError::NotPresent) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' is not set"
        )),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' contains invalid Unicode"
        )),
    }
}

// 校验 server 名只含 `[a-zA-Z0-9_-]`。server 名是用户可控的，会被拼进工具名、
// 路径、配置键等位置，限制字符集可避免注入与命名冲突。
fn validate_mcp_server_name(server_name: &str) -> Result<()> {
    let re = regex_lite::Regex::new(r"^[a-zA-Z0-9_-]+$")?;
    if !re.is_match(server_name) {
        return Err(anyhow!(
            "Invalid MCP server name '{server_name}': must match pattern {pattern}",
            pattern = re.as_str()
        ));
    }
    Ok(())
}

/// 在已建好传输的客户端上完成启动：MCP `initialize` 握手 → 探测 server 能力 →
/// 列举工具（必要时写缓存）→ 套用工具过滤器 → 封装为就绪的 `ManagedClient`。
///
/// 所有错误都转成 `StartupOutcomeError`（扁平化为字符串），以便随共享 future 分发。
async fn start_server_task(
    server_name: String,
    client: Arc<RmcpClient>,
    params: StartServerTaskParams,
) -> Result<ManagedClient, StartupOutcomeError> {
    let StartServerTaskParams {
        startup_timeout,
        tool_timeout,
        tool_filter,
        tx_event,
        elicitation_requests,
        codex_apps_tools_cache_context,
        client_elicitation_capability,
    } = params;
    // Step 1：组装 initialize 请求参数——声明客户端能力（含 elicitation）、
    // 自报实现名/版本，并固定协议版本。
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation = Some(client_elicitation_capability);
    let params = InitializeRequestParams::new(
        capabilities,
        Implementation::new("codex-mcp-client", env!("CARGO_PKG_VERSION")).with_title("Codex"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18);

    // 为本 server 生成 elicitation 发送器：当 server 在握手或后续调用中发起
    // elicitation（征询授权/输入）时，请求会经此发往事件通道交由上层处理。
    let send_elicitation = elicitation_requests.make_sender(server_name.clone(), tx_event);

    // Step 2：执行 initialize 握手（受 startup_timeout 约束）。失败即启动失败。
    let initialize_result = client
        .initialize(params, startup_timeout, send_elicitation)
        .await
        .map_err(StartupOutcomeError::from)?;

    // Step 3：从握手返回的能力里探测 server 是否支持沙箱状态 meta 能力，
    // 决定后续工具调用是否要附带 SandboxState。
    let server_supports_sandbox_state_meta_capability = initialize_result
        .capabilities
        .experimental
        .as_ref()
        .and_then(|exp| exp.get(MCP_SANDBOX_STATE_META_CAPABILITY))
        .is_some();
    // Step 4：列举工具（绕过缓存的实时拉取），并埋点记录耗时。
    let list_start = Instant::now();
    let fetch_start = Instant::now();
    let tools = list_tools_for_client_uncached(
        &server_name,
        &client,
        startup_timeout,
        initialize_result.instructions.as_deref(),
    )
    .await
    .map_err(StartupOutcomeError::from)?;
    emit_duration(
        MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC,
        fetch_start.elapsed(),
        &[],
    );
    let server_info = mcp_server_info_from_implementation(initialize_result.server_info);
    write_cached_codex_apps_tools_if_needed(
        &server_name,
        codex_apps_tools_cache_context.as_ref(),
        &server_info,
        &tools,
    );
    if server_name == CODEX_APPS_MCP_SERVER_NAME {
        emit_duration(
            MCP_TOOLS_LIST_DURATION_METRIC,
            list_start.elapsed(),
            &[("cache", "miss")],
        );
    }
    let tools = filter_tools(tools, &tool_filter);

    let managed = ManagedClient {
        client: Arc::clone(&client),
        server_info,
        tools,
        tool_timeout: Some(tool_timeout),
        tool_filter,
        server_instructions: initialize_result.instructions,
        server_supports_sandbox_state_meta_capability,
        codex_apps_tools_cache_context,
    };

    Ok(managed)
}

fn mcp_server_info_from_implementation(server_info: Implementation) -> McpServerInfo {
    McpServerInfo {
        name: server_info.name,
        title: server_info.title,
        version: server_info.version,
        description: server_info.description,
        icons: server_info.icons.map(|icons| {
            icons
                .into_iter()
                .filter_map(|icon| serde_json::to_value(icon).ok())
                .collect()
        }),
        website_url: server_info.website_url,
    }
}

// `start_server_task` 的参数包：字段较多，打包成结构体以保持函数签名整洁。
struct StartServerTaskParams {
    // TODO 原注：未来应改由 cancel_token 统一处理超时，而非这里单独传 startup_timeout。
    startup_timeout: Option<Duration>, // TODO: cancel_token should handle this.
    tool_timeout: Duration,
    tool_filter: ToolFilter,
    tx_event: Sender<Event>,
    elicitation_requests: ElicitationRequestManager,
    codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    client_elicitation_capability: ElicitationCapability,
}

/// 按传输类型构造底层 `RmcpClient`：
///   - `Stdio`：拉起本地/远端子进程作为 server（本地用 `LocalStdioServerLauncher`，
///     非本地经执行后端用 `ExecutorStdioServerLauncher`）；
///   - `StreamableHttp`：连接 HTTP 端点，按需解析 bearer token / 鉴权提供方。
/// 仅负责「建连接」，不做握手——握手在 `start_server_task()` 里进行。
async fn make_rmcp_client(
    server_name: &str,
    server: EffectiveMcpServer,
    store_mode: OAuthCredentialsStoreMode,
    runtime_context: McpRuntimeContext,
    runtime_auth_provider: Option<SharedAuthProvider>,
) -> Result<RmcpClient, StartupOutcomeError> {
    let config = match server.launch() {
        McpServerLaunch::Configured(config) => config.as_ref().clone(),
    };
    // 解析该 server 应在哪个运行环境里启动（本地进程 / 远端执行后端等）。
    let resolved_environment = runtime_context
        .resolve_server_environment(server_name, &config)
        .map_err(|err| StartupOutcomeError::from(anyhow!(err)))?;
    let is_local_environment = config.is_local_environment();
    let McpServerConfig { transport, .. } = config;

    match transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            let command_os: OsString = command.into();
            let args_os: Vec<OsString> = args.into_iter().map(Into::into).collect();
            let env_os = env.map(|env| {
                env.into_iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect::<HashMap<_, _>>()
            });
            // 按是否本地环境选择 stdio 启动器：本地直接拉子进程，非本地走执行后端。
            let launcher = if is_local_environment {
                // TODO(starr): Unify local stdio MCP launch with
                // `ExecutorStdioServerLauncher` once the executor-backed path
                // preserves `LocalStdioServerLauncher` semantics.
                // TODO 原注：待 executor 路径能完整保留 `LocalStdioServerLauncher`
                //   语义后，再把本地 stdio 启动统一到 `ExecutorStdioServerLauncher`。
                Arc::new(LocalStdioServerLauncher::new(
                    runtime_context.local_stdio_fallback_cwd(),
                )) as Arc<dyn StdioServerLauncher>
            } else {
                // 非本地 server 在启动前必定已解析出环境（见上方 resolve），
                // 故此处缺失视为不变量被破坏，直接 panic。
                let Some(environment) = resolved_environment.as_ref() else {
                    unreachable!(
                        "non-local stdio MCP servers resolve an environment before launch"
                    );
                };
                Arc::new(ExecutorStdioServerLauncher::new(
                    environment.get_exec_backend(),
                )) as Arc<dyn StdioServerLauncher>
            };

            RmcpClient::new_stdio_client(command_os, args_os, env_os, &env_vars, cwd, launcher)
                .await
                .map_err(|err| StartupOutcomeError::from(anyhow!(err)))
        }
        McpServerTransportConfig::StreamableHttp {
            url,
            http_headers,
            env_http_headers,
            bearer_token_env_var,
        } => {
            let http_client = resolved_environment.as_ref().map_or_else(
                || Arc::new(ReqwestHttpClient) as Arc<dyn HttpClient>,
                |environment| environment.get_http_client(),
            );
            let resolved_bearer_token =
                match resolve_bearer_token(server_name, bearer_token_env_var.as_deref()) {
                    Ok(token) => token,
                    Err(error) => return Err(error.into()),
                };
            RmcpClient::new_streamable_http_client(
                server_name,
                &url,
                resolved_bearer_token,
                http_headers,
                env_http_headers,
                store_mode,
                http_client,
                runtime_auth_provider,
            )
            .await
            .map_err(StartupOutcomeError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::JsonObject;
    use rmcp::model::Meta;

    fn tool_with_connector_meta() -> RmcpTool {
        RmcpTool::new(
            "capture_file_upload",
            "test tool",
            Arc::new(JsonObject::default()),
        )
        .with_meta(Meta(
            serde_json::json!({
                "connector_id": "connector_gmail",
                "connector_name": "Gmail",
                "connector_display_name": "Gmail",
                "connector_description": "Mail connector",
                "connectorDescription": "Mail connector",
                "connectorFutureField": "future connector metadata",
                "CONNECTOR_UPPERCASE": "uppercase connector metadata",
                "openai/fileParams": ["file"],
                "custom": "kept"
            })
            .as_object()
            .expect("object")
            .clone(),
        ))
    }

    #[test]
    fn custom_mcp_connector_metadata_is_stripped() {
        let mut tool = tool_with_connector_meta();

        let (connector_id, connector_name, connector_description) =
            sanitize_tool_connector_metadata(
                "minimaltest",
                &mut tool,
                Some("connector_gmail".to_string()),
                Some("Gmail".to_string()),
                Some("Mail connector".to_string()),
            );

        assert_eq!(connector_id, None);
        assert_eq!(connector_name, None);
        assert_eq!(connector_description, None);

        let meta = tool.meta.as_ref().expect("meta");
        for key in [
            "connector_id",
            "connector_name",
            "connector_display_name",
            "connector_description",
            "connectorDescription",
        ] {
            assert!(!meta.0.contains_key(key), "{key} should be stripped");
        }
        assert!(meta.0.contains_key("connectorFutureField"));
        assert!(meta.0.contains_key("CONNECTOR_UPPERCASE"));
        assert!(meta.0.contains_key("openai/fileParams"));
        assert_eq!(
            meta.0.get("custom").and_then(|value| value.as_str()),
            Some("kept")
        );
    }

    #[test]
    fn codex_apps_connector_metadata_is_preserved() {
        let mut tool = tool_with_connector_meta();

        let (connector_id, connector_name, connector_description) =
            sanitize_tool_connector_metadata(
                CODEX_APPS_MCP_SERVER_NAME,
                &mut tool,
                Some("connector_gmail".to_string()),
                Some("Gmail".to_string()),
                Some("Mail connector".to_string()),
            );

        assert_eq!(connector_id.as_deref(), Some("connector_gmail"));
        assert_eq!(connector_name.as_deref(), Some("Gmail"));
        assert_eq!(connector_description.as_deref(), Some("Mail connector"));

        let meta = tool.meta.as_ref().expect("meta");
        for key in [
            "connector_id",
            "connector_name",
            "connector_display_name",
            "connector_description",
            "connectorDescription",
            "connectorFutureField",
            "CONNECTOR_UPPERCASE",
        ] {
            assert!(meta.0.contains_key(key), "{key} should be preserved");
        }
    }
}
