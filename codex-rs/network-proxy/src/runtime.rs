//! 代理运行时状态：策略的「活视图」+ 按需热重载 + 基线域名/IP 判定。
//!
//! 【文件职责】定义 `NetworkProxyState`——把磁盘上的 `config.toml` 编译成的
//!   可查询状态（白/黑名单 globset、MITM、受管约束、阻断记录），并在每次查询
//!   前按需重载，使配置改动（含 Codex 自身写入）无需重启即可生效。核心安全逻辑
//!   是 `host_blocked()`：判定某个 host:port 是否放行。
//!
//! 【架构位置】
//!   层级：网络代理 · 运行时状态层（被 lib.rs 与 state.rs 双重再导出）
//!   上游：`network_policy.rs::evaluate_host_policy()`、`proxy.rs`（构建/替换状态）
//!   下游：`policy.rs`（host 解析、本地 IP 分类、globset 编译）、
//!         `state.rs`（受管约束校验 `validate_policy_against_constraints`）
//!
//! 【数据流】
//!   ConfigReloader 重载 → ConfigState（编译后的策略）→ host_blocked() 查询
//!     → HostBlockDecision；被拦请求 → record_blocked() → 环形缓冲 + 观察者
//!
//! 【并发模型】内部 `Arc<RwLock<ConfigState>>`，读多写少；热重载/改名单时持写锁
//!   并用「比较前置快照」做乐观重试，避免覆盖并发写入（见 update_domain_list）。
//!
//! 【阅读建议】先看 `ConfigState`（状态全貌）与 `NetworkProxyState` 字段，
//!   再重点读 `host_blocked()`（三步判定顺序：deny→本地网络→白名单）；
//!   `reload_if_needed()` 是所有公共方法的统一前置；底部 `#[cfg(test)]` 可跳过。

use crate::config::NetworkDomainPermission;
use crate::config::NetworkMode;
use crate::config::NetworkProxyConfig;
use crate::config::ValidatedUnixSocketPath;
use crate::mitm::MitmState;
use crate::mitm_hook::HookEvaluation;
use crate::mitm_hook::MitmHooksByHost;
use crate::mitm_hook::evaluate_mitm_hooks;
use crate::policy::Host;
use crate::policy::is_loopback_host;
use crate::policy::is_non_public_ip;
use crate::policy::normalize_host;
use crate::policy::unscoped_ip_literal;
use crate::reasons::REASON_DENIED;
use crate::reasons::REASON_NOT_ALLOWED;
use crate::reasons::REASON_NOT_ALLOWED_LOCAL;
use crate::state::NetworkProxyConstraintError;
use crate::state::NetworkProxyConstraints;
use crate::state::build_config_state;
use crate::state::validate_policy_against_constraints;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use codex_utils_absolute_path::AbsolutePathBuf;
use globset::GlobSet;
use serde::Serialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::lookup_host;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::debug;
use tracing::info;
use tracing::warn;

// 阻断事件环形缓冲的上限：保留最近 200 条供 UI/审计拉取，超出从头丢弃。
// 纯内存遥测，封顶以防长会话无限增长占内存。
const MAX_BLOCKED_EVENTS: usize = 200;
// 本地网络防护时 DNS 解析的超时：2s。超时即按「无法证明目标是公网」拒绝（见
// host_resolves_to_non_public_ip），宁可误拦也不放过可能指向内网的域名。
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
// 违规日志行的固定前缀，便于日志系统按字符串抓取网络策略违规事件。
const NETWORK_POLICY_VIOLATION_PREFIX: &str = "CODEX_NETWORK_POLICY_VIOLATION";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkProxyAuditMetadata {
    pub conversation_id: Option<String>,
    pub app_version: Option<String>,
    pub user_account_id: Option<String>,
    pub auth_mode: Option<String>,
    pub originator: Option<String>,
    pub user_email: Option<String>,
    pub terminal_type: Option<String>,
    pub model: Option<String>,
    pub slug: Option<String>,
}

/// 基线策略拦截一个 host 的三种原因，安全语义各不相同：
/// - `Denied`：命中黑名单——硬拒，不可被运行时审批覆写。
/// - `NotAllowed`：未命中白名单——灰色地带，`evaluate_host_policy` 会问审批。
/// - `NotAllowedLocal`：目标解析到本地/内网 IP 且未显式放行——防 SSRF/DNS 重绑定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostBlockReason {
    Denied,
    NotAllowed,
    NotAllowedLocal,
}

impl HostBlockReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => REASON_DENIED,
            Self::NotAllowed => REASON_NOT_ALLOWED,
            Self::NotAllowedLocal => REASON_NOT_ALLOWED_LOCAL,
        }
    }
}

impl std::fmt::Display for HostBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `host_blocked()` 的基线判定结果：放行，或带原因拦截。
/// 注意这只是「基线」结论，`NotAllowed` 之后还可能被运行时审批翻盘。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostBlockDecision {
    Allowed,
    Blocked(HostBlockReason),
}

/// 一条被拦请求的遥测记录（可 `Serialize` 落日志/上报）。进入环形缓冲供 UI
/// 与审计拉取，也用于拼出 `CODEX_NETWORK_POLICY_VIOLATION` 违规日志行。
#[derive(Clone, Debug, Serialize)]
pub struct BlockedRequest {
    pub host: String,
    pub reason: String,
    pub client: Option<String>,
    pub method: Option<String>,
    pub mode: Option<NetworkMode>,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub timestamp: i64,
}

pub struct BlockedRequestArgs {
    pub host: String,
    pub reason: String,
    pub client: Option<String>,
    pub method: Option<String>,
    pub mode: Option<NetworkMode>,
    pub protocol: String,
    pub decision: Option<String>,
    pub source: Option<String>,
    pub port: Option<u16>,
}

impl BlockedRequest {
    pub fn new(args: BlockedRequestArgs) -> Self {
        let BlockedRequestArgs {
            host,
            reason,
            client,
            method,
            mode,
            protocol,
            decision,
            source,
            port,
        } = args;
        Self {
            host,
            reason,
            client,
            method,
            mode,
            protocol,
            decision,
            source,
            port,
            timestamp: unix_timestamp(),
        }
    }
}

fn blocked_request_violation_log_line(entry: &BlockedRequest) -> String {
    match serde_json::to_string(entry) {
        Ok(json) => format!("{NETWORK_POLICY_VIOLATION_PREFIX} {json}"),
        Err(err) => {
            debug!("failed to serialize blocked request for violation log: {err}");
            format!(
                "{NETWORK_POLICY_VIOLATION_PREFIX} host={} reason={}",
                entry.host, entry.reason
            )
        }
    }
}

