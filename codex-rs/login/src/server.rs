//! Local OAuth callback server for CLI login.
//!
//! This module runs the short-lived localhost server used by interactive sign-in.
//!
//! The callback flow has two competing responsibilities:
//!
//! - preserve enough backend and transport detail for developers, sysadmins, and support
//!   engineers to diagnose failed sign-ins
//! - avoid persisting secrets or sensitive URL/query data into normal application logs
//!
//! This module therefore keeps the user-facing error path and the structured-log path separate.
//! Returned `io::Error` values still carry the detail needed by CLI/browser callers, while
//! structured logs only emit explicitly reviewed fields plus redacted URL/error values.
//!
//! 【文件职责】实现 CLI 交互式登录用的本地 localhost OAuth 回调服务器（短生命周期）。
//! 浏览器完成授权后会跳回 `http://localhost:<port>/auth/callback`，本模块在此处接收
//! 授权码、用 PKCE 换取令牌、落盘凭证，并把浏览器重定向到成功/失败页面。
//!
//! 【架构位置】
//!   层级：登录/认证层（`codex-login` crate）
//!   上游：CLI 登录命令（调用 `run_login_server` 拿到 `auth_url` 并打开浏览器）；
//!         `device_code_auth.rs` 复用本模块的令牌交换/落盘函数。
//!   下游：`auth.rs`（凭证读写与吊销）、`pkce.rs`（PKCE 生成）、
//!         `token_data.rs`（JWT 解析）、OpenAI OAuth 端点（HTTP）。
//!
//! 【数据流】
//!   生成 PKCE+state → 绑定本地端口 → 拼 authorize URL → 打开浏览器 →
//!   回调携带 code → `exchange_code_for_tokens` 换令牌 → `persist_tokens_async` 落盘 →
//!   302 跳转到 `/success`（命中后 server 退出）。
//!
//! 【设计要点 · 双轨日志】这是本文件最重要的安全约束：错误「面向用户/CLI」与
//!   「面向结构化日志」分两条路径。返回给调用方的 `io::Error` 保留后端原始细节（便于排障），
//!   而 `tracing` 日志只输出显式审查过的字段，URL/错误中的敏感参数（token/code/state 等）
//!   一律打码。修改任何日志语句时务必维持这条边界。
//!
//! 【阅读建议】主入口 `run_login_server`；核心回调分发在 `process_request`；
//!   令牌交换看 `exchange_code_for_tokens`；脱敏逻辑看 `redact_sensitive_url_parts`
//!   一族函数。底部 `html_escape` / 模板渲染是工具函数，可后看。
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use crate::auth::AuthDotJson;
use crate::auth::load_auth_dot_json;
use crate::auth::revoke_auth_tokens;
use crate::auth::save_auth;
use crate::auth::should_revoke_auth_tokens;
use crate::default_client::originator;
use crate::pkce::PkceCodes;
use crate::pkce::generate_pkce;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;
use base64::Engine;
use chrono::Utc;
use codex_app_server_protocol::AuthMode;
use codex_client::build_reqwest_client_with_custom_ca;
use codex_config::types::AuthCredentialsStoreMode;
use codex_utils_template::Template;
use rand::RngCore;
use serde_json::Value as JsonValue;
use tiny_http::Header;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;
use tracing::error;
use tracing::info;
use tracing::warn;

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
// 默认回调端口：必须与 OAuth 服务端登记的 redirect URI 白名单一致，
// 不能随意改动，否则授权服务器会拒绝回调。
const DEFAULT_PORT: u16 = 1455;
// Keep in sync with the Codex CLI Hydra redirect URI allow-list.
// 备用端口：当 1455 被占用且无法腾出时回退到此端口。
// 同样在服务端白名单内，改这里需同步更新服务端 allow-list（见 `bind_server`）。
const FALLBACK_PORT: u16 = 1457;
static LOGIN_ERROR_PAGE_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(include_str!("assets/error.html"))
        .unwrap_or_else(|err| panic!("login error page template must parse: {err}"))
});

/// Options for launching the local login callback server.
/// 启动本地登录回调服务器的配置项。
/// `force_state` / `forced_chatgpt_workspace_id` 主要服务于测试与企业版工作区限制：
/// 前者固定 state 便于断言，后者把登录限制在指定 ChatGPT 工作区。
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub codex_home: PathBuf,
    pub client_id: String,
    pub issuer: String,
    pub port: u16,
    pub open_browser: bool,
    pub force_state: Option<String>,
    pub forced_chatgpt_workspace_id: Option<Vec<String>>,
    pub codex_streamlined_login: bool,
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
}

impl ServerOptions {
    /// Creates a server configuration with the default issuer and port.
    pub fn new(
        codex_home: PathBuf,
        client_id: String,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    ) -> Self {
        Self {
            codex_home,
            client_id,
            issuer: DEFAULT_ISSUER.to_string(),
            port: DEFAULT_PORT,
            open_browser: true,
            force_state: None,
            forced_chatgpt_workspace_id,
            codex_streamlined_login: false,
            cli_auth_credentials_store_mode,
        }
    }
}

/// Handle for a running login callback server.
/// 一个正在运行的回调服务器的句柄。
/// 调用方拿到它后：把 `auth_url` 交给浏览器打开，用 `block_until_done` 等待登录结果，
/// 或用 `cancel` 提前中止。`actual_port` 可能因端口占用而不同于请求的端口。
pub struct LoginServer {
    pub auth_url: String,
    pub actual_port: u16,
    server_handle: tokio::task::JoinHandle<io::Result<()>>,
    shutdown_handle: ShutdownHandle,
}

impl LoginServer {
    /// Waits for the login callback loop to finish.
    pub async fn block_until_done(self) -> io::Result<()> {
        self.server_handle
            .await
            .map_err(|err| io::Error::other(format!("login server thread panicked: {err:?}")))?
    }

    /// Requests shutdown of the callback server.
    pub fn cancel(&self) {
        self.shutdown_handle.shutdown();
    }

    /// Returns a cloneable cancel handle for the running server.
    pub fn cancel_handle(&self) -> ShutdownHandle {
        self.shutdown_handle.clone()
    }
}

/// Handle used to signal the login server loop to exit.
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    shutdown_notify: Arc<tokio::sync::Notify>,
}

impl ShutdownHandle {
    /// Signals the login loop to terminate.
    pub fn shutdown(&self) {
        self.shutdown_notify.notify_one();
    }
}

