//! Auth elicitation helpers.
//!
//! This module owns protocol-neutral auth elicitation parsing and payload shaping.
//! Session orchestration stays in `codex-core`.
//!
//! 【文件职责】处理「connector 鉴权失败 → 引导用户重新授权」的纯逻辑：
//!   从工具调用结果里解析出鉴权失败信息，并据此构造一条 elicitation（征询用户去
//!   完成登录/重连）的载荷。本模块只做「解析 + 组装载荷」，不涉及会话编排。
//!
//! 【架构位置】
//!   层级：工具执行层（auth elicitation 的「数据塑形」层，协议中立）
//!   上游：`codex-core`（拿到工具结果后调用这里解析、再驱动实际的 elicitation 流程）
//!   下游：仅依赖 `codex_protocol::mcp::CallToolResult` 等数据类型，无 I/O
//!
//! 【数据流】
//!   `CallToolResult`（含失败 meta）→ `connector_auth_failure_from_tool_result()`
//!   解析出 `CodexAppsConnectorAuthFailure` → `build_auth_elicitation()` 组装成
//!   带 message/url/meta 的 `CodexAppsAuthElicitation`。
//!
//! 【阅读建议】先看 `connector_auth_failure_from_tool_result`（解析入口、含信任校验），
//!   再看 `build_auth_elicitation`（载荷组装）；顶部一组 `const` 是 meta 的键名约定。

use codex_protocol::mcp::CallToolResult;
use serde::Serialize;

// 以下一组常量是工具结果 `meta` 里「connector 鉴权失败」信息的键名约定。
// 嵌套结构形如：meta[_codex_apps][connector_auth_failure][is_auth_failure / auth_reason / ...]。
// 解析（`connector_auth_failure_from_tool_result`）与回写（`build_auth_elicitation`）
// 共用这套键名，故抽成常量保持两端一致。
pub const MCP_TOOL_CODEX_APPS_META_KEY: &str = "_codex_apps";
pub const CONNECTOR_AUTH_FAILURE_META_KEY: &str = "connector_auth_failure";
pub const CONNECTOR_AUTH_FAILURE_IS_AUTH_FAILURE_KEY: &str = "is_auth_failure";
pub const CONNECTOR_AUTH_FAILURE_AUTH_REASON_KEY: &str = "auth_reason";
pub const CONNECTOR_AUTH_FAILURE_CONNECTOR_ID_KEY: &str = "connector_id";
pub const CONNECTOR_AUTH_FAILURE_LINK_ID_KEY: &str = "link_id";
pub const CONNECTOR_AUTH_FAILURE_ERROR_CODE_KEY: &str = "error_code";
pub const CONNECTOR_AUTH_FAILURE_ERROR_HTTP_STATUS_CODE_KEY: &str = "error_http_status_code";
pub const CONNECTOR_AUTH_FAILURE_ERROR_ACTION_KEY: &str = "error_action";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAppsConnectorAuthFailure {
    pub connector_id: String,
    pub connector_name: String,
    pub install_url: String,
    pub auth_reason: Option<String>,
    pub link_id: Option<String>,
    pub error_code: Option<String>,
    pub error_http_status_code: Option<i64>,
    pub error_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexAppsAuthElicitation {
    pub meta: serde_json::Value,
    pub message: String,
    pub url: String,
    pub elicitation_id: String,
}

/// 「解析结果 + 待发起的 elicitation」打包：上层既需要原始失败信息（用于后续
/// 重试/上报），也需要现成的 elicitation 载荷，故一并返回省得重算。
#[derive(Debug, Clone, PartialEq)]
pub struct CodexAppsAuthElicitationPlan {
    pub auth_failure: CodexAppsConnectorAuthFailure,
    pub elicitation: CodexAppsAuthElicitation,
}

#[derive(Serialize)]
struct CodexAppsConnectorAuthFailureMeta<'a> {
    is_auth_failure: bool,
    connector_id: &'a str,
    connector_name: &'a str,
    install_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_http_status_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_action: Option<&'a str>,
}

