//! 【文件职责】MCP 工具调用在 core 侧的业务编排主入口。模型发起一个 MCP 工具
//! 调用后，最终落到本文件的 `handle_mcp_tool_call`，由它完成完整链路：
//! 解析参数 → 查元数据 → 定审批模式 → 发 started 事件 → 请求审批 → 执行 →
//! 发 completed 事件 → 上报指标，返回 `HandledMcpToolCall`。
//!
//! 审批是这里最复杂、最易出 bug 的部分（`maybe_request_mcp_tool_approval` 是一棵
//! 多层决策树），围绕它还有一组「记住决策」的逻辑（会话级 / 持久化 / Guardian
//! 委托）和审批提示/引导（elicitation）的构造与解析。
//!
//! 【架构位置】
//!   层级：Agent 核心层（MCP 客户端方向，工具调用编排）
//!   上游：工具路由（模型 tool call 分派到 MCP 工具时调 `handle_mcp_tool_call`）
//!   下游：`session/mcp.rs`（`call_tool` / `request_mcp_server_elicitation`）、
//!         `guardian`（审批委托）、`connectors`（codex_apps 策略）、config 持久化
//!
//! 【数据流】tool call(server, tool, arguments)
//!           → 审批决策 → [Accept] Session::call_tool → server → 结果
//!           → 清洗/截断 → completed 事件 + 回喂模型的 `CallToolResult`
//!
//! 【阅读建议】先读 `handle_mcp_tool_call`（主线），它调用的两个核心是
//!   `maybe_request_mcp_tool_approval`（审批决策树）和 `handle_approved_mcp_tool_call`
//!   （实际执行）。审批「记住」相关看 `apply_mcp_tool_approval_decision` /
//!   `maybe_persist_mcp_tool_approval`。其余 `build_*` / `parse_*` / `persist_*`
//!   是审批提示构造、应答解析、config 写入的辅助，按需查阅即可。
//!   对照 learn_docs/4_工具与多Agent/19_mcp_integration.md §3.4、§5、§7。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use crate::config::Config;
use crate::config::edit::ConfigEdit;
use crate::config::edit::ConfigEditsBuilder;
use crate::connectors;
use crate::guardian::GuardianApprovalRequest;
use crate::guardian::GuardianMcpAnnotations;
use crate::guardian::guardian_rejection_message;
use crate::guardian::guardian_timeout_message;
use crate::guardian::new_guardian_review_id;
use crate::guardian::review_approval_request;
use crate::guardian::routes_approval_to_guardian;
use crate::hook_runtime::run_permission_request_hooks;
use crate::mcp_openai_file::rewrite_mcp_tool_arguments_for_openai_files;
use crate::mcp_tool_approval_templates::RenderedMcpToolApprovalParam;
use crate::mcp_tool_approval_templates::render_mcp_tool_approval_template;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::turn_metadata::McpTurnMetadataContext;
use codex_analytics::AppInvocation;
use codex_analytics::InvocationType;
use codex_analytics::build_track_events_context;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::McpElicitationObjectType;
use codex_app_server_protocol::McpElicitationSchema;
use codex_app_server_protocol::McpServerElicitationRequest;
use codex_app_server_protocol::McpServerElicitationRequestParams;
use codex_config::types::AppToolApproval;
use codex_features::Feature;
use codex_hooks::PermissionRequestDecision;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::MCP_TOOL_CODEX_APPS_META_KEY;
use codex_mcp::McpPermissionPromptAutoApproveContext;
use codex_mcp::SandboxState;
use codex_mcp::auth_elicitation_completed_result;
use codex_mcp::build_auth_elicitation_plan;
use codex_mcp::declared_openai_file_input_param_names;
use codex_mcp::mcp_permission_prompt_is_auto_approved;
use codex_otel::sanitize_metric_tag_value;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY as MCP_TOOL_APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL as MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::CONNECTOR_DESCRIPTION_KEY as MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_ID_KEY as MCP_TOOL_APPROVAL_CONNECTOR_ID_KEY;
use codex_protocol::mcp_approval_meta::CONNECTOR_NAME_KEY as MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_ALWAYS as MCP_TOOL_APPROVAL_PERSIST_ALWAYS;
use codex_protocol::mcp_approval_meta::PERSIST_KEY as MCP_TOOL_APPROVAL_PERSIST_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_SESSION as MCP_TOOL_APPROVAL_PERSIST_SESSION;
use codex_protocol::mcp_approval_meta::SOURCE_CONNECTOR as MCP_TOOL_APPROVAL_SOURCE_CONNECTOR;
use codex_protocol::mcp_approval_meta::SOURCE_KEY as MCP_TOOL_APPROVAL_SOURCE_KEY;
use codex_protocol::mcp_approval_meta::TOOL_DESCRIPTION_KEY as MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_DISPLAY_KEY as MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY as MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY;
use codex_protocol::mcp_approval_meta::TOOL_TITLE_KEY as MCP_TOOL_APPROVAL_TOOL_TITLE_KEY;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rollout::state_db;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use rmcp::model::ToolAnnotations;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use toml_edit::value;
use tracing::Instrument;
use tracing::Span;
use tracing::error;
use tracing::field::Empty;
use url::Url;

const MCP_CALL_COUNT_METRIC: &str = "codex.mcp.call";
const MCP_CALL_DURATION_METRIC: &str = "codex.mcp.call.duration_ms";
const MCP_RESULT_TELEMETRY_META_KEY: &str = "codex/telemetry";
const MCP_RESULT_TELEMETRY_SPAN_KEY: &str = "span";
const MCP_RESULT_TELEMETRY_TARGET_ID_KEY: &str = "target_id";
const MCP_RESULT_TELEMETRY_DID_TRIGGER_SERVER_USER_FLOW_KEY: &str = "did_trigger_server_user_flow";
const MCP_RESULT_TELEMETRY_TARGET_ID_SPAN_ATTR: &str = "codex.mcp.target.id";
const MCP_RESULT_TELEMETRY_SERVER_USER_FLOW_SPAN_ATTR: &str =
    "codex.mcp.server_user_flow.triggered";
const MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS: usize = 256;
const MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES: usize = DEFAULT_OUTPUT_BYTES_CAP;

/// Handles the specified tool call and dispatches the appropriate MCP tool-call
/// item lifecycle events to the `Session`.
/// MCP 工具调用的总入口：完成「解析 → 查元数据 → 定审批 → started 事件 →
/// 审批 → 执行 → completed 事件 → 指标」全链路，并向 `Session` 发出对应的
/// 工具调用生命周期事件（started/completed/skip）。
///
/// @returns - `HandledMcpToolCall`：回喂模型的 `CallToolResult` + 实际入参
///            `tool_input`（可能被重写过，如 OpenAI 文件上传）。
///
/// 副作用：发会话事件、上报指标，并可能向用户请求审批/引导。
/// 异常：参数解析失败、被禁用、被拒绝等都不抛错，而是返回承载错误的结果。
pub(crate) async fn handle_mcp_tool_call(
    sess: Arc<Session>,
    turn_context: &Arc<TurnContext>,
    call_id: String,
    server: String,
    tool_name: String,
    hook_tool_name: HookToolName,
    arguments: String,
) -> HandledMcpToolCall {
    // Parse the `arguments` as JSON. An empty string is OK, but invalid JSON
    // is not.
    // Step 1：把 `arguments` 解析为 JSON。空串视作「无参数」（None）合法；
    // 非法 JSON 直接短路返回错误结果（不发起调用）。
    let arguments_value = if arguments.trim().is_empty() {
        None
    } else {
        match serde_json::from_str::<serde_json::Value>(&arguments) {
            Ok(value) => Some(value),
            Err(e) => {
                error!("failed to parse tool call arguments: {e}");
                return HandledMcpToolCall {
                    result: CallToolResult::from_error_text(format!("err: {e}")),
                    tool_input: JsonValue::Object(serde_json::Map::new()),
                };
            }
        }
    };

    let invocation = McpInvocation {
        server: server.clone(),
        tool: tool_name.clone(),
        arguments: arguments_value.clone(),
    };

    // Step 2：聚合该工具的元数据（注释/连接器/插件/文件参数等），供审批提示、
    // 埋点与事件项使用。
    let metadata =
        lookup_mcp_tool_metadata(sess.as_ref(), turn_context.as_ref(), &server, &tool_name).await;
    let item_metadata = McpToolCallItemMetadata {
        mcp_app_resource_uri: metadata
            .as_ref()
            .and_then(|metadata| metadata.mcp_app_resource_uri.clone()),
        plugin_id: metadata
            .as_ref()
            .and_then(|metadata| metadata.plugin_id.clone()),
    };
    // Step 3：确定审批模式。codex_apps 工具用连接器侧的 app 策略（含启用开关），
    // 其它自定义 MCP 用用户在 config/插件里配的 `custom_mcp_tool_approval_mode`。
    let app_tool_policy = if server == CODEX_APPS_MCP_SERVER_NAME {
        connectors::app_tool_policy(
            &turn_context.config,
            metadata
                .as_ref()
                .and_then(|metadata| metadata.connector_id.as_deref()),
            &tool_name,
            metadata
                .as_ref()
                .and_then(|metadata| metadata.tool_title.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.annotations.as_ref()),
        )
    } else {
        connectors::AppToolPolicy::default()
    };
    let approval_mode = if server == CODEX_APPS_MCP_SERVER_NAME {
        app_tool_policy.approval
    } else {
        custom_mcp_tool_approval_mode(sess.as_ref(), turn_context.as_ref(), &server, &tool_name)
            .await
    };

    // codex_apps 工具若被配置禁用，直接跳过（发 skip 事件 + 计数）并返回，
    // 不进入审批/执行。
    if server == CODEX_APPS_MCP_SERVER_NAME && !app_tool_policy.enabled {
        let result = notify_mcp_tool_call_skip(
            sess.as_ref(),
            turn_context.as_ref(),
            &call_id,
            invocation,
            item_metadata.clone(),
            "MCP tool call blocked by app configuration".to_string(),
            /*already_started*/ false,
        )
        .await;
        let status = if result.is_ok() { "ok" } else { "error" };
        turn_context.session_telemetry.counter(
            MCP_CALL_COUNT_METRIC,
            /*inc*/ 1,
            &[("status", status)],
        );
        return HandledMcpToolCall {
            result: CallToolResult::from_result(result),
            tool_input: arguments_value
                .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new())),
        };
    }
    let connector_id = metadata
        .as_ref()
        .and_then(|metadata| metadata.connector_id.clone());
    let connector_name = metadata
        .as_ref()
        .and_then(|metadata| metadata.connector_name.clone());

    // Step 4：发 started 事件，UI 立即出现「工具调用进行中」。
    notify_mcp_tool_call_started(
        sess.as_ref(),
        turn_context.as_ref(),
        &call_id,
        invocation.clone(),
        item_metadata.clone(),
    )
    .await;

    // Step 5：请求审批。返回 `None` 表示无需审批（直接走最后的执行分支）；
    // 返回 `Some(决策)` 则按决策映射：接受类 → 执行；拒绝/取消 → 发 skip 事件。
    if let Some(decision) = maybe_request_mcp_tool_approval(
        &sess,
        turn_context,
        &call_id,
        &invocation,
        &hook_tool_name,
        metadata.as_ref(),
        approval_mode,
    )
    .await
    {
        let result = match decision {
            McpToolApprovalDecision::Accept
            | McpToolApprovalDecision::AcceptForSession
            | McpToolApprovalDecision::AcceptAndRemember => {
                return handle_approved_mcp_tool_call(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &call_id,
                    invocation,
                    metadata.as_ref(),
                    item_metadata,
                )
                .await;
            }
            McpToolApprovalDecision::Decline { message } => {
                let message = message.unwrap_or_else(|| "user rejected MCP tool call".to_string());
                notify_mcp_tool_call_skip(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &call_id,
                    invocation,
                    item_metadata.clone(),
                    message,
                    /*already_started*/ true,
                )
                .await
            }
            McpToolApprovalDecision::Cancel => {
                let message = "user cancelled MCP tool call".to_string();
                notify_mcp_tool_call_skip(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &call_id,
                    invocation,
                    item_metadata.clone(),
                    message,
                    /*already_started*/ true,
                )
                .await
            }
        };

        let status = if result.is_ok() { "ok" } else { "error" };
        emit_mcp_call_metrics(
            turn_context.as_ref(),
            status,
            &tool_name,
            connector_id.as_deref(),
            connector_name.as_deref(),
            /*duration*/ None,
        );

        return HandledMcpToolCall {
            result: CallToolResult::from_result(result),
            tool_input: arguments_value
                .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new())),
        };
    }

    // Step 6：审批返回 None（无需审批）→ 直接执行。
    handle_approved_mcp_tool_call(
        sess.as_ref(),
        turn_context.as_ref(),
        &call_id,
        invocation,
        metadata.as_ref(),
        item_metadata,
    )
    .await
}

