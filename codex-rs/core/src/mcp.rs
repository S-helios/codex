//! 【文件职责】core 侧 MCP 配置门面：把 `codex-mcp` crate 的几个无状态查询
//! 函数（`configured_mcp_servers` / `effective_mcp_servers` /
//! `tool_plugin_provenance`）包成一个持有 `PluginsManager` 的 `McpManager`，
//! 让调用方不必每次自己拼 `McpConfig`。
//!
//! 【架构位置】
//!   层级：Agent 核心层（MCP 客户端方向的业务门面）
//!   上游：`Session` / 启动流程（构造 `McpManager`，调三个查询方法）
//!   下游：`codex_mcp` crate（真正的配置解析逻辑）
//!
//! 【数据流】`Config` + 插件 → `to_mcp_config()` 提炼出 `McpConfig`
//!           → `codex-mcp` 的查询函数 → server 列表 / 工具来源元数据
//!
//! 【阅读建议】文件很薄，三个方法是同一套模式（先 `to_mcp_config`，再委托）。
//!   「已配置 vs 生效」的区别见 `effective_servers`：后者会运行时注入 `codex_apps`。

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use codex_config::McpServerConfig;
use codex_core_plugins::PluginsManager;
use codex_login::CodexAuth;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::ToolPluginProvenance;
use codex_mcp::configured_mcp_servers;
use codex_mcp::effective_mcp_servers;
use codex_mcp::tool_plugin_provenance as collect_tool_plugin_provenance;

/// MCP 配置查询门面。持有 `PluginsManager` 是因为「生效的 MCP server」依赖
/// 当前激活的插件（插件可声明自带的 MCP server），所以查询前必须把插件状态
/// 一起 fold 进 `McpConfig`。`Clone` 廉价（内部只是 `Arc`）。
#[derive(Clone)]
pub struct McpManager {
    plugins_manager: Arc<PluginsManager>,
}

impl McpManager {
    pub fn new(plugins_manager: Arc<PluginsManager>) -> Self {
        Self { plugins_manager }
    }

    /// 返回「已配置」的 MCP server（config.toml + 激活插件声明的），
    /// 不含运行时注入的 `codex_apps`。技能依赖检测等场景用它判断某 server
    /// 是否已被用户配置。
    pub async fn configured_servers(&self, config: &Config) -> HashMap<String, McpServerConfig> {
        let mcp_config = config.to_mcp_config(self.plugins_manager.as_ref()).await;
        configured_mcp_servers(&mcp_config)
    }

    /// 返回「生效」的 MCP server，在 `configured_servers` 基础上按 `auth`
    /// 运行时注入/移除 host 端的 `codex_apps`（仅在启用 apps 且 auth 走
    /// Codex 后端时注入）。连接池 `McpConnectionManager` 用这份列表启动。
    pub async fn effective_servers(
        &self,
        config: &Config,
        auth: Option<&CodexAuth>,
    ) -> HashMap<String, EffectiveMcpServer> {
        let mcp_config = config.to_mcp_config(self.plugins_manager.as_ref()).await;
        effective_mcp_servers(&mcp_config, auth)
    }

    /// 返回工具来源元数据：哪个工具/连接器是哪个插件提供的。是个静态快照
    /// （config 加载后不更新），刷新连接池时会重新计算并传给新管理器。
    pub async fn tool_plugin_provenance(&self, config: &Config) -> ToolPluginProvenance {
        let mcp_config = config.to_mcp_config(self.plugins_manager.as_ref()).await;
        collect_tool_plugin_provenance(&mcp_config)
    }
}
