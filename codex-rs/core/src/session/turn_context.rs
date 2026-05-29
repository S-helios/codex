//! 「单轮上下文」`TurnContext`——一次模型回合（turn）执行时所需的全部只读快照。
//!
//! 为什么要把这些字段从 `Session` 里拆出来单独打包？因为同一个会话里，模型、
//! 推理强度、审批策略、沙箱权限、工作目录都可能「逐轮」变化（用户中途切模型、
//! 改权限……）。把它们冻结成一份 `TurnContext`，本轮内的所有代码读同一份快照，
//! 既避免「执行到一半配置被改」的竞态，也让回合可被独立追踪/重放。
//!
//! 三条主线：
//! ① 构造——`make_turn_context` 是底层工厂；`new_turn_with_sub_id` 先把
//!    `SessionSettingsUpdate` 应用到会话配置、再产出新一轮 `TurnContext`；
//!    `new_default_turn*` 用当前配置直接造一轮（无改动）。
//! ② 派生——`sandbox_policy`/`file_system_sandbox_context` 把权限档位翻译成
//!    具体沙箱策略；`model_context_window` 算出本轮可用的上下文窗口上限。
//! ③ 序列化——`to_turn_context_item` 把本轮关键设置打成一条历史项，写进 thread
//!    历史，供「设置变更 diff」与回放使用。
use super::*;
use crate::SkillLoadOutcome;
use crate::config::GhostSnapshotConfig;
use crate::environment_selection::ResolvedTurnEnvironments;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_protocol::SessionId;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_sandboxing::compatibility_sandbox_policy_for_permission_profile;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::effective_network_sandbox_policy;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Clone, Debug)]
pub(crate) struct TurnSkillsContext {
    pub(crate) outcome: Arc<SkillLoadOutcome>,
    pub(crate) implicit_invocation_seen_skills: Arc<Mutex<HashSet<String>>>,
}

