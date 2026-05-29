//! 【文件职责】Guardian 审查的主流程与对外入口：把一个审批请求送进受限审查
//! 子会话裁决，将结果映射为 `ReviewDecision`，并负责埋点、警告事件、拒绝原因
//! 暂存与「拒绝熔断」。
//!
//! 【架构位置】
//!   层级：Agent 核心层 · 审批旁路
//!   上游：审批处理处经 `routes_approval_to_guardian()` 判断后调用
//!         `review_approval_request*()` / `spawn_approval_request_review()`
//!   下游：`run_guardian_review_session()` 委托 review_session.rs 跑子会话；
//!         prompt.rs 解析裁决；mod.rs 的熔断器与常量
//!
//! 【失败即拒】见 `run_guardian_review` 的契约注释——超时 / 会话失败 / 解析失败
//!   一律阻断执行，但超时单独以 `ReviewDecision::TimedOut` 回传。
//!
//! 【阅读建议】先读对外入口 `review_approval_request()`，再读核心状态机
//!   `run_guardian_review()`（事件发射 + 结果分支 + 熔断记录），
//!   `run_guardian_review_session()` 负责挑模型/构 config 后委托子会话。

use codex_analytics::GuardianApprovalRequestSource;
use codex_analytics::GuardianReviewAnalyticsResult;
use codex_analytics::GuardianReviewDecision;
use codex_analytics::GuardianReviewFailureReason;
use codex_analytics::GuardianReviewTerminalStatus;
use codex_analytics::GuardianReviewTrackContext;
use codex_analytics::GuardianReviewedAction;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentDecisionSource;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::GuardianRiskLevel;
use codex_protocol::protocol::GuardianUserAuthorization;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::turn_timing::now_unix_timestamp_ms;

use super::AUTO_REVIEW_DENIAL_WINDOW_SIZE;
use super::GUARDIAN_REVIEW_TIMEOUT;
use super::GUARDIAN_REVIEWER_NAME;
use super::GuardianApprovalRequest;
use super::GuardianAssessment;
use super::GuardianAssessmentOutcome;
use super::GuardianRejection;
use super::GuardianRejectionCircuitBreakerAction;
use super::approval_request::guardian_assessment_action;
use super::approval_request::guardian_request_target_item_id;
use super::approval_request::guardian_request_turn_id;
use super::approval_request::guardian_reviewed_action;
use super::metrics::emit_guardian_review_metrics;
use super::prompt::guardian_output_schema;
use super::prompt::parse_guardian_assessment;
use super::review_session::GuardianReviewSessionOutcome;
use super::review_session::GuardianReviewSessionParams;
use super::review_session::build_guardian_review_session_config;

// 被拒后回灌给主 Agent 的行为约束文案：明确禁止「绕路 / 间接执行 / 规避策略」
// 去达成同一被拒目标，只能改用实质更安全的方案或在告知风险后取得用户显式批准。
// 与拒绝熔断配合，防止模型把一次拒绝当成「换个写法重试」的信号。
const GUARDIAN_REJECTION_INSTRUCTIONS: &str = concat!(
    "The agent must not attempt to achieve the same outcome via workaround, ",
    "indirect execution, or policy circumvention. ",
    "Proceed only with a materially safer alternative, ",
    "or if the user explicitly approves the action after being informed of the risk. ",
    "Otherwise, stop and request user input.",
);

// 超时回灌文案：强调「超时 ≠ 不安全」，避免主 Agent 据此误判动作危险；
// 允许重试一次或转而征询用户。对应 `ReviewDecision::TimedOut` 分支。
const GUARDIAN_TIMEOUT_INSTRUCTIONS: &str = concat!(
    "The automatic permission approval review did not finish before its deadline. ",
    "Do not assume the action is unsafe based on the timeout alone. ",
    "You may retry once, or ask the user for guidance or explicit approval.",
);

