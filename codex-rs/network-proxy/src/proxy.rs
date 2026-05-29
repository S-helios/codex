//! 代理主体：进程内本地代理的构建、启动/停止，以及把代理「注入」子进程环境。
//!
//! 【文件职责】定义 `NetworkProxy` 与其 `NetworkProxyBuilder`：在 loopback 上
//!   预占 HTTP/SOCKS5 监听端口、`run()` 起协程开始转发、`apply_to_env()` 把代理
//!   地址写进一大批 `*_PROXY` 环境变量逼迫子进程（curl/npm/git…）全部走代理。
//!   它是「让网络管控生效」的接线层，真正的放行判定在 network_policy/runtime。
//!
//! 【架构位置】
//!   层级：网络代理 · 代理实例与生命周期层
//!   上游：core 侧会话接线（构建 proxy、随权限切换重建、把 env 注入 exec）
//!   下游：`http_proxy.rs` / `socks5.rs`（实际监听与转发）、
//!         `runtime.rs::NetworkProxyState`（共享的策略活视图）
//!
//! 【数据流】
//!   builder.state(...).build() → 预占 loopback 端口 → run() spawn 监听协程
//!     → NetworkProxyHandle（wait/shutdown 控制生命周期）
//!   apply_to_env(env) → 写入 HTTP_PROXY/ALL_PROXY/NO_PROXY 等 → 注入子进程
//!
//! 【设计要点】代理只绑回环地址（即便受管 Windows 端口也强制 clamp 到 127.0.0.1），
//!   所以端口即使泄露也无法被外网利用；`apply_to_env` 故意「覆写」已有代理变量，
//!   不让命令级环境绕过受管端点。
//!
//! 【阅读建议】先看 `NetworkProxyBuilder::build()`（端口预占与 clamp）与
//!   `NetworkProxy::run()`（起监听），再看 `apply_proxy_env_overrides()`（env 注入
//!   的全部细节）；大段 `PROXY_*_ENV_KEYS` 常量是各工具链的代理变量清单，可速览。

use crate::config;
use crate::http_proxy;
use crate::network_policy::NetworkPolicyDecider;
use crate::runtime::BlockedRequestObserver;
use crate::runtime::ConfigState;
use crate::runtime::unix_socket_permissions_supported;
use crate::socks5;
use crate::state::NetworkProxyState;
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::warn;

#[derive(Debug, Clone, Parser)]
#[command(name = "codex-network-proxy", about = "Codex network sandbox proxy")]
pub struct Args {}

/// 在 build 阶段就预先绑定好的监听 socket，留待 run() 时取走交给监听协程。
/// 「先占端口、后启动」是为了让 `http_addr()`/`socks_addr()` 在真正 run 之前
/// 就能拿到确定的端口号（注入子进程 env 需要它），同时避免 build→run 之间端口被抢。
/// 用 `Mutex<Option<..>>` + `take()`：listener 只能被取走一次。
#[derive(Debug)]
struct ReservedListeners {
    http: Mutex<Option<StdTcpListener>>,
    socks: Mutex<Option<StdTcpListener>>,
}

impl ReservedListeners {
    fn new(http: StdTcpListener, socks: Option<StdTcpListener>) -> Self {
        Self {
            http: Mutex::new(Some(http)),
            socks: Mutex::new(socks),
        }
    }

    fn take_http(&self) -> Option<StdTcpListener> {
        let mut guard = self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take()
    }

    fn take_socks(&self) -> Option<StdTcpListener> {
        let mut guard = self
            .socks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take()
    }
}

struct ReservedListenerSet {
    http_listener: StdTcpListener,
    socks_listener: Option<StdTcpListener>,
}

impl ReservedListenerSet {
    fn new(http_listener: StdTcpListener, socks_listener: Option<StdTcpListener>) -> Self {
        Self {
            http_listener,
            socks_listener,
        }
    }

    fn http_addr(&self) -> Result<SocketAddr> {
        self.http_listener
            .local_addr()
            .context("failed to read reserved HTTP proxy address")
    }

    fn socks_addr(&self, default_addr: SocketAddr) -> Result<SocketAddr> {
        self.socks_listener
            .as_ref()
            .map_or(Ok(default_addr), |listener| {
                listener
                    .local_addr()
                    .context("failed to read reserved SOCKS5 proxy address")
            })
    }

    fn into_reserved_listeners(self) -> Arc<ReservedListeners> {
        Arc::new(ReservedListeners::new(
            self.http_listener,
            self.socks_listener,
        ))
    }
}

/// `NetworkProxy` 的构建器。`state` 必填（策略活视图），其余可选。
/// `managed_by_codex`（默认 true）决定端口来源：受管时自动预占 loopback 临时端口，
/// 非受管时用配置/调用方指定的地址（供外部托管场景）。
#[derive(Clone)]
pub struct NetworkProxyBuilder {
    state: Option<Arc<NetworkProxyState>>,
    http_addr: Option<SocketAddr>,
    socks_addr: Option<SocketAddr>,
    managed_by_codex: bool,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
}

