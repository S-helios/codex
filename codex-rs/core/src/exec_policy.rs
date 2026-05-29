//! 【文件职责】execpolicy 的 core 侧「翻译层」与门面：把 execpolicy crate 输出
//! 的三值 `Decision`（Allow/Prompt/Forbidden）翻译成 core 内部真正的审批要求
//! `ExecApprovalRequirement`（跳过/需审批/禁止），并管理策略的加载、热更新、
//! 以及「用户放行后自动追加规则」的推导。
//!
//! 【架构位置】
//!   层级：执行与安全层 · 命令执行策略
//!   上游：工具执行链（shell/apply_patch 等运行前调
//!         `create_exec_approval_requirement_for_command` 拿审批结论）
//!   下游：`codex_execpolicy` crate（`Policy` / `Decision` / 规则匹配）、
//!         `codex_shell_command`（命令解析、安全/危险启发式）
//!
//! 【三道关卡里的位置】这是「execpolicy 关卡」的 core 落点。它回答两个问题：
//!   (1) 这条命令该 允许 / 提示 / 禁止？——委托给 `Policy` 匹配，未命中显式
//!       规则时由本文件的 `render_decision_for_unmatched_command` 启发式兜底。
//!   (2) 「提示」要不要真弹给用户？——由 `AskForApproval` 配置裁剪：例如
//!       `Never` 下「提示」会被降级为「禁止」（见 `prompt_is_rejected_by_policy`）。
//!
//! 【数据流】命令 argv → `commands_for_exec_policy`（按 shell 语义拆成多段）
//!   → `Policy::check_multiple_with_options`（+启发式兜底）→ `Evaluation`
//!   → 按 `Decision` 与 `AskForApproval` 映射 → `ExecApprovalRequirement`
//!   （顺带推导可选的「自动放行 amendment」）
//!
//! 【阅读建议】先看 `ExecPolicyManager::create_exec_approval_requirement_for_command`
//! （翻译主流程，全文核心），再看 `render_decision_for_unmatched_command`
//! （未命中规则时的启发式裁决矩阵）；`try_derive_*_amendment*` 是「自动加规则」
//! 推导，`load_exec_policy` / `collect_policy_files` 是加载侧，可后看。
//! 对照 `learn_docs/3_执行与安全/18_exec_and_safety.md` 第 3、4 节。

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;

use codex_app_server_protocol::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ConfigLayerStackOrdering;
use codex_execpolicy::AmendError;
use codex_execpolicy::Decision;
use codex_execpolicy::Error as ExecPolicyRuleError;
use codex_execpolicy::Evaluation;
use codex_execpolicy::MatchOptions;
use codex_execpolicy::NetworkRuleProtocol;
use codex_execpolicy::Policy;
use codex_execpolicy::PolicyParser;
use codex_execpolicy::RuleMatch;
use codex_execpolicy::blocking_append_allow_prefix_rule;
use codex_execpolicy::blocking_append_network_rule;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_shell_command::is_dangerous_command::command_might_be_dangerous;
use codex_shell_command::is_safe_command::is_known_safe_command;
use thiserror::Error;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;
use tracing::instrument;

use crate::config::Config;
use crate::sandboxing::SandboxPermissions;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_shell_command::bash::parse_shell_lc_plain_commands;
use codex_shell_command::bash::parse_shell_lc_single_command_prefix;
use codex_utils_absolute_path::AbsolutePathBuf;
use shlex::try_join as shlex_try_join;

// 三条「策略要求审批、但审批被配置禁掉」时回给用户的拒绝理由。
// 当裁决是 Prompt 却无处可弹（Never / Granular 关闭了对应审批开关）时，
// 命令被降级为 Forbidden，并附上其中一条理由解释「为何不是弹窗而是直接拒」。
const PROMPT_CONFLICT_REASON: &str =
    "approval required by policy, but AskForApproval is set to Never";
const REJECT_SANDBOX_APPROVAL_REASON: &str =
    "approval required by policy, but AskForApproval::Granular.sandbox_approval is false";
const REJECT_RULES_APPROVAL_REASON: &str =
    "approval required by policy rule, but AskForApproval::Granular.rules is false";
// 规则文件的目录名与扩展名约定：`<config>/rules/*.rules`，默认写入 default.rules。
const RULES_DIR_NAME: &str = "rules";
const RULE_EXTENSION: &str = "rules";
const DEFAULT_POLICY_FILE: &str = "default.rules";
// 「禁止自动建议为放行前缀」的黑名单。
// 背景：用户放行某命令后，系统会尝试自动追加一条 allow 规则方便下次（amendment）。
// 但若把这些前缀加进白名单，等于放开任意代码执行——它们要么是解释器/shell
// （python/bash/node/ruby... 配 -c/-e/-lc 可跑任意脚本），要么是提权/包装器
// （sudo/env/git）。对这些前缀坚决不自动建议，必须每次单独审批。
// [引用范围] 仅被 `derive_requested_execpolicy_amendment_from_prefix_rule` 读取。
static BANNED_PREFIX_SUGGESTIONS: &[&[&str]] = &[
    &["python3"],
    &["python3", "-"],
    &["python3", "-c"],
    &["python"],
    &["python", "-"],
    &["python", "-c"],
    &["py"],
    &["py", "-3"],
    &["pythonw"],
    &["pyw"],
    &["pypy"],
    &["pypy3"],
    &["git"],
    &["bash"],
    &["bash", "-lc"],
    &["sh"],
    &["sh", "-c"],
    &["sh", "-lc"],
    &["zsh"],
    &["zsh", "-lc"],
    &["/bin/zsh"],
    &["/bin/zsh", "-lc"],
    &["/bin/bash"],
    &["/bin/bash", "-lc"],
    &["pwsh"],
    &["pwsh", "-Command"],
    &["pwsh", "-c"],
    &["powershell"],
    &["powershell", "-Command"],
    &["powershell", "-c"],
    &["powershell.exe"],
    &["powershell.exe", "-Command"],
    &["powershell.exe", "-c"],
    &["env"],
    &["sudo"],
    &["node"],
    &["node", "-e"],
    &["perl"],
    &["perl", "-e"],
    &["ruby"],
    &["ruby", "-e"],
    &["php"],
    &["php", "-r"],
    &["lua"],
    &["lua", "-e"],
    &["osascript"],
];