/// Starts a local callback server and returns the browser auth URL.
/// 启动本地回调服务器并返回应在浏览器打开的授权 URL。
///
/// 本函数不阻塞：它绑定端口、构造 authorize URL、（可选）打开浏览器，然后立即返回
/// `LoginServer` 句柄；真正的回调处理在后台异步任务里进行。
///
/// 副作用：绑定 TCP 端口、可能启动系统浏览器、spawn 一个 OS 线程 + 一个 tokio 任务。
/// @returns - `LoginServer`，含 `auth_url` 与用于等待/取消的句柄。
pub fn run_login_server(opts: ServerOptions) -> io::Result<LoginServer> {
    // 每次登录都新生成 PKCE 与 state：PKCE 防授权码被截获后冒用，
    // state 用于校验回调确实来自本次请求（防 CSRF / 串号）。
    let pkce = generate_pkce();
    let state = opts.force_state.clone().unwrap_or_else(generate_state);

    let server = bind_server(opts.port)?;
    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "Unable to determine the server port",
            ));
        }
    };
    let server = Arc::new(server);

    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(
        &opts.issuer,
        &opts.client_id,
        &redirect_uri,
        &pkce,
        &state,
        opts.forced_chatgpt_workspace_id.as_deref(),
    );

    if opts.open_browser {
        let _ = webbrowser::open(&auth_url);
    }

    // Map blocking reads from server.recv() to an async channel.
    // 把 `tiny_http` 的阻塞式 `server.recv()` 转接到 async 通道：tiny_http 是同步库，
    // 这里用一个专用 OS 线程做阻塞读，再 `blocking_send` 进 channel，
    // 让下方的 tokio 任务能用 `select!` 同时监听「新请求」和「取消信号」。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Request>(16);
    let _server_handle = {
        let server = server.clone();
        thread::spawn(move || -> io::Result<()> {
            while let Ok(request) = server.recv() {
                match tx.blocking_send(request) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("Failed to send request to channel: {error}");
                        return Err(io::Error::other("Failed to send request to channel"));
                    }
                }
            }
            Ok(())
        })
    };

    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let server_handle = {
        let shutdown_notify = shutdown_notify.clone();
        let server = server;
        tokio::spawn(async move {
            // 回调主循环：在「收到取消通知」与「收到新 HTTP 请求」之间二选一。
            // 命中 `/auth/callback` 成功跳转或 `/success`/`/cancel` 时通过 `break` 携带
            // 最终结果退出；其余路径（如 404、state 不符）只回响应、继续循环等下一个请求。
            let result = loop {
                tokio::select! {
                    _ = shutdown_notify.notified() => {
                        break Err(io::Error::other("Login was not completed"));
                    }
                    maybe_req = rx.recv() => {
                        let Some(req) = maybe_req else {
                            break Err(io::Error::other("Login was not completed"));
                        };

                        let url_raw = req.url().to_string();
                        let response =
                            process_request(&url_raw, &opts, &redirect_uri, &pkce, actual_port, &state).await;

                        let exit_result = match response {
                            HandledRequest::Response(response) => {
                                let _ = tokio::task::spawn_blocking(move || req.respond(response)).await;
                                None
                            }
                            HandledRequest::ResponseAndExit {
                                headers,
                                body,
                                result,
                            } => {
                                let _ = tokio::task::spawn_blocking(move || {
                                    send_response_with_disconnect(req, headers, body)
                                })
                                .await;
                                Some(result)
                            }
                            HandledRequest::RedirectWithHeader(header) => {
                                let redirect = Response::empty(302).with_header(header);
                                let _ = tokio::task::spawn_blocking(move || req.respond(redirect)).await;
                                None
                            }
                        };

                        if let Some(result) = exit_result {
                            break result;
                        }
                    }
                }
            };

            // Ensure that the server is unblocked so the thread dedicated to
            // running `server.recv()` in a loop exits cleanly.
            server.unblock();
            result
        })
    };

    Ok(LoginServer {
        auth_url,
        actual_port,
        server_handle,
        shutdown_handle: ShutdownHandle { shutdown_notify },
    })
}

/// Internal callback handling outcome.
/// 单次回调处理的内部结果，决定主循环如何应答以及是否退出：
///   - `Response`：普通应答，连接保持，循环继续等下一个请求。
///   - `RedirectWithHeader`：302 跳转（授权成功后跳 `/success`），循环继续。
///   - `ResponseAndExit`：应答后结束登录流程，`result` 即整个登录的成败。
enum HandledRequest {
    Response(Response<Cursor<Vec<u8>>>),
    RedirectWithHeader(Header),
    ResponseAndExit {
        headers: Vec<Header>,
        body: Vec<u8>,
        result: io::Result<()>,
    },
}

/// 处理一次到达本地回调服务器的 HTTP 请求，按路径分发。
///
/// 这是回调流程的核心分发器。仅 `/auth/callback`（授权成功路径）会触发
/// 「换令牌 → 落盘 → 跳转 `/success`」的完整链路；其它路径只做应答或结束。
///
/// @param url_raw      - 原始请求行里的 URL（形如 `/auth/callback?code=...`）。
/// @param state        - 本次登录生成的 state，用于校验回调防 CSRF。
/// @returns            - `HandledRequest`，指示主循环如何应答 / 是否退出。
///
/// 副作用：成功路径会发起网络请求换令牌并把凭证写入磁盘。
/// 安全：state 不匹配直接 400 拒绝；错误信息分「用户可见」与「日志」两路（见文件头）。

