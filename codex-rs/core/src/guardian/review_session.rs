//! 【文件职责】管理 Guardian 审查子会话的「生命周期与复用」：维护一个可复用的
//! 主干（trunk）会话，必要时派生一次性的 ephemeral fork 会话，并在每次审查里
//! 受 deadline / 取消令牌约束地提交一轮、等待结果、推进游标。
//!
//! 【架构位置】
//!   层级：Agent 核心层 · 审批旁路（被 review.rs 委托）
//!   上游：review.rs 的 `run_guardian_review_session()` 调用 `run_review()`
//!   下游：codex_delegate 拉起受限子会话；prompt.rs 构造提示；context 注入提醒
//!
//! 【复用模型（关键）】
//!   - trunk：缓存的长驻审查会话，空闲时后续审查续在其上，保持 prompt-cache key
//!     稳定以省 token；其有效配置由 `GuardianReviewSessionReuseKey` 标识，配置变
//!     化即作废重建。
//!   - ephemeral fork：trunk 忙 / 配置不匹配时，从 trunk「最后提交的 rollout」
//!     fork 出一次性会话，跑完即销毁，使并行审查互不阻塞、不污染缓存线程。
//!   - 增量提示：trunk 复用时只把「自上次审查以来新增的转写稿」作为 Delta 发给
//!     模型（靠 `GuardianTranscriptCursor` 定位），首次则发 Full。
//!
//! 【阅读建议】先看状态容器 `GuardianReviewSessionManager` / `*State` /
//!   `GuardianReviewSession`，再看核心调度 `run_review()`（选 trunk 还是 fork），
//!   随后 `run_review_on_session()`（构提示→提交→等结果→推进游标）与
//!   `wait_for_guardian_review()`（带超时/取消的事件循环）。
//!   `build_guardian_review_session_config()` 是「把父配置锁成只读审查配置」的关键。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use codex_analytics::GuardianReviewAnalyticsResult;
use codex_analytics::GuardianReviewSessionKind;
use codex_protocol::ThreadId;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::codex_delegate::run_codex_thread_interactive;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ManagedFeatures;
use crate::config::NetworkProxySpec;
use crate::config::Permissions;
use crate::context::ContextualUserFragment;
use crate::context::GuardianFollowupReviewReminder;
use crate::session::Codex;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_config::types::McpServerConfig;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::GUARDIAN_REVIEW_TIMEOUT;
use super::GUARDIAN_REVIEWER_NAME;
use super::GuardianApprovalRequest;
use super::prompt::GuardianPromptMode;
use super::prompt::GuardianTranscriptCursor;
use super::prompt::build_guardian_prompt_items;
use super::prompt::guardian_policy_prompt;
use super::prompt::guardian_policy_prompt_with_config;

// 审查超时/取消后向子会话发 Interrupt 再「排空」其事件的等待上限。
// 给 5s 让子会话把 TurnAborted/TurnComplete 吐出来，以便安全复用该会话；
// 排空失败则放弃复用、销毁会话（见 `interrupt_and_drain_turn` 调用处）。
const GUARDIAN_INTERRUPT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 审查子会话一次运行的结果。`Completed(Ok(Some(msg)))` 表示拿到模型最终消息
/// （待 review.rs 解析为裁决）；其余为各类失败/超时/中止，供上层 fail-closed。
#[derive(Debug)]
pub(crate) enum GuardianReviewSessionOutcome {
    Completed(anyhow::Result<Option<String>>),
    PromptBuildFailed(anyhow::Error),
    SessionFailed(anyhow::Error),
    TimedOut,
    Aborted,
}

/// 发起一次审查所需的全部入参（父会话/父轮、受限 spawn 配置、待审请求、
/// 模型与推理设置、可选外部取消等），由 review.rs 组装后传入。
pub(crate) struct GuardianReviewSessionParams {
    pub(crate) parent_session: Arc<Session>,
    pub(crate) parent_turn: Arc<TurnContext>,
    pub(crate) spawn_config: Config,
    pub(crate) request: GuardianApprovalRequest,
    pub(crate) retry_reason: Option<String>,
    pub(crate) schema: Value,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) personality: Option<Personality>,
    pub(crate) external_cancel: Option<CancellationToken>,
}

/// 审查会话管理器：会话级单例，持有可复用的 trunk 与一组在飞的 ephemeral 会话。
/// [引用范围] 作为会话服务持有（见 session/services），跨多次审查共享。
#[derive(Default)]
pub(crate) struct GuardianReviewSessionManager {
    // 用 Mutex 串行化「选会话 + 派生 trunk」，保证复用判定与新建不竞态。
    state: Arc<Mutex<GuardianReviewSessionState>>,
}

#[derive(Default)]
struct GuardianReviewSessionState {
    // 长驻可复用的主干会话；为空表示尚未建立或已作废。
    trunk: Option<Arc<GuardianReviewSession>>,
    // 当前在飞的一次性 fork 会话，跑完即从此处移除并销毁。
    ephemeral_reviews: Vec<Arc<GuardianReviewSession>>,
}

/// 一个具体的审查子会话实例（trunk 或 ephemeral 通用）。
struct GuardianReviewSession {
    codex: Codex,
    cancel_token: CancellationToken,
    // 标识本会话由哪份「有效配置」派生；与新请求不一致即视为不可复用。
    reuse_key: GuardianReviewSessionReuseKey,
    // 单许可信号量：保证同一会话同一时刻只跑一轮审查（trunk 复用时尤为关键）。
    review_lock: Semaphore,
    state: Mutex<GuardianReviewState>,
}

/// 单个会话的可变状态：用于增量提示与 fork 续接。
struct GuardianReviewState {
    // 已在本会话上完成的审查次数；==0 时发 Full 提示，==1 时补发一次跟进提醒。
    prior_review_count: usize,
    // 上次审查已覆盖到的转写稿游标，用于下次只发 Delta（增量）。
    last_reviewed_transcript_cursor: Option<GuardianTranscriptCursor>,
    // trunk 最近一次「成功提交」后的 rollout 快照，供 ephemeral fork 续接历史。
    last_committed_fork_snapshot: Option<GuardianReviewForkSnapshot>,
}

// 是否「带着先前审查上下文」在跑：仅 Delta 模式为真，用于埋点维度。
fn had_prior_review_context(prompt_mode: &GuardianPromptMode) -> bool {
    matches!(prompt_mode, GuardianPromptMode::Delta { .. })
}

// 计算本次审查消耗的 token 增量（end - start），各项以 0 兜底防止出现负值，
// 因为 trunk 复用时统计的是「同一会话累计用量」的差值。
fn token_usage_delta(start: &TokenUsage, end: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: (end.input_tokens - start.input_tokens).max(0),
        cached_input_tokens: (end.cached_input_tokens - start.cached_input_tokens).max(0),
        output_tokens: (end.output_tokens - start.output_tokens).max(0),
        reasoning_output_tokens: (end.reasoning_output_tokens - start.reasoning_output_tokens)
            .max(0),
        total_tokens: (end.total_tokens - start.total_tokens).max(0),
    }
}