/// Describes which unmatched-command heuristics should classify the command
/// words being evaluated by exec-policy.
///
/// The command tokens may be the original argv or a shell-specific lowering of
/// a wrapper such as `bash -lc ...` or `powershell.exe -Command ...`. We only
/// need to distinguish the PowerShell case because its safelist and dangerous
/// heuristics operate on PowerShell-flavored inner command words rather than
/// the generic command classifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecPolicyCommandOrigin {
    /// Use the generic unmatched-command heuristics.
    Generic,
    #[cfg(windows)]
    /// The command words came from the `-Command` body of a top-level
    /// PowerShell wrapper, so use PowerShell-specific unmatched-command
    /// heuristics for the lowered words.
    PowerShell,
}

/// 未命中显式规则、走启发式兜底时所需的全部上下文。
/// `render_decision_for_unmatched_command` 据此在「安全/危险启发式 ×
/// 审批策略 × 沙箱形态」的矩阵里推出一个 `Decision`。
/// `used_complex_parsing` 标记命令是否经过 heredoc 等复杂回退解析（为真时
/// 不允许据此自动加规则，避免误放行）。
#[derive(Clone, Copy)]
pub(crate) struct UnmatchedCommandContext<'a> {
    pub(crate) approval_policy: AskForApproval,
    pub(crate) permission_profile: &'a PermissionProfile,
    pub(crate) file_system_sandbox_policy: &'a FileSystemSandboxPolicy,
    pub(crate) sandbox_cwd: &'a Path,
    pub(crate) sandbox_permissions: SandboxPermissions,
    pub(crate) used_complex_parsing: bool,
    pub(crate) command_origin: ExecPolicyCommandOrigin,
}

/// `commands_for_exec_policy` 的解析产物：原始 argv 被按 shell 语义拆成的
/// 若干条命令段，外加两个元信息——是否用了复杂回退解析、命令来源（影响启发式
/// 选择 Generic 还是 PowerShell）。
#[derive(Debug, Eq, PartialEq)]
struct ExecPolicyCommands {
    commands: Vec<Vec<String>>,
    used_complex_parsing: bool,
    command_origin: ExecPolicyCommandOrigin,
}

/// 判断子配置是否与父配置使用「同一套 exec policy」。
/// 三项都相同才算同源：参与加载的配置目录列表、是否忽略用户/项目层规则、
/// requirements 强制策略。用途：子 thread 沿用父策略时可跳过重复加载/校验。
pub(crate) fn child_uses_parent_exec_policy(parent_config: &Config, child_config: &Config) -> bool {
    fn exec_policy_config_folders(config: &Config) -> Vec<AbsolutePathBuf> {
        config
            .config_layer_stack
            .get_layers(
                ConfigLayerStackOrdering::LowestPrecedenceFirst,
                /*include_disabled*/ false,
            )
            .into_iter()
            .filter_map(codex_config::ConfigLayerEntry::config_folder)
            .collect()
    }

    exec_policy_config_folders(parent_config) == exec_policy_config_folders(child_config)
        && parent_config
            .config_layer_stack
            .ignore_user_and_project_exec_policy_rules()
            == child_config
                .config_layer_stack
                .ignore_user_and_project_exec_policy_rules()
        && parent_config.config_layer_stack.requirements().exec_policy
            == child_config.config_layer_stack.requirements().exec_policy
}

/// 判断一次匹配是否来自**显式策略规则**（而非启发式兜底）。
/// 全文反复用它区分二者：显式规则优先级最高、不可被自动覆盖；只有当命中全是
/// 启发式时，才允许推导「自动放行 amendment」（见各 `try_derive_*`）。
fn is_policy_match(rule_match: &RuleMatch) -> bool {
    match rule_match {
        RuleMatch::PrefixRuleMatch { .. } => true,
        RuleMatch::HeuristicsRuleMatch { .. } => false,
    }
}