/// `handle_mcp_tool_call` 的返回值。`result` 回喂模型；`tool_input` 是工具实际
/// 收到的入参（重写后），与原始 `arguments` 可能不同（如 OpenAI 文件上传重写）。
pub(crate) struct HandledMcpToolCall {
    pub(crate) result: CallToolResult,
    pub(crate) tool_input: JsonValue,
}

/// 写入工具调用事件项（`McpToolCallItem`）的附加字段：app UI 资源 URI 与来源
/// 插件 id。与审批用的 `McpToolApprovalMetadata` 分开，只携带事件展示所需信息。
#[derive(Clone)]
struct McpToolCallItemMetadata {
    mcp_app_resource_uri: Option<String>,
    plugin_id: Option<String>,
}

/// 审批通过后的实际执行路径。负责：必要的参数重写（OpenAI 文件上传）、构造
/// request meta、调用工具、记录耗时、发 completed 事件、上报指标与埋点。
/// 全程包在一个 tracing span 里（`mcp.tools.call`）以便观测。
///
/// 副作用：发 completed 事件、上报指标/埋点；可能标记线程记忆被污染、触发
///   codex_apps 鉴权引导。错误以 `CallToolResult` 形式回喂模型，不向上抛。
async fn handle_approved_mcp_tool_call(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    metadata: Option<&McpToolApprovalMetadata>,
    item_metadata: McpToolCallItemMetadata,
) -> HandledMcpToolCall {
    let server = invocation.server.clone();
    // 若该 server 会污染记忆且配置要求，标记本线程「记忆模式被外部上下文污染」。
    maybe_mark_thread_memory_mode_polluted(sess, turn_context, &server).await;
    let tool_name = invocation.tool.clone();
    let arguments_value = invocation.arguments.clone();
    let connector_id = metadata.and_then(|metadata| metadata.connector_id.as_deref());
    let connector_name = metadata.and_then(|metadata| metadata.connector_name.as_deref());
    let server_origin = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .server_origin(&server)
        .map(str::to_string);

    let start = Instant::now();
    // 按需把入参里的文件参数重写为 OpenAI 文件引用（仅 codex_apps 声明了文件
    // 参数时生效）。重写失败时 `tool_input` 回退到原始参数。
    let rewrite = rewrite_mcp_tool_arguments_for_openai_files(
        sess,
        turn_context,
        arguments_value.clone(),
        metadata.and_then(|metadata| metadata.openai_file_input_params.as_deref()),
    )
    .await;
    let tool_input = match &rewrite {
        Ok(Some(rewritten_arguments)) => rewritten_arguments.clone(),
        Ok(None) | Err(_) => arguments_value
            .clone()
            .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new())),
    };
    // 构造执行 future 并用 tracing span 包裹（span 字段含 server/tool/连接器
    // /传输方式等），便于分布式追踪。`rewrite?` 把重写错误并入结果。
    let result = async {
        let rewritten_arguments = rewrite?;
        let request_meta =
            build_mcp_tool_call_request_meta(turn_context, &server, call_id, metadata);
        let result = execute_mcp_tool_call(
            sess,
            turn_context,
            call_id,
            &invocation,
            rewritten_arguments,
            metadata,
            request_meta,
        )
        .await;
        record_mcp_result_span_telemetry(&Span::current(), result.as_ref().ok());
        result
    }
    .instrument(mcp_tool_call_span(
        sess,
        turn_context,
        McpToolCallSpanFields {
            server_name: &server,
            tool_name: &tool_name,
            call_id,
            server_origin: server_origin.as_deref(),
            connector_id,
            connector_name,
        },
    ))
    .await;
    if let Err(error) = &result {
        tracing::warn!("MCP tool call error: {error:?}");
    }
    let duration = start.elapsed();
    notify_mcp_tool_call_completed(
        sess,
        turn_context,
        call_id,
        invocation,
        item_metadata,
        duration,
        truncate_mcp_tool_result_for_event(&result),
    )
    .await;
    maybe_track_codex_app_used(sess, turn_context, &server, &tool_name).await;

    let status = if result.is_ok() { "ok" } else { "error" };
    emit_mcp_call_metrics(
        turn_context,
        status,
        &tool_name,
        connector_id,
        connector_name,
        Some(duration),
    );

    HandledMcpToolCall {
        result: CallToolResult::from_result(result),
        tool_input,
    }
}

fn emit_mcp_call_metrics(
    turn_context: &TurnContext,
    status: &str,
    tool_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
    duration: Option<Duration>,
) {
    let tags = mcp_call_metric_tags(status, tool_name, connector_id, connector_name);
    let tag_refs: Vec<(&str, &str)> = tags
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    turn_context
        .session_telemetry
        .counter(MCP_CALL_COUNT_METRIC, /*inc*/ 1, &tag_refs);
    if let Some(duration) = duration {
        turn_context.session_telemetry.record_duration(
            MCP_CALL_DURATION_METRIC,
            duration,
            &tag_refs,
        );
    }
}

fn mcp_call_metric_tags(
    status: &str,
    tool_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut tags = vec![
        ("status", sanitize_metric_tag_value(status)),
        ("tool", sanitize_metric_tag_value(tool_name)),
    ];
    if let Some(connector_id) = connector_id.filter(|connector_id| !connector_id.is_empty()) {
        tags.push(("connector_id", sanitize_metric_tag_value(connector_id)));
    }
    if let Some(connector_name) = connector_name.filter(|connector_name| !connector_name.is_empty())
    {
        tags.push(("connector_name", sanitize_metric_tag_value(connector_name)));
    }
    tags
}

fn mcp_tool_call_span(
    session: &Session,
    turn_context: &TurnContext,
    fields: McpToolCallSpanFields<'_>,
) -> Span {
    let transport = match fields.server_origin {
        Some("stdio") => "stdio",
        Some("in_process") => "in_process",
        Some(_) => "streamable_http",
        None => "",
    };
    let span = tracing::info_span!(
        "mcp.tools.call",
        otel.kind = "client",
        rpc.system = "jsonrpc",
        rpc.method = "tools/call",
        mcp.server.name = fields.server_name,
        mcp.server.origin = fields.server_origin.unwrap_or(""),
        mcp.transport = transport,
        mcp.connector.id = fields.connector_id.unwrap_or(""),
        mcp.connector.name = fields.connector_name.unwrap_or(""),
        tool.name = fields.tool_name,
        tool.call_id = fields.call_id,
        conversation.id = %session.conversation_id,
        session.id = %session.conversation_id,
        turn.id = turn_context.sub_id.as_str(),
        server.address = Empty,
        server.port = Empty,
        codex.mcp.target.id = Empty,
        codex.mcp.server_user_flow.triggered = Empty,
    );
    record_server_fields(&span, fields.server_origin);
    span
}