async fn process_request(
    url_raw: &str,
    opts: &ServerOptions,
    redirect_uri: &str,
    pkce: &PkceCodes,
    actual_port: u16,
    state: &str,
) -> HandledRequest {
    let parsed_url = match url::Url::parse(&format!("http://localhost{url_raw}")) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("URL parse error: {e}");
            return HandledRequest::Response(
                Response::from_string("Bad Request").with_status_code(400),
            );
        }
    };
    let path = parsed_url.path().to_string();

    match path.as_str() {
        "/auth/callback" => {
            let params: std::collections::HashMap<String, String> =
                parsed_url.query_pairs().into_owned().collect();
            let has_code = params.get("code").is_some_and(|code| !code.is_empty());
            let has_state = params.get("state").is_some_and(|state| !state.is_empty());
            let has_error = params.get("error").is_some_and(|error| !error.is_empty());
            let state_valid = params.get("state").map(String::as_str) == Some(state);
            info!(
                path = %path,
                has_code,
                has_state,
                has_error,
                state_valid,
                "received login callback"
            );
            // state 不匹配：很可能是过期标签页、串号或 CSRF，立即拒绝且不泄露任何细节。
            if !state_valid {
                warn!(
                    path = %path,
                    has_code,
                    has_state,
                    has_error,
                    "login callback state mismatch"
                );
                return HandledRequest::Response(
                    Response::from_string("State mismatch").with_status_code(400),
                );
            }
            if let Some(error_code) = params.get("error") {
                let error_description = params.get("error_description").map(String::as_str);
                let message = oauth_callback_error_message(error_code, error_description);
                eprintln!("OAuth callback error: {message}");
                warn!(
                    error_code,
                    has_error_description = error_description.is_some_and(|s| !s.trim().is_empty()),
                    "oauth callback returned error"
                );
                return login_error_response(
                    &message,
                    io::ErrorKind::PermissionDenied,
                    Some(error_code),
                    error_description,
                );
            }
            let code = match params.get("code") {
                Some(c) if !c.is_empty() => c.clone(),
                _ => {
                    return login_error_response(
                        "Missing authorization code. Sign-in could not be completed.",
                        io::ErrorKind::InvalidData,
                        Some("missing_authorization_code"),
                        /*error_description*/ None,
                    );
                }
            };

            // 授权码换令牌成功后的串行链路：工作区校验 → 换 API Key → 落盘 → 跳转。
            // 任一步失败都改走错误页（`login_error_response`），但已成功的步骤不回滚。
            match exchange_code_for_tokens(&opts.issuer, &opts.client_id, redirect_uri, pkce, &code)
                .await
            {
                Ok(tokens) => {
                    // 企业版工作区限制：若配置了 allowed workspace，token 必须命中其一。
                    if let Err(message) = ensure_workspace_allowed(
                        opts.forced_chatgpt_workspace_id.as_deref(),
                        &tokens.id_token,
                    ) {
                        eprintln!("Workspace restriction error: {message}");
                        return login_error_response(
                            &message,
                            io::ErrorKind::PermissionDenied,
                            Some("workspace_restriction"),
                            /*error_description*/ None,
                        );
                    }
                    // Obtain API key via token-exchange and persist
                    let api_key = obtain_api_key(&opts.issuer, &opts.client_id, &tokens.id_token)
                        .await
                        .ok();
                    if let Err(err) = persist_tokens_async(
                        &opts.codex_home,
                        api_key.clone(),
                        tokens.id_token.clone(),
                        tokens.access_token.clone(),
                        tokens.refresh_token.clone(),
                        opts.cli_auth_credentials_store_mode,
                    )
                    .await
                    {
                        eprintln!("Persist error: {err}");
                        return login_error_response(
                            "Sign-in completed but credentials could not be saved locally.",
                            io::ErrorKind::Other,
                            Some("persist_failed"),
                            Some(&err.to_string()),
                        );
                    }

                    let success_url = compose_success_url(
                        actual_port,
                        &opts.issuer,
                        &tokens.id_token,
                        &tokens.access_token,
                        opts.codex_streamlined_login,
                    );
                    match tiny_http::Header::from_bytes(&b"Location"[..], success_url.as_bytes()) {
                        Ok(header) => HandledRequest::RedirectWithHeader(header),
                        Err(_) => login_error_response(
                            "Sign-in completed but redirecting back to Codex failed.",
                            io::ErrorKind::Other,
                            Some("redirect_failed"),
                            /*error_description*/ None,
                        ),
                    }
                }
                Err(err) => {
                    eprintln!("Token exchange error: {err}");
                    error!("login callback token exchange failed");
                    login_error_response(
                        &format!("Token exchange failed: {err}"),
                        io::ErrorKind::Other,
                        Some("token_exchange_failed"),
                        /*error_description*/ None,
                    )
                }
            }
        }
        // `/success`：浏览器被 302 引导到这里展示成功页，并以此为信号结束登录。
        "/success" => {
            let use_streamlined_success = parsed_url
                .query_pairs()
                .any(|(key, value)| key == "codex_streamlined_login" && value == "true");
            let body = if use_streamlined_success {
                include_str!("assets/success.html")
            } else {
                include_str!("assets/success_legacy.html")
            };
            HandledRequest::ResponseAndExit {
                headers: match Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"text/html; charset=utf-8"[..],
                ) {
                    Ok(header) => vec![header],
                    Err(_) => Vec::new(),
                },
                body: body.as_bytes().to_vec(),
                result: Ok(()),
            }
        }
        // `/cancel`：由 `send_cancel_request` 触发，用于让占用端口的旧实例自行退出，
        // 以便新一次登录能绑定同一端口（见 `bind_server`）。
        "/cancel" => HandledRequest::ResponseAndExit {
            headers: Vec::new(),
            body: b"Login cancelled".to_vec(),
            result: Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Login cancelled",
            )),
        },
        _ => HandledRequest::Response(Response::from_string("Not Found").with_status_code(404)),
    }
}

/// tiny_http filters `Connection` headers out of `Response` objects, so using
/// `req.respond` never informs the client (or the library) that a keep-alive
/// socket should be closed. That leaves the per-connection worker parked in a
/// loop waiting for more requests, which in turn causes the next login attempt
/// to hang on the old connection. This helper bypasses tiny_http’s response
/// machinery: it extracts the raw writer, prints the HTTP response manually,
/// and always appends `Connection: close`, ensuring the socket is closed from
/// the server side. Ideally, tiny_http would provide an API to control
/// server-side connection persistence, but it does not.
///
/// 绕开 `tiny_http` 的应答机制手写 HTTP 响应，强制加 `Connection: close`。
/// 背景：tiny_http 会过滤掉 `Connection` 头，用 `req.respond` 无法告知客户端关闭
/// keep-alive 连接，导致该连接的 worker 一直挂着等后续请求，使下一次登录卡在旧连接上。
/// 这是 tiny_http 缺少「服务端控制连接保持」API 的无奈变通。
fn send_response_with_disconnect(
    req: Request,
    mut headers: Vec<Header>,
    body: Vec<u8>,
) -> io::Result<()> {
    let status = StatusCode(200);
    let mut writer = req.into_writer();
    let reason = status.default_reason_phrase();
    write!(writer, "HTTP/1.1 {} {}\r\n", status.0, reason)?;
    headers.retain(|h| !h.field.equiv("Connection"));
    if let Ok(close_header) = Header::from_bytes(&b"Connection"[..], &b"close"[..]) {
        headers.push(close_header);
    }

    let content_length_value = format!("{}", body.len());
    if let Ok(content_length_header) =
        Header::from_bytes(&b"Content-Length"[..], content_length_value.as_bytes())
    {
        headers.push(content_length_header);
    }

    for header in headers {
        write!(
            writer,
            "{}: {}\r\n",
            header.field.as_str(),
            header.value.as_str()
        )?;
    }

    writer.write_all(b"\r\n")?;
    writer.write_all(&body)?;
    writer.flush()
}