/// Returns a rejection reason when `approval_policy` disallows surfacing the
/// current prompt to the user.
///
/// `prompt_is_rule` distinguishes policy-rule prompts from sandbox/escalation
/// prompts so granular `rules` and `sandbox_approval` settings are honored
/// independently. When both are present, policy-rule prompts take precedence.
/// 当审批策略不允许把「提示」弹给用户时，返回对应的拒绝理由（否则 `None`）。
///
/// 即「Prompt → 是否降级为 Forbidden」的判定：
///   - `Never`：一律不弹 → 返回理由（降级为禁止）。
///   - `OnFailure/OnRequest/UnlessTrusted`：允许弹 → `None`。
///   - `Granular`：按 `prompt_is_rule` 分流——规则类提示看 `rules` 开关，
///     沙箱/提权类提示看 `sandbox_approval` 开关；二者都在时规则类优先。
/// `None` 表示「可以弹」，`Some(reason)` 表示「不能弹，按此理由拒绝」。
pub(crate) fn prompt_is_rejected_by_policy(
    approval_policy: AskForApproval,
    prompt_is_rule: bool,
) -> Option<&'static str> {
    match approval_policy {
        AskForApproval::Never => Some(PROMPT_CONFLICT_REASON),
        AskForApproval::OnFailure => None,
        AskForApproval::OnRequest => None,
        AskForApproval::UnlessTrusted => None,
        AskForApproval::Granular(granular_config) => {
            if prompt_is_rule {
                if !granular_config.allows_rules_approval() {
                    Some(REJECT_RULES_APPROVAL_REASON)
                } else {
                    None
                }
            } else if !granular_config.allows_sandbox_approval() {
                Some(REJECT_SANDBOX_APPROVAL_REASON)
            } else {
                None
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecPolicyError {
    #[error("failed to read rules files from {dir}: {source}")]
    ReadDir {
        dir: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to read rules file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse rules file {path}: {source}")]
    ParsePolicy {
        path: String,
        source: codex_execpolicy::Error,
    },
}

#[derive(Debug, Error)]
pub enum ExecPolicyUpdateError {
    #[error("failed to update rules file {path}: {source}")]
    AppendRule { path: PathBuf, source: AmendError },

    #[error("failed to join blocking rules update task: {source}")]
    JoinBlockingTask { source: tokio::task::JoinError },

    #[error("failed to update in-memory rules: {source}")]
    AddRule {
        #[from]
        source: ExecPolicyRuleError,
    },
}

/// 进程级别的策略持有者与更新协调器。
/// - `policy: ArcSwap<Policy>`：当前生效策略。用 `ArcSwap` 让「读」无锁（每次
///   评估 `load_full` 拿快照），同时支持「写」时原子换整份策略（热更新）。
/// - `update_lock`：单许可信号量，串行化 amendment 写盘+换内存策略，避免并发
///   更新互相覆盖。
pub(crate) struct ExecPolicyManager {
    policy: ArcSwap<Policy>,
    update_lock: Semaphore,
}

/// 一次命令审批请求的入参聚合体（避免函数签名参数过多）。
/// `prefix_rule` 是 UI/调用方建议的「可放行前缀」候选，仅在满足条件时才会被
/// 真正推导成 amendment（见 `derive_requested_execpolicy_amendment_from_prefix_rule`）。
pub(crate) struct ExecApprovalRequest<'a> {
    pub(crate) command: &'a [String],
    pub(crate) approval_policy: AskForApproval,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) file_system_sandbox_policy: &'a FileSystemSandboxPolicy,
    pub(crate) sandbox_cwd: &'a Path,
    pub(crate) sandbox_permissions: SandboxPermissions,
    pub(crate) prefix_rule: Option<Vec<String>>,
}

impl ExecPolicyManager {
    pub(crate) fn new(policy: Arc<Policy>) -> Self {
        Self {
            policy: ArcSwap::from(policy),
            update_lock: Semaphore::new(/*permits*/ 1),
        }
    }

    #[instrument(level = "info", skip_all)]
    pub(crate) async fn load(config_stack: &ConfigLayerStack) -> Result<Self, ExecPolicyError> {
        let (policy, warning) = load_exec_policy_with_warning(config_stack).await?;
        if let Some(err) = warning.as_ref() {
            tracing::warn!("failed to parse rules: {err}");
        }
        Ok(Self::new(Arc::new(policy)))
    }

    pub(crate) fn current(&self) -> Arc<Policy> {
        self.policy.load_full()
    }

    /// 本文件核心：把一条待执行命令翻译成审批要求 `ExecApprovalRequirement`。
    ///
    /// @param req - 命令、审批策略、权限画像、沙箱信息等聚合入参
    /// @returns   - `Skip`（可执行，可能附带「自动放行 amendment」）/
    ///              `NeedsApproval`（需弹窗）/ `Forbidden`（直接拒）
    ///
    /// 流程：拆命令段 → 逐段评估（显式规则 + 启发式兜底）→ 按 `Decision` 三分支
    /// 映射成审批要求，并在允许的前提下推导可选 amendment。
    /// 注意：`Forbidden` 优先级最高；`Prompt` 可能因审批策略被降级为
    /// `Forbidden`（见 `prompt_is_rejected_by_policy`）。
    pub(crate) async fn create_exec_approval_requirement_for_command(
        &self,
        req: ExecApprovalRequest<'_>,
    ) -> ExecApprovalRequirement {
        let ExecApprovalRequest {
            command,
            approval_policy,
            permission_profile,
            file_system_sandbox_policy,
            sandbox_cwd,
            sandbox_permissions,
            prefix_rule,
        } = req;
        // 取当前策略快照（无锁读）。
        let exec_policy = self.current();
        // Step 1：按 shell 语义把原始 argv 拆成若干命令段（如 `a && b` → [a, b]）。
        let ExecPolicyCommands {
            commands,
            used_complex_parsing,
            command_origin,
        } = commands_for_exec_policy(command);
        // Keep heredoc prefix parsing for rule evaluation so existing
        // allow/prompt/forbidden rules still apply, but avoid auto-derived
        // amendments when only the heredoc fallback parser matched.
        // 保留 heredoc 前缀解析用于规则匹配（已有规则照常生效），但当命令只能靠
        // 复杂回退解析才解出来时，禁止自动推导 amendment——这类命令结构不可靠，
        // 自动放行风险太高。
        let auto_amendment_allowed = !used_complex_parsing;
        // 启发式兜底闭包：某段命令没命中任何显式规则时，由它给出裁决。
        let exec_policy_fallback = |cmd: &[String]| {
            render_decision_for_unmatched_command(
                cmd,
                UnmatchedCommandContext {
                    approval_policy,
                    permission_profile: &permission_profile,
                    file_system_sandbox_policy,
                    sandbox_cwd,
                    sandbox_permissions,
                    used_complex_parsing,
                    command_origin,
                },
            )
        };
        // resolve_host_executables=true：命令用绝对路径调用时按 basename 匹配
        // 规则（并校验路径白名单），详见 `Policy::match_host_executable_rules`。
        let match_options = MatchOptions {
            resolve_host_executables: true,
        };
        // Step 2：逐段评估并聚合。`Evaluation` 取所有命令段里最严的裁决——
        // 任一段需禁止/提示，整条命令就按更严的来。
        let evaluation = exec_policy.check_multiple_with_options(
            commands.iter(),
            &exec_policy_fallback,
            &match_options,
        );

        // Step 3（可选）：若调用方给了 prefix_rule 候选且允许自动 amendment，
        // 尝试推导出「下次自动放行」的规则建议。
        let requested_amendment = if auto_amendment_allowed {
            derive_requested_execpolicy_amendment_from_prefix_rule(
                prefix_rule.as_ref(),
                &evaluation.matched_rules,
                exec_policy.as_ref(),
                &commands,
                &exec_policy_fallback,
                &match_options,
            )
        } else {
            None
        };

        // Step 4：按聚合裁决三分支映射成最终审批要求。
        match evaluation.decision {
            // 禁止：直接拒，附「最具体的禁止规则」给出的理由。
            Decision::Forbidden => ExecApprovalRequirement::Forbidden {
                reason: derive_forbidden_reason(command, &evaluation),
            },
            Decision::Prompt => {
                // 该「提示」是否由显式策略规则驱动（而非启发式/沙箱提权）。
                // 影响下面降级判定走 `rules` 还是 `sandbox_approval` 开关。
                let prompt_is_rule = evaluation.matched_rules.iter().any(|rule_match| {
                    is_policy_match(rule_match) && rule_match.decision() == Decision::Prompt
                });
                // 审批策略可能禁止弹窗 → 降级为 Forbidden；否则需用户审批。
                match prompt_is_rejected_by_policy(approval_policy, prompt_is_rule) {
                    Some(reason) => ExecApprovalRequirement::Forbidden {
                        reason: reason.to_string(),
                    },
                    None => ExecApprovalRequirement::NeedsApproval {
                        reason: derive_prompt_reason(command, &evaluation),
                        proposed_execpolicy_amendment: requested_amendment.or_else(|| {
                            if auto_amendment_allowed {
                                try_derive_execpolicy_amendment_for_prompt_rules(
                                    &evaluation.matched_rules,
                                )
                            } else {
                                None
                            }
                        }),
                    },
                }
            }
            // 允许：跳过审批直接执行。是否绕过沙箱见下方内联说明。
            Decision::Allow => ExecApprovalRequirement::Skip {
                // Bypass sandbox only when every parsed command segment is
                // explicitly allowed by execpolicy.
                // 仅当**每一段**命令都被显式规则允许时才绕过沙箱——靠启发式
                // 兜底放行的命令仍需在沙箱内跑，保留一道防线。
                bypass_sandbox: commands.iter().all(|command| {
                    exec_policy
                        .matches_for_command_with_options(
                            command,
                            /*heuristics_fallback*/ None,
                            &match_options,
                        )
                        .iter()
                        .any(|rule_match| {
                            is_policy_match(rule_match) && rule_match.decision() == Decision::Allow
                        })
                }),
                proposed_execpolicy_amendment: if auto_amendment_allowed {
                    try_derive_execpolicy_amendment_for_allow_rules(&evaluation.matched_rules)
                } else {
                    None
                },
            },
        }
    }

    /// 应用一条 allow amendment：把前缀规则**写盘**并**热更新内存策略**。
    ///
    /// 副作用：追加写 `default.rules`；原子替换 `self.policy`。
    /// 并发：先抢 `update_lock` 串行化，避免与其它更新交错。
    /// 幂等：若当前策略已显式允许该命令，则只写盘不重复改内存（直接返回）。
    /// 阻塞 I/O 经 `spawn_blocking` 移出 async 执行器（见 amend.rs 的契约）。
    pub(crate) async fn append_amendment_and_update(
        &self,
        codex_home: &Path,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let _update_guard =
            self.update_lock
                .acquire()
                .await
                .map_err(|_| ExecPolicyUpdateError::AddRule {
                    source: ExecPolicyRuleError::InvalidRule(
                        "exec policy update semaphore closed".to_string(),
                    ),
                })?;
        let policy_path = default_policy_path(codex_home);
        spawn_blocking({
            let policy_path = policy_path.clone();
            let prefix = amendment.command.clone();
            move || blocking_append_allow_prefix_rule(&policy_path, &prefix)
        })
        .await
        .map_err(|source| ExecPolicyUpdateError::JoinBlockingTask { source })?
        .map_err(|source| ExecPolicyUpdateError::AppendRule {
            path: policy_path,
            source,
        })?;

        // 幂等检查：用一个「未命中即 Forbidden」的兜底来评估——只有命中显式
        // Allow 规则才会判定为已允许（兜底的 Forbidden 不会误判为允许）。
        let current_policy = self.current();
        let match_options = MatchOptions {
            resolve_host_executables: true,
        };
        let existing_evaluation = current_policy.check_multiple_with_options(
            [&amendment.command],
            &|_| Decision::Forbidden,
            &match_options,
        );
        let already_allowed = existing_evaluation.decision == Decision::Allow
            && existing_evaluation.matched_rules.iter().any(|rule_match| {
                is_policy_match(rule_match) && rule_match.decision() == Decision::Allow
            });
        if already_allowed {
            return Ok(());
        }

        // clone 当前策略 → 加规则 → 原子换回（ArcSwap，旧读者继续用旧快照）。
        let mut updated_policy = current_policy.as_ref().clone();
        updated_policy.add_prefix_rule(&amendment.command, Decision::Allow)?;
        self.policy.store(Arc::new(updated_policy));
        Ok(())
    }

    /// 应用一条网络规则：写盘 + 热更新内存策略。
    /// 与 `append_amendment_and_update` 同样的串行化/写盘/原子替换套路，
    /// 但作用对象是网络规则，且不做幂等去重（网络规则按定义顺序后者覆盖前者，
    /// 重复追加不会改变最终编译结果）。
    pub(crate) async fn append_network_rule_and_update(
        &self,
        codex_home: &Path,
        host: &str,
        protocol: NetworkRuleProtocol,
        decision: Decision,
        justification: Option<String>,
    ) -> Result<(), ExecPolicyUpdateError> {
        let _update_guard =
            self.update_lock
                .acquire()
                .await
                .map_err(|_| ExecPolicyUpdateError::AddRule {
                    source: ExecPolicyRuleError::InvalidRule(
                        "exec policy update semaphore closed".to_string(),
                    ),
                })?;
        let policy_path = default_policy_path(codex_home);
        let host = host.to_string();
        spawn_blocking({
            let policy_path = policy_path.clone();
            let host = host.clone();
            let justification = justification.clone();
            move || {
                blocking_append_network_rule(
                    &policy_path,
                    &host,
                    protocol,
                    decision,
                    justification.as_deref(),
                )
            }
        })
        .await
        .map_err(|source| ExecPolicyUpdateError::JoinBlockingTask { source })?
        .map_err(|source| ExecPolicyUpdateError::AppendRule {
            path: policy_path,
            source,
        })?;

        let mut updated_policy = self.current().as_ref().clone();
        updated_policy.add_network_rule(&host, protocol, decision, justification)?;
        self.policy.store(Arc::new(updated_policy));
        Ok(())
    }
}

impl Default for ExecPolicyManager {
    fn default() -> Self {
        Self::new(Arc::new(Policy::empty()))
    }
}

pub async fn check_execpolicy_for_warnings(
    config_stack: &ConfigLayerStack,
) -> Result<Option<ExecPolicyError>, ExecPolicyError> {
    let (_, warning) = load_exec_policy_with_warning(config_stack).await?;
    Ok(warning)
}

fn exec_policy_message_for_display(source: &codex_execpolicy::Error) -> String {
    let message = source.to_string();
    if let Some(line) = message
        .lines()
        .find(|line| line.trim_start().starts_with("error: "))
    {
        return line.to_owned();
    }
    if let Some(first_line) = message.lines().next()
        && let Some((_, detail)) = first_line.rsplit_once(": starlark error: ")
    {
        return detail.trim().to_string();
    }

    message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 从 Starlark 错误的文本消息里「反向解析」出文件路径与行号。
/// 用作结构化位置的兜底/补充：消息形如 `path:line:col: starlark error: ...`，
/// 这里按冒号从右往左切出 col、line、path。line 为 0 视为无效返回 `None`。
fn parse_starlark_line_from_message(message: &str) -> Option<(PathBuf, usize)> {
    let first_line = message.lines().next()?.trim();
    let (path_and_position, _) = first_line.rsplit_once(": starlark error:")?;

    let mut parts = path_and_position.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = PathBuf::from(parts.next()?);

    if line == 0 {
        return None;
    }

    Some((path, line))
}

/// 把解析类策略错误渲染成对用户友好的「file:line: 说明」单行文案。
/// 仅对 `ParsePolicy` 做精细处理；其余错误直接 `to_string()`。
///
/// 位置信息有两个来源：结构化的 `source.location()` 与从消息文本解析出的
/// `parsed_location`。当结构化只给出第 1 行（往往是「定位到文件开头」的兜底）
/// 而文本解析出更靠后的具体行时，优先采信文本解析的行号——更接近真正出错处。
pub fn format_exec_policy_error_with_source(error: &ExecPolicyError) -> String {
    match error {
        ExecPolicyError::ParsePolicy { path, source } => {
            let rendered_source = source.to_string();
            let structured_location = source
                .location()
                .map(|location| (PathBuf::from(location.path), location.range.start.line));
            let parsed_location = parse_starlark_line_from_message(&rendered_source);
            // 结构化只到第 1 行、而文本解析出 >1 的具体行时，采信后者。
            let location = match (structured_location, parsed_location) {
                (Some((_, 1)), Some((parsed_path, parsed_line))) if parsed_line > 1 => {
                    Some((parsed_path, parsed_line))
                }
                (Some(structured), _) => Some(structured),
                (None, parsed) => parsed,
            };
            let message = exec_policy_message_for_display(source);
            match location {
                Some((path, line)) => {
                    format!(
                        "{}:{}: {} (problem is on or around line {})",
                        path.display(),
                        line,
                        message,
                        line
                    )
                }
                None => format!("{path}: {message}"),
            }
        }
        _ => error.to_string(),
    }
}

/// 加载策略，但把「解析错误」降级为告警而非硬失败。
/// 返回 `(policy, warning)`：解析失败时回退到空策略 + 把错误放进 `warning`，
/// 让 Codex 仍能启动（只是该批规则未生效）；其它错误（如 IO）照常上抛。
/// 设计意图：一个写坏的 `.rules` 文件不应让整个工具无法启动。
async fn load_exec_policy_with_warning(
    config_stack: &ConfigLayerStack,
) -> Result<(Policy, Option<ExecPolicyError>), ExecPolicyError> {
    match load_exec_policy(config_stack).await {
        Ok(policy) => Ok((policy, None)),
        Err(err @ ExecPolicyError::ParsePolicy { .. }) => Ok((Policy::empty(), Some(err))),
        Err(err) => Err(err),
    }
}

/// 按配置层加载并合并出最终 `Policy`。
///
/// 设计要点：按「优先级从低到高」遍历各配置层，依次把每层 `rules/*.rules`
/// 喂给同一个 `PolicyParser` 累积——因为 `Evaluation` 取最严裁决，后加入的
/// 高优先级层只能让策略更严或追加规则，从而实现「高层覆盖低层」的语义。
/// 最后再把 requirements 强制策略 `merge_overlay` 叠加上去（最强约束）。
/// 解析/IO 失败返回 `ExecPolicyError`（具体哪种见错误枚举）。
pub async fn load_exec_policy(config_stack: &ConfigLayerStack) -> Result<Policy, ExecPolicyError> {
    // Disabled project layers already represent the trust decision, so hooks
    // and exec-policy loading can reuse the normal trusted-layer view.
    // Iterate the layers in increasing order of precedence, adding the *.rules
    // from each layer, so that higher-precedence layers can override
    // rules defined in lower-precedence ones.
    // 被禁用的项目层已代表「不信任」决定，故加载时复用常规的「可信层」视图。
    // 按优先级升序遍历各层，逐层加入其 *.rules，让高优先级层覆盖低优先级层。
    let mut policy_paths = Vec::new();
    for layer in config_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        if config_stack.ignore_user_and_project_exec_policy_rules()
            && matches!(
                layer.name,
                ConfigLayerSource::User { .. } | ConfigLayerSource::Project { .. }
            )
        {
            continue;
        }
        if let Some(config_folder) = layer.config_folder() {
            let policy_dir = config_folder.join(RULES_DIR_NAME);
            let layer_policy_paths = collect_policy_files(&policy_dir).await?;
            policy_paths.extend(layer_policy_paths);
        }
    }
    tracing::trace!(
        policy_paths = ?policy_paths,
        "loaded exec policies"
    );

    let mut parser = PolicyParser::new();
    for policy_path in &policy_paths {
        let contents =
            fs::read_to_string(policy_path)
                .await
                .map_err(|source| ExecPolicyError::ReadFile {
                    path: policy_path.clone(),
                    source,
                })?;
        let identifier = policy_path.to_string_lossy().to_string();
        parser
            .parse(&identifier, &contents)
            .map_err(|source| ExecPolicyError::ParsePolicy {
                path: identifier,
                source,
            })?;
    }

    let policy = parser.build();
    tracing::debug!("loaded rules from {} files", policy_paths.len());
    tracing::trace!(rules = ?policy, "exec policy rules loaded");

    let Some(requirements_policy) = config_stack.requirements().exec_policy.as_deref() else {
        return Ok(policy);
    };

    Ok(policy.merge_overlay(requirements_policy.as_ref()))
}

/// If a command is not matched by any execpolicy rule, derive a [`Decision`].
/// 没有任何显式规则命中时的**启发式兜底裁决**——也是上面 `exec_policy_fallback`
/// 闭包的实现。
///
/// @returns - 该命令在「安全/危险启发式 × 审批策略 × 沙箱形态」下的 `Decision`
///
/// 核心判定顺序（任一步命中即返回）：
///   1. 命令在已知安全名单内、未经复杂解析，且（UnlessTrusted 或环境本就无
///      沙箱保护）→ `Allow`。
///   2. 命令被判危险 或 环境无沙箱保护 → 倾向 `Prompt`（弹窗确认）；但
///      `Never` 下无处弹，则按「沙箱是否被显式禁用」决定 `Allow` 还是
///      `Forbidden`。
///   3. 其余普通命令：依审批策略 + 沙箱形态决定 `Allow`（信任沙箱兜底）或
///      `Prompt`。
/// 设计哲学：能靠沙箱约束的就放行不打扰；危险或无沙箱时宁可弹窗也不静默放行。
pub(crate) fn render_decision_for_unmatched_command(
    command: &[String],
    context: UnmatchedCommandContext<'_>,
) -> Decision {
    let UnmatchedCommandContext {
        approval_policy,
        permission_profile,
        file_system_sandbox_policy,
        sandbox_cwd,
        sandbox_permissions,
        used_complex_parsing,
        command_origin,
    } = context;
    let is_known_safe = match command_origin {
        ExecPolicyCommandOrigin::Generic => is_known_safe_command(command),
        #[cfg(windows)]
        ExecPolicyCommandOrigin::PowerShell => {
            codex_shell_command::is_safe_command::is_safe_powershell_words(command)
        }
    };

    // On Windows, ReadOnly sandbox is not a real sandbox, so special-case it
    // here.
    // Windows 上「只读沙箱」并非真正的沙箱（缺乏内核级隔离），故特判：此时视为
    // 环境没有沙箱保护，后续会更倾向于弹窗/禁止而非放心放行。
    let environment_lacks_sandbox_protections = cfg!(windows)
        && profile_is_managed_read_only(
            permission_profile,
            file_system_sandbox_policy,
            sandbox_cwd,
        );

    // 已知安全命令的快速放行：要求未经复杂解析（结构可信），且仅在
    // UnlessTrusted（用户要求「不信任才问」）或无沙箱保护时才在此放行——
    // 其它策略下安全命令继续走下方常规逻辑（多半也会 Allow，但路径不同）。
    if is_known_safe
        && !used_complex_parsing
        && (approval_policy == AskForApproval::UnlessTrusted
            || environment_lacks_sandbox_protections)
    {
        return Decision::Allow;
    }

    // If the command is flagged as dangerous or we have no sandbox protection,
    // we should never allow it to run without approval.
    //
    // We prefer to prompt the user rather than outright forbid the command,
    // but if the user has explicitly disabled prompts, we must
    // forbid the command.
    let command_is_dangerous = match command_origin {
        ExecPolicyCommandOrigin::Generic => command_might_be_dangerous(command),
        #[cfg(windows)]
        ExecPolicyCommandOrigin::PowerShell => {
            codex_shell_command::is_dangerous_command::is_dangerous_powershell_words(command)
        }
    };
    if command_is_dangerous || environment_lacks_sandbox_protections {
        return match approval_policy {
            AskForApproval::Never => {
                let sandbox_is_explicitly_disabled = matches!(
                    permission_profile,
                    PermissionProfile::Disabled | PermissionProfile::External { .. }
                );
                if sandbox_is_explicitly_disabled {
                    // If the sandbox is explicitly disabled, we should allow the command to run
                    Decision::Allow
                } else {
                    Decision::Forbidden
                }
            }
            AskForApproval::OnFailure
            | AskForApproval::OnRequest
            | AskForApproval::UnlessTrusted
            | AskForApproval::Granular(_) => Decision::Prompt,
        };
    }

    match approval_policy {
        AskForApproval::Never | AskForApproval::OnFailure => {
            // We allow the command to run, relying on the sandbox for
            // protection.
            Decision::Allow
        }
        AskForApproval::UnlessTrusted => {
            // We already checked the unmatched-command safelist and it
            // returned false, so we must prompt.
            Decision::Prompt
        }
        AskForApproval::OnRequest => {
            match file_system_sandbox_policy.kind {
                FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                    // The user has indicated we should "just run" commands
                    // in their unrestricted environment, so we do so since the
                    // command has not been flagged as dangerous.
                    Decision::Allow
                }
                FileSystemSandboxKind::Restricted => {
                    // In restricted sandboxes, do not prompt for non-escalated,
                    // non-dangerous commands; let the sandbox enforce
                    // restrictions without a user prompt.
                    if sandbox_permissions.requests_sandbox_override() {
                        Decision::Prompt
                    } else {
                        Decision::Allow
                    }
                }
            }
        }
        AskForApproval::Granular(_) => match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                // Mirror on-request behavior for unmatched commands; prompt-vs-reject is handled
                // by `prompt_is_rejected_by_policy`.
                Decision::Allow
            }
            FileSystemSandboxKind::Restricted => {
                if sandbox_permissions.requests_sandbox_override() {
                    Decision::Prompt
                } else {
                    Decision::Allow
                }
            }
        },
    }
}

