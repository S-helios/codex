//! MCP tool metadata, filtering, schema shaping, and name normalization.
//!
//! Raw MCP tool identities must be preserved for protocol calls, while
//! model-visible tool names must be sanitized, deduplicated, and kept within API
//! limits. This module owns that translation as well as the shared [`ToolInfo`]
//! type and helpers that adjust tool schemas before exposing them to the model.
//!
//! 【文件职责】管理 MCP 工具的「双重身份」：一边保留协议调用所需的原始工具标识，
//!   一边把模型可见名做净化、去重、限长（API 要求 ≤64 字节）；同时持有共享类型
//!   [`ToolInfo`] 与「暴露给模型前调整工具 schema」的辅助函数。
//!
//! 【架构位置】
//!   层级：工具执行层（MCP 子系统的「工具名/schema 翻译」层）
//!   上游：`connection_manager`（聚合工具后调 `normalize_tools_for_model_with_prefix`）、
//!         `rmcp_client`（列举工具后构造 `ToolInfo`、调用过滤与 schema 整形）
//!   下游：`mcp::sanitize_responses_api_tool_name`（字符集净化）
//!
//! 【核心难点】模型可见名的「唯一化」：原始 server/工具名可能重名或超长，需要在
//!   净化后检测命名空间/工具名碰撞，对碰撞者追加 SHA1 哈希后缀，并在拼接后仍超长时
//!   截断+哈希，最终保证每个 `<namespace><name>` 全局唯一且不超限。
//!
//! 【阅读建议】先看 `ToolInfo`（字段含义=工具的两重身份），再看
//!   `normalize_tools_for_model_with_prefix`（去重/碰撞/哈希的主流程），
//!   底部的 `*_hash_suffix` / `fit_callable_parts_with_hash` 是其调用的纯函数辅助。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_config::McpServerConfig;
use codex_protocol::ToolName;
use rmcp::model::Tool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value as JsonValue;
use sha1::Digest;
use sha1::Sha1;
use tracing::warn;

use crate::mcp::sanitize_responses_api_tool_name;

pub(crate) const MCP_TOOLS_CACHE_WRITE_DURATION_METRIC: &str =
    "codex.mcp.tools.cache_write.duration_ms";

const LEGACY_MCP_TOOL_NAME_PREFIX: &str = "mcp__";

/// 一个 MCP 工具的完整描述，承载其「两重身份」：
///   - 原始身份（`server_name` + `tool.name`）：用于按 server 路由、回传给 MCP server；
///   - 模型可见身份（`callable_namespace` + `callable_name`）：净化/去重/限长后
///     用于 Responses API 的工具声明。
/// 该类型会被序列化进工具缓存，故字段上用 `alias`/`rename` 兼容历史序列化名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Raw MCP server name used for routing the tool call.
    pub server_name: String,
    /// Whether calls routed to this server may run in parallel.
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
    /// MCP server origin used for telemetry and diagnostics, when known.
    #[serde(default)]
    pub server_origin: Option<String>,
    /// Model-visible tool name used in Responses API tool declarations.
    #[serde(rename = "tool_name", alias = "callable_name")]
    pub callable_name: String,
    /// Model-visible namespace used for deferred tool loading.
    #[serde(rename = "tool_namespace", alias = "callable_namespace")]
    pub callable_namespace: String,
    /// Model-visible namespace description.
    // Keep the old serialized field name readable for cached ToolInfo values.
    #[serde(default, alias = "connector_description")]
    pub namespace_description: Option<String>,
    /// Raw MCP tool definition; `tool.name` is sent back to the MCP server.
    pub tool: Tool,
    pub connector_id: Option<String>,
    pub connector_name: Option<String>,
    #[serde(default)]
    pub plugin_display_names: Vec<String>,
}

impl ToolInfo {
    pub fn canonical_tool_name(&self) -> ToolName {
        ToolName::namespaced(self.callable_namespace.clone(), self.callable_name.clone())
    }
}