struct McpToolCallSpanFields<'a> {
    server_name: &'a str,
    tool_name: &'a str,
    call_id: &'a str,
    server_origin: Option<&'a str>,
    connector_id: Option<&'a str>,
    connector_name: Option<&'a str>,
}

fn record_server_fields(span: &Span, url: Option<&str>) {
    let Some(url) = url else {
        return;
    };
    let Ok(parsed) = Url::parse(url) else {
        return;
    };
    if let Some(host) = parsed.host_str() {
        span.record("server.address", host);
    }
    if let Some(port) = parsed.port_or_known_default() {
        span.record("server.port", port as i64);
    }
}

fn record_mcp_result_span_telemetry(span: &Span, result: Option<&CallToolResult>) {
    let Some(span_telemetry) = result
        .and_then(|result| result.meta.as_ref())
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(MCP_RESULT_TELEMETRY_META_KEY))
        .and_then(JsonValue::as_object)
        .and_then(|telemetry| telemetry.get(MCP_RESULT_TELEMETRY_SPAN_KEY))
        .and_then(JsonValue::as_object)
    else {
        return;
    };

    if let Some(target_id) = span_telemetry
        .get(MCP_RESULT_TELEMETRY_TARGET_ID_KEY)
        .and_then(JsonValue::as_str)
        .filter(|target_id| !target_id.is_empty())
    {
        span.record(
            MCP_RESULT_TELEMETRY_TARGET_ID_SPAN_ATTR,
            truncate_str_to_char_boundary(target_id, MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS),
        );
    }

    if let Some(did_trigger_server_user_flow) = span_telemetry
        .get(MCP_RESULT_TELEMETRY_DID_TRIGGER_SERVER_USER_FLOW_KEY)
        .and_then(JsonValue::as_bool)
    {
        span.record(
            MCP_RESULT_TELEMETRY_SERVER_USER_FLOW_SPAN_ATTR,
            did_trigger_server_user_flow,
        );
    }
}

fn truncate_str_to_char_boundary(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

/// 真正执行一次 MCP 工具调用：补齐 request meta（线程 id、沙箱状态、调用追踪）→
/// 经 `Session::call_tool` 路由到对应 server → 对结果做模型适配清洗 → 必要时
/// 触发 codex_apps 鉴权引导。错误统一转成 `Err(String)`。
async fn execute_mcp_tool_call(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: &McpInvocation,
    rewritten_arguments: Option<JsonValue>,
    metadata: Option<&McpToolApprovalMetadata>,
    request_meta: Option<JsonValue>,
) -> Result<CallToolResult, String> {
    // 给 request meta 注入线程 id；若 server 声明支持沙箱状态能力，再补沙箱状态。
    let request_meta =
        with_mcp_tool_call_thread_id_meta(request_meta, &sess.conversation_id.to_string());
    let request_meta = augment_mcp_tool_request_meta_with_sandbox_state(
        sess,
        turn_context,
        &invocation.server,
        request_meta,
    )
    .await
    .map_err(|e| format!("failed to build MCP tool request metadata: {e:#}"))?;
    // 开一条 rollout 调用追踪，把追踪信息也并入 request meta，再发起调用。
    let mcp_call_trace = sess
        .services
        .rollout_thread_trace
        .start_mcp_call_trace(call_id);
    let request_meta = mcp_call_trace.add_request_meta(request_meta);
    let result = sess
        .call_tool(
            &invocation.server,
            &invocation.tool,
            rewritten_arguments,
            request_meta,
        )
        .await
        .map_err(|e| format!("tool call error: {e:?}"))?;
    // 按模型能力清洗结果（不支持图片输入时把 image 块替换为占位文本）。
    let result = sanitize_mcp_tool_result_for_model(
        turn_context
            .model_info
            .input_modalities
            .contains(&InputModality::Image),
        Ok(result),
    )?;
    Ok(maybe_request_codex_apps_auth_elicitation(
        sess,
        turn_context,
        call_id,
        &invocation.server,
        metadata,
        result,
    )
    .await)
}

/// codex_apps 工具调用返回鉴权失败时，按需向用户发起一次「去 URL 授权」引导。
/// 只在 host 端 codex_apps server、启用 `AuthElicitation` 特性、且审批策略允许
/// 引导时触发；用户接受后刷新 codex_apps 工具缓存并把结果标记为「已完成鉴权」。
/// 任一前置不满足或用户未接受，原样返回结果。
async fn maybe_request_codex_apps_auth_elicitation(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    server: &str,
    metadata: Option<&McpToolApprovalMetadata>,
    result: CallToolResult,
) -> CallToolResult {
    // 非 host 端 codex_apps server 不涉及该鉴权引导。
    if !sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .is_host_owned_codex_apps_server(server)
    {
        return result;
    }

    if !turn_context.features.enabled(Feature::AuthElicitation) {
        return result;
    }

    match turn_context.approval_policy.value() {
        AskForApproval::Never => return result,
        AskForApproval::Granular(granular_config) if !granular_config.allows_mcp_elicitations() => {
            return result;
        }
        AskForApproval::OnFailure
        | AskForApproval::OnRequest
        | AskForApproval::UnlessTrusted
        | AskForApproval::Granular(_) => {}
    }

    let connector_id = metadata.and_then(|metadata| metadata.connector_id.as_deref());
    let connector_name = metadata.and_then(|metadata| metadata.connector_name.as_deref());
    let install_url = connector_id.map(|connector_id| {
        codex_connectors::metadata::connector_install_url(
            connector_name.unwrap_or(connector_id),
            connector_id,
        )
    });
    let Some(plan) =
        build_auth_elicitation_plan(call_id, &result, connector_id, connector_name, install_url)
    else {
        return result;
    };

    let request_id = rmcp::model::RequestId::String(plan.elicitation.elicitation_id.clone().into());
    let params = McpServerElicitationRequestParams {
        thread_id: sess.conversation_id.to_string(),
        turn_id: Some(turn_context.sub_id.clone()),
        server_name: CODEX_APPS_MCP_SERVER_NAME.to_string(),
        request: McpServerElicitationRequest::Url {
            meta: Some(plan.elicitation.meta),
            message: plan.elicitation.message,
            url: plan.elicitation.url,
            elicitation_id: plan.elicitation.elicitation_id,
        },
    };
    let response = sess
        .request_mcp_server_elicitation(turn_context, request_id, params)
        .await
        .response;
    if !response
        .as_ref()
        .is_some_and(|response| response.action == ElicitationAction::Accept)
    {
        return result;
    }

    refresh_codex_apps_after_connector_auth(sess, turn_context).await;
    auth_elicitation_completed_result(&plan.auth_failure, result.meta)
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "Codex Apps cache refresh reads through the session-owned manager guard"
)]
async fn refresh_codex_apps_after_connector_auth(sess: &Session, turn_context: &TurnContext) {
    let mcp_tools_result = {
        let manager = sess.services.mcp_connection_manager.read().await;
        manager.hard_refresh_codex_apps_tools_cache().await
    };

    match mcp_tools_result {
        Ok(mcp_tools) => {
            let auth = sess.services.auth_manager.auth().await;
            connectors::refresh_accessible_connectors_cache_from_mcp_tools(
                &turn_context.config,
                auth.as_ref(),
                &mcp_tools,
            );
        }
        Err(err) => {
            tracing::warn!("failed to refresh Codex Apps tools after connector auth: {err:#}");
        }
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "MCP sandbox metadata reads through the session-owned manager guard"
)]
async fn augment_mcp_tool_request_meta_with_sandbox_state(
    sess: &Session,
    turn_context: &TurnContext,
    server: &str,
    mut meta: Option<serde_json::Value>,
) -> anyhow::Result<Option<serde_json::Value>> {
    let supports_sandbox_state_meta = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .server_supports_sandbox_state_meta_capability(server)
        .await
        .unwrap_or(false);
    if !supports_sandbox_state_meta {
        return Ok(meta);
    }

    let sandbox_state = serde_json::to_value(SandboxState {
        permission_profile: Some(turn_context.permission_profile()),
        sandbox_policy: turn_context.sandbox_policy(),
        codex_linux_sandbox_exe: turn_context.codex_linux_sandbox_exe.clone(),
        #[allow(deprecated)]
        sandbox_cwd: turn_context.cwd.to_path_buf(),
        use_legacy_landlock: turn_context.features.use_legacy_landlock(),
    })?;

    match meta.as_mut() {
        Some(serde_json::Value::Object(map)) => {
            map.insert(
                codex_mcp::MCP_SANDBOX_STATE_META_CAPABILITY.to_string(),
                sandbox_state,
            );
        }
        Some(_) => {}
        None => {
            let mut map = serde_json::Map::new();
            map.insert(
                codex_mcp::MCP_SANDBOX_STATE_META_CAPABILITY.to_string(),
                sandbox_state,
            );
            meta = Some(serde_json::Value::Object(map));
        }
    }

    Ok(meta)
}

async fn maybe_mark_thread_memory_mode_polluted(
    sess: &Session,
    turn_context: &TurnContext,
    server: &str,
) {
    if !turn_context.config.memories.disable_on_external_context {
        return;
    }
    let pollutes_memory = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .server_pollutes_memory(server);
    if !pollutes_memory {
        return;
    }
    state_db::mark_thread_memory_mode_polluted(
        sess.services.state_db.as_deref(),
        sess.conversation_id,
        "mcp_tool_call",
    )
    .await;
}