/// 判断是否处于「受管的纯只读」环境：受管画像 + 受限沙箱 + 无全盘写 + 无任何
/// 可写根目录。配合上面的 Windows 特判使用——这种环境在 Windows 上等于没有
/// 有效的写入隔离，需当作「无沙箱保护」处理。
fn profile_is_managed_read_only(
    permission_profile: &PermissionProfile,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    sandbox_cwd: &Path,
) -> bool {
    matches!(permission_profile, PermissionProfile::Managed { .. })
        && matches!(
            file_system_sandbox_policy.kind,
            FileSystemSandboxKind::Restricted
        )
        && !file_system_sandbox_policy.has_full_disk_write_access()
        && file_system_sandbox_policy
            .get_writable_roots_with_cwd(sandbox_cwd)
            .is_empty()
}

fn default_policy_path(codex_home: &Path) -> PathBuf {
    codex_home.join(RULES_DIR_NAME).join(DEFAULT_POLICY_FILE)
}

/// 把原始命令 argv 拆成供策略评估的命令段，并标注解析质量与来源。
///
/// 按可靠度从高到低尝试：
///   1. `bash -lc "..."` 内的「朴素命令」拆解（最可靠，`used_complex_parsing=false`）；
///   2. (Windows) PowerShell 命令拆解；
///   3. `bash -lc` 单命令前缀的复杂回退（`used_complex_parsing=true`，不允许
///      据此自动加规则）；
///   4. 都不行就原样当作单条命令。
/// `used_complex_parsing` 这个标记一路传到上层，门控「是否允许自动 amendment」。
fn commands_for_exec_policy(command: &[String]) -> ExecPolicyCommands {
    if let Some(commands) = parse_shell_lc_plain_commands(command)
        && !commands.is_empty()
    {
        return ExecPolicyCommands {
            commands,
            used_complex_parsing: false,
            command_origin: ExecPolicyCommandOrigin::Generic,
        };
    }

    #[cfg(windows)]
    {
        if let Some(commands) =
            codex_shell_command::powershell::parse_powershell_command_into_plain_commands(command)
            && !commands.is_empty()
        {
            return ExecPolicyCommands {
                commands,
                used_complex_parsing: false,
                command_origin: ExecPolicyCommandOrigin::PowerShell,
            };
        }
    }

    if let Some(single_command) = parse_shell_lc_single_command_prefix(command) {
        return ExecPolicyCommands {
            commands: vec![single_command],
            used_complex_parsing: true,
            command_origin: ExecPolicyCommandOrigin::Generic,
        };
    }

    ExecPolicyCommands {
        commands: vec![command.to_vec()],
        used_complex_parsing: false,
        command_origin: ExecPolicyCommandOrigin::Generic,
    }
}

