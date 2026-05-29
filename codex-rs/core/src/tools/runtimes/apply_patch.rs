//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
//!
//! 【文件职责】apply_patch 的「运行时」：在 orchestrator 调度下，把已通过
//! 安全裁决的补丁真正落盘，并产出工具输出 + 已提交变更 delta。
//!
//! 【架构位置】
//!   层级：工具执行层（实际写文件系统的一端）
//!   上游：`core/src/apply_patch.rs` 决策后转交（DelegateToRuntime）；
//!         orchestrator 通过 `Sandboxable`/`Approvable`/`ToolRuntime` 三套 trait 驱动
//!   下游：`codex_apply_patch::apply_patch`（真正解析+落盘）；
//!         `turn_diff_tracker` 消费这里产出的 `AppliedPatchDelta`
//!
//! 【前提】补丁的「能不能改、要不要审批」已在上游 `assess_patch_safety` 决定，
//!   本文件不再重复安全判断（审批的具体「弹框/缓存」流程仍在此实现，
//!   见 `start_approval_async`）。本地与远程回合复用同一套环境文件系统，
//!   沙箱由显式的 `FileSystemSandboxContext` 强制约束。
//!
//! 【阅读建议】按 orchestrator 的调用顺序看三个 trait 实现：
//!   `Approvable`（审批是否放行）→ `Sandboxable`（沙箱偏好/失败升级）→
//!   `ToolRuntime::run`（真正落盘）。`run` 是核心。
use crate::exec::is_likely_sandbox_denied;
use crate::guardian::GuardianApprovalRequest;
use crate::guardian::review_approval_request;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::time::Instant;

/// 审批缓存的键：以「环境 + 单个文件路径」为粒度。
/// 一次补丁可能涉及多个文件，故 `approval_keys` 会为每个路径生成一把键，
/// 让「已批准过的文件」在同环境内复用批准结果（见 `with_cached_approval`）。
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    environment_id: String,
    path: AbsolutePathBuf,
}

/// 运行时执行一次补丁所需的全部输入。
/// 由上游决策层组装：`action` 是待落盘的补丁，`file_paths`/`changes` 用于
/// 审批展示与缓存键，`exec_approval_requirement` 携带上游已定的审批结论，
/// `permissions_preapproved` 表示权限是否已预批（可跳过再次询问）。
#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub turn_environment: TurnEnvironment,
    pub action: ApplyPatchAction,
    pub file_paths: Vec<AbsolutePathBuf>,
    pub changes: std::collections::HashMap<PathBuf, FileChange>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

/// apply_patch 运行时本体。
/// 唯一可变状态是 `committed_delta`：跨多次 `run` 累积「本运行时已落盘的全部变更」，
/// 沙箱失败升级重试时，先前成功的前缀变更也已记入其中（见 `run` 里的 `append`）。
#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
}

/// 运行时输出：标准工具执行结果（含 stdout/stderr/退出码）+ 本次累积到的 delta。
/// `delta` 交给 `turn_diff_tracker` 维护跨回合净 diff。
#[derive(Debug)]
pub struct ApplyPatchRuntimeOutput {
    pub exec_output: ExecToolCallOutput,
    pub delta: AppliedPatchDelta,
}

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回迄今为止本运行时已落盘的累积变更（供调用方读取，不可变借用）。
    pub fn committed_delta(&self) -> &AppliedPatchDelta {
        &self.committed_delta
    }

    /// 构造交给 Guardian（安全护栏）复核的请求载荷。
    /// 仅在存在 `guardian_review_id` 时走此路径，把补丁原文/路径/cwd 打包送审。
    fn build_guardian_review_request(
        req: &ApplyPatchRequest,
        call_id: &str,
    ) -> GuardianApprovalRequest {
        GuardianApprovalRequest::ApplyPatch {
            id: call_id.to_string(),
            cwd: req.action.cwd.clone(),
            files: req.file_paths.clone(),
            patch: req.action.patch.clone(),
        }
    }

    /// 为「本次沙箱尝试」构造文件系统沙箱上下文。
    /// 返回 `None` 表示本次不启用沙箱（`SandboxType::None`），落盘将不受 FS 约束。
    /// 否则把本次尝试的权限画像（叠加额外权限）、cwd、各平台沙箱开关打包，
    /// 交给底层 `codex_apply_patch::apply_patch` 在沙箱内执行。
    fn file_system_sandbox_context_for_attempt(
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if attempt.sandbox == SandboxType::None {
            return None;
        }

        let permissions =
            effective_permission_profile(attempt.permissions, req.additional_permissions.as_ref());
        Some(FileSystemSandboxContext {
            permissions,
            cwd: Some(attempt.sandbox_cwd.clone()),
            windows_sandbox_level: attempt.windows_sandbox_level,
            windows_sandbox_private_desktop: attempt.windows_sandbox_private_desktop,
            use_legacy_landlock: attempt.use_legacy_landlock,
        })
    }
}