// 按模型能力清洗回喂给模型的结果：模型不支持图片输入时，把结果里的 image 内容
// 块替换为说明性文本，避免给模型发它无法处理的图片。支持图片则原样返回。
fn sanitize_mcp_tool_result_for_model(
    supports_image_input: bool,
    result: Result<CallToolResult, String>,
) -> Result<CallToolResult, String> {
    if supports_image_input {
        return result;
    }

    result.map(|call_tool_result| CallToolResult {
        content: call_tool_result
            .content
            .iter()
            .map(|block| {
                if let Some(content_type) = block.get("type").and_then(serde_json::Value::as_str)
                    && content_type == "image"
                {
                    return serde_json::json!({
                        "type": "text",
                        "text": "<image content omitted because you do not support image input>",
                    });
                }

                block.clone()
            })
            .collect::<Vec<_>>(),
        structured_content: call_tool_result.structured_content,
        is_error: call_tool_result.is_error,
        meta: call_tool_result.meta,
    })
}

// 为「事件/rollout 持久化」截断工具结果。回喂模型的结果不走这里，截断只作用于
// 事件副本：超大结果会被 rollout 长期存储，故超过字节上限时折叠成一段文本预览。
fn truncate_mcp_tool_result_for_event(
    result: &Result<CallToolResult, String>,
) -> Result<CallToolResult, String> {
    match result {
        Ok(call_tool_result) => {
            // The app-server rebuilds `ThreadItem::McpToolCall` from this item,
            // so avoid persisting multi-megabyte results in rollout storage.
            // app-server 会用这个事件项重建 `ThreadItem::McpToolCall`，因此不能
            // 把数 MB 的结果原样写进 rollout 存储。
            let Ok(serialized) = serde_json::to_string(call_tool_result) else {
                return Ok(call_tool_result.clone());
            };
            if serialized.len() <= MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES {
                return Ok(call_tool_result.clone());
            }

            // A huge MCP result can put bytes in `content`, `structuredContent`,
            // or `_meta`. Collapse the event copy to a text preview of the whole
            // serialized result so the UI still has useful context without
            // preserving a multi-megabyte structured payload.
            //
            // This budget applies to the preview text, not the final event JSON.
            // The preview is itself serialized into a JSON string, so quotes and
            // backslashes can be escaped again and the stored event may end up
            // somewhat larger than this byte budget.
            let truncated = truncate_text(
                &serialized,
                TruncationPolicy::Bytes(MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES),
            );
            Ok(CallToolResult {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": truncated,
                })],
                structured_content: None,
                is_error: call_tool_result.is_error,
                meta: None,
            })
        }
        Err(message) => Err(truncate_text(
            message,
            TruncationPolicy::Bytes(MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES),
        )),
    }
}

async fn notify_mcp_tool_call_started(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
) {
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(JsonValue::Null),
        mcp_app_resource_uri: item_metadata.mcp_app_resource_uri,
        plugin_id: item_metadata.plugin_id,
        status: McpToolCallStatus::InProgress,
        result: None,
        error: None,
        duration: None,
    });
    sess.emit_turn_item_started(turn_context, &item).await;
}

async fn notify_mcp_tool_call_completed(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
    duration: Duration,
    result: Result<CallToolResult, String>,
) {
    let (status, result, error) = match result {
        Ok(result) if result.is_error.unwrap_or(false) => {
            (McpToolCallStatus::Failed, Some(result), None)
        }
        Ok(result) => (McpToolCallStatus::Completed, Some(result), None),
        Err(message) => (
            McpToolCallStatus::Failed,
            None,
            Some(McpToolCallError { message }),
        ),
    };
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(JsonValue::Null),
        mcp_app_resource_uri: item_metadata.mcp_app_resource_uri,
        plugin_id: item_metadata.plugin_id,
        status,
        result,
        error,
        duration: Some(duration),
    });
    sess.emit_turn_item_completed(turn_context, item).await;
}

struct McpAppUsageMetadata {
    connector_id: Option<String>,
    app_name: Option<String>,
}

async fn maybe_track_codex_app_used(
    sess: &Session,
    turn_context: &TurnContext,
    server: &str,
    tool_name: &str,
) {
    if server != CODEX_APPS_MCP_SERVER_NAME {
        return;
    }
    let metadata = lookup_mcp_app_usage_metadata(sess, server, tool_name).await;
    let (connector_id, app_name) = metadata
        .map(|metadata| (metadata.connector_id, metadata.app_name))
        .unwrap_or((None, None));
    let invocation_type = if let Some(connector_id) = connector_id.as_deref() {
        let mentioned_connector_ids = sess.get_connector_selection().await;
        if mentioned_connector_ids.contains(connector_id) {
            InvocationType::Explicit
        } else {
            InvocationType::Implicit
        }
    } else {
        InvocationType::Implicit
    };

    let tracking = build_track_events_context(
        turn_context.model_info.slug.clone(),
        sess.conversation_id.to_string(),
        turn_context.sub_id.clone(),
    );
    sess.services.analytics_events_client.track_app_used(
        tracking,
        AppInvocation {
            connector_id,
            app_name,
            invocation_type: Some(invocation_type),
        },
    );
}

/// 审批决策。三种「接受」对应不同的记忆范围：`Accept` 只此一次；
/// `AcceptForSession` 记住到本会话；`AcceptAndRemember` 持久化到 config。
/// `Decline` 带可选拒绝消息，`Cancel` 表示用户取消。落地由
/// `apply_mcp_tool_approval_decision` 处理。
#[derive(Debug, Clone, PartialEq, Eq)]
enum McpToolApprovalDecision {
    Accept,
    AcceptForSession,
    AcceptAndRemember,
    Decline { message: Option<String> },
    Cancel,
}

/// 一个 MCP 工具的全部审批/展示元数据，由 `lookup_mcp_tool_metadata` 聚合。
/// 喂给审批提示、Guardian 复核请求、埋点。codex_apps 专属字段
/// （`codex_apps_meta` / `openai_file_input_params`）对自定义 MCP 为 `None`。
pub(crate) struct McpToolApprovalMetadata {
    annotations: Option<ToolAnnotations>,
    connector_id: Option<String>,
    connector_name: Option<String>,
    connector_description: Option<String>,
    plugin_id: Option<String>,
    tool_title: Option<String>,
    tool_description: Option<String>,
    mcp_app_resource_uri: Option<String>,
    codex_apps_meta: Option<serde_json::Map<String, serde_json::Value>>,
    openai_file_input_params: Option<Vec<String>>,
}

const MCP_TOOL_OPENAI_OUTPUT_TEMPLATE_META_KEY: &str = "openai/outputTemplate";
const MCP_TOOL_UI_RESOURCE_URI_META_KEY: &str = "ui/resourceUri";
const MCP_TOOL_PLUGIN_ID_META_KEY: &str = "plugin_id";
const MCP_TOOL_THREAD_ID_META_KEY: &str = "threadId";

async fn custom_mcp_tool_approval_mode(
    sess: &Session,
    turn_context: &TurnContext,
    server: &str,
    tool_name: &str,
) -> AppToolApproval {
    let user_configured_mode = turn_context
        .config
        .config_layer_stack
        .effective_config()
        .as_table()
        .and_then(|table| table.get("mcp_servers"))
        .cloned()
        .and_then(|value| {
            HashMap::<String, codex_config::types::McpServerConfig>::deserialize(value).ok()
        })
        .and_then(|servers| {
            let server_config = servers.get(server)?;
            Some(
                server_config
                    .tools
                    .get(tool_name)
                    .and_then(|tool| tool.approval_mode)
                    .or(server_config.default_tools_approval_mode)
                    .unwrap_or_default(),
            )
        });
    if let Some(user_configured_mode) = user_configured_mode {
        return user_configured_mode;
    }

    sess.services
        .plugins_manager
        .plugins_for_config(&turn_context.config.plugins_config_input())
        .await
        .plugins()
        .iter()
        .filter(|plugin| plugin.is_active())
        .find_map(|plugin| {
            let server_config = plugin.mcp_servers.get(server)?;
            server_config
                .tools
                .get(tool_name)
                .and_then(|tool| tool.approval_mode)
                .or(server_config.default_tools_approval_mode)
        })
        .unwrap_or_default()
}

fn build_mcp_tool_call_request_meta(
    turn_context: &TurnContext,
    server: &str,
    call_id: &str,
    metadata: Option<&McpToolApprovalMetadata>,
) -> Option<serde_json::Value> {
    let mut request_meta = serde_json::Map::new();

    if let Some(turn_metadata) = turn_context
        .turn_metadata_state
        .current_meta_value_for_mcp_request(McpTurnMetadataContext {
            model: turn_context.model_info.slug.as_str(),
            reasoning_effort: turn_context.effective_reasoning_effort(),
        })
    {
        request_meta.insert(
            crate::X_CODEX_TURN_METADATA_HEADER.to_string(),
            turn_metadata,
        );
    }

    if server == CODEX_APPS_MCP_SERVER_NAME {
        let mut codex_apps_meta = metadata
            .and_then(|metadata| metadata.codex_apps_meta.clone())
            .unwrap_or_default();
        codex_apps_meta.insert(
            "call_id".to_string(),
            serde_json::Value::String(call_id.to_string()),
        );
        request_meta.insert(
            MCP_TOOL_CODEX_APPS_META_KEY.to_string(),
            serde_json::Value::Object(codex_apps_meta),
        );
    }
    if let Some(plugin_id) = metadata.and_then(|metadata| metadata.plugin_id.as_ref()) {
        request_meta.insert(
            MCP_TOOL_PLUGIN_ID_META_KEY.to_string(),
            serde_json::Value::String(plugin_id.clone()),
        );
    }

    (!request_meta.is_empty()).then_some(serde_json::Value::Object(request_meta))
}