/// Derive a proposed execpolicy amendment when a command requires user approval
/// - If any execpolicy rule prompts, return None, because an amendment would not skip that policy requirement.
/// - Otherwise return the first heuristics Prompt.
/// - Examples:
/// - execpolicy: empty. Command: `["python"]`. Heuristics prompt -> `Some(vec!["python"])`.
/// - execpolicy: empty. Command: `["bash", "-c", "cd /some/folder && prog1 --option1 arg1 && prog2 --option2 arg2"]`.
///   Parsed commands include `cd /some/folder`, `prog1 --option1 arg1`, and `prog2 --option2 arg2`. If heuristics allow `cd` but prompt
///   on `prog1`, we return `Some(vec!["prog1", "--option1", "arg1"])`.
/// - execpolicy: contains a `prompt for prefix ["prog2"]` rule. For the same command as above,
///   we return `None` because an execpolicy prompt still applies even if we amend execpolicy to allow ["prog1", "--option1", "arg1"].
/// 为「需审批」的命令推导可自动追加的 allow 规则建议。
/// 规则：只要有**显式策略规则**也判 Prompt，就返回 `None`——因为即便放行了
/// 启发式命中的那段，显式 Prompt 规则依然会拦下，加规则没意义。否则取第一个
/// 「启发式 Prompt」命中段作为建议放行前缀。
fn try_derive_execpolicy_amendment_for_prompt_rules(
    matched_rules: &[RuleMatch],
) -> Option<ExecPolicyAmendment> {
    if matched_rules
        .iter()
        .any(|rule_match| is_policy_match(rule_match) && rule_match.decision() == Decision::Prompt)
    {
        return None;
    }

    matched_rules
        .iter()
        .find_map(|rule_match| match rule_match {
            RuleMatch::HeuristicsRuleMatch {
                command,
                decision: Decision::Prompt,
            } => Some(ExecPolicyAmendment::from(command.clone())),
            _ => None,
        })
}

