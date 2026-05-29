//! 网络策略判定层：把「基线域名策略」和「运行时审批回调」串成一次最终裁决。
//!
//! 【文件职责】定义出网请求的判定契约，核心入口是 `evaluate_host_policy()`：
//!   它先问基线策略（白/黑名单 + 本地网络防护），再在「未命中白名单」
//!   这一灰色地带把决定权交给可选的 `NetworkPolicyDecider`（运行时审批），
//!   最后把裁决结果以 OTel 审计事件落盘。
//!
//! 【架构位置】
//!   层级：网络代理 · 策略判定层
//!   上游：`http_proxy.rs` / `socks5.rs`（每条连接拿到目标 host:port 后调用本层）
//!   下游：`runtime.rs::NetworkProxyState::host_blocked()`（基线域名/IP 判定）、
//!         调用方注入的 `NetworkPolicyDecider`（如把 exec 审批映射到网络放行）
//!
//! 【数据流】
//!   NetworkPolicyRequest → host_blocked()（基线）→ [NotAllowed 时] decider.decide()
//!     → NetworkDecision（Allow / Deny{reason,source,decision}）→ 审计事件
//!
//! 【三道闸门中的定位】本层负责「域名白名单」这道闸门，并衔接「运行时审批」；
//!   「协议方法」闸门（Limited/Full）在 http_proxy/socks5 调用前另行判定。
//!
//! 【阅读建议】先看 `NetworkDecision` 与 `NetworkDecisionSource` 两个枚举理解
//!   裁决形态，再读 `evaluate_host_policy()` 的判定顺序；底部 `test_support`
//!   是测试用的 tracing 采集器，可跳过。

use crate::reasons::REASON_POLICY_DENIED;
use crate::runtime::HostBlockDecision;
use crate::runtime::HostBlockReason;
use crate::state::NetworkProxyState;
use anyhow::Result;
use async_trait::async_trait;
use chrono::SecondsFormat;
use chrono::Utc;
use std::future::Future;
use std::sync::Arc;

const AUDIT_TARGET: &str = "codex_otel.network_proxy";
const POLICY_DECISION_EVENT_NAME: &str = "codex.network_proxy.policy_decision";
const POLICY_SCOPE_DOMAIN: &str = "domain";
const POLICY_SCOPE_NON_DOMAIN: &str = "non_domain";
const POLICY_DECISION_ALLOW: &str = "allow";
const POLICY_DECISION_DENY: &str = "deny";
const POLICY_REASON_ALLOW: &str = "allow";
const DEFAULT_METHOD: &str = "none";
const DEFAULT_CLIENT_ADDRESS: &str = "unknown";

/// 触发判定的传输协议种类，用于审计字段与判定上下文区分。
/// 同一套域名策略对四种协议一视同仁，此枚举只影响日志归类与下游转发方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkProtocol {
    Http,
    HttpsConnect,
    Socks5Tcp,
    Socks5Udp,
}

impl NetworkProtocol {
    pub const fn as_policy_protocol(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::HttpsConnect => "https_connect",
            Self::Socks5Tcp => "socks5_tcp",
            Self::Socks5Udp => "socks5_udp",
        }
    }
}

/// 「拒绝」的两种子形态，决定被拦后的后续动作。
/// `Deny` 是硬拒（直接断流）；`Ask` 表示需要弹出审批让用户决定，
/// 代理侧仍先按拒绝处理，由上层审批 UI 决定是否最终放行。
/// 注意：本枚举只在 `NetworkDecision::Deny` 内部出现，`Allow` 不带它。
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyDecision {
    Deny,
    Ask,
}

impl NetworkPolicyDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// 裁决「出自哪一道闸门」，仅用于审计/可观测，便于排查一次拦截的根因。
/// - `BaselinePolicy`：基线域名/IP 策略（白名单未命中、命中黑名单、本地网络拦截）
/// - `ModeGuard`：协议方法闸门（Limited 模式下非 GET/HEAD/OPTIONS 被拦）
/// - `ProxyState`：代理运行时状态层做出的判定
/// - `Decider`：运行时审批回调（`NetworkPolicyDecider`）覆写或确认的结果
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDecisionSource {
    BaselinePolicy,
    ModeGuard,
    ProxyState,
    Decider,
}

