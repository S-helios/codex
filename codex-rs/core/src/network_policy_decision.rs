//! 【文件职责】core 侧的网络策略「决策翻译层」：在网络代理的阻断结果、用户
//! 审批上下文、与 execpolicy 网络规则三者之间做转换与措辞，把底层的
//! deny/ask 决策呈现为可读消息，并把用户的批准/拒绝固化为一条 execpolicy 规则。
//!
//! 【架构位置】
//!   层级：Agent 核心层 → 网络审批/策略桥接
//!   上游：审批流程在收到代理 `ask`（需用户决定）或 `deny`（已被拦截）时调用本文件。
//!   下游：`codex_network_proxy`（`BlockedRequest`/决策类型）、
//!         `codex_protocol::approvals`（审批上下文/修正动作）、
//!         `codex_execpolicy`（把审批结果落成网络规则）。
//!
//! 【三个职责函数】
//!   - `network_approval_context_from_payload`：仅当决策是「来自 decider 的 ask」
//!     时，从载荷抽出 host+protocol 组成审批上下文（否则无需弹审批）。
//!   - `denied_network_policy_message`：把已被 deny 的阻断记录转成给用户看的
//!     解释文案（按 reason 给出具体原因）。
//!   - `execpolicy_network_rule_amendment`：把用户对某 host 的 allow/deny 决定
//!     翻译成一条 execpolicy 网络规则（协议 + 决策 + 人读 justification）。

use codex_execpolicy::Decision as ExecPolicyDecision;
use codex_execpolicy::NetworkRuleProtocol as ExecPolicyNetworkRuleProtocol;
use codex_network_proxy::BlockedRequest;
use codex_network_proxy::NetworkPolicyDecision;
use codex_protocol::approvals::NetworkApprovalContext;
use codex_protocol::approvals::NetworkApprovalProtocol;
use codex_protocol::approvals::NetworkPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyRuleAction;
use codex_protocol::network_policy::NetworkPolicyDecisionPayload;

/// 一条由用户审批结果转化而来的 execpolicy 网络规则修正：作用协议、决策
/// （Allow/Forbidden）、以及人读的 `justification`（记录为何加这条规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecPolicyNetworkRuleAmendment {
    pub protocol: ExecPolicyNetworkRuleProtocol,
    pub decision: ExecPolicyDecision,
    pub justification: String,
}

fn parse_network_policy_decision(value: &str) -> Option<NetworkPolicyDecision> {
    match value {
        "deny" => Some(NetworkPolicyDecision::Deny),
        "ask" => Some(NetworkPolicyDecision::Ask),
        _ => None,
    }
}

/// 从决策载荷构造「网络审批上下文」，仅在确实需要弹审批时返回 `Some`。
///
/// 仅处理「来自 decider 的 ask」——只有这种情形才需要向用户征询；其余
/// （直接 allow/deny、或非 decider 来源）一律返回 `None` 表示无需审批。
/// 同时要求 protocol 与非空 host 都齐备，缺一则放弃（信息不足无法发起审批）。
pub(crate) fn network_approval_context_from_payload(
    payload: &NetworkPolicyDecisionPayload,
) -> Option<NetworkApprovalContext> {
    if !payload.is_ask_from_decider() {
        return None;
    }

    let protocol = payload.protocol?;

    let host = payload.host.as_deref()?.trim();
    if host.is_empty() {
        return None;
    }

    Some(NetworkApprovalContext {
        host: host.to_string(),
        protocol,
    })
}

/// 把一条「被 deny 的」阻断记录翻译成给用户看的解释文案。
///
/// 仅对 `Deny` 决策产出文案——`Ask`/无决策不在此处理（ask 走审批、不是终态拒绝），
/// 这些情形返回 `None`。host 为空时给通用兜底句；否则按 `reason` 映射出具体原因
/// （denied/not_allowed/方法受限/代理禁用等），让用户明白为何被挡。
pub(crate) fn denied_network_policy_message(blocked: &BlockedRequest) -> Option<String> {
    let decision = blocked
        .decision
        .as_deref()
        .and_then(parse_network_policy_decision);
    if decision != Some(NetworkPolicyDecision::Deny) {
        return None;
    }

    let host = blocked.host.trim();
    if host.is_empty() {
        return Some("Network access was blocked by policy.".to_string());
    }

    // 按阻断原因码映射出人读说明；未识别的原因走兜底分支。这些字符串与
    // network-proxy 的 `reasons.rs` 常量一一对应。
    let detail = match blocked.reason.as_str() {
        "denied" => "domain is explicitly denied by policy and cannot be approved from this prompt",
        "not_allowed" => "domain is not on the allowlist for the current sandbox mode",
        "not_allowed_local" => "local/private network addresses are blocked by the sandbox policy",
        "method_not_allowed" => "request method is blocked by the current network mode",
        "proxy_disabled" => "network proxy is disabled",
        _ => "request is blocked by network policy",
    };

    Some(format!(
        "Network access to \"{host}\" was blocked: {detail}."
    ))
}

/// 把用户的审批决定（`NetworkPolicyAmendment` 的 Allow/Deny）翻译成一条
/// execpolicy 网络规则：映射协议、把 Allow→`Allow`/Deny→`Forbidden`，并生成
/// 形如 "Allow https_connect access to {host}" 的人读 justification 备查。
/// 这样用户在审批中做的一次性决定被固化为后续可复用的策略规则。
pub(crate) fn execpolicy_network_rule_amendment(
    amendment: &NetworkPolicyAmendment,
    network_approval_context: &NetworkApprovalContext,
    host: &str,
) -> ExecPolicyNetworkRuleAmendment {
    let protocol = match network_approval_context.protocol {
        NetworkApprovalProtocol::Http => ExecPolicyNetworkRuleProtocol::Http,
        NetworkApprovalProtocol::Https => ExecPolicyNetworkRuleProtocol::Https,
        NetworkApprovalProtocol::Socks5Tcp => ExecPolicyNetworkRuleProtocol::Socks5Tcp,
        NetworkApprovalProtocol::Socks5Udp => ExecPolicyNetworkRuleProtocol::Socks5Udp,
    };
    let (decision, action_verb) = match amendment.action {
        NetworkPolicyRuleAction::Allow => (ExecPolicyDecision::Allow, "Allow"),
        NetworkPolicyRuleAction::Deny => (ExecPolicyDecision::Forbidden, "Deny"),
    };
    let protocol_label = match network_approval_context.protocol {
        NetworkApprovalProtocol::Http => "http",
        NetworkApprovalProtocol::Https => "https_connect",
        NetworkApprovalProtocol::Socks5Tcp => "socks5_tcp",
        NetworkApprovalProtocol::Socks5Udp => "socks5_udp",
    };
    let justification = format!("{action_verb} {protocol_label} access to {host}");

    ExecPolicyNetworkRuleAmendment {
        protocol,
        decision,
        justification,
    }
}

#[cfg(test)]
#[path = "network_policy_decision_tests.rs"]
mod tests;
