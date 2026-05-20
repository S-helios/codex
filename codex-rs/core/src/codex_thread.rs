//! 单条会话线程（thread）的门面层。
//!
//! 【架构位置】
//! ```text
//! ThreadManager           ← 管理所有线程的生命周期（thread_manager.rs）
//!   └─ CodexThread        ← 本文件：单条线程的门面，持有 `Codex` 句柄
//!         └─ Codex        ← 会话入口（session/mod.rs）
//!               └─ Session ← 核心业务逻辑
//!                     └─ Turn ← 单次用户 ↔ 模型交互
//! ```
//!
//! 【职责】
//! - 向 app-server / agent 暴露线程级 API（`submit`、`next_event`、配置快照…）
//! - 管理「带外引导（out-of-band elicitation）」的暂停 / 恢复计数
//! - 提供对 session 内部状态的窄接口只读快照
//!
//! 【阅读建议】先看 `CodexThread` 的 `submit` / `next_event` / `steer_input` 三个
//! 核心通道方法，再看 `inject_*` 系列的消息注入接口（用于 fork / replay），
//! 最后看 `*_out_of_band_elicitation_count` 这对的暂停语义。

use crate::agent::AgentStatus;
use crate::config::ConstraintResult;
use crate::goals::ExternalGoalSet;
use crate::goals::GoalRuntimeEvent;
use crate::session::Codex;
use crate::session::SessionSettingsUpdate;
use crate::session::SteerInputError;
use codex_features::Feature;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::user_input::UserInput;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadMetadataPatch;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreResult;
use codex_utils_absolute_path::AbsolutePathBuf;
use rmcp::model::ReadResourceRequestParams;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

use codex_rollout::state_db::StateDbHandle;

// ═══════════════════════════════════════════════════════════════
// ThreadConfigSnapshot  ·  线程配置的不可变快照
// ═══════════════════════════════════════════════════════════════

/// 某一时刻线程配置的只读快照，供 app-server 在不持有 Session 锁的前提下
/// 查询当前配置。
///
/// 与 `Config` 的关系：`Config` 是完整的初始配置对象；本快照只包含
/// app-server 真正需要展示 / 校验的字段子集，并已解析了
/// `active_permission_profile`。
#[derive(Clone, Debug)]
pub struct ThreadConfigSnapshot {
    pub model: String,
    pub model_provider_id: String,
    pub service_tier: Option<String>,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub permission_profile: PermissionProfile,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub cwd: AbsolutePathBuf,
    pub workspace_roots: Vec<AbsolutePathBuf>,
    pub profile_workspace_roots: Vec<AbsolutePathBuf>,
    /// 是否为「临时线程」：true 时不持久化到 thread store。
    pub ephemeral: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_summary: Option<ReasoningSummary>,
    pub personality: Option<Personality>,
    pub collaboration_mode: CollaborationMode,
    /// 线程来源（Exec / WebApp / SubAgent …），影响权限与日志归类。
    pub session_source: SessionSource,
    /// fork / resume 场景下指向父线程；首条线程为 None。
    pub thread_source: Option<ThreadSource>,
}

impl ThreadConfigSnapshot {
    /// 根据当前权限 profile 推导出对应的沙箱策略。
    ///
    /// 沙箱策略由「文件系统沙箱策略」+「网络沙箱策略」+ cwd 三者共同决定。
    /// 这里封装了 `codex_sandboxing` 模块的计算细节，调用方无需了解。
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        let file_system_sandbox_policy = self.permission_profile.file_system_sandbox_policy();
        codex_sandboxing::compatibility_sandbox_policy_for_permission_profile(
            &self.permission_profile,
            &file_system_sandbox_policy,
            self.permission_profile.network_sandbox_policy(),
            self.cwd.as_path(),
        )
    }
}

// ═══════════════════════════════════════════════════════════════
// CodexThreadSettingsOverrides  ·  线程设置的覆盖 DTO
// ═══════════════════════════════════════════════════════════════