impl TurnSkillsContext {
    pub(crate) fn new(outcome: Arc<SkillLoadOutcome>) -> Self {
        Self {
            outcome,
            implicit_invocation_seen_skills: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TurnEnvironment {
    pub(crate) environment_id: String,
    pub(crate) environment: Arc<Environment>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) shell: Option<String>,
}

impl TurnEnvironment {
    pub(crate) fn selection(&self) -> TurnEnvironmentSelection {
        TurnEnvironmentSelection {
            environment_id: self.environment_id.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

/// The context needed for a single turn of the thread.
///
/// 一轮对话的「冻结配置」。字段大致分四组：① 身份/追踪（sub_id、trace_id、
/// session_source）；② 模型与推理（model_info、provider、reasoning_effort）；
/// ③ 权限与沙箱（approval_policy、permission_profile、network、environments）；
/// ④ 提示词与能力（developer/user_instructions、dynamic_tools、features）。
/// 注意 `cwd` 已标 `#[deprecated]`——新代码应从「选定的 turn environment」取 cwd，
/// 这是「多工作区/多环境」改造留下的过渡痕迹。
#[derive(Debug)]
pub struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) trace_id: Option<String>,
    pub(crate) realtime_active: bool,
    pub config: Arc<Config>,
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    pub(crate) model_info: ModelInfo,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) provider: SharedModelProvider,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) session_source: SessionSource,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) environments: ResolvedTurnEnvironments,
    /// The session's absolute working directory. All relative paths provided
    /// by the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    #[deprecated(note = "use the selected turn environment cwd instead")]
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) current_date: Option<String>,
    pub(crate) timezone: Option<String>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) compact_prompt: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) collaboration_mode: CollaborationMode,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: Constrained<AskForApproval>,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) network: Option<NetworkProxy>,
    pub(crate) windows_sandbox_level: WindowsSandboxLevel,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) available_models: Vec<ModelPreset>,
    pub(crate) unified_exec_shell_mode: UnifiedExecShellMode,
    pub(crate) goal_tools_supported: bool,
    pub features: ManagedFeatures,
    pub(crate) ghost_snapshot: GhostSnapshotConfig,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) codex_self_exe: Option<PathBuf>,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
    pub(crate) truncation_policy: TruncationPolicy,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) turn_metadata_state: Arc<TurnMetadataState>,
    pub(crate) extension_data: Arc<codex_extension_api::ExtensionData>,
    pub(crate) turn_skills: TurnSkillsContext,
    pub(crate) turn_timing_state: Arc<TurnTimingState>,
    pub(crate) server_model_warning_emitted: AtomicBool,
    pub(crate) model_verification_emitted: AtomicBool,
}
impl TurnContext {
    pub(crate) fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile.clone()
    }

    pub(crate) fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        self.permission_profile.file_system_sandbox_policy()
    }

    pub(crate) fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile.network_sandbox_policy()
    }

    pub(crate) fn sandbox_policy(&self) -> SandboxPolicy {
        let file_system_sandbox_policy = self.file_system_sandbox_policy();
        let network_sandbox_policy = self.network_sandbox_policy();
        compatibility_sandbox_policy_for_permission_profile(
            &self.permission_profile,
            &file_system_sandbox_policy,
            network_sandbox_policy,
            #[allow(deprecated)]
            &self.cwd,
        )
    }

    pub(crate) fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if self.model_info.supports_reasoning_summaries {
            self.reasoning_effort
                .or(self.model_info.default_reasoning_level)
        } else {
            None
        }
    }

    pub(crate) fn effective_reasoning_effort_for_tracing(&self) -> String {
        self.effective_reasoning_effort()
            .map(|effort| effort.to_string())
            .unwrap_or_else(|| "default".to_string())
    }

    /// 本轮「实际可用」的上下文窗口（token 数）。不是直接用模型标称窗口，而是
    /// 乘以 `effective_context_window_percent`——codex 故意留出一截余量（不把窗口
    /// 用满），给输出/推理留空间，也降低踩到服务端硬上限的风险。压缩、token 预算
    /// 判断都以这个「打折后」的值为准。
    pub(crate) fn model_context_window(&self) -> Option<i64> {
        let effective_context_window_percent = self.model_info.effective_context_window_percent;
        self.model_info
            .resolved_context_window()
            .map(|context_window| {
                context_window.saturating_mul(effective_context_window_percent) / 100
            })
    }

    pub(crate) fn apps_enabled(&self) -> bool {
        let uses_codex_backend = self
            .auth_manager
            .as_deref()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        self.features.apps_enabled_for_auth(uses_codex_backend)
    }

    pub(crate) fn tool_environment_mode(&self) -> ToolEnvironmentMode {
        ToolEnvironmentMode::from_count(self.environments.turn_environments.len())
    }

    pub(crate) fn goal_tools_enabled(&self) -> bool {
        self.goal_tools_supported && self.features.get().enabled(Feature::Goals)
    }

    /// 基于当前轮「换一个模型」派生出新的 `TurnContext`（其余设置尽量沿用）。
    /// 难点是推理强度的迁移：若当前 effort 新模型也支持就保留；否则取新模型
    /// 支持档位的「中位数」（`len-1`/2 那一档）兜底，再不行用模型默认值。
    /// 这样换模型不会因为「旧档位非法」而报错或丢失用户意图。
    pub(crate) async fn with_model(
        &self,
        model: String,
        models_manager: &SharedModelsManager,
    ) -> Self {
        let mut config = (*self.config).clone();
        config.model = Some(model.clone());
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let truncation_policy = model_info.truncation_policy.into();
        let supported_reasoning_levels = model_info
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort)
            .collect::<Vec<_>>();
        let reasoning_effort = if let Some(current_reasoning_effort) = self.reasoning_effort {
            if supported_reasoning_levels.contains(&current_reasoning_effort) {
                Some(current_reasoning_effort)
            } else {
                supported_reasoning_levels
                    .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                    .copied()
                    .or(model_info.default_reasoning_level)
            }
        } else {
            supported_reasoning_levels
                .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                .copied()
                .or(model_info.default_reasoning_level)
        };
        config.model_reasoning_effort = reasoning_effort;

        let collaboration_mode = self.collaboration_mode.with_updates(
            Some(model.clone()),
            Some(reasoning_effort),
            /*developer_instructions*/ None,
        );
        let features = self.features.clone();
        let available_models = models_manager
            .list_models(RefreshStrategy::OnlineIfUncached)
            .await;

        Self {
            sub_id: self.sub_id.clone(),
            trace_id: self.trace_id.clone(),
            realtime_active: self.realtime_active,
            config: Arc::new(config),
            auth_manager: self.auth_manager.clone(),
            model_info: model_info.clone(),
            session_telemetry: self
                .session_telemetry
                .clone()
                .with_model(model.as_str(), model_info.slug.as_str()),
            provider: self.provider.clone(),
            reasoning_effort,
            reasoning_summary: self.reasoning_summary,
            session_source: self.session_source.clone(),
            thread_source: self.thread_source,
            environments: self.environments.clone(),
            #[allow(deprecated)]
            cwd: self.cwd.clone(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            app_server_client_name: self.app_server_client_name.clone(),
            developer_instructions: self.developer_instructions.clone(),
            compact_prompt: self.compact_prompt.clone(),
            user_instructions: self.user_instructions.clone(),
            collaboration_mode,
            personality: self.personality,
            approval_policy: self.approval_policy.clone(),
            permission_profile: self.permission_profile.clone(),
            network: self.network.clone(),
            windows_sandbox_level: self.windows_sandbox_level,
            shell_environment_policy: self.shell_environment_policy.clone(),
            available_models,
            unified_exec_shell_mode: self.unified_exec_shell_mode.clone(),
            goal_tools_supported: self.goal_tools_supported,
            features,
            ghost_snapshot: self.ghost_snapshot.clone(),
            final_output_json_schema: self.final_output_json_schema.clone(),
            codex_self_exe: self.codex_self_exe.clone(),
            codex_linux_sandbox_exe: self.codex_linux_sandbox_exe.clone(),
            truncation_policy,
            dynamic_tools: self.dynamic_tools.clone(),
            turn_metadata_state: self.turn_metadata_state.clone(),
            extension_data: Arc::clone(&self.extension_data),
            turn_skills: self.turn_skills.clone(),
            turn_timing_state: Arc::clone(&self.turn_timing_state),
            server_model_warning_emitted: AtomicBool::new(
                self.server_model_warning_emitted.load(Ordering::Relaxed),
            ),
            model_verification_emitted: AtomicBool::new(
                self.model_verification_emitted.load(Ordering::Relaxed),
            ),
        }
    }

    #[deprecated(note = "resolve paths from the selected turn environment cwd instead")]
    pub(crate) fn resolve_path(&self, path: Option<String>) -> AbsolutePathBuf {
        #[allow(deprecated)]
        path.as_ref()
            .map_or_else(|| self.cwd.clone(), |path| self.cwd.join(path))
    }

    /// 为「单次命令执行」算出文件系统沙箱上下文。允许叠加 `additional_permissions`
    /// （某条命令临时多给的权限），与基础权限档位合并后得到本次实际生效的读写边界。
    /// 这是 turn 级权限 → exec 级沙箱的桥接点。
    pub(crate) fn file_system_sandbox_context(
        &self,
        additional_permissions: Option<AdditionalPermissionProfile>,
        cwd: &AbsolutePathBuf,
    ) -> FileSystemSandboxContext {
        let (base_file_system_sandbox_policy, base_network_sandbox_policy) =
            self.permission_profile.to_runtime_permissions();
        let file_system_sandbox_policy = effective_file_system_sandbox_policy(
            &base_file_system_sandbox_policy,
            additional_permissions.as_ref(),
        );
        let network_sandbox_policy = effective_network_sandbox_policy(
            base_network_sandbox_policy,
            additional_permissions.as_ref(),
        );
        let permissions = PermissionProfile::from_runtime_permissions_with_enforcement(
            self.permission_profile.enforcement(),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
        FileSystemSandboxContext {
            permissions,
            cwd: Some(cwd.clone()),
            windows_sandbox_level: self.windows_sandbox_level,
            windows_sandbox_private_desktop: self
                .config
                .permissions
                .windows_sandbox_private_desktop,
            use_legacy_landlock: self.features.use_legacy_landlock(),
        }
    }

    fn non_legacy_file_system_sandbox_policy(&self) -> Option<FileSystemSandboxPolicy> {
        // Omit the derived split filesystem policy when it is equivalent to
        // the legacy sandbox policy. This keeps turn-context payloads stable
        // while both fields exist; once callers consume only the split policy,
        // this comparison and the legacy projection should go away.
        let legacy_file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                &self.sandbox_policy(),
                #[allow(deprecated)]
                &self.cwd,
            );
        let file_system_sandbox_policy = self.file_system_sandbox_policy();
        (file_system_sandbox_policy != legacy_file_system_sandbox_policy)
            .then_some(file_system_sandbox_policy)
    }

    pub(crate) fn compact_prompt(&self) -> &str {
        self.compact_prompt
            .as_deref()
            .unwrap_or(compact::SUMMARIZATION_PROMPT)
    }

    /// 把本轮的关键设置「拍扁」成一条 `TurnContextItem` 写进历史。它是后续
    /// 「设置变更检测」的快照基准：下一轮若 cwd/审批/沙箱/模型等变了，会与这条
    /// 做 diff，只把变动部分作为「设置更新」注入给模型，而非每轮全量重述。
    pub(crate) fn to_turn_context_item(&self) -> TurnContextItem {
        TurnContextItem {
            turn_id: Some(self.sub_id.clone()),
            #[allow(deprecated)]
            cwd: self.cwd.to_path_buf(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            approval_policy: self.approval_policy.value(),
            sandbox_policy: self.sandbox_policy(),
            permission_profile: Some(self.permission_profile()),
            network: self.turn_context_network_item(),
            file_system_sandbox_policy: self.non_legacy_file_system_sandbox_policy(),
            model: self.model_info.slug.clone(),
            personality: self.personality,
            collaboration_mode: Some(self.collaboration_mode.clone()),
            realtime_active: Some(self.realtime_active),
            effort: self.reasoning_effort,
            summary: ReasoningSummaryConfig::Auto,
        }
    }

    fn turn_context_network_item(&self) -> Option<TurnContextNetworkItem> {
        let network = self
            .config
            .config_layer_stack
            .requirements()
            .network
            .as_ref()?;
        Some(TurnContextNetworkItem {
            allowed_domains: network
                .domains
                .as_ref()
                .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains)
                .unwrap_or_default(),
            denied_domains: network
                .domains
                .as_ref()
                .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains)
                .unwrap_or_default(),
        })
    }
}

