//! 【文件职责】查询某个 ChatGPT「工作区」的 beta 设置，目前只关心一项：
//! 该工作区是否启用了 Codex 插件 (`enable_plugins`)。带进程内 TTL 缓存以减少请求。
//!
//! 【架构位置】
//!   层级：`codex-chatgpt` 业务层；经 `chatgpt_client` 调后端 `/accounts/{id}/settings`。
//!   消费方：插件相关逻辑据此决定是否对该工作区开放 Codex 插件。
//!
//! 【阅读建议】主入口 `codex_plugins_enabled_for_workspace`（含多重「直接放行」的
//!   短路条件 + 缓存读写）；`WorkspaceSettingsCache` 实现按 (base_url, account) 键的
//!   TTL 缓存。
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use codex_core::config::Config;
use codex_login::CodexAuth;
use serde::Deserialize;

use crate::chatgpt_client::chatgpt_get_request_with_timeout;

// 拉取工作区设置的网络超时：10s，控制对登录/启动等路径的阻塞上限。
const WORKSPACE_SETTINGS_TIMEOUT: Duration = Duration::from_secs(10);
// 缓存有效期：15 分钟。工作区 beta 设置变动不频繁，缓存以避免每次都打后端。
const WORKSPACE_SETTINGS_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
// 后端 beta_settings 中代表「是否启用 Codex 插件」的键名。
const CODEX_PLUGINS_BETA_SETTING: &str = "enable_plugins";

#[derive(Debug, Deserialize)]
struct WorkspaceSettingsResponse {
    #[serde(default)]
    beta_settings: HashMap<String, bool>,
}

/// 工作区设置的进程内缓存。
/// 只存「单个」最近条目（`Option`），因为同一进程通常只服务一个工作区；
/// 切换工作区时键不匹配会自然失效。`RwLock` 允许并发读、写时独占。
#[derive(Debug, Default)]
pub struct WorkspaceSettingsCache {
    entry: RwLock<Option<CachedWorkspaceSettings>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkspaceSettingsCacheKey {
    chatgpt_base_url: String,
    account_id: String,
}

#[derive(Clone, Debug)]
struct CachedWorkspaceSettings {
    key: WorkspaceSettingsCacheKey,
    expires_at: Instant,
    codex_plugins_enabled: bool,
}

impl WorkspaceSettingsCache {
    // 命中返回缓存值，否则 `None`（调用方据此去拉后端）。
    // 先以「读锁」快路径检查（未过期且键一致才命中）；未命中再取「写锁」清理掉
    // 已过期或键不匹配的陈旧条目，避免它长期占位。`PoisonError` 一律降级为取内部值。
    fn get_codex_plugins_enabled(&self, key: &WorkspaceSettingsCacheKey) -> Option<bool> {
        {
            let entry = match self.entry.read() {
                Ok(entry) => entry,
                Err(err) => err.into_inner(),
            };
            let now = Instant::now();
            if let Some(cached) = entry.as_ref()
                && now < cached.expires_at
                && cached.key == *key
            {
                return Some(cached.codex_plugins_enabled);
            }
        }

        let mut entry = match self.entry.write() {
            Ok(entry) => entry,
            Err(err) => err.into_inner(),
        };
        let now = Instant::now();
        if entry
            .as_ref()
            .is_some_and(|cached| now >= cached.expires_at || cached.key != *key)
        {
            *entry = None;
        }
        None
    }

    fn set_codex_plugins_enabled(&self, key: WorkspaceSettingsCacheKey, enabled: bool) {
        let mut entry = match self.entry.write() {
            Ok(entry) => entry,
            Err(err) => err.into_inner(),
        };
        *entry = Some(CachedWorkspaceSettings {
            key,
            expires_at: Instant::now() + WORKSPACE_SETTINGS_CACHE_TTL,
            codex_plugins_enabled: enabled,
        });
    }
}

/// 判断当前工作区是否启用 Codex 插件。
///
/// 采用「默认放行」策略：以下情况一律视为启用（返回 `true`），不去打后端——
///   无 auth / 非 ChatGPT 登录 / 无 token / 非工作区账户 / 缺 account id。
/// 仅当确为「工作区账户且有 account id」时才查（带缓存）后端的 `beta_settings`，
/// 且该项缺省时同样按启用处理。即：只有后端显式置为 false 才禁用。
///
/// @param cache - 可选缓存；传入则命中走缓存、未命中回填，省去重复请求。
pub async fn codex_plugins_enabled_for_workspace(
    config: &Config,
    auth: Option<&CodexAuth>,
    cache: Option<&WorkspaceSettingsCache>,
) -> anyhow::Result<bool> {
    let Some(auth) = auth else {
        return Ok(true);
    };
    if !auth.is_chatgpt_auth() {
        return Ok(true);
    }

    let token_data = auth
        .get_token_data()
        .context("ChatGPT token data is not available")?;
    if !token_data.id_token.is_workspace_account() {
        return Ok(true);
    }

    let Some(account_id) = token_data.account_id.as_deref().filter(|id| !id.is_empty()) else {
        return Ok(true);
    };

    let cache_key = WorkspaceSettingsCacheKey {
        chatgpt_base_url: config.chatgpt_base_url.clone(),
        account_id: account_id.to_string(),
    };
    if let Some(cache) = cache
        && let Some(enabled) = cache.get_codex_plugins_enabled(&cache_key)
    {
        return Ok(enabled);
    }

    let encoded_account_id = encode_path_segment(account_id);
    let settings: WorkspaceSettingsResponse = chatgpt_get_request_with_timeout(
        config,
        format!("/accounts/{encoded_account_id}/settings"),
        Some(WORKSPACE_SETTINGS_TIMEOUT),
    )
    .await?;

    let codex_plugins_enabled = settings
        .beta_settings
        .get(CODEX_PLUGINS_BETA_SETTING)
        .copied()
        .unwrap_or(true);

    if let Some(cache) = cache {
        cache.set_codex_plugins_enabled(cache_key, codex_plugins_enabled);
    }

    Ok(codex_plugins_enabled)
}

// 对 account id 做 URL 路径段百分号编码（仅保留 RFC3986 unreserved 字符），
// 防止其中的特殊字符破坏 `/accounts/{id}/settings` 的路径结构或被误解析。
fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
#[path = "workspace_settings_tests.rs"]
mod tests;