// 构造 OAuth `/oauth/authorize` 授权 URL（浏览器首跳目标）。
// 关键参数：PKCE 的 `code_challenge`（S256）、`state`、以及 Codex 专属的
// `codex_cli_simplified_flow` / `id_token_add_organizations` 等开关；
// 配了工作区限制时追加 `allowed_workspace_id`。
fn build_authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
    forced_chatgpt_workspace_ids: Option<&[String]>,
) -> String {
    let mut query = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        (
            "scope".to_string(),
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_string(),
        ),
        (
            "code_challenge".to_string(),
            pkce.code_challenge.to_string(),
        ),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("id_token_add_organizations".to_string(), "true".to_string()),
        ("codex_cli_simplified_flow".to_string(), "true".to_string()),
        ("state".to_string(), state.to_string()),
        ("originator".to_string(), originator().value),
    ];
    if let Some(workspace_ids) = forced_chatgpt_workspace_ids {
        query.push(("allowed_workspace_id".to_string(), workspace_ids.join(",")));
    }
    let qs = query
        .into_iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(&v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{issuer}/oauth/authorize?{qs}")
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// 向疑似仍在运行的旧登录服务器发一个 `GET /cancel`，请它退出以腾出端口。
// 手写最小 HTTP 报文 + 短超时，避免引入完整 HTTP 客户端，且不阻塞过久。
fn send_cancel_request(port: u16) -> io::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    stream.write_all(b"GET /cancel HTTP/1.1\r\n")?;
    stream.write_all(format!("Host: 127.0.0.1:{port}\r\n").as_bytes())?;
    stream.write_all(b"Connection: close\r\n\r\n")?;

    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    Ok(())
}

// 绑定本地端口，处理「端口被占用」的常见场景。
// 策略：优先用请求端口；若 AddrInUse，先尝试 `/cancel` 掉旧实例并带退避重试，
// 重试 `MAX_ATTEMPTS` 次仍失败时，默认端口会回退到 `FALLBACK_PORT` 再试一轮。
// 仅 `is_addr_in_use` 才重试，其它绑定错误直接返回。
fn bind_server(port: u16) -> io::Result<Server> {
    let preferred_bind_address = format!("127.0.0.1:{port}");
    let fallback_bind_address = format!("127.0.0.1:{FALLBACK_PORT}");
    let mut bind_address = preferred_bind_address.clone();
    let mut cancel_attempted = false;
    let mut attempts = 0;
    let mut using_fallback_port = false;
    const MAX_ATTEMPTS: u32 = 10;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    loop {
        match Server::http(&bind_address) {
            Ok(server) => return Ok(server),
            Err(err) => {
                attempts += 1;
                let is_addr_in_use = err
                    .downcast_ref::<io::Error>()
                    .map(|io_err| io_err.kind() == io::ErrorKind::AddrInUse)
                    .unwrap_or(false);

                // If the address is in use, there may be another instance of the login server
                // running. Attempt to cancel it and retry before falling back.
                if is_addr_in_use {
                    if !cancel_attempted && !using_fallback_port {
                        cancel_attempted = true;
                        if let Err(cancel_err) = send_cancel_request(port) {
                            eprintln!("Failed to cancel previous login server: {cancel_err}");
                        }
                    }

                    thread::sleep(RETRY_DELAY);

                    if attempts >= MAX_ATTEMPTS {
                        if port == DEFAULT_PORT && !using_fallback_port {
                            warn!(
                                %preferred_bind_address,
                                %fallback_bind_address,
                                "default login callback port is unavailable; falling back to the registered fallback port"
                            );
                            bind_address = fallback_bind_address.clone();
                            attempts = 0;
                            using_fallback_port = true;
                            continue;
                        }

                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("Port {bind_address} is already in use"),
                        ));
                    }

                    continue;
                }

                return Err(io::Error::other(err));
            }
        }
    }
}

/// Tokens returned by the OAuth authorization-code exchange.
/// 授权码交换后拿到的三件套令牌：`id_token`（含账户/工作区声明）、
/// `access_token`（调用后端）、`refresh_token`（续期）。
pub(crate) struct ExchangedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenEndpointErrorDetail {
    error_code: Option<String>,
    error_message: Option<String>,
    display_message: String,
}

impl std::fmt::Display for TokenEndpointErrorDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display_message.fmt(f)
    }
}

const REDACTED_URL_VALUE: &str = "<redacted>";
// [引用范围] 仅供本文件 `redact_sensitive_query_value` 使用。
// 出现在 URL query 中的敏感参数名清单：凡命中（大小写不敏感）即在日志里替换为
// `<redacted>`。新增任何会携带密钥/令牌的查询参数时，务必同步加入此表。
const SENSITIVE_URL_QUERY_KEYS: &[&str] = &[
    "access_token",
    "api_key",
    "client_secret",
    "code",
    "code_verifier",
    "id_token",
    "key",
    "refresh_token",
    "requested_token",
    "state",
    "subject_token",
    "token",
];

fn redact_sensitive_query_value(key: &str, value: &str) -> String {
    if SENSITIVE_URL_QUERY_KEYS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(key))
    {
        REDACTED_URL_VALUE.to_string()
    } else {
        value.to_string()
    }
}