impl NetworkDecisionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaselinePolicy => "baseline_policy",
            Self::ModeGuard => "mode_guard",
            Self::ProxyState => "proxy_state",
            Self::Decider => "decider",
        }
    }
}

/// 一次出网请求的判定输入：目标 host:port + 触发上下文。
/// `command` / `exec_policy_hint` 是给 `NetworkPolicyDecider` 的线索——
/// 调用方可据此把「已批准的 exec 命令前缀（如 `curl *`）」映射为网络放行，
/// 实现「批了命令就别再二次拦它发出的网络请求」的体验。
#[derive(Clone, Debug)]
pub struct NetworkPolicyRequest {
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub client_addr: Option<String>,
    pub method: Option<String>,
    pub command: Option<String>,
    pub exec_policy_hint: Option<String>,
}

pub struct NetworkPolicyRequestArgs {
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub client_addr: Option<String>,
    pub method: Option<String>,
    pub command: Option<String>,
    pub exec_policy_hint: Option<String>,
}

impl NetworkPolicyRequest {
    pub fn new(args: NetworkPolicyRequestArgs) -> Self {
        let NetworkPolicyRequestArgs {
            protocol,
            host,
            port,
            client_addr,
            method,
            command,
            exec_policy_hint,
        } = args;
        Self {
            protocol,
            host,
            port,
            client_addr,
            method,
            command,
            exec_policy_hint,
        }
    }
}

/// 一次判定的最终结果。`Allow` 放行；`Deny` 携带拒绝原因、来源闸门与子形态
/// （硬拒 `Deny` 还是待审批 `Ask`），三者共同支撑审计与上层审批 UI。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkDecision {
    Allow,
    Deny {
        reason: String,
        source: NetworkDecisionSource,
        decision: NetworkPolicyDecision,
    },
}

impl NetworkDecision {
    // 下面 `deny` / `ask` 两个便捷构造默认把来源标为 `Decider`：
    // 它们是给运行时审批回调用的快捷方式。基线策略走 `*_with_source` 显式指定来源。
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_source(reason, NetworkDecisionSource::Decider)
    }

    pub fn ask(reason: impl Into<String>) -> Self {
        Self::ask_with_source(reason, NetworkDecisionSource::Decider)
    }

    pub fn deny_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        let reason = reason.into();
        // 空 reason 兜底为通用文案：审计与 UI 都假设 reason 非空，避免出现
        // 「被拒但说不出为什么」的空白记录。
        let reason = if reason.is_empty() {
            REASON_POLICY_DENIED.to_string()
        } else {
            reason
        };
        Self::Deny {
            reason,
            source,
            decision: NetworkPolicyDecision::Deny,
        }
    }

    pub fn ask_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        let reason = reason.into();
        let reason = if reason.is_empty() {
            REASON_POLICY_DENIED.to_string()
        } else {
            reason
        };
        Self::Deny {
            reason,
            source,
            decision: NetworkPolicyDecision::Ask,
        }
    }
}

// ───────────────────────────────────────────────────────────────
// Audit events  ·  判定结果的 OTel 审计事件
// 供 mode_guard 等「非域名维度」的拦点直接上报；域名维度的判定由
// evaluate_host_policy() 内部统一上报。
// ───────────────────────────────────────────────────────────────

pub(crate) struct BlockDecisionAuditEventArgs<'a> {
    pub source: NetworkDecisionSource,
    pub reason: &'a str,
    pub protocol: NetworkProtocol,
    pub server_address: &'a str,
    pub server_port: u16,
    pub method: Option<&'a str>,
    pub client_addr: Option<&'a str>,
}