/// 一份编译好的策略快照：原始配置 + 预编译的白/黑名单 globset + MITM 状态 +
/// 受管约束 + 阻断遥测。`NetworkProxyState` 持有它的 `RwLock`，热重载即整体替换。
/// 把域名 pattern 预编译成 `GlobSet` 是为了让每次 `host_blocked()` 走 O(1) 匹配，
/// 而非每次请求都重新解析字符串规则。
#[derive(Clone)]
pub struct ConfigState {
    pub config: NetworkProxyConfig,
    pub allow_set: GlobSet,
    pub deny_set: GlobSet,
    pub mitm: Option<Arc<MitmState>>,
    pub mitm_hooks: MitmHooksByHost,
    pub constraints: NetworkProxyConstraints,
    // blocked / blocked_total：阻断遥测。环形缓冲只留最近 MAX_BLOCKED_EVENTS 条，
    // blocked_total 是不封顶的累计计数；重载/替换状态时这两者会被继承而非清零。
    pub blocked: VecDeque<BlockedRequest>,
    pub blocked_total: u64,
}

/// 配置来源的抽象：把「从哪里、何时重新加载策略」与状态层解耦。
/// 生产实现读 `config.toml`（带 mtime 检测）；测试用 NoopReloader/StaticReloader。
#[async_trait]
pub trait ConfigReloader: Send + Sync {
    /// Human-readable description of where config is loaded from, for logs.
    /// 配置来源的人类可读描述，仅用于日志（如文件路径）。
    fn source_label(&self) -> String;

    /// Return a freshly loaded state if a reload is needed; otherwise, return `None`.
    /// 检测到变化才返回新状态；无变化返回 `None`。这是「按需重载」的关键：
    /// 大多数查询命中 `None`，零成本；仅在配置确实改动时承担一次重编译。
    async fn maybe_reload(&self) -> Result<Option<ConfigState>>;

    /// Force a reload, regardless of whether a change was detected.
    /// 强制重载，无视是否检测到变化（用于显式刷新场景）。
    async fn reload_now(&self) -> Result<ConfigState>;
}

/// 阻断事件观察者：每当有请求被拦，回调通知（如推送到 UI/遥测管道）。
/// 下面为 `Arc<O>` 和闭包都提供了 blanket impl，方便直接传函数。
#[async_trait]
pub trait BlockedRequestObserver: Send + Sync + 'static {
    async fn on_blocked_request(&self, request: BlockedRequest);
}

#[async_trait]
impl<O: BlockedRequestObserver + ?Sized> BlockedRequestObserver for Arc<O> {
    async fn on_blocked_request(&self, request: BlockedRequest) {
        (**self).on_blocked_request(request).await
    }
}

#[async_trait]
impl<F, Fut> BlockedRequestObserver for F
where
    F: Fn(BlockedRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
{
    async fn on_blocked_request(&self, request: BlockedRequest) {
        (self)(request).await
    }
}

/// 策略的「活视图」：本 crate 对外的状态句柄，可 `Clone` 共享同一份内部锁。
/// 所有查询/改写方法都先 `reload_if_needed()` 再读写 `state`，从而提供
/// 「无需重启、改 config.toml 即刻生效」的语义。
/// 字段全部私有：内部状态（含敏感路径）不外泄，外部只能经方法访问。
pub struct NetworkProxyState {
    state: Arc<RwLock<ConfigState>>,
    reloader: Arc<dyn ConfigReloader>,
    blocked_request_observer: Arc<RwLock<Option<Arc<dyn BlockedRequestObserver>>>>,
    audit_metadata: NetworkProxyAuditMetadata,
}

impl std::fmt::Debug for NetworkProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid logging internal state (config contents, derived globsets, etc.) which can be noisy
        // and may contain sensitive paths.
        f.debug_struct("NetworkProxyState").finish_non_exhaustive()
    }
}

impl Clone for NetworkProxyState {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            reloader: self.reloader.clone(),
            blocked_request_observer: self.blocked_request_observer.clone(),
            audit_metadata: self.audit_metadata.clone(),
        }
    }
}

impl NetworkProxyState {
    pub fn with_reloader(state: ConfigState, reloader: Arc<dyn ConfigReloader>) -> Self {
        Self::with_reloader_and_audit_metadata(
            state,
            reloader,
            NetworkProxyAuditMetadata::default(),
        )
    }

    pub fn with_reloader_and_blocked_observer(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) -> Self {
        Self::with_reloader_and_audit_metadata_and_blocked_observer(
            state,
            reloader,
            NetworkProxyAuditMetadata::default(),
            blocked_request_observer,
        )
    }

    pub fn with_reloader_and_audit_metadata(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        audit_metadata: NetworkProxyAuditMetadata,
    ) -> Self {
        Self::with_reloader_and_audit_metadata_and_blocked_observer(
            state,
            reloader,
            audit_metadata,
            /*blocked_request_observer*/ None,
        )
    }