/// Thread settings overrides that app-server validates before starting a turn.
/// app-server 在启动新 Turn 前传入的可选覆盖项，先经校验、不立即生效。
///
/// 所有字段均为 `Option`：`None` 表示沿用线程级默认值，`Some` 表示本次覆盖。
///
/// 注意 `effort` / `service_tier` 是 `Option<Option<_>>` 的双层 Option：
///   - 外层 `None`        → 不覆盖
///   - 外层 `Some(None)`  → 显式「清除」该设置，回退到模型 / 系统默认
///   - 外层 `Some(Some(v))` → 覆盖为指定值 v
#[derive(Clone, Default)]
pub struct CodexThreadSettingsOverrides {
    pub cwd: Option<PathBuf>,
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub permission_profile: Option<PermissionProfile>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub model: Option<String>,
    pub effort: Option<Option<ReasoningEffort>>,
    pub summary: Option<ReasoningSummary>,
    pub service_tier: Option<Option<String>>,
    pub collaboration_mode: Option<CollaborationMode>,
    pub personality: Option<Personality>,
}

// ═══════════════════════════════════════════════════════════════
// CodexThread  ·  单条线程的门面结构体
// ═══════════════════════════════════════════════════════════════

pub struct CodexThread {
    pub(crate) codex: Codex,
    pub(crate) session_source: SessionSource,
    session_configured: SessionConfiguredEvent,
    rollout_path: Option<PathBuf>,
    /// 带外引导（MCP elicitation）的并发计数。
    /// 计数 > 0 时通知 Session 暂停新 Turn，归零后才恢复；用异步 Mutex 是因为
    /// 持锁期间需 await session 方法（同步 Mutex 在 await 点会编译失败）。
    out_of_band_elicitation_count: Mutex<u64>,
}

/// Conduit for the bidirectional stream of messages that compose a thread
/// (formerly called a conversation) in Codex.
/// 单条 Codex 线程（旧称 conversation）的双向消息流管道。
///
/// 【生命周期】
///   1. 由 `ThreadManager` 在 spawn 完 `Codex` 后调用 `new()` 创建。
///   2. 存入 `ThreadManagerState::threads`（`HashMap<ThreadId, Arc<CodexThread>>`）。
///   3. 上层通过 `submit(op)` 发送操作，通过 `next_event()` 接收事件。
///   4. `shutdown_and_wait()` 或 Drop 触发 Session 主循环退出。
impl CodexThread {
    // ───────────────────────────────────────────────────────────────
    // 构造
    // ───────────────────────────────────────────────────────────────

    pub(crate) fn new(
        codex: Codex,
        session_configured: SessionConfiguredEvent,
        rollout_path: Option<PathBuf>,
        session_source: SessionSource,
    ) -> Self {
        Self {
            codex,
            session_source,
            session_configured,
            rollout_path,
            out_of_band_elicitation_count: Mutex::new(0),
        }
    }

    // ───────────────────────────────────────────────────────────────
    // 核心 Submit / Event 通道
    // ───────────────────────────────────────────────────────────────

