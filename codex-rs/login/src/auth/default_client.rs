//! Default Codex HTTP client: shared `User-Agent`, `originator`, optional residency header, and
//! reqwest/`CodexHttpClient` construction.
//!
//! Use [`crate::default_client`] or [`codex_login::default_client`] from other crates in this
//! workspace.
//!
//! 【文件职责】构建 Codex 全局统一的 HTTP 客户端，集中管理三个跨请求共享的标识：
//!   - `User-Agent`：标明 Codex 版本 / 操作系统 / 终端环境（可由 MCP 追加后缀）。
//!   - `originator`：调用来源标识（CLI / TUI / VSCode / Atlas 等），后端据此区分一方/三方。
//!   - 数据驻留头（residency）：FedRAMP 等场景下强制请求落在指定区域（如 `us`）。
//!
//! 【架构位置】
//!   层级：网络基础设施层（认证子系统下属）
//!   上游：`auth/manager.rs`（刷新令牌时复用此客户端）、core 的请求循环（携带认证头发请求）
//!   下游：`codex_client`（实际的 reqwest 封装、自定义 CA、Cloudflare cookie 处理）
//!
//! 【为什么用全局单例】`originator` / residency / UA 后缀都是进程级、一次设定后全程不变的值，
//!   用 `LazyLock<RwLock<...>>` 收口，避免每个调用点各自传参导致漏配。代价是测试隔离性差，
//!   见 `USER_AGENT_SUFFIX` 注释里关于"全局静态不理想"的权衡说明。
//!
//! 【阅读建议】主入口是 `create_client()` → `build_reqwest_client()`；
//!   `originator()` / `get_codex_user_agent()` / `default_headers()` 负责拼装三个共享头；
//!   `sanitize_user_agent()` 是处理非法字符的兜底工具，可最后看。

use codex_client::BuildCustomCaTransportError;
use codex_client::CodexHttpClient;
pub use codex_client::CodexRequestBuilder;
use codex_client::build_reqwest_client_with_custom_ca;
use codex_client::with_chatgpt_cloudflare_cookie_store;
use codex_terminal_detection::user_agent;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::RwLock;

/// Set this to add a suffix to the User-Agent string.
///
/// It is not ideal that we're using a global singleton for this.
/// This is primarily designed to differentiate MCP clients from each other.
/// Because there can only be one MCP server per process, it should be safe for this to be a global static.
/// However, future users of this should use this with caution as a result.
/// In addition, we want to be confident that this value is used for ALL clients and doing that requires a
/// lot of wiring and it's easy to miss code paths by doing so.
/// See https://github.com/openai/codex/pull/3388/files for an example of what that would look like.
/// Finally, we want to make sure this is set for ALL mcp clients without needing to know a special env var
/// or having to set data that they already specified in the mcp initialize request somewhere else.
///
/// A space is automatically added between the suffix and the rest of the User-Agent string.
/// The full user agent string is returned from the mcp initialize response.
/// Parenthesis will be added by Codex. This should only specify what goes inside of the parenthesis.
// [引用范围 · 全局单例] 由 MCP initialize 流程写入一次，之后所有 HTTP 客户端共享。
// 中文要点：用来在 UA 末尾追加括号后缀以区分不同 MCP 客户端；之所以做成进程级全局，
// 是因为一个进程至多一个 MCP server，且想保证所有客户端都带上它而无需逐处接线（详见上方英文权衡）。
pub static USER_AGENT_SUFFIX: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
// 默认调用来源标识：未显式设置且无环境变量覆盖时，一律标记为 Rust 版 CLI。
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
pub const CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
pub const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";

pub use codex_config::ResidencyRequirement;

#[derive(Debug, Clone)]
pub struct Originator {
    pub value: String,
    pub header_value: HeaderValue,
}
// [引用范围 · 全局单例] 缓存解析后的 originator（含预构建的 header 值），首次设置后不再变更。
static ORIGINATOR: LazyLock<RwLock<Option<Originator>>> = LazyLock::new(|| RwLock::new(None));
// [引用范围 · 全局单例] 数据驻留要求；为 `Some` 时给每个请求注入驻留头（如强制落在美区）。
static REQUIREMENTS_RESIDENCY: LazyLock<RwLock<Option<ResidencyRequirement>>> =
    LazyLock::new(|| RwLock::new(None));

#[derive(Debug)]
pub enum SetOriginatorError {
    InvalidHeaderValue,
    AlreadyInitialized,
}