impl Default for NetworkProxyBuilder {
    fn default() -> Self {
        Self {
            state: None,
            http_addr: None,
            socks_addr: None,
            managed_by_codex: true,
            policy_decider: None,
            blocked_request_observer: None,
        }
    }
}

impl NetworkProxyBuilder {
    pub fn state(mut self, state: Arc<NetworkProxyState>) -> Self {
        self.state = Some(state);
        self
    }

    pub fn http_addr(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    pub fn socks_addr(mut self, addr: SocketAddr) -> Self {
        self.socks_addr = Some(addr);
        self
    }

    pub fn managed_by_codex(mut self, managed_by_codex: bool) -> Self {
        self.managed_by_codex = managed_by_codex;
        self
    }

    pub fn policy_decider<D>(mut self, decider: D) -> Self
    where
        D: NetworkPolicyDecider,
    {
        self.policy_decider = Some(Arc::new(decider));
        self
    }

    pub fn policy_decider_arc(mut self, decider: Arc<dyn NetworkPolicyDecider>) -> Self {
        self.policy_decider = Some(decider);
        self
    }

    pub fn blocked_request_observer<O>(mut self, observer: O) -> Self
    where
        O: BlockedRequestObserver,
    {
        self.blocked_request_observer = Some(Arc::new(observer));
        self
    }

    pub fn blocked_request_observer_arc(
        mut self,
        observer: Arc<dyn BlockedRequestObserver>,
    ) -> Self {
        self.blocked_request_observer = Some(observer);
        self
    }

    /// 组装 `NetworkProxy`：挂上阻断观察者、按 `managed_by_codex` 解析监听地址、
    /// 受管时预占 loopback 端口，最后再统一做一次回环 clamp。返回的 proxy 尚未监听，
    /// 需调用 `run()` 才真正起协程。
    pub async fn build(self) -> Result<NetworkProxy> {
        let state = self.state.ok_or_else(|| {
            anyhow::anyhow!(
                "NetworkProxyBuilder requires a state; supply one via builder.state(...)"
            )
        })?;
        state
            .set_blocked_request_observer(self.blocked_request_observer.clone())
            .await;
        let current_cfg = state.current_cfg().await?;
        // 受管 vs 非受管两条路径：受管时由我们预占 loopback 端口（Windows 走固定
        // 端口+忙则回退临时口，其余平台直接占临时口）；非受管时沿用配置/调用方地址。
        let (requested_http_addr, requested_socks_addr, reserved_listeners) = if self
            .managed_by_codex
        {
            let runtime = config::resolve_runtime(&current_cfg)?;
            #[cfg(target_os = "windows")]
            let (managed_http_addr, managed_socks_addr) = config::clamp_bind_addrs(
                runtime.http_addr,
                runtime.socks_addr,
                &current_cfg.network,
            );
            #[cfg(target_os = "windows")]
            let reserved = reserve_windows_managed_listeners(
                managed_http_addr,
                managed_socks_addr,
                current_cfg.network.enable_socks5,
            )
            .context("reserve managed loopback proxy listeners")?;
            #[cfg(not(target_os = "windows"))]
            let reserved = reserve_loopback_ephemeral_listeners(current_cfg.network.enable_socks5)
                .context("reserve managed loopback proxy listeners")?;
            let http_addr = reserved.http_addr()?;
            let socks_addr = reserved.socks_addr(runtime.socks_addr)?;
            (
                http_addr,
                socks_addr,
                Some(reserved.into_reserved_listeners()),
            )
        } else {
            let runtime = config::resolve_runtime(&current_cfg)?;
            (
                self.http_addr.unwrap_or(runtime.http_addr),
                self.socks_addr.unwrap_or(runtime.socks_addr),
                None,
            )
        };

        // Reapply bind clamping for caller overrides so unix-socket proxying stays loopback-only.
        let (http_addr, socks_addr) = config::clamp_bind_addrs(
            requested_http_addr,
            requested_socks_addr,
            &current_cfg.network,
        );

        Ok(NetworkProxy {
            state,
            http_addr,
            socks_addr,
            socks_enabled: current_cfg.network.enable_socks5,
            runtime_settings: Arc::new(RwLock::new(NetworkProxyRuntimeSettings::from_config(
                &current_cfg,
            ))),
            reserved_listeners,
            policy_decider: self.policy_decider,
        })
    }
}

fn reserve_loopback_ephemeral_listeners(
    reserve_socks_listener: bool,
) -> Result<ReservedListenerSet> {
    let http_listener =
        reserve_loopback_ephemeral_listener().context("reserve HTTP proxy listener")?;
    let socks_listener = if reserve_socks_listener {
        Some(reserve_loopback_ephemeral_listener().context("reserve SOCKS5 proxy listener")?)
    } else {
        None
    };
    Ok(ReservedListenerSet::new(http_listener, socks_listener))
}

#[cfg(target_os = "windows")]
fn reserve_windows_managed_listeners(
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    reserve_socks_listener: bool,
) -> Result<ReservedListenerSet> {
    let http_addr = windows_managed_loopback_addr(http_addr);
    let socks_addr = windows_managed_loopback_addr(socks_addr);

    match try_reserve_windows_managed_listeners(http_addr, socks_addr, reserve_socks_listener) {
        Ok(listeners) => Ok(listeners),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            warn!("managed Windows proxy ports are busy; falling back to ephemeral loopback ports");
            reserve_loopback_ephemeral_listeners(reserve_socks_listener)
                .context("reserve fallback loopback proxy listeners")
        }
        Err(err) => Err(err).context("reserve Windows managed proxy listeners"),
    }
}