    /// 向 Session 投递一个 `Op`（用户 / agent 的意图载体，如
    /// `Op::UserTurnInput`、`Op::Interrupt`、`Op::Shutdown`）。
    /// 返回提交 ID，用于将后续事件关联回本次提交。
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        self.codex.submit(op).await
    }

    /// Returns the session telemetry handle for thread-scoped production instrumentation.
    /// 返回线程作用域的生产环境遥测句柄；由 app-server 用于打点埋点。
    pub fn session_telemetry(&self) -> SessionTelemetry {
        self.codex.session.services.session_telemetry.clone()
    }

    /// 发送 Shutdown Op 并等待 Session 主循环完全终止。
    pub async fn shutdown_and_wait(&self) -> CodexResult<()> {
        self.codex.shutdown_and_wait().await
    }

    /// Wait until the underlying session loop has terminated.
    /// 不主动发送 Shutdown，仅等待 Session 主循环自然终止。
    /// 内部 await 的是一个 `Shared<JoinHandle>`，可被多处并发等待。
    pub async fn wait_until_terminated(&self) {
        self.codex.session_loop_termination.clone().await;
    }

    // ───────────────────────────────────────────────────────────────
    // Goal（目标）运行时
    // ───────────────────────────────────────────────────────────────
    // Goal 是跨 Turn 持续存在的意图表示，由 `GoalRuntimeEvent` 驱动状态机。
    // 下面这组方法都是把外部触发器翻译成 `GoalRuntimeEvent` 投递给 session。

    pub(crate) async fn emit_thread_resume_lifecycle(&self) {
        for contributor in self
            .codex
            .session
            .services
            .extensions
            .thread_lifecycle_contributors()
        {
            contributor
                .on_thread_resume(codex_extension_api::ThreadResumeInput {
                    session_store: &self.codex.session.services.session_extension_data,
                    thread_store: &self.codex.session.services.thread_extension_data,
                })
                .await;
        }
    }

    /// 线程恢复（resume）后应用 Goal 的运行时效果：
    /// 触发 `ThreadResumed` 事件，由状态机决定是否自动续跑中断的任务。
    pub async fn apply_goal_resume_runtime_effects(&self) -> anyhow::Result<()> {
        self.codex
            .session
            .goal_runtime_apply(GoalRuntimeEvent::ThreadResumed)
            .await
    }

    /// 若线程空闲则推进活跃 Goal；app-server 在检测空闲时轮询本方法，
    /// 防止 Goal 因无新输入而卡死。
    pub async fn continue_active_goal_if_idle(&self) -> anyhow::Result<()> {
        self.codex
            .session
            .goal_runtime_apply(GoalRuntimeEvent::MaybeContinueIfIdle)
            .await
    }

    /// 通知 Goal 运行时「外部即将批量修改目标集合」，使其进入准备状态，
    /// 避免在修改中途做出错误决策（如提前触发新 Turn）。
    pub async fn prepare_external_goal_mutation(&self) {
        if let Err(err) = self
            .codex
            .session
            .goal_runtime_apply(GoalRuntimeEvent::ExternalMutationStarting)
            .await
        {
            tracing::warn!("failed to prepare external goal mutation: {err}");
        }
    }

    /// 由 app-server 注入外部构造的目标列表，触发 `ExternalSet` 事件。
    pub async fn apply_external_goal_set(&self, external_set: ExternalGoalSet) {
        if let Err(err) = self
            .codex
            .session
            .goal_runtime_apply(GoalRuntimeEvent::ExternalSet { external_set })
            .await
        {
            tracing::warn!("failed to apply external goal status runtime effects: {err}");
        }
    }

    /// 清空所有外部 Goal，触发 `ExternalClear` 事件。
    pub async fn apply_external_goal_clear(&self) {
        if let Err(err) = self
            .codex
            .session
            .goal_runtime_apply(GoalRuntimeEvent::ExternalClear)
            .await
        {
            tracing::warn!("failed to apply external goal clear runtime effects: {err}");
        }
    }

    // ───────────────────────────────────────────────────────────────
    // Rollout（对话历史回放文件）维护
    // ───────────────────────────────────────────────────────────────
    // Rollout = 对话历史的持久化记录，用于 fork / resume / replay；
    // `#[doc(hidden)]` 表示这两个方法是测试 / 诊断用途，不属于稳定公开 API。

    #[doc(hidden)]
    pub async fn ensure_rollout_materialized(&self) {
        self.codex.session.ensure_rollout_materialized().await;
    }

    #[doc(hidden)]
    pub async fn flush_rollout(&self) -> std::io::Result<()> {
        self.codex.session.flush_rollout().await
    }

    /// `submit` 的扩展版：附加 W3C TraceContext，用于分布式链路追踪。
    /// trace 头会被注入到本次 Turn 发往模型 API 的 HTTP 请求中。
    pub async fn submit_with_trace(
        &self,
        op: Op,
        trace: Option<W3cTraceContext>,
    ) -> CodexResult<String> {
        self.codex.submit_with_trace(op, trace).await
    }

    /// Persist whether this thread is eligible for future memory generation.
    /// 持久化「本线程是否参与事后的记忆提炼」标志。
    /// 例如临时调试线程可设为 `Disabled`，避免污染用户的长期记忆库。
    pub async fn set_thread_memory_mode(&self, mode: ThreadMemoryMode) -> anyhow::Result<()> {
        self.codex.set_thread_memory_mode(mode).await
    }

    /// 在「当前 Turn 仍在运行期间」向模型注入额外的用户输入。
    ///
    /// 与 `submit(Op::UserTurnInput)` 不同：本方法不开新 Turn，而是把
    /// `UserInput` 追加到当前 Turn 的上下文，引导模型调整输出方向。
    ///
    /// - `expected_turn_id`：乐观并发控制，若与当前 Turn ID 不符则拒绝；
    /// - `responsesapi_client_metadata`：透传给 Responses API 的元数据。
    pub async fn steer_input(
        &self,
        input: Vec<UserInput>,
        expected_turn_id: Option<&str>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
    ) -> Result<String, SteerInputError> {
        self.codex
            .steer_input(input, expected_turn_id, responsesapi_client_metadata)
            .await
    }

    /// 记录连接到本线程的 app-server 客户端信息。
    ///
    /// `mcp_elicitations_auto_deny` 为 true 时，所有 MCP elicitation 自动拒绝；
    /// 适用于 CI / 自动化等无人值守场景，避免线程因等用户响应而永久阻塞。
    pub async fn set_app_server_client_info(
        &self,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        mcp_elicitations_auto_deny: bool,
    ) -> ConstraintResult<()> {
        self.codex
            .set_app_server_client_info(
                app_server_client_name,
                app_server_client_version,
                mcp_elicitations_auto_deny,
            )
            .await
    }

    /// Preview persistent thread settings overrides without committing them.
    /// 预览线程级设置覆盖（只校验、不真正写入）。
    ///
    /// app-server 在用户编辑设置时调用，用于即时反馈合法性；校验失败时返回
    /// `ConstraintError`，前端可直接展示给用户。
    pub async fn preview_thread_settings_overrides(
        &self,
        overrides: CodexThreadSettingsOverrides,
    ) -> ConstraintResult<ThreadConfigSnapshot> {
        let updates = self.thread_settings_update(overrides).await;
        self.codex.session.preview_settings(&updates).await
    }

    /// 内部辅助：将 `CodexThreadSettingsOverrides` 翻译为 `SessionSettingsUpdate`。
    ///
    /// 关键逻辑——`collaboration_mode` 的回退计算：调用方未显式传入时，从当前
    /// session 的协作模式出发、叠加 model / effort 的覆盖再产出新值，避免单独
    /// 改 model 或 effort 时意外重置其他协作模式字段。
    async fn thread_settings_update(
        &self,
        overrides: CodexThreadSettingsOverrides,
    ) -> SessionSettingsUpdate {
        let CodexThreadSettingsOverrides {
            cwd,
            workspace_roots,
            profile_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        } = overrides;
        let collaboration_mode = if let Some(collaboration_mode) = collaboration_mode {
            collaboration_mode
        } else {
            self.codex
                .session
                .collaboration_mode()
                .await
                .with_updates(model, effort, /*developer_instructions*/ None)
        };

        SessionSettingsUpdate {
            cwd,
            workspace_roots,
            profile_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level,
            collaboration_mode: Some(collaboration_mode),
            reasoning_summary: summary,
            service_tier,
            personality,
            ..Default::default()
        }
    }

    /// Use sparingly: this is intended to be removed soon.
    /// 慎用：临时过渡接口，预计未来会被移除。
    /// 使用预构造的 `Submission`（含自定义 ID）直接提交，绕过 UUID 自动生成。
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.codex.submit_with_id(sub).await
    }

    /// 从事件队列取下一个事件，无事件时异步等待。
    /// 事件类型涵盖 `SessionConfigured` / `TurnStarted` / `OutputItem` /
    /// `TurnComplete` / `TurnAborted` / `Error` 等。
    pub async fn next_event(&self) -> CodexResult<Event> {
        self.codex.next_event().await
    }

    pub async fn agent_status(&self) -> AgentStatus {
        self.codex.agent_status().await
    }

    /// 订阅 Agent 状态变化的 watch channel。多消费者，每次状态变化后立刻
    /// 推送最新值；app-server 用它把状态转发到前端 WebSocket。
    pub(crate) fn subscribe_status(&self) -> watch::Receiver<AgentStatus> {
        self.codex.agent_status.clone()
    }

    /// Returns the complete token usage snapshot currently cached for this thread.
    ///
    /// This accessor is intentionally narrower than direct session access: it lets
    /// app-server lifecycle paths replay restored usage after resume or fork without
    /// exposing broader session mutation authority. A caller that only reads
    /// `total_token_usage` would drop last-turn usage and make the v2
    /// `thread/tokenUsage/updated` payload incomplete.
    /// 返回本线程当前缓存的 token 用量完整快照（含累计 + 最近一次 Turn）。
    ///
    /// 设计意图：比直接访问 Session 更窄的接口——只能读用量，不能改 Session
    /// 状态。这让 app-server 在 resume / fork 后可安全地重放历史用量，同时
    /// 确保 `thread/tokenUsage/updated` 事件包含完整数据（只读 `total_token_usage`
    /// 会丢失最近一轮的用量，导致 payload 不完整）。
    pub async fn token_usage_info(&self) -> Option<TokenUsageInfo> {
        self.codex.session.token_usage_info().await
    }

    // ───────────────────────────────────────────────────────────────
    // 消息注入（内部 / 测试用）
    // ───────────────────────────────────────────────────────────────

    /// Records a user-role session-prefix message without creating a new user turn boundary.
    /// 将一条 user 角色消息注入 session 前缀，**不**创建新的 Turn 边界。
    ///
    /// 与 `submit(Op::UserTurnInput)` 的区别：本方法注入的消息对模型可见，
    /// 但不触发 `TurnStarted` 事件、不计入 Turn 计数。
    /// 典型用途：第一个真正 Turn 开始前预填充上下文（session-prefix 注入）。
    pub(crate) async fn inject_user_message_without_turn(&self, message: String) {
        let message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: message }],
            phase: None,
        };
        let pending_item = match pending_message_input_item(&message) {
            Ok(pending_item) => pending_item,
            Err(err) => {
                // `debug_assert!` 在 debug 构建下 panic、release 构建静默跳过——
                // 这里相当于「预期 100% 成功，但万一失败也不要崩生产环境」。
                debug_assert!(false, "session-prefix message append should succeed: {err}");
                return;
            }
        };
        // 主路径：注入到当前活跃 Turn 的 pending items。
        // 降级路径：无活跃 Turn 时退回到 `record_conversation_items`，
        // 确保消息不丢失。
        if self
            .codex
            .session
            .inject_response_items(vec![pending_item])
            .await
            .is_err()
        {
            let turn_context = self.codex.session.new_default_turn().await;
            self.codex
                .session
                .record_conversation_items(turn_context.as_ref(), &[message])
                .await;
        }
    }

    /// Append a prebuilt message to the thread history without treating it as a user turn.
    ///
    /// If the thread already has an active turn, the message is queued as pending input for that
    /// turn. Otherwise it is queued at session scope and a regular turn is started so the agent
    /// can consume that pending input through the normal turn pipeline.
    /// 将预构造的消息追加到线程历史，**不**当作用户 Turn 处理（仅测试用）。
    ///
    /// 两条路径：
    /// 1. 有活跃 Turn → 直接挂到当前 Turn 的 pending input；
    /// 2. 无活跃 Turn → 排队到 session 级队列，再尝试触发新 Turn 来消费它。
    #[cfg(test)]
    pub(crate) async fn append_message(&self, message: ResponseItem) -> CodexResult<String> {
        let submission_id = uuid::Uuid::new_v4().to_string();
        let pending_item = pending_message_input_item(&message)?;
        if let Err(items) = self
            .codex
            .session
            .inject_response_items(vec![pending_item])
            .await
        {
            // 降级：当前无活跃 Turn，先把消息排到下一个 Turn 的输入队列，
            // 再让 session 「在空闲时启动新 Turn」消费这批 pending 输入。
            self.codex
                .session
                .input_queue
                .queue_response_items_for_next_turn(items)
                .await;
            self.codex.session.maybe_start_turn_for_pending_work().await;
        }

        Ok(submission_id)
    }

    /// Append raw Responses API items to the thread's model-visible history.
    /// 向线程的「模型可见历史」追加一批 Responses API items，绕过 Turn 流程。
    ///
    /// 主要用途：将外部产物（如 Guardian Review）直接写入对话历史，使后续
    /// Turn 能看到这些内容，但不触发事件、不占用 Turn 配额。
    ///
    /// 步骤：
    ///   1. items 不能为空（否则 `InvalidRequest`）；
    ///   2. 拿到默认 turn_context 作为写入归属；
    ///   3. 若尚无 reference context item，先记录一个作为后续 diff 的基准锚点；
    ///   4. 写入历史；
    ///   5. 强制 flush rollout，确保磁盘与内存一致。
    pub async fn inject_response_items(&self, items: Vec<ResponseItem>) -> CodexResult<()> {
        if items.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "items must not be empty".to_string(),
            ));
        }

        let turn_context = self.codex.session.new_default_turn().await;
        if self.codex.session.reference_context_item().await.is_none() {
            self.codex
                .session
                .record_context_updates_and_set_reference_context_item(turn_context.as_ref())
                .await;
        }
        self.codex
            .session
            .record_conversation_items(turn_context.as_ref(), &items)
            .await;
        self.codex.session.flush_rollout().await?;
        Ok(())
    }

    // ───────────────────────────────────────────────────────────────
    // 只读属性访问器
    // ───────────────────────────────────────────────────────────────

    pub fn rollout_path(&self) -> Option<PathBuf> {
        self.rollout_path.clone()
    }

    pub fn session_configured(&self) -> SessionConfiguredEvent {
        self.session_configured.clone()
    }

    /// 线程是否仍在运行：底层判断 `tx_sub` 通道是否仍开放。
    /// `tx_sub.is_closed()` 为 true ⇒ Session 主循环已退出，后续 submit 必败。
    pub(crate) fn is_running(&self) -> bool {
        !self.codex.tx_sub.is_closed()
    }

    pub async fn guardian_trunk_rollout_path(&self) -> Option<PathBuf> {
        self.codex
            .session
            .guardian_review_session
            .trunk_rollout_path()
            .await
    }

    // ───────────────────────────────────────────────────────────────
    // 持久化：thread store 读取
    // ───────────────────────────────────────────────────────────────
    // 下面 3 个方法都先通过 `live_thread_for_persistence(<操作名>)` 拿到持久化
    // 层句柄，操作名仅作为出错时的诊断标签，方便定位是哪条调用路径失败。

    pub async fn load_history(
        &self,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        let live_thread = self
            .codex
            .session
            .live_thread_for_persistence("load history")
            .map_err(|err| ThreadStoreError::Internal {
                message: err.to_string(),
            })?;
        live_thread.load_history(include_archived).await
    }

    pub async fn read_thread(
        &self,
        include_archived: bool,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        let live_thread = self
            .codex
            .session
            .live_thread_for_persistence("read thread")
            .map_err(|err| ThreadStoreError::Internal {
                message: err.to_string(),
            })?;
        live_thread
            .read_thread(include_archived, include_history)
            .await
    }

    pub async fn update_thread_metadata(
        &self,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThread> {
        let live_thread = self
            .codex
            .session
            .live_thread_for_persistence("update thread metadata")
            .map_err(|err| ThreadStoreError::Internal {
                message: err.to_string(),
            })?;
        live_thread.update_metadata(patch, include_archived).await
    }

    // ───────────────────────────────────────────────────────────────
    // 状态数据库 / 配置访问
    // ───────────────────────────────────────────────────────────────

    pub fn state_db(&self) -> Option<StateDbHandle> {
        self.codex.state_db()
    }

    pub async fn config_snapshot(&self) -> ThreadConfigSnapshot {
        self.codex.thread_config_snapshot().await
    }

    pub async fn config(&self) -> Arc<crate::config::Config> {
        self.codex.session.get_config().await
    }

    /// Refresh the thread's layer-backed user config state from a caller-supplied
    /// config snapshot. Thread-scoped layers and session-static settings remain
    /// unchanged.
    /// 用调用方提供的 Config 快照刷新线程的「层叠用户配置」状态。
    /// 注意：线程作用域的 layer 和 session 级静态设置不会被改写——只刷新
    /// 用户态那一层。
    pub async fn refresh_runtime_config(&self, next_config: crate::config::Config) {
        self.codex.session.refresh_runtime_config(next_config).await;
    }

    pub async fn environment_selections(&self) -> Vec<TurnEnvironmentSelection> {
        self.codex.thread_environment_selections().await
    }

    // ───────────────────────────────────────────────────────────────
    // MCP 工具调用
    // ───────────────────────────────────────────────────────────────

    /// 读取指定 MCP 服务器上的资源，返回 JSON 值。
    /// `server` 对应 config 的 `mcp_servers` 配置项；`uri` 的格式由具体 MCP
    /// 服务器自行定义。
    pub async fn read_mcp_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let result = self
            .codex
            .session
            .read_resource(
                server,
                ReadResourceRequestParams {
                    meta: None,
                    uri: uri.to_string(),
                },
            )
            .await?;

        Ok(serde_json::to_value(result)?)
    }

    /// 调用指定 MCP 服务器上的工具。
    /// - `tool`：MCP 服务器声明的工具名；
    /// - `arguments`：符合该工具 input schema 的 JSON 对象；
    /// - `meta`：可选的 MCP 协议元数据。
    pub async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.codex
            .session
            .call_tool(server, tool, arguments, meta)
            .await
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.codex.enabled(feature)
    }

    // ───────────────────────────────────────────────────────────────
    // 带外引导（Out-of-Band Elicitation）暂停计数
    // ───────────────────────────────────────────────────────────────
    //
    // 背景：MCP 协议支持在 Turn 运行中途发起「引导」（elicitation）——即向用户
    // 请求额外输入。多个引导可能并发进行，需要用计数器追踪「未完成引导数」：
    //   计数 > 0 → Session 暂停，阻止新 Turn 开始（避免竞态）
    //   计数 == 0 → Session 恢复正常运行
    //
    // 两个方法成对使用：每次发起引导前 increment，引导完成后 decrement。

    /// 原子地 +1；若从 0 升到 1，第一次进入暂停状态时通知 Session。
    /// 用 `checked_add` 防溢出（实际不可能发生，但满足类型系统的防御性要求）。
    pub async fn increment_out_of_band_elicitation_count(&self) -> CodexResult<u64> {
        let mut guard = self.out_of_band_elicitation_count.lock().await;
        let was_zero = *guard == 0;
        *guard = guard.checked_add(1).ok_or_else(|| {
            CodexErr::Fatal("out-of-band elicitation count overflowed".to_string())
        })?;

        if was_zero {
            self.codex
                .session
                .set_out_of_band_elicitation_pause_state(/*paused*/ true);
        }

        Ok(*guard)
    }

    /// 原子地 -1；若归零则通知 Session 解除暂停。
    /// 若调用时计数已为 0 返回 `InvalidRequest`——这是防御性检查，正常调用
    /// 路径下不应出现 decrement 多于 increment 的情况。
    pub async fn decrement_out_of_band_elicitation_count(&self) -> CodexResult<u64> {
        let mut guard = self.out_of_band_elicitation_count.lock().await;
        if *guard == 0 {
            return Err(CodexErr::InvalidRequest(
                "out-of-band elicitation count is already zero".to_string(),
            ));
        }

        *guard -= 1;
        let now_zero = *guard == 0;
        if now_zero {
            self.codex
                .session
                .set_out_of_band_elicitation_pause_state(/*paused*/ false);
        }

        Ok(*guard)
    }
}