// ───────────────────────────────────────────────────────────────
// Sandboxable  ·  沙箱偏好与失败升级
// ───────────────────────────────────────────────────────────────

impl Sandboxable for ApplyPatchRuntime {
    // 偏好让 orchestrator 自动决定是否加沙箱（依审批策略/环境）。
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    // 沙箱内失败时允许「升级」重试（如降沙箱/请求更高权限），
    // 因为补丁失败常因沙箱拒绝写入，升级后多半能成功。
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

// ───────────────────────────────────────────────────────────────
// Approvable  ·  审批是否放行（弹框 / 缓存 / Guardian 复核）
// ───────────────────────────────────────────────────────────────

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    type ApprovalKey = ApplyPatchApprovalKey;

    /// 为补丁涉及的每个文件路径各生成一把审批缓存键（同环境内可复用批准）。
    fn approval_keys(&self, req: &ApplyPatchRequest) -> Vec<Self::ApprovalKey> {
        req.file_paths
            .iter()
            .cloned()
            .map(|path| ApplyPatchApprovalKey {
                environment_id: req.turn_environment.environment_id.clone(),
                path,
            })
            .collect()
    }

    /// 异步求取本次补丁的审批结论（`ReviewDecision`）。按优先级短路判断：
    ///   ① 有 Guardian 复核 ID → 交安全护栏复核；
    ///   ② 权限已预批且非重试 → 直接通过，不打扰用户；
    ///   ③ 处于重试（带 `retry_reason`）→ 带原因重新询问，且不走缓存
    ///      （上次可能正是被拒，缓存会污染本次决定）；
    ///   ④ 常规首次询问 → 经 `with_cached_approval` 询问并缓存结果。
    ///
    /// 返回 `BoxFuture` 是为满足 trait 的对象安全（dyn-compatible）要求。
    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ApplyPatchRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        let retry_reason = ctx.retry_reason.clone();
        let approval_keys = self.approval_keys(req);
        let changes = req.changes.clone();
        let guardian_review_id = ctx.guardian_review_id.clone();
        Box::pin(async move {
            // 分支①：Guardian 复核优先于一切人工/缓存审批。
            if let Some(review_id) = guardian_review_id {
                let action = ApplyPatchRuntime::build_guardian_review_request(req, ctx.call_id);
                return review_approval_request(session, turn, review_id, action, retry_reason)
                    .await;
            }
            // 分支②：权限已预批且非重试，直接放行。
            if req.permissions_preapproved && retry_reason.is_none() {
                return ReviewDecision::Approved;
            }
            // 分支③：重试场景——带 `reason` 重新询问用户，刻意不读缓存。
            if let Some(reason) = retry_reason {
                let rx_approve = session
                    .request_patch_approval(
                        turn,
                        call_id,
                        changes.clone(),
                        Some(reason),
                        /*grant_root*/ None,
                    )
                    .await;
                return rx_approve.await.unwrap_or_default();
            }

            // 分支④：常规首次询问，结果按 approval_keys 缓存以便同环境复用。
            with_cached_approval(
                &session.services,
                "apply_patch",
                approval_keys,
                || async move {
                    let rx_approve = session
                        .request_patch_approval(
                            turn, call_id, changes, /*reason*/ None, /*grant_root*/ None,
                        )
                        .await;
                    rx_approve.await.unwrap_or_default()
                },
            )
            .await
        })
    }

    /// 在「不加沙箱执行」这一前提下，给定审批策略是否仍需向用户征求同意。
    /// 仅 `Never`（从不询问）返回 false；其余策略下「无沙箱」都属于需用户确认的情形。
    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnFailure => true,
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    // apply_patch 的审批结论由上游 `assess_patch_safety` 决定。
    // 这里覆盖默认实现，是为了让 orchestrator 在需要时走「补丁审批」流程，
    // 而不是回退到全局的 exec 审批策略——两者门控逻辑不同，不能混用。
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }

    /// 提供给权限钩子（hook）的请求载荷：声明这是 apply_patch 工具，
    /// 并把补丁原文作为 `command` 传入，供外部钩子据此做权限决策。
    fn permission_request_payload(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<PermissionRequestPayload> {
        Some(PermissionRequestPayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: serde_json::json!({ "command": req.action.patch }),
        })
    }
}