/// ephemeral 会话的「保险清理」哨兵：基于 RAII。若审查中途 panic/提前返回而
/// 没来得及正常注销该会话，`Drop` 会兜底把它从在飞列表移除并销毁，避免泄漏。
/// 正常路径下调用方会先 `disarm()` 解除，再走显式销毁。
struct EphemeralReviewCleanup {
    state: Arc<Mutex<GuardianReviewSessionState>>,
    review_session: Option<Arc<GuardianReviewSession>>,
}

/// trunk 最近一次成功提交后的快照，供 ephemeral fork 续接「同一段历史 + 游标」，
/// 从而让 fork 出的审查也能用增量提示而非从零开始。
#[derive(Clone)]
struct GuardianReviewForkSnapshot {
    initial_history: InitialHistory,
    prior_review_count: usize,
    last_reviewed_transcript_cursor: Option<GuardianTranscriptCursor>,
}

/// trunk 复用键：把「影响子会话行为」的配置子集打包，作为是否可复用的判据。
/// 任意一项变化即作废旧 trunk、重建新会话。
#[derive(Debug, Clone, PartialEq)]
struct GuardianReviewSessionReuseKey {
    // Only include settings that affect spawned-session behavior so reuse
    // invalidation remains explicit and does not depend on unrelated config
    // bookkeeping.
    // 只纳入「会改变子会话行为」的设置，使复用作废显式可控、不被无关配置干扰。
    model: Option<String>,
    model_provider_id: String,
    model_provider: ModelProviderInfo,
    model_context_window: Option<i64>,
    model_auto_compact_token_limit: Option<i64>,
    model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope,
    model_reasoning_effort: Option<ReasoningEffortConfig>,
    model_reasoning_summary: Option<ReasoningSummaryConfig>,
    permissions: Permissions,
    developer_instructions: Option<String>,
    base_instructions: Option<String>,
    user_instructions: Option<String>,
    compact_prompt: Option<String>,
    cwd: AbsolutePathBuf,
    mcp_servers: Constrained<HashMap<String, McpServerConfig>>,
    codex_linux_sandbox_exe: Option<PathBuf>,
    main_execve_wrapper_exe: Option<PathBuf>,
    zsh_path: Option<PathBuf>,
    features: ManagedFeatures,
    use_experimental_unified_exec_tool: bool,
}

impl GuardianReviewSessionReuseKey {
    fn from_spawn_config(spawn_config: &Config) -> Self {
        Self {
            model: spawn_config.model.clone(),
            model_provider_id: spawn_config.model_provider_id.clone(),
            model_provider: spawn_config.model_provider.clone(),
            model_context_window: spawn_config.model_context_window,
            model_auto_compact_token_limit: spawn_config.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: spawn_config.model_auto_compact_token_limit_scope,
            model_reasoning_effort: spawn_config.model_reasoning_effort,
            model_reasoning_summary: spawn_config.model_reasoning_summary,
            permissions: spawn_config.permissions.clone(),
            developer_instructions: spawn_config.developer_instructions.clone(),
            base_instructions: spawn_config.base_instructions.clone(),
            user_instructions: spawn_config.user_instructions.clone(),
            compact_prompt: spawn_config.compact_prompt.clone(),
            cwd: spawn_config.cwd.clone(),
            mcp_servers: spawn_config.mcp_servers.clone(),
            codex_linux_sandbox_exe: spawn_config.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: spawn_config.main_execve_wrapper_exe.clone(),
            zsh_path: spawn_config.zsh_path.clone(),
            features: spawn_config.features.clone(),
            use_experimental_unified_exec_tool: spawn_config.use_experimental_unified_exec_tool,
        }
    }
}

/// 为 Guardian 审查会话生成稳定的 prompt-cache key：`guardian:<父线程 id>`。
/// 把缓存键绑定到「父线程」而非每次审查，使同一父对话的多次审查命中同一缓存、
/// 显著省 token。非 Guardian 来源或缺父线程 id 时返回 None（不覆盖默认行为）。
/// （key 长度受 Responses API 限制，父线程 id 足够短以满足上限，见测试。）
pub(crate) fn prompt_cache_key_override_for_review_session(
    session_source: &SessionSource,
    parent_thread_id: Option<ThreadId>,
) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::Other(name)) = session_source else {
        return None;
    };
    if name != GUARDIAN_REVIEWER_NAME {
        return None;
    }
    let parent_thread_id = parent_thread_id?;
    Some(format!("guardian:{parent_thread_id}"))
}

impl GuardianReviewSession {
    // 取消令牌并等待子会话彻底关闭，释放底层资源。
    async fn shutdown(&self) {
        self.cancel_token.cancel();
        let _ = self.codex.shutdown_and_wait().await;
    }

    // 后台异步关闭：调用方不想阻塞等待时用（如替换 stale trunk）。
    fn shutdown_in_background(self: &Arc<Self>) {
        let review_session = Arc::clone(self);
        drop(tokio::spawn(async move {
            review_session.shutdown().await;
        }));
    }

    // 读取当前已提交的 fork 快照（trunk 忙时供 ephemeral fork 续接历史）。
    async fn fork_snapshot(&self) -> Option<GuardianReviewForkSnapshot> {
        self.state.lock().await.last_committed_fork_snapshot.clone()
    }

    /// 在一次审查成功提交后刷新 fork 快照：把当前 rollout 持久化后的条目连同
    /// 审查计数与游标存为快照，使后续 ephemeral fork 能从最新进度续接。
    /// 失败仅 warn 不致命——拿不到快照大不了下次 fork 走 Full 提示。
    async fn refresh_last_committed_fork_snapshot(&self) {
        match load_rollout_items_for_fork(&self.codex.session).await {
            Ok(Some(items)) if !items.is_empty() => {
                let mut state = self.state.lock().await;
                let prior_review_count = state.prior_review_count;
                let last_reviewed_transcript_cursor = state.last_reviewed_transcript_cursor;
                state.last_committed_fork_snapshot = Some(GuardianReviewForkSnapshot {
                    initial_history: InitialHistory::Forked(items),
                    prior_review_count,
                    last_reviewed_transcript_cursor,
                });
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(err) => {
                warn!("failed to refresh guardian trunk rollout snapshot: {err}");
            }
        }
    }
}

impl EphemeralReviewCleanup {
    fn new(
        state: Arc<Mutex<GuardianReviewSessionState>>,
        review_session: Arc<GuardianReviewSession>,
    ) -> Self {
        Self {
            state,
            review_session: Some(review_session),
        }
    }

