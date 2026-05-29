//! 【文件职责】实现「设备码」(device code) 登录流程：在无法/不便打开本地回调服务器
//! 的环境（如远程 SSH、无浏览器的终端）下，让用户拿一个短码到另一台设备上完成授权。
//!
//! 【架构位置】
//!   层级：登录/认证层（`codex-login` crate），是 `server.rs` 浏览器流程的替代路径。
//!   上游：CLI 登录命令（在检测到适合设备码时调用 `run_device_code_login`）。
//!   下游：复用 `server.rs` 的 `exchange_code_for_tokens` / `ensure_workspace_allowed`
//!         / `persist_tokens_async` 完成换令牌、工作区校验与落盘；HTTP 走自建后端的
//!         `/api/accounts/deviceauth/*` 端点。
//!
//! 【数据流】
//!   `request_user_code`（拿短码+轮询间隔）→ 终端打印提示 →
//!   `poll_for_token`（按间隔轮询直到用户在别处授权或超时）→ 拿到授权码+PKCE →
//!   复用 `exchange_code_for_tokens` 换令牌 → 校验工作区 → 落盘。
//!
//! 【阅读建议】入口 `run_device_code_login`；其余三个 `async fn` 按数据流顺序看即可。
//! 与浏览器流程的差异：PKCE 由「服务端」在轮询响应里下发，而非本地生成。
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Deserializer;
use serde::de::{self};
use std::time::Duration;
use std::time::Instant;

use crate::pkce::PkceCodes;
use crate::server::ServerOptions;
use codex_client::build_reqwest_client_with_custom_ca;
use std::io;

const ANSI_BLUE: &str = "\x1b[94m";
const ANSI_GRAY: &str = "\x1b[90m";
const ANSI_RESET: &str = "\x1b[0m";

/// 一次设备码登录会话的句柄。
/// `verification_url` + `user_code` 展示给用户去授权；`device_auth_id` 与 `interval`
/// 供后续轮询使用（私有，不外泄）。`interval` 为服务端要求的轮询间隔（秒）。
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
    device_auth_id: String,
    interval: u64,
}

#[derive(Deserialize)]
struct UserCodeResp {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

#[derive(Serialize)]
struct UserCodeReq {
    client_id: String,
}

#[derive(Serialize)]
struct TokenPollReq {
    device_auth_id: String,
    user_code: String,
}

// 自定义反序列化：服务端把 `interval` 当字符串返回（如 "5"），这里去空格后解析为 u64。
// 直接 `#[serde]` 成数字会因类型不符失败，故单独处理。
fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.trim().parse::<u64>().map_err(de::Error::custom)
}

#[derive(Deserialize)]
struct CodeSuccessResp {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

/// Request the user code and polling interval.
/// 向 `/deviceauth/usercode` 请求一次性短码与轮询间隔。
/// 若服务端返回 404，说明该 Codex 后端未启用设备码登录，转成更友好的 `NotFound` 错误
/// 提示用户改用浏览器登录或检查服务地址。
async fn request_user_code(
    client: &reqwest::Client,
    auth_base_url: &str,
    client_id: &str,
) -> std::io::Result<UserCodeResp> {
    let url = format!("{auth_base_url}/deviceauth/usercode");
    let body = serde_json::to_string(&UserCodeReq {
        client_id: client_id.to_string(),
    })
    .map_err(std::io::Error::other)?;
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(std::io::Error::other)?;

    if !resp.status().is_success() {
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "device code login is not enabled for this Codex server. Use the browser login or verify the server URL.",
            ));
        }

        return Err(std::io::Error::other(format!(
            "device code request failed with status {status}"
        )));
    }

    let body = resp.text().await.map_err(std::io::Error::other)?;
    serde_json::from_str(&body).map_err(std::io::Error::other)
}