/// - Note: we only use this amendment when the command fails to run in sandbox and codex prompts the user to run outside the sandbox
/// - The purpose of this amendment is to bypass sandbox for similar commands in the future
/// - If any execpolicy rule matches, return None, because we would already be running command outside the sandbox
/// 为「沙箱内失败、提示用户改到沙箱外运行」的场景推导 allow 规则建议——目的是
/// 让今后同类命令直接绕过沙箱。只要命中了任何**显式规则**就返回 `None`（那种
/// 情况下命令本就走显式规则路径，无需再加）；否则取首个「启发式 Allow」命中段。
fn try_derive_execpolicy_amendment_for_allow_rules(
    matched_rules: &[RuleMatch],
) -> Option<ExecPolicyAmendment> {
    if matched_rules.iter().any(is_policy_match) {
        return None;
    }

    matched_rules
        .iter()
        .find_map(|rule_match| match rule_match {
            RuleMatch::HeuristicsRuleMatch {
                command,
                decision: Decision::Allow,
            } => Some(ExecPolicyAmendment::from(command.clone())),
            _ => None,
        })
}

/// 把调用方/UI 建议的 `prefix_rule` 候选推导成正式的 amendment——前提是它
/// 安全且确实够用。返回 `None`（不建议）的几种情况：
///   - 候选为空 / 在 `BANNED_PREFIX_SUGGESTIONS` 黑名单里（解释器/提权等）；
///   - 已有显式规则命中（再加可能冲突或多余）；
///   - 加了这条前缀也无法让**所有**命令段都变成 Allow（即不够用）。
fn derive_requested_execpolicy_amendment_from_prefix_rule(
    prefix_rule: Option<&Vec<String>>,
    matched_rules: &[RuleMatch],
    exec_policy: &Policy,
    commands: &[Vec<String>],
    exec_policy_fallback: &impl Fn(&[String]) -> Decision,
    match_options: &MatchOptions,
) -> Option<ExecPolicyAmendment> {
    let prefix_rule = prefix_rule?;
    if prefix_rule.is_empty() {
        return None;
    }
    // 黑名单逐条全等比对（长度相等且每个 token 相同）。
    if BANNED_PREFIX_SUGGESTIONS.iter().any(|banned| {
        prefix_rule.len() == banned.len()
            && prefix_rule
                .iter()
                .map(String::as_str)
                .eq(banned.iter().copied())
    }) {
        return None;
    }

    // if any policy rule already matches, don't suggest an additional rule that might conflict or not apply
    if matched_rules.iter().any(is_policy_match) {
        return None;
    }

    let amendment = ExecPolicyAmendment::new(prefix_rule.clone());
    if prefix_rule_would_approve_all_commands(
        exec_policy,
        &amendment.command,
        commands,
        exec_policy_fallback,
        match_options,
    ) {
        Some(amendment)
    } else {
        None
    }
}

