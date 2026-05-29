//! 【文件职责】向 ChatGPT 后端 API 发起带认证的 GET 请求的底层客户端（crate 私有）。
//! 统一处理：取用 ChatGPT 登录态、注入认证头与产品标识头、拼 URL、解析 JSON、错误归一。
//!
//! 【架构位置】被本 crate 的 `get_task` / `connectors` / `workspace_settings` 复用，
//! 是它们访问 ChatGPT 后端的唯一出口。
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_login::default_client::create_client;

use anyhow::Context;
use serde::de::DeserializeOwned;
use std::time::Duration;

// 产品标识请求头：后端据此识别请求来自 Codex（用于配额/路由/统计等）。
const OAI_PRODUCT_SKU_HEADER: &str = "OAI-Product-Sku";
const CODEX_PRODUCT_SKU: &str = "codex";

/// Make a GET request to the ChatGPT backend API.
/// 向 ChatGPT 后端发 GET 请求（不带超时），是下方带超时版本的便捷包装。
pub(crate) async fn chatgpt_get_request<T: DeserializeOwned>(
    config: &Config,
    path: String,
) -> anyhow::Result<T> {
    chatgpt_get_request_with_timeout(config, path, /*timeout*/ None).await
}

/// 向 ChatGPT 后端发 GET 请求并把响应反序列化为 `T`，可选超时。
///
/// 前置校验（任一不满足直接 `Err`）：必须有 ChatGPT 登录态、且使用 Codex 后端认证、
/// 且带有 account id（否则提示重新 `codex login`）。
/// 非 2xx 响应会带上状态码与响应体文本作为错误返回。
pub(crate) async fn chatgpt_get_request_with_timeout<T: DeserializeOwned>(
    config: &Config,
    path: String,
    timeout: Option<Duration>,
) -> anyhow::Result<T> {
    let chatgpt_base_url = &config.chatgpt_base_url;
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager
        .auth()
        .await
        .ok_or_else(|| anyhow::anyhow!("ChatGPT auth not available"))?;
    anyhow::ensure!(
        auth.uses_codex_backend(),
        "ChatGPT backend requests require Codex backend auth"
    );
    anyhow::ensure!(
        auth.get_account_id().is_some(),
        "ChatGPT account ID not available, please re-run `codex login`"
    );

    // Make direct HTTP request to ChatGPT backend API with the token
    let client = create_client();
    let url = format!(
        "{}/{}",
        chatgpt_base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );

    let mut request = client
        .get(&url)
        .headers(codex_model_provider::auth_provider_from_auth(&auth).to_auth_headers())
        .header(OAI_PRODUCT_SKU_HEADER, CODEX_PRODUCT_SKU)
        .header("Content-Type", "application/json");

    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }

    let response = request.send().await.context("Failed to send request")?;

    if response.status().is_success() {
        let result: T = response
            .json()
            .await
            .context("Failed to parse JSON response")?;
        Ok(result)
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Request failed with status {status}: {body}")
    }
}