// 为一次审查生成全局唯一 id，用于关联审查事件、暂存拒绝原因等。
pub(crate) fn new_guardian_review_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 取出（并从暂存 map 中移除）指定审查的拒绝原因，拼成回灌给主 Agent 的
/// 提示文本。若找不到或理由为空，则用兜底文案；末尾附上行为约束指令。
/// 副作用：会 `remove` 掉 `guardian_rejections` 中该 review_id 的记录（消费一次）。
pub(crate) async fn guardian_rejection_message(session: &Session, review_id: &str) -> String {
    let rejection = session
        .services
        .guardian_rejections
        .lock()
        .await
        .remove(review_id)
        .filter(|rejection| !rejection.rationale.trim().is_empty())
        .unwrap_or_else(|| GuardianRejection {
            rationale: "Auto-reviewer denied the action without a specific rationale.".to_string(),
            source: GuardianAssessmentDecisionSource::Agent,
        });
    match rejection.source {
        GuardianAssessmentDecisionSource::Agent => format!(
            "This action was rejected due to unacceptable risk.\nReason: {}\n{}",
            rejection.rationale.trim(),
            GUARDIAN_REJECTION_INSTRUCTIONS
        ),
    }
}

// 超时时回灌给主 Agent 的固定文案（见上面的 INSTRUCTIONS 常量）。
pub(crate) fn guardian_timeout_message() -> String {
    GUARDIAN_TIMEOUT_INSTRUCTIONS.to_string()
}

/// 一次审查的内部结果：要么拿到完整裁决，要么是各类错误。
/// 注意这是「子会话执行层」的结果，再由 `run_guardian_review` 翻译成对外的
/// `ReviewDecision`（含 fail-closed 逻辑）。
#[derive(Debug)]
pub(super) enum GuardianReviewOutcome {
    Completed(GuardianAssessment),
    Error(GuardianReviewError),
}

/// 审查失败的细分原因。区分这些 case 是为了：① 埋点统计 failure_reason；
/// ② 让 Timeout / Cancelled 走与「真实失败」不同的对外状态（超时不视作拒绝、
/// 中止回传 Abort）。其余三类（构造提示/会话/解析失败）统一 fail closed 为拒绝。
#[derive(Debug)]
pub(super) enum GuardianReviewError {
    PromptBuild { message: String },
    Session { message: String },
    Parse { message: String },
    Timeout,
    Cancelled,
}

impl GuardianReviewError {
    fn prompt_build(err: anyhow::Error) -> Self {
        Self::PromptBuild {
            message: err.to_string(),
        }
    }

    fn session(err: anyhow::Error) -> Self {
        Self::Session {
            message: err.to_string(),
        }
    }

    fn parse(err: anyhow::Error) -> Self {
        Self::Parse {
            message: err.to_string(),
        }
    }

    fn failure_reason(&self) -> GuardianReviewFailureReason {
        match self {
            Self::PromptBuild { .. } => GuardianReviewFailureReason::PromptBuildError,
            Self::Session { .. } => GuardianReviewFailureReason::SessionError,
            Self::Parse { .. } => GuardianReviewFailureReason::ParseError,
            Self::Timeout => GuardianReviewFailureReason::Timeout,
            Self::Cancelled => GuardianReviewFailureReason::Cancelled,
        }
    }
}

fn guardian_risk_level_str(level: GuardianRiskLevel) -> &'static str {
    match level {
        GuardianRiskLevel::Low => "low",
        GuardianRiskLevel::Medium => "medium",
        GuardianRiskLevel::High => "high",
        GuardianRiskLevel::Critical => "critical",
    }
}

/// Whether this turn should route allowed approval prompts through the guardian
/// reviewer instead of surfacing them to the user. ARC may still block actions
/// earlier in the flow.
/// 判定本轮是否应把审批交给 Guardian 自动裁决（而非弹给用户）：仅当审批策略为
/// `OnRequest` 或 `Granular`，且配置选定了 `AutoReview` 审查器时才走 Guardian。
/// 注：ARC（access/approval rules）可能在更早阶段就拦掉动作，这里只决定「放行后
/// 是否改走自动审查」这一岔路。
pub(crate) fn routes_approval_to_guardian(turn: &TurnContext) -> bool {
    matches!(
        turn.approval_policy.value(),
        AskForApproval::OnRequest | AskForApproval::Granular(_)
    ) && turn.config.approvals_reviewer == ApprovalsReviewer::AutoReview
}

