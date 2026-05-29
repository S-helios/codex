//! Guardian review decides whether an `on-request` approval should be granted
//! automatically instead of shown to the user.
//!
//! High-level approach:
//! 1. Reconstruct a compact transcript that preserves user intent plus the most
//!    relevant recent assistant and tool context.
//! 2. Ask a dedicated guardian review session to assess the exact planned
//!    action and return strict JSON.
//!    The guardian clones the parent config, so it inherits any managed
//!    network proxy / allowlist that the parent turn already had.
//! 3. Fail closed on timeout, execution failure, or malformed output.
//! 4. Apply the guardian's explicit allow/deny outcome.
//!
//! 【文件职责】Guardian 安全审查子系统的模块根：当审批策略为 `on-request`
//! 时，由一个独立的「审查 Agent」自动裁决某个待执行动作是否放行，从而把
//! 本该弹给用户确认的审批拦截在自动化流程里。
//!
//! 【架构位置】
//!   层级：Agent 核心层 · 审批旁路（在工具执行前介入）
//!   上游：tools/handlers/* 与 session/handlers.rs 产生审批请求后，经
//!         `routes_approval_to_guardian()` 判断是否改走 Guardian
//!   下游：`review_session` 拉起一个受限的子会话（只读沙箱、approval=never）
//!         调用模型，再由 `prompt` 构造提示词、`approval_request` 序列化动作
//!
//! 【数据流】
//!   审批请求 `GuardianApprovalRequest`
//!     → 构造紧凑「转写稿」+ 动作 JSON（prompt.rs）
//!     → 受限审查子会话跑模型（review_session.rs）
//!     → 解析严格 JSON 得 `GuardianAssessment`（prompt.rs）
//!     → 映射为 `ReviewDecision::{Approved,Denied,...}`（review.rs）
//!
//! 【失败即拒（fail closed）】超时 / 子会话失败 / 输出不合法都按「拒绝」处理，
//!   但超时会以独立的 `TimedOut` 状态回传，便于上游区分「明确拒绝」与「没跑完」。
//!
//! 【阅读建议】先看 `review.rs` 的 `review_approval_request()` 入口与
//!   `run_guardian_review()` 主流程，再看 `review_session.rs` 的会话复用
//!   状态机；`prompt.rs` 是提示词与解析，`approval_request.rs` 是动作建模与
//!   序列化，`metrics.rs` 是埋点。

mod approval_request;
mod metrics;
mod prompt;
mod review;
mod review_session;

use std::time::Duration;

use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use codex_protocol::protocol::GuardianAssessmentOutcome;
use serde::Deserialize;
use serde::Serialize;

pub(crate) use approval_request::GuardianApprovalRequest;
pub(crate) use approval_request::GuardianMcpAnnotations;
pub(crate) use approval_request::GuardianNetworkAccessTrigger;
#[cfg(test)]
pub(crate) use approval_request::guardian_approval_request_to_json;
pub(crate) use review::guardian_rejection_message;
pub(crate) use review::guardian_timeout_message;
pub(crate) use review::is_guardian_reviewer_source;
pub(crate) use review::new_guardian_review_id;
#[cfg(test)]
pub(crate) use review::record_guardian_denial_for_test;
pub(crate) use review::review_approval_request;
#[cfg(test)]
pub(crate) use review::review_approval_request_with_cancel;
pub(crate) use review::routes_approval_to_guardian;
pub(crate) use review::spawn_approval_request_review;
pub(crate) use review_session::GuardianReviewSessionManager;
pub(crate) use review_session::prompt_cache_key_override_for_review_session;

// 单次 Guardian 审查的总超时上限。审查会拉起一个子会话跑模型，可能还会做
// 只读工具检查，90s 是兼顾「模型推理 + 工具往返」的安全上限；超时按拒绝处理
// 但以独立状态回传（见 review.rs 的 `TimedOut`）。
pub(crate) const GUARDIAN_REVIEW_TIMEOUT: Duration = Duration::from_secs(90);
// 审查子会话的 SubAgent 名称标识。用于识别「这是 Guardian 发起的会话」
// （`is_guardian_reviewer_source`）以及生成稳定的 prompt-cache key。
pub(crate) const GUARDIAN_REVIEWER_NAME: &str = "guardian";
// 熔断阈值 A：同一轮内 Guardian「连续」拒绝达到此次数即中断该轮。
// 连续被拒通常意味着模型在反复尝试同一类危险动作，继续放任无意义。
pub(crate) const MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN: u32 = 3;
// 熔断阈值 B：同一轮「最近窗口内」累计拒绝达到此次数也中断该轮。
// 与阈值 A 互补——拦截「拒一次换个花样再拒」这类非连续但高频的规避行为。
pub(crate) const MAX_RECENT_AUTO_REVIEW_DENIALS_PER_TURN: u32 = 10;
// 上面「最近窗口」的滑动窗口大小：只统计最近 50 次审查里的拒绝数。
// [引用范围] 被熔断逻辑（本文件 `record_recent_review`）与拒绝提示文案
// （review.rs 的警告消息）共同引用，改动需同步两处语义。
pub(crate) const AUTO_REVIEW_DENIAL_WINDOW_SIZE: usize = 50;
// developer 消息前缀：标记「用户手动批准了一个此前被 Guardian 拒绝的动作」。
// 这是唯一被保留进审查转写稿的 developer 消息类型（见 prompt.rs），用于让
// Guardian 知道某个先前被拒的动作已获用户显式授权。
pub(crate) const AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX: &str =
    "The user has manually approved a specific action that was previously `Rejected`.";