    pub fn with_reloader_and_audit_metadata_and_blocked_observer(
        state: ConfigState,
        reloader: Arc<dyn ConfigReloader>,
        audit_metadata: NetworkProxyAuditMetadata,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
            reloader,
            blocked_request_observer: Arc::new(RwLock::new(blocked_request_observer)),
            audit_metadata,
        }
    }

    pub async fn set_blocked_request_observer(
        &self,
        blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    ) {
        let mut observer = self.blocked_request_observer.write().await;
        *observer = blocked_request_observer;
    }

    pub fn audit_metadata(&self) -> &NetworkProxyAuditMetadata {
        &self.audit_metadata
    }

    pub async fn current_cfg(&self) -> Result<NetworkProxyConfig> {
        // Callers treat `NetworkProxyState` as a live view of policy. We reload-on-demand so edits to
        // `config.toml` (including Codex-managed writes) take effect without a restart.
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.clone())
    }

    pub async fn current_patterns(&self) -> Result<(Vec<String>, Vec<String>)> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok((
            guard.config.network.allowed_domains().unwrap_or_default(),
            guard.config.network.denied_domains().unwrap_or_default(),
        ))
    }

    pub async fn enabled(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.network.enabled)
    }

    pub async fn force_reload(&self) -> Result<()> {
        let previous_cfg = {
            let guard = self.state.read().await;
            guard.config.clone()
        };

        match self.reloader.reload_now().await {
            Ok(mut new_state) => {
                // Policy changes are operationally sensitive; logging diffs makes changes traceable
                // without needing to dump full config blobs (which can include unrelated settings).
                // 策略变更属敏感操作：只记差异（增删了哪些域名）而非整份配置，
                // 既可追溯又不泄露无关设置。
                log_policy_changes(&previous_cfg, &new_state.config);
                {
                    let mut guard = self.state.write().await;
                    // 把已积累的阻断记录搬到新状态：重载只换策略，不该丢掉遥测历史。
                    new_state.blocked = guard.blocked.clone();
                    *guard = new_state;
                }
                let source = self.reloader.source_label();
                info!("reloaded config from {source}");
                Ok(())
            }
            Err(err) => {
                let source = self.reloader.source_label();
                warn!("failed to reload config from {source}: {err}; keeping previous config");
                Err(err)
            }
        }
    }

    pub async fn replace_config_state(&self, mut new_state: ConfigState) -> Result<()> {
        self.reload_if_needed().await?;
        let mut guard = self.state.write().await;
        log_policy_changes(&guard.config, &new_state.config);
        new_state.blocked = guard.blocked.clone();
        new_state.blocked_total = guard.blocked_total;
        *guard = new_state;
        info!("updated network proxy config state");
        Ok(())
    }

    /// 基线域名/IP 判定：本 crate 最核心的安全裁决，决定一个 host:port 是否放行。
    ///
    /// @param host - 目标主机（域名或 IP 字面量，可能带 IPv6 scope，如 `fe80::1%lo0`）
    /// @param port - 目标端口（仅用于本地网络检查时的 DNS 解析，不参与白名单匹配）
    /// @returns 基线结论；`NotAllowed` 还可能被上层运行时审批翻盘
    ///
    /// 副作用：可能触发一次 DNS 解析（本地网络防护），故为 async 且可返回 Err。
    pub async fn host_blocked(&self, host: &str, port: u16) -> Result<HostBlockDecision> {
        self.reload_if_needed().await?;
        let host = match Host::parse(host) {
            Ok(host) => host,
            // host 无法解析直接当未命中白名单拒绝：宁可错杀畸形输入也不放行。
            Err(_) => return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowed)),
        };
        let (deny_set, allow_set, allow_local_binding, allowed_domains) = {
            let guard = self.state.read().await;
            let allowed_domains = guard.config.network.allowed_domains();
            (
                guard.deny_set.clone(),
                guard.allow_set.clone(),
                guard.config.network.allow_local_binding,
                allowed_domains,
            )
        };
        let allowed_domains_empty = allowed_domains.is_none();
        let allowed_domains = allowed_domains.unwrap_or_default();

        let host_str = host.as_str();

        // Decision order matters:
        //  1) explicit deny always wins
        //  2) local/private networking is opt-in (defense-in-depth)
        //  3) allowlist is enforced when configured
        // 判定顺序至关重要（越靠前优先级越高、越无法被绕过）：
        //  1) 黑名单永远优先——显式 deny 不可被任何放行覆盖；
        //  2) 访问本地/内网默认关闭（纵深防御），需显式开关或显式放行；
        //  3) 最后才看白名单。顺序错了会留出绕过缺口。
        if globset_matches_host_or_unscoped(&deny_set, host_str) {
            return Ok(HostBlockDecision::Blocked(HostBlockReason::Denied));
        }

        let is_allowlisted = globset_matches_host_or_unscoped(&allow_set, host_str);
        if !allow_local_binding {
            // If the intent is "prevent access to local/internal networks", we must not rely solely
            // on string checks like `localhost` / `127.0.0.1`. Attackers can use DNS rebinding or
            // public suffix services that map hostnames onto private IPs.
            //
            // We therefore do a best-effort DNS + IP classification check before allowing the
            // request. Explicit local/loopback literals are allowed only when explicitly
            // allowlisted; hostnames that resolve to local/private IPs are blocked even if
            // allowlisted.
            //
            // 防本地/内网访问不能只靠字符串匹配 `localhost`/`127.0.0.1`：攻击者可用
            // DNS 重绑定或把域名解析到私网 IP 的公共后缀服务绕过。故此处做
            // 「DNS 解析 + IP 分类」：
            //   · 显式本地/回环字面量：只有被「精确」白名单放行才允许（见
            //     is_explicit_local_allowlisted，通配符如 `*` 不算数）；
            //   · 解析后落到本地/私网 IP 的域名：即便在白名单里也一律拦截。
            let local_literal = {
                let host_no_scope = unscoped_ip_literal(host_str).unwrap_or(host_str);
                if is_loopback_host(&host) {
                    true
                } else if let Ok(ip) = host_no_scope.parse::<IpAddr>() {
                    is_non_public_ip(ip)
                } else {
                    false
                }
            };

            if local_literal {
                if !is_explicit_local_allowlisted(&allowed_domains, &host) {
                    return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal));
                }
            } else if host_resolves_to_non_public_ip(
                host_str,
                port,
                DNS_LOOKUP_TIMEOUT,
                |host, port| async move {
                    lookup_host((host.as_str(), port))
                        .await
                        .map(Iterator::collect)
                },
            )
            .await
            {
                return Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal));
            }
        }

        // 第 3 步白名单闸门：白名单为空（默认拒绝一切）或未命中 → NotAllowed。
        // 这是默认安全姿态：不显式放行就不放行。
        if allowed_domains_empty || !is_allowlisted {
            Ok(HostBlockDecision::Blocked(HostBlockReason::NotAllowed))
        } else {
            Ok(HostBlockDecision::Allowed)
        }
    }

    /// 记录一条被拦请求：推入环形缓冲、累加计数、写违规日志，并通知观察者。
    /// 副作用：修改内部 `blocked` 缓冲（超出 MAX_BLOCKED_EVENTS 从头丢弃）。
    pub async fn record_blocked(&self, entry: BlockedRequest) -> Result<()> {
        self.reload_if_needed().await?;
        let blocked_for_observer = entry.clone();
        let blocked_request_observer = self.blocked_request_observer.read().await.clone();
        let violation_line = blocked_request_violation_log_line(&entry);
        let host = entry.host.clone();
        let reason = entry.reason.clone();
        let decision = entry.decision.clone();
        let source = entry.source.clone();
        let protocol = entry.protocol.clone();
        let port = entry.port;
        let (total, buffered) = {
            let mut guard = self.state.write().await;
            guard.blocked.push_back(entry);
            guard.blocked_total = guard.blocked_total.saturating_add(1);
            let total = guard.blocked_total;
            while guard.blocked.len() > MAX_BLOCKED_EVENTS {
                guard.blocked.pop_front();
            }
            (total, guard.blocked.len())
        };
        debug!(
            "recorded blocked request telemetry (\
             total={total}, host={host}, reason={reason}, \
             decision={decision:?}, source={source:?}, \
             protocol={protocol}, port={port:?}, buffered={buffered})"
        );
        debug!("{violation_line}");

        if let Some(observer) = blocked_request_observer {
            observer.on_blocked_request(blocked_for_observer).await;
        }
        Ok(())
    }

    /// Returns a snapshot of buffered blocked-request entries without consuming
    /// them.
    pub async fn blocked_snapshot(&self) -> Result<Vec<BlockedRequest>> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.blocked.iter().cloned().collect())
    }

    /// Drain and return the buffered blocked-request entries in FIFO order.
    pub async fn drain_blocked(&self) -> Result<Vec<BlockedRequest>> {
        self.reload_if_needed().await?;
        let blocked = {
            let mut guard = self.state.write().await;
            std::mem::take(&mut guard.blocked)
        };
        Ok(blocked.into_iter().collect())
    }

    pub async fn is_unix_socket_allowed(&self, path: &str) -> Result<bool> {
        self.reload_if_needed().await?;
        if !unix_socket_permissions_supported() {
            return Ok(false);
        }

        // We only support absolute unix socket paths (a relative path would be ambiguous with
        // respect to the proxy process's CWD and can lead to confusing allowlist behavior).
        let requested_path = Path::new(path);
        if !requested_path.is_absolute() {
            return Ok(false);
        }

        let guard = self.state.read().await;
        if guard.config.network.dangerously_allow_all_unix_sockets {
            return Ok(true);
        }

        // Normalize the path while keeping the absolute-path requirement explicit.
        let requested_abs = match AbsolutePathBuf::from_absolute_path(requested_path) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        let requested_canonical = std::fs::canonicalize(requested_abs.as_path()).ok();
        for allowed in &guard.config.network.allow_unix_sockets() {
            let allowed_path = match ValidatedUnixSocketPath::parse(allowed) {
                Ok(ValidatedUnixSocketPath::Native(path)) => path,
                Ok(ValidatedUnixSocketPath::UnixStyleAbsolute(_)) => continue,
                Err(err) => {
                    warn!("ignoring invalid network.allow_unix_sockets entry at runtime: {err:#}");
                    continue;
                }
            };

            if allowed_path.as_path() == requested_abs.as_path() {
                return Ok(true);
            }

            // Best-effort canonicalization to reduce surprises with symlinks.
            // If canonicalization fails (e.g., socket not created yet), fall back to raw comparison.
            let Some(requested_canonical) = &requested_canonical else {
                continue;
            };
            if let Ok(allowed_canonical) = std::fs::canonicalize(allowed_path.as_path())
                && &allowed_canonical == requested_canonical
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn method_allowed(&self, method: &str) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.network.mode.allows_method(method))
    }

    pub async fn allow_upstream_proxy(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.network.allow_upstream_proxy)
    }

    pub async fn allow_local_binding(&self) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.network.allow_local_binding)
    }

    pub async fn network_mode(&self) -> Result<NetworkMode> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.config.network.mode)
    }

    /// 切换网络模式（Limited/Full）。受管约束可禁止放宽（如管理员锁死 Limited
    /// 时不允许切到 Full），校验失败返回 Err。
    ///
    /// 用「读快照→校验→写时复核」的乐观重试，而非全程持锁：先在读锁下取候选，
    /// 释放锁做校验，再拿写锁；若期间约束被并发改动（快照失效）则 `continue`
    /// 重来。这样把耗时的校验挪出写锁，缩短临界区。
    pub async fn set_network_mode(&self, mode: NetworkMode) -> Result<()> {
        loop {
            self.reload_if_needed().await?;
            let (candidate, constraints) = {
                let guard = self.state.read().await;
                let mut candidate = guard.config.clone();
                candidate.network.mode = mode;
                (candidate, guard.constraints.clone())
            };

            // 受管约束校验：不能把策略放宽到超过管理员设定的上限。
            validate_policy_against_constraints(&candidate, &constraints)
                .map_err(NetworkProxyConstraintError::into_anyhow)
                .context("network.mode constrained by managed config")?;

            let mut guard = self.state.write().await;
            // 写时复核：约束在校验后被并发改动则放弃本次、重新来过。
            if guard.constraints != constraints {
                drop(guard);
                continue;
            }
            guard.config.network.mode = mode;
            info!("updated network mode to {mode:?}");
            return Ok(());
        }
    }

    pub async fn mitm_state(&self) -> Result<Option<Arc<MitmState>>> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.mitm.clone())
    }

    pub(crate) async fn evaluate_mitm_hook_request(
        &self,
        host: &str,
        req: &rama_http::Request,
    ) -> Result<HookEvaluation> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(evaluate_mitm_hooks(&guard.mitm_hooks, host, req))
    }

    pub async fn host_has_mitm_hooks(&self, host: &str) -> Result<bool> {
        self.reload_if_needed().await?;
        let guard = self.state.read().await;
        Ok(guard.mitm_hooks.contains_key(&normalize_host(host)))
    }

    pub async fn add_allowed_domain(&self, host: &str) -> Result<()> {
        self.update_domain_list(host, DomainListKind::Allow).await
    }

    pub async fn add_denied_domain(&self, host: &str) -> Result<()> {
        self.update_domain_list(host, DomainListKind::Deny).await
    }

    /// `add_allowed_domain` / `add_denied_domain` 的共用实现：把一个 host 加入
    /// 白名单或黑名单，并保证它不同时停留在对侧名单（加白即从黑名单移除，反之亦然）。
    ///
    /// 与 `set_network_mode` 同样的乐观重试：读快照 → 算候选 → 受管约束校验
    /// → 重编译 globset → 写时复核 config 与 constraints 均未被并发改动才提交。
    /// 比 mode 多一步 `build_config_state`（重编译名单为 globset），因为加域名要
    /// 同步更新预编译匹配集，否则查询用的还是旧 globset。
    async fn update_domain_list(&self, host: &str, target: DomainListKind) -> Result<()> {
        let host = Host::parse(host).context("invalid network host")?;
        let normalized_host = host.as_str().to_string();
        let list_name = target.list_name();
        let constraint_field = target.constraint_field();

        loop {
            self.reload_if_needed().await?;
            // 连同 blocked/blocked_total 一起取快照：重建状态时要原样搬回遥测。
            let (previous_cfg, constraints, blocked, blocked_total) = {
                let guard = self.state.read().await;
                (
                    guard.config.clone(),
                    guard.constraints.clone(),
                    guard.blocked.clone(),
                    guard.blocked_total,
                )
            };

            let mut candidate = previous_cfg.clone();
            let target_entries = target.entries(&candidate.network);
            let opposite_entries = target.opposite_entries(&candidate.network);
            let target_contains = target_entries
                .iter()
                .any(|entry| normalize_host(entry) == normalized_host);
            let opposite_contains = opposite_entries
                .iter()
                .any(|entry| normalize_host(entry) == normalized_host);
            // 幂等短路：已在目标名单且不在对侧名单，无需改动直接返回。
            if target_contains && !opposite_contains {
                return Ok(());
            }

            // upsert：插入到目标名单，同时把它从对侧名单清掉（白/黑互斥）。
            candidate.network.upsert_domain_permission(
                normalized_host.clone(),
                target.permission(),
                normalize_host,
            );

            validate_policy_against_constraints(&candidate, &constraints)
                .map_err(NetworkProxyConstraintError::into_anyhow)
                .with_context(|| format!("{constraint_field} constrained by managed config"))?;

            // 重编译候选配置为带 globset 的完整状态，并搬回阻断遥测。
            let mut new_state = build_config_state(candidate.clone(), constraints.clone())
                .with_context(|| format!("failed to compile updated network {list_name}"))?;
            new_state.blocked = blocked;
            new_state.blocked_total = blocked_total;

            let mut guard = self.state.write().await;
            // 写时复核：约束或基线配置在校验/编译期间被并发改动则重来。
            if guard.constraints != constraints || guard.config != previous_cfg {
                drop(guard);
                continue;
            }

            log_policy_changes(&guard.config, &candidate);
            *guard = new_state;
            info!("updated network {list_name} with {normalized_host}");
            return Ok(());
        }
    }

    /// 所有公共查询/改写方法的统一前置：按需热重载。
    /// `maybe_reload()` 返回 `None`（无变化）时零成本直接放行；返回新状态时
    /// 搬回阻断遥测、记差异日志并整体替换。这就是「改 config.toml 即刻生效」的实现点。
    async fn reload_if_needed(&self) -> Result<()> {
        match self.reloader.maybe_reload().await? {
            None => Ok(()),
            Some(mut new_state) => {
                let (previous_cfg, blocked, blocked_total) = {
                    let guard = self.state.read().await;
                    (
                        guard.config.clone(),
                        guard.blocked.clone(),
                        guard.blocked_total,
                    )
                };
                log_policy_changes(&previous_cfg, &new_state.config);
                new_state.blocked = blocked;
                new_state.blocked_total = blocked_total;
                {
                    let mut guard = self.state.write().await;
                    *guard = new_state;
                }
                let source = self.reloader.source_label();
                info!("reloaded config from {source}");
                Ok(())
            }
        }
    }
}

