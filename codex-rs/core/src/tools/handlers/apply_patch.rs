//! 【文件职责】apply_patch 工具的 handler 层，是「工具调用」与「apply-patch crate
//! （真正改文件的引擎）」之间的桥。它有两个入口：
//!   1. ApplyPatchHandler::handle —— 模型显式调用 freeform `apply_patch` 工具时走这里。
//!   2. intercept_apply_patch —— 模型用 shell 工具发了一条形如 `apply_patch <<'EOF' …`
//!      的命令时，shell 路径在执行前先来这里「拦截」，若识别出是合法补丁就转交本机制，
//!      否则放行回 shell 正常执行。两条入口共用同一套「校验→算权限→落盘」逻辑。
//! 【核心流程】解析补丁文本 → 选定运行环境（multi-environment 时按 environment_id）→
//!   对照该环境文件系统校验补丁（verify_apply_patch_args）→ 计算落盘所需的有效沙箱权限
//!   （effective_patch_permissions）→ 二选一执行：
//!     · InternalApplyPatchInvocation::Output：无需沙箱/审批，直接拿到结果文本。
//!     · DelegateToRuntime：交给 ApplyPatchRuntime + ToolOrchestrator 走「审批→沙箱→
//!       重试」标准链路（与 shell 工具同一套 orchestrator）。
//! 【流式预览】ApplyPatchArgumentDiffConsumer 在模型「边生成补丁参数边推流」时增量解析，
//!   节流后发出 PatchApplyUpdatedEvent，让 UI 实时显示将要改动的文件。
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ApplyPatchToolOutput;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch_spec::create_apply_patch_freeform_tool;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_apply_patch::Hunk;
use codex_apply_patch::StreamingPatchParser;
use codex_exec_server::ExecutorFileSystem;
use codex_features::Feature;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyUpdatedEvent;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use codex_sandboxing::policy_transforms::normalize_additional_permissions;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;

/// 流式预览的节流窗口：两次 PatchApplyUpdatedEvent 之间至少间隔 500ms，避免补丁参数
/// 高频推流时把 UI 事件刷爆。
const APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL: Duration = Duration::from_millis(500);
/// Handles freeform `apply_patch` requests and routes verified patches to the
/// selected environment filesystem.
/// 处理 freeform `apply_patch` 工具调用，把校验通过的补丁落到「选定环境」的文件系统上。
/// multi_environment：是否允许补丁里携带 environment_id 选择目标环境（多环境会话才开），
/// 关闭时若补丁仍带了 environment_id 会被 require_environment_id 拒掉。
#[derive(Default)]
pub struct ApplyPatchHandler {
    multi_environment: bool,
}

impl ApplyPatchHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

/// 把模型「逐字推流」过来的 apply_patch 参数增量喂给流式解析器，攒出可预览的文件改动
/// 并节流发出 PatchApplyUpdatedEvent。
/// - parser：跨多次 delta 累积、增量解析补丁文本的状态机。
/// - last_sent_at：上次真正发事件的时刻，用于实现 500ms 节流。
/// - pending：被节流压下、尚未发出的「最新一帧」——窗口到点（或 finish）时补发，保证
///   最终态不丢。
#[derive(Default)]
struct ApplyPatchArgumentDiffConsumer {
    parser: StreamingPatchParser,
    last_sent_at: Option<Instant>,
    pending: Option<PatchApplyUpdatedEvent>,
}

impl ToolArgumentDiffConsumer for ApplyPatchArgumentDiffConsumer {
    fn consume_diff(
        &mut self,
        turn: &TurnContext,
        call_id: String,
        diff: &str,
    ) -> Option<EventMsg> {
        if !turn.features.enabled(Feature::ApplyPatchStreamingEvents) {
            return None;
        }

        self.push_delta(call_id, diff)
            .map(EventMsg::PatchApplyUpdated)
    }

    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        self.finish_update_on_complete()
            .map(|event| event.map(EventMsg::PatchApplyUpdated))
    }
}