pub(crate) fn emit_block_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_non_domain_policy_decision_audit_event(state, args, POLICY_DECISION_DENY);
}

pub(crate) fn emit_allow_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
) {
    emit_non_domain_policy_decision_audit_event(state, args, POLICY_DECISION_ALLOW);
}

fn emit_non_domain_policy_decision_audit_event(
    state: &NetworkProxyState,
    args: BlockDecisionAuditEventArgs<'_>,
    decision: &'static str,
) {
    emit_policy_audit_event(
        state,
        PolicyAuditEventArgs {
            scope: POLICY_SCOPE_NON_DOMAIN,
            decision,
            source: args.source.as_str(),
            reason: args.reason,
            protocol: args.protocol,
            server_address: args.server_address,
            server_port: args.server_port,
            method: args.method,
            client_addr: args.client_addr,
            policy_override: false,
        },
    );
}

struct PolicyAuditEventArgs<'a> {
    scope: &'static str,
    decision: &'a str,
    source: &'a str,
    reason: &'a str,
    protocol: NetworkProtocol,
    server_address: &'a str,
    server_port: u16,
    method: Option<&'a str>,
    client_addr: Option<&'a str>,
    policy_override: bool,
}

// 统一发射一条策略判定审计事件：合并会话级元数据（conversation_id、
// user_email 等）与本次判定字段，打到固定 OTel target `AUDIT_TARGET`。
// 所有放行/拒绝最终都汇聚到这唯一出口，保证审计字段口径一致。
fn emit_policy_audit_event(state: &NetworkProxyState, args: PolicyAuditEventArgs<'_>) {
    let metadata = state.audit_metadata();
    tracing::event!(
        target: AUDIT_TARGET,
        tracing::Level::INFO,
        event.name = POLICY_DECISION_EVENT_NAME,
        event.timestamp = %audit_timestamp(),
        conversation.id = metadata.conversation_id.as_deref(),
        app.version = metadata.app_version.as_deref(),
        auth_mode = metadata.auth_mode.as_deref(),
        originator = metadata.originator.as_deref(),
        user.account_id = metadata.user_account_id.as_deref(),
        user.email = metadata.user_email.as_deref(),
        terminal.type = metadata.terminal_type.as_deref(),
        model = metadata.model.as_deref(),
        slug = metadata.slug.as_deref(),
        network.policy.scope = args.scope,
        network.policy.decision = args.decision,
        network.policy.source = args.source,
        network.policy.reason = args.reason,
        network.transport.protocol = args.protocol.as_policy_protocol(),
        server.address = args.server_address,
        server.port = args.server_port,
        http.request.method = args.method.unwrap_or(DEFAULT_METHOD),
        client.address = args.client_addr.unwrap_or(DEFAULT_CLIENT_ADDRESS),
        network.policy.override = args.policy_override,
    );
}

fn audit_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Decide whether a network request should be allowed.
///
/// If `command` or `exec_policy_hint` is provided, callers can map exec-policy
/// approvals to network access (e.g., allow all requests for commands matching
/// approved prefixes like `curl *`).
///
/// 运行时审批回调：第三道闸门。仅当基线策略判为「未命中白名单(NotAllowed)」
/// 这一灰色地带时才被询问——它可以把请求 Allow（覆写放行）、Deny（确认拒绝）
/// 或 Ask（升级为人工审批）。`Deny`(黑名单)与本地网络拦截不经此回调，
/// 是不可被覆写的硬约束。由 core 侧注入，典型实现把 exec 审批映射为网络放行。
#[async_trait]
pub trait NetworkPolicyDecider: Send + Sync + 'static {
    async fn decide(&self, req: NetworkPolicyRequest) -> NetworkDecision;
}

#[async_trait]
impl<D: NetworkPolicyDecider + ?Sized> NetworkPolicyDecider for Arc<D> {
    async fn decide(&self, req: NetworkPolicyRequest) -> NetworkDecision {
        (**self).decide(req).await
    }
}