/// Redacts URL components that commonly carry auth secrets while preserving the host/path shape.
///
/// This keeps developer-facing logs useful for debugging transport failures without persisting
/// tokens, callback codes, fragments, or embedded credentials.
///
/// 对 URL 中常见的密钥载体做打码，同时保留 host/path 结构以便排障。
/// 处理项：清空内嵌的 user/password、丢弃 fragment（`#...`，可能含 token）、
/// 按 `SENSITIVE_URL_QUERY_KEYS` 逐项打码 query。就地修改传入的 `url`。
fn redact_sensitive_url_parts(url: &mut url::Url) {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);

    let query_pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let key = key.into_owned();
            let value = value.into_owned();
            (key.clone(), redact_sensitive_query_value(&key, &value))
        })
        .collect::<Vec<_>>();

    if query_pairs.is_empty() {
        url.set_query(None);
        return;
    }

    let redacted_query = query_pairs
        .into_iter()
        .fold(
            url::form_urlencoded::Serializer::new(String::new()),
            |mut serializer, (key, value)| {
                serializer.append_pair(&key, &value);
                serializer
            },
        )
        .finish();
    url.set_query(Some(&redacted_query));
}

/// Redacts any URL attached to a reqwest transport error before it is logged or returned.
/// 在记录或返回 reqwest 传输错误前，对其携带的 URL 做脱敏（错误里常含完整请求 URL）。
fn redact_sensitive_error_url(mut err: reqwest::Error) -> reqwest::Error {
    if let Some(url) = err.url_mut() {
        redact_sensitive_url_parts(url);
    }
    err
}

/// Sanitizes a free-form URL string for structured logging.
///
/// This is used for caller-supplied issuer values, which may contain credentials or query
/// parameters on non-default deployments.
///
/// 把任意 URL 字符串脱敏成可安全写日志的形式；解析失败时返回 `<invalid-url>`。
/// 主要用于调用方传入的 issuer——非默认部署下它可能带有凭证或查询参数。
fn sanitize_url_for_logging(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut url) => {
            redact_sensitive_url_parts(&mut url);
            url.to_string()
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}
/// Exchanges an authorization code for tokens.
///
/// The returned error remains suitable for user-facing CLI/browser surfaces, so backend-provided
/// non-JSON error text is preserved there. Structured logging stays narrower: it logs reviewed
/// fields from parsed token responses and redacted transport errors, but does not log the final
/// callback-layer `%err` string.
///
/// 用授权码向 `/oauth/token` 换取令牌（authorization_code 授权类型，带 PKCE verifier）。
/// 同时被浏览器回调与设备码流程复用。
///
/// 错误分两路（呼应文件头的双轨日志原则）：返回给调用方的 `io::Error` 保留后端原始文本
/// （含非 JSON 报文）以便 CLI/浏览器展示；而 `tracing` 只记审查过的字段和「打码后的」
/// 传输错误，不打印最终错误字符串。传输错误经 `redact_sensitive_error_url` 脱敏后才记录。
pub(crate) async fn exchange_code_for_tokens(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    code: &str,
) -> io::Result<ExchangedTokens> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        id_token: String,
        access_token: String,
        refresh_token: String,
    }

    let client = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let token_endpoint = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    info!(
        issuer = %sanitize_url_for_logging(issuer),
        token_endpoint = %sanitize_url_for_logging(&token_endpoint),
        redirect_uri = %redirect_uri,
        "starting oauth token exchange"
    );
    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(code),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(client_id),
            urlencoding::encode(&pkce.code_verifier)
        ))
        .send()
        .await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(error) => {
            let error = redact_sensitive_error_url(error);
            error!(
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                is_request = error.is_request(),
                error = %error,
                "oauth token exchange transport failure"
            );
            return Err(io::Error::other(error));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.map_err(io::Error::other)?;
        let detail = parse_token_endpoint_error(&body);
        warn!(
            %status,
            error_code = detail.error_code.as_deref().unwrap_or("unknown"),
            error_message = detail.error_message.as_deref().unwrap_or("unknown"),
            "oauth token exchange returned non-success status"
        );
        return Err(io::Error::other(format!(
            "token endpoint returned status {status}: {detail}"
        )));
    }

    let tokens: TokenResponse = resp.json().await.map_err(io::Error::other)?;
    info!(%status, "oauth token exchange succeeded");
    Ok(ExchangedTokens {
        id_token: tokens.id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
    })
}

/// Persists exchanged credentials using the configured local auth store, then
/// best-effort revokes any superseded managed ChatGPT tokens.
///
/// 把换来的凭证写入本地 auth 存储（文件或 keyring，由 store mode 决定），随后
/// 尽力（best-effort）吊销被本次登录取代的旧 ChatGPT 令牌。
///
/// @param api_key - 可选，token-exchange 得到的 API Key（设备码流程不带）。
/// 副作用：读旧 auth、解析 JWT 声明、落盘新凭证、可能发起一次吊销网络请求。
/// 异常处理：读旧 auth 或吊销失败只 `warn!` 不影响登录成功（凭证已落盘）。
pub(crate) async fn persist_tokens_async(
    codex_home: &Path,
    api_key: Option<String>,
    id_token: String,
    access_token: String,
    refresh_token: String,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> io::Result<()> {
    // Reuse existing synchronous logic but run it off the async runtime.
    let codex_home = codex_home.to_path_buf();
    let (previous_auth, auth) = tokio::task::spawn_blocking(move || {
        let previous_auth = match load_auth_dot_json(&codex_home, auth_credentials_store_mode) {
            Ok(auth) => auth,
            Err(err) => {
                warn!("failed to load previous auth before saving new login: {err}");
                None
            }
        };
        let mut tokens = TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token).map_err(io::Error::other)?,
            access_token,
            refresh_token,
            account_id: None,
        };
        if let Some(acc) = jwt_auth_claims(&id_token)
            .get("chatgpt_account_id")
            .and_then(|v| v.as_str())
        {
            tokens.account_id = Some(acc.to_string());
        }
        let auth = AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: api_key,
            tokens: Some(tokens),
            last_refresh: Some(Utc::now()),
            agent_identity: None,
        };
        save_auth(&codex_home, &auth, auth_credentials_store_mode)?;
        Ok::<_, io::Error>((previous_auth, auth))
    })
    .await
    .map_err(|e| io::Error::other(format!("persist task failed: {e}")))??;

    // 仅当旧令牌确实被「取代」时才吊销：若新旧 refresh token 相同（同账户复用），
    // 吊销会连带废掉刚保存的新令牌，故 `should_revoke_auth_tokens` 会先排除这种情况。
    if should_revoke_auth_tokens(previous_auth.as_ref(), &auth)
        && let Err(err) = revoke_auth_tokens(previous_auth.as_ref()).await
    {
        warn!("failed to revoke superseded auth tokens after login: {err}");
    }

    Ok(())
}