/// 尝试从工具调用结果中解析出「connector 鉴权失败」信息；非鉴权失败则返回 None。
///
/// 解析需通过一系列校验，任一不满足都返回 None（用 `?` 短路）：
///   - 结果必须 `is_error == true`；
///   - meta 中必须存在 `_codex_apps.connector_auth_failure` 且 `is_auth_failure == true`；
///   - 调用方必须提供非空 `connector_id`，且若 meta 里也带了 connector_id，二者必须一致
///     （信任校验：防止把 A 连接器的失败错配到 B 连接器的授权引导上）。
/// `connector_name` 缺省时回退用 `connector_id`；`install_url` 为 None 时整体失败返回 None。
pub fn connector_auth_failure_from_tool_result(
    result: &CallToolResult,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
    install_url: Option<String>,
) -> Option<CodexAppsConnectorAuthFailure> {
    // Step 1：只有标记为错误的结果才可能是鉴权失败。
    if result.is_error != Some(true) {
        return None;
    }

    // Step 2：按约定键路径逐层下钻到 connector_auth_failure 对象。
    let auth_failure = result
        .meta
        .as_ref()?
        .as_object()?
        .get(MCP_TOOL_CODEX_APPS_META_KEY)?
        .as_object()?
        .get(CONNECTOR_AUTH_FAILURE_META_KEY)?
        .as_object()?;
    // 必须显式标记 is_auth_failure=true，否则视为普通错误、不触发授权引导。
    if auth_failure
        .get(CONNECTOR_AUTH_FAILURE_IS_AUTH_FAILURE_KEY)
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }

    // Step 3：信任校验——调用方须给出 connector_id；若 meta 也带了 id 则必须吻合。
    let connector_id = connector_id
        .map(str::trim)
        .filter(|connector_id| !connector_id.is_empty())?;
    if let Some(auth_failure_connector_id) =
        string_auth_failure_field(auth_failure, CONNECTOR_AUTH_FAILURE_CONNECTOR_ID_KEY)
        && auth_failure_connector_id != connector_id
    {
        return None;
    }
    let connector_name = connector_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(connector_id)
        .to_string();

    Some(CodexAppsConnectorAuthFailure {
        connector_id: connector_id.to_string(),
        connector_name,
        install_url: install_url?,
        auth_reason: string_auth_failure_field(
            auth_failure,
            CONNECTOR_AUTH_FAILURE_AUTH_REASON_KEY,
        ),
        link_id: string_auth_failure_field(auth_failure, CONNECTOR_AUTH_FAILURE_LINK_ID_KEY),
        error_code: string_auth_failure_field(auth_failure, CONNECTOR_AUTH_FAILURE_ERROR_CODE_KEY),
        error_http_status_code: auth_failure
            .get(CONNECTOR_AUTH_FAILURE_ERROR_HTTP_STATUS_CODE_KEY)
            .and_then(serde_json::Value::as_i64),
        error_action: string_auth_failure_field(
            auth_failure,
            CONNECTOR_AUTH_FAILURE_ERROR_ACTION_KEY,
        ),
    })
}

pub fn build_auth_elicitation_plan(
    call_id: &str,
    result: &CallToolResult,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
    install_url: Option<String>,
) -> Option<CodexAppsAuthElicitationPlan> {
    let auth_failure =
        connector_auth_failure_from_tool_result(result, connector_id, connector_name, install_url)?;
    let elicitation = build_auth_elicitation(call_id, &auth_failure);
    Some(CodexAppsAuthElicitationPlan {
        auth_failure,
        elicitation,
    })
}