// 判断某个会话来源是否就是 Guardian 审查子会话（SubAgent 名为 "guardian"）。
// 用于在通用会话逻辑中识别并特殊处理 Guardian 自身的会话。
pub(crate) fn is_guardian_reviewer_source(
    session_source: &codex_protocol::protocol::SessionSource,
) -> bool {
    matches!(
        session_source,
        codex_protocol::protocol::SessionSource::SubAgent(SubAgentSource::Other(name))
            if name == GUARDIAN_REVIEWER_NAME
    )
}

fn track_guardian_review(
    session: &Session,
    tracking: &GuardianReviewTrackContext,
    approval_request_source: GuardianApprovalRequestSource,
    reviewed_action: &GuardianReviewedAction,
    result: GuardianReviewAnalyticsResult,
    completed_at_ms: u64,
) {
    emit_guardian_review_metrics(
        &session.services.session_telemetry,
        &result,
        approval_request_source,
        reviewed_action,
        completed_at_ms.saturating_sub(tracking.started_at_ms),
    );
    session
        .services
        .analytics_events_client
        .track_guardian_review(tracking, result, completed_at_ms);
}

// 向熔断器记录一次「非拒绝」结果（放行/中止/超时等），清零连续拒绝计数。
async fn record_guardian_non_denial(session: &Arc<Session>, turn_id: &str) {
    session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await
        .record_non_denial(turn_id);
}

/// 记录一次拒绝；若熔断器判定需中断本轮，则发一条用户可见的 GuardianWarning，
/// 并异步中断该轮（`abort_turn_if_active`）。
/// 副作用：发事件 + 可能 spawn 一个中断任务。
/// 防御：若该轮的 turn_context 已不存在（轮已结束），则不再发警告 / 中断。
async fn record_guardian_denial(session: &Arc<Session>, turn: &Arc<TurnContext>, turn_id: &str) {
    let action = session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await
        .record_denial(turn_id);
    let GuardianRejectionCircuitBreakerAction::InterruptTurn {
        consecutive_denials,
        recent_denials,
    } = action
    else {
        return;
    };

    if session.turn_context_for_sub_id(turn_id).await.is_none() {
        return;
    }

    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianWarning(WarningEvent {
                message: format!(
                    "Automatic approval review rejected too many approval requests for this turn ({consecutive_denials} consecutive, {recent_denials} in the last {AUTO_REVIEW_DENIAL_WINDOW_SIZE} reviews); interrupting the turn."
                ),
            }),
        )
        .await;

    let runtime_handle = session.services.runtime_handle.clone();
    let session = Arc::clone(session);
    let turn_id = turn_id.to_string();
    let _abort_task = runtime_handle.spawn(async move {
        session
            .abort_turn_if_active(&turn_id, TurnAbortReason::Interrupted)
            .await;
    });
}

#[cfg(test)]
pub(crate) async fn record_guardian_denial_for_test(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    turn_id: &str,
) {
    record_guardian_denial(session, turn, turn_id).await;
}