impl ApplyPatchArgumentDiffConsumer {
    /// 喂入一段增量并按节流策略决定是否立刻发事件。解析出错或本次没攒出完整 hunk 时返回
    /// None（静默吞掉，等后续 delta）。节流逻辑：距上次发送不足 500ms → 暂存到 pending、
    /// 本次不发；否则立即发并刷新时间戳。
    fn push_delta(&mut self, call_id: String, delta: &str) -> Option<PatchApplyUpdatedEvent> {
        let hunks = self.parser.push_delta(delta).ok()?;
        if hunks.is_empty() {
            return None;
        }
        let changes = convert_apply_patch_hunks_to_protocol(&hunks);
        let event = PatchApplyUpdatedEvent { call_id, changes };
        let now = Instant::now();
        match self.last_sent_at {
            // 窗口未到：丢弃旧 pending、只留这一帧最新的，等下次窗口或 finish 时补发。
            Some(last_sent_at)
                if now.duration_since(last_sent_at) < APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL =>
            {
                self.pending = Some(event);
                None
            }
            // 首帧或窗口已过：立即发送并记下时间戳。
            Some(_) | None => {
                self.pending = None;
                self.last_sent_at = Some(now);
                Some(event)
            }
        }
    }

    /// 参数推流结束时调用：先让解析器收尾（补丁不完整就报错回模型），再把被节流压下的
    /// 最后一帧 pending 补发出去，保证 UI 收到的是最终态。
    fn finish_update_on_complete(
        &mut self,
    ) -> Result<Option<PatchApplyUpdatedEvent>, FunctionCallError> {
        self.parser.finish().map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse apply_patch: {err}"))
        })?;

        let event = self.pending.take();
        if event.is_some() {
            self.last_sent_at = Some(Instant::now());
        }
        Ok(event)
    }
}

fn convert_apply_patch_hunks_to_protocol(hunks: &[Hunk]) -> HashMap<PathBuf, FileChange> {
    hunks
        .iter()
        .map(|hunk| {
            let path = hunk_source_path(hunk).to_path_buf();
            let change = match hunk {
                Hunk::AddFile { contents, .. } => FileChange::Add {
                    content: contents.clone(),
                },
                Hunk::DeleteFile { .. } => FileChange::Delete {
                    content: String::new(),
                },
                Hunk::UpdateFile {
                    chunks, move_path, ..
                } => FileChange::Update {
                    unified_diff: format_update_chunks_for_progress(chunks),
                    move_path: move_path.clone(),
                },
            };
            (path, change)
        })
        .collect()
}

fn hunk_source_path(hunk: &Hunk) -> &Path {
    match hunk {
        Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } | Hunk::UpdateFile { path, .. } => {
            path
        }
    }
}