#[cfg(target_os = "windows")]
fn try_reserve_windows_managed_listeners(
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    reserve_socks_listener: bool,
) -> std::io::Result<ReservedListenerSet> {
    let http_listener = StdTcpListener::bind(http_addr)?;
    let socks_listener = if reserve_socks_listener {
        Some(StdTcpListener::bind(socks_addr)?)
    } else {
        None
    };
    Ok(ReservedListenerSet::new(http_listener, socks_listener))
}

#[cfg(target_os = "windows")]
fn windows_managed_loopback_addr(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_loopback() {
        warn!(
            "managed Windows proxies must bind to loopback; clamping {addr} to 127.0.0.1:{}",
            addr.port()
        );
    }
    SocketAddr::from(([127, 0, 0, 1], addr.port()))
}

fn reserve_loopback_ephemeral_listener() -> Result<StdTcpListener> {
    StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .context("bind loopback ephemeral port")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkProxyRuntimeSettings {
    allow_local_binding: bool,
    allow_unix_sockets: Arc<[String]>,
    dangerously_allow_all_unix_sockets: bool,
}

impl NetworkProxyRuntimeSettings {
    fn from_config(config: &config::NetworkProxyConfig) -> Self {
        Self {
            allow_local_binding: config.network.allow_local_binding,
            allow_unix_sockets: config.network.allow_unix_sockets().into(),
            dangerously_allow_all_unix_sockets: config.network.dangerously_allow_all_unix_sockets,
        }
    }
}

/// 一个已配置好的代理实例（可 `Clone`，共享内部状态）。持有策略活视图 `state`、
/// 确定的监听地址、预占的 listener，以及可选的运行时审批 `policy_decider`。
/// 由 `run()` 启动监听协程；`apply_to_env()` 负责把它注入子进程环境。
#[derive(Clone)]
pub struct NetworkProxy {
    state: Arc<NetworkProxyState>,
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    socks_enabled: bool,
    runtime_settings: Arc<RwLock<NetworkProxyRuntimeSettings>>,
    reserved_listeners: Option<Arc<ReservedListeners>>,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
}

impl std::fmt::Debug for NetworkProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid logging internal state (config contents, derived globsets, etc.) which can be noisy
        // and may contain sensitive paths.
        f.debug_struct("NetworkProxy")
            .field("http_addr", &self.http_addr)
            .field("socks_addr", &self.socks_addr)
            .finish_non_exhaustive()
    }
}

impl PartialEq for NetworkProxy {
    fn eq(&self, other: &Self) -> bool {
        self.http_addr == other.http_addr
            && self.socks_addr == other.socks_addr
            && self.runtime_settings() == other.runtime_settings()
    }
}

impl Eq for NetworkProxy {}

// ═══════════════════════════════════════════════════════════════
// Proxy env keys  ·  各工具链的代理相关环境变量清单
// 不同生态（npm/yarn/bundler/pip/docker/electron/node…）认不同的代理变量名，
// 这里穷举它们，注入时一次性覆写，确保没有工具能绕过受管代理。
// PROXY_ENV_KEYS 是「我们会写入的全集」（含 NO_PROXY、激活标记等），测试据此
// 校验「不多写无关变量」。
// ═══════════════════════════════════════════════════════════════

// 用于「探测子进程环境是否已带代理 URL」的键集（读，不写）。
pub const PROXY_URL_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
];

pub const ALL_PROXY_ENV_KEYS: &[&str] = &["ALL_PROXY", "all_proxy"];
pub const PROXY_ACTIVE_ENV_KEY: &str = "CODEX_NETWORK_PROXY_ACTIVE";
pub const ALLOW_LOCAL_BINDING_ENV_KEY: &str = "CODEX_NETWORK_ALLOW_LOCAL_BINDING";
const ELECTRON_GET_USE_PROXY_ENV_KEY: &str = "ELECTRON_GET_USE_PROXY";
const NODE_USE_ENV_PROXY_ENV_KEY: &str = "NODE_USE_ENV_PROXY";
#[cfg(any(target_os = "macos", test))]
const GIT_SSH_COMMAND_ENV_KEY: &str = "GIT_SSH_COMMAND";
pub const PROXY_ENV_KEYS: &[&str] = &[
    PROXY_ACTIVE_ENV_KEY,
    ALLOW_LOCAL_BINDING_ENV_KEY,
    ELECTRON_GET_USE_PROXY_ENV_KEY,
    NODE_USE_ENV_PROXY_ENV_KEY,
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "npm_config_http_proxy",
    "npm_config_https_proxy",
    "npm_config_proxy",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "NPM_CONFIG_PROXY",
    "BUNDLE_HTTP_PROXY",
    "BUNDLE_HTTPS_PROXY",
    "PIP_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "ws_proxy",
    "wss_proxy",
    "NO_PROXY",
    "no_proxy",
    "npm_config_noproxy",
    "NPM_CONFIG_NOPROXY",
    "YARN_NO_PROXY",
    "BUNDLE_NO_PROXY",
    "ALL_PROXY",
    "all_proxy",
    "FTP_PROXY",
    "ftp_proxy",
];