/// 把「白名单 vs 黑名单」参数化，让 `update_domain_list` 一套代码同时服务
/// 加白/加黑：各方法分别给出名单名、约束字段名、权限值、本侧/对侧条目。
#[derive(Clone, Copy)]
enum DomainListKind {
    Allow,
    Deny,
}

impl DomainListKind {
    fn list_name(self) -> &'static str {
        match self {
            Self::Allow => "allowlist",
            Self::Deny => "denylist",
        }
    }

    fn constraint_field(self) -> &'static str {
        match self {
            Self::Allow => "network.allowed_domains",
            Self::Deny => "network.denied_domains",
        }
    }

    fn permission(self) -> NetworkDomainPermission {
        match self {
            Self::Allow => NetworkDomainPermission::Allow,
            Self::Deny => NetworkDomainPermission::Deny,
        }
    }

    fn entries(self, network: &crate::config::NetworkProxySettings) -> Vec<String> {
        match self {
            Self::Allow => network.allowed_domains().unwrap_or_default(),
            Self::Deny => network.denied_domains().unwrap_or_default(),
        }
    }

    fn opposite_entries(self, network: &crate::config::NetworkProxySettings) -> Vec<String> {
        match self {
            Self::Allow => network.denied_domains().unwrap_or_default(),
            Self::Deny => network.allowed_domains().unwrap_or_default(),
        }
    }
}