// ═══════════════════════════════════════════════════════════════
// 内部辅助函数
// ═══════════════════════════════════════════════════════════════

/// 将 `ResponseItem::Message` 转换为 pending 状态的 `ResponseInputItem::Message`。
///
/// 设计点：
/// - `ResponseItem` 是模型输出 / 历史记录的消息格式（含 `id`）；
/// - `ResponseInputItem` 是即将注入下一次 API 请求的输入格式（无 `id`）；
/// - 只处理 `Message` 变体，其他变体（ToolCall / ToolResult / Reasoning…）
///   在 session-prefix 注入场景下不合法，直接返回 `InvalidRequest`。
///
/// 为何不实现 `From` trait：`ResponseItem` 有多个变体，并非所有变体都能
/// 无损转换；用返回 `CodexResult` 的函数比 `From` 更安全。
fn pending_message_input_item(message: &ResponseItem) -> CodexResult<ResponseInputItem> {
    match message {
        ResponseItem::Message {
            role,
            content,
            phase,
            ..
        } => Ok(ResponseInputItem::Message {
            role: role.clone(),
            content: content.clone(),
            phase: phase.clone(),
        }),
        _ => Err(CodexErr::InvalidRequest(
            "append_message only supports ResponseItem::Message".to_string(),
        )),
    }
}