/// 由鉴权失败信息组装一条 elicitation 载荷：把失败字段回写进 `_codex_apps.
/// connector_auth_failure` meta、生成面向用户的引导文案、附上安装/重连 URL，
/// 并基于 `call_id` 派生稳定的 elicitation id。
pub fn build_auth_elicitation(
    call_id: &str,
    auth_failure: &CodexAppsConnectorAuthFailure,
) -> CodexAppsAuthElicitation {
    CodexAppsAuthElicitation {
        meta: serde_json::json!({
            MCP_TOOL_CODEX_APPS_META_KEY: {
                CONNECTOR_AUTH_FAILURE_META_KEY: CodexAppsConnectorAuthFailureMeta {
                    is_auth_failure: true,
                    connector_id: &auth_failure.connector_id,
                    connector_name: &auth_failure.connector_name,
                    install_url: &auth_failure.install_url,
                    auth_reason: auth_failure.auth_reason.as_deref(),
                    link_id: auth_failure.link_id.as_deref(),
                    error_code: auth_failure.error_code.as_deref(),
                    error_http_status_code: auth_failure.error_http_status_code,
                    error_action: auth_failure.error_action.as_deref(),
                },
            },
        }),
        message: auth_elicitation_message(auth_failure),
        url: auth_failure.install_url.clone(),
        elicitation_id: auth_elicitation_id(call_id),
    }
}

/// 用户完成授权后，构造回灌给模型的工具结果：告知「授权已完成，请重试本次调用」。
///
/// 注意 `is_error: Some(true)` 是「有意为之」：本次原始工具调用并未真正成功，
/// 仍需模型主动重试一次；用 error 标记可促使模型把它当作待重试而非已完成，
/// 避免误以为操作已生效。
pub fn auth_elicitation_completed_result(
    auth_failure: &CodexAppsConnectorAuthFailure,
    meta: Option<serde_json::Value>,
) -> CallToolResult {
    CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": format!(
                "Authentication for {} was requested and accepted. Retry this tool call now.",
                auth_failure.connector_name
            ),
        })],
        structured_content: None,
        is_error: Some(true),
        meta,
    }
}

pub fn auth_elicitation_id(call_id: &str) -> String {
    format!("codex_apps_auth_{call_id}")
}

