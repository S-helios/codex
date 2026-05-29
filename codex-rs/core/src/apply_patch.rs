//! 【文件职责】`apply_patch` 在 core 侧的「安全决策 + 协议转换」胶水层，
//! 把独立 crate `codex-apply-patch` 解析出的补丁动作接入 Codex 的权限/沙箱体系。
//!
//! 【架构位置】
//!   层级：工具执行层（apply_patch 工具的前置门控）
//!   上游：`tools/handlers/apply_patch.rs`（工具入口，已注释，勿动）
//!   下游：`safety::assess_patch_safety`（安全裁决）、
//!         `tools/runtimes/apply_patch.rs`（真正落盘的运行时）
//!
//! 【数据流】
//!   ApplyPatchAction → assess_patch_safety() →
//!     ├─ 自动批准 / 需询问 → DelegateToRuntime（交运行时落盘）
//!     └─ 拒绝            → Output(Err)（直接回模型一条 patch rejected）
//!
//! 【阅读建议】先看 `apply_patch()` 的三分支裁决，再看
//!             `convert_apply_patch_to_protocol()`（把 crate 内部变更枚举
//!             转成对外 `protocol::FileChange`，供 UI/审批展示）。
//!
//! 注意：本文件本身不写文件系统——它只做「准不准、怎么转」的决策，
//!       真正的落盘在 runtimes/apply_patch.rs。
use crate::function_tool::FunctionCallError;
use crate::safety::SafetyCheck;
use crate::safety::assess_patch_safety;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use std::collections::HashMap;
use std::path::PathBuf;

/// 安全裁决后的两种归宿：要么已有现成结果直接回模型，要么转交运行时落盘。
/// 之所以叫 `Internal`：这是 core 内部在 shell/apply_patch 入口处的中间结果，
/// 不对外暴露给协议层。
pub(crate) enum InternalApplyPatchInvocation {
    /// The `apply_patch` call was handled programmatically, without any sort
    /// of sandbox, because the user explicitly approved it. This is the
    /// result to use with the `shell` function call that contained `apply_patch`.
    /// 补丁已在程序内直接得出结论（如被拒绝），无需走沙箱。
    /// 这是「shell 调用里内联了 apply_patch」场景要回给模型的现成结果。
    Output(Result<String, FunctionCallError>),

    /// The `apply_patch` call was approved, either automatically because it
    /// appears that it should be allowed based on the user's sandbox policy
    /// *or* because the user explicitly approved it. The runtime realizes the
    /// patch through the selected environment filesystem.
    /// 补丁已获批（自动批准 or 用户显式批准 or 留待运行时询问），
    /// 交给运行时通过所选环境的文件系统真正落盘。
    DelegateToRuntime(ApplyPatchRuntimeInvocation),
}

/// 交付给运行时的「补丁 + 审批结论」打包。
/// `auto_approved` 标记本次是否系统自动放行（用于事件/遥测区分人工与自动），
/// `exec_approval_requirement` 决定运行时是否还要再走一遍审批询问。
#[derive(Debug)]
pub(crate) struct ApplyPatchRuntimeInvocation {
    pub(crate) action: ApplyPatchAction,
    pub(crate) auto_approved: bool,
    pub(crate) exec_approval_requirement: ExecApprovalRequirement,
}

/// 对单次补丁做安全裁决，产出「直接回模型」或「转交运行时」的归宿。
///
/// @param turn_context              - 当前回合上下文，提供审批策略、权限画像等
/// @param file_system_sandbox_policy - 文件系统沙箱策略，参与可否自动放行的判断
/// @param action                    - 已解析好的补丁动作（路径 + 各文件变更）
/// @returns                         - 三态裁决见 `InternalApplyPatchInvocation`
///
/// 设计要点：本函数只决策、不落盘。`AskUser` 分支也走 `DelegateToRuntime`，
/// 把「弹审批框（含缓存批准）」的职责下沉到运行时，与 shell/unified_exec
/// 的审批同样由 orchestrator 驱动，保持各工具审批路径一致。
pub(crate) async fn apply_patch(
    turn_context: &TurnContext,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    action: ApplyPatchAction,
) -> InternalApplyPatchInvocation {
    match assess_patch_safety(
        &action,
        turn_context.approval_policy.value(),
        &turn_context.permission_profile(),
        file_system_sandbox_policy,
        &action.cwd,
        turn_context.windows_sandbox_level,
    ) {
        // 分支一：可放行（合策略自动放行 or 用户已显式批准）。
        // 直接转运行时并跳过再次审批（Skip）；`auto_approved` 取
        // 「非用户显式批准」即「系统自动放行」，用于后续事件区分来源。
        SafetyCheck::AutoApprove {
            user_explicitly_approved,
            ..
        } => InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
            action,
            auto_approved: !user_explicitly_approved,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        }),
        // 分支二：需要询问用户。
        SafetyCheck::AskUser => {
            // Delegate the approval prompt (including cached approvals) to the
            // tool runtime, consistent with how shell/unified_exec approvals
            // are orchestrator-driven.
            InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
                action,
                auto_approved: false,
                exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
                    reason: None,
                    proposed_execpolicy_amendment: None,
                },
            })
        }
        // 分支三：直接拒绝（如越权写入沙箱外路径）。不落盘，
        // 把拒因原样回给模型，让模型自行调整补丁后重试。
        SafetyCheck::Reject { reason } => InternalApplyPatchInvocation::Output(Err(
            FunctionCallError::RespondToModel(format!("patch rejected: {reason}")),
        )),
    }
}

/// 把 crate 内部的 `ApplyPatchFileChange` 转成对外协议的 `FileChange`。
/// 两者结构相近，差异在于：内部 `Update` 带 `new_content`（落盘用的完整新内容），
/// 而协议层只需 `unified_diff` + 可选 `move_path` 用于 UI 展示/审批，
/// 故此处刻意丢弃 `new_content`（见下方 `_new_content` 占位）。
pub(crate) fn convert_apply_patch_to_protocol(
    action: &ApplyPatchAction,
) -> HashMap<PathBuf, FileChange> {
    let mut result = HashMap::with_capacity(action.changes().len());
    for (path, change) in action.changes() {
        let protocol_change = match change {
            ApplyPatchFileChange::Add { content, .. } => FileChange::Add {
                content: content.clone(),
            },
            ApplyPatchFileChange::Delete { content } => FileChange::Delete {
                content: content.clone(),
            },
            ApplyPatchFileChange::Update {
                unified_diff,
                move_path,
                new_content: _new_content,
            } => FileChange::Update {
                unified_diff: unified_diff.clone(),
                move_path: move_path.clone(),
            },
        };
        result.insert(path.to_path_buf(), protocol_change);
    }
    result
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