// unix socket 放行能力仅 macOS 支持；其他平台 allowUnixSockets 一律按拒绝处理。
pub(crate) fn unix_socket_permissions_supported() -> bool {
    cfg!(target_os = "macos")
}

/// 判断 host 是否解析到非公网 IP（本地/私网/链路本地等）——本地网络防护的判定核心。
/// `lookup` 以闭包注入便于测试（生产传 `tokio::net::lookup_host`）。
/// 返回 `true` 即「应拦截」：注意此处采取「fail-closed（失败即拦）」策略——
/// DNS 失败/超时都返回 `true`，宁可误拦也不放过可能指向内网的目标。
async fn host_resolves_to_non_public_ip<F, Fut>(
    host: &str,
    port: u16,
    lookup_timeout: Duration,
    lookup: F,
) -> bool
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_non_public_ip(ip);
    }

    // Block the request if this DNS lookup fails. We resolve the hostname again when we connect,
    // so a failed check here does not prove the destination is public.
    // 解析失败就拦：连接时还会再解析一次，这里失败并不能证明目标是公网，
    // 故按「无法证明安全 → 拦截」处理（fail-closed）。
    let addrs = match timeout(lookup_timeout, lookup(host.to_string(), port)).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(err)) => {
            debug!(
                "blocking host because DNS lookup failed during local/private IP check (host={host}, port={port}): {err}"
            );
            return true;
        }
        Err(_) => {
            debug!(
                "blocking host because DNS lookup timed out during local/private IP check (host={host}, port={port})"
            );
            return true;
        }
    };

    for addr in addrs {
        if is_non_public_ip(addr.ip()) {
            return true;
        }
    }

    false
}

fn log_policy_changes(previous: &NetworkProxyConfig, next: &NetworkProxyConfig) {
    let previous_allowed_domains = previous.network.allowed_domains().unwrap_or_default();
    let next_allowed_domains = next.network.allowed_domains().unwrap_or_default();
    log_domain_list_changes(
        "allowlist",
        &previous_allowed_domains,
        &next_allowed_domains,
    );
    let previous_denied_domains = previous.network.denied_domains().unwrap_or_default();
    let next_denied_domains = next.network.denied_domains().unwrap_or_default();
    log_domain_list_changes("denylist", &previous_denied_domains, &next_denied_domains);
}

fn log_domain_list_changes(list_name: &str, previous: &[String], next: &[String]) {
    let previous_set: HashSet<String> = previous
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();
    let next_set: HashSet<String> = next
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();

    let added = next_set
        .difference(&previous_set)
        .cloned()
        .collect::<HashSet<_>>();
    let removed = previous_set
        .difference(&next_set)
        .cloned()
        .collect::<HashSet<_>>();

    let mut seen_next = HashSet::new();
    for entry in next {
        let key = entry.to_ascii_lowercase();
        if seen_next.insert(key.clone()) && added.contains(&key) {
            info!("config entry added to {list_name}: {entry}");
        }
    }

    let mut seen_previous = HashSet::new();
    for entry in previous {
        let key = entry.to_ascii_lowercase();
        if seen_previous.insert(key.clone()) && removed.contains(&key) {
            info!("config entry removed from {list_name}: {entry}");
        }
    }
}

// 用 host 原文及其「去 scope 的 IP 字面量」两种形式去匹配 globset。
// 兼顾带 scope 的 IPv6（如 `fe80::1%lo0`）：规则里通常只写 `fe80::1`，
// 需剥掉 `%lo0` 再匹配，否则带 scope 的请求会绕过 deny/allow。
fn globset_matches_host_or_unscoped(set: &GlobSet, host: &str) -> bool {
    set.is_match(host) || unscoped_ip_literal(host).is_some_and(|ip| set.is_match(ip))
}

/// 判断本地/内网目标是否被「显式精确」放行（而非通配命中）。
/// 关键安全约束：含 `*` / `?` 的通配 pattern（包括全局 `*`、`*.x`、`**.x`）
/// 一律不算放行——放开本地网络必须逐条精确列出，避免一个 `*` 把内网全暴露。
fn is_explicit_local_allowlisted(allowed_domains: &[String], host: &Host) -> bool {
    let normalized_host = host.as_str();
    let unscoped_host = unscoped_ip_literal(normalized_host);
    allowed_domains.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern == "*" || pattern.starts_with("*.") || pattern.starts_with("**.") {
            return false;
        }
        if pattern.contains('*') || pattern.contains('?') {
            return false;
        }
        let normalized_pattern = normalize_host(pattern);
        normalized_pattern == normalized_host
            || unscoped_host.is_some_and(|ip| normalized_pattern == ip)
    })
}

