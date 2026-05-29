//! 【文件职责】列举并合并用户可用的「连接器 / Apps」(connectors)：把目录服务返回的
//! 全量连接器、用户实际可访问的连接器、以及本地插件声明的 App 三方数据合并去重，
//! 过滤掉当前 originator 不允许的项，并标注启用状态。
//!
//! 【架构位置】
//!   层级：`codex-chatgpt` 业务层，桥接 `codex-connectors`（合并/缓存/过滤原语）、
//!         `codex-core`（可访问连接器探测）、`codex-core-plugins`（本地插件 App）。
//!   下游：ChatGPT 目录 HTTP 端点（经 `chatgpt_client`）、连接器磁盘缓存。
//!
//! 【阅读建议】对外主入口 `list_connectors`（全量 ⨝ 可访问）；
//!   `list_all_connectors*` 负责拉全量（带缓存/强制刷新）；
//!   `merge_connectors_with_accessible` / `connectors_for_plugin_apps` 是纯合并逻辑。
//!   `apps_enabled` / `connector_auth` 是前置开关与鉴权门槛。
//!
//! 注：本文件多数 `pub fn` 是对 `codex_connectors` 原语的编排，名字已较自解释，
//! 仅对「合并/过滤策略」这类不显然处补注。
use std::collections::HashSet;
use std::time::Duration;

use crate::chatgpt_client::chatgpt_get_request_with_timeout;

use codex_app_server_protocol::AppInfo;
use codex_connectors::ConnectorDirectoryCacheContext;
use codex_connectors::ConnectorDirectoryCacheKey;
use codex_connectors::DirectoryListResponse;
use codex_connectors::filter::filter_disallowed_connectors;
use codex_connectors::merge::merge_connectors;
use codex_connectors::merge::merge_plugin_connectors;
use codex_core::config::Config;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_environment_manager;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_options;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_options_and_status;
pub use codex_core::connectors::list_cached_accessible_connectors_from_mcp_tools;
pub use codex_core::connectors::with_app_enabled_state;
use codex_core_plugins::PluginsManager;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::originator;
use codex_plugin::AppConnectorId;

const DIRECTORY_CONNECTORS_TIMEOUT: Duration = Duration::from_secs(60);

// 总开关：当前 auth 与 feature flag 是否允许使用 Apps/连接器。
// 未登录或非 Codex 后端时按 false 传入，让上层据 feature 策略决定是否放行。
async fn apps_enabled(config: &Config) -> bool {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager.auth().await;
    config
        .features
        .apps_enabled_for_auth(auth.as_ref().is_some_and(CodexAuth::uses_codex_backend))
}

// 取用于连接器请求的认证；要求已登录且为 Codex 后端，否则返回错误。
async fn connector_auth(config: &Config) -> anyhow::Result<CodexAuth> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager
        .auth()
        .await
        .ok_or_else(|| anyhow::anyhow!("ChatGPT auth not available"))?;
    anyhow::ensure!(
        auth.uses_codex_backend(),
        "ChatGPT connectors require Codex backend auth"
    );
    Ok(auth)
}

/// 对外主入口：返回用户可见的连接器全集（已合并可访问状态、过滤、标注启用态）。
/// Apps 未启用时返回空列表。并发拉取「全量目录」与「可访问连接器」后再合并。
pub async fn list_connectors(config: &Config) -> anyhow::Result<Vec<AppInfo>> {
    if !apps_enabled(config).await {
        return Ok(Vec::new());
    }
    // 两路请求互不依赖，用 `join!` 并发以省一次往返延迟。
    let (connectors_result, accessible_result) = tokio::join!(
        list_all_connectors(config),
        list_accessible_connectors_from_mcp_tools(config),
    );
    let connectors = connectors_result?;
    let accessible = accessible_result?;
    Ok(with_app_enabled_state(
        merge_connectors_with_accessible(
            connectors, accessible, /*all_connectors_loaded*/ true,
        ),
        config,
    ))
}

pub async fn list_all_connectors(config: &Config) -> anyhow::Result<Vec<AppInfo>> {
    list_all_connectors_with_options(config, /*force_refetch*/ false).await
}

/// 仅读磁盘缓存的连接器（不触网）：缓存未命中返回 `None`，供需要即时结果的场景兜底。
/// 仍会叠加本地插件 App 并做 originator 过滤。
pub async fn list_cached_all_connectors(config: &Config) -> Option<Vec<AppInfo>> {
    if !apps_enabled(config).await {
        return Some(Vec::new());
    }

    let auth = connector_auth(config).await.ok()?;
    let cache_context = connector_directory_cache_context(config, &auth);
    let connectors = codex_connectors::cached_directory_connectors(&cache_context)?;
    let connectors = merge_plugin_connectors(
        connectors,
        plugin_apps_for_config(config)
            .await
            .into_iter()
            .map(|connector_id| connector_id.0),
    );
    Some(filter_disallowed_connectors(
        connectors,
        originator().value.as_str(),
    ))
}