// 转写稿 token 预算（见 prompt.rs 的渲染逻辑）。消息与工具各占独立预算，
// 避免大段工具输出把人类对话挤掉：
// 「消息」类条目（user/assistant/developer）总预算。
const GUARDIAN_MAX_MESSAGE_TRANSCRIPT_TOKENS: usize = 10_000;
// 「工具」类条目（tool call / result）总预算，与消息预算分开计。
const GUARDIAN_MAX_TOOL_TRANSCRIPT_TOKENS: usize = 10_000;
// 单条「消息」条目的截断上限。
const GUARDIAN_MAX_MESSAGE_ENTRY_TOKENS: usize = 2_000;
// 单条「工具」条目的截断上限（比消息更小，工具输出往往冗长）。
const GUARDIAN_MAX_TOOL_ENTRY_TOKENS: usize = 1_000;
// 待审查「动作 JSON」字符串的截断上限：动作本身是审查核心证据，预算给得最高。
const GUARDIAN_MAX_ACTION_STRING_TOKENS: usize = 16_000;
// 转写稿中「最近的非 user 条目」保留条数上限，防止条目数量爆炸。
const GUARDIAN_RECENT_ENTRY_LIMIT: usize = 40;
// 截断标记的标签名，渲染为 `<truncated omitted_approx_tokens="N" />`。
const TRUNCATION_TAG: &str = "truncated";

/// Structured output contract that the guardian reviewer must satisfy.
/// Guardian 审查模型必须产出的结构化裁决契约（最终落到严格 JSON）。
/// 由 `parse_guardian_assessment()` 从模型输出解析得到（见 prompt.rs）。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct GuardianAssessment {
    // 风险等级（low/medium/high/critical），仅用于展示与埋点，不直接决定放行。
    pub(crate) risk_level: codex_protocol::protocol::GuardianRiskLevel,
    // 模型对「用户是否授权了该动作」的判断置信度，同样用于展示与埋点。
    pub(crate) user_authorization: codex_protocol::protocol::GuardianUserAuthorization,
    // 真正决定放行与否的字段：Allow / Deny。
    pub(crate) outcome: GuardianAssessmentOutcome,
    // 裁决理由：被拒时回灌给主 Agent，告知为何被拦（见 GuardianRejection）。
    pub(crate) rationale: String,
}

/// 一次被 Guardian 拒绝的记录，按 review_id 暂存于会话级 map。
/// 主流程随后用它构造给主 Agent 的「为何被拒 + 后续约束」提示
/// （见 review.rs 的 `guardian_rejection_message`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardianRejection {
    pub(crate) rationale: String,
    // 决策来源。当前恒为 `Agent`（即审查模型自身），保留枚举以备扩展。
    pub(crate) source: GuardianAssessmentDecisionSource,
}

/// 拒绝熔断器：按「轮（turn）」维度统计 Guardian 拒绝频次，过高则中断该轮，
/// 避免模型在一轮内对危险动作反复试探、空耗时间与额度。
/// [引用范围] 作为会话服务的共享状态存放（见 session/services），由 review.rs
/// 的 `record_guardian_denial` / `record_guardian_non_denial` 经 Mutex 访问。
#[derive(Debug, Default)]
pub(crate) struct GuardianRejectionCircuitBreaker {
    // 以 turn_id 为键，每轮独立计数；轮结束时由 `clear_turn` 清理。
    turns: std::collections::HashMap<String, GuardianRejectionCircuitBreakerTurn>,
}

/// 单轮的熔断计数状态。
#[derive(Debug, Default)]
struct GuardianRejectionCircuitBreakerTurn {
    // 「连续」拒绝计数：遇到任一非拒绝结果即清零（见 `record_non_denial`）。
    consecutive_denials: u32,
    // 「最近窗口」内每次审查是否被拒的滑动队列，长度上限 = 窗口大小。
    recent_denials: std::collections::VecDeque<bool>,
    // 是否已对本轮触发过中断：只触发一次，避免重复发警告 / 重复中断。
    interrupt_triggered: bool,
}