fn with_mcp_tool_call_thread_id_meta(
    meta: Option<serde_json::Value>,
    thread_id: &str,
) -> Option<serde_json::Value> {
    match meta {
        Some(serde_json::Value::Object(mut map)) => {
            map.insert(
                MCP_TOOL_THREAD_ID_META_KEY.to_string(),
                serde_json::Value::String(thread_id.to_string()),
            );
            Some(serde_json::Value::Object(map))
        }
        None => {
            let mut map = serde_json::Map::new();
            map.insert(
                MCP_TOOL_THREAD_ID_META_KEY.to_string(),
                serde_json::Value::String(thread_id.to_string()),
            );
            Some(serde_json::Value::Object(map))
        }
        other => other,
    }
}

#[derive(Clone, Copy)]
struct McpToolApprovalPromptOptions {
    allow_session_remember: bool,
    allow_persistent_approval: bool,
}

struct McpToolApprovalElicitationRequest<'a> {
    server: &'a str,
    metadata: Option<&'a McpToolApprovalMetadata>,
    tool_params: Option<&'a serde_json::Value>,
    tool_params_display: Option<&'a [RenderedMcpToolApprovalParam]>,
    question: RequestUserInputQuestion,
    message_override: Option<&'a str>,
    prompt_options: McpToolApprovalPromptOptions,
}

pub(crate) const MCP_TOOL_APPROVAL_QUESTION_ID_PREFIX: &str = "mcp_tool_call_approval";
pub(crate) const MCP_TOOL_APPROVAL_ACCEPT: &str = "Allow";
pub(crate) const MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION: &str = "Allow for this session";
// Internal-only token used when guardian auto-reviews delegated MCP approvals on the
// RequestUserInput compatibility path. That legacy MCP prompt has allow/cancel labels but no
// real "Decline" answer, so this lets guardian denials round-trip distinctly from user cancel.
// This is not a user-facing option.
pub(crate) const MCP_TOOL_APPROVAL_DECLINE_SYNTHETIC: &str = "__codex_mcp_decline__";
const MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER: &str = "Allow and don't ask me again";
const MCP_TOOL_APPROVAL_CANCEL: &str = "Cancel";

pub(crate) fn is_mcp_tool_approval_question_id(question_id: &str) -> bool {
    question_id
        .strip_prefix(MCP_TOOL_APPROVAL_QUESTION_ID_PREFIX)
        .is_some_and(|suffix| suffix.starts_with('_'))
}

/// 审批记忆/持久化的缓存键。codex_apps 工具必须带 `connector_id` 才能构成完整
/// 键（否则无法记住，见 `session_mcp_tool_approval_key`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct McpToolApprovalKey {
    server: String,
    connector_id: Option<String>,
    tool_name: String,
}

fn mcp_tool_approval_prompt_options(
    session_approval_key: Option<&McpToolApprovalKey>,
    persistent_approval_key: Option<&McpToolApprovalKey>,
    tool_call_mcp_elicitation_enabled: bool,
) -> McpToolApprovalPromptOptions {
    McpToolApprovalPromptOptions {
        allow_session_remember: session_approval_key.is_some(),
        allow_persistent_approval: tool_call_mcp_elicitation_enabled
            && persistent_approval_key.is_some(),
    }
}

/// MCP 工具调用的审批决策树。返回 `None` = 无需询问、直接放行；返回 `Some(决策)`
/// = 已得到（自动或用户的）决策。按以下顺序逐层判断（任一命中即返回）：
///   ① 自动放行（审批策略 + profile + 工具模式判定无需问）→ None
///   ② 工具本身不要求审批 且 模式不是 Prompt → None
///   ③ 会话级已记住 → Accept
///   ④ permission_request 钩子：Allow→Accept / Deny→Decline / None→继续
///   ⑤ 路由到 Guardian：委托 Guardian 复核
///   ⑥ ToolCallMcpElicitation 特性开 → 走 MCP elicitation 提示
///   ⑦ 降级到 RequestUserInput（兼容旧路径）
///
/// 关键：⑤⑥⑦ 三条返回前都调 `apply_mcp_tool_approval_decision` 落地决策，
/// 这是「记住」的唯一落点——漏掉它「本会话内允许」就不生效。
///
/// 副作用：可能调钩子、向 Guardian/用户发起审批，并把决策写入记忆/config。
async fn maybe_request_mcp_tool_approval(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    call_id: &str,
    invocation: &McpInvocation,
    hook_tool_name: &HookToolName,
    metadata: Option<&McpToolApprovalMetadata>,
    approval_mode: AppToolApproval,
) -> Option<McpToolApprovalDecision> {
    // ① 审批策略/profile/工具模式联合判定为「自动放行」→ 不问。
    if mcp_permission_prompt_is_auto_approved(
        turn_context.approval_policy.value(),
        &turn_context.permission_profile(),
        McpPermissionPromptAutoApproveContext {
            approvals_reviewer: Some(turn_context.config.approvals_reviewer),
            tool_approval_mode: Some(approval_mode),
        },
    ) {
        return None;
    }

    // ② 工具注释不要求审批（非破坏性/只读等）且模式不是 Prompt → 不问。
    let annotations = metadata.and_then(|metadata| metadata.annotations.as_ref());
    let approval_required = requires_mcp_tool_approval(annotations);
    if !approval_required && approval_mode != AppToolApproval::Prompt {
        return None;
    }

    // ③ 命中会话级已记住的批准（仅 Auto 模式有 key）→ 直接 Accept，不再问。
    let session_approval_key = session_mcp_tool_approval_key(invocation, metadata, approval_mode);
    let persistent_approval_key =
        persistent_mcp_tool_approval_key(invocation, metadata, approval_mode);
    if let Some(key) = session_approval_key.as_ref()
        && mcp_tool_approval_is_remembered(sess, key).await
    {
        return Some(McpToolApprovalDecision::Accept);
    }

    // ④ 跑外部 permission 钩子，钩子可直接 Allow/Deny，None 则继续后续判断。
    match run_permission_request_hooks(
        sess,
        turn_context,
        call_id,
        PermissionRequestPayload {
            tool_name: hook_tool_name.clone(),
            tool_input: invocation
                .arguments
                .clone()
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
        },
    )
    .await
    {
        Some(PermissionRequestDecision::Allow) => {
            return Some(McpToolApprovalDecision::Accept);
        }
        Some(PermissionRequestDecision::Deny { message }) => {
            return Some(McpToolApprovalDecision::Decline {
                message: Some(message),
            });
        }
        None => {}
    }

    let tool_call_mcp_elicitation_enabled = turn_context
        .config
        .features
        .enabled(Feature::ToolCallMcpElicitation);

    // ⑤ 路由到 Guardian：委托 Guardian 模型复核，翻译其决策并落地后返回。
    if routes_approval_to_guardian(turn_context) {
        let review_id = new_guardian_review_id();
        let decision = review_approval_request(
            sess,
            turn_context,
            review_id.clone(),
            build_guardian_mcp_tool_review_request(call_id, invocation, metadata),
            /*retry_reason*/ None,
        )
        .await;
        let decision = mcp_tool_approval_decision_from_guardian(sess, &review_id, decision).await;
        apply_mcp_tool_approval_decision(
            sess,
            turn_context,
            &decision,
            session_approval_key,
            persistent_approval_key,
        )
        .await;
        return Some(decision);
    }

    let prompt_options = mcp_tool_approval_prompt_options(
        session_approval_key.as_ref(),
        persistent_approval_key.as_ref(),
        tool_call_mcp_elicitation_enabled,
    );
    // ⑥/⑦ 共用：先准备审批提示。`render_*` 按 (server, connector_id, tool_title)
    // 三元组尝试匹配内置模板让提示更友好，未命中则回退到通用展示参数与问句。
    let question_id = format!("{MCP_TOOL_APPROVAL_QUESTION_ID_PREFIX}_{call_id}");
    let rendered_template = render_mcp_tool_approval_template(
        &invocation.server,
        metadata.and_then(|metadata| metadata.connector_id.as_deref()),
        metadata.and_then(|metadata| metadata.connector_name.as_deref()),
        metadata.and_then(|metadata| metadata.tool_title.as_deref()),
        invocation.arguments.as_ref(),
    );
    let tool_params_display = rendered_template
        .as_ref()
        .map(|rendered_template| rendered_template.tool_params_display.clone())
        .or_else(|| build_mcp_tool_approval_display_params(invocation.arguments.as_ref()));
    let question = build_mcp_tool_approval_question(
        question_id.clone(),
        &invocation.server,
        &invocation.tool,
        metadata.and_then(|metadata| metadata.connector_name.as_deref()),
        prompt_options,
        rendered_template
            .as_ref()
            .map(|rendered_template| rendered_template.question.as_str()),
    );
    // ⑥ 特性开启：把审批做成一次 MCP elicitation（带「记住」选项的表单），
    // 解析应答 → 按模式归一化 → 落地 → 返回。
    if tool_call_mcp_elicitation_enabled {
        let request_id = rmcp::model::RequestId::String(
            format!("{MCP_TOOL_APPROVAL_QUESTION_ID_PREFIX}_{call_id}").into(),
        );
        let params = build_mcp_tool_approval_elicitation_request(
            sess.as_ref(),
            turn_context.as_ref(),
            McpToolApprovalElicitationRequest {
                server: &invocation.server,
                metadata,
                tool_params: rendered_template
                    .as_ref()
                    .and_then(|rendered_template| rendered_template.tool_params.as_ref())
                    .or(invocation.arguments.as_ref()),
                tool_params_display: tool_params_display.as_deref(),
                question,
                message_override: rendered_template
                    .as_ref()
                    .map(|rendered_template| rendered_template.elicitation_message.as_str()),
                prompt_options,
            },
        );
        let decision = parse_mcp_tool_approval_elicitation_response(
            sess.request_mcp_server_elicitation(turn_context.as_ref(), request_id, params)
                .await
                .response,
            &question_id,
        );
        let decision = normalize_approval_decision_for_mode(decision, approval_mode);
        apply_mcp_tool_approval_decision(
            sess,
            turn_context,
            &decision,
            session_approval_key,
            persistent_approval_key,
        )
        .await;
        return Some(decision);
    }

    // ⑦ 兼容旧路径：用通用的 RequestUserInput 提问，解析应答 → 归一化 → 落地。
    let args = RequestUserInputArgs {
        questions: vec![question],
    };
    let response = sess
        .request_user_input(turn_context.as_ref(), call_id.to_string(), args)
        .await;
    let decision = normalize_approval_decision_for_mode(
        parse_mcp_tool_approval_response(response, &question_id),
        approval_mode,
    );
    apply_mcp_tool_approval_decision(
        sess,
        turn_context,
        &decision,
        session_approval_key,
        persistent_approval_key,
    )
    .await;
    Some(decision)
}