    // 解除哨兵：正常完成路径下调用，表示由调用方自行负责销毁，Drop 不再兜底。
    fn disarm(&mut self) {
        self.review_session = None;
    }
}

impl Drop for EphemeralReviewCleanup {
    // 兜底清理：仅当未 disarm（即异常路径）时触发，后台把该会话从在飞列表移除
    // 并 shutdown，防止 panic/提前返回造成会话泄漏。
    fn drop(&mut self) {
        let Some(review_session) = self.review_session.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        drop(tokio::spawn(async move {
            let review_session = {
                let mut state = state.lock().await;
                state
                    .ephemeral_reviews
                    .iter()
                    .position(|active_review| Arc::ptr_eq(active_review, &review_session))
                    .map(|index| state.ephemeral_reviews.swap_remove(index))
            };
            if let Some(review_session) = review_session {
                review_session.shutdown().await;
            }
        }));
    }
}

impl GuardianReviewSessionManager {
    // 返回当前 trunk 会话的 rollout 文件路径（先确保已落盘）。无 trunk 或解析
    // 失败返回 None。
    pub(crate) async fn trunk_rollout_path(&self) -> Option<PathBuf> {
        let trunk = self.state.lock().await.trunk.clone()?;
        trunk.codex.session.ensure_rollout_materialized().await;
        match trunk.codex.session.current_rollout_path().await {
            Ok(path) => path,
            Err(err) => {
                warn!("failed to resolve guardian trunk rollout path: {err}");
                None
            }
        }
    }

    // 关停管理器：取出并销毁 trunk 与全部在飞 ephemeral 会话（会话/进程退出时调用）。
    pub(crate) async fn shutdown(&self) {
        let (review_session, ephemeral_reviews) = {
            let mut state = self.state.lock().await;
            (
                state.trunk.take(),
                std::mem::take(&mut state.ephemeral_reviews),
            )
        };
        if let Some(review_session) = review_session {
            review_session.shutdown().await;
        }
        for review_session in ephemeral_reviews {
            review_session.shutdown().await;
        }
    }

    /// 审查调度核心：决定本次审查跑在「复用的 trunk」上还是「新 fork 的
    /// ephemeral 会话」上，并施加统一的 deadline。
    /// 决策顺序：trunk 配置变了 → 作废重建；无 trunk → 新建为 trunk；
    /// trunk 存在但 reuse_key 不匹配或已被占用 → 改走 ephemeral fork。
    /// 返回 (本次结果, 埋点)。
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "review session selection and trunk spawning must stay serialized"
    )]
    pub(super) async fn run_review(
        &self,
        params: GuardianReviewSessionParams,
    ) -> (GuardianReviewSessionOutcome, GuardianReviewAnalyticsResult) {
        // 统一截止时刻：后续每个可能阻塞的步骤都受它约束（见 run_before_review_deadline）。
        let deadline = tokio::time::Instant::now() + GUARDIAN_REVIEW_TIMEOUT;
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(&params.spawn_config);
        let mut stale_trunk_to_shutdown = None;
        let mut spawned_trunk = false;
        let trunk_candidate = match run_before_review_deadline(
            deadline,
            params.external_cancel.as_ref(),
            self.state.lock(),
        )
        .await
        {
            Ok(mut state) => {
                // 配置已变且当前 trunk 空闲（能拿到锁）→ 摘除旧 trunk 稍后销毁。
                // 若旧 trunk 正忙则不动它，本次改走 ephemeral，等它自己跑完。
                if let Some(trunk) = state.trunk.as_ref()
                    && trunk.reuse_key != next_reuse_key
                    && trunk.review_lock.try_acquire().is_ok()
                {
                    stale_trunk_to_shutdown = state.trunk.take();
                }

                // 无可用 trunk → 在持锁状态下新建一个并设为 trunk（串行化保证）。
                if state.trunk.is_none() {
                    let spawn_cancel_token = CancellationToken::new();
                    let review_session = match run_before_review_deadline_with_cancel(
                        deadline,
                        params.external_cancel.as_ref(),
                        &spawn_cancel_token,
                        Box::pin(spawn_guardian_review_session(
                            &params,
                            params.spawn_config.clone(),
                            next_reuse_key.clone(),
                            spawn_cancel_token.clone(),
                            /*fork_snapshot*/ None,
                        )),
                    )
                    .await
                    {
                        Ok(Ok(review_session)) => Arc::new(review_session),
                        Ok(Err(err)) => {
                            return (
                                GuardianReviewSessionOutcome::PromptBuildFailed(err),
                                GuardianReviewAnalyticsResult::without_session(),
                            );
                        }
                        Err(outcome) => {
                            return (outcome, GuardianReviewAnalyticsResult::without_session());
                        }
                    };
                    state.trunk = Some(Arc::clone(&review_session));
                    spawned_trunk = true;
                }

                state.trunk.as_ref().cloned()
            }
            Err(outcome) => return (outcome, GuardianReviewAnalyticsResult::without_session()),
        };

        if let Some(review_session) = stale_trunk_to_shutdown {
            review_session.shutdown_in_background();
        }

        let Some(trunk) = trunk_candidate else {
            return (
                GuardianReviewSessionOutcome::Completed(Err(anyhow!(
                    "guardian review session was not available after spawn"
                ))),
                GuardianReviewAnalyticsResult::without_session(),
            );
        };

        if trunk.reuse_key != next_reuse_key {
            return Box::pin(self.run_ephemeral_review(
                params,
                next_reuse_key,
                deadline,
                /*fork_snapshot*/ None,
            ))
            .await;
        }

        // 尝试占用 trunk 的单许可锁：拿到则在 trunk 上跑；拿不到（已有审查在跑）
        // 则带上 trunk 当前快照去 fork 一个 ephemeral 会话并行处理。
        let trunk_guard = match trunk.review_lock.try_acquire() {
            Ok(trunk_guard) => trunk_guard,
            Err(_) => {
                return Box::pin(self.run_ephemeral_review(
                    params,
                    next_reuse_key,
                    deadline,
                    trunk.fork_snapshot().await,
                ))
                .await;
            }
        };

        let guardian_session_kind = if spawned_trunk {
            GuardianReviewSessionKind::TrunkNew
        } else {
            GuardianReviewSessionKind::TrunkReused
        };
        let (outcome, keep_review_session, analytics_result) = Box::pin(run_review_on_session(
            trunk.as_ref(),
            &params,
            guardian_session_kind,
            deadline,
        ))
        .await;
        // 成功且会话仍可复用 → 刷新 fork 快照，供后续 ephemeral 续接。
        if keep_review_session && matches!(outcome, GuardianReviewSessionOutcome::Completed(_)) {
            trunk.refresh_last_committed_fork_snapshot().await;
        }
        drop(trunk_guard);

        // 若本次判定不可再复用该会话（如超时排空失败），把它从 trunk 槽摘除并销毁，
        // 下次审查会重新建一个干净的 trunk。
        if keep_review_session {
            (outcome, analytics_result)
        } else {
            if let Some(review_session) = self.remove_trunk_if_current(&trunk).await {
                review_session.shutdown_in_background();
            }
            (outcome, analytics_result)
        }
    }

    #[cfg(test)]
    pub(crate) async fn cache_for_test(&self, codex: Codex) {
        let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            codex.session.get_config().await.as_ref(),
        );
        self.state.lock().await.trunk = Some(Arc::new(GuardianReviewSession {
            reuse_key,
            codex,
            cancel_token: CancellationToken::new(),
            review_lock: Semaphore::new(/*permits*/ 1),
            state: Mutex::new(GuardianReviewState {
                prior_review_count: 0,
                last_reviewed_transcript_cursor: None,
                last_committed_fork_snapshot: None,
            }),
        }));
    }

    #[cfg(test)]
    pub(crate) async fn register_ephemeral_for_test(&self, codex: Codex) {
        let reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(
            codex.session.get_config().await.as_ref(),
        );
        self.state
            .lock()
            .await
            .ephemeral_reviews
            .push(Arc::new(GuardianReviewSession {
                reuse_key,
                codex,
                cancel_token: CancellationToken::new(),
                review_lock: Semaphore::new(/*permits*/ 1),
                state: Mutex::new(GuardianReviewState {
                    prior_review_count: 0,
                    last_reviewed_transcript_cursor: None,
                    last_committed_fork_snapshot: None,
                }),
            }));
    }

    #[cfg(test)]
    pub(crate) async fn committed_fork_rollout_items_for_test(&self) -> Option<Vec<RolloutItem>> {
        let trunk = self.state.lock().await.trunk.clone()?;
        let state = trunk.state.lock().await;
        let snapshot = state.last_committed_fork_snapshot.as_ref()?;
        match &snapshot.initial_history {
            InitialHistory::Forked(items) => Some(items.clone()),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Resumed(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn send_trunk_event_raw_for_test(&self, event: Event) {
        let trunk = self
            .state
            .lock()
            .await
            .trunk
            .clone()
            .expect("guardian trunk should exist");
        trunk.codex.session.send_event_raw(event).await;
    }

    async fn remove_trunk_if_current(
        &self,
        trunk: &Arc<GuardianReviewSession>,
    ) -> Option<Arc<GuardianReviewSession>> {
        let mut state = self.state.lock().await;
        if state
            .trunk
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, trunk))
        {
            state.trunk.take()
        } else {
            None
        }
    }

    async fn register_active_ephemeral(&self, review_session: Arc<GuardianReviewSession>) {
        self.state
            .lock()
            .await
            .ephemeral_reviews
            .push(review_session);
    }

    async fn take_active_ephemeral(
        &self,
        review_session: &Arc<GuardianReviewSession>,
    ) -> Option<Arc<GuardianReviewSession>> {
        let mut state = self.state.lock().await;
        let ephemeral_review_index = state
            .ephemeral_reviews
            .iter()
            .position(|active_review| Arc::ptr_eq(active_review, review_session))?;
        Some(state.ephemeral_reviews.swap_remove(ephemeral_review_index))
    }

    /// 在一次性 fork 会话上跑审查：trunk 忙或配置不匹配时的并行路径。
    /// 用 `EphemeralReviewCleanup` 哨兵保证异常时也能回收，正常完成则 disarm 后
    /// 显式后台销毁。`fork_config.ephemeral = true` 标记其不进入复用池。
    async fn run_ephemeral_review(
        &self,
        params: GuardianReviewSessionParams,
        reuse_key: GuardianReviewSessionReuseKey,
        deadline: tokio::time::Instant,
        fork_snapshot: Option<GuardianReviewForkSnapshot>,
    ) -> (GuardianReviewSessionOutcome, GuardianReviewAnalyticsResult) {
        let spawn_cancel_token = CancellationToken::new();
        let mut fork_config = params.spawn_config.clone();
        fork_config.ephemeral = true;
        let review_session = match run_before_review_deadline_with_cancel(
            deadline,
            params.external_cancel.as_ref(),
            &spawn_cancel_token,
            Box::pin(spawn_guardian_review_session(
                &params,
                fork_config,
                reuse_key,
                spawn_cancel_token.clone(),
                fork_snapshot,
            )),
        )
        .await
        {
            Ok(Ok(review_session)) => Arc::new(review_session),
            Ok(Err(err)) => {
                return (
                    GuardianReviewSessionOutcome::PromptBuildFailed(err),
                    GuardianReviewAnalyticsResult::without_session(),
                );
            }
            Err(outcome) => return (outcome, GuardianReviewAnalyticsResult::without_session()),
        };
        self.register_active_ephemeral(Arc::clone(&review_session))
            .await;
        let mut cleanup =
            EphemeralReviewCleanup::new(Arc::clone(&self.state), Arc::clone(&review_session));

        let (outcome, _, analytics_result) = Box::pin(run_review_on_session(
            review_session.as_ref(),
            &params,
            GuardianReviewSessionKind::EphemeralForked,
            deadline,
        ))
        .await;
        if let Some(review_session) = self.take_active_ephemeral(&review_session).await {
            cleanup.disarm();
            review_session.shutdown_in_background();
        }
        (outcome, analytics_result)
    }
}

