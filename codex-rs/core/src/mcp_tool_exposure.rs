//! 【文件职责】决定 MCP 工具以「直接暴露」还是「延迟暴露」的方式进入模型的
//! 工具列表。核心是 `build_mcp_tool_exposure`：工具数小于阈值就全部直接给模型；
//! 数量过大（或强制延迟特性开启）则全部转为延迟，模型须先搜索/显式选择才能用。
//!
//! 【架构位置】
//!   层级：Agent 核心层（MCP 客户端方向，工具暴露策略）
//!   上游：工具注册/装配流程（拿到 `McpConnectionManager::list_all_tools()`
//!         的全量工具后调本文件）
//!   下游：工具注册表（`direct_tools` 直接注入，`deferred_tools` 走搜索通道）
//!
//! 【数据流】全量 MCP 工具 + connectors + config
//!           → 过滤「模型可见」+ codex_apps 白名单 → 按阈值二选一 → 暴露结构
//!
//! 【为什么要延迟】Responses API 的工具列表有大小上限，几百个工具会把工具描述
//!   塞爆 token 预算 / 撞 API 限制。延迟模式用「搜索 + 按需加载」换列表瘦身，
//!   代价是超阈值时部分工具对模型「隐身」。

use std::collections::HashSet;

use codex_features::Feature;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;

use crate::config::Config;
use crate::connectors;

// Threshold above which MCP tools are deferred instead of listed directly.
// 直接暴露的工具数阈值：达到此数（且搜索工具可用）就整体转为延迟暴露。
// 100 是经验上限——超过后工具描述会显著挤占模型上下文 / 撞工具列表大小限制。
// [引用范围] 仅本文件 `build_mcp_tool_exposure` 使用；放开/收紧大型 MCP 生态
//   下模型能直接看到多少工具，调整前需评估对 token 预算的影响。
pub(crate) const DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD: usize = 100;

/// MCP 工具暴露结果，二选一的语义：
/// - `direct_tools` 非空、`deferred_tools` 为 `None`：全部直接注入模型工具列表。
/// - `direct_tools` 为空、`deferred_tools` 为 `Some`：全部延迟，模型须先搜索/
///   显式选择才能调用。两者不会同时非空（见 `build_mcp_tool_exposure`）。
pub(crate) struct McpToolExposure {
    pub(crate) direct_tools: Vec<McpToolInfo>,
    pub(crate) deferred_tools: Option<Vec<McpToolInfo>>,
}

/// 计算 MCP 工具暴露策略。
///
/// @param all_mcp_tools     - 连接池聚合的全量工具（含 codex_apps 与自定义 MCP）
/// @param connectors        - 已授权的 codex_apps 连接器；`None` 表示不放任何
///                            codex_apps 工具
/// @param search_tool_enabled - 搜索工具是否可用：延迟暴露依赖它，故为前提条件
/// @returns                 - 直接 or 延迟的二选一结果（见 `McpToolExposure`）
///
/// 注意：变量名 `deferred_tools` 是先按「候选集合」收集，最终可能落到
/// `direct_tools`（不延迟时）——名字在不延迟分支下略有误导，逻辑无误。
pub(crate) fn build_mcp_tool_exposure(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    config: &Config,
    search_tool_enabled: bool,
) -> McpToolExposure {
    // Step 1：收集模型可见的候选工具。
    // 先收非 codex_apps 工具，再按授权连接器追加 codex_apps 工具（过滤更严）。
    let mut deferred_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    if let Some(connectors) = connectors {
        deferred_tools.extend(filter_codex_apps_mcp_tools(
            all_mcp_tools,
            connectors,
            config,
        ));
    }

    // Step 2：判断是否延迟。延迟以「搜索工具可用」为硬前提（否则延迟的工具就
    // 永远无法被发现）；满足前提后，强制延迟特性开 或 工具数达阈值即延迟。
    let should_defer = search_tool_enabled
        && (config
            .features
            .enabled(Feature::ToolSearchAlwaysDeferMcpTools)
            || deferred_tools.len() >= DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD);

    if !should_defer {
        // 不延迟：候选集合整体转为直接暴露。
        return McpToolExposure {
            direct_tools: deferred_tools,
            deferred_tools: None,
        };
    }

    // 延迟：清空直接列表；候选若为空则连延迟列表也置 `None`（无工具可暴露）。
    McpToolExposure {
        direct_tools: Vec::new(),
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
    }
}

// 取所有「非 codex_apps」且对模型可见的工具（即用户自定义 MCP server 的工具）。
fn filter_non_codex_apps_mcp_tools_only(mcp_tools: &[McpToolInfo]) -> Vec<McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
        })
        .cloned()
        .collect()
}

// 取 codex_apps server 的工具，但过滤更严：除了「模型可见」，还要求工具携带
// `connector_id`、该 id 在已授权连接器白名单内、且该 app 工具在 config 里启用。
// 任一不满足都剔除——避免把未授权/未启用的连接器能力暴露给模型。
fn filter_codex_apps_mcp_tools(
    mcp_tools: &[McpToolInfo],
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> Vec<McpToolInfo> {
    // 用 `HashSet` 把授权连接器 id 收成 O(1) 查表集合。
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();

    mcp_tools
        .iter()
        .filter(|tool| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return false;
            }
            if !tool_is_model_visible(tool) {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            allowed.contains(connector_id) && connectors::codex_app_tool_is_enabled(config, tool)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