/// 计算会话级审批缓存键。返回 `None` 即「此调用无法被记住」，影响提示里是否
/// 出现「本会话内允许」选项。两个早退很关键（见 19_mcp_integration §5.3 / §9）：
///   - 仅 `AppToolApproval::Auto` 模式能记住，其它模式直接 `None`。
///   - codex_apps server 但缺 `connector_id` 时无法记住（键不完整）。
fn session_mcp_tool_approval_key(
    invocation: &McpInvocation,
    metadata: Option<&McpToolApprovalMetadata>,
    approval_mode: AppToolApproval,
) -> Option<McpToolApprovalKey> {
    if approval_mode != AppToolApproval::Auto {
        return None;
    }

    let connector_id = metadata.and_then(|metadata| metadata.connector_id.clone());
    if invocation.server == CODEX_APPS_MCP_SERVER_NAME && connector_id.is_none() {
        return None;
    }

    Some(McpToolApprovalKey {
        server: invocation.server.clone(),
        connector_id,
        tool_name: invocation.tool.clone(),
    })
}

fn persistent_mcp_tool_approval_key(
    invocation: &McpInvocation,
    metadata: Option<&McpToolApprovalMetadata>,
    approval_mode: AppToolApproval,
) -> Option<McpToolApprovalKey> {
    session_mcp_tool_approval_key(invocation, metadata, approval_mode)
}

pub(crate) fn build_guardian_mcp_tool_review_request(
    call_id: &str,
    invocation: &McpInvocation,
    metadata: Option<&McpToolApprovalMetadata>,
) -> GuardianApprovalRequest {
    GuardianApprovalRequest::McpToolCall {
        id: call_id.to_string(),
        server: invocation.server.clone(),
        tool_name: invocation.tool.clone(),
        arguments: invocation.arguments.clone(),
        connector_id: metadata.and_then(|metadata| metadata.connector_id.clone()),
        connector_name: metadata.and_then(|metadata| metadata.connector_name.clone()),
        connector_description: metadata.and_then(|metadata| metadata.connector_description.clone()),
        tool_title: metadata.and_then(|metadata| metadata.tool_title.clone()),
        tool_description: metadata.and_then(|metadata| metadata.tool_description.clone()),
        annotations: metadata
            .and_then(|metadata| metadata.annotations.as_ref())
            .map(|annotations| GuardianMcpAnnotations {
                destructive_hint: annotations.destructive_hint,
                open_world_hint: annotations.open_world_hint,
                read_only_hint: annotations.read_only_hint,
            }),
    }
}

async fn mcp_tool_approval_decision_from_guardian(
    sess: &Session,
    review_id: &str,
    decision: ReviewDecision,
) -> McpToolApprovalDecision {
    match decision {
        ReviewDecision::Approved
        | ReviewDecision::ApprovedExecpolicyAmendment { .. }
        | ReviewDecision::NetworkPolicyAmendment { .. } => McpToolApprovalDecision::Accept,
        ReviewDecision::ApprovedForSession => McpToolApprovalDecision::AcceptForSession,
        ReviewDecision::Denied => McpToolApprovalDecision::Decline {
            message: Some(guardian_rejection_message(sess, review_id).await),
        },
        ReviewDecision::TimedOut => McpToolApprovalDecision::Decline {
            message: Some(guardian_timeout_message()),
        },
        ReviewDecision::Abort => McpToolApprovalDecision::Decline { message: None },
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "MCP approval metadata reads through the session-owned manager guard"
)]
/// 从连接管理器聚合某工具的全部审批/展示元数据。取管理器读锁、列出全部工具、
/// 按 (server, tool_name) 找到目标后组装 `McpToolApprovalMetadata`；codex_apps
/// 工具会额外查连接器描述。工具不存在则返回 `None`。
pub(crate) async fn lookup_mcp_tool_metadata(
    sess: &Session,
    turn_context: &TurnContext,
    server: &str,
    tool_name: &str,
) -> Option<McpToolApprovalMetadata> {
    let manager = sess.services.mcp_connection_manager.read().await;
    let plugin_id = manager
        .plugin_id_for_mcp_server_name(server)
        .map(str::to_string);
    let tools = manager.list_all_tools().await;
    let tool_info = tools
        .into_iter()
        .find(|tool_info| tool_info.server_name == server && tool_info.tool.name == tool_name)?;
    let connector_description = if server == CODEX_APPS_MCP_SERVER_NAME {
        let connectors = match connectors::list_cached_accessible_connectors_from_mcp_tools(
            turn_context.config.as_ref(),
        )
        .await
        {
            Some(connectors) => Some(connectors),
            None => {
                connectors::list_accessible_connectors_from_mcp_tools(turn_context.config.as_ref())
                    .await
                    .ok()
            }
        };
        connectors.and_then(|connectors| {
            let connector_id = tool_info.connector_id.as_deref()?;
            connectors
                .into_iter()
                .find(|connector| connector.id == connector_id)
                .and_then(|connector| connector.description)
        })
    } else {
        None
    };

    Some(McpToolApprovalMetadata {
        annotations: tool_info.tool.annotations,
        connector_id: tool_info.connector_id,
        connector_name: tool_info.connector_name,
        connector_description,
        plugin_id,
        tool_title: tool_info.tool.title,
        tool_description: tool_info.tool.description.map(std::borrow::Cow::into_owned),
        mcp_app_resource_uri: get_mcp_app_resource_uri(tool_info.tool.meta.as_deref()),
        codex_apps_meta: tool_info
            .tool
            .meta
            .as_ref()
            .and_then(|meta| meta.get(MCP_TOOL_CODEX_APPS_META_KEY))
            .and_then(serde_json::Value::as_object)
            .cloned(),
        // Disallow custom MCPs from uploading files via fileParams.
        // 禁止自定义 MCP 通过 fileParams 上传文件——文件上传是 codex_apps 专属
        // 能力（见 `openai_file_input_params_for_server`）。
        openai_file_input_params: openai_file_input_params_for_server(
            server,
            tool_info.tool.meta.as_deref(),
        ),
    })
}

fn openai_file_input_params_for_server(
    server: &str,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<Vec<String>> {
    (server == CODEX_APPS_MCP_SERVER_NAME)
        .then_some(declared_openai_file_input_param_names(meta))
        .filter(|params| !params.is_empty())
}

fn get_mcp_app_resource_uri(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    meta.and_then(|meta| {
        meta.get("ui")
            .and_then(serde_json::Value::as_object)
            .and_then(|ui| ui.get("resourceUri"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                meta.get(MCP_TOOL_UI_RESOURCE_URI_META_KEY)
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                meta.get(MCP_TOOL_OPENAI_OUTPUT_TEMPLATE_META_KEY)
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string)
    })
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "MCP app metadata reads through the session-owned manager guard"
)]
async fn lookup_mcp_app_usage_metadata(
    sess: &Session,
    server: &str,
    tool_name: &str,
) -> Option<McpAppUsageMetadata> {
    let tools = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .list_all_tools()
        .await;

    tools.into_iter().find_map(|tool_info| {
        if tool_info.server_name == server && tool_info.tool.name == tool_name {
            Some(McpAppUsageMetadata {
                connector_id: tool_info.connector_id,
                app_name: tool_info.connector_name,
            })
        } else {
            None
        }
    })
}

fn build_mcp_tool_approval_question(
    question_id: String,
    server: &str,
    tool_name: &str,
    connector_name: Option<&str>,
    prompt_options: McpToolApprovalPromptOptions,
    question_override: Option<&str>,
) -> RequestUserInputQuestion {
    let question = question_override
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            build_mcp_tool_approval_fallback_message(server, tool_name, connector_name)
        });
    let question = format!("{}?", question.trim_end_matches('?'));

    let mut options = vec![RequestUserInputQuestionOption {
        label: MCP_TOOL_APPROVAL_ACCEPT.to_string(),
        description: "Run the tool and continue.".to_string(),
    }];
    if prompt_options.allow_session_remember {
        options.push(RequestUserInputQuestionOption {
            label: MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION.to_string(),
            description: "Run the tool and remember this choice for this session.".to_string(),
        });
    }
    if prompt_options.allow_persistent_approval {
        options.push(RequestUserInputQuestionOption {
            label: MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER.to_string(),
            description: "Run the tool and remember this choice for future tool calls.".to_string(),
        });
    }
    options.push(RequestUserInputQuestionOption {
        label: MCP_TOOL_APPROVAL_CANCEL.to_string(),
        description: "Cancel this tool call.".to_string(),
    });

    RequestUserInputQuestion {
        id: question_id,
        header: "Approve app tool call?".to_string(),
        question,
        is_other: false,
        is_secret: false,
        options: Some(options),
    }
}