fn unix_timestamp() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
pub(crate) fn network_proxy_state_for_policy(
    mut network: crate::config::NetworkProxySettings,
) -> NetworkProxyState {
    network.enabled = true;
    let config = NetworkProxyConfig { network };
    let state = ConfigState {
        allow_set: crate::policy::compile_allowlist_globset(
            &config.network.allowed_domains().unwrap_or_default(),
        )
        .unwrap(),
        blocked: VecDeque::new(),
        blocked_total: 0,
        config: config.clone(),
        constraints: NetworkProxyConstraints::default(),
        deny_set: crate::policy::compile_denylist_globset(
            &config.network.denied_domains().unwrap_or_default(),
        )
        .unwrap(),
        mitm: None,
        mitm_hooks: crate::mitm_hook::compile_mitm_hooks(&config).unwrap(),
    };

    NetworkProxyState::with_reloader(state, Arc::new(NoopReloader))
}

#[cfg(test)]
struct NoopReloader;

#[cfg(test)]
#[async_trait]
impl ConfigReloader for NoopReloader {
    fn source_label(&self) -> String {
        "test config state".to_string()
    }

    async fn maybe_reload(&self) -> Result<Option<ConfigState>> {
        Ok(None)
    }

    async fn reload_now(&self) -> Result<ConfigState> {
        Err(anyhow::anyhow!("force reload is not supported in tests"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::NetworkProxyConfig;
    use crate::config::NetworkProxySettings;
    use crate::policy::compile_allowlist_globset;
    use crate::policy::compile_denylist_globset;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use crate::state::validate_policy_against_constraints;
    use pretty_assertions::assert_eq;

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    fn network_settings(allowed_domains: &[&str], denied_domains: &[&str]) -> NetworkProxySettings {
        let mut network = NetworkProxySettings::default();
        if !allowed_domains.is_empty() {
            network.set_allowed_domains(strings(allowed_domains));
        }
        if !denied_domains.is_empty() {
            network.set_denied_domains(strings(denied_domains));
        }
        network
    }

    fn network_settings_with_unix_sockets(
        allowed_domains: &[&str],
        denied_domains: &[&str],
        unix_sockets: &[String],
    ) -> NetworkProxySettings {
        let mut network = network_settings(allowed_domains, denied_domains);
        if !unix_sockets.is_empty() {
            network.set_allow_unix_sockets(unix_sockets.to_vec());
        }
        network
    }

    #[tokio::test]
    async fn host_blocked_denied_wins_over_allowed() {
        let state =
            network_proxy_state_for_policy(network_settings(&["example.com"], &["example.com"]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn host_blocked_requires_allowlist_match() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            // Use a public IP literal to avoid relying on ambient DNS behavior (some networks
            // resolve unknown hostnames to private IPs, which would trigger `not_allowed_local`).
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn add_allowed_domain_removes_matching_deny_entry() {
        let state = network_proxy_state_for_policy(network_settings(&[], &["example.com"]));

        state.add_allowed_domain("ExAmPlE.CoM").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(allowed, vec!["example.com".to_string()]);
        assert!(denied.is_empty());
        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn add_denied_domain_removes_matching_allow_entry() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        state.add_denied_domain("EXAMPLE.COM").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert!(allowed.is_empty());
        assert_eq!(denied, vec!["example.com".to_string()]);
        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn add_denied_domain_forces_block_with_global_wildcard_allowlist() {
        let state = network_proxy_state_for_policy(network_settings(&["*"], &[]));

        assert_eq!(
            // Use a public IP literal to avoid relying on ambient DNS behavior.
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );

        state.add_denied_domain("8.8.8.8").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(allowed, vec!["*".to_string()]);
        assert_eq!(denied, vec!["8.8.8.8".to_string()]);
        assert_eq!(
            state.host_blocked("8.8.8.8", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn add_allowed_domain_succeeds_when_managed_baseline_allows_expansion() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["managed.example.com"], &[]);
                network.enabled = true;
                network
            },
        };
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["managed.example.com".to_string()]),
            allowlist_expansion_enabled: Some(true),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints).unwrap(),
            Arc::new(NoopReloader),
        );

        state.add_allowed_domain("user.example.com").await.unwrap();

        let (allowed, denied) = state.current_patterns().await.unwrap();
        assert_eq!(
            allowed,
            vec![
                "managed.example.com".to_string(),
                "user.example.com".to_string()
            ]
        );
        assert!(denied.is_empty());
    }

    #[tokio::test]
    async fn add_allowed_domain_rejects_expansion_when_managed_baseline_is_fixed() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["managed.example.com"], &[]);
                network.enabled = true;
                network
            },
        };
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["managed.example.com".to_string()]),
            allowlist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints).unwrap(),
            Arc::new(NoopReloader),
        );

        let err = state
            .add_allowed_domain("user.example.com")
            .await
            .expect_err("managed baseline should reject allowlist expansion");

        assert!(
            format!("{err:#}").contains("network.allowed_domains constrained by managed config"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn add_denied_domain_rejects_expansion_when_managed_baseline_is_fixed() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&[], &["managed.example.com"]);
                network.enabled = true;
                network
            },
        };
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["managed.example.com".to_string()]),
            denylist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };
        let state = NetworkProxyState::with_reloader(
            build_config_state(config, constraints).unwrap(),
            Arc::new(NoopReloader),
        );

        let err = state
            .add_denied_domain("user.example.com")
            .await
            .expect_err("managed baseline should reject denylist expansion");

        assert!(
            format!("{err:#}").contains("network.denied_domains constrained by managed config"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn blocked_snapshot_does_not_consume_entries() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());

        state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: "google.com".to_string(),
                reason: "not_allowed".to_string(),
                client: None,
                method: Some("GET".to_string()),
                mode: None,
                protocol: "http".to_string(),
                decision: Some("ask".to_string()),
                source: Some("decider".to_string()),
                port: Some(80),
            }))
            .await
            .expect("entry should be recorded");

        let snapshot = state
            .blocked_snapshot()
            .await
            .expect("snapshot should succeed");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].host, "google.com");
        assert_eq!(snapshot[0].decision.as_deref(), Some("ask"));

        let drained = state
            .drain_blocked()
            .await
            .expect("drain should include snapshot entry");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].host, snapshot[0].host);
        assert_eq!(drained[0].reason, snapshot[0].reason);
        assert_eq!(drained[0].decision, snapshot[0].decision);
        assert_eq!(drained[0].source, snapshot[0].source);
        assert_eq!(drained[0].port, snapshot[0].port);
    }

    #[tokio::test]
    async fn drain_blocked_returns_buffered_window() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());

        for idx in 0..(MAX_BLOCKED_EVENTS + 5) {
            state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: format!("example{idx}.com"),
                    reason: "not_allowed".to_string(),
                    client: None,
                    method: Some("GET".to_string()),
                    mode: None,
                    protocol: "http".to_string(),
                    decision: Some("ask".to_string()),
                    source: Some("decider".to_string()),
                    port: Some(80),
                }))
                .await
                .expect("entry should be recorded");
        }

        let blocked = state.drain_blocked().await.expect("drain should succeed");
        assert_eq!(blocked.len(), MAX_BLOCKED_EVENTS);
        assert_eq!(blocked[0].host, "example5.com");
    }

    #[test]
    fn blocked_request_violation_log_line_serializes_payload() {
        let entry = BlockedRequest {
            host: "google.com".to_string(),
            reason: "not_allowed".to_string(),
            client: Some("127.0.0.1".to_string()),
            method: Some("GET".to_string()),
            mode: Some(NetworkMode::Full),
            protocol: "http".to_string(),
            decision: Some("ask".to_string()),
            source: Some("decider".to_string()),
            port: Some(80),
            timestamp: 1_735_689_600,
        };

        assert_eq!(
            blocked_request_violation_log_line(&entry),
            r#"CODEX_NETWORK_POLICY_VIOLATION {"host":"google.com","reason":"not_allowed","client":"127.0.0.1","method":"GET","mode":"full","protocol":"http","decision":"ask","source":"decider","port":80,"timestamp":1735689600}"#
        );
    }

    #[tokio::test]
    async fn host_blocked_subdomain_wildcards_exclude_apex() {
        let state = network_proxy_state_for_policy(network_settings(&["*.openai.com"], &[]));

        assert_eq!(
            state
                .host_blocked("api.openai.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state.host_blocked("openai.com", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn host_blocked_global_wildcard_allowlist_allows_public_hosts_except_denylist() {
        let state = network_proxy_state_for_policy(network_settings(&["*"], &["evil.example"]));

        assert_eq!(
            state
                .host_blocked("example.com", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("api.openai.com", /*port*/ 443)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("evil.example", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_loopback_when_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state.host_blocked("127.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
        assert_eq!(
            state.host_blocked("localhost", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_loopback_when_explicitly_allowlisted_and_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["localhost"], &[]));

        assert_eq!(
            state.host_blocked("localhost", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_private_ip_literal_when_explicitly_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["10.0.0.1"], &[]));

        assert_eq!(
            state.host_blocked("10.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_scoped_ipv6_literal_when_not_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state
                .host_blocked("fe80::1%lo0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_allows_scoped_ipv6_literal_when_explicitly_allowlisted() {
        let state = network_proxy_state_for_policy(network_settings(&["fe80::1"], &[]));

        assert_eq!(
            state
                .host_blocked("fe80::1%lo0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_requires_exact_scoped_ipv6_allowlist_match() {
        let state = network_proxy_state_for_policy(NetworkProxySettings {
            allow_local_binding: true,
            ..network_settings(&["fe80::1%eth0"], &[])
        });

        assert_eq!(
            state
                .host_blocked("fe80::1%eth0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
        assert_eq!(
            state
                .host_blocked("fe80::1%eth1", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn host_blocked_denies_scoped_ipv6_literal_before_local_binding() {
        let state = network_proxy_state_for_policy(NetworkProxySettings {
            allow_local_binding: true,
            ..network_settings(&["*"], &["fd00::1"])
        });

        for host in ["fd00::1%eth0", "[fd00::1%eth0]", "[fd00::1%25eth0]"] {
            assert_eq!(
                state.host_blocked(host, /*port*/ 80).await.unwrap(),
                HostBlockDecision::Blocked(HostBlockReason::Denied),
                "host should be denied after normalization: {host}"
            );
        }
    }

    #[tokio::test]
    async fn host_blocked_requires_exact_scoped_ipv6_denylist_match() {
        let state = network_proxy_state_for_policy(NetworkProxySettings {
            allow_local_binding: true,
            ..network_settings(&["*"], &["fd00::1%eth0"])
        });

        assert_eq!(
            state
                .host_blocked("fd00::1%eth0", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::Denied)
        );
        assert_eq!(
            state
                .host_blocked("fd00::1%eth1", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Allowed
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_private_ip_literals_when_local_binding_disabled() {
        let state = network_proxy_state_for_policy(network_settings(&["example.com"], &[]));

        assert_eq!(
            state.host_blocked("10.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_loopback_when_allowlist_empty() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());

        assert_eq!(
            state.host_blocked("127.0.0.1", /*port*/ 80).await.unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_blocked_rejects_allowlisted_hostname_when_dns_lookup_fails() {
        let mut network = NetworkProxySettings::default();
        network.set_allowed_domains(vec!["does-not-resolve.invalid".to_string()]);
        let state = network_proxy_state_for_policy(network);

        assert_eq!(
            state
                .host_blocked("does-not-resolve.invalid", /*port*/ 80)
                .await
                .unwrap(),
            HostBlockDecision::Blocked(HostBlockReason::NotAllowedLocal)
        );
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_on_dns_lookup_timeout() {
        let blocked = host_resolves_to_non_public_ip(
            "slow.example",
            /*port*/ 80,
            Duration::from_millis(1),
            |_host, _port| async {
                std::future::pending::<std::io::Result<Vec<SocketAddr>>>().await
            },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_on_dns_lookup_error() {
        let blocked = host_resolves_to_non_public_ip(
            "error.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async {
                Err::<Vec<SocketAddr>, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "forced failure",
                ))
            },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_blocks_private_resolution() {
        let blocked = host_resolves_to_non_public_ip(
            "local.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async { Ok(vec!["127.0.0.1:80".parse().unwrap()]) },
        )
        .await;

        assert!(blocked);
    }

    #[tokio::test]
    async fn host_resolves_to_non_public_ip_allows_public_resolution() {
        let blocked = host_resolves_to_non_public_ip(
            "public.example",
            /*port*/ 80,
            Duration::from_millis(10),
            |_host, _port| async { Ok(vec!["8.8.8.8:80".parse().unwrap()]) },
        )
        .await;

        assert!(!blocked);
    }

    #[test]
    fn validate_policy_against_constraints_disallows_widening_allowed_domains() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["example.com", "evil.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_expanding_allowed_domains_when_enabled() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["example.com".to_string()]),
            allowlist_expansion_enabled: Some(true),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["example.com", "api.openai.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_widening_mode() {
        let constraints = NetworkProxyConstraints {
            mode: Some(NetworkMode::Limited),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                mode: NetworkMode::Full,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_narrowing_wildcard_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*.example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["api.example.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_widening_wildcard_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*.example.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["**.example.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_global_wildcard_in_managed_allowlist() {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["*".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["api.example.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_bracketed_global_wildcard_in_managed_allowlist()
    {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["[*]".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["api.example.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_rejects_double_wildcard_bracketed_global_wildcard_in_managed_allowlist()
     {
        let constraints = NetworkProxyConstraints {
            allowed_domains: Some(vec!["**.[*]".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["api.example.com"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_requires_managed_denied_domains_entries() {
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["evil.com".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_expanding_denied_domains_when_fixed() {
        let constraints = NetworkProxyConstraints {
            denied_domains: Some(vec!["evil.com".to_string()]),
            denylist_expansion_enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&[], &["evil.com", "more-evil.com"]);
                network.enabled = true;
                network
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_enabling_when_managed_disabled() {
        let constraints = NetworkProxyConstraints {
            enabled: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_local_binding_when_managed_disabled() {
        let constraints = NetworkProxyConstraints {
            allow_local_binding: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                allow_local_binding: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_all_unix_sockets_without_managed_opt_in()
    {
        let constraints = NetworkProxyConstraints {
            dangerously_allow_all_unix_sockets: Some(false),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                dangerously_allow_all_unix_sockets: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_disallows_allow_all_unix_sockets_when_allowlist_is_managed()
     {
        let constraints = NetworkProxyConstraints {
            allow_unix_sockets: Some(vec!["/tmp/allowed.sock".to_string()]),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                dangerously_allow_all_unix_sockets: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_err());
    }

    #[test]
    fn validate_policy_against_constraints_allows_allow_all_unix_sockets_with_managed_opt_in() {
        let constraints = NetworkProxyConstraints {
            dangerously_allow_all_unix_sockets: Some(true),
            ..NetworkProxyConstraints::default()
        };

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                dangerously_allow_all_unix_sockets: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn validate_policy_against_constraints_allows_allow_all_unix_sockets_when_unmanaged() {
        let constraints = NetworkProxyConstraints::default();

        let config = NetworkProxyConfig {
            network: NetworkProxySettings {
                enabled: true,
                dangerously_allow_all_unix_sockets: true,
                ..NetworkProxySettings::default()
            },
        };

        assert!(validate_policy_against_constraints(&config, &constraints).is_ok());
    }

    #[test]
    fn compile_globset_is_case_insensitive() {
        let patterns = vec!["ExAmPle.CoM".to_string()];
        let set = compile_denylist_globset(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("EXAMPLE.COM"));
    }

    #[test]
    fn compile_globset_excludes_apex_for_subdomain_patterns() {
        let patterns = vec!["*.openai.com".to_string()];
        let set = compile_denylist_globset(&patterns).unwrap();
        assert!(set.is_match("api.openai.com"));
        assert!(!set.is_match("openai.com"));
        assert!(!set.is_match("evilopenai.com"));
    }

    #[test]
    fn compile_globset_includes_apex_for_double_wildcard_patterns() {
        let patterns = vec!["**.openai.com".to_string()];
        let set = compile_denylist_globset(&patterns).unwrap();
        assert!(set.is_match("openai.com"));
        assert!(set.is_match("api.openai.com"));
        assert!(!set.is_match("evilopenai.com"));
    }

    #[test]
    fn compile_globset_rejects_global_wildcard() {
        let patterns = vec!["*".to_string()];
        assert!(compile_denylist_globset(&patterns).is_err());
    }

    #[test]
    fn compile_globset_allows_global_wildcard_when_enabled() {
        let patterns = vec!["*".to_string()];
        let set = compile_allowlist_globset(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("api.openai.com"));
        assert!(set.is_match("localhost"));
    }

    #[test]
    fn compile_globset_rejects_bracketed_global_wildcard() {
        let patterns = vec!["[*]".to_string()];
        assert!(compile_denylist_globset(&patterns).is_err());
    }

    #[test]
    fn compile_globset_rejects_double_wildcard_bracketed_global_wildcard() {
        let patterns = vec!["**.[*]".to_string()];
        assert!(compile_denylist_globset(&patterns).is_err());
    }

    #[test]
    fn compile_globset_dedupes_patterns_without_changing_behavior() {
        let patterns = vec!["example.com".to_string(), "example.com".to_string()];
        let set = compile_denylist_globset(&patterns).unwrap();
        assert!(set.is_match("example.com"));
        assert!(set.is_match("EXAMPLE.COM"));
        assert!(!set.is_match("not-example.com"));
    }

    #[test]
    fn compile_globset_rejects_invalid_patterns() {
        let patterns = vec!["[".to_string()];
        assert!(compile_denylist_globset(&patterns).is_err());
    }

    #[test]
    fn build_config_state_allows_global_wildcard_allowed_domains() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["*"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(build_config_state(config, NetworkProxyConstraints::default()).is_ok());
    }

    #[test]
    fn build_config_state_allows_bracketed_global_wildcard_allowed_domains() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["[*]"], &[]);
                network.enabled = true;
                network
            },
        };

        assert!(build_config_state(config, NetworkProxyConstraints::default()).is_ok());
    }

    #[test]
    fn build_config_state_rejects_global_wildcard_denied_domains() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["example.com"], &["*"]);
                network.enabled = true;
                network
            },
        };

        assert!(build_config_state(config, NetworkProxyConstraints::default()).is_err());
    }

    #[test]
    fn build_config_state_rejects_bracketed_global_wildcard_denied_domains() {
        let config = NetworkProxyConfig {
            network: {
                let mut network = network_settings(&["example.com"], &["[*]"]);
                network.enabled = true;
                network
            },
        };

        assert!(build_config_state(config, NetworkProxyConstraints::default()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allowlist_is_respected_on_macos() {
        let socket_path = "/tmp/example.sock".to_string();
        let state = network_proxy_state_for_policy(network_settings_with_unix_sockets(
            &["example.com"],
            &[],
            std::slice::from_ref(&socket_path),
        ));

        assert!(state.is_unix_socket_allowed(&socket_path).await.unwrap());
        assert!(
            !state
                .is_unix_socket_allowed("/tmp/not-allowed.sock")
                .await
                .unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allowlist_resolves_symlinks() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;

        let temp_dir = tempdir().unwrap();
        let dir = temp_dir.path();

        let real = dir.join("real.sock");
        let link = dir.join("link.sock");

        // The allowlist mechanism is path-based; for test purposes we don't need an actual unix
        // domain socket. Any filesystem entry works for canonicalization.
        std::fs::write(&real, b"not a socket").unwrap();
        symlink(&real, &link).unwrap();

        let real_s = real.to_str().unwrap().to_string();
        let link_s = link.to_str().unwrap().to_string();

        let state = network_proxy_state_for_policy(network_settings_with_unix_sockets(
            &["example.com"],
            &[],
            std::slice::from_ref(&real_s),
        ));

        assert!(state.is_unix_socket_allowed(&link_s).await.unwrap());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unix_socket_allow_all_flag_bypasses_allowlist() {
        let state = network_proxy_state_for_policy({
            let mut network = network_settings(&["example.com"], &[]);
            network.dangerously_allow_all_unix_sockets = true;
            network
        });

        assert!(state.is_unix_socket_allowed("/tmp/any.sock").await.unwrap());
        assert!(!state.is_unix_socket_allowed("relative.sock").await.unwrap());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn unix_socket_allowlist_is_rejected_on_non_macos() {
        let socket_path = "/tmp/example.sock".to_string();
        let state = network_proxy_state_for_policy({
            let mut network = network_settings_with_unix_sockets(
                &["example.com"],
                &[],
                std::slice::from_ref(&socket_path),
            );
            network.dangerously_allow_all_unix_sockets = true;
            network
        });

        assert!(!state.is_unix_socket_allowed(&socket_path).await.unwrap());
    }
}
