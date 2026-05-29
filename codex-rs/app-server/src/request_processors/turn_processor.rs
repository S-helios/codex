//! 【文件职责】处理「回合（turn）级」的 JSON-RPC 请求：把客户端发来的
//! `turn/start`、`turn/steer`、`turn/interrupt`、`review/start`、实时对话
//! （realtime）、`thread/settings/update` 等请求，翻译成内核 `Op` 并投递给
//! 对应的 `CodexThread`。
//!
//! 【架构位置】
//!   层级：App-Server 请求处理层（位于 JSON-RPC 协议层与 codex-core 内核之间）
//!   上游：`request_processors.rs` 的分发逻辑（按方法名路由到本处理器）
//!   下游：`ThreadManager` / `CodexThread`（提交 `Op`、读取配置快照）、
//!         `thread_lifecycle`（挂载监听器）、`OutgoingMessageSender`（回发响应/通知）
//!
//! 【数据流】
//!   JSON-RPC 请求参数 → `*_inner()` 校验 + 翻译 → `Op::{UserInput,Review,...}`
//!   → `submit_core_op()` 提交内核 → 返回 turn_id 包装成 `Turn` 响应
//!
//! 【与 thread_processor 的分工】本文件只管「在已存在的线程上推进回合」；
//!   线程的创建/恢复/归档/列举等生命周期操作在 `thread_processor.rs`。
//!   两者通过 `use super::*` 共享 `request_processors.rs` 顶部的全部导入。
//!
//! 【阅读建议】先看 `turn_start_inner()`（最核心的回合启动流程：校验输入 →
//!   映射 input items → 构建 thread settings 覆盖 → 提交 `Op::UserInput`），
//!   再看 `build_thread_settings_overrides()`（逐个请求字段翻译成内核覆盖项，
//!   并做 preview 校验）。`review_start` 的内联/分离两种交付方式可按需阅读。

use super::*;
use codex_protocol::protocol::AdditionalContextEntry as CoreAdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind as CoreAdditionalContextKind;

/// 回合级请求处理器：持有推进回合所需的全部共享句柄（线程管理器、配置、
/// 出站消息发送器、各类状态管理器等），所有 `turn/*` 与部分 `thread/*`
/// 方法都挂在它的 `impl` 上。`Clone` 因为内部字段全是 `Arc`/可廉价克隆的句柄，
/// 分发层会按连接克隆出处理器实例。
#[derive(Clone)]
pub(crate) struct TurnRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    analytics_events_client: AnalyticsEventsClient,
    arg0_paths: Arg0DispatchPaths,
    config: Arc<Config>,
    config_manager: ConfigManager,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<Semaphore>,
    skills_watcher: Arc<SkillsWatcher>,
}

// 将客户端给的（可能是相对的）workspace 根路径列表，逐个解析为绝对路径并去重。
// base_cwd 作为相对路径的解析基准；去重保证同一根目录不会重复进入沙箱可写集合。
fn resolve_runtime_workspace_roots(
    workspace_roots: Vec<PathBuf>,
    base_cwd: &AbsolutePathBuf,
) -> Vec<AbsolutePathBuf> {
    let mut resolved_roots = Vec::new();
    for path in workspace_roots {
        let root = AbsolutePathBuf::resolve_path_against_base(path, base_cwd.as_path());
        if !resolved_roots.iter().any(|existing| existing == &root) {
            resolved_roots.push(root);
        }
    }
    resolved_roots
}

// 把 API 层的「附加上下文」映射成内核类型。两层各有一套 enum（API 的
// `AdditionalContextKind` vs 内核的 `CoreAdditionalContextKind`），此处逐项翻译。
// 用 `BTreeMap` 而非 `HashMap` 是为了让 key 顺序稳定（影响 prompt 拼装的确定性）。
fn map_additional_context(
    additional_context: Option<HashMap<String, AdditionalContextEntry>>,
) -> BTreeMap<String, CoreAdditionalContextEntry> {
    additional_context
        .unwrap_or_default()
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                CoreAdditionalContextEntry {
                    value: entry.value,
                    kind: match entry.kind {
                        AdditionalContextKind::Untrusted => CoreAdditionalContextKind::Untrusted,
                        AdditionalContextKind::Application => {
                            CoreAdditionalContextKind::Application
                        }
                    },
                },
            )
        })
        .collect()
}

/// `build_thread_settings_overrides()` 的入参聚合体。`turn/start` 与
/// `thread/settings/update` 两条路径都会构造它，把「本次请求想覆盖的线程设置」
/// （工作目录、审批策略、沙箱、权限、模型、推理力度、协作模式、人格等）打包传入。
/// `method` 仅用于出错时拼可读的错误信息（区分是哪个 RPC 触发的）。
struct ThreadSettingsBuildParams {
    method: &'static str,
    cwd: Option<PathBuf>,
    runtime_workspace_roots: Option<Vec<PathBuf>>,
    approval_policy: Option<codex_app_server_protocol::AskForApproval>,
    approvals_reviewer: Option<codex_app_server_protocol::ApprovalsReviewer>,
    sandbox_policy: Option<codex_app_server_protocol::SandboxPolicy>,
    permissions: Option<String>,
    model: Option<String>,
    service_tier: Option<Option<String>>,
    effort: Option<ReasoningEffort>,
    summary: Option<ReasoningSummary>,
    collaboration_mode: Option<CollaborationMode>,
    personality: Option<Personality>,
}