/// 试算：把 `prefix_rule` 作为 allow 规则加进策略副本后，是否**所有**命令段
/// 都会被判 Allow。用于验证一条建议规则「确实够用」再决定是否提给用户。
/// 在策略克隆副本上试加，不污染当前生效策略；加规则失败（如空前缀）直接返回
/// false。
fn prefix_rule_would_approve_all_commands(
    exec_policy: &Policy,
    prefix_rule: &[String],
    commands: &[Vec<String>],
    exec_policy_fallback: &impl Fn(&[String]) -> Decision,
    match_options: &MatchOptions,
) -> bool {
    let mut policy_with_prefix_rule = exec_policy.clone();
    if policy_with_prefix_rule
        .add_prefix_rule(prefix_rule, Decision::Allow)
        .is_err()
    {
        return false;
    }

    commands.iter().all(|command| {
        policy_with_prefix_rule
            .check_with_options(command, exec_policy_fallback, match_options)
            .decision
            == Decision::Allow
    })
}

/// Only return a reason when a policy rule drove the prompt decision.
/// 仅当「提示」由显式前缀规则驱动时才返回理由文案（启发式 Prompt 返回 `None`，
/// 不向用户展示策略理由）。在所有 Prompt 规则里取**前缀最长**的那条——前缀越长
/// 越具体，理由越贴切。有 `justification` 就带上，否则给通用措辞。
fn derive_prompt_reason(command_args: &[String], evaluation: &Evaluation) -> Option<String> {
    let command = render_shlex_command(command_args);

    let most_specific_prompt = evaluation
        .matched_rules
        .iter()
        .filter_map(|rule_match| match rule_match {
            RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: Decision::Prompt,
                justification,
                ..
            } => Some((matched_prefix.len(), justification.as_deref())),
            _ => None,
        })
        .max_by_key(|(matched_prefix_len, _)| *matched_prefix_len);

    match most_specific_prompt {
        Some((_matched_prefix_len, Some(justification))) => {
            Some(format!("`{command}` requires approval: {justification}"))
        }
        Some((_matched_prefix_len, None)) => {
            Some(format!("`{command}` requires approval by policy"))
        }
        None => None,
    }
}