// 根据 JWT 声明拼出本地 `/success` 重定向 URL。
// 从 id_token/access_token 中提取 org/project/plan 等信息并作为 query 透传，
// 供成功页据此渲染（如是否需要 onboarding `needs_setup`）。
fn compose_success_url(
    port: u16,
    issuer: &str,
    id_token: &str,
    access_token: &str,
    codex_streamlined_login: bool,
) -> String {
    let token_claims = jwt_auth_claims(id_token);
    let access_claims = jwt_auth_claims(access_token);

    let org_id = token_claims
        .get("organization_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let project_id = token_claims
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let completed_onboarding = token_claims
        .get("completed_platform_onboarding")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let is_org_owner = token_claims
        .get("is_org_owner")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let needs_setup = (!completed_onboarding) && is_org_owner;
    let plan_type = access_claims
        .get("chatgpt_plan_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let platform_url = if issuer == DEFAULT_ISSUER {
        "https://platform.openai.com"
    } else {
        "https://platform.api.openai.org"
    };

    let mut params = vec![
        ("id_token", id_token.to_string()),
        ("needs_setup", needs_setup.to_string()),
        ("org_id", org_id.to_string()),
        ("project_id", project_id.to_string()),
        ("plan_type", plan_type.to_string()),
        ("platform_url", platform_url.to_string()),
    ];
    if codex_streamlined_login {
        params.push(("codex_streamlined_login", "true".to_string()));
    }
    let qs = params
        .drain(..)
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(&v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("http://localhost:{port}/success?{qs}")
}

// 解析 JWT，仅取出自定义命名空间 `https://api.openai.com/auth` 下的声明对象。
// 不校验签名（仅本地读 payload）；任何格式/解码错误都打印到 stderr 并返回空 Map，
// 让调用方按「无声明」降级处理而非崩溃。
fn jwt_auth_claims(jwt: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut parts = jwt.split('.');
    let (_h, payload_b64, _s) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => {
            eprintln!("Invalid JWT format while extracting claims");
            return serde_json::Map::new();
        }
    };
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(mut v) => {
                if let Some(obj) = v
                    .get_mut("https://api.openai.com/auth")
                    .and_then(|x| x.as_object_mut())
                {
                    return obj.clone();
                }
                eprintln!("JWT payload missing expected 'https://api.openai.com/auth' object");
            }
            Err(e) => {
                eprintln!("Failed to parse JWT JSON payload: {e}");
            }
        },
        Err(e) => {
            eprintln!("Failed to base64url-decode JWT payload: {e}");
        }
    }
    serde_json::Map::new()
}

/// Validates the ID token against an optional workspace restriction.
/// 校验 id_token 是否落在允许的工作区内。
/// `expected` 为 `None` 时直接放行；否则要求 token 的 `chatgpt_account_id` 声明
/// 命中列表之一，缺失声明或不匹配都返回带说明的 `Err`（用作面向用户的错误文案）。
pub(crate) fn ensure_workspace_allowed(
    expected: Option<&[String]>,
    id_token: &str,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };

    let claims = jwt_auth_claims(id_token);
    let Some(actual) = claims.get("chatgpt_account_id").and_then(JsonValue::as_str) else {
        return Err("Login is restricted to a specific workspace, but the token did not include an chatgpt_account_id claim.".to_string());
    };

    if expected.iter().any(|workspace_id| workspace_id == actual) {
        Ok(())
    } else {
        Err(format!(
            "Login is restricted to workspace id(s) {}.",
            expected.join(", ")
        ))
    }
}

/// Builds a terminal callback response for login failures.
/// 为登录失败构造「终止型」回调响应：渲染品牌错误页作为 body，
/// 并把 `kind`+`message` 包成 `io::Error` 作为整个登录的失败结果（主循环据此退出）。
fn login_error_response(
    message: &str,
    kind: io::ErrorKind,
    error_code: Option<&str>,
    error_description: Option<&str>,
) -> HandledRequest {
    let mut headers = Vec::new();
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        headers.push(header);
    }
    let body = render_login_error_page(message, error_code, error_description);
    HandledRequest::ResponseAndExit {
        headers,
        body,
        result: Err(io::Error::new(kind, message.to_string())),
    }
}

/// Returns true when the OAuth callback represents a missing Codex entitlement.
/// 判断回调错误是否为「该工作区未开通 Codex 权限」这一特例
/// （`access_denied` + 描述含 `missing_codex_entitlement`），以便给出专门的引导文案。
fn is_missing_codex_entitlement_error(error_code: &str, error_description: Option<&str>) -> bool {
    error_code == "access_denied"
        && error_description.is_some_and(|description| {
            description
                .to_ascii_lowercase()
                .contains("missing_codex_entitlement")
        })
}

/// Converts OAuth callback errors into a user-facing message.
/// 把回调的 `error`/`error_description` 转成面向用户的提示文案：
/// 未开通 Codex 走专门引导；否则优先用 description，再退化到 error code。
fn oauth_callback_error_message(error_code: &str, error_description: Option<&str>) -> String {
    if is_missing_codex_entitlement_error(error_code, error_description) {
        return "Codex is not enabled for your workspace. Contact your workspace administrator to request access to Codex.".to_string();
    }

    if let Some(description) = error_description
        && !description.trim().is_empty()
    {
        return format!("Sign-in failed: {description}");
    }

    format!("Sign-in failed: {error_code}")
}