/// Poll token endpoint until a code is issued or timeout occurs.
/// 轮询 `/deviceauth/token`，直到用户在别处完成授权（拿到授权码）或超时。
///
/// 关键约定：`403 Forbidden` / `404 Not Found` 表示「尚未授权，请继续等」，
/// 据此按 `interval` 间隔睡眠后重试；其它非成功状态视为真正失败立即返回。
/// 最长等待 15 分钟（与短码有效期一致），到点返回超时错误。
async fn poll_for_token(
    client: &reqwest::Client,
    auth_base_url: &str,
    device_auth_id: &str,
    user_code: &str,
    interval: u64,
) -> std::io::Result<CodeSuccessResp> {
    let url = format!("{auth_base_url}/deviceauth/token");
    // 15 分钟硬上限：与终端提示「expires in 15 minutes」及短码有效期保持一致。
    let max_wait = Duration::from_secs(15 * 60);
    let start = Instant::now();

    loop {
        let body = serde_json::to_string(&TokenPollReq {
            device_auth_id: device_auth_id.to_string(),
            user_code: user_code.to_string(),
        })
        .map_err(std::io::Error::other)?;
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(std::io::Error::other)?;

        let status = resp.status();

        if status.is_success() {
            return resp.json().await.map_err(std::io::Error::other);
        }

        // 403/404 = 用户还没授权完成：先看是否超时，否则睡 `interval` 再轮询。
        // 睡眠时长用 `min(剩余时间)` 收口，避免最后一次睡过 15 分钟上限。
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            if start.elapsed() >= max_wait {
                return Err(std::io::Error::other(
                    "device auth timed out after 15 minutes",
                ));
            }
            let sleep_for = Duration::from_secs(interval).min(max_wait - start.elapsed());
            tokio::time::sleep(sleep_for).await;
            continue;
        }

        return Err(std::io::Error::other(format!(
            "device auth failed with status {}",
            resp.status()
        )));
    }
}

fn print_device_code_prompt(verification_url: &str, code: &str) {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "\nWelcome to Codex [v{ANSI_GRAY}{version}{ANSI_RESET}]\n{ANSI_GRAY}OpenAI's command-line coding agent{ANSI_RESET}\n\
\nFollow these steps to sign in with ChatGPT using device code authorization:\n\
\n1. Open this link in your browser and sign in to your account\n   {ANSI_BLUE}{verification_url}{ANSI_RESET}\n\
\n2. Enter this one-time code {ANSI_GRAY}(expires in 15 minutes){ANSI_RESET}\n   {ANSI_BLUE}{code}{ANSI_RESET}\n\
\n{ANSI_GRAY}Device codes are a common phishing target. Never share this code.{ANSI_RESET}\n",
    );
}

/// 发起设备码登录的第一步：取得短码并组装 `DeviceCode`（含给用户看的验证地址）。
/// 注意验证地址用面向用户的 `/codex/device`，而后续轮询走 `/api/accounts/deviceauth/*`。
pub async fn request_device_code(opts: &ServerOptions) -> std::io::Result<DeviceCode> {
    let client = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let base_url = opts.issuer.trim_end_matches('/');
    let api_base_url = format!("{base_url}/api/accounts");
    let uc = request_user_code(&client, &api_base_url, &opts.client_id).await?;

    Ok(DeviceCode {
        verification_url: format!("{base_url}/codex/device"),
        user_code: uc.user_code,
        device_auth_id: uc.device_auth_id,
        interval: uc.interval,
    })
}

/// 第二步：阻塞轮询直到授权完成，然后复用浏览器流程的逻辑收尾。
/// 轮询响应里带回服务端下发的 PKCE（verifier+challenge）与授权码，据此换令牌、
/// 校验工作区、落盘。任一步失败按错误返回，工作区不符返回 `PermissionDenied`。
pub async fn complete_device_code_login(
    opts: ServerOptions,
    device_code: DeviceCode,
) -> std::io::Result<()> {
    let client = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let base_url = opts.issuer.trim_end_matches('/');
    let api_base_url = format!("{base_url}/api/accounts");

    let code_resp = poll_for_token(
        &client,
        &api_base_url,
        &device_code.device_auth_id,
        &device_code.user_code,
        device_code.interval,
    )
    .await?;

    let pkce = PkceCodes {
        code_verifier: code_resp.code_verifier,
        code_challenge: code_resp.code_challenge,
    };
    let redirect_uri = format!("{base_url}/deviceauth/callback");

    let tokens = crate::server::exchange_code_for_tokens(
        base_url,
        &opts.client_id,
        &redirect_uri,
        &pkce,
        &code_resp.authorization_code,
    )
    .await
    .map_err(|err| std::io::Error::other(format!("device code exchange failed: {err}")))?;

    if let Err(message) = crate::server::ensure_workspace_allowed(
        opts.forced_chatgpt_workspace_id.as_deref(),
        &tokens.id_token,
    ) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
    }

    crate::server::persist_tokens_async(
        &opts.codex_home,
        /*api_key*/ None,
        tokens.id_token,
        tokens.access_token,
        tokens.refresh_token,
        opts.cli_auth_credentials_store_mode,
    )
    .await
}

/// 设备码登录的总入口：取码 → 终端打印提示 → 等待授权并落盘，串起完整流程。
pub async fn run_device_code_login(opts: ServerOptions) -> std::io::Result<()> {
    let device_code = request_device_code(&opts).await?;
    print_device_code_prompt(&device_code.verification_url, &device_code.user_code);
    complete_device_code_login(opts, device_code).await
}