// 解析最终生效的 originator 字符串并预构建其 header 值。
// 优先级：环境变量覆盖 > 调用方传入的 `provided` > 默认 `codex_cli_rs`。
// 若解析出的值含非法 header 字符，则降级回默认值而非报错（保证本函数不失败）。
fn get_originator_value(provided: Option<String>) -> Originator {
    let value = std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR)
        .ok()
        .or(provided)
        .unwrap_or(DEFAULT_ORIGINATOR.to_string());

    match HeaderValue::from_str(&value) {
        Ok(header_value) => Originator {
            value,
            header_value,
        },
        Err(e) => {
            tracing::error!("Unable to turn originator override {value} into header value: {e}");
            Originator {
                value: DEFAULT_ORIGINATOR.to_string(),
                header_value: HeaderValue::from_static(DEFAULT_ORIGINATOR),
            }
        }
    }
}

// 设置全局 originator，只能成功设置一次。
// 失败语义：值含非法 header 字符 → `InvalidHeaderValue`；已被设置过 → `AlreadyInitialized`。
// 「只设一次」是有意约束：originator 代表进程身份，运行中途改变会让后端看到不一致的来源。
pub fn set_default_originator(value: String) -> Result<(), SetOriginatorError> {
    if HeaderValue::from_str(&value).is_err() {
        return Err(SetOriginatorError::InvalidHeaderValue);
    }
    let originator = get_originator_value(Some(value));
    let Ok(mut guard) = ORIGINATOR.write() else {
        return Err(SetOriginatorError::AlreadyInitialized);
    };
    if guard.is_some() {
        return Err(SetOriginatorError::AlreadyInitialized);
    }
    *guard = Some(originator);
    Ok(())
}

pub fn set_default_client_residency_requirement(enforce_residency: Option<ResidencyRequirement>) {
    let Ok(mut guard) = REQUIREMENTS_RESIDENCY.write() else {
        tracing::warn!("Failed to acquire requirements residency lock");
        return;
    };
    *guard = enforce_residency;
}

// 读取当前 originator；若尚未设置且存在环境变量覆盖，则就地惰性初始化全局缓存。
// 三段逻辑：① 已缓存直接返回；② 仅有环境变量覆盖时填充缓存（处理与 set 的并发竞争）；
// ③ 都没有则按默认值即时构造（不写缓存，留待后续显式 set）。
pub fn originator() -> Originator {
    if let Ok(guard) = ORIGINATOR.read()
        && let Some(originator) = guard.as_ref()
    {
        return originator.clone();
    }

    if std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR).is_ok() {
        let originator = get_originator_value(/*provided*/ None);
        if let Ok(mut guard) = ORIGINATOR.write() {
            match guard.as_ref() {
                Some(originator) => return originator.clone(),
                None => *guard = Some(originator.clone()),
            }
        }
        return originator;
    }

    get_originator_value(/*provided*/ None)
}

pub fn is_first_party_originator(originator_value: &str) -> bool {
    originator_value == DEFAULT_ORIGINATOR
        || originator_value == "codex-tui"
        || originator_value == "codex_vscode"
        || originator_value.starts_with("Codex ")
}

pub fn is_first_party_chat_originator(originator_value: &str) -> bool {
    originator_value == "codex_atlas" || originator_value == "codex_chatgpt_desktop"
}

// 拼装完整 User-Agent：`<originator>/<版本> (<OS类型> <版本>; <架构>) <终端UA>` + 可选 `(<后缀>)`。
// 后缀来自 `USER_AGENT_SUFFIX`（MCP 客户端标识），自动用空格分隔并包裹括号。
// 末尾统一过一道 `sanitize_user_agent` 兜底，确保结果一定能作为合法 header 值。
pub fn get_codex_user_agent() -> String {
    let build_version = env!("CARGO_PKG_VERSION");
    let os_info = os_info::get();
    let originator = originator();
    let prefix = format!(
        "{}/{build_version} ({} {}; {}) {}",
        originator.value.as_str(),
        os_info.os_type(),
        os_info.version(),
        os_info.architecture().unwrap_or("unknown"),
        user_agent()
    );
    let suffix = USER_AGENT_SUFFIX
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let suffix = suffix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(String::new, |value| format!(" ({value})"));

    let candidate = format!("{prefix}{suffix}");
    sanitize_user_agent(candidate, &prefix)
}