/// Extracts token endpoint error detail for both structured logging and caller-visible errors.
///
/// Parsed JSON fields are safe to log individually. If the response is not JSON, the raw body is
/// preserved only for the returned error path so the CLI/browser can still surface the backend
/// detail, while the structured log path continues to use the explicitly parsed safe fields above.
///
/// 解析 token 端点的错误响应，兼容多种后端格式：
///   `{error, error_description}` / `{error:{code, message}}` / 仅 `{error}` / 纯文本。
/// 拆出的 JSON 字段可安全单独入日志；非 JSON 原文只放进返回的 `display_message`
/// （供 CLI/浏览器展示后端细节），结构化日志侧仍只用上面显式解析的安全字段。
fn parse_token_endpoint_error(body: &str) -> TokenEndpointErrorDetail {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return TokenEndpointErrorDetail {
            error_code: None,
            error_message: None,
            display_message: "unknown error".to_string(),
        };
    }

    let parsed = serde_json::from_str::<JsonValue>(trimmed).ok();
    if let Some(json) = parsed {
        let error_code = json
            .get("error")
            .and_then(JsonValue::as_str)
            .filter(|error_code| !error_code.trim().is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                json.get("error")
                    .and_then(JsonValue::as_object)
                    .and_then(|error_obj| error_obj.get("code"))
                    .and_then(JsonValue::as_str)
                    .filter(|code| !code.trim().is_empty())
                    .map(ToString::to_string)
            });
        if let Some(description) = json.get("error_description").and_then(JsonValue::as_str)
            && !description.trim().is_empty()
        {
            return TokenEndpointErrorDetail {
                error_code,
                error_message: Some(description.to_string()),
                display_message: description.to_string(),
            };
        }
        if let Some(error_obj) = json.get("error")
            && let Some(message) = error_obj.get("message").and_then(JsonValue::as_str)
            && !message.trim().is_empty()
        {
            return TokenEndpointErrorDetail {
                error_code,
                error_message: Some(message.to_string()),
                display_message: message.to_string(),
            };
        }
        if let Some(error_code) = error_code {
            return TokenEndpointErrorDetail {
                display_message: error_code.clone(),
                error_code: Some(error_code),
                error_message: None,
            };
        }
    }

    // Preserve non-JSON token-endpoint bodies for the returned error so CLI/browser flows still
    // surface the backend detail users and admins need, but keep that text out of structured logs
    // by only logging explicitly parsed fields above and avoiding `%err` logging at the callback
    // layer.
    TokenEndpointErrorDetail {
        error_code: None,
        error_message: None,
        display_message: trimmed.to_string(),
    }
}

/// Renders the branded error page used by callback failures.
/// 渲染回调失败时展示的品牌错误页。
/// 对「未开通 Codex」特例使用专门的标题/说明/引导文案，其余情况用通用文案；
/// 所有动态字段都先经 `html_escape` 转义后再填入模板，防止 HTML 注入。
fn render_login_error_page(
    message: &str,
    error_code: Option<&str>,
    error_description: Option<&str>,
) -> Vec<u8> {
    let code = error_code.unwrap_or("unknown_error");
    let (title, display_message, display_description, help_text) =
        if is_missing_codex_entitlement_error(code, error_description) {
            (
                "You do not have access to Codex".to_string(),
                "This account is not currently authorized to use Codex in this workspace."
                    .to_string(),
                "Contact your workspace administrator to request access to Codex.".to_string(),
                "Contact your workspace administrator to get access to Codex, then return to Codex and try again."
                    .to_string(),
            )
        } else {
            (
                "Sign-in could not be completed".to_string(),
                message.to_string(),
                error_description.unwrap_or(message).to_string(),
                "Return to Codex to retry, switch accounts, or contact your workspace admin if access is restricted."
                    .to_string(),
            )
        };
    LOGIN_ERROR_PAGE_TEMPLATE
        .render([
            ("error_title", html_escape(&title)),
            ("error_message", html_escape(&display_message)),
            ("error_code", html_escape(code)),
            ("error_description", html_escape(&display_description)),
            ("error_help", html_escape(&help_text)),
        ])
        .unwrap_or_else(|err| panic!("login error page template must render: {err}"))
        .into_bytes()
}