fn string_auth_failure_field(
    auth_failure: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    auth_failure
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

// 按 `auth_reason` 给出贴合场景的用户引导文案：
//   oauth_upgrade_required → 重新连接以授予新权限；
//   reauthentication_required → 重新连接以恢复访问；
//   missing_link → 首次登录该连接器；
//   其他/缺省 → 通用「去 ChatGPT 登录后继续」。
fn auth_elicitation_message(auth_failure: &CodexAppsConnectorAuthFailure) -> String {
    match auth_failure.auth_reason.as_deref() {
        Some("oauth_upgrade_required") => format!(
            "Reconnect {} on ChatGPT to grant the permissions needed for this request.",
            auth_failure.connector_name
        ),
        Some("reauthentication_required") => format!(
            "Reconnect {} on ChatGPT to restore access for this request.",
            auth_failure.connector_name
        ),
        Some("missing_link") => format!(
            "Sign in to {} on ChatGPT to use it in Codex.",
            auth_failure.connector_name
        ),
        _ => format!(
            "Sign in to {} on ChatGPT to continue.",
            auth_failure.connector_name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn auth_failure_result() -> CallToolResult {
        CallToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "Connector reauthentication required",
            })],
            structured_content: None,
            is_error: Some(true),
            meta: Some(serde_json::json!({
                MCP_TOOL_CODEX_APPS_META_KEY: {
                    CONNECTOR_AUTH_FAILURE_META_KEY: {
                        CONNECTOR_AUTH_FAILURE_IS_AUTH_FAILURE_KEY: true,
                        CONNECTOR_AUTH_FAILURE_AUTH_REASON_KEY: "reauthentication_required",
                        CONNECTOR_AUTH_FAILURE_CONNECTOR_ID_KEY: "connector_calendar",
                        "connector_name": "Untrusted Calendar",
                        CONNECTOR_AUTH_FAILURE_LINK_ID_KEY: "link_123",
                        CONNECTOR_AUTH_FAILURE_ERROR_CODE_KEY: "UNAUTHORIZED",
                        CONNECTOR_AUTH_FAILURE_ERROR_HTTP_STATUS_CODE_KEY: 401,
                        CONNECTOR_AUTH_FAILURE_ERROR_ACTION_KEY: "TRIGGER_REAUTHENTICATION",
                    },
                },
            })),
        }
    }

    #[test]
    fn parses_auth_failure_from_trusted_connector_metadata() {
        assert_eq!(
            connector_auth_failure_from_tool_result(
                &auth_failure_result(),
                Some("connector_calendar"),
                Some("Google Calendar"),
                Some("https://chatgpt.com/apps/google-calendar/connector_calendar".to_string()),
            ),
            Some(CodexAppsConnectorAuthFailure {
                connector_id: "connector_calendar".to_string(),
                connector_name: "Google Calendar".to_string(),
                install_url: "https://chatgpt.com/apps/google-calendar/connector_calendar"
                    .to_string(),
                auth_reason: Some("reauthentication_required".to_string()),
                link_id: Some("link_123".to_string()),
                error_code: Some("UNAUTHORIZED".to_string()),
                error_http_status_code: Some(401),
                error_action: Some("TRIGGER_REAUTHENTICATION".to_string()),
            })
        );
    }

    #[test]
    fn rejects_missing_or_mismatched_connector_ids() {
        assert_eq!(
            connector_auth_failure_from_tool_result(
                &auth_failure_result(),
                /*connector_id*/ None,
                Some("Google Calendar"),
                Some("https://chatgpt.com/apps/google-calendar/connector_calendar".to_string()),
            ),
            None
        );
        assert_eq!(
            connector_auth_failure_from_tool_result(
                &auth_failure_result(),
                Some("connector_drive"),
                Some("Google Drive"),
                Some("https://chatgpt.com/apps/google-drive/connector_drive".to_string()),
            ),
            None
        );
    }

    #[test]
    fn builds_url_elicitation_payload() {
        let auth_failure = connector_auth_failure_from_tool_result(
            &auth_failure_result(),
            Some("connector_calendar"),
            Some("Google Calendar"),
            Some("https://chatgpt.com/apps/google-calendar/connector_calendar".to_string()),
        )
        .expect("auth failure");

        assert_eq!(
            build_auth_elicitation("call_123", &auth_failure),
            CodexAppsAuthElicitation {
                meta: serde_json::json!({
                    MCP_TOOL_CODEX_APPS_META_KEY: {
                        CONNECTOR_AUTH_FAILURE_META_KEY: {
                            CONNECTOR_AUTH_FAILURE_IS_AUTH_FAILURE_KEY: true,
                            CONNECTOR_AUTH_FAILURE_CONNECTOR_ID_KEY: "connector_calendar",
                            "connector_name": "Google Calendar",
                            "install_url":
                                "https://chatgpt.com/apps/google-calendar/connector_calendar",
                            CONNECTOR_AUTH_FAILURE_AUTH_REASON_KEY: "reauthentication_required",
                            CONNECTOR_AUTH_FAILURE_LINK_ID_KEY: "link_123",
                            CONNECTOR_AUTH_FAILURE_ERROR_CODE_KEY: "UNAUTHORIZED",
                            CONNECTOR_AUTH_FAILURE_ERROR_HTTP_STATUS_CODE_KEY: 401,
                            CONNECTOR_AUTH_FAILURE_ERROR_ACTION_KEY: "TRIGGER_REAUTHENTICATION",
                        },
                    },
                }),
                message: "Reconnect Google Calendar on ChatGPT to restore access for this request."
                    .to_string(),
                url: "https://chatgpt.com/apps/google-calendar/connector_calendar".to_string(),
                elicitation_id: "codex_apps_auth_call_123".to_string(),
            }
        );
    }

    #[test]
    fn builds_auth_elicitation_plan() {
        let plan = build_auth_elicitation_plan(
            "call_123",
            &auth_failure_result(),
            Some("connector_calendar"),
            Some("Google Calendar"),
            Some("https://chatgpt.com/apps/google-calendar/connector_calendar".to_string()),
        )
        .expect("auth elicitation plan");

        assert_eq!(plan.auth_failure.connector_name, "Google Calendar");
        assert_eq!(plan.elicitation.elicitation_id, "codex_apps_auth_call_123");
    }
}