pub fn declared_openai_file_input_param_names(
    meta: Option<&Map<String, JsonValue>>,
) -> Vec<String> {
    let Some(meta) = meta else {
        return Vec::new();
    };

    meta.get(META_OPENAI_FILE_PARAMS)
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// A tool is allowed to be used if both are true:
/// 1. enabled is None (no allowlist is set) or the tool is explicitly enabled.
/// 2. The tool is not explicitly disabled.
///
/// per-server 的工具白/黑名单。放行规则=「白名单命中（或无白名单）」且「不在黑名单」。
/// `enabled` 为 None 表示「不设白名单」（默认全允许）；一旦设了白名单，
/// 未列入者一律拒绝。黑名单优先级更高，可在白名单基础上再排除个别工具。
#[derive(Default, Clone)]
pub(crate) struct ToolFilter {
    pub(crate) enabled: Option<HashSet<String>>,
    pub(crate) disabled: HashSet<String>,
}

impl ToolFilter {
    pub(crate) fn from_config(cfg: &McpServerConfig) -> Self {
        let enabled = cfg
            .enabled_tools
            .as_ref()
            .map(|tools| tools.iter().cloned().collect::<HashSet<_>>());
        let disabled = cfg
            .disabled_tools
            .as_ref()
            .map(|tools| tools.iter().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();

        Self { enabled, disabled }
    }

    pub(crate) fn allows(&self, tool_name: &str) -> bool {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(tool_name)
        {
            return false;
        }

        !self.disabled.contains(tool_name)
    }
}

/// Returns the model-visible view of a tool while preserving the raw metadata
/// used by execution. Keep cache entries raw and call this at manager return
/// boundaries.
///
/// 返回工具的「模型可见视图」：若其声明了 `openai/fileParams`（文件类入参），
/// 则把这些入参的 schema 重写为「期望绝对本地文件路径」的字符串/字符串数组，
/// 并补一段引导性 description。无文件入参时原样克隆返回。
/// 约定：缓存里存「裸」工具，仅在管理器对外返回的边界处才调用本函数做整形，
/// 避免把模型视图污染进缓存。
pub(crate) fn tool_with_model_visible_input_schema(tool: &Tool) -> Tool {
    let file_params = declared_openai_file_input_param_names(tool.meta.as_deref());
    if file_params.is_empty() {
        return tool.clone();
    }

    let mut tool = tool.clone();
    let mut input_schema = JsonValue::Object(tool.input_schema.as_ref().clone());
    mask_input_schema_for_file_path_params(&mut input_schema, &file_params);
    if let JsonValue::Object(input_schema) = input_schema {
        tool.input_schema = Arc::new(input_schema);
    }
    tool
}

pub(crate) fn filter_tools(tools: Vec<ToolInfo>, filter: &ToolFilter) -> Vec<ToolInfo> {
    tools
        .into_iter()
        .filter(|tool| filter.allows(&tool.tool.name))
        .collect()
}

/// Returns MCP tools with model-visible names normalized.
///
/// Raw MCP server/tool names are kept on each [`ToolInfo`] for protocol calls, while
/// `callable_namespace` / `callable_name` are sanitized and, when necessary, hashed so
/// every model-visible name is unique and <= 64 bytes.
///
/// When `prefix_mcp_tool_names` is true, the historical `mcp__` namespace
/// prefix is added without restoring the old trailing `__` namespace suffix.
///
/// 把一批工具的「模型可见名」规范化到唯一且 ≤64 字节。
/// 原始 server/工具名仍保留在各 `ToolInfo` 上供协议调用；这里只调整
/// `callable_namespace` / `callable_name`。`prefix_mcp_tool_names` 为真时补
/// 历史 `mcp__` 命名空间前缀（但不再恢复旧的尾部 `__` 后缀）。
///
/// 算法分四阶段（见下方各 Step）：先按「原始身份」去重，再分别消解命名空间碰撞、
/// 工具名碰撞（碰撞者追加哈希后缀），最后在拼接超长时做截断+哈希以保证唯一+限长。
/// 排序基于 `raw_tool_identity`，使输出顺序稳定、与输入插入顺序无关。
pub(crate) fn normalize_tools_for_model_with_prefix<I>(
    tools: I,
    prefix_mcp_tool_names: bool,
) -> Vec<ToolInfo>
where
    I: IntoIterator<Item = ToolInfo>,
{
    // Step 1：按「原始身份」（server+namespace+connector / +callable+原始工具名）
    // 去重，重复者直接跳过；同时对净化后的名字构造候选项，待后续消解碰撞。
    let mut seen_raw_names = HashSet::new();
    let mut candidates = Vec::new();
    for tool in tools {
        let raw_namespace_identity = format!(
            "{}\0{}\0{}",
            tool.server_name,
            tool.callable_namespace,
            tool.connector_id.as_deref().unwrap_or_default()
        );
        let raw_tool_identity = format!(
            "{}\0{}\0{}",
            raw_namespace_identity, tool.callable_name, tool.tool.name
        );
        if !seen_raw_names.insert(raw_tool_identity.clone()) {
            warn!("skipping duplicated tool {}", tool.tool.name);
            continue;
        }

        let callable_namespace = callable_namespace_with_prefix(
            &sanitize_responses_api_tool_name(&tool.callable_namespace),
            prefix_mcp_tool_names,
        );

        candidates.push(CallableToolCandidate {
            callable_namespace,
            callable_name: sanitize_responses_api_tool_name(&tool.callable_name),
            raw_namespace_identity,
            raw_tool_identity,
            tool,
        });
    }

    // Step 2：消解「命名空间碰撞」——多个不同原始命名空间净化后落到同一个
    // callable_namespace 的，给它们各自追加基于原始身份的哈希后缀以区分。
    let mut namespace_identities_by_base = HashMap::<String, HashSet<String>>::new();
    for candidate in &candidates {
        namespace_identities_by_base
            .entry(candidate.callable_namespace.clone())
            .or_default()
            .insert(candidate.raw_namespace_identity.clone());
    }
    let colliding_namespaces = namespace_identities_by_base
        .into_iter()
        .filter_map(|(namespace, identities)| (identities.len() > 1).then_some(namespace))
        .collect::<HashSet<_>>();
    for candidate in &mut candidates {
        if colliding_namespaces.contains(&candidate.callable_namespace) {
            candidate.callable_namespace = append_namespace_hash_suffix(
                &candidate.callable_namespace,
                &candidate.raw_namespace_identity,
            );
        }
    }

    // Step 3：消解「工具名碰撞」——在（已去碰撞的）命名空间内，若多个不同原始工具
    // 净化后落到同一个 callable_name，则给它们追加哈希后缀。
    let mut tool_identities_by_base = HashMap::<(String, String), HashSet<String>>::new();
    for candidate in &candidates {
        tool_identities_by_base
            .entry((
                candidate.callable_namespace.clone(),
                candidate.callable_name.clone(),
            ))
            .or_default()
            .insert(candidate.raw_tool_identity.clone());
    }
    let colliding_tools = tool_identities_by_base
        .into_iter()
        .filter_map(|(key, identities)| (identities.len() > 1).then_some(key))
        .collect::<HashSet<_>>();
    for candidate in &mut candidates {
        if colliding_tools.contains(&(
            candidate.callable_namespace.clone(),
            candidate.callable_name.clone(),
        )) {
            candidate.callable_name =
                append_hash_suffix(&candidate.callable_name, &candidate.raw_tool_identity);
        }
    }

    // 按原始身份排序，使输出顺序稳定可复现（与输入顺序、HashMap 迭代序无关）。
    candidates.sort_by(|left, right| left.raw_tool_identity.cmp(&right.raw_tool_identity));

    // Step 4：最终确定唯一且 ≤64 字节的 (namespace, name)。即使前几步已去碰撞，
    // 拼接后仍可能超长或与已用名冲突，故用 `unique_callable_parts` 兜底截断+哈希。
    let mut used_names = HashSet::new();
    let mut model_tools = Vec::new();
    for mut candidate in candidates {
        let (callable_namespace, callable_name) = unique_callable_parts(
            &candidate.callable_namespace,
            &candidate.callable_name,
            &candidate.raw_tool_identity,
            &mut used_names,
            MCP_TOOL_NAME_DELIMITER.len(),
        );
        candidate.tool.callable_namespace = callable_namespace;
        candidate.tool.callable_name = callable_name;
        model_tools.push(candidate.tool);
    }
    model_tools
}

#[derive(Debug)]
struct CallableToolCandidate {
    tool: ToolInfo,
    raw_namespace_identity: String,
    raw_tool_identity: String,
    callable_namespace: String,
    callable_name: String,
}

const MCP_TOOL_NAME_DELIMITER: &str = "__";
// 模型可见工具名（namespace+name 拼接后）的最大字节数：Responses API 的硬限制。
const MAX_TOOL_NAME_LENGTH: usize = 64;
// 碰撞/超长时追加的 SHA1 哈希后缀长度（取十六进制串前 12 位）：
// 兼顾「足够低碰撞概率」与「不过度挤占有限的名字预算」。
const CALLABLE_NAME_HASH_LEN: usize = 12;
// 工具 meta 中声明「文件类入参」的键名（见 `tool_with_model_visible_input_schema`）。
const META_OPENAI_FILE_PARAMS: &str = "openai/fileParams";

fn callable_namespace_with_prefix(namespace: &str, prefix_mcp_tool_names: bool) -> String {
    if !prefix_mcp_tool_names || namespace.starts_with(LEGACY_MCP_TOOL_NAME_PREFIX) {
        namespace.to_string()
    } else {
        format!("{LEGACY_MCP_TOOL_NAME_PREFIX}{namespace}")
    }
}

fn mask_input_schema_for_file_path_params(input_schema: &mut JsonValue, file_params: &[String]) {
    let Some(properties) = input_schema
        .as_object_mut()
        .and_then(|schema| schema.get_mut("properties"))
        .and_then(JsonValue::as_object_mut)
    else {
        return;
    };

    for field_name in file_params {
        let Some(property_schema) = properties.get_mut(field_name) else {
            continue;
        };
        mask_input_property_schema(property_schema);
    }
}

fn mask_input_property_schema(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    let mut description = object
        .get("description")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let guidance = "This parameter expects an absolute local file path. If you want to upload a file, provide the absolute path to that file here.";
    if description.is_empty() {
        description = guidance.to_string();
    } else if !description.contains(guidance) {
        description = format!("{description} {guidance}");
    }

    let is_array = object.get("type").and_then(JsonValue::as_str) == Some("array")
        || object.get("items").is_some();
    object.clear();
    object.insert("description".to_string(), JsonValue::String(description));
    if is_array {
        object.insert("type".to_string(), JsonValue::String("array".to_string()));
        object.insert("items".to_string(), serde_json::json!({ "type": "string" }));
    } else {
        object.insert("type".to_string(), JsonValue::String("string".to_string()));
    }
}

fn sha1_hex(s: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(s.as_bytes());
    let sha1 = hasher.finalize();
    format!("{sha1:x}")
}

fn callable_name_hash_suffix(raw_identity: &str) -> String {
    let hash = sha1_hex(raw_identity);
    format!("_{}", &hash[..CALLABLE_NAME_HASH_LEN])
}

fn append_hash_suffix(value: &str, raw_identity: &str) -> String {
    format!("{value}{}", callable_name_hash_suffix(raw_identity))
}

fn append_namespace_hash_suffix(namespace: &str, raw_identity: &str) -> String {
    if let Some(namespace) = namespace.strip_suffix(MCP_TOOL_NAME_DELIMITER) {
        format!(
            "{}{}{}",
            namespace,
            callable_name_hash_suffix(raw_identity),
            MCP_TOOL_NAME_DELIMITER
        )
    } else {
        append_hash_suffix(namespace, raw_identity)
    }
}

fn truncate_name(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

// 在「namespace+name+哈希后缀」总长不超 `MAX_TOOL_NAME_LENGTH` 的约束下，
// 给出截断方案（`reserved_len` 是分隔符等预留字节）。
// 策略：优先保留完整 namespace、截短 name 再接哈希后缀；当 namespace 自身就太长、
// 连后缀都放不下时，退化为「截短 namespace + 后缀」（此时 name 整段丢弃）。
fn fit_callable_parts_with_hash(
    namespace: &str,
    tool_name: &str,
    raw_identity: &str,
    reserved_len: usize,
) -> (String, String) {
    let suffix = callable_name_hash_suffix(raw_identity);
    let max_tool_len = MAX_TOOL_NAME_LENGTH.saturating_sub(namespace.len() + reserved_len);
    if max_tool_len >= suffix.len() {
        let prefix_len = max_tool_len - suffix.len();
        return (
            namespace.to_string(),
            format!("{}{}", truncate_name(tool_name, prefix_len), suffix),
        );
    }

    let max_namespace_len = MAX_TOOL_NAME_LENGTH.saturating_sub(suffix.len() + reserved_len);
    (truncate_name(namespace, max_namespace_len), suffix)
}

// 产出一个「既不超长、又未被占用」的 (namespace, name)。
// 快路径：原样拼接若已合规且未被占用，直接采用。否则进入循环：用原始身份
// （必要时叠加 attempt 计数）反复生成哈希后缀并截断适配，直到得到未占用的名字。
// `used_names` 记录已分配名字，跨工具防重；`attempt` 递增以应对极罕见的二次碰撞。
fn unique_callable_parts(
    namespace: &str,
    tool_name: &str,
    raw_identity: &str,
    used_names: &mut HashSet<String>,
    reserved_len: usize,
) -> (String, String) {
    let model_name = format!("{namespace}{tool_name}");
    if model_name.len() + reserved_len <= MAX_TOOL_NAME_LENGTH && used_names.insert(model_name) {
        return (namespace.to_string(), tool_name.to_string());
    }

    let mut attempt = 0_u32;
    loop {
        let hash_input = if attempt == 0 {
            raw_identity.to_string()
        } else {
            format!("{raw_identity}\0{attempt}")
        };
        let (namespace, tool_name) =
            fit_callable_parts_with_hash(namespace, tool_name, &hash_input, reserved_len);
        let model_name = format!("{namespace}{tool_name}");
        if used_names.insert(model_name) {
            return (namespace, tool_name);
        }
        attempt = attempt.saturating_add(1);
    }
}