/// Sanitize the user agent string.
///
/// Invalid characters are replaced with an underscore.
///
/// If the user agent fails to parse, it falls back to fallback and then to ORIGINATOR.
fn sanitize_user_agent(candidate: String, fallback: &str) -> String {
    if HeaderValue::from_str(candidate.as_str()).is_ok() {
        return candidate;
    }

    let sanitized: String = candidate
        .chars()
        .map(|ch| if matches!(ch, ' '..='~') { ch } else { '_' })
        .collect();
    if !sanitized.is_empty() && HeaderValue::from_str(sanitized.as_str()).is_ok() {
        tracing::warn!(
            "Sanitized Codex user agent because provided suffix contained invalid header characters"
        );
        sanitized
    } else if HeaderValue::from_str(fallback).is_ok() {
        tracing::warn!(
            "Falling back to base Codex user agent because provided suffix could not be sanitized"
        );
        fallback.to_string()
    } else {
        tracing::warn!(
            "Falling back to default Codex originator because base user agent string is invalid"
        );
        originator().value
    }
}

/// Create an HTTP client with default `originator` and `User-Agent` headers set.
/// 创建带默认 `originator` / `User-Agent` 头的 `CodexHttpClient`，是本模块对外的主入口。
pub fn create_client() -> CodexHttpClient {
    let inner = build_reqwest_client();
    CodexHttpClient::new(inner)
}

/// Builds the default reqwest client used for ordinary Codex HTTP traffic.
///
/// This starts from the standard Codex user agent, default headers, and sandbox-specific proxy
/// policy, then layers in shared custom CA handling from `CODEX_CA_CERTIFICATE` /
/// `SSL_CERT_FILE`. The function remains infallible for compatibility with existing call sites, so
/// a custom-CA or builder failure is logged and falls back to `reqwest::Client::new()`.
///
/// 构建普通 Codex HTTP 流量使用的默认 reqwest 客户端。
/// 设计取舍：本函数刻意「永不失败」——自定义 CA 加载或 builder 构造出错时，
/// 只记日志并逐级降级（先退到带 Cloudflare cookie 的最简 builder，再退到裸 `Client::new()`），
/// 以兼容大量既有调用点。需要拿到结构化错误的调用方应改用 `try_build_reqwest_client()`。
pub fn build_reqwest_client() -> reqwest::Client {
    try_build_reqwest_client().unwrap_or_else(|error| {
        tracing::warn!(error = %error, "failed to build default reqwest client");
        with_chatgpt_cloudflare_cookie_store(reqwest::Client::builder())
            .build()
            .unwrap_or_else(|fallback_error| {
                tracing::warn!(
                    error = %fallback_error,
                    "failed to build fallback reqwest client with ChatGPT Cloudflare cookie store"
                );
                reqwest::Client::new()
            })
    })
}

/// Tries to build the default reqwest client used for ordinary Codex HTTP traffic.
///
/// Callers that need a structured CA-loading failure instead of the legacy logged fallback can use
/// this method directly.
///
/// 与 `build_reqwest_client()` 同流程，但把 CA 加载失败作为 `Err` 返回而非降级吞掉。
pub fn try_build_reqwest_client() -> Result<reqwest::Client, BuildCustomCaTransportError> {
    let mut builder = reqwest::Client::builder().default_headers(default_headers());
    // 沙箱（seatbelt）环境下禁用代理：沙箱内不应经由外部代理出网，避免绕过隔离策略。
    if is_sandboxed() {
        builder = builder.no_proxy();
    }
    builder = with_chatgpt_cloudflare_cookie_store(builder);

    build_reqwest_client_with_custom_ca(builder)
}

// 组装每个请求都携带的默认头：`originator` + `User-Agent` +（按需）数据驻留头。
// 驻留头仅在 `REQUIREMENTS_RESIDENCY` 已设置且 headers 中尚无同名头时注入，不覆盖已有值。
pub fn default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("originator", originator().header_value);
    if let Ok(user_agent) = HeaderValue::from_str(&get_codex_user_agent()) {
        headers.insert(USER_AGENT, user_agent);
    }
    if let Ok(guard) = REQUIREMENTS_RESIDENCY.read()
        && let Some(requirement) = guard.as_ref()
        && !headers.contains_key(RESIDENCY_HEADER_NAME)
    {
        let value = match requirement {
            ResidencyRequirement::Us => HeaderValue::from_static("us"),
        };
        headers.insert(RESIDENCY_HEADER_NAME, value);
    }
    headers
}

fn is_sandboxed() -> bool {
    std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt")
}

#[cfg(test)]
#[path = "default_client_tests.rs"]
mod tests;