fn format_update_chunks_for_progress(chunks: &[codex_apply_patch::UpdateFileChunk]) -> String {
    let mut unified_diff = String::new();
    for chunk in chunks {
        match &chunk.change_context {
            Some(context) => {
                unified_diff.push_str("@@ ");
                unified_diff.push_str(context);
                unified_diff.push('\n');
            }
            None => {
                unified_diff.push_str("@@");
                unified_diff.push('\n');
            }
        }
        for line in &chunk.old_lines {
            unified_diff.push('-');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        for line in &chunk.new_lines {
            unified_diff.push('+');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        if chunk.is_end_of_file {
            unified_diff.push_str("*** End of File");
            unified_diff.push('\n');
        }
    }
    unified_diff
}

/// 收集本次补丁会触碰到的所有绝对路径（相对 cwd 解析）。注意：Update 若带「移动」目标
/// （move_path），源路径和目标路径都要算进去——后续据此申请写权限、做审批 key，漏掉目标
/// 路径会导致移动落盘时权限不足。
fn file_paths_for_action(action: &ApplyPatchAction) -> Vec<AbsolutePathBuf> {
    let mut keys = Vec::new();
    let cwd = &action.cwd;

    for (path, change) in action.changes() {
        if let Some(key) = to_abs_path(cwd, path) {
            keys.push(key);
        }

        if let ApplyPatchFileChange::Update { move_path, .. } = change
            && let Some(dest) = move_path
            && let Some(key) = to_abs_path(cwd, dest)
        {
            keys.push(key);
        }
    }

    keys
}

fn to_abs_path(cwd: &AbsolutePathBuf, path: &Path) -> Option<AbsolutePathBuf> {
    Some(AbsolutePathBuf::resolve_path_against_base(path, cwd))
}

/// 算出「补丁要落盘，但当前沙箱策略不允许写」的那些父目录，打包成一份额外写权限 profile。
/// 思路：对每个目标文件取其父目录 → 过滤掉策略已允许写的 → 去重（BTreeSet）→ 若仍有剩余
/// 就构造一个只含这些写根的 AdditionalPermissionProfile（读根给空 vec）。全都在允许范围内
/// 时返回 None（无需额外提权）。这份 profile 最终决定 apply_patch 要不要弹审批 / 升级沙箱。
fn write_permissions_for_paths(
    file_paths: &[AbsolutePathBuf],
    file_system_sandbox_policy: &codex_protocol::permissions::FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> Option<AdditionalPermissionProfile> {
    let write_paths = file_paths
        .iter()
        .map(|path| {
            path.parent()
                .unwrap_or_else(|| path.clone())
                .into_path_buf()
        })
        .filter(|path| {
            !file_system_sandbox_policy.can_write_path_with_cwd(path.as_path(), cwd.as_path())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(AbsolutePathBuf::from_absolute_path)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    let permissions = (!write_paths.is_empty()).then_some(AdditionalPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(write_paths),
        )),
        ..Default::default()
    })?;

    normalize_additional_permissions(permissions).ok()
}

/// Extracts the raw patch text used as the command-shaped hook input for apply_patch.
fn apply_patch_payload_command(payload: &ToolPayload) -> Option<String> {
    match payload {
        ToolPayload::Custom { input } => Some(input.clone()),
        _ => None,
    }
}

/// 综合「会话级 + 回合级已授予权限」与「补丁实际要写的路径」，算出本次落盘的有效权限三件套。
/// 步骤：① 收集补丁触碰的全部路径；② 合并 session/turn 两层已授予权限；③ 把它们叠加到回合
/// 基线沙箱策略上得到「有效文件系统沙箱策略」；④ 在该策略下算出仍需额外申请的写权限，再经
/// apply_granted_turn_permissions 归一为 EffectiveAdditionalPermissions（含是否已预批准）。
/// 返回的 (file_paths, 额外权限, 有效策略) 会一路传给 ApplyPatchRequest 决定审批与沙箱。
async fn effective_patch_permissions(
    session: &Session,
    turn: &TurnContext,
    action: &ApplyPatchAction,
    cwd: &AbsolutePathBuf,
) -> (
    Vec<AbsolutePathBuf>,
    crate::tools::handlers::EffectiveAdditionalPermissions,
    codex_protocol::permissions::FileSystemSandboxPolicy,
) {
    let file_paths = file_paths_for_action(action);
    let granted_permissions = merge_permission_profiles(
        session.granted_session_permissions().await.as_ref(),
        session.granted_turn_permissions().await.as_ref(),
    );
    let base_file_system_sandbox_policy = turn.file_system_sandbox_policy();
    let file_system_sandbox_policy = effective_file_system_sandbox_policy(
        &base_file_system_sandbox_policy,
        granted_permissions.as_ref(),
    );
    let effective_additional_permissions = apply_granted_turn_permissions(
        session,
        cwd.as_path(),
        crate::sandboxing::SandboxPermissions::UseDefault,
        write_permissions_for_paths(&file_paths, &file_system_sandbox_policy, cwd),
    )
    .await;

    (
        file_paths,
        effective_additional_permissions,
        file_system_sandbox_policy,
    )
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for ApplyPatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("apply_patch")
    }

    fn spec(&self) -> ToolSpec {
        create_apply_patch_freeform_tool(self.multi_environment)
    }

    /// freeform `apply_patch` 工具的主入口。流程：① 取出 Custom payload 里的补丁原文；
    /// ② parse_patch 解析（失败即报错回模型）；③ 按 multi_environment 校验并解析目标环境；
    /// ④ 用该环境的文件系统 + 沙箱上下文 verify 补丁；⑤ 校验通过（Body）则算有效权限、调
    /// apply_patch::apply_patch，按返回是 Output（直接出结果）还是 DelegateToRuntime
    /// （走 orchestrator）分流；其余校验失败分支各自转成给模型的错误文本。
    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        // ① apply_patch 的参数走 Custom（自由文本），不是结构化 JSON；其余 payload 形态不接。
        let ToolPayload::Custom { input: patch_input } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "apply_patch handler received unsupported payload".to_string(),
            ));
        };
        // ② 解析补丁文本为结构化 action（含 cwd、environment_id、各文件改动）。
        let args = match codex_apply_patch::parse_patch(&patch_input) {
            Ok(args) => args,
            Err(parse_error) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )));
            }
        };
        // ③ 多环境会话才允许补丁指定 environment_id，否则拒绝。
        let selected_environment_id =
            require_environment_id(args.environment_id.as_deref(), self.multi_environment)?;

        // Verify the parsed patch against the selected environment filesystem.
        // 解析出目标环境（拿到它的 cwd 和文件系统句柄）；环境不可用直接报错。
        let Some(turn_environment) =
            resolve_tool_environment(turn.as_ref(), selected_environment_id.as_deref())?
        else {
            return Err(FunctionCallError::RespondToModel(
                "apply_patch is unavailable in this session".to_string(),
            ));
        };
        let cwd = turn_environment.cwd.clone();
        let fs = turn_environment.environment.get_filesystem();
        let sandbox = turn.file_system_sandbox_context(/*additional_permissions*/ None, &cwd);
        // ④ 对照该环境真实文件系统校验补丁（上下文是否匹配、路径是否越界等）。
        match codex_apply_patch::verify_apply_patch_args(args, &cwd, fs.as_ref(), Some(&sandbox))
            .await
        {
            // ⑤ 校验通过：算出有效权限，交给 apply_patch::apply_patch 决定执行方式。
            codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                let (file_paths, effective_additional_permissions, file_system_sandbox_policy) =
                    effective_patch_permissions(session.as_ref(), turn.as_ref(), &changes, &cwd)
                        .await;
                match apply_patch::apply_patch(turn.as_ref(), &file_system_sandbox_policy, changes)
                    .await
                {
                    // 分支 A：无需沙箱/审批，apply_patch 已直接落盘并给出结果文本。
                    InternalApplyPatchInvocation::Output(item) => {
                        let content = item?;
                        Ok(boxed_tool_output(ApplyPatchToolOutput::from_text(content)))
                    }
                    // 分支 B：需要走「审批→沙箱→重试」标准链路，委托给 ApplyPatchRuntime。
                    InternalApplyPatchInvocation::DelegateToRuntime(apply) => {
                        let changes = convert_apply_patch_to_protocol(&apply.action);
                        let emitter =
                            ToolEmitter::apply_patch(changes.clone(), apply.auto_approved);
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        emitter.begin(event_ctx).await;

                        let req = ApplyPatchRequest {
                            turn_environment: turn_environment.clone(),
                            action: apply.action,
                            file_paths,
                            changes,
                            exec_approval_requirement: apply.exec_approval_requirement,
                            additional_permissions: effective_additional_permissions
                                .additional_permissions,
                            permissions_preapproved: effective_additional_permissions
                                .permissions_preapproved,
                        };

                        let mut orchestrator = ToolOrchestrator::new();
                        let mut runtime = ApplyPatchRuntime::new();
                        let tool_ctx = ToolCtx {
                            session: session.clone(),
                            turn: turn.clone(),
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                        };
                        let out = orchestrator
                            .run(
                                &mut runtime,
                                &req,
                                &tool_ctx,
                                turn.as_ref(),
                                turn.approval_policy.value(),
                            )
                            .await
                            .map(|result| result.output);
                        // 成功取 output 自带的 delta；失败则改用 runtime 已提交的 delta——
                        // 补丁可能「部分落盘」，这份增量要照样喂给 emitter，让 UI/diff 反映
                        // 真实已改动的内容，而不是当作什么都没发生。
                        let (out, delta) = match out {
                            Ok(output) => (Ok(output.exec_output), Some(output.delta)),
                            Err(error) => (Err(error), Some(runtime.committed_delta().clone())),
                        };
                        let event_ctx = ToolEventCtx::new(
                            session.as_ref(),
                            turn.as_ref(),
                            &call_id,
                            Some(&tracker),
                        );
                        let content = emitter.finish(event_ctx, out, delta.as_ref()).await?;
                        Ok(boxed_tool_output(ApplyPatchToolOutput::from_text(content)))
                    }
                }
            }
            codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )))
            }
            codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                tracing::trace!("Failed to parse apply_patch input, {error:?}");
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received invalid patch input".to_string(),
                ))
            }
            codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received non-apply_patch input".to_string(),
                ))
            }
        }
    }
}