/// This function always fails closed: timeouts, review-session failures, and
/// parse failures all block execution, but timeouts are still surfaced to the
/// caller as distinct from explicit guardian denials.
/// 审查主状态机：构造审查上下文 → 发「进行中」事件 → 跑子会话拿裁决 →
/// 按结果发「完成/超时/中止」事件、记埋点、写熔断器，最终返回 `ReviewDecision`。
/// 失败即拒：任何错误都阻断执行；但「超时」回传 `TimedOut`、「中止」回传 `Abort`，
/// 与模型明确 Deny 区分开。
/// 副作用：多次 `send_event`、写 `guardian_rejections` 暂存、更新熔断器。
async fn run_guardian_review(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    external_cancel: Option<CancellationToken>,
) -> ReviewDecision {
    // Step 1：从请求中抽取追踪所需的标识与动作摘要，建立审查追踪上下文。
    let target_item_id = guardian_request_target_item_id(&request).map(str::to_string);
    let assessment_turn_id = guardian_request_turn_id(&request, &turn.sub_id).to_string();
    let action_summary = guardian_assessment_action(&request);
    let reviewed_action = guardian_reviewed_action(&request);
    let review_tracking = GuardianReviewTrackContext::new(
        session.conversation_id.to_string(),
        assessment_turn_id.clone(),
        review_id.clone(),
        target_item_id.clone(),
        approval_request_source,
        reviewed_action.clone(),
        GUARDIAN_REVIEW_TIMEOUT.as_millis() as u64,
    );
    let started_at_ms = review_tracking.started_at_ms.try_into().unwrap_or_default();
    // Step 2：先广播一条「审查进行中」事件，让 UI 立刻显示审查已开始。
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                id: review_id.clone(),
                target_item_id: target_item_id.clone(),
                turn_id: assessment_turn_id.clone(),
                started_at_ms,
                completed_at_ms: None,
                status: GuardianAssessmentStatus::InProgress,
                risk_level: None,
                user_authorization: None,
                rationale: None,
                decision_source: None,
                action: action_summary.clone(),
            }),
        )
        .await;

    // Step 3：若外部已取消，直接短路为 Abort——不必再花时间拉起子会话。
    // 同样要补发终态事件、记埋点、并按「非拒绝」更新熔断器。
    if external_cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        let completed_at_ms = now_unix_timestamp_ms();
        track_guardian_review(
            session.as_ref(),
            &review_tracking,
            approval_request_source,
            &reviewed_action,
            GuardianReviewAnalyticsResult {
                decision: GuardianReviewDecision::Aborted,
                terminal_status: GuardianReviewTerminalStatus::Aborted,
                failure_reason: Some(GuardianReviewFailureReason::Cancelled),
                ..GuardianReviewAnalyticsResult::without_session()
            },
            completed_at_ms.try_into().unwrap_or_default(),
        );
        session
            .send_event(
                turn.as_ref(),
                EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                    id: review_id,
                    target_item_id,
                    turn_id: assessment_turn_id.clone(),
                    started_at_ms,
                    completed_at_ms: Some(completed_at_ms),
                    status: GuardianAssessmentStatus::Aborted,
                    risk_level: None,
                    user_authorization: None,
                    rationale: None,
                    decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                    action: action_summary,
                }),
            )
            .await;
        record_guardian_non_denial(&session, &assessment_turn_id).await;
        return ReviewDecision::Abort;
    }

    let schema = guardian_output_schema();
    let terminal_action = action_summary.clone();
    // Step 4：拉起受限审查子会话跑模型，拿到内部结果与埋点数据。
    // Box::pin 是因为该 future 体积很大（内含整套子会话状态机）。
    let (outcome, analytics_result) = Box::pin(run_guardian_review_session(
        session.clone(),
        turn.clone(),
        request,
        retry_reason.clone(),
        schema,
        external_cancel,
    ))
    .await;

    let completed_at_ms = now_unix_timestamp_ms();
    // Step 5：把子会话结果翻译成 (裁决, 是否计入熔断拒绝)。
    // 超时 / 中止在此直接 return 对应的对外状态；三类「真失败」则 fail-closed
    // 合成一个 Deny 裁决（但不计入熔断，因为不是模型的主动拒绝）继续往下走。
    let (assessment, count_denial_for_circuit_breaker) = match outcome {
        GuardianReviewOutcome::Completed(assessment) => {
            let approved = matches!(assessment.outcome, GuardianAssessmentOutcome::Allow);
            track_guardian_review(
                session.as_ref(),
                &review_tracking,
                approval_request_source,
                &reviewed_action,
                GuardianReviewAnalyticsResult {
                    decision: if approved {
                        GuardianReviewDecision::Approved
                    } else {
                        GuardianReviewDecision::Denied
                    },
                    terminal_status: if approved {
                        GuardianReviewTerminalStatus::Approved
                    } else {
                        GuardianReviewTerminalStatus::Denied
                    },
                    failure_reason: None,
                    risk_level: Some(assessment.risk_level),
                    user_authorization: Some(assessment.user_authorization),
                    outcome: Some(assessment.outcome),
                    ..analytics_result
                },
                completed_at_ms.try_into().unwrap_or_default(),
            );
            let count_denial_for_circuit_breaker =
                matches!(assessment.outcome, GuardianAssessmentOutcome::Deny);
            (assessment, count_denial_for_circuit_breaker)
        }
        GuardianReviewOutcome::Error(error) => match error {
            GuardianReviewError::Timeout => {
                let rationale =
                    "Automatic approval review timed out while evaluating the requested approval."
                        .to_string();
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Denied,
                        terminal_status: GuardianReviewTerminalStatus::TimedOut,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianWarning(WarningEvent {
                            message: rationale.clone(),
                        }),
                    )
                    .await;
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                            id: review_id,
                            target_item_id,
                            turn_id: assessment_turn_id.clone(),
                            started_at_ms,
                            completed_at_ms: Some(completed_at_ms),
                            status: GuardianAssessmentStatus::TimedOut,
                            risk_level: None,
                            user_authorization: None,
                            rationale: Some(rationale),
                            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                            action: terminal_action,
                        }),
                    )
                    .await;
                record_guardian_non_denial(&session, &assessment_turn_id).await;
                return ReviewDecision::TimedOut;
            }
            GuardianReviewError::Cancelled => {
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Aborted,
                        terminal_status: GuardianReviewTerminalStatus::Aborted,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                session
                    .send_event(
                        turn.as_ref(),
                        EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                            id: review_id,
                            target_item_id,
                            turn_id: assessment_turn_id.clone(),
                            started_at_ms,
                            completed_at_ms: Some(completed_at_ms),
                            status: GuardianAssessmentStatus::Aborted,
                            risk_level: None,
                            user_authorization: None,
                            rationale: None,
                            decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                            action: action_summary,
                        }),
                    )
                    .await;
                record_guardian_non_denial(&session, &assessment_turn_id).await;
                return ReviewDecision::Abort;
            }
            GuardianReviewError::PromptBuild { .. }
            | GuardianReviewError::Session { .. }
            | GuardianReviewError::Parse { .. } => {
                let message = match &error {
                    GuardianReviewError::PromptBuild { message }
                    | GuardianReviewError::Session { message }
                    | GuardianReviewError::Parse { message } => message,
                    GuardianReviewError::Timeout | GuardianReviewError::Cancelled => {
                        "guardian review failed"
                    }
                };
                let rationale = format!("Automatic approval review failed: {message}");
                track_guardian_review(
                    session.as_ref(),
                    &review_tracking,
                    approval_request_source,
                    &reviewed_action,
                    GuardianReviewAnalyticsResult {
                        decision: GuardianReviewDecision::Denied,
                        terminal_status: GuardianReviewTerminalStatus::FailedClosed,
                        failure_reason: Some(error.failure_reason()),
                        ..analytics_result
                    },
                    completed_at_ms.try_into().unwrap_or_default(),
                );
                (
                    GuardianAssessment {
                        risk_level: GuardianRiskLevel::High,
                        user_authorization: GuardianUserAuthorization::Unknown,
                        outcome: GuardianAssessmentOutcome::Deny,
                        rationale,
                    },
                    false,
                )
            }
        },
    };

    let approved = match assessment.outcome {
        GuardianAssessmentOutcome::Allow => true,
        GuardianAssessmentOutcome::Deny => false,
    };
    let verdict = if approved { "approved" } else { "denied" };
    let user_authorization = match assessment.user_authorization {
        GuardianUserAuthorization::Unknown => "unknown",
        GuardianUserAuthorization::Low => "low",
        GuardianUserAuthorization::Medium => "medium",
        GuardianUserAuthorization::High => "high",
    };
    let warning = format!(
        "Automatic approval review {verdict} (risk: {}, authorization: {user_authorization}): {}",
        guardian_risk_level_str(assessment.risk_level),
        assessment.rationale
    );
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianWarning(WarningEvent { message: warning }),
        )
        .await;
    let status = if approved {
        GuardianAssessmentStatus::Approved
    } else {
        GuardianAssessmentStatus::Denied
    };
    // Step 6：暂存/清理拒绝原因。放行则清掉残留记录；拒绝则按 review_id 存入，
    // 供主 Agent 后续通过 `guardian_rejection_message()` 取用并消费。
    {
        let mut rationales = session.services.guardian_rejections.lock().await;
        if approved {
            rationales.remove(&review_id);
        } else {
            let rejection = GuardianRejection {
                rationale: assessment.rationale.clone(),
                source: GuardianAssessmentDecisionSource::Agent,
            };
            rationales.insert(review_id.clone(), rejection);
        }
    }
    session
        .send_event(
            turn.as_ref(),
            EventMsg::GuardianAssessment(GuardianAssessmentEvent {
                id: review_id,
                target_item_id,
                turn_id: assessment_turn_id.clone(),
                started_at_ms,
                completed_at_ms: Some(completed_at_ms),
                status,
                risk_level: Some(assessment.risk_level),
                user_authorization: Some(assessment.user_authorization),
                rationale: Some(assessment.rationale.clone()),
                decision_source: Some(GuardianAssessmentDecisionSource::Agent),
                action: terminal_action,
            }),
        )
        .await;

    // Step 7：更新熔断器。仅「模型主动 Deny」计入拒绝（可能触发中断本轮）；
    // fail-closed 合成的拒绝按非拒绝处理，避免审查器自身故障误触发熔断。
    if count_denial_for_circuit_breaker {
        record_guardian_denial(&session, &turn, &assessment_turn_id).await;
    } else {
        record_guardian_non_denial(&session, &assessment_turn_id).await;
    }

    if approved {
        ReviewDecision::Approved
    } else {
        ReviewDecision::Denied
    }
}