#[cfg(target_os = "macos")]
pub const PROXY_GIT_SSH_COMMAND_ENV_KEY: &str = GIT_SSH_COMMAND_ENV_KEY;

const FTP_PROXY_ENV_KEYS: &[&str] = &["FTP_PROXY", "ftp_proxy"];
const WEBSOCKET_PROXY_ENV_KEYS: &[&str] = &["WS_PROXY", "WSS_PROXY", "ws_proxy", "wss_proxy"];

pub const NO_PROXY_ENV_KEYS: &[&str] = &[
    "NO_PROXY",
    "no_proxy",
    "npm_config_noproxy",
    "NPM_CONFIG_NOPROXY",
    "YARN_NO_PROXY",
    "BUNDLE_NO_PROXY",
];

// NO_PROXY 默认值：让回环与私网网段「直连、不走代理」，便于本地 IPC/局域网访问。
// 故意只列 IP/网段、不列任何主机名后缀：列主机名会迫使客户端在本地解析内网名，
// 反而削弱代理对域名解析的统一管控（也是 SSRF 防护的一环）。
pub const DEFAULT_NO_PROXY_VALUE: &str = concat!(
    "localhost,127.0.0.1,::1,",
    "10.0.0.0/8,",
    "172.16.0.0/12,",
    "192.168.0.0/16"
);

#[cfg(target_os = "macos")]
pub const CODEX_PROXY_GIT_SSH_COMMAND_MARKER: &str = "CODEX_PROXY_GIT_SSH_COMMAND=1 ";
#[cfg(target_os = "macos")]
const CODEX_PROXY_GIT_SSH_COMMAND_PREFIX: &str =
    "CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x ";
#[cfg(target_os = "macos")]
const CODEX_PROXY_GIT_SSH_COMMAND_SUFFIX: &str = " %h %p'";

pub fn proxy_url_env_value<'a>(
    env: &'a HashMap<String, String>,
    canonical_key: &str,
) -> Option<&'a str> {
    if let Some(value) = env.get(canonical_key) {
        return Some(value.as_str());
    }
    let lower_key = canonical_key.to_ascii_lowercase();
    env.get(lower_key.as_str()).map(String::as_str)
}

// 探测给定环境里是否已设置了任一非空代理 URL 变量（大小写别名都查）。
// 供上游判断「是否已有代理在生效」，避免与受管代理冲突。
pub fn has_proxy_url_env_vars(env: &HashMap<String, String>) -> bool {
    PROXY_URL_ENV_KEYS
        .iter()
        .any(|key| proxy_url_env_value(env, key).is_some_and(|value| !value.trim().is_empty()))
}

fn set_env_keys(env: &mut HashMap<String, String>, keys: &[&str], value: &str) {
    for key in keys {
        env.insert((*key).to_string(), value.to_string());
    }
}

#[cfg(target_os = "macos")]
fn codex_proxy_git_ssh_command(socks_addr: SocketAddr) -> String {
    format!("{CODEX_PROXY_GIT_SSH_COMMAND_PREFIX}{socks_addr}{CODEX_PROXY_GIT_SSH_COMMAND_SUFFIX}")
}

#[cfg(target_os = "macos")]
fn is_codex_proxy_git_ssh_command(command: &str) -> bool {
    command.starts_with(CODEX_PROXY_GIT_SSH_COMMAND_PREFIX)
        && command.ends_with(CODEX_PROXY_GIT_SSH_COMMAND_SUFFIX)
}