fn build_mcp_tool_approval_fallback_message(
    server: &str,
    tool_name: &str,
    connector_name: Option<&str>,
) -> String {
    let actor = connector_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            if server == CODEX_APPS_MCP_SERVER_NAME {
                "this app".to_string()
            } else {
                format!("the {server} MCP server")
            }
        });
    format!("Allow {actor} to run tool \"{tool_name}\"?")
}

fn build_mcp_tool_approval_elicitation_request(
    sess: &Session,
    turn_context: &TurnContext,
    request: McpToolApprovalElicitationRequest<'_>,
) -> McpServerElicitationRequestParams {
    let message = request
        .message_override
        .map(ToString::to_string)
        .unwrap_or_else(|| request.question.question.clone());

    McpServerElicitationRequestParams {
        thread_id: sess.conversation_id.to_string(),
        turn_id: Some(turn_context.sub_id.clone()),
        server_name: request.server.to_string(),
        request: McpServerElicitationRequest::Form {
            meta: build_mcp_tool_approval_elicitation_meta(
                request.server,
                request.metadata,
                request.tool_params,
                request.tool_params_display,
                request.prompt_options,
            ),
            message,
            requested_schema: McpElicitationSchema {
                schema_uri: None,
                type_: McpElicitationObjectType::Object,
                properties: BTreeMap::new(),
                required: None,
            },
        },
    }
}

fn build_mcp_tool_approval_elicitation_meta(
    server: &str,
    metadata: Option<&McpToolApprovalMetadata>,
    tool_params: Option<&serde_json::Value>,
    tool_params_display: Option<&[RenderedMcpToolApprovalParam]>,
    prompt_options: McpToolApprovalPromptOptions,
) -> Option<serde_json::Value> {
    let mut meta = serde_json::Map::new();
    meta.insert(
        MCP_TOOL_APPROVAL_KIND_KEY.to_string(),
        serde_json::Value::String(MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL.to_string()),
    );
    match (
        prompt_options.allow_session_remember,
        prompt_options.allow_persistent_approval,
    ) {
        (true, true) => {
            meta.insert(
                MCP_TOOL_APPROVAL_PERSIST_KEY.to_string(),
                serde_json::json!([
                    MCP_TOOL_APPROVAL_PERSIST_SESSION,
                    MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
                ]),
            );
        }
        (true, false) => {
            meta.insert(
                MCP_TOOL_APPROVAL_PERSIST_KEY.to_string(),
                serde_json::Value::String(MCP_TOOL_APPROVAL_PERSIST_SESSION.to_string()),
            );
        }
        (false, true) => {
            meta.insert(
                MCP_TOOL_APPROVAL_PERSIST_KEY.to_string(),
                serde_json::Value::String(MCP_TOOL_APPROVAL_PERSIST_ALWAYS.to_string()),
            );
        }
        (false, false) => {}
    }
    if let Some(metadata) = metadata {
        if let Some(tool_title) = metadata.tool_title.as_ref() {
            meta.insert(
                MCP_TOOL_APPROVAL_TOOL_TITLE_KEY.to_string(),
                serde_json::Value::String(tool_title.clone()),
            );
        }
        if let Some(tool_description) = metadata.tool_description.as_ref() {
            meta.insert(
                MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY.to_string(),
                serde_json::Value::String(tool_description.clone()),
            );
        }
        if server == CODEX_APPS_MCP_SERVER_NAME
            && (metadata.connector_id.is_some()
                || metadata.connector_name.is_some()
                || metadata.connector_description.is_some())
        {
            meta.insert(
                MCP_TOOL_APPROVAL_SOURCE_KEY.to_string(),
                serde_json::Value::String(MCP_TOOL_APPROVAL_SOURCE_CONNECTOR.to_string()),
            );
            if let Some(connector_id) = metadata.connector_id.as_deref() {
                meta.insert(
                    MCP_TOOL_APPROVAL_CONNECTOR_ID_KEY.to_string(),
                    serde_json::Value::String(connector_id.to_string()),
                );
            }
            if let Some(connector_name) = metadata.connector_name.as_ref() {
                meta.insert(
                    MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY.to_string(),
                    serde_json::Value::String(connector_name.clone()),
                );
            }
            if let Some(connector_description) = metadata.connector_description.as_ref() {
                meta.insert(
                    MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY.to_string(),
                    serde_json::Value::String(connector_description.clone()),
                );
            }
        }
    }
    if let Some(tool_params) = tool_params {
        meta.insert(
            MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY.to_string(),
            tool_params.clone(),
        );
    }
    if let Some(tool_params_display) = tool_params_display
        && let Ok(tool_params_display) = serde_json::to_value(tool_params_display)
    {
        meta.insert(
            MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY.to_string(),
            tool_params_display,
        );
    }
    (!meta.is_empty()).then_some(serde_json::Value::Object(meta))
}

fn build_mcp_tool_approval_display_params(
    tool_params: Option<&serde_json::Value>,
) -> Option<Vec<crate::mcp_tool_approval_templates::RenderedMcpToolApprovalParam>> {
    let tool_params = tool_params?.as_object()?;
    let mut display_params = tool_params
        .iter()
        .map(
            |(name, value)| crate::mcp_tool_approval_templates::RenderedMcpToolApprovalParam {
                name: name.clone(),
                value: value.clone(),
                display_name: name.clone(),
            },
        )
        .collect::<Vec<_>>();
    display_params.sort_by(|left, right| left.name.cmp(&right.name));
    Some(display_params)
}

/// 把 elicitation 表单应答解析成审批决策。优先读 meta 里的 persist 字段区分
/// 「本会话 / 以后都允许」；否则回退到按选项标签解析（复用旧的
/// `parse_mcp_tool_approval_response`），其中「Accept 动作 + 解析出 Cancel」
/// 视为普通 Accept。无应答（通道关闭）= Cancel。
fn parse_mcp_tool_approval_elicitation_response(
    response: Option<ElicitationResponse>,
    question_id: &str,
) -> McpToolApprovalDecision {
    let Some(response) = response else {
        return McpToolApprovalDecision::Cancel;
    };
    match response.action {
        ElicitationAction::Accept => {
            match response
                .meta
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .and_then(|meta| meta.get(MCP_TOOL_APPROVAL_PERSIST_KEY))
                .and_then(serde_json::Value::as_str)
            {
                Some(MCP_TOOL_APPROVAL_PERSIST_SESSION) => {
                    return McpToolApprovalDecision::AcceptForSession;
                }
                Some(MCP_TOOL_APPROVAL_PERSIST_ALWAYS) => {
                    return McpToolApprovalDecision::AcceptAndRemember;
                }
                _ => {}
            }

            match parse_mcp_tool_approval_response(
                request_user_input_response_from_elicitation_content(response.content),
                question_id,
            ) {
                McpToolApprovalDecision::Cancel => McpToolApprovalDecision::Accept,
                decision => decision,
            }
        }
        ElicitationAction::Decline => McpToolApprovalDecision::Decline { message: None },
        ElicitationAction::Cancel => McpToolApprovalDecision::Cancel,
    }
}

fn request_user_input_response_from_elicitation_content(
    content: Option<serde_json::Value>,
) -> Option<RequestUserInputResponse> {
    let Some(content) = content else {
        return Some(RequestUserInputResponse {
            answers: std::collections::HashMap::new(),
        });
    };
    let content = content.as_object()?;
    let answers = content
        .iter()
        .filter_map(|(question_id, value)| {
            let answers = match value {
                serde_json::Value::String(answer) => vec![answer.clone()],
                serde_json::Value::Array(values) => values
                    .iter()
                    .filter_map(|value| value.as_str().map(ToString::to_string))
                    .collect(),
                _ => return None,
            };
            Some((question_id.clone(), RequestUserInputAnswer { answers }))
        })
        .collect();

    Some(RequestUserInputResponse { answers })
}

fn parse_mcp_tool_approval_response(
    response: Option<RequestUserInputResponse>,
    question_id: &str,
) -> McpToolApprovalDecision {
    let Some(response) = response else {
        return McpToolApprovalDecision::Cancel;
    };
    let answers = response
        .answers
        .get(question_id)
        .map(|answer| answer.answers.as_slice());
    let Some(answers) = answers else {
        return McpToolApprovalDecision::Cancel;
    };
    if answers
        .iter()
        .any(|answer| answer == MCP_TOOL_APPROVAL_DECLINE_SYNTHETIC)
    {
        McpToolApprovalDecision::Decline { message: None }
    } else if answers
        .iter()
        .any(|answer| answer == MCP_TOOL_APPROVAL_ACCEPT_FOR_SESSION)
    {
        McpToolApprovalDecision::AcceptForSession
    } else if answers
        .iter()
        .any(|answer| answer == MCP_TOOL_APPROVAL_ACCEPT_AND_REMEMBER)
    {
        McpToolApprovalDecision::AcceptAndRemember
    } else if answers
        .iter()
        .any(|answer| answer == MCP_TOOL_APPROVAL_ACCEPT)
    {
        McpToolApprovalDecision::Accept
    } else {
        McpToolApprovalDecision::Cancel
    }
}