/// `record_denial` 的返回动作：告诉调用方本次拒绝后该继续还是中断本轮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardianRejectionCircuitBreakerAction {
    Continue,
    // 命中阈值，需中断本轮；附带当前统计值用于面向用户的警告文案。
    InterruptTurn {
        consecutive_denials: u32,
        recent_denials: u32,
    },
}

impl GuardianRejectionCircuitBreaker {
    // 轮结束时清除该轮计数，避免 map 无限增长。
    pub(crate) fn clear_turn(&mut self, turn_id: &str) {
        self.turns.remove(turn_id);
    }

    /// 记录一次「拒绝」并判定是否需要中断本轮。
    /// 返回 `InterruptTurn` 当且仅当：本轮尚未触发过中断，且命中两个阈值之一
    /// （连续拒绝 ≥ A，或最近窗口内拒绝 ≥ B）。命中后置位 `interrupt_triggered`，
    /// 保证一轮只中断一次。
    pub(crate) fn record_denial(&mut self, turn_id: &str) -> GuardianRejectionCircuitBreakerAction {
        let turn = self.turns.entry(turn_id.to_string()).or_default();
        // saturating_add 防溢出：极端高频拒绝下计数也不会回绕。
        turn.consecutive_denials = turn.consecutive_denials.saturating_add(1);
        Self::record_recent_review(turn, /*denied*/ true);
        // 重新数一遍滑动窗口里的拒绝数（窗口长度有限，开销可忽略）。
        let recent_denials = turn.recent_denials.iter().filter(|denied| **denied).count() as u32;
        if !turn.interrupt_triggered
            && (turn.consecutive_denials >= MAX_CONSECUTIVE_GUARDIAN_DENIALS_PER_TURN
                || recent_denials >= MAX_RECENT_AUTO_REVIEW_DENIALS_PER_TURN)
        {
            turn.interrupt_triggered = true;
            GuardianRejectionCircuitBreakerAction::InterruptTurn {
                consecutive_denials: turn.consecutive_denials,
                recent_denials,
            }
        } else {
            GuardianRejectionCircuitBreakerAction::Continue
        }
    }

    // 记录一次「非拒绝」（放行 / 中止 / 超时等）：清零连续计数，但仍计入窗口，
    // 使「最近窗口」反映真实的近期审查序列。
    pub(crate) fn record_non_denial(&mut self, turn_id: &str) {
        let turn = self.turns.entry(turn_id.to_string()).or_default();
        turn.consecutive_denials = 0;
        Self::record_recent_review(turn, /*denied*/ false);
    }

    // 把本次审查结果（是否被拒）压入滑动窗口，超出窗口大小则丢弃最旧一条。
    fn record_recent_review(turn: &mut GuardianRejectionCircuitBreakerTurn, denied: bool) {
        turn.recent_denials.push_back(denied);
        if turn.recent_denials.len() > AUTO_REVIEW_DENIAL_WINDOW_SIZE {
            turn.recent_denials.pop_front();
        }
    }
}

#[cfg(test)]
use approval_request::format_guardian_action_pretty;
#[cfg(test)]
use approval_request::guardian_assessment_action;
#[cfg(test)]
use approval_request::guardian_request_turn_id;
#[cfg(test)]
use prompt::GuardianPromptMode;
#[cfg(test)]
use prompt::GuardianTranscriptCursor;
#[cfg(test)]
use prompt::GuardianTranscriptEntry;
#[cfg(test)]
use prompt::GuardianTranscriptEntryKind;
#[cfg(test)]
use prompt::build_guardian_prompt_items;
#[cfg(test)]
use prompt::collect_guardian_transcript_entries;
#[cfg(test)]
use prompt::guardian_output_schema;
#[cfg(test)]
pub(crate) use prompt::guardian_policy_prompt;
#[cfg(test)]
pub(crate) use prompt::guardian_policy_prompt_with_config;
#[cfg(test)]
use prompt::guardian_truncate_text;
#[cfg(test)]
use prompt::parse_guardian_assessment;
#[cfg(test)]
use prompt::render_guardian_transcript_entries;
#[cfg(test)]
use review::GuardianReviewOutcome;
#[cfg(test)]
use review::run_guardian_review_session as run_guardian_review_session_for_test;
#[cfg(test)]
use review_session::build_guardian_review_session_config as build_guardian_review_session_config_for_test;

#[cfg(test)]
mod tests;