/// Public entrypoint for approval requests that should be reviewed by guardian.
/// 主轮（MainTurn）审批的标准入口：以「无外部取消」方式跑一次审查并返回裁决。
pub(crate) async fn review_approval_request(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
) -> ReviewDecision {
    // Box the delegated review future so callers do not inline the entire
    // guardian session state machine into their own async stack.
    Box::pin(run_guardian_review(
        Arc::clone(session),
        Arc::clone(turn),
        review_id,
        request,
        retry_reason,
        GuardianApprovalRequestSource::MainTurn,
        /*external_cancel*/ None,
    ))
    .await
}

// 带外部取消令牌 + 可指定来源（如委派子 Agent）的入口，供需要中途取消的调用方使用。
pub(crate) async fn review_approval_request_with_cancel(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    cancel_token: CancellationToken,
) -> ReviewDecision {
    run_guardian_review(
        Arc::clone(session),
        Arc::clone(turn),
        review_id,
        request,
        retry_reason,
        approval_request_source,
        Some(cancel_token),
    )
    .await
}

/// 在「独立 OS 线程 + 独立 current-thread runtime」上跑审查，立即返回一个
/// oneshot 接收端供调用方异步等待裁决。
/// 为何另起线程而非 `tokio::spawn`：调用点可能身处不同的 runtime 上下文，
/// 用专属 runtime 隔离子会话的事件循环，避免与调用方的执行器相互阻塞。
/// 失败即拒：若连 runtime 都建不起来，直接回传 `Denied`。
pub(crate) fn spawn_approval_request_review(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    review_id: String,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    approval_request_source: GuardianApprovalRequestSource,
    cancel_token: CancellationToken,
) -> oneshot::Receiver<ReviewDecision> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = tx.send(ReviewDecision::Denied);
            return;
        };
        let decision = runtime.block_on(review_approval_request_with_cancel(
            &session,
            &turn,
            review_id,
            request,
            retry_reason,
            approval_request_source,
            cancel_token,
        ));
        let _ = tx.send(decision);
    });
    rx
}