// ── CoreToolRuntime：把 apply_patch 接入 registry 的生命周期与 hook 体系 ──
// matches_kind 声明「我只认 Custom payload」；create_diff_consumer 提供上面的流式预览
// 消费者；pre/post_tool_use_payload 把补丁原文包成 PreToolUse/PostToolUse hook 的输入
// （工具名固定为 apply_patch）；with_updated_hook_input 允许 hook 改写补丁后再执行。
impl CoreToolRuntime for ApplyPatchHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        Some(Box::<ApplyPatchArgumentDiffConsumer>::default())
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        apply_patch_payload_command(&invocation.payload).map(|command| PreToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: serde_json::json!({ "command": command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let patch = updated_hook_command(&updated_input)?;
        invocation.payload = match invocation.payload {
            ToolPayload::Custom { .. } => ToolPayload::Custom {
                input: patch.to_string(),
            },
            payload => payload,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let tool_response =
            result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: serde_json::json!({
                "command": apply_patch_payload_command(&invocation.payload)?,
            }),
            tool_response,
        })
    }
}

/// 第二入口：shell 工具收到形如 `apply_patch <<'EOF' …` 的命令时，在真正交给 shell 执行
/// 前先来这里「拦截」。用 maybe_parse_apply_patch_verified 尝试把命令解析+校验成补丁：
///   · Body：确实是合法补丁 → 走与 handle() 完全相同的「算权限 → apply_patch → Output /
///     DelegateToRuntime」逻辑，返回 Some(output) 表示「已接管，shell 不用再跑了」。
///   · ShellParseError / NotApplyPatch：不是补丁（或不像）→ 返回 Ok(None)，放行回 shell。
///   · CorrectnessError：像补丁但内容有误 → 直接报错回模型（不放行，避免误当普通命令执行）。
/// 与 handle() 的差异仅在入参形态（已有 fs/turn_environment、call_id 是 &str），核心机制同源。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn intercept_apply_patch(
    command: &[String],
    cwd: &AbsolutePathBuf,
    fs: &dyn ExecutorFileSystem,
    turn_environment: TurnEnvironment,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: Option<&SharedTurnDiffTracker>,
    call_id: &str,
    tool_name: &str,
) -> Result<Option<FunctionToolOutput>, FunctionCallError> {
    let sandbox = turn.file_system_sandbox_context(/*additional_permissions*/ None, cwd);
    match codex_apply_patch::maybe_parse_apply_patch_verified(command, cwd, fs, Some(&sandbox))
        .await
    {
        codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
            let (approval_keys, effective_additional_permissions, file_system_sandbox_policy) =
                effective_patch_permissions(session.as_ref(), turn.as_ref(), &changes, cwd).await;
            match apply_patch::apply_patch(turn.as_ref(), &file_system_sandbox_policy, changes)
                .await
            {
                InternalApplyPatchInvocation::Output(item) => {
                    let content = item?;
                    Ok(Some(FunctionToolOutput::from_text(content, Some(true))))
                }
                InternalApplyPatchInvocation::DelegateToRuntime(apply) => {
                    let changes = convert_apply_patch_to_protocol(&apply.action);
                    let emitter = ToolEmitter::apply_patch(changes.clone(), apply.auto_approved);
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id,
                        tracker.as_ref().copied(),
                    );
                    emitter.begin(event_ctx).await;

                    let req = ApplyPatchRequest {
                        turn_environment,
                        action: apply.action,
                        file_paths: approval_keys,
                        changes,
                        exec_approval_requirement: apply.exec_approval_requirement,
                        additional_permissions: effective_additional_permissions
                            .additional_permissions,
                        permissions_preapproved: effective_additional_permissions
                            .permissions_preapproved,
                    };

                    let mut orchestrator = ToolOrchestrator::new();
                    let mut runtime = ApplyPatchRuntime::new();
                    let tool_ctx = ToolCtx {
                        session: session.clone(),
                        turn: turn.clone(),
                        call_id: call_id.to_string(),
                        tool_name: ToolName::plain(tool_name),
                    };
                    let out = orchestrator
                        .run(
                            &mut runtime,
                            &req,
                            &tool_ctx,
                            turn.as_ref(),
                            turn.approval_policy.value(),
                        )
                        .await
                        .map(|result| result.output);
                    let (out, delta) = match out {
                        Ok(output) => (Ok(output.exec_output), Some(output.delta)),
                        Err(error) => (Err(error), Some(runtime.committed_delta().clone())),
                    };
                    let event_ctx = ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id,
                        tracker.as_ref().copied(),
                    );
                    let content = emitter.finish(event_ctx, out, delta.as_ref()).await?;
                    Ok(Some(FunctionToolOutput::from_text(content, Some(true))))
                }
            }
        }
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
            Err(FunctionCallError::RespondToModel(format!(
                "apply_patch verification failed: {parse_error}"
            )))
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
            tracing::trace!("Failed to parse apply_patch input, {error:?}");
            Ok(None)
        }
        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => Ok(None),
    }
}

/// 校验补丁里的 environment_id 与本回合能力是否匹配：补丁指定了 id 但当前不允许多环境 →
/// 报错；允许或没指定 → 原样返回（None 表示用默认环境）。
fn require_environment_id(
    parsed_environment_id: Option<&str>,
    allow_environment_id: bool,
) -> Result<Option<String>, FunctionCallError> {
    match parsed_environment_id {
        Some(_) if !allow_environment_id => Err(FunctionCallError::RespondToModel(
            "apply_patch environment selection is unavailable for this turn".to_string(),
        )),
        Some(environment_id) => Ok(Some(environment_id.to_string())),
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