fn render_shlex_command(args: &[String]) -> String {
    shlex_try_join(args.iter().map(String::as_str)).unwrap_or_else(|_| args.join(" "))
}

/// Derive a string explaining why the command was forbidden. If `justification`
/// is set by the user, this can contain instructions with recommended
/// alternatives, for example.
/// 生成「为何禁止」的说明文案。同样取**前缀最长**（最具体）的 Forbidden 规则：
/// 有 `justification` 直接回显（作者可借此给出替代方案建议）；没有则说明「策略
/// 禁止以该前缀开头的命令」；连规则都没有（启发式禁止）则给兜底措辞。
fn derive_forbidden_reason(command_args: &[String], evaluation: &Evaluation) -> String {
    let command = render_shlex_command(command_args);

    let most_specific_forbidden = evaluation
        .matched_rules
        .iter()
        .filter_map(|rule_match| match rule_match {
            RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: Decision::Forbidden,
                justification,
                ..
            } => Some((matched_prefix, justification.as_deref())),
            _ => None,
        })
        .max_by_key(|(matched_prefix, _)| matched_prefix.len());

    match most_specific_forbidden {
        Some((_matched_prefix, Some(justification))) => {
            format!("`{command}` rejected: {justification}")
        }
        Some((matched_prefix, None)) => {
            let prefix = render_shlex_command(matched_prefix);
            format!("`{command}` rejected: policy forbids commands starting with `{prefix}`")
        }
        None => format!("`{command}` rejected: blocked by policy"),
    }
}

/// 收集目录下所有 `*.rules` 文件路径，排序后返回（保证加载顺序稳定可复现）。
/// 目录不存在视为「无规则」返回空 vec 而非报错——某层没有 rules 目录是正常的。
/// 只收文件类型且扩展名为 `rules` 的条目。
async fn collect_policy_files(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, ExecPolicyError> {
    let dir = dir.as_ref();
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut policy_paths = Vec::new();
    while let Some(entry) =
        read_dir
            .next_entry()
            .await
            .map_err(|source| ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            })?
    {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|source| ExecPolicyError::ReadDir {
                dir: dir.to_path_buf(),
                source,
            })?;

        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == RULE_EXTENSION)
            && file_type.is_file()
        {
            policy_paths.push(path);
        }
    }

    policy_paths.sort();

    tracing::debug!(
        "loaded {} .rules files in {}",
        policy_paths.len(),
        dir.display()
    );
    Ok(policy_paths)
}

#[cfg(test)]
#[path = "exec_policy_tests.rs"]
mod tests;