/// Runs the guardian in a locked-down reusable review session.
///
/// The guardian itself should not mutate state or trigger further approvals, so
/// it is pinned to a read-only sandbox with `approval_policy = never` and
/// nonessential agent features disabled. When the cached trunk session is idle,
/// later approvals append onto that same guardian conversation to preserve a
/// stable prompt-cache key. If the trunk is already busy, the review runs in an
/// ephemeral fork from the last committed trunk rollout so parallel approvals
/// do not block each other or mutate the cached thread. The trunk is recreated
/// when the effective review-session config changes, and any future compaction
/// must continue to preserve the guardian policy as exact top-level developer
/// context. It may still reuse the parent's managed-network allowlist for
/// read-only checks, but it intentionally runs without inherited exec-policy
/// rules.
/// 在「上锁、可复用」的审查子会话中跑 Guardian。
/// 关键约束（也是安全设计核心）：
///   - 子会话锁死为只读沙箱、`approval_policy = never`、关闭非必要 Agent 特性，
///     确保审查器本身不会改状态、也不会再触发新的审批（防递归）。
///   - 复用策略：主干（trunk）空闲时后续审批续在同一会话，保持 prompt-cache key
///     稳定（省 token）；主干忙时则从「最后提交的 rollout」临时 fork 一个一次性
///     会话，让并行审批互不阻塞、也不污染缓存线程。
///   - 主干会在「有效审查配置变化」时重建；未来若做压缩，必须把 Guardian 策略
///     原样保留为顶层 developer 上下文。
///   - 网络：可复用父会话的受管网络白名单做只读检查，但故意不继承 exec-policy。
/// 本函数负责：挑选审查模型与推理强度 → 构造受限 config → 委托
/// `GuardianReviewSessionManager::run_review` 跑会话 → 把会话产物解析为
/// `GuardianReviewOutcome`。
pub(super) async fn run_guardian_review_session(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    request: GuardianApprovalRequest,
    retry_reason: Option<String>,
    schema: serde_json::Value,
    external_cancel: Option<CancellationToken>,
) -> (GuardianReviewOutcome, GuardianReviewAnalyticsResult) {
    let network_proxy = session.services.network_proxy.load_full();
    let live_network_config = match network_proxy.as_ref() {
        Some(network_proxy) => match network_proxy.proxy().current_cfg().await {
            Ok(config) => Some(config),
            Err(err) => {
                return (
                    GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(err)),
                    GuardianReviewAnalyticsResult::without_session(),
                );
            }
        },
        None => None,
    };
    let available_models = session
        .services
        .models_manager
        .list_models(codex_models_manager::manager::RefreshStrategy::Offline)
        .await;
    let preferred_reasoning_effort = |supports_low: bool, fallback| {
        if supports_low {
            Some(codex_protocol::openai_models::ReasoningEffort::Low)
        } else {
            fallback
        }
    };
    // 选审查模型：优先用 provider 指定的「审批审查专用模型」；找不到则回退到
    // 当前轮所用模型。推理强度尽量取 Low（审查任务相对轻量、追求快），不支持
    // 时再退回模型默认值。
    let preferred_model_id = turn.provider.approval_review_preferred_model();
    let preferred_model = available_models
        .iter()
        .find(|preset| preset.model == preferred_model_id);
    let (guardian_model, guardian_reasoning_effort) = if let Some(preset) = preferred_model {
        let reasoning_effort = preferred_reasoning_effort(
            preset
                .supported_reasoning_efforts
                .iter()
                .any(|effort| effort.effort == codex_protocol::openai_models::ReasoningEffort::Low),
            Some(preset.default_reasoning_effort),
        );
        (preferred_model_id.to_string(), reasoning_effort)
    } else {
        let reasoning_effort = preferred_reasoning_effort(
            turn.model_info
                .supported_reasoning_levels
                .iter()
                .any(|preset| preset.effort == codex_protocol::openai_models::ReasoningEffort::Low),
            turn.reasoning_effort
                .or(turn.model_info.default_reasoning_level),
        );
        (turn.model_info.slug.clone(), reasoning_effort)
    };
    let guardian_config = build_guardian_review_session_config(
        turn.config.as_ref(),
        live_network_config.clone(),
        guardian_model.as_str(),
        guardian_reasoning_effort,
    );
    let guardian_config = match guardian_config {
        Ok(config) => config,
        Err(err) => {
            return (
                GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(err)),
                GuardianReviewAnalyticsResult::without_session(),
            );
        }
    };

    let (session_outcome, session_analytics_result) = Box::pin(
        session
            .guardian_review_session
            .run_review(GuardianReviewSessionParams {
                parent_session: Arc::clone(&session),
                parent_turn: turn.clone(),
                spawn_config: guardian_config,
                request,
                retry_reason,
                schema,
                model: guardian_model,
                reasoning_effort: guardian_reasoning_effort,
                reasoning_summary: turn.reasoning_summary,
                personality: turn.personality,
                external_cancel,
            }),
    )
    .await;

    // 把子会话产物归一为 `GuardianReviewOutcome`：成功完成则解析最后一条
    // assistant 消息为裁决 JSON；缺消息 / 解析失败 / 各类会话错误分别映射为
    // 对应的 `GuardianReviewError`，交由上层 fail-closed 处理。
    match session_outcome {
        GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) => match last_agent_message
        {
            Some(last_agent_message) => {
                match parse_guardian_assessment(Some(&last_agent_message)) {
                    Ok(assessment) => (
                        GuardianReviewOutcome::Completed(assessment),
                        session_analytics_result,
                    ),
                    Err(err) => (
                        GuardianReviewOutcome::Error(GuardianReviewError::parse(err)),
                        session_analytics_result,
                    ),
                }
            }
            None => (
                GuardianReviewOutcome::Error(GuardianReviewError::session(anyhow::anyhow!(
                    "guardian review completed without an assessment payload"
                ))),
                session_analytics_result,
            ),
        },
        GuardianReviewSessionOutcome::Completed(Err(err)) => (
            GuardianReviewOutcome::Error(GuardianReviewError::session(err)),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::PromptBuildFailed(err) => (
            GuardianReviewOutcome::Error(GuardianReviewError::prompt_build(err)),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::SessionFailed(err) => (
            GuardianReviewOutcome::Error(GuardianReviewError::session(err)),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::TimedOut => (
            GuardianReviewOutcome::Error(GuardianReviewError::Timeout),
            session_analytics_result,
        ),
        GuardianReviewSessionOutcome::Aborted => (
            GuardianReviewOutcome::Error(GuardianReviewError::Cancelled),
            session_analytics_result,
        ),
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn guardian_review_error_reason_distinguishes_error_kinds() {
        let parse_error = GuardianReviewError::parse(anyhow::anyhow!("bad guardian JSON"));
        let prompt_error = GuardianReviewError::prompt_build(anyhow::anyhow!("bad prompt/config"));
        let session_error =
            GuardianReviewError::session(anyhow::anyhow!("guardian runtime failed"));

        assert!(matches!(
            parse_error.failure_reason(),
            GuardianReviewFailureReason::ParseError
        ));
        assert!(matches!(
            prompt_error.failure_reason(),
            GuardianReviewFailureReason::PromptBuildError
        ));
        assert!(matches!(
            session_error.failure_reason(),
            GuardianReviewFailureReason::SessionError
        ));
    }
}