#[async_trait]
impl<F, Fut> NetworkPolicyDecider for F
where
    F: Fn(NetworkPolicyRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = NetworkDecision> + Send,
{
    async fn decide(&self, req: NetworkPolicyRequest) -> NetworkDecision {
        (self)(req).await
    }
}

/// 本 crate 的判定主入口：把基线策略与运行时审批合成一次最终裁决并落审计。
///
/// 判定顺序（安全优先级由高到低）：
///   1. `host_blocked()` 给出基线结论（黑名单 / 本地网络拦截 / 未命中白名单 / 放行）
///   2. 唯独「未命中白名单(NotAllowed)」才咨询 `decider`，允许其覆写为放行；
///      黑名单与本地网络拦截属硬约束，不交给 decider。
///   3. 无论结果如何都发射一条审计事件，`policy_override` 标记是否被审批覆写。
///
/// @returns 最终 `NetworkDecision`；DNS 查询等 I/O 失败时返回 `Err` 向上抛出。
pub(crate) async fn evaluate_host_policy(
    state: &NetworkProxyState,
    decider: Option<&Arc<dyn NetworkPolicyDecider>>,
    request: &NetworkPolicyRequest,
) -> Result<NetworkDecision> {
    // ── Step 1：取基线结论，并仅对灰色地带咨询运行时审批 ──────────────
    let host_decision = state.host_blocked(&request.host, request.port).await?;
    let (decision, policy_override) = match host_decision {
        HostBlockDecision::Allowed => (NetworkDecision::Allow, false),
        HostBlockDecision::Blocked(HostBlockReason::NotAllowed) => {
            // 未命中白名单 = 唯一可被审批覆写的灰色地带。
            // 有 decider 就问它（可放行/拒绝/升级 Ask）；没有则按基线拒绝。
            if let Some(decider) = decider {
                let decider_decision = map_decider_decision(decider.decide(request.clone()).await);
                // policy_override 仅在审批把「本应拒绝」翻成 Allow 时为真，供审计标记。
                let policy_override = matches!(decider_decision, NetworkDecision::Allow);
                (decider_decision, policy_override)
            } else {
                (
                    NetworkDecision::deny_with_source(
                        HostBlockReason::NotAllowed.as_str(),
                        NetworkDecisionSource::BaselinePolicy,
                    ),
                    false,
                )
            }
        }
        // 黑名单命中 / 本地网络拦截：硬约束，直接拒绝，不经 decider。
        HostBlockDecision::Blocked(reason) => (
            NetworkDecision::deny_with_source(
                reason.as_str(),
                NetworkDecisionSource::BaselinePolicy,
            ),
            false,
        ),
    };

    // ── Step 2：从裁决推导审计三元组（决策/来源/原因）并上报 ──────────
    // 被审批覆写的放行要如实归因到 `Decider`、原因记为「本应 NotAllowed」，
    // 以区别于「白名单本就放行」，方便事后审计追溯谁松了口。
    let (policy_decision, source, reason) = match &decision {
        NetworkDecision::Allow => (
            POLICY_DECISION_ALLOW,
            if policy_override {
                NetworkDecisionSource::Decider
            } else {
                NetworkDecisionSource::BaselinePolicy
            },
            if policy_override {
                HostBlockReason::NotAllowed.as_str()
            } else {
                POLICY_REASON_ALLOW
            },
        ),
        NetworkDecision::Deny {
            reason,
            source,
            decision,
        } => (decision.as_str(), *source, reason.as_str()),
    };

    emit_policy_audit_event(
        state,
        PolicyAuditEventArgs {
            scope: POLICY_SCOPE_DOMAIN,
            decision: policy_decision,
            source: source.as_str(),
            reason,
            protocol: request.protocol,
            server_address: request.host.as_str(),
            server_port: request.port,
            method: request.method.as_deref(),
            client_addr: request.client_addr.as_deref(),
            policy_override,
        },
    );

    Ok(decision)
}

// 把审批回调返回的拒绝统一改记来源为 `Decider`：无论回调自报什么 source，
// 既然结论出自它，审计就该归因到它，避免回调伪装成基线策略。
fn map_decider_decision(decision: NetworkDecision) -> NetworkDecision {
    match decision {
        NetworkDecision::Allow => NetworkDecision::Allow,
        NetworkDecision::Deny {
            reason, decision, ..
        } => NetworkDecision::Deny {
            reason,
            source: NetworkDecisionSource::Decider,
            decision,
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) const POLICY_DECISION_EVENT_NAME: &str = super::POLICY_DECISION_EVENT_NAME;

    use std::collections::BTreeMap;
    use std::fmt;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use tracing::Event;
    use tracing::Id;
    use tracing::Metadata;
    use tracing::Subscriber;
    use tracing::field::Field;
    use tracing::field::Visit;
    use tracing::span::Attributes;
    use tracing::span::Record;
    use tracing::subscriber::Interest;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct CapturedEvent {
        pub target: String,
        pub fields: BTreeMap<String, String>,
    }

    impl CapturedEvent {
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }
    }

    #[derive(Clone, Default)]
    struct EventCollector {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
        next_span_id: Arc<AtomicU64>,
    }

    impl EventCollector {
        fn events(&self) -> Vec<CapturedEvent> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Subscriber for EventCollector {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(CapturedEvent {
                    target: event.metadata().target().to_string(),
                    fields: visitor.fields,
                });
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: BTreeMap<String, String>,
    }

    impl FieldVisitor {
        fn insert(&mut self, field: &Field, value: impl Into<String>) {
            self.fields.insert(field.name().to_string(), value.into());
        }
    }

    impl Visit for FieldVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.insert(field, value);
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.insert(field, value.to_string());
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.insert(field, value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.insert(field, value.to_string());
        }

        fn record_i128(&mut self, field: &Field, value: i128) {
            self.insert(field, value.to_string());
        }

        fn record_u128(&mut self, field: &Field, value: u128) {
            self.insert(field, value.to_string());
        }

        fn record_f64(&mut self, field: &Field, value: f64) {
            self.insert(field, value.to_string());
        }

        fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
            self.insert(field, value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.insert(field, format!("{value:?}"));
        }
    }

    pub(crate) async fn capture_events<F, Fut, T>(f: F) -> (T, Vec<CapturedEvent>)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let collector = EventCollector::default();
        let _guard = tracing::subscriber::set_default(collector.clone());
        let output = f().await;
        let events = collector.events();
        (output, events)
    }

    pub(crate) fn find_event_by_name<'a>(
        events: &'a [CapturedEvent],
        event_name: &str,
    ) -> Option<&'a CapturedEvent> {
        events
            .iter()
            .find(|event| event.field("event.name") == Some(event_name))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::capture_events;
    use super::test_support::find_event_by_name;
    use super::*;
    use crate::config::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use crate::config::NetworkProxySettings;
    use crate::reasons::REASON_DENIED;
    use crate::reasons::REASON_METHOD_NOT_ALLOWED;
    use crate::reasons::REASON_NOT_ALLOWED;
    use crate::reasons::REASON_NOT_ALLOWED_LOCAL;
    use crate::runtime::ConfigReloader;
    use crate::runtime::ConfigState;
    use crate::runtime::NetworkProxyAuditMetadata;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use crate::state::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    const LEGACY_DOMAIN_POLICY_DECISION_EVENT_NAME: &str =
        "codex.network_proxy.domain_policy_decision";
    const LEGACY_BLOCK_DECISION_EVENT_NAME: &str = "codex.network_proxy.block_decision";

    #[derive(Clone)]
    struct StaticReloader {
        state: ConfigState,
    }

    #[async_trait]
    impl ConfigReloader for StaticReloader {
        async fn maybe_reload(&self) -> anyhow::Result<Option<ConfigState>> {
            Ok(None)
        }

        async fn reload_now(&self) -> anyhow::Result<ConfigState> {
            Ok(self.state.clone())
        }

        fn source_label(&self) -> String {
            "static test reloader".to_string()
        }
    }

    fn state_with_metadata(metadata: NetworkProxyAuditMetadata) -> NetworkProxyState {
        let network = NetworkProxySettings {
            enabled: true,
            mode: NetworkMode::Full,
            ..NetworkProxySettings::default()
        };
        let config = NetworkProxyConfig { network };
        let state = build_config_state(config, NetworkProxyConstraints::default()).unwrap();
        let reloader = Arc::new(StaticReloader {
            state: state.clone(),
        });
        NetworkProxyState::with_reloader_and_audit_metadata(state, reloader, metadata)
    }

    fn is_rfc3339_utc_millis(timestamp: &str) -> bool {
        let bytes = timestamp.as_bytes();
        if bytes.len() != 24 {
            return false;
        }
        bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'.'
            && bytes[23] == b'Z'
            && bytes.iter().enumerate().all(|(idx, value)| match idx {
                4 | 7 | 10 | 13 | 16 | 19 | 23 => true,
                _ => value.is_ascii_digit(),
            })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_decider_allow_override() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let decider: Arc<dyn NetworkPolicyDecider> = Arc::new({
            let calls = calls.clone();
            move |_req| {
                calls.fetch_add(1, Ordering::SeqCst);
                // The default policy denies all; the decider is consulted for not_allowed
                // requests and can override that decision.
                async { NetworkDecision::Allow }
            }
        });

        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            client_addr: None,
            method: None,
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, Some(&decider), &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(decision, NetworkDecision::Allow);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.target, AUDIT_TARGET);
        assert!(event.target.starts_with("codex_otel."));
        assert_eq!(
            event.field("network.policy.scope"),
            Some(POLICY_SCOPE_DOMAIN)
        );
        assert_eq!(event.field("network.policy.decision"), Some("allow"));
        assert_eq!(event.field("network.policy.source"), Some("decider"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.transport.protocol"), Some("http"));
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("80"));
        assert_eq!(event.field("http.request.method"), Some(DEFAULT_METHOD));
        assert_eq!(event.field("client.address"), Some(DEFAULT_CLIENT_ADDRESS));
        assert_eq!(event.field("network.policy.override"), Some("true"));
        let timestamp = event
            .field("event.timestamp")
            .expect("event timestamp should be present");
        assert!(is_rfc3339_utc_millis(timestamp));
        assert_eq!(
            find_event_by_name(&events, LEGACY_DOMAIN_POLICY_DECISION_EVENT_NAME),
            None
        );
        assert_eq!(
            find_event_by_name(&events, LEGACY_BLOCK_DECISION_EVENT_NAME),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_baseline_deny() {
        let state = network_proxy_state_for_policy({
            let mut network = NetworkProxySettings::default();
            network.set_allowed_domains(vec!["example.com".to_string()]);
            network.set_denied_domains(vec!["blocked.com".to_string()]);
            network
        });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "blocked.com".to_string(),
            port: 80,
            client_addr: Some("127.0.0.1:1234".to_string()),
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, /*decider*/ None, &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_DENIED.to_string(),
                source: NetworkDecisionSource::BaselinePolicy,
                decision: NetworkPolicyDecision::Deny,
            }
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(
            event.field("network.policy.source"),
            Some("baseline_policy")
        );
        assert_eq!(event.field("network.policy.reason"), Some(REASON_DENIED));
        assert_eq!(event.field("network.policy.override"), Some("false"));
        assert_eq!(event.field("http.request.method"), Some("GET"));
        assert_eq!(event.field("client.address"), Some("127.0.0.1:1234"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_decider_ask() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());
        let decider: Arc<dyn NetworkPolicyDecider> =
            Arc::new(|_req| async { NetworkDecision::ask(REASON_NOT_ALLOWED) });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, Some(&decider), &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED.to_string(),
                source: NetworkDecisionSource::Decider,
                decision: NetworkPolicyDecision::Ask,
            }
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("network.policy.decision"), Some("ask"));
        assert_eq!(event.field("network.policy.source"), Some("decider"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.policy.override"), Some("false"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_metadata_fields() {
        let metadata = NetworkProxyAuditMetadata {
            conversation_id: Some("conversation-1".to_string()),
            app_version: Some("1.2.3".to_string()),
            user_account_id: Some("acct-1".to_string()),
            auth_mode: Some("Chatgpt".to_string()),
            originator: Some("codex_cli_rs".to_string()),
            user_email: Some("test@example.com".to_string()),
            terminal_type: Some("iTerm.app/3.6.5".to_string()),
            model: Some("gpt-5.3-codex".to_string()),
            slug: Some("gpt-5.3-codex".to_string()),
        };
        let state = state_with_metadata(metadata);
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let (_decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, /*decider*/ None, &request)
                .await
                .unwrap()
        })
        .await;

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("conversation.id"), Some("conversation-1"));
        assert_eq!(event.field("app.version"), Some("1.2.3"));
        assert_eq!(event.field("auth_mode"), Some("Chatgpt"));
        assert_eq!(event.field("originator"), Some("codex_cli_rs"));
        assert_eq!(event.field("user.account_id"), Some("acct-1"));
        assert_eq!(event.field("user.email"), Some("test@example.com"));
        assert_eq!(event.field("terminal.type"), Some("iTerm.app/3.6.5"));
        assert_eq!(event.field("model"), Some("gpt-5.3-codex"));
        assert_eq!(event.field("slug"), Some("gpt-5.3-codex"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emit_block_decision_audit_event_emits_non_domain_event() {
        let state = network_proxy_state_for_policy(NetworkProxySettings::default());

        let (_, events) = capture_events(|| async {
            emit_block_decision_audit_event(
                &state,
                BlockDecisionAuditEventArgs {
                    source: NetworkDecisionSource::ModeGuard,
                    reason: REASON_METHOD_NOT_ALLOWED,
                    protocol: NetworkProtocol::Http,
                    server_address: "unix-socket",
                    server_port: 0,
                    method: Some("POST"),
                    client_addr: None,
                },
            );
        })
        .await;

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.target, AUDIT_TARGET);
        assert_eq!(
            event.field("network.policy.scope"),
            Some(POLICY_SCOPE_NON_DOMAIN)
        );
        assert_eq!(
            event.field("network.policy.decision"),
            Some(POLICY_DECISION_DENY)
        );
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.transport.protocol"), Some("http"));
        assert_eq!(event.field("server.address"), Some("unix-socket"));
        assert_eq!(event.field("server.port"), Some("0"));
        assert_eq!(event.field("http.request.method"), Some("POST"));
        assert_eq!(event.field("client.address"), Some(DEFAULT_CLIENT_ADDRESS));
        assert_eq!(event.field("network.policy.override"), Some("false"));
        assert_eq!(
            find_event_by_name(&events, LEGACY_BLOCK_DECISION_EVENT_NAME),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_still_denies_not_allowed_local_without_decider_override() {
        let state = network_proxy_state_for_policy({
            let mut network = NetworkProxySettings::default();
            network.set_allowed_domains(vec!["example.com".to_string()]);
            network.allow_local_binding = false;
            network
        });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "127.0.0.1".to_string(),
            port: 80,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let decision = evaluate_host_policy(&state, /*decider*/ None, &request)
            .await
            .unwrap();
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED_LOCAL.to_string(),
                source: NetworkDecisionSource::BaselinePolicy,
                decision: NetworkPolicyDecision::Deny,
            }
        );
    }

    #[test]
    fn ask_uses_decider_source_and_ask_decision() {
        assert_eq!(
            NetworkDecision::ask(REASON_NOT_ALLOWED),
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED.to_string(),
                source: NetworkDecisionSource::Decider,
                decision: NetworkPolicyDecision::Ask,
            }
        );
    }
}