fn local_time_context() -> (String, String) {
    match iana_time_zone::get_timezone() {
        Ok(timezone) => (Local::now().format("%Y-%m-%d").to_string(), timezone),
        Err(_) => (
            Utc::now().format("%Y-%m-%d").to_string(),
            "Etc/UTC".to_string(),
        ),
    }
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    pub(crate) fn build_per_turn_config(
        session_configuration: &SessionConfiguration,
        cwd: AbsolutePathBuf,
    ) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.cwd = cwd;
        per_turn_config.workspace_roots = session_configuration.workspace_roots.clone();
        per_turn_config
            .permissions
            .set_workspace_roots(session_configuration.workspace_roots.clone());
        per_turn_config.model_reasoning_effort =
            session_configuration.collaboration_mode.reasoning_effort();
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.service_tier = session_configuration.service_tier.clone();
        per_turn_config.personality = session_configuration.personality;
        per_turn_config.approvals_reviewer = session_configuration.approvals_reviewer;
        session_configuration
            .apply_permission_profile_to_permissions(&mut per_turn_config.permissions);
        let permission_profile = session_configuration.permission_profile();
        let resolved_web_search_mode =
            resolve_web_search_mode_for_turn(&per_turn_config.web_search_mode, &permission_profile);
        if let Err(err) = per_turn_config
            .web_search_mode
            .set(resolved_web_search_mode)
        {
            let fallback_value = per_turn_config.web_search_mode.value();
            tracing::warn!(
                error = %err,
                ?resolved_web_search_mode,
                ?fallback_value,
                "resolved web_search_mode is disallowed by requirements; keeping constrained value"
            );
        }
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    pub(crate) fn build_effective_session_config(
        session_configuration: &SessionConfiguration,
    ) -> Config {
        let mut config =
            Self::build_per_turn_config(session_configuration, session_configuration.cwd.clone());
        config.model = Some(session_configuration.collaboration_mode.model().to_string());
        config.permissions.approval_policy = session_configuration.approval_policy.clone();
        config.workspace_roots = session_configuration.workspace_roots.clone();
        config
            .permissions
            .set_workspace_roots(session_configuration.workspace_roots.clone());
        config
    }

    /// `TurnContext` 的底层工厂——把会话级配置 + 本轮解析出的环境/模型/沙箱等
    /// 一次性组装成一份不可变快照。参数多到要 `#[allow(too_many_arguments)]`，
    /// 正因为它要「冻结一整轮所需的一切」。上层一般不直接调它，而是走
    /// `new_turn_from_configuration`（已备好各依赖）再转交到这里。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn make_turn_context(
        thread_id: ThreadId,
        session_id: SessionId,
        auth_manager: Option<Arc<AuthManager>>,
        session_telemetry: &SessionTelemetry,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        user_shell: &shell::Shell,
        shell_zsh_path: Option<&PathBuf>,
        main_execve_wrapper_exe: Option<&PathBuf>,
        per_turn_config: Config,
        model_info: ModelInfo,
        models_manager: &SharedModelsManager,
        network: Option<NetworkProxy>,
        environments: ResolvedTurnEnvironments,
        cwd: AbsolutePathBuf,
        sub_id: String,
        skills_outcome: Arc<SkillLoadOutcome>,
        goal_tools_supported: bool,
    ) -> TurnContext {
        let reasoning_effort = session_configuration.collaboration_mode.reasoning_effort();
        let reasoning_summary = session_configuration
            .model_reasoning_summary
            .unwrap_or(model_info.default_reasoning_summary);
        let session_telemetry = session_telemetry.clone().with_model(
            session_configuration.collaboration_mode.model(),
            model_info.slug.as_str(),
        );
        let session_source = session_configuration.session_source.clone();
        let auth_manager_for_context = auth_manager.clone();
        let provider_for_context = create_model_provider(provider, auth_manager);
        let session_telemetry_for_context = session_telemetry;
        let available_models = models_manager.try_list_models().unwrap_or_default();
        let shell_command_backend =
            shell_command_backend_for_features(per_turn_config.features.get());
        let unified_exec_shell_mode = UnifiedExecShellMode::for_session(
            shell_command_backend,
            crate::tools::tool_user_shell_type(user_shell),
            shell_zsh_path,
            main_execve_wrapper_exe,
        );

        let mut per_turn_config = per_turn_config;
        per_turn_config.service_tier = get_service_tier(
            per_turn_config.service_tier,
            per_turn_config.features.enabled(Feature::FastMode),
            &model_info,
        );
        let per_turn_config = Arc::new(per_turn_config);
        let turn_metadata_state = Arc::new(TurnMetadataState::new(
            session_id.to_string(),
            thread_id.to_string(),
            session_configuration.forked_from_thread_id,
            session_configuration.thread_source,
            sub_id.clone(),
            cwd.clone(),
            &session_configuration.permission_profile(),
            session_configuration.windows_sandbox_level,
            network.is_some(),
        ));
        let (current_date, timezone) = local_time_context();
        let extension_data = Arc::new(codex_extension_api::ExtensionData::new(sub_id.clone()));
        TurnContext {
            sub_id,
            trace_id: current_span_trace_id(),
            realtime_active: false,
            config: per_turn_config.clone(),
            auth_manager: auth_manager_for_context,
            model_info: model_info.clone(),
            session_telemetry: session_telemetry_for_context,
            provider: provider_for_context,
            reasoning_effort,
            reasoning_summary,
            session_source,
            thread_source: session_configuration.thread_source,
            environments,
            #[allow(deprecated)]
            cwd,
            current_date: Some(current_date),
            timezone: Some(timezone),
            app_server_client_name: session_configuration.app_server_client_name.clone(),
            developer_instructions: session_configuration.developer_instructions.clone(),
            compact_prompt: session_configuration.compact_prompt.clone(),
            user_instructions: session_configuration.user_instructions.clone(),
            collaboration_mode: session_configuration.collaboration_mode.clone(),
            personality: session_configuration.personality,
            approval_policy: session_configuration.approval_policy.clone(),
            permission_profile: session_configuration.permission_profile(),
            network,
            windows_sandbox_level: session_configuration.windows_sandbox_level,
            shell_environment_policy: per_turn_config.permissions.shell_environment_policy.clone(),
            available_models,
            unified_exec_shell_mode,
            goal_tools_supported,
            features: per_turn_config.features.clone(),
            ghost_snapshot: per_turn_config.ghost_snapshot.clone(),
            final_output_json_schema: None,
            codex_self_exe: per_turn_config.codex_self_exe.clone(),
            codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
            truncation_policy: model_info.truncation_policy.into(),
            dynamic_tools: session_configuration.dynamic_tools.clone(),
            turn_metadata_state,
            extension_data,
            turn_skills: TurnSkillsContext::new(skills_outcome),
            turn_timing_state: Arc::new(TurnTimingState::default()),
            server_model_warning_emitted: AtomicBool::new(false),
            model_verification_emitted: AtomicBool::new(false),
        }
    }

    /// 「应用一批设置变更 → 产出新一轮上下文」的入口。流程：在持锁临界区内
    /// 把 `SessionSettingsUpdate` 应用到会话配置（校验非法值会直接报错回滚），
    /// 解析新的 turn environments，并记录权限档位是否变化等;出锁后再做副作用
    /// （通知配置贡献者、刷新 shell 快照、权限变了就重建网络代理），最后委托
    /// `new_turn_from_configuration` 真正造出 `TurnContext`。
    /// 设计要点：所有「改会话状态」的活都压在锁内最小范围，I/O 类副作用挪到锁外，
    /// 既保证状态一致又不长时间持锁。
    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> CodexResult<Arc<TurnContext>> {
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let update_result: CodexResult<_> = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(&updates) {
                Ok(next) => {
                    let mut effective_environments = updates
                        .environments
                        .clone()
                        .unwrap_or_else(|| next.environments.clone());
                    if updates.environments.is_none() {
                        Self::overlay_runtime_cwd_on_primary_environment(
                            &mut effective_environments,
                            &next.cwd,
                        );
                    }
                    let turn_environments =
                        self.resolve_turn_environments(&effective_environments)?;
                    let previous_cwd = state.session_configuration.cwd.clone();
                    let previous_permission_profile =
                        state.session_configuration.permission_profile();
                    let next_permission_profile = next.permission_profile();
                    let permission_profile_changed =
                        previous_permission_profile != next_permission_profile;
                    let codex_home = next.codex_home.clone();
                    let session_source = next.session_source.clone();
                    let previous_config = notify_config_contributors.then(|| {
                        Self::build_effective_session_config(&state.session_configuration)
                    });
                    let new_config = notify_config_contributors
                        .then(|| Self::build_effective_session_config(&next));
                    state.session_configuration = next.clone();
                    Ok((
                        next,
                        turn_environments,
                        permission_profile_changed,
                        previous_cwd,
                        codex_home,
                        session_source,
                        previous_config,
                        new_config,
                    ))
                }
                Err(err) => Err(CodexErr::InvalidRequest(err.to_string())),
            }
        };

        let (
            session_configuration,
            turn_environments,
            permission_profile_changed,
            previous_cwd,
            codex_home,
            session_source,
            previous_config,
            new_config,
        ) = match update_result {
            Ok(update) => update,
            Err(err) => {
                let message = err.to_string();
                self.send_event_raw(Event {
                    id: sub_id.clone(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: message.clone(),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                })
                .await;
                return Err(CodexErr::InvalidRequest(message));
            }
        };

        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        self.maybe_refresh_shell_snapshot_for_cwd(
            &previous_cwd,
            &session_configuration.cwd,
            &codex_home,
            &session_source,
        );

        if permission_profile_changed {
            self.refresh_managed_network_proxy_for_current_permission_profile()
                .await;
        }

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                updates.final_output_json_schema,
                turn_environments,
            )
            .await)
    }

    fn resolve_turn_environments(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> CodexResult<ResolvedTurnEnvironments> {
        crate::environment_selection::resolve_environment_selections(
            self.services.environment_manager.as_ref(),
            environments,
        )
    }

    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        turn_environments: ResolvedTurnEnvironments,
    ) -> Arc<TurnContext> {
        let primary_turn_environment = turn_environments.primary();
        let cwd = primary_turn_environment
            .map(|turn_environment| turn_environment.cwd.clone())
            .unwrap_or_else(|| session_configuration.cwd.clone());
        let per_turn_config = Self::build_per_turn_config(&session_configuration, cwd.clone());
        {
            let mcp_connection_manager = self.services.mcp_connection_manager.read().await;
            mcp_connection_manager.set_approval_policy(&session_configuration.approval_policy);
            mcp_connection_manager
                .set_permission_profile(session_configuration.permission_profile());
        }

        let model_info = self
            .services
            .models_manager
            .get_model_info(
                session_configuration.collaboration_mode.model(),
                &per_turn_config.to_models_manager_config(),
            )
            .await;
        let plugin_outcome = self
            .services
            .plugins_manager
            .plugins_for_config(&per_turn_config.plugins_config_input())
            .await;
        let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
        let skills_input = skills_load_input_from_config(&per_turn_config, effective_skill_roots);
        let fs = primary_turn_environment
            .map(|turn_environment| turn_environment.environment.get_filesystem());
        let skills_outcome = Arc::new(
            self.services
                .skills_manager
                .skills_for_config(&skills_input, fs)
                .await,
        );
        let goal_tools_supported = !per_turn_config.ephemeral && self.state_db().is_some();
        let mut turn_context: TurnContext = Self::make_turn_context(
            self.thread_id(),
            self.session_id(),
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.session_telemetry,
            session_configuration.provider.clone(),
            &session_configuration,
            self.services.user_shell.as_ref(),
            self.services.shell_zsh_path.as_ref(),
            self.services.main_execve_wrapper_exe.as_ref(),
            per_turn_config,
            model_info,
            &self.services.models_manager,
            self.services
                .network_proxy
                .load_full()
                .as_ref()
                .and_then(|started_proxy| {
                    Self::managed_network_proxy_active_for_permission_profile(
                        &session_configuration.permission_profile(),
                    )
                    .then(|| started_proxy.proxy())
                }),
            turn_environments,
            cwd,
            sub_id,
            skills_outcome,
            goal_tools_supported,
        );
        turn_context.realtime_active = self.conversation.running_state().await.is_some();

        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        let turn_context = Arc::new(turn_context);
        turn_context.turn_metadata_state.spawn_git_enrichment_task();
        turn_context
    }

    pub(crate) async fn maybe_emit_unknown_model_warning_for_turn(&self, tc: &TurnContext) {
        if tc.model_info.used_fallback_model_metadata {
            self.send_event(
                tc,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Model metadata for `{}` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.",
                        tc.model_info.slug
                    ),
                }),
            )
            .await;
        }
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        let mut effective_environments = session_configuration.environments.clone();
        Self::overlay_runtime_cwd_on_primary_environment(
            &mut effective_environments,
            &session_configuration.cwd,
        );
        let turn_environments = match self.resolve_turn_environments(&effective_environments) {
            Ok(turn_environments) => turn_environments,
            Err(err) => {
                warn!("failed to resolve stored session environments: {err}");
                ResolvedTurnEnvironments::default()
            }
        };

        self.new_turn_from_configuration(
            sub_id,
            session_configuration,
            /*final_output_json_schema*/ None,
            turn_environments,
        )
        .await
    }

    fn overlay_runtime_cwd_on_primary_environment(
        environments: &mut [TurnEnvironmentSelection],
        runtime_cwd: &AbsolutePathBuf,
    ) {
        if let Some(turn_environment) = environments.first_mut()
            && turn_environment.cwd != *runtime_cwd
        {
            turn_environment.cwd = runtime_cwd.clone();
        }
    }
}