/// 实际拉起一个 Guardian 审查子会话（`SubAgentSource::Other("guardian")`）。
/// 若带 fork_snapshot，则以其历史/计数/游标初始化（续接 trunk 进度）；否则全新开始。
/// 子会话经 `run_codex_thread_interactive` 创建，复用父会话的 auth/models 管理器。
async fn spawn_guardian_review_session(
    params: &GuardianReviewSessionParams,
    spawn_config: Config,
    reuse_key: GuardianReviewSessionReuseKey,
    cancel_token: CancellationToken,
    fork_snapshot: Option<GuardianReviewForkSnapshot>,
) -> anyhow::Result<GuardianReviewSession> {
    let (initial_history, prior_review_count, initial_transcript_cursor) = match fork_snapshot {
        Some(fork_snapshot) => (
            Some(fork_snapshot.initial_history),
            fork_snapshot.prior_review_count,
            fork_snapshot.last_reviewed_transcript_cursor,
        ),
        None => (None, 0, None),
    };
    let codex = Box::pin(run_codex_thread_interactive(
        spawn_config,
        params.parent_session.services.auth_manager.clone(),
        params.parent_session.services.models_manager.clone(),
        Arc::clone(&params.parent_session),
        Arc::clone(&params.parent_turn),
        cancel_token.clone(),
        SubAgentSource::Other(GUARDIAN_REVIEWER_NAME.to_string()),
        initial_history,
    ))
    .await?;

    Ok(GuardianReviewSession {
        codex,
        cancel_token,
        reuse_key,
        review_lock: Semaphore::new(/*permits*/ 1),
        state: Mutex::new(GuardianReviewState {
            prior_review_count,
            last_reviewed_transcript_cursor: initial_transcript_cursor,
            last_committed_fork_snapshot: None,
        }),
    })
}