// Prompt 模式下没有「记住」语义，故把误传回的 AcceptForSession/AcceptAndRemember
// 收敛为一次性 Accept，避免被错误持久化。
fn normalize_approval_decision_for_mode(
    decision: McpToolApprovalDecision,
    approval_mode: AppToolApproval,
) -> McpToolApprovalDecision {
    if approval_mode == AppToolApproval::Prompt
        && matches!(
            decision,
            McpToolApprovalDecision::AcceptForSession | McpToolApprovalDecision::AcceptAndRemember
        )
    {
        McpToolApprovalDecision::Accept
    } else {
        decision
    }
}

// 查会话级记忆是否命中。只认 `ApprovedForSession`——其它决策不视为已记住。
async fn mcp_tool_approval_is_remembered(sess: &Session, key: &McpToolApprovalKey) -> bool {
    let store = sess.services.tool_approvals.lock().await;
    matches!(store.get(key), Some(ReviewDecision::ApprovedForSession))
}

async fn remember_mcp_tool_approval(sess: &Session, key: McpToolApprovalKey) {
    let mut store = sess.services.tool_approvals.lock().await;
    store.put(key, ReviewDecision::ApprovedForSession);
}

/// 把审批决策落地（「记住」的唯一落点）。
/// - `AcceptForSession` → 写会话级记忆（有 session key 时）。
/// - `AcceptAndRemember` → 尝试持久化到 config；缺 persistent key 时降级为
///   会话级记忆。
/// - `Accept` / `Decline` / `Cancel` → 不记。
async fn apply_mcp_tool_approval_decision(
    sess: &Session,
    turn_context: &TurnContext,
    decision: &McpToolApprovalDecision,
    session_approval_key: Option<McpToolApprovalKey>,
    persistent_approval_key: Option<McpToolApprovalKey>,
) {
    match decision {
        McpToolApprovalDecision::AcceptForSession => {
            if let Some(key) = session_approval_key {
                remember_mcp_tool_approval(sess, key).await;
            }
        }
        McpToolApprovalDecision::AcceptAndRemember => {
            if let Some(key) = persistent_approval_key {
                maybe_persist_mcp_tool_approval(sess, turn_context, key).await;
            } else if let Some(key) = session_approval_key {
                remember_mcp_tool_approval(sess, key).await;
            }
        }
        McpToolApprovalDecision::Accept
        | McpToolApprovalDecision::Decline { .. }
        | McpToolApprovalDecision::Cancel => {}
    }
}

/// 把审批持久化到 config（写 `approval_mode = "approve"`）。codex_apps 走
/// `persist_codex_app_tool_approval`（需 connector_id，缺则降级会话级），普通
/// MCP 走 `persist_non_app_mcp_tool_approval`。持久化失败也降级为会话级记忆，
/// 不让本次批准白点。成功后 `reload_user_config_layer` 让新配置即时生效。
async fn maybe_persist_mcp_tool_approval(
    sess: &Session,
    turn_context: &TurnContext,
    key: McpToolApprovalKey,
) {
    let tool_name = key.tool_name.clone();

    let persist_result = if key.server == CODEX_APPS_MCP_SERVER_NAME {
        // codex_apps 缺 connector_id 无法写 config 路径 → 退回会话级记忆。
        let Some(connector_id) = key.connector_id.clone() else {
            remember_mcp_tool_approval(sess, key).await;
            return;
        };
        persist_codex_app_tool_approval(&turn_context.config, &connector_id, &tool_name).await
    } else {
        persist_non_app_mcp_tool_approval(sess, &turn_context.config, &key.server, &tool_name).await
    };

    if let Err(err) = persist_result {
        error!(
            error = %err,
            server = key.server,
            tool_name,
            "failed to persist MCP tool approval"
        );
        remember_mcp_tool_approval(sess, key).await;
        return;
    }

    sess.reload_user_config_layer().await;
    remember_mcp_tool_approval(sess, key).await;
}

async fn persist_codex_app_tool_approval(
    config: &Config,
    connector_id: &str,
    tool_name: &str,
) -> anyhow::Result<()> {
    ConfigEditsBuilder::for_config(config)
        .with_edits([ConfigEdit::SetPath {
            segments: vec![
                "apps".to_string(),
                connector_id.to_string(),
                "tools".to_string(),
                tool_name.to_string(),
                "approval_mode".to_string(),
            ],
            value: value("approve"),
        }])
        .apply()
        .await
}

#[cfg(test)]
async fn persist_custom_mcp_tool_approval(
    config: &Config,
    server: &str,
    tool_name: &str,
) -> anyhow::Result<()> {
    let Some(config_edits_builder) = custom_mcp_tool_approval_config_builder(config, server)?
    else {
        anyhow::bail!("MCP server `{server}` is not configured in config.toml");
    };

    persist_custom_mcp_tool_approval_with(config_edits_builder, server, tool_name).await
}

async fn persist_non_app_mcp_tool_approval(
    sess: &Session,
    config: &Config,
    server: &str,
    tool_name: &str,
) -> anyhow::Result<()> {
    if let Some(config_edits_builder) = custom_mcp_tool_approval_config_builder(config, server)? {
        return persist_custom_mcp_tool_approval_with(config_edits_builder, server, tool_name)
            .await;
    }

    let plugin_config_name = sess
        .services
        .plugins_manager
        .plugins_for_config(&config.plugins_config_input())
        .await
        .plugins()
        .iter()
        .filter(|plugin| plugin.is_active())
        .find(|plugin| plugin.mcp_servers.contains_key(server))
        .map(|plugin| plugin.config_name.clone());

    if let Some(plugin_config_name) = plugin_config_name {
        return ConfigEditsBuilder::for_config(config)
            .with_edits([ConfigEdit::SetPath {
                segments: vec![
                    "plugins".to_string(),
                    plugin_config_name,
                    "mcp_servers".to_string(),
                    server.to_string(),
                    "tools".to_string(),
                    tool_name.to_string(),
                    "approval_mode".to_string(),
                ],
                value: value("approve"),
            }])
            .apply()
            .await;
    }

    anyhow::bail!("MCP server `{server}` is not configured in config.toml or an enabled plugin")
}

fn custom_mcp_tool_approval_config_builder(
    config: &Config,
    server: &str,
) -> anyhow::Result<Option<ConfigEditsBuilder>> {
    if let Some(project_config_folder) = project_mcp_tool_approval_config_folder(config, server) {
        return Ok(Some(ConfigEditsBuilder::new(&project_config_folder)));
    }

    Ok(user_mcp_server_is_configured(config, server)?
        .then(|| ConfigEditsBuilder::for_config(config)))
}

async fn persist_custom_mcp_tool_approval_with(
    config_edits_builder: ConfigEditsBuilder,
    server: &str,
    tool_name: &str,
) -> anyhow::Result<()> {
    config_edits_builder
        .with_edits([ConfigEdit::SetPath {
            segments: vec![
                "mcp_servers".to_string(),
                server.to_string(),
                "tools".to_string(),
                tool_name.to_string(),
                "approval_mode".to_string(),
            ],
            value: value("approve"),
        }])
        .apply()
        .await
}

fn user_mcp_server_is_configured(config: &Config, server: &str) -> anyhow::Result<bool> {
    let Some(mcp_servers_toml) = config
        .config_layer_stack
        .effective_user_config()
        .as_ref()
        .and_then(|user_config| user_config.get("mcp_servers"))
        .cloned()
    else {
        return Ok(false);
    };
    let servers =
        HashMap::<String, codex_config::types::McpServerConfig>::deserialize(mcp_servers_toml)?;
    Ok(servers.contains_key(server))
}

fn project_mcp_tool_approval_config_folder(
    config: &Config,
    server: &str,
) -> Option<AbsolutePathBuf> {
    config
        .config_layer_stack
        .layers_high_to_low()
        .into_iter()
        .find_map(|layer| {
            if !matches!(layer.name, ConfigLayerSource::Project { .. }) {
                return None;
            }

            let servers = layer
                .config
                .as_table()
                .and_then(|table| table.get("mcp_servers"))
                .cloned()
                .and_then(|value| {
                    HashMap::<String, codex_config::types::McpServerConfig>::deserialize(value).ok()
                })?;
            if servers.contains_key(server) {
                layer.config_folder()
            } else {
                None
            }
        })
}

/// 仅凭工具注释（hint）判断是否「需要审批」。优先级：明确破坏性 → 必须审批；
/// 明确只读 → 不需；否则按「破坏性默认 true 或 开放世界默认 true」从严处理
/// （缺省 hint 时倾向要求审批，安全优先）。
fn requires_mcp_tool_approval(annotations: Option<&ToolAnnotations>) -> bool {
    let destructive_hint = annotations.and_then(|annotations| annotations.destructive_hint);
    if destructive_hint == Some(true) {
        return true;
    }

    let read_only_hint = annotations
        .and_then(|annotations| annotations.read_only_hint)
        .unwrap_or(false);
    if read_only_hint {
        return false;
    }

    destructive_hint.unwrap_or(true)
        || annotations
            .and_then(|annotations| annotations.open_world_hint)
            .unwrap_or(true)
}

async fn notify_mcp_tool_call_skip(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
    message: String,
    already_started: bool,
) -> Result<CallToolResult, String> {
    if !already_started {
        notify_mcp_tool_call_started(
            sess,
            turn_context,
            call_id,
            invocation.clone(),
            item_metadata.clone(),
        )
        .await;
    }

    notify_mcp_tool_call_completed(
        sess,
        turn_context,
        call_id,
        invocation,
        item_metadata,
        Duration::ZERO,
        truncate_mcp_tool_result_for_event(&Err(message.clone())),
    )
    .await;
    Err(message)
}

#[cfg(test)]
#[path = "mcp_tool_call_tests.rs"]
mod tests;
