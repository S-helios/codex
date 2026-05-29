//! 【文件职责】把执行命令的 argv 归一化（canonicalize）成一个稳定形态，
//!   专供「审批缓存」（approval cache）匹配使用。
//!
//! 【架构位置】
//!   层级：工具执行层 / 审批前置归一
//!   上游：`tools/runtimes/shell.rs`、`tools/runtimes/unified_exec.rs`
//!         在记录/查询审批缓存条目时调用 `canonicalize_command_for_approval`
//!   下游：`codex_shell_command` crate（bash / powershell 命令解析器）
//!
//! 【为什么需要归一】
//!   同一条命令可能因「包装路径」不同而呈现多种 argv：例如
//!   `/bin/bash -lc "git status"` 与 `bash -lc "git status"` 语义相同，
//!   但逐字节比较会被当成两条不同命令，导致用户对前者批准过、后者又重复弹审批。
//!   本模块把这些等价形态折叠成同一 key，让审批决定在包装差异、shell 包装
//!   工具之间保持稳定。
//!
//! 【取舍】对能安全 token 化的「简单单命令」直接还原成命令序列；对无法安全
//!   还原的「复杂脚本」则保留脚本原文 + 加前缀标记，宁可粒度粗一点也不冒险
//!   把不同脚本误判为相同。

use codex_shell_command::bash::extract_bash_command;
use codex_shell_command::bash::parse_shell_lc_plain_commands;
use codex_shell_command::powershell::extract_powershell_command;

// 复杂 bash 脚本归一后的首元素标记：用于在缓存 key 中区分「这是一段
// 未经 token 化的 bash 脚本原文」，避免与普通命令 argv 撞车。
const CANONICAL_BASH_SCRIPT_PREFIX: &str = "__codex_shell_script__";
// 同上，针对 PowerShell 脚本的标记前缀。
const CANONICAL_POWERSHELL_SCRIPT_PREFIX: &str = "__codex_powershell_script__";

/// Canonicalize command argv for approval-cache matching.
///
/// This keeps approval decisions stable across wrapper-path differences (for
/// example `/bin/bash -lc` vs `bash -lc`) and across shell wrapper tools while
/// preserving exact script text for complex scripts where we cannot safely
/// recover a tokenized command sequence.
///
/// 将命令 argv 归一化为审批缓存的匹配 key。
///
/// @param command - 原始命令 argv（如 `["/bin/bash", "-lc", "git status"]`）
/// @returns       - 归一后的 argv：简单命令还原为命令序列；复杂脚本则为
///                  `[前缀标记, (shell_mode), 脚本原文]`；无法识别时原样返回。
///
/// 设计要点：归一只为「比较是否同一条命令」服务，**不用于实际执行**，因此
/// 丢弃 shell 路径（`_shell`）等不影响语义的包装信息是安全的。匹配优先级
/// 从「最能精确还原」到「只能保留原文」逐级回退，见下方三步。
pub(crate) fn canonicalize_command_for_approval(command: &[String]) -> Vec<String> {
    // Step 1：简单 `shell -lc "<单条命令>"` —— 直接还原成 token 化命令序列。
    // 这是最理想的形态：能把 `bash -lc "git status"` 还原为 `["git","status"]`，
    // 从而无视 shell 路径、登录态等包装差异，匹配粒度最细。
    // 仅当解析出「恰好一条」命令（`[single_command]`）时才走这条路；多条命令
    // （如 `a && b`）无法安全拆分，留给后续步骤按整段脚本处理。
    if let Some(commands) = parse_shell_lc_plain_commands(command)
        && let [single_command] = commands.as_slice()
    {
        return single_command.clone();
    }

    // Step 2：复杂 bash 脚本 —— 无法安全 token 化，保留脚本原文。
    // 此时只能退化为「前缀标记 + shell 模式 + 脚本原文」三元组。
    // 保留 `shell_mode`（argv[1]，如 `-lc` / `-c`）是因为登录态会改变行为，
    // 不同模式应被视为不同命令；缺失时回退为空串。
    if let Some((_shell, script)) = extract_bash_command(command) {
        let shell_mode = command.get(1).cloned().unwrap_or_default();
        return vec![
            CANONICAL_BASH_SCRIPT_PREFIX.to_string(),
            shell_mode,
            script.to_string(),
        ];
    }

    // Step 3：复杂 PowerShell 脚本 —— 同样退化为「前缀标记 + 脚本原文」。
    if let Some((_shell, script)) = extract_powershell_command(command) {
        return vec![
            CANONICAL_POWERSHELL_SCRIPT_PREFIX.to_string(),
            script.to_string(),
        ];
    }

    // 兜底：既非可解析的 shell 包装、也非已知脚本形态，原样返回 argv。
    command.to_vec()
}

#[cfg(test)]
#[path = "command_canonicalization_tests.rs"]
mod tests;