/// 在「已选定的某个会话」上跑完整一轮审查。
/// 流程：定提示模式（Full/Delta）→ 构造提示项 → 以只读+approval=never 覆盖提交
/// 一轮 → 等待该轮完成 → 成功则推进审查计数与转写稿游标。
/// 返回 (结果, 是否保留会话以供复用, 埋点)。其中第二个 bool 决定上层是否回收会话。
async fn run_review_on_session(
    review_session: &GuardianReviewSession,
    params: &GuardianReviewSessionParams,
    guardian_session_kind: GuardianReviewSessionKind,
    deadline: tokio::time::Instant,
) -> (
    GuardianReviewSessionOutcome,
    bool,
    GuardianReviewAnalyticsResult,
) {
    // Step 1：依据本会话历史决定提示形态。
    // prior_review_count==0 → 首审，发 Full；==1 → 额外补发一次「跟进提醒」；
    // 已有游标 → 发 Delta（仅增量转写稿），否则回退 Full。
    let (send_followup_reminder, prompt_mode) = {
        let state = review_session.state.lock().await;

        let send_followup_reminder = state.prior_review_count == 1;
        let prompt_mode = if state.prior_review_count == 0 {
            GuardianPromptMode::Full
        } else if let Some(cursor) = state.last_reviewed_transcript_cursor {
            GuardianPromptMode::Delta { cursor }
        } else {
            GuardianPromptMode::Full
        };

        (send_followup_reminder, prompt_mode)
    };
    let model_info = params
        .parent_session
        .services
        .models_manager
        .get_model_info(
            params.model.as_str(),
            &params.spawn_config.to_models_manager_config(),
        )
        .await;
    let guardian_reasoning_effort = if model_info.supports_reasoning_summaries {
        params
            .reasoning_effort
            .or(model_info.default_reasoning_level)
    } else {
        None
    };
    let mut analytics_result = GuardianReviewAnalyticsResult::from_session(
        review_session.codex.session.conversation_id.to_string(),
        guardian_session_kind,
        params.model.clone(),
        guardian_reasoning_effort.map(|effort| effort.to_string()),
        had_prior_review_context(&prompt_mode),
    );
    if send_followup_reminder {
        append_guardian_followup_reminder(review_session).await;
    }

    // Step 2：在 deadline 约束下构造提示项。构造前先把父会话已批准的网络主机
    // 同步给审查子会话，使其做只读网络检查时与父会话看到一致的白名单。
    let prompt_items = run_before_review_deadline(
        deadline,
        params.external_cancel.as_ref(),
        Box::pin(async {
            params
                .parent_session
                .services
                .network_approval
                .sync_session_approved_hosts_to(
                    &review_session.codex.session.services.network_approval,
                )
                .await;

            build_guardian_prompt_items(
                params.parent_session.as_ref(),
                params.retry_reason.clone(),
                params.request.clone(),
                prompt_mode,
            )
            .await
        }),
    )
    .await;
    let prompt_items = match prompt_items {
        Ok(prompt_items) => prompt_items,
        Err(outcome) => return (outcome, false, analytics_result),
    };
    let prompt_items = match prompt_items {
        Ok(prompt_items) => prompt_items,
        Err(err) => {
            return (
                GuardianReviewSessionOutcome::PromptBuildFailed(err.into()),
                false,
                analytics_result,
            );
        }
    };
    let reviewed_action_truncated = prompt_items.reviewed_action_truncated;
    let transcript_cursor = prompt_items.transcript_cursor;
    // 记录提交前的累计 token 用量，结束后做差得本次增量（trunk 复用语义，见 token_usage_delta）。
    let token_usage_at_review_start = review_session
        .codex
        .session
        .total_token_usage()
        .await
        .unwrap_or_default();
    // The legacy SandboxPolicy should match the PermissionProfile.
    // Step 3：把审查这一轮强制锁成只读：权限档 = read_only，沙箱策略 = 只读，
    // 二者必须一致。下面提交时还会叠加 approval_policy = Never，确保审查器既不能
    // 改动文件，也不会再弹出新的审批。
    let guardian_permission_profile = PermissionProfile::read_only();
    let legacy_sandbox_policy = SandboxPolicy::new_read_only_policy();

    let submit_result = run_before_review_deadline(
        deadline,
        params.external_cancel.as_ref(),
        Box::pin(review_session.codex.submit(Op::UserInput {
            items: prompt_items.items,
            environments: None,
            final_output_json_schema: Some(params.schema.clone()),
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                #[allow(deprecated)]
                cwd: Some(params.parent_turn.cwd.to_path_buf()),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(legacy_sandbox_policy),
                permission_profile: Some(guardian_permission_profile),
                summary: Some(params.reasoning_summary),
                personality: params.personality,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: params.model.clone(),
                        reasoning_effort: params.reasoning_effort,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })),
    )
    .await;
    let child_turn_id = match submit_result {
        Ok(Ok(child_turn_id)) => child_turn_id,
        Ok(Err(err)) => {
            return (
                GuardianReviewSessionOutcome::SessionFailed(err.into()),
                false,
                analytics_result,
            );
        }
        Err(outcome) => return (outcome, false, analytics_result),
    };
    analytics_result.reviewed_action_truncated = reviewed_action_truncated;

    // Step 4：等待「我们刚提交的这一轮」完成（按 turn_id 精确匹配，忽略残留旧事件）。
    let outcome = wait_for_guardian_review(
        review_session,
        child_turn_id.as_str(),
        deadline,
        params.external_cancel.as_ref(),
        &mut analytics_result,
    )
    .await;
    // Step 5：仅当本轮真正完成时，才记录 token 增量并推进会话进度
    // （审查计数 +1、保存最新游标），使下次审查可走 Delta 增量提示。
    if matches!(outcome.0, GuardianReviewSessionOutcome::Completed(_)) {
        if outcome.2
            && let Some(total_token_usage) = review_session.codex.session.total_token_usage().await
        {
            analytics_result.token_usage = Some(token_usage_delta(
                &token_usage_at_review_start,
                &total_token_usage,
            ));
        }
        let mut state = review_session.state.lock().await;
        state.prior_review_count = state.prior_review_count.saturating_add(1);
        state.last_reviewed_transcript_cursor = Some(transcript_cursor);
    }
    (outcome.0, outcome.1, analytics_result)
}

async fn append_guardian_followup_reminder(review_session: &GuardianReviewSession) {
    let reminder: ResponseItem = ContextualUserFragment::into(GuardianFollowupReviewReminder);
    review_session
        .codex
        .session
        .inject_no_new_turn(vec![reminder], /*current_turn_context*/ None)
        .await;
}

// 读取某会话已落盘的完整 rollout 条目（含归档），作为 fork 的初始历史来源。
// 先确保 rollout 物化并 flush，再从持久化线程加载，保证拿到的是最新已提交状态。
async fn load_rollout_items_for_fork(
    session: &Session,
) -> anyhow::Result<Option<Vec<RolloutItem>>> {
    session.try_ensure_rollout_materialized().await?;
    session.flush_rollout().await?;
    let live_thread = session.live_thread_for_persistence("guardian review fork")?;
    let history = live_thread.load_history(/*include_archived*/ true).await?;
    Ok(Some(history.items))
}