// ───────────────────────────────────────────────────────────────
// ToolRuntime  ·  真正落盘执行
// ───────────────────────────────────────────────────────────────

impl ToolRuntime<ApplyPatchRequest, ApplyPatchRuntimeOutput> for ApplyPatchRuntime {
    // 沙箱以补丁的 cwd 作为工作目录。
    fn sandbox_cwd<'a>(&self, req: &'a ApplyPatchRequest) -> Option<&'a AbsolutePathBuf> {
        Some(&req.action.cwd)
    }

    /// 在本次沙箱尝试下执行补丁并落盘，产出工具输出 + 累积 delta。
    ///
    /// @param req     - 待执行的补丁请求
    /// @param attempt - 本次沙箱尝试的环境（沙箱类型、权限、cwd 等）
    /// @returns       - 成功返回 `ApplyPatchRuntimeOutput`；疑似沙箱拒绝则返回
    ///                  `SandboxErr::Denied`，交由上层触发升级重试
    ///
    /// 副作用：写文件系统；就地累加 `self.committed_delta`。
    /// 关键设计：即便补丁中途失败，也要把「失败前已落盘的前缀变更」并入
    /// `committed_delta`（见下 Step 3），否则跨回合 diff 会漏掉这些已生效改动。
    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ApplyPatchRuntimeOutput, ToolError> {
        let started_at = Instant::now();
        // Step 1：取本回合环境的文件系统句柄 + 构造本次沙箱上下文，
        // 委托底层 crate 在沙箱内解析并落盘补丁（stdout/stderr 写入缓冲区）。
        let fs = req.turn_environment.environment.get_filesystem();
        let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = codex_apply_patch::apply_patch(
            &req.action.patch,
            &req.action.cwd,
            &mut stdout,
            &mut stderr,
            fs.as_ref(),
            sandbox.as_ref(),
        )
        .await;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let failed = result.is_err();
        let exit_code = if failed { 1 } else { 0 };
        // Step 2：无论成败都取出 delta——失败也携带「已落盘部分」（`into_parts().1`）。
        let delta = match result {
            Ok(delta) => delta,
            Err(failure) => failure.into_parts().1,
        };
        // Step 3：累加到本运行时的提交记录，再组装标准工具输出（含耗时）。
        self.committed_delta.append(delta);
        let output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(stdout.clone()),
            stderr: StreamOutput::new(stderr.clone()),
            aggregated_output: StreamOutput::new(format!("{stdout}{stderr}")),
            duration: started_at.elapsed(),
            timed_out: false,
        };
        // Step 4：若失败且像是「沙箱拒绝」，上报 Denied 让 orchestrator 走升级重试；
        // 其余失败（如补丁本身不匹配）按普通 Ok 输出回模型，由模型自行修正。
        if failed && is_likely_sandbox_denied(attempt.sandbox, &output) {
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output: Box::new(output),
                network_policy_decision: None,
            })));
        }
        Ok(ApplyPatchRuntimeOutput {
            exec_output: output,
            delta: self.committed_delta.clone(),
        })
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