/// 拉取全量连接器目录（带缓存）：`force_refetch=true` 时跳过缓存强制刷新。
/// 把 `chatgpt_get_request_with_timeout` 作为取数闭包交给 `codex_connectors`，
/// 后者负责缓存读写；随后合并本地插件 App 并按 originator 过滤。
pub async fn list_all_connectors_with_options(
    config: &Config,
    force_refetch: bool,
) -> anyhow::Result<Vec<AppInfo>> {
    if !apps_enabled(config).await {
        return Ok(Vec::new());
    }
    let auth = connector_auth(config).await?;
    let cache_context = connector_directory_cache_context(config, &auth);
    let connectors = codex_connectors::list_all_connectors_with_options(
        cache_context,
        auth.is_workspace_account(),
        force_refetch,
        |path| async move {
            chatgpt_get_request_with_timeout::<DirectoryListResponse>(
                config,
                path,
                Some(DIRECTORY_CONNECTORS_TIMEOUT),
            )
            .await
        },
    )
    .await?;
    let connectors = merge_plugin_connectors(
        connectors,
        plugin_apps_for_config(config)
            .await
            .into_iter()
            .map(|connector_id| connector_id.0),
    );
    Ok(filter_disallowed_connectors(
        connectors,
        originator().value.as_str(),
    ))
}

fn connector_directory_cache_context(
    config: &Config,
    auth: &CodexAuth,
) -> ConnectorDirectoryCacheContext {
    ConnectorDirectoryCacheContext::new(
        config.codex_home.to_path_buf(),
        ConnectorDirectoryCacheKey::new(
            config.chatgpt_base_url.clone(),
            auth.get_account_id(),
            auth.get_chatgpt_user_id(),
            auth.is_workspace_account(),
        ),
    )
}

async fn plugin_apps_for_config(config: &Config) -> Vec<AppConnectorId> {
    let plugins_input = config.plugins_config_input();
    PluginsManager::new(config.codex_home.to_path_buf())
        .plugins_for_config(&plugins_input)
        .await
        .effective_apps()
}

/// 在给定连接器集合基础上，仅返回「由本地插件声明」且被允许的那些 App。
/// 先把插件 App 并入（补齐目录中缺失的项），过滤掉 disallowed，再用插件 id 集合
/// 收窄结果——确保输出严格对应传入的 `plugin_apps`。
pub fn connectors_for_plugin_apps(
    connectors: Vec<AppInfo>,
    plugin_apps: &[AppConnectorId],
) -> Vec<AppInfo> {
    let plugin_app_ids = plugin_apps
        .iter()
        .map(|connector_id| connector_id.0.as_str())
        .collect::<HashSet<_>>();

    let connectors = merge_plugin_connectors(
        connectors,
        plugin_apps
            .iter()
            .map(|connector_id| connector_id.0.clone()),
    );
    filter_disallowed_connectors(connectors, originator().value.as_str())
        .into_iter()
        .filter(|connector| plugin_app_ids.contains(connector.id.as_str()))
        .collect()
}

/// 把「可访问连接器」合并进「全量连接器」，标注 `is_accessible` 并过滤 disallowed。
///
/// 关键开关 `all_connectors_loaded`：
///   - true（全量已加载完）：丢弃不在全量列表里的可访问项，避免展示已下架/不在目录的连接器；
///   - false（全量仍在加载）：保留可访问项，以免用户暂时看不到自己实际能用的连接器。
pub fn merge_connectors_with_accessible(
    connectors: Vec<AppInfo>,
    accessible_connectors: Vec<AppInfo>,
    all_connectors_loaded: bool,
) -> Vec<AppInfo> {
    let accessible_connectors = if all_connectors_loaded {
        let connector_ids: HashSet<&str> = connectors
            .iter()
            .map(|connector| connector.id.as_str())
            .collect();
        accessible_connectors
            .into_iter()
            .filter(|connector| connector_ids.contains(connector.id.as_str()))
            .collect()
    } else {
        accessible_connectors
    };
    let merged = merge_connectors(connectors, accessible_connectors);
    filter_disallowed_connectors(merged, originator().value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_connectors::metadata::connector_install_url;
    use codex_plugin::AppConnectorId;
    use pretty_assertions::assert_eq;

    fn app(id: &str) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: None,
            is_accessible: false,
            is_enabled: true,
            plugin_display_names: Vec::new(),
        }
    }

    fn merged_app(id: &str, is_accessible: bool) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: Some(connector_install_url(id, id)),
            is_accessible,
            is_enabled: true,
            plugin_display_names: Vec::new(),
        }
    }

    #[test]
    fn excludes_accessible_connectors_not_in_all_when_all_loaded() {
        let merged = merge_connectors_with_accessible(
            vec![app("alpha")],
            vec![app("alpha"), app("beta")],
            /*all_connectors_loaded*/ true,
        );
        assert_eq!(merged, vec![merged_app("alpha", /*is_accessible*/ true)]);
    }

    #[test]
    fn keeps_accessible_connectors_not_in_all_while_all_loading() {
        let merged = merge_connectors_with_accessible(
            vec![app("alpha")],
            vec![app("alpha"), app("beta")],
            /*all_connectors_loaded*/ false,
        );
        assert_eq!(
            merged,
            vec![
                merged_app("alpha", /*is_accessible*/ true),
                merged_app("beta", /*is_accessible*/ true)
            ]
        );
    }

    #[test]
    fn connectors_for_plugin_apps_returns_only_requested_plugin_apps() {
        let connectors = connectors_for_plugin_apps(
            vec![app("alpha"), app("beta")],
            &[
                AppConnectorId("alpha".to_string()),
                AppConnectorId("gmail".to_string()),
            ],
        );
        assert_eq!(
            connectors,
            vec![app("alpha"), merged_app("gmail", /*is_accessible*/ false)]
        );
    }

    #[test]
    fn connectors_for_plugin_apps_filters_disallowed_plugin_apps() {
        let connectors = connectors_for_plugin_apps(
            Vec::new(),
            &[AppConnectorId(
                "asdk_app_6938a94a61d881918ef32cb999ff937c".to_string(),
            )],
        );
        assert_eq!(connectors, Vec::<AppInfo>::new());
    }
}