/// 等待指定 turn_id 的审查轮结束，三路竞争：deadline 超时 / 外部取消 / 子会话事件。
/// 超时或取消时会向子会话发 Interrupt 并排空，排空成功才允许复用该会话（返回的
/// 第二个 bool）。事件流里只认匹配 turn_id 的终态，旧轮的残留事件被忽略。
/// 返回 (结果, 是否可复用会话, 是否应统计 token 用量)。
async fn wait_for_guardian_review(
    review_session: &GuardianReviewSession,
    expected_turn_id: &str,
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    analytics_result: &mut GuardianReviewAnalyticsResult,
) -> (GuardianReviewSessionOutcome, bool, bool) {
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let mut last_error_message: Option<String> = None;

    loop {
        tokio::select! {
            _ = &mut timeout => {
                let keep_review_session = interrupt_and_drain_turn(
                    &review_session.codex,
                    expected_turn_id,
                )
                .await
                .is_ok();
                return (GuardianReviewSessionOutcome::TimedOut, keep_review_session, false);
            }
            _ = async {
                if let Some(cancel_token) = external_cancel {
                    cancel_token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let keep_review_session = interrupt_and_drain_turn(
                    &review_session.codex,
                    expected_turn_id,
                )
                .await
                .is_ok();
                return (GuardianReviewSessionOutcome::Aborted, keep_review_session, false);
            }
            event = review_session.codex.next_event() => {
                match event {
                    Ok(event) if !event_matches_turn(&event, expected_turn_id) => {}
                    Ok(event) => match event.msg {
                        EventMsg::TurnComplete(turn_complete) => {
                            analytics_result.time_to_first_token_ms = turn_complete
                                .time_to_first_token_ms
                                .and_then(|ms| u64::try_from(ms).ok());
                            // 本轮完成却没有最终消息，但此前收到过 Error 事件：
                            // 把那条错误上抛为「完成但失败」，避免静默返回空裁决。
                            if turn_complete.last_agent_message.is_none()
                                && let Some(error_message) = last_error_message
                            {
                                return (
                                    GuardianReviewSessionOutcome::Completed(Err(anyhow!(error_message))),
                                    true,
                                    true,
                                );
                            }
                            return (
                                GuardianReviewSessionOutcome::Completed(Ok(turn_complete.last_agent_message)),
                                true,
                                true,
                            );
                        }
                        EventMsg::Error(error) => {
                            last_error_message = Some(error.message);
                        }
                        EventMsg::TurnAborted(_) => {
                            return (GuardianReviewSessionOutcome::Aborted, true, false);
                        }
                        _ => {}
                    },
                    Err(err) => {
                        return (
                            GuardianReviewSessionOutcome::Completed(Err(err.into())),
                            false,
                            false,
                        );
                    }
                }
            }
        }
    }
}

// 判断一个事件是否属于「我们期待的那一轮」。复用会话上可能混入旧轮的残留终态
// 事件，需双重校验：先比 event.id，对终态事件再比其内部 turn_id，避免误把上一轮
// 的 TurnComplete/TurnAborted 当成本轮结果。
fn event_matches_turn(event: &Event, expected_turn_id: &str) -> bool {
    if event.id != expected_turn_id {
        return false;
    }

    match &event.msg {
        EventMsg::TurnComplete(turn_complete) => turn_complete.turn_id == expected_turn_id,
        EventMsg::TurnAborted(turn_aborted) => {
            turn_aborted.turn_id.as_deref() == Some(expected_turn_id)
        }
        _ => true,
    }
}

/// 由父配置派生出「锁死的审查子会话配置」——Guardian 安全边界的核心所在。
/// 在父配置基础上做减法/替换：换审查模型与推理强度；用 Guardian 策略覆盖
/// base_instructions；清空 developer 指令、通知、MCP servers；强制 approval=Never
/// 与只读权限档；按需用实时网络配置重建白名单（仅供只读检查）；最后关闭一批
/// 非必要特性（spawn/collab/hooks/apps/plugins/websearch 等）防止审查器越权。
/// 任一关键设置失败即返回 Err（fail closed）。
pub(crate) fn build_guardian_review_session_config(
    parent_config: &Config,
    live_network_config: Option<codex_network_proxy::NetworkProxyConfig>,
    active_model: &str,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
) -> anyhow::Result<Config> {
    let mut guardian_config = parent_config.clone();
    guardian_config.model = Some(active_model.to_string());
    guardian_config.model_reasoning_effort = reasoning_effort;
    guardian_config.include_skill_instructions = false;
    // 用 Guardian 审查策略覆盖系统指令：优先用工作区下发的租户策略覆写，
    // 否则用内置默认策略（见 prompt.rs 的 guardian_policy_prompt*）。
    guardian_config.base_instructions = Some(
        parent_config
            .guardian_policy_config
            .as_deref()
            .map(guardian_policy_prompt_with_config)
            .unwrap_or_else(guardian_policy_prompt),
    );
    guardian_config.notify = None;
    guardian_config.developer_instructions = None;
    guardian_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    guardian_config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .map_err(|err| {
            anyhow::anyhow!("guardian review session could not set permission profile: {err}")
        })?;
    guardian_config.include_apps_instructions = false;
    guardian_config
        .mcp_servers
        .set(HashMap::new())
        .map_err(|err| {
            anyhow::anyhow!("guardian review session could not clear MCP servers: {err}")
        })?;
    // 若有实时网络配置且原本就允许网络：用实时配置 + 既有约束 + 只读权限档重建
    // 网络规格。这样审查器能做只读网络检查（如解析白名单），但仍受只读档约束。
    if let Some(live_network_config) = live_network_config
        && guardian_config.permissions.network.is_some()
    {
        let network_constraints = guardian_config
            .config_layer_stack
            .requirements()
            .network
            .as_ref()
            .map(|network| network.value.clone());
        guardian_config.permissions.network = Some(NetworkProxySpec::from_config_and_constraints(
            live_network_config,
            network_constraints,
            guardian_config.permissions.permission_profile(),
        )?);
    }
    // 逐项关闭非必要 Agent 特性：审查器不该派生子 Agent、跑 hooks、装 apps/plugins
    // 或发联网搜索——这些都可能让审查越权或产生副作用。disable 失败直接返回 Err；
    // 若关后仍显示启用，则降级为 warn 继续（尽力而为，不阻断审查）。
    for feature in [
        Feature::SpawnCsv,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::CodexHooks,
        Feature::Apps,
        Feature::Plugins,
        Feature::WebSearchRequest,
        Feature::WebSearchCached,
    ] {
        guardian_config.features.disable(feature).map_err(|err| {
            anyhow::anyhow!(
                "guardian review session could not disable `features.{}`: {err}",
                feature.key()
            )
        })?;
        if guardian_config.features.enabled(feature) {
            warn!(
                "guardian review session could not disable `features.{}`; continuing with the feature enabled",
                feature.key()
            );
        }
    }
    Ok(guardian_config)
}

/// 通用包装：让任意 future 在「截止时刻」与「外部取消」双重约束下运行。
/// 三路 select：超时 → `TimedOut`；取消 → `Aborted`；正常完成 → `Ok(结果)`。
/// 整个审查流程的每个阻塞步骤都套这层，保证不会超出统一 deadline。
async fn run_before_review_deadline<T>(
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    future: impl Future<Output = T>,
) -> Result<T, GuardianReviewSessionOutcome> {
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => Err(GuardianReviewSessionOutcome::TimedOut),
        result = future => Ok(result),
        _ = async {
            if let Some(cancel_token) = external_cancel {
                cancel_token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => Err(GuardianReviewSessionOutcome::Aborted),
    }
}

// 同上，但在超时/取消时额外取消传入的 `cancel_token`——用于「正在 spawn 子会话」
// 的场景：一旦放弃等待，就连带取消那次半途的 spawn，避免遗留半初始化的会话。
async fn run_before_review_deadline_with_cancel<T>(
    deadline: tokio::time::Instant,
    external_cancel: Option<&CancellationToken>,
    cancel_token: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, GuardianReviewSessionOutcome> {
    let result = run_before_review_deadline(deadline, external_cancel, future).await;
    if result.is_err() {
        cancel_token.cancel();
    }
    result
}

/// 向子会话发 Interrupt 并「排空」直到看见本轮的 TurnAborted/TurnComplete。
/// 目的：超时/取消后，把会话恢复到干净的空闲态，从而安全复用（trunk）。
/// 受 `GUARDIAN_INTERRUPT_DRAIN_TIMEOUT` 限时，超时则返回 Err（调用方据此放弃复用）。
async fn interrupt_and_drain_turn(codex: &Codex, expected_turn_id: &str) -> anyhow::Result<()> {
    let _ = codex.submit(Op::Interrupt).await;

    tokio::time::timeout(GUARDIAN_INTERRUPT_DRAIN_TIMEOUT, async {
        loop {
            let event = codex.next_event().await?;
            if event_matches_turn(&event, expected_turn_id)
                && matches!(
                    event.msg,
                    EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
                )
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out draining guardian review session after interrupt"))??;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Submission;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;

    async fn test_review_session() -> (
        GuardianReviewSession,
        async_channel::Sender<Event>,
        async_channel::Receiver<Submission>,
    ) {
        let (session, _turn, _rx) = crate::session::tests::make_session_and_context_with_rx().await;
        let (tx_sub, rx_sub) = async_channel::bounded(4);
        let (tx_event, rx_event) = async_channel::unbounded();
        let (_agent_status_tx, agent_status) =
            tokio::sync::watch::channel(AgentStatus::PendingInit);
        let reuse_key =
            GuardianReviewSessionReuseKey::from_spawn_config(session.get_config().await.as_ref());

        (
            GuardianReviewSession {
                codex: Codex {
                    tx_sub,
                    rx_event,
                    agent_status,
                    session,
                    session_loop_termination: crate::session::completed_session_loop_termination(),
                },
                cancel_token: CancellationToken::new(),
                reuse_key,
                review_lock: Semaphore::new(/*permits*/ 1),
                state: Mutex::new(GuardianReviewState {
                    prior_review_count: 0,
                    last_reviewed_transcript_cursor: None,
                    last_committed_fork_snapshot: None,
                }),
            },
            tx_event,
            rx_sub,
        )
    }

    fn turn_complete_event(
        turn_id: &str,
        last_agent_message: Option<&str>,
        time_to_first_token_ms: Option<i64>,
    ) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_id.to_string(),
                last_agent_message: last_agent_message.map(str::to_string),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms,
            }),
        }
    }

    fn turn_aborted_event(turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_id.to_string()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            }),
        }
    }

    async fn test_review_params() -> GuardianReviewSessionParams {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let model = turn.model_info.slug.clone();
        let reasoning_effort = turn.reasoning_effort;
        let reasoning_summary = turn.reasoning_summary;
        let personality = turn.personality;
        #[allow(deprecated)]
        let cwd = turn.cwd.clone();
        let spawn_config = build_guardian_review_session_config(
            turn.config.as_ref(),
            /*live_network_config*/ None,
            model.as_str(),
            reasoning_effort,
        )
        .expect("guardian config");

        GuardianReviewSessionParams {
            parent_session: Arc::new(session),
            parent_turn: Arc::new(turn),
            spawn_config,
            request: GuardianApprovalRequest::Shell {
                id: "shell-1".to_string(),
                command: vec!["git".to_string(), "status".to_string()],
                cwd,
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: Some("Inspect repo state.".to_string()),
            },
            retry_reason: None,
            schema: super::super::prompt::guardian_output_schema(),
            model,
            reasoning_effort,
            reasoning_summary,
            personality,
            external_cancel: None,
        }
    }

    #[tokio::test]
    async fn guardian_review_session_config_change_invalidates_cached_session() {
        let parent_config = crate::config::test_config().await;
        let cached_spawn_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("cached guardian config");
        let cached_reuse_key =
            GuardianReviewSessionReuseKey::from_spawn_config(&cached_spawn_config);

        let mut changed_parent_config = parent_config;
        changed_parent_config.model_provider.base_url =
            Some("https://guardian.example.invalid/v1".to_string());
        let next_spawn_config = build_guardian_review_session_config(
            &changed_parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("next guardian config");
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(&next_spawn_config);

        assert_ne!(cached_reuse_key, next_reuse_key);
        assert_eq!(
            cached_reuse_key,
            GuardianReviewSessionReuseKey::from_spawn_config(&cached_spawn_config)
        );
    }

    #[tokio::test]
    async fn guardian_prompt_cache_key_is_scoped_to_parent_thread() {
        let session_source =
            SessionSource::SubAgent(SubAgentSource::Other(GUARDIAN_REVIEWER_NAME.to_string()));
        let parent_thread_id = ThreadId::new();
        let key =
            prompt_cache_key_override_for_review_session(&session_source, Some(parent_thread_id))
                .expect("guardian prompt cache key");

        assert_eq!(key, format!("guardian:{parent_thread_id}"));
        assert!(
            key.len() <= 64,
            "guardian prompt cache key should fit the Responses API limit"
        );
        assert_eq!(
            key,
            prompt_cache_key_override_for_review_session(&session_source, Some(parent_thread_id))
                .expect("same guardian prompt cache key")
        );
        assert_ne!(
            key,
            prompt_cache_key_override_for_review_session(&session_source, Some(ThreadId::new()))
                .expect("different parent guardian prompt cache key")
        );
        assert_eq!(
            None,
            prompt_cache_key_override_for_review_session(
                &SessionSource::Cli,
                Some(parent_thread_id)
            )
        );
        assert_eq!(
            None,
            prompt_cache_key_override_for_review_session(
                &session_source,
                /*parent_thread_id*/ None
            )
        );
    }

    #[tokio::test]
    async fn guardian_review_session_compact_scope_change_invalidates_cached_session() {
        let parent_config = crate::config::test_config().await;
        let cached_spawn_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("cached guardian config");
        let cached_reuse_key =
            GuardianReviewSessionReuseKey::from_spawn_config(&cached_spawn_config);

        let mut changed_parent_config = parent_config;
        changed_parent_config.model_auto_compact_token_limit_scope =
            AutoCompactTokenLimitScope::BodyAfterPrefix;
        let next_spawn_config = build_guardian_review_session_config(
            &changed_parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("next guardian config");
        let next_reuse_key = GuardianReviewSessionReuseKey::from_spawn_config(&next_spawn_config);

        assert_ne!(cached_reuse_key, next_reuse_key);
    }

    #[tokio::test]
    async fn guardian_review_session_config_disables_hooks() {
        let mut parent_config = crate::config::test_config().await;
        parent_config
            .features
            .enable(Feature::CodexHooks)
            .expect("enable hooks on parent config");

        let guardian_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("guardian config");

        assert!(!guardian_config.features.enabled(Feature::CodexHooks));
    }

    #[tokio::test]
    async fn guardian_review_session_config_disables_skill_instructions() {
        let mut parent_config = crate::config::test_config().await;
        parent_config.include_skill_instructions = true;

        let guardian_config = build_guardian_review_session_config(
            &parent_config,
            /*live_network_config*/ None,
            "active-model",
            /*reasoning_effort*/ None,
        )
        .expect("guardian config");

        assert!(!guardian_config.include_skill_instructions);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_times_out_before_future_completes() {
        let outcome = run_before_review_deadline(
            tokio::time::Instant::now() + Duration::from_millis(10),
            /*external_cancel*/ None,
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::TimedOut)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_aborts_when_cancelled() {
        let cancel_token = CancellationToken::new();
        let canceller = cancel_token.clone();
        drop(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            canceller.cancel();
        }));

        let outcome = run_before_review_deadline(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(&cancel_token),
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::Aborted)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_cancels_token_on_timeout() {
        let cancel_token = CancellationToken::new();

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_millis(10),
            /*external_cancel*/ None,
            &cancel_token,
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::TimedOut)
        ));
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_cancels_token_on_abort() {
        let external_cancel = CancellationToken::new();
        let external_canceller = external_cancel.clone();
        let cancel_token = CancellationToken::new();
        drop(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            external_canceller.cancel();
        }));

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(&external_cancel),
            &cancel_token,
            std::future::pending::<()>(),
        )
        .await;

        assert!(matches!(
            outcome,
            Err(GuardianReviewSessionOutcome::Aborted)
        ));
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_before_review_deadline_with_cancel_preserves_token_on_success() {
        let cancel_token = CancellationToken::new();

        let outcome = run_before_review_deadline_with_cancel(
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &cancel_token,
            async { 42usize },
        )
        .await;

        assert_eq!(outcome.unwrap(), 42);
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn had_prior_review_context_tracks_prompt_mode() {
        assert!(!had_prior_review_context(&GuardianPromptMode::Full));
        assert!(had_prior_review_context(&GuardianPromptMode::Delta {
            cursor: GuardianTranscriptCursor {
                parent_history_version: 7,
                transcript_entry_count: 42,
            }
        }));
    }

    #[test]
    fn token_usage_delta_never_reports_negative_usage() {
        let start = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 8,
            output_tokens: 6,
            reasoning_output_tokens: 4,
            total_tokens: 28,
        };
        let end = TokenUsage {
            input_tokens: 15,
            cached_input_tokens: 7,
            output_tokens: 10,
            reasoning_output_tokens: 2,
            total_tokens: 34,
        };

        assert_eq!(
            token_usage_delta(&start, &end),
            TokenUsage {
                input_tokens: 5,
                cached_input_tokens: 0,
                output_tokens: 4,
                reasoning_output_tokens: 0,
                total_tokens: 6,
            }
        );
    }

    #[tokio::test]
    async fn run_review_on_reused_session_waits_for_submitted_turn() {
        let (review_session, tx_event, rx_sub) = test_review_session().await;
        {
            let mut state = review_session.state.lock().await;
            state.prior_review_count = 1;
            state.last_reviewed_transcript_cursor = Some(GuardianTranscriptCursor {
                parent_history_version: 0,
                transcript_entry_count: 0,
            });
        }
        let params = test_review_params().await;

        let review = tokio::spawn(async move {
            run_review_on_session(
                &review_session,
                &params,
                GuardianReviewSessionKind::TrunkReused,
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await
        });
        let submission = rx_sub.recv().await.expect("guardian submission");
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        tx_event
            .send(turn_complete_event(
                submission.id.as_str(),
                Some("fresh"),
                Some(42),
            ))
            .await
            .expect("queue submitted turn completion");

        let (outcome, keep_review_session, analytics_result) =
            review.await.expect("review task should complete");
        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected submitted turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_completion() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        tx_event
            .send(turn_complete_event("current-turn", Some("fresh"), Some(42)))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_errors() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(Event {
                id: "prior-turn".to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "stale guardian error".to_string(),
                    codex_error_info: None,
                }),
            })
            .await
            .expect("queue prior turn error");
        tx_event
            .send(turn_complete_event(
                "current-turn",
                /*last_agent_message*/ None,
                Some(42),
            ))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message, None);
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_ignores_prior_turn_aborts() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(turn_aborted_event("prior-turn"))
            .await
            .expect("queue prior turn abort");
        tx_event
            .send(turn_complete_event("current-turn", Some("fresh"), Some(42)))
            .await
            .expect("queue current turn completion");

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        let GuardianReviewSessionOutcome::Completed(Ok(last_agent_message)) = outcome else {
            panic!("expected current turn completion");
        };
        assert_eq!(last_agent_message.as_deref(), Some("fresh"));
        assert_eq!(analytics_result.time_to_first_token_ms, Some(42));
        assert!(keep_review_session);
        assert!(capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_timeout_drains_expected_turn_after_stale_terminal_event() {
        let (review_session, tx_event, rx_sub) = test_review_session().await;
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        let tx_interrupt_event = tx_event.clone();
        let interrupt_response = tokio::spawn(async move {
            let submission = rx_sub.recv().await.expect("interrupt submission");
            assert!(matches!(submission.op, Op::Interrupt));
            tx_interrupt_event
                .send(turn_aborted_event("current-turn"))
                .await
                .expect("queue current turn abort");
        });

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_millis(10),
            /*external_cancel*/ None,
            &mut analytics_result,
        )
        .await;

        interrupt_response
            .await
            .expect("interrupt response task should complete");
        assert!(matches!(outcome, GuardianReviewSessionOutcome::TimedOut));
        assert!(keep_review_session);
        assert!(!capture_token_usage);
    }

    #[tokio::test]
    async fn wait_for_guardian_review_cancel_drains_expected_turn_after_stale_terminal_event() {
        let (review_session, tx_event, rx_sub) = test_review_session().await;
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        let tx_interrupt_event = tx_event.clone();
        let interrupt_response = tokio::spawn(async move {
            let submission = rx_sub.recv().await.expect("interrupt submission");
            assert!(matches!(submission.op, Op::Interrupt));
            tx_interrupt_event
                .send(turn_aborted_event("current-turn"))
                .await
                .expect("queue current turn abort");
        });
        let external_cancel = CancellationToken::new();
        external_cancel.cancel();

        let mut analytics_result = GuardianReviewAnalyticsResult::without_session();
        let (outcome, keep_review_session, capture_token_usage) = wait_for_guardian_review(
            &review_session,
            "current-turn",
            tokio::time::Instant::now() + Duration::from_secs(1),
            Some(&external_cancel),
            &mut analytics_result,
        )
        .await;

        interrupt_response
            .await
            .expect("interrupt response task should complete");
        assert!(matches!(outcome, GuardianReviewSessionOutcome::Aborted));
        assert!(keep_review_session);
        assert!(!capture_token_usage);
    }

    #[tokio::test]
    async fn interrupt_and_drain_turn_ignores_prior_turn_completion() {
        let (review_session, tx_event, _rx_sub) = test_review_session().await;
        tx_event
            .send(turn_complete_event("prior-turn", Some("stale"), Some(9)))
            .await
            .expect("queue prior turn completion");
        tx_event
            .send(turn_aborted_event("current-turn"))
            .await
            .expect("queue current turn abort");

        interrupt_and_drain_turn(&review_session.codex, "current-turn")
            .await
            .expect("drain current turn");

        assert!(review_session.codex.rx_event.try_recv().is_err());
    }
}