/// 把受管代理地址写进一大批工具链的代理环境变量（就地修改 `env`）。
/// 这是「逼子进程走代理」的核心：HTTP(S)/WS/npm/yarn/bundler/pip/docker 全指向
/// HTTP 代理；NO_PROXY 放行回环/私网；socks 开启时 ALL_PROXY/FTP 走 socks5h。
/// 副作用：覆写已有同名变量（有意为之，防绕过）。socks5h 的 `h` 表示由代理做 DNS。
fn apply_proxy_env_overrides(
    env: &mut HashMap<String, String>,
    http_addr: SocketAddr,
    socks_addr: SocketAddr,
    socks_enabled: bool,
    allow_local_binding: bool,
) {
    let http_proxy_url = format!("http://{http_addr}");
    let socks_proxy_url = format!("socks5h://{socks_addr}");
    env.insert(PROXY_ACTIVE_ENV_KEY.to_string(), "1".to_string());
    env.insert(
        ALLOW_LOCAL_BINDING_ENV_KEY.to_string(),
        if allow_local_binding {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );

    // HTTP-based clients are best served by explicit HTTP proxy URLs.
    set_env_keys(
        env,
        &[
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "YARN_HTTP_PROXY",
            "YARN_HTTPS_PROXY",
            "npm_config_http_proxy",
            "npm_config_https_proxy",
            "npm_config_proxy",
            "NPM_CONFIG_HTTP_PROXY",
            "NPM_CONFIG_HTTPS_PROXY",
            "NPM_CONFIG_PROXY",
            "BUNDLE_HTTP_PROXY",
            "BUNDLE_HTTPS_PROXY",
            "PIP_PROXY",
            "DOCKER_HTTP_PROXY",
            "DOCKER_HTTPS_PROXY",
        ],
        &http_proxy_url,
    );
    // Some websocket clients look for dedicated WS/WSS proxy environment variables instead of
    // HTTP(S)_PROXY. Keep them aligned with the managed HTTP proxy endpoint.
    set_env_keys(env, WEBSOCKET_PROXY_ENV_KEYS, &http_proxy_url);

    // Keep loopback and IP-literal private targets direct so local IPC/LAN access avoids the proxy.
    // Do not include hostname suffixes here: those can force clients to resolve internal names
    // locally instead of letting the proxy resolve them.
    set_env_keys(env, NO_PROXY_ENV_KEYS, DEFAULT_NO_PROXY_VALUE);

    env.insert(
        ELECTRON_GET_USE_PROXY_ENV_KEY.to_string(),
        "true".to_string(),
    );
    // Node.js built-in HTTP clients only honor proxy environment variables when this is enabled.
    env.insert(NODE_USE_ENV_PROXY_ENV_KEY.to_string(), "1".to_string());

    // Keep HTTP_PROXY/HTTPS_PROXY as HTTP endpoints. A lot of clients break if
    // those vars contain SOCKS URLs. We only switch ALL_PROXY here.
    //
    if socks_enabled {
        set_env_keys(env, ALL_PROXY_ENV_KEYS, &socks_proxy_url);
        set_env_keys(env, FTP_PROXY_ENV_KEYS, &socks_proxy_url);
    } else {
        set_env_keys(env, ALL_PROXY_ENV_KEYS, &http_proxy_url);
        set_env_keys(env, FTP_PROXY_ENV_KEYS, &http_proxy_url);
    }

    #[cfg(target_os = "macos")]
    if socks_enabled {
        // Preserve existing SSH wrappers (for example: Secretive/Teleport setups)
        // but refresh a previously injected Codex fallback so it cannot point
        // at a stale proxy port after the proxy is restarted.
        match env.get(GIT_SSH_COMMAND_ENV_KEY) {
            Some(command) if !is_codex_proxy_git_ssh_command(command) => {}
            _ => {
                env.insert(
                    GIT_SSH_COMMAND_ENV_KEY.to_string(),
                    codex_proxy_git_ssh_command(socks_addr),
                );
            }
        }
    }
}

impl NetworkProxy {
    pub fn builder() -> NetworkProxyBuilder {
        NetworkProxyBuilder::default()
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    pub fn socks_addr(&self) -> SocketAddr {
        self.socks_addr
    }

    pub async fn current_cfg(&self) -> Result<config::NetworkProxyConfig> {
        self.state.current_cfg().await
    }

    pub async fn add_allowed_domain(&self, host: &str) -> Result<()> {
        self.state.add_allowed_domain(host).await
    }

    pub async fn add_denied_domain(&self, host: &str) -> Result<()> {
        self.state.add_denied_domain(host).await
    }

    pub fn allow_local_binding(&self) -> bool {
        self.runtime_settings().allow_local_binding
    }

    pub fn allow_unix_sockets(&self) -> Arc<[String]> {
        self.runtime_settings().allow_unix_sockets
    }

    pub fn dangerously_allow_all_unix_sockets(&self) -> bool {
        self.runtime_settings().dangerously_allow_all_unix_sockets
    }

    /// 把本代理注入一份子进程环境变量（就地修改）。core 在 spawn 受沙箱命令前调用，
    /// 是「网络管控落到子进程」的接线点。
    pub fn apply_to_env(&self, env: &mut HashMap<String, String>) {
        let allow_local_binding = self.allow_local_binding();
        // Enforce proxying for child processes. We intentionally override existing values so
        // command-level environment cannot bypass the managed proxy endpoint.
        // 强制子进程走代理：有意覆写已有值，使命令级环境无法绕过受管端点。
        apply_proxy_env_overrides(
            env,
            self.http_addr,
            self.socks_addr,
            self.socks_enabled,
            allow_local_binding,
        );
    }

    /// 热替换策略状态，但禁止改动「监听形态」类字段：enabled、proxy_url、
    /// socks_url、enable_socks5(_udp)。这些一旦运行就固定（端口已绑、env 已注入），
    /// 改它们需重建代理。可热改的是域名白/黑名单、mode、unix socket 等运行期设置。
    pub async fn replace_config_state(&self, new_state: ConfigState) -> Result<()> {
        let current_cfg = self.state.current_cfg().await?;
        anyhow::ensure!(
            new_state.config.network.enabled == current_cfg.network.enabled,
            "cannot update network.enabled on a running proxy"
        );
        anyhow::ensure!(
            new_state.config.network.proxy_url == current_cfg.network.proxy_url,
            "cannot update network.proxy_url on a running proxy"
        );
        anyhow::ensure!(
            new_state.config.network.socks_url == current_cfg.network.socks_url,
            "cannot update network.socks_url on a running proxy"
        );
        anyhow::ensure!(
            new_state.config.network.enable_socks5 == current_cfg.network.enable_socks5,
            "cannot update network.enable_socks5 on a running proxy"
        );
        anyhow::ensure!(
            new_state.config.network.enable_socks5_udp == current_cfg.network.enable_socks5_udp,
            "cannot update network.enable_socks5_udp on a running proxy"
        );

        let settings = NetworkProxyRuntimeSettings::from_config(&new_state.config);
        self.state.replace_config_state(new_state).await?;
        let mut guard = self
            .runtime_settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = settings;
        Ok(())
    }

    fn runtime_settings(&self) -> NetworkProxyRuntimeSettings {
        self.runtime_settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 启动监听：分别为 HTTP（必开）与 SOCKS5（按配置）spawn 转发协程，
    /// 返回 `NetworkProxyHandle` 控制其生命周期。优先用 build 时预占的 listener，
    /// 没有则现绑地址。`network.enabled=false` 时返回 noop 句柄、不真正监听。
    pub async fn run(&self) -> Result<NetworkProxyHandle> {
        let current_cfg = self.state.current_cfg().await?;
        if !current_cfg.network.enabled {
            warn!("network.enabled is false; skipping proxy listeners");
            return Ok(NetworkProxyHandle::noop());
        }

        if !unix_socket_permissions_supported() {
            warn!(
                "allowUnixSockets and dangerouslyAllowAllUnixSockets are macOS-only; requests will be rejected on this platform"
            );
        }

        let reserved_listeners = self.reserved_listeners.as_ref();
        let http_listener = reserved_listeners.and_then(|listeners| listeners.take_http());
        let socks_listener = reserved_listeners.and_then(|listeners| listeners.take_socks());

        let http_state = self.state.clone();
        let http_decider = self.policy_decider.clone();
        let http_addr = self.http_addr;
        let http_task = tokio::spawn(async move {
            match http_listener {
                Some(listener) => {
                    http_proxy::run_http_proxy_with_std_listener(http_state, listener, http_decider)
                        .await
                }
                None => http_proxy::run_http_proxy(http_state, http_addr, http_decider).await,
            }
        });

        let socks_task = if current_cfg.network.enable_socks5 {
            let socks_state = self.state.clone();
            let socks_decider = self.policy_decider.clone();
            let socks_addr = self.socks_addr;
            let enable_socks5_udp = current_cfg.network.enable_socks5_udp;
            Some(tokio::spawn(async move {
                match socks_listener {
                    Some(listener) => {
                        socks5::run_socks5_with_std_listener(
                            socks_state,
                            listener,
                            socks_decider,
                            enable_socks5_udp,
                        )
                        .await
                    }
                    None => {
                        socks5::run_socks5(
                            socks_state,
                            socks_addr,
                            socks_decider,
                            enable_socks5_udp,
                        )
                        .await
                    }
                }
            }))
        } else {
            None
        };

        Ok(NetworkProxyHandle {
            http_task: Some(http_task),
            socks_task,
            completed: false,
        })
    }
}

/// 运行中代理的句柄：持有 HTTP/SOCKS 两个监听协程。`wait()` 阻塞到协程结束、
/// `shutdown()` 主动终止。`completed` 标记是否已正常收尾，供 `Drop` 判断是否需兜底中止。
pub struct NetworkProxyHandle {
    http_task: Option<JoinHandle<Result<()>>>,
    socks_task: Option<JoinHandle<Result<()>>>,
    completed: bool,
}

impl NetworkProxyHandle {
    // 空操作句柄：network.enabled=false 时返回，假装在跑实则什么都不监听。
    fn noop() -> Self {
        Self {
            http_task: Some(tokio::spawn(async { Ok(()) })),
            socks_task: None,
            completed: true,
        }
    }

    pub async fn wait(mut self) -> Result<()> {
        let http_task = self.http_task.take().context("missing http proxy task")?;
        let socks_task = self.socks_task.take();
        let http_result = http_task.await;
        let socks_result = match socks_task {
            Some(task) => Some(task.await),
            None => None,
        };
        self.completed = true;
        http_result??;
        if let Some(socks_result) = socks_result {
            socks_result??;
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        abort_tasks(self.http_task.take(), self.socks_task.take()).await;
        self.completed = true;
        Ok(())
    }
}

async fn abort_task(task: Option<JoinHandle<Result<()>>>) {
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

async fn abort_tasks(
    http_task: Option<JoinHandle<Result<()>>>,
    socks_task: Option<JoinHandle<Result<()>>>,
) {
    abort_task(http_task).await;
    abort_task(socks_task).await;
}

impl Drop for NetworkProxyHandle {
    // 兜底中止：句柄被丢弃且未正常收尾时，spawn 一个协程去 abort 监听任务，
    // 防止代理协程在持有者消失后变成泄漏的孤儿（仍占着端口转发）。
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let http_task = self.http_task.take();
        let socks_task = self.socks_task.take();
        tokio::spawn(async move {
            abort_tasks(http_task, socks_task).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkProxySettings;
    use crate::state::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn managed_proxy_builder_uses_loopback_ports() {
        let http_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let socks_listener = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let socks_addr = socks_listener.local_addr().unwrap();
        drop(http_listener);
        drop(socks_listener);

        let state = Arc::new(network_proxy_state_for_policy(NetworkProxySettings {
            proxy_url: format!("http://{http_addr}"),
            socks_url: format!("http://{socks_addr}"),
            ..NetworkProxySettings::default()
        }));
        let proxy = match NetworkProxy::builder().state(state).build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("Operation not permitted"))
                {
                    return;
                }
                panic!("failed to build managed proxy: {err:#}");
            }
        };

        assert!(proxy.http_addr.ip().is_loopback());
        assert!(proxy.socks_addr.ip().is_loopback());
        #[cfg(target_os = "windows")]
        {
            assert_eq!(proxy.http_addr, http_addr);
            assert_eq!(proxy.socks_addr, socks_addr);
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_ne!(proxy.http_addr.port(), 0);
            assert_ne!(proxy.socks_addr.port(), 0);
        }
    }

    #[tokio::test]
    async fn non_codex_managed_proxy_builder_uses_configured_ports() {
        let settings = NetworkProxySettings {
            proxy_url: "http://127.0.0.1:43128".to_string(),
            socks_url: "http://127.0.0.1:48081".to_string(),
            ..NetworkProxySettings::default()
        };
        let state = Arc::new(network_proxy_state_for_policy(settings));
        let proxy = NetworkProxy::builder()
            .state(state)
            .managed_by_codex(/*managed_by_codex*/ false)
            .build()
            .await
            .unwrap();

        assert_eq!(
            proxy.http_addr,
            "127.0.0.1:43128".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            proxy.socks_addr,
            "127.0.0.1:48081".parse::<SocketAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn managed_proxy_builder_does_not_reserve_socks_listener_when_disabled() {
        let settings = NetworkProxySettings {
            enable_socks5: false,
            proxy_url: "http://127.0.0.1:43128".to_string(),
            socks_url: "http://127.0.0.1:43129".to_string(),
            ..NetworkProxySettings::default()
        };
        let state = Arc::new(network_proxy_state_for_policy(settings));
        let proxy = match NetworkProxy::builder().state(state).build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                if err
                    .chain()
                    .any(|cause| cause.to_string().contains("Operation not permitted"))
                {
                    return;
                }
                panic!("failed to build managed proxy: {err:#}");
            }
        };

        assert!(proxy.http_addr.ip().is_loopback());
        assert_ne!(proxy.http_addr.port(), 0);
        assert_eq!(
            proxy.socks_addr,
            "127.0.0.1:43129".parse::<SocketAddr>().unwrap()
        );
        assert!(
            proxy
                .reserved_listeners
                .as_ref()
                .expect("managed builder should reserve listeners")
                .take_socks()
                .is_none()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_managed_loopback_addr_clamps_non_loopback_inputs() {
        assert_eq!(
            windows_managed_loopback_addr("0.0.0.0:3128".parse::<SocketAddr>().unwrap()),
            "127.0.0.1:3128".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            windows_managed_loopback_addr("[::]:8081".parse::<SocketAddr>().unwrap()),
            "127.0.0.1:8081".parse::<SocketAddr>().unwrap()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reserve_windows_managed_listeners_falls_back_when_http_port_is_busy() {
        let occupied = StdTcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let busy_port = occupied.local_addr().unwrap().port();

        let reserved = reserve_windows_managed_listeners(
            SocketAddr::from(([127, 0, 0, 1], busy_port)),
            SocketAddr::from(([127, 0, 0, 1], 48081)),
            /*reserve_socks_listener*/ false,
        )
        .unwrap();

        assert!(reserved.socks_listener.is_none());
        assert!(
            reserved
                .http_listener
                .local_addr()
                .unwrap()
                .ip()
                .is_loopback()
        );
        assert_ne!(
            reserved.http_listener.local_addr().unwrap().port(),
            busy_port
        );
    }

    #[test]
    fn proxy_url_env_value_resolves_lowercase_aliases() {
        let mut env = HashMap::new();
        env.insert(
            "http_proxy".to_string(),
            "http://127.0.0.1:3128".to_string(),
        );

        assert_eq!(
            proxy_url_env_value(&env, "HTTP_PROXY"),
            Some("http://127.0.0.1:3128")
        );
    }

    #[test]
    fn has_proxy_url_env_vars_detects_lowercase_aliases() {
        let mut env = HashMap::new();
        env.insert(
            "all_proxy".to_string(),
            "socks5h://127.0.0.1:8081".to_string(),
        );

        assert_eq!(has_proxy_url_env_vars(&env), true);
    }

    #[test]
    fn has_proxy_url_env_vars_detects_websocket_proxy_keys() {
        let mut env = HashMap::new();
        env.insert("wss_proxy".to_string(), "http://127.0.0.1:3128".to_string());

        assert_eq!(has_proxy_url_env_vars(&env), true);
    }

    #[test]
    fn apply_proxy_env_overrides_sets_common_tool_vars() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        assert_eq!(
            env.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WSS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("npm_config_proxy"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        assert_eq!(
            env.get("FTP_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        assert_eq!(
            env.get("NO_PROXY"),
            Some(&DEFAULT_NO_PROXY_VALUE.to_string())
        );
        let no_proxy = env.get("NO_PROXY").expect("NO_PROXY should be set");
        assert!(no_proxy.contains("10.0.0.0/8"));
        assert!(no_proxy.contains("172.16.0.0/12"));
        assert!(no_proxy.contains("192.168.0.0/16"));
        assert!(!no_proxy.contains("169.254.0.0/16"));
        assert_eq!(env.get(PROXY_ACTIVE_ENV_KEY), Some(&"1".to_string()));
        assert_eq!(env.get(ALLOW_LOCAL_BINDING_ENV_KEY), Some(&"0".to_string()));
        assert_eq!(
            env.get(ELECTRON_GET_USE_PROXY_ENV_KEY),
            Some(&"true".to_string())
        );
        assert_eq!(env.get(NODE_USE_ENV_PROXY_ENV_KEY), Some(&"1".to_string()));
        #[cfg(target_os = "macos")]
        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(
                &"CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'"
                    .to_string()
            )
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(env.get(GIT_SSH_COMMAND_ENV_KEY), None);
    }

    #[test]
    fn apply_proxy_env_overrides_sets_only_expected_env_keys() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        for key in env.keys() {
            let is_managed_git_ssh_key =
                cfg!(target_os = "macos") && key == GIT_SSH_COMMAND_ENV_KEY;
            assert!(
                PROXY_ENV_KEYS.contains(&key.as_str()) || is_managed_git_ssh_key,
                "proxy env writer set unexpected key: {key}"
            );
        }
    }

    #[test]
    fn apply_proxy_env_overrides_uses_http_for_all_proxy_without_socks() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ false,
            /*allow_local_binding*/ true,
        );

        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(env.get(ALLOW_LOCAL_BINDING_ENV_KEY), Some(&"1".to_string()));
    }

    #[test]
    fn apply_proxy_env_overrides_uses_plain_http_proxy_url() {
        let mut env = HashMap::new();
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        assert_eq!(
            env.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("HTTPS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("WSS_PROXY"),
            Some(&"http://127.0.0.1:3128".to_string())
        );
        assert_eq!(
            env.get("ALL_PROXY"),
            Some(&"socks5h://127.0.0.1:8081".to_string())
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(
                &"CODEX_PROXY_GIT_SSH_COMMAND=1 ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'"
                    .to_string()
            )
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(env.get(GIT_SSH_COMMAND_ENV_KEY), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_preserves_existing_git_ssh_command() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            "ssh -o ProxyCommand='tsh proxy ssh --cluster=dev %r@%h:%p'".to_string(),
        );
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&"ssh -o ProxyCommand='tsh proxy ssh --cluster=dev %r@%h:%p'".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_preserves_unmarked_git_ssh_command_with_proxy_shape() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            "ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'".to_string(),
        );
        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 48081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&"ssh -o ProxyCommand='nc -X 5 -x 127.0.0.1:8081 %h %p'".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_proxy_env_overrides_refreshes_previous_codex_proxy_git_ssh_command() {
        let mut env = HashMap::new();
        env.insert(
            GIT_SSH_COMMAND_ENV_KEY.to_string(),
            codex_proxy_git_ssh_command(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081)),
        );

        apply_proxy_env_overrides(
            &mut env,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43128),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 48081),
            /*socks_enabled*/ true,
            /*allow_local_binding*/ false,
        );

        assert_eq!(
            env.get(GIT_SSH_COMMAND_ENV_KEY),
            Some(&codex_proxy_git_ssh_command(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                48081,
            )))
        );
    }
}