impl TurnRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        analytics_events_client: AnalyticsEventsClient,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        config_manager: ConfigManager,
        pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_list_state_permit: Arc<Semaphore>,
        skills_watcher: Arc<SkillsWatcher>,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            analytics_events_client,
            arg0_paths,
            config,
            config_manager,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
        }
    }

    pub(crate) async fn turn_start(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.turn_start_inner(
            request_id,
            params,
            app_server_client_name,
            app_server_client_version,
        )
        .await
        .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_inject_items(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_inject_items_response_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_settings_update(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_settings_update_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_steer(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.turn_steer_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_interrupt(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.turn_interrupt_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_start_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_audio(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_audio_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_text(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_text_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_stop(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_stop_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_list_voices(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Ok(Some(
            ThreadRealtimeListVoicesResponse {
                voices: RealtimeVoicesList::builtin(),
            }
            .into(),
        ))
    }

    pub(crate) async fn review_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ReviewStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.review_start_inner(request_id, params)
            .await
            .map(|()| None)
    }

    fn track_error_response(
        &self,
        request_id: &ConnectionRequestId,
        error: &JSONRPCErrorError,
        error_type: Option<AnalyticsJsonRpcError>,
    ) {
        self.analytics_events_client.track_error_response(
            request_id.connection_id.0,
            request_id.request_id.clone(),
            error.clone(),
            error_type,
        );
    }

    async fn load_thread(
        &self,
        thread_id: &str,
    ) -> Result<(ThreadId, Arc<CodexThread>), JSONRPCErrorError> {
        // Resolve the core conversation handle from a v2 thread id string.
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;

        Ok((thread_id, thread))
    }
    // 规整协作模式：若客户端没显式给 developer_instructions，则从内置预设里按
    // mode 匹配出一份默认指令补上（预设里为空的指令不采用）。保证切换协作模式时
    // 即使客户端省略了指令，也能带上该模式应有的开发者指令。
    fn normalize_collaboration_mode(
        &self,
        mut collaboration_mode: CollaborationMode,
    ) -> CollaborationMode {
        if collaboration_mode.settings.developer_instructions.is_none()
            && let Some(instructions) = builtin_collaboration_mode_presets()
                .into_iter()
                .find(|preset| preset.mode == Some(collaboration_mode.mode))
                .and_then(|preset| preset.developer_instructions.flatten())
                .filter(|instructions| !instructions.is_empty())
        {
            collaboration_mode.settings.developer_instructions = Some(instructions);
        }

        collaboration_mode
    }

    /// 把 API 层的评审目标 `ApiReviewTarget` 校验并翻译为内核 `ReviewRequest`，
    /// 同时产出一段「面向用户的提示文案」（hint，用作回合首条用户消息的展示文本）。
    /// 校验包括：分支名/commit sha/自定义指令去空白后不得为空。
    fn review_request_from_target(
        target: ApiReviewTarget,
    ) -> Result<(ReviewRequest, String), JSONRPCErrorError> {
        let cleaned_target = match target {
            ApiReviewTarget::UncommittedChanges => ApiReviewTarget::UncommittedChanges,
            ApiReviewTarget::BaseBranch { branch } => {
                let branch = branch.trim().to_string();
                if branch.is_empty() {
                    return Err(invalid_request("branch must not be empty".to_string()));
                }
                ApiReviewTarget::BaseBranch { branch }
            }
            ApiReviewTarget::Commit { sha, title } => {
                let sha = sha.trim().to_string();
                if sha.is_empty() {
                    return Err(invalid_request("sha must not be empty".to_string()));
                }
                let title = title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty());
                ApiReviewTarget::Commit { sha, title }
            }
            ApiReviewTarget::Custom { instructions } => {
                let trimmed = instructions.trim().to_string();
                if trimmed.is_empty() {
                    return Err(invalid_request(
                        "instructions must not be empty".to_string(),
                    ));
                }
                ApiReviewTarget::Custom {
                    instructions: trimmed,
                }
            }
        };

        let core_target = match cleaned_target {
            ApiReviewTarget::UncommittedChanges => CoreReviewTarget::UncommittedChanges,
            ApiReviewTarget::BaseBranch { branch } => CoreReviewTarget::BaseBranch { branch },
            ApiReviewTarget::Commit { sha, title } => CoreReviewTarget::Commit { sha, title },
            ApiReviewTarget::Custom { instructions } => CoreReviewTarget::Custom { instructions },
        };

        let hint = codex_core::review_prompts::user_facing_hint(&core_target);
        let review_request = ReviewRequest {
            target: core_target,
            user_facing_hint: Some(hint.clone()),
        };

        Ok((review_request, hint))
    }

    fn parse_environment_selections(
        &self,
        environments: Option<Vec<TurnEnvironmentParams>>,
    ) -> Result<Option<Vec<TurnEnvironmentSelection>>, JSONRPCErrorError> {
        let environment_selections = environments.map(|environments| {
            environments
                .into_iter()
                .map(|environment| TurnEnvironmentSelection {
                    environment_id: environment.environment_id,
                    cwd: environment.cwd,
                })
                .collect::<Vec<_>>()
        });
        if let Some(environment_selections) = environment_selections.as_ref() {
            self.thread_manager
                .validate_environment_selections(environment_selections)
                .map_err(|err| invalid_request(environment_selection_error_message(err)))?;
        }
        Ok(environment_selections)
    }

    async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<codex_protocol::protocol::W3cTraceContext> {
        self.outgoing.request_trace_context(request_id).await
    }

    /// 向内核线程提交一个 `Op`，并附带从出站请求中取出的 W3C trace 上下文
    /// （用于跨进程链路追踪）。返回内核分配的提交 id，上层通常把它当作 turn_id。
    /// 这是本文件几乎所有「动作类」请求的统一出口。
    async fn submit_core_op(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
        op: Op,
    ) -> CodexResult<String> {
        thread
            .submit_with_trace(op, self.request_trace_context(request_id).await)
            .await
    }

    // 构造「输入过长」错误：除人类可读消息外，还在 data 里塞入机器可读的
    // 错误码与字符数上下限，方便客户端 UI 精确提示（而非只显示一句话）。
    fn input_too_large_error(actual_chars: usize) -> JSONRPCErrorError {
        let mut error = invalid_params(format!(
            "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
        ));
        error.data = Some(serde_json::json!({
            "input_error_code": INPUT_TOO_LARGE_ERROR_CODE,
            "max_chars": MAX_USER_INPUT_TEXT_CHARS,
            "actual_chars": actual_chars,
        }));
        error
    }

    fn validate_v2_input_limit(items: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
        let actual_chars: usize = items.iter().map(V2UserInput::text_char_count).sum();
        if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
            return Err(Self::input_too_large_error(actual_chars));
        }
        Ok(())
    }

    /// `turn/start` 的核心实现：在一个已加载的线程上启动新回合。
    ///
    /// 主要步骤（与函数体内顺序一致）：
    ///   1. 校验输入长度，超限则记录埋点并返回错误；
    ///   2. 按 thread_id 加载线程句柄，并写入 app-server 客户端信息；
    ///   3. 解析 environment 选择项；
    ///   4. 把 v2 输入项映射为内核 `CoreInputItem`，构建本回合的 thread settings 覆盖；
    ///   5. 组装 `Op::UserInput` 提交内核，拿到的提交 id 即为 turn_id；
    ///   6. 若本回合带输入，则异步触发「记忆（memories）」启动任务；
    ///   7. 记录 request↔turn_id 映射，返回一个初始状态为 InProgress 的 `Turn`。
    ///
    /// 副作用：提交内核 Op（驱动模型开始生成）、写客户端信息、可能拉起记忆任务、
    ///         向 outgoing 登记 request→turn 映射；失败路径都会调用 `track_error_response` 埋点。
    async fn turn_start_inner(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<TurnStartResponse, JSONRPCErrorError> {
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                &request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }
        let (thread_id, thread) =
            self.load_thread(&params.thread_id)
                .await
                .inspect_err(|error| {
                    self.track_error_response(&request_id, error, /*error_type*/ None);
                })?;
        Self::set_app_server_client_info(
            thread.as_ref(),
            app_server_client_name,
            app_server_client_version,
        )
        .await
        .inspect_err(|error| {
            self.track_error_response(&request_id, error, /*error_type*/ None);
        })?;

        let environment_selections = self.parse_environment_selections(params.environments)?;

        // Map v2 input items to core input items.
        // 把协议 v2 的用户输入项逐个转换为内核输入项（文本/图片等）。
        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let additional_context = map_additional_context(params.additional_context);
        let turn_has_input = !mapped_items.is_empty();
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "turn/start",
                    cwd: params.cwd,
                    runtime_workspace_roots: params.runtime_workspace_roots,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;

        // Start the turn by submitting the user input. Return its submission id as turn_id.
        // 通过提交用户输入来「启动回合」：内核把这次提交分配的 id 直接作为 turn_id 回传，
        // 即「一次 UserInput 提交 == 一个回合」，回合 id 不在 app-server 侧另行生成。
        let turn_op = Op::UserInput {
            items: mapped_items,
            environments: environment_selections,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
            additional_context,
            thread_settings,
        };
        let turn_id = self
            .submit_core_op(&request_id, thread.as_ref(), turn_op)
            .await
            .map_err(|err| {
                let error = internal_error(format!("failed to start turn: {err}"));
                self.track_error_response(&request_id, &error, /*error_type*/ None);
                error
            })?;

        if turn_has_input {
            let config_snapshot = thread.config_snapshot().await;
            codex_memories_write::start_memories_startup_task(
                Arc::clone(&self.thread_manager),
                Arc::clone(&self.auth_manager),
                thread_id,
                Arc::clone(&thread),
                thread.config().await,
                &config_snapshot.session_source,
            );
        }

        self.outgoing
            .record_request_turn_id(&request_id, &turn_id)
            .await;
        let turn = Turn {
            id: turn_id,
            items: vec![],
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        };

        Ok(TurnStartResponse { turn })
    }

    /// 把一次请求里携带的「线程设置覆盖」字段翻译为内核 `ThreadSettingsOverrides`。
    /// 被 `turn/start`（随回合一起覆盖）和 `thread/settings/update`（单独更新）共用。
    ///
    /// 关键点：
    ///   - `permissions` 与 `sandboxPolicy` 互斥（二选一，不能同时给）；
    ///   - 当涉及 `permissions` 或 `runtime_workspace_roots` 时，需要先拿线程的
    ///     `config_snapshot()` 作为解析基准（相对路径、回退 workspace 根等）；
    ///   - `permissions` 指定的权限档会通过 `config_manager` 重新加载求值；与启动期
    ///     不同，显式的 settings 更新若档位被策略禁用必须直接报错（不做静默回退）；
    ///   - 若存在任意覆盖项，先调用 `preview_thread_settings_overrides()` 在内核侧
    ///     做一次「预演校验」，非法组合在此处被拒，避免把坏设置真正提交进回合。
    ///
    /// 副作用：可能触发一次配置加载（磁盘/IO）与一次内核 preview 调用；本身不提交 Op。
    async fn build_thread_settings_overrides(
        &self,
        thread: &CodexThread,
        params: ThreadSettingsBuildParams,
    ) -> Result<codex_protocol::protocol::ThreadSettingsOverrides, JSONRPCErrorError> {
        let ThreadSettingsBuildParams {
            method,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permissions,
            model,
            service_tier,
            effort,
            summary,
            collaboration_mode,
            personality,
        } = params;

        if sandbox_policy.is_some() && permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandboxPolicy`",
            ));
        }

        let collaboration_mode =
            collaboration_mode.map(|mode| self.normalize_collaboration_mode(mode));
        let runtime_workspace_roots_request = runtime_workspace_roots;
        // `thread/settings/update` only acknowledges that the update was queued.
        // Clients that send dependent partial updates should wait for
        // `thread/settings/updated` or combine the fields in one request.
        let snapshot = if permissions.is_some() || runtime_workspace_roots_request.is_some() {
            Some(thread.config_snapshot().await)
        } else {
            None
        };

        let has_any_overrides = cwd.is_some()
            || runtime_workspace_roots_request.is_some()
            || approval_policy.is_some()
            || approvals_reviewer.is_some()
            || sandbox_policy.is_some()
            || permissions.is_some()
            || model.is_some()
            || service_tier.is_some()
            || effort.is_some()
            || summary.is_some()
            || collaboration_mode.is_some()
            || personality.is_some();

        let runtime_workspace_roots = if let Some(workspace_roots) =
            runtime_workspace_roots_request.clone()
        {
            let Some(snapshot) = snapshot.as_ref() else {
                return Err(internal_error(format!(
                    "{method} runtime workspace roots missing thread snapshot"
                )));
            };
            let base_cwd = cwd
                .as_ref()
                .map(|cwd| AbsolutePathBuf::resolve_path_against_base(cwd, snapshot.cwd.as_path()))
                .unwrap_or_else(|| snapshot.cwd.clone());
            Some(resolve_runtime_workspace_roots(workspace_roots, &base_cwd))
        } else {
            None
        };
        let approval_policy =
            approval_policy.map(codex_app_server_protocol::AskForApproval::to_core);
        let approvals_reviewer =
            approvals_reviewer.map(codex_app_server_protocol::ApprovalsReviewer::to_core);
        let sandbox_policy = sandbox_policy.map(|policy| policy.to_core());
        let (permission_profile, active_permission_profile, profile_workspace_roots) =
            if let Some(permissions) = permissions {
                let Some(snapshot) = snapshot.as_ref() else {
                    return Err(internal_error(format!(
                        "{method} permission selection missing thread snapshot"
                    )));
                };
                let overrides = ConfigOverrides {
                    cwd: cwd.clone(),
                    workspace_roots: Some(runtime_workspace_roots_request.clone().unwrap_or_else(
                        || {
                            snapshot
                                .workspace_roots
                                .iter()
                                .map(AbsolutePathBuf::to_path_buf)
                                .collect()
                        },
                    )),
                    default_permissions: Some(permissions),
                    codex_linux_sandbox_exe: self.arg0_paths.codex_linux_sandbox_exe.clone(),
                    main_execve_wrapper_exe: self.arg0_paths.main_execve_wrapper_exe.clone(),
                    ..Default::default()
                };
                let config = self
                    .config_manager
                    .load_for_cwd(
                        /*request_overrides*/ None,
                        overrides,
                        Some(snapshot.cwd.to_path_buf()),
                    )
                    .await
                    .map_err(|err| config_load_error(&err))?;
                // Startup config is allowed to fall back when requirements
                // disallow a configured profile. An explicit settings update
                // is different: reject it before accepting the request.
                if let Some(warning) = config.startup_warnings.iter().find(|warning| {
                    warning.contains("Configured value for `permission_profile` is disallowed")
                }) {
                    return Err(invalid_request(format!(
                        "invalid thread settings override: {warning}"
                    )));
                }
                (
                    Some(config.permissions.permission_profile().clone()),
                    config.permissions.active_permission_profile(),
                    Some(config.permissions.profile_workspace_roots().to_vec()),
                )
            } else {
                (None, None, None)
            };
        let effort = effort.map(Some);

        if has_any_overrides {
            thread
                .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                    cwd: cwd.clone(),
                    workspace_roots: runtime_workspace_roots.clone(),
                    approval_policy,
                    approvals_reviewer,
                    sandbox_policy: sandbox_policy.clone(),
                    permission_profile: permission_profile.clone(),
                    active_permission_profile: active_permission_profile.clone(),
                    profile_workspace_roots: profile_workspace_roots.clone(),
                    windows_sandbox_level: None,
                    model: model.clone(),
                    effort,
                    summary,
                    service_tier: service_tier.clone(),
                    collaboration_mode: collaboration_mode.clone(),
                    personality,
                })
                .await
                .map_err(|err| {
                    invalid_request(format!("invalid thread settings override: {err}"))
                })?;
        }

        Ok(codex_protocol::protocol::ThreadSettingsOverrides {
            cwd,
            workspace_roots: runtime_workspace_roots,
            profile_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level: None,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        })
    }

    async fn thread_settings_update_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<ThreadSettingsUpdateResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "thread/settings/update",
                    cwd: params.cwd,
                    runtime_workspace_roots: None,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;

        // 仅当确实有非默认的覆盖项时才提交 `Op::ThreadSettings`：空更新没有意义，
        // 避免给内核投递一个等价于「什么都不改」的回合外操作。
        if thread_settings != codex_protocol::protocol::ThreadSettingsOverrides::default() {
            self.submit_core_op(
                request_id,
                thread.as_ref(),
                Op::ThreadSettings { thread_settings },
            )
            .await
            .map_err(|err| internal_error(format!("failed to update thread settings: {err}")))?;
        }

        Ok(ThreadSettingsUpdateResponse {})
    }

    async fn thread_inject_items_response_inner(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<ThreadInjectItemsResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;

        let items = params
            .items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                serde_json::from_value::<ResponseItem>(value)
                    .map_err(|err| format!("items[{index}] is not a valid response item: {err}"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(invalid_request)?;

        thread
            .inject_response_items(items)
            .await
            .map_err(|err| match err {
                CodexErr::InvalidRequest(message) => invalid_request(message),
                err => internal_error(format!("failed to inject response items: {err}")),
            })?;
        Ok(ThreadInjectItemsResponse {})
    }

    async fn set_app_server_client_info(
        thread: &CodexThread,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let mcp_elicitations_auto_deny = xcode_26_4_mcp_elicitations_auto_deny(
            app_server_client_name.as_deref(),
            app_server_client_version.as_deref(),
        );
        thread
            .set_app_server_client_info(
                app_server_client_name,
                app_server_client_version,
                mcp_elicitations_auto_deny,
            )
            .await
            .map_err(|err| internal_error(format!("failed to set app server client info: {err}")))
    }

    /// `turn/steer` 的核心实现：向「正在进行中的回合」追加输入以「引导/插话」，
    /// 而非另起新回合。调用方必须给出 `expectedTurnId`，内核会校验它是否与当前
    /// 活跃回合一致，避免把输入误投到已经结束/被替换的回合上。
    ///
    /// 错误处理是本函数的重头戏：`SteerInputError` 的每个变体都被翻译成带
    /// 机器可读 data 的 JSON-RPC 错误，并配套上报对应的 analytics 错误类型
    /// （无活跃回合 / 回合不匹配 / review|compact 回合不可引导 / 空输入）。
    async fn turn_steer_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<TurnSteerResponse, JSONRPCErrorError> {
        let (_, thread) = self
            .load_thread(&params.thread_id)
            .await
            .inspect_err(|error| {
                self.track_error_response(request_id, error, /*error_type*/ None);
            })?;

        if params.expected_turn_id.is_empty() {
            return Err(invalid_request("expectedTurnId must not be empty"));
        }
        self.outgoing
            .record_request_turn_id(request_id, &params.expected_turn_id)
            .await;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }

        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let additional_context = map_additional_context(params.additional_context);

        let turn_id = thread
            .steer_input(
                mapped_items,
                additional_context,
                Some(&params.expected_turn_id),
                params.responsesapi_client_metadata,
            )
            .await
            .map_err(|err| {
                let (message, data, error_type) = match err {
                    SteerInputError::NoActiveTurn(_) => (
                        "no active turn to steer".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::NoActiveTurn,
                        )),
                    ),
                    SteerInputError::ExpectedTurnMismatch { expected, actual } => (
                        format!("expected active turn id `{expected}` but found `{actual}`"),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::ExpectedTurnMismatch,
                        )),
                    ),
                    SteerInputError::ActiveTurnNotSteerable { turn_kind } => {
                        let (message, turn_steer_error) = match turn_kind {
                            codex_protocol::protocol::NonSteerableTurnKind::Review => (
                                "cannot steer a review turn".to_string(),
                                TurnSteerRequestError::NonSteerableReview,
                            ),
                            codex_protocol::protocol::NonSteerableTurnKind::Compact => (
                                "cannot steer a compact turn".to_string(),
                                TurnSteerRequestError::NonSteerableCompact,
                            ),
                        };
                        let error = TurnError {
                            message: message.clone(),
                            codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                                turn_kind: turn_kind.into(),
                            }),
                            additional_details: None,
                        };
                        let data = match serde_json::to_value(error) {
                            Ok(data) => Some(data),
                            Err(error) => {
                                tracing::error!(
                                    ?error,
                                    "failed to serialize active-turn-not-steerable turn error"
                                );
                                None
                            }
                        };
                        (
                            message,
                            data,
                            Some(AnalyticsJsonRpcError::TurnSteer(turn_steer_error)),
                        )
                    }
                    SteerInputError::EmptyInput => (
                        "input must not be empty".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::Input(InputError::Empty)),
                    ),
                };
                let mut error = invalid_request(message);
                error.data = data;
                self.track_error_response(request_id, &error, error_type);
                error
            })?;
        Ok(TurnSteerResponse { turn_id })
    }

    /// 实时（realtime）会话各操作的公共前置：加载线程 → 确保已挂载事件监听器
    /// → 校验该线程开启了 `RealtimeConversation` 特性。
    ///
    /// 返回 `Ok(None)` 表示「连接已关闭」这一非错误的提前退出（调用方据此静默返回），
    /// 与 `Err` 的真正失败区分开。被 start/append_audio/append_text/stop 四个实时
    /// 入口共用，避免重复样板。
    async fn prepare_realtime_conversation_thread(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> Result<Option<(ThreadId, Arc<CodexThread>)>, JSONRPCErrorError> {
        let (thread_id, thread) = self.load_thread(thread_id).await?;

        match self
            .ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await
        {
            Ok(EnsureConversationListenerResult::Attached) => {}
            Ok(EnsureConversationListenerResult::ConnectionClosed) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }

        if !thread.enabled(Feature::RealtimeConversation) {
            return Err(invalid_request(format!(
                "thread {thread_id} does not support realtime conversation"
            )));
        }

        Ok(Some((thread_id, thread)))
    }

    async fn thread_realtime_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ThreadRealtimeStartResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationStart(ConversationStartParams {
                output_modality: params.output_modality,
                prompt: params.prompt,
                realtime_session_id: params.realtime_session_id,
                transport: params.transport.map(|transport| match transport {
                    ThreadRealtimeStartTransport::Websocket => {
                        ConversationStartTransport::Websocket
                    }
                    ThreadRealtimeStartTransport::Webrtc { sdp } => {
                        ConversationStartTransport::Webrtc { sdp }
                    }
                }),
                voice: params.voice,
            }),
        )
        .await
        .map_err(|err| internal_error(format!("failed to start realtime conversation: {err}")))?;
        Ok(Some(ThreadRealtimeStartResponse::default()))
    }

    async fn thread_realtime_append_audio_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ThreadRealtimeAppendAudioResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationAudio(ConversationAudioParams {
                frame: params.audio.into(),
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation audio: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendAudioResponse::default()))
    }

    async fn thread_realtime_append_text_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ThreadRealtimeAppendTextResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationText(ConversationTextParams { text: params.text }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation text: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendTextResponse::default()))
    }

    async fn thread_realtime_stop_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ThreadRealtimeStopResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(request_id, thread.as_ref(), Op::RealtimeConversationClose)
            .await
            .map_err(|err| {
                internal_error(format!("failed to stop realtime conversation: {err}"))
            })?;
        Ok(Some(ThreadRealtimeStopResponse::default()))
    }

    fn build_review_turn(turn_id: String, display_text: &str) -> Turn {
        let items = if display_text.is_empty() {
            Vec::new()
        } else {
            vec![ThreadItem::UserMessage {
                id: turn_id.clone(),
                content: vec![V2UserInput::Text {
                    text: display_text.to_string(),
                    // Review prompt display text is synthesized; no UI element ranges to preserve.
                    text_elements: Vec::new(),
                }],
            }]
        };

        Turn {
            id: turn_id,
            items,
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    async fn emit_review_started(
        &self,
        request_id: &ConnectionRequestId,
        turn: Turn,
        review_thread_id: String,
    ) {
        let response = ReviewStartResponse {
            turn,
            review_thread_id,
        };
        self.outgoing
            .send_response(request_id.clone(), response)
            .await;
    }

    async fn start_inline_review(
        &self,
        request_id: &ConnectionRequestId,
        parent_thread: Arc<CodexThread>,
        review_request: ReviewRequest,
        display_text: &str,
        parent_thread_id: String,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        let turn_id = self
            .submit_core_op(
                request_id,
                parent_thread.as_ref(),
                Op::Review { review_request },
            )
            .await
            .map_err(|err| internal_error(format!("failed to start review: {err}")))?;
        let turn = Self::build_review_turn(turn_id, display_text);
        self.emit_review_started(request_id, turn, parent_thread_id)
            .await;
        Ok(())
    }

    /// 「分离式」代码评审：不在父线程里就地评审，而是基于父线程的当前历史
    /// **fork 出一个独立的评审线程**，在其中跑评审回合。
    ///
    /// 流程：固化并刷盘父线程 rollout → 加载父历史 → （若配置了 review_model 则切换模型）
    /// → `fork_thread_from_history` 派生评审线程 → 挂监听器 → 读回 stored thread 发出
    /// `ThreadStarted` 通知 → 提交 `Op::Review` → 发 review-started 响应。
    ///
    /// 与 `start_inline_review` 的区别：内联评审复用父线程、不新建线程；分离式会产生
    /// 一个新的可被独立订阅/展示的线程，适合「边干活边在旁路跑评审」的场景。
    async fn start_detached_review(
        &self,
        request_id: &ConnectionRequestId,
        parent_thread_id: ThreadId,
        parent_thread: Arc<CodexThread>,
        review_request: ReviewRequest,
        display_text: &str,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        parent_thread.ensure_rollout_materialized().await;
        parent_thread.flush_rollout().await.map_err(|err| {
            internal_error(format!(
                "failed to flush parent thread {parent_thread_id}: {err}"
            ))
        })?;
        let parent_history = parent_thread
            .load_history(/*include_archived*/ true)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to load parent thread {parent_thread_id}: {err}"
                ))
            })?;

        let mut config = self.config.as_ref().clone();
        if let Some(review_model) = &config.review_model {
            config.model = Some(review_model.clone());
        }

        let NewThread {
            thread_id,
            thread: review_thread,
            ..
        } = self
            .thread_manager
            .fork_thread_from_history(
                ForkSnapshot::Interrupted,
                config.clone(),
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: parent_thread_id,
                    history: parent_history.items,
                    rollout_path: parent_thread.rollout_path(),
                }),
                /*thread_source*/ None,
                /*persist_extended_history*/ false,
                self.request_trace_context(request_id).await,
            )
            .await
            .map_err(|err| {
                internal_error(format!("error creating detached review thread: {err}"))
            })?;

        log_listener_attach_result(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await,
            thread_id,
            request_id.connection_id,
            "review thread",
        );

        let fallback_provider = self.config.model_provider_id.as_str();
        match review_thread
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false,
            )
            .await
        {
            Ok(stored_thread) => {
                let (mut thread, _) =
                    thread_from_stored_thread(stored_thread, fallback_provider, &self.config.cwd);
                thread.session_id = review_thread.session_configured().session_id.to_string();
                self.thread_watch_manager
                    .upsert_thread_silently(thread.clone())
                    .await;
                thread.status = resolve_thread_status(
                    self.thread_watch_manager
                        .loaded_status_for_thread(&thread.id)
                        .await,
                    /*has_in_progress_turn*/ false,
                );
                let notif = thread_started_notification(thread);
                self.outgoing
                    .send_server_notification(ServerNotification::ThreadStarted(notif))
                    .await;
            }
            Err(err) => {
                tracing::warn!("failed to load summary for review thread {thread_id}: {err}");
            }
        }

        let turn_id = self
            .submit_core_op(
                request_id,
                review_thread.as_ref(),
                Op::Review { review_request },
            )
            .await
            .map_err(|err| {
                internal_error(format!("failed to start detached review turn: {err}"))
            })?;

        let turn = Self::build_review_turn(turn_id, display_text);
        let review_thread_id = thread_id.to_string();
        self.emit_review_started(request_id, turn, review_thread_id)
            .await;

        Ok(())
    }

    /// `review/start` 的入口：解析评审目标（未提交改动 / 基线分支 / 指定 commit /
    /// 自定义指令），再按 `delivery` 选择交付方式——`Inline`（在父线程内评审）或
    /// `Detached`（fork 出独立评审线程）。默认走 `Inline`。
    async fn review_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ReviewStartParams,
    ) -> Result<(), JSONRPCErrorError> {
        let ReviewStartParams {
            thread_id,
            target,
            delivery,
        } = params;

        let (parent_thread_id, parent_thread) = self.load_thread(&thread_id).await?;
        let (review_request, display_text) = Self::review_request_from_target(target)?;
        match delivery.unwrap_or(ApiReviewDelivery::Inline).to_core() {
            CoreReviewDelivery::Inline => {
                self.start_inline_review(
                    request_id,
                    parent_thread,
                    review_request,
                    &display_text,
                    thread_id,
                )
                .await?;
            }
            CoreReviewDelivery::Detached => {
                self.start_detached_review(
                    request_id,
                    parent_thread_id,
                    parent_thread,
                    review_request,
                    &display_text,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// `turn/interrupt` 的核心实现：中断一个正在进行的回合。
    ///
    /// 两种语义由 `turn_id` 是否为空区分：
    ///   - 普通回合中断（turn_id 非空）：校验目标确实是当前活跃回合，把本次请求登记到
    ///     `pending_interrupts`，**不在此处立即回复**——等内核派发出 `TurnAborted`
    ///     事件后，由监听器一侧统一回应（返回 `Ok(None)`）；
    ///   - 启动期中断（turn_id 为空）：此时还没有回合可言（线程仍在启动），提交
    ///     `Op::Interrupt` 后**立即回复**（返回 `Ok(Some(...))`），因为不会有回合事件。
    ///
    /// 副作用：可能向 thread_state 追加 pending_interrupt；提交失败时会回滚该登记。
    async fn turn_interrupt_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<TurnInterruptResponse>, JSONRPCErrorError> {
        let TurnInterruptParams { thread_id, turn_id } = params;
        let is_startup_interrupt = turn_id.is_empty();

        let (thread_uuid, thread) = self.load_thread(&thread_id).await?;

        // Record turn interrupts so we can reply when TurnAborted arrives. Startup
        // interrupts do not have a turn and are acknowledged after submission.
        // 登记普通回合中断，以便 TurnAborted 到达时再回复客户端；启动期中断没有回合，
        // 在提交后就地确认。先校验 turn_id 与活跃回合一致，否则拒绝（防止误中断）。
        if !is_startup_interrupt {
            let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
            let is_running = matches!(thread.agent_status().await, AgentStatus::Running);
            {
                let mut thread_state = thread_state.lock().await;
                if let Some(active_turn) = thread_state.active_turn_snapshot() {
                    if active_turn.id != turn_id {
                        return Err(invalid_request(format!(
                            "expected active turn id {turn_id} but found {}",
                            active_turn.id
                        )));
                    }
                } else if thread_state.last_terminal_turn_id.as_deref() == Some(turn_id.as_str())
                    || !is_running
                {
                    return Err(invalid_request("no active turn to interrupt"));
                }
                thread_state.pending_interrupts.push(request_id.clone());
            }

            self.outgoing
                .record_request_turn_id(request_id, &turn_id)
                .await;
        }

        // Submit the interrupt. Turn interrupts respond upon TurnAborted; startup
        // interrupts respond here because startup cancellation has no turn event.
        // 提交中断 Op：回合中断走「TurnAborted 事件 → 异步回复」（这里返回 Ok(None)）；
        // 启动期中断没有回合事件，只能就地返回 Ok(Some(...)) 完成应答。
        // 提交失败时，若先前登记过 pending_interrupt，需在错误分支里把它摘除以免泄漏。
        match self
            .submit_core_op(request_id, thread.as_ref(), Op::Interrupt)
            .await
        {
            Ok(_) if is_startup_interrupt => Ok(Some(TurnInterruptResponse {})),
            Ok(_) => Ok(None),
            Err(err) => {
                if !is_startup_interrupt {
                    let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
                    let mut thread_state = thread_state.lock().await;
                    thread_state
                        .pending_interrupts
                        .retain(|pending_request_id| pending_request_id != request_id);
                }
                let interrupt_target = if is_startup_interrupt {
                    "startup"
                } else {
                    "turn"
                };
                Err(internal_error(format!(
                    "failed to interrupt {interrupt_target}: {err}"
                )))
            }
        }
    }

    fn listener_task_context(&self) -> ListenerTaskContext {
        ListenerTaskContext {
            thread_manager: Arc::clone(&self.thread_manager),
            thread_state_manager: self.thread_state_manager.clone(),
            outgoing: Arc::clone(&self.outgoing),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch_manager: self.thread_watch_manager.clone(),
            thread_list_state_permit: self.thread_list_state_permit.clone(),
            fallback_model_provider: self.config.model_provider_id.clone(),
            codex_home: self.config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&self.skills_watcher),
        }
    }

    async fn ensure_conversation_listener(
        &self,
        conversation_id: ThreadId,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
        super::thread_lifecycle::ensure_conversation_listener(
            self.listener_task_context(),
            conversation_id,
            connection_id,
            raw_events_enabled,
        )
        .await
    }
}

fn xcode_26_4_mcp_elicitations_auto_deny(
    client_name: Option<&str>,
    client_version: Option<&str>,
) -> bool {
    // Xcode 26.4 shipped before app-server MCP elicitation requests were
    // client-visible. Keep elicitations auto-denied for that client line.
    // TODO: Remove this compatibility hack once Xcode 26.4 ages out.
    client_name == Some("Xcode")
        && client_version.is_some_and(|version| version.starts_with("26.4"))
}