/// Escapes error strings before inserting them into HTML.
fn html_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Exchanges an authenticated ID token for an API-key style access token.
/// 用已认证的 id_token 通过 OAuth token-exchange 换取「API Key 形态」的 access token。
/// 即把 ChatGPT 登录态换成可像 API Key 一样使用的凭证，落盘后供后端调用。
pub(crate) async fn obtain_api_key(
    issuer: &str,
    client_id: &str,
    id_token: &str,
) -> io::Result<String> {
    // Token exchange for an API key access token
    #[derive(serde::Deserialize)]
    struct ExchangeResp {
        access_token: String,
    }
    let client = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let token_endpoint = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
            urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
            urlencoding::encode(client_id),
            urlencoding::encode("openai-api-key"),
            urlencoding::encode(id_token),
            urlencoding::encode("urn:ietf:params:oauth:token-type:id_token")
        ))
        .send()
        .await
        .map_err(io::Error::other)?;
    if !resp.status().is_success() {
        return Err(io::Error::other(format!(
            "api key exchange failed with status {}",
            resp.status()
        )));
    }
    let body: ExchangeResp = resp.json().await.map_err(io::Error::other)?;
    Ok(body.access_token)
}
#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use anyhow::Context;
    use base64::Engine;
    use codex_app_server_protocol::AuthMode;
    use codex_config::types::AuthCredentialsStoreMode;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use crate::auth::AuthDotJson;
    use crate::auth::REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR;
    use crate::auth::load_auth_dot_json;
    use crate::auth::save_auth;
    use crate::token_data::TokenData;
    use crate::token_data::parse_chatgpt_jwt_claims;
    use core_test_support::skip_if_no_network;
    use pretty_assertions::assert_eq;

    use super::DEFAULT_ISSUER;
    use super::TokenEndpointErrorDetail;
    use super::compose_success_url;
    use super::html_escape;
    use super::is_missing_codex_entitlement_error;
    use super::parse_token_endpoint_error;
    use super::persist_tokens_async;
    use super::redact_sensitive_query_value;
    use super::redact_sensitive_url_parts;
    use super::render_login_error_page;
    use super::sanitize_url_for_logging;

    #[serial_test::serial(logout_revoke)]
    #[tokio::test]
    async fn persist_tokens_async_revokes_previous_auth_without_failing_login() -> anyhow::Result<()>
    {
        skip_if_no_network!(Ok(()));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/revoke"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "error": {
                    "message": "revoke failed"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let _env_guard = EnvGuard::set(
            REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR,
            format!("{}/oauth/revoke", server.uri()),
        );

        let codex_home = tempdir()?;
        save_auth(
            codex_home.path(),
            &chatgpt_auth("old-access", "old-refresh", "old-account"),
            AuthCredentialsStoreMode::File,
        )?;

        persist_tokens_async(
            codex_home.path(),
            /*api_key*/ None,
            jwt_for_account("new-account"),
            "new-access".to_string(),
            "new-refresh".to_string(),
            AuthCredentialsStoreMode::File,
        )
        .await?;

        let auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)?
            .context("auth.json should exist after login")?;
        assert_eq!(
            auth.tokens.context("new tokens should be persisted")?,
            TokenData {
                id_token: parse_chatgpt_jwt_claims(&jwt_for_account("new-account"))
                    .expect("new JWT should parse"),
                access_token: "new-access".to_string(),
                refresh_token: "new-refresh".to_string(),
                account_id: Some("new-account".to_string()),
            }
        );

        let requests = server
            .received_requests()
            .await
            .context("failed to fetch revoke requests")?;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .body_json::<Value>()
                .context("revoke request should be JSON")?,
            json!({
                "token": "old-refresh",
                "token_type_hint": "refresh_token",
                "client_id": crate::auth::CLIENT_ID,
            })
        );
        server.verify().await;
        Ok(())
    }

    #[serial_test::serial(logout_revoke)]
    #[tokio::test]
    async fn persist_tokens_async_does_not_revoke_reused_refresh_token() -> anyhow::Result<()> {
        skip_if_no_network!(Ok(()));

        let server = MockServer::start().await;
        let _env_guard = EnvGuard::set(
            REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR,
            format!("{}/oauth/revoke", server.uri()),
        );

        let codex_home = tempdir()?;
        save_auth(
            codex_home.path(),
            &chatgpt_auth("old-access", "shared-refresh", "old-account"),
            AuthCredentialsStoreMode::File,
        )?;

        persist_tokens_async(
            codex_home.path(),
            /*api_key*/ None,
            jwt_for_account("new-account"),
            "new-access".to_string(),
            "shared-refresh".to_string(),
            AuthCredentialsStoreMode::File,
        )
        .await?;

        let requests = server
            .received_requests()
            .await
            .context("failed to fetch revoke requests")?;
        assert_eq!(requests.len(), 0);
        Ok(())
    }

    fn chatgpt_auth(access_token: &str, refresh_token: &str, account_id: &str) -> AuthDotJson {
        AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: parse_chatgpt_jwt_claims(&jwt_for_account(account_id))
                    .expect("test JWT should parse"),
                access_token: access_token.to_string(),
                refresh_token: refresh_token.to_string(),
                account_id: Some(account_id.to_string()),
            }),
            last_refresh: None,
            agent_identity: None,
        }
    }

    fn jwt_for_account(account_id: &str) -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header_b64 = encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload_b64 = encode(
            serde_json::to_string(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id,
                }
            }))
            .expect("payload should serialize")
            .as_bytes(),
        );
        let signature_b64 = encode(b"sig");
        format!("{header_b64}.{payload_b64}.{signature_b64}")
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: String) -> Self {
            let original = std::env::var_os(key);
            // SAFETY: this test executes serially with other revoke tests.
            unsafe {
                std::env::set_var(key, &value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: the guard restores the original environment before other revoke tests run.
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn parse_token_endpoint_error_prefers_error_description() {
        let detail = parse_token_endpoint_error(
            r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
        );

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("invalid_grant".to_string()),
                error_message: Some("refresh token expired".to_string()),
                display_message: "refresh token expired".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_reads_nested_error_message_and_code() {
        let detail = parse_token_endpoint_error(
            r#"{"error":{"code":"proxy_auth_required","message":"proxy authentication required"}}"#,
        );

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("proxy_auth_required".to_string()),
                error_message: Some("proxy authentication required".to_string()),
                display_message: "proxy authentication required".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_falls_back_to_error_code() {
        let detail = parse_token_endpoint_error(r#"{"error":"temporarily_unavailable"}"#);

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("temporarily_unavailable".to_string()),
                error_message: None,
                display_message: "temporarily_unavailable".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_preserves_plain_text_for_display() {
        let detail = parse_token_endpoint_error("service unavailable");

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: None,
                error_message: None,
                display_message: "service unavailable".to_string(),
            }
        );
    }

    #[test]
    fn redact_sensitive_query_value_only_scrubs_known_keys() {
        assert_eq!(
            redact_sensitive_query_value("code", "abc123"),
            "<redacted>".to_string()
        );
        assert_eq!(
            redact_sensitive_query_value("redirect_uri", "http://localhost:1455/auth/callback"),
            "http://localhost:1455/auth/callback".to_string()
        );
    }

    #[test]
    fn redact_sensitive_url_parts_preserves_safe_url_shape() {
        let mut url = url::Url::parse(
            "https://user:pass@auth.openai.com/oauth/token?code=abc123&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback#frag",
        )
        .expect("valid url");

        redact_sensitive_url_parts(&mut url);

        assert_eq!(
            url.as_str(),
            "https://auth.openai.com/oauth/token?code=%3Credacted%3E&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        );
    }

    #[test]
    fn sanitize_url_for_logging_redacts_sensitive_issuer_parts() {
        let redacted =
            sanitize_url_for_logging("https://user:pass@example.com/base?token=abc123&env=prod");

        assert_eq!(
            redacted,
            "https://example.com/base?token=%3Credacted%3E&env=prod".to_string()
        );
    }

    #[test]
    fn compose_success_url_omits_streamlined_success_by_default() {
        let url = url::Url::parse(&compose_success_url(
            /*port*/ 1455,
            DEFAULT_ISSUER,
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            /*codex_streamlined_login*/ false,
        ))
        .expect("success url should parse");

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "codex_streamlined_login"),
            None
        );
    }

    #[test]
    fn compose_success_url_includes_streamlined_success_when_requested() {
        let url = url::Url::parse(&compose_success_url(
            /*port*/ 1455,
            DEFAULT_ISSUER,
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            /*codex_streamlined_login*/ true,
        ))
        .expect("success url should parse");

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "codex_streamlined_login")
                .map(|(_, value)| value.into_owned()),
            Some("true".to_string())
        );
    }

    #[test]
    fn render_login_error_page_escapes_dynamic_fields() {
        let body = String::from_utf8(render_login_error_page(
            "<bad>",
            Some("code&value"),
            Some("\"quoted\""),
        ))
        .expect("login error page should be utf-8");

        assert!(body.contains(&html_escape("Sign-in could not be completed")));
        assert!(body.contains("&lt;bad&gt;"));
        assert!(body.contains("code&amp;value"));
        assert!(body.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn render_login_error_page_uses_entitlement_copy() {
        let error_description = Some("missing_codex_entitlement");
        assert!(is_missing_codex_entitlement_error(
            "access_denied",
            error_description
        ));

        let body = String::from_utf8(render_login_error_page(
            "access denied",
            Some("access_denied"),
            error_description,
        ))
        .expect("login error page should be utf-8");

        assert!(body.contains("You do not have access to Codex"));
        assert!(body.contains("Contact your workspace administrator"));
        assert!(!body.contains("missing_codex_entitlement"));
    }
}
