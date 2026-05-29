//! 【文件职责】实现 `codex apply <task_id>` 子命令：把某个云端 Codex 任务产出的最新
//! diff（PR 形态）拉取下来并 `git apply` 到本地工作区。
//!
//! 【架构位置】
//!   层级：CLI 命令层（`codex-chatgpt` crate）。
//!   下游：`get_task`（拉任务）→ `codex_git_utils::apply_git_patch`（实际打补丁）。
//!
//! 【数据流】task_id → `get_task` → 取 `current_diff_task_turn` 里的 PR diff →
//!           `apply_git_patch` → 打印结果 / 报冲突。
use std::path::PathBuf;

use clap::Parser;
use codex_core::config::Config;
use codex_git_utils::ApplyGitRequest;
use codex_git_utils::apply_git_patch;
use codex_utils_cli::CliConfigOverrides;

use crate::get_task::GetTaskResponse;
use crate::get_task::OutputItem;
use crate::get_task::PrOutputItem;
use crate::get_task::get_task;

/// Applies the latest diff from a Codex agent task.
/// `codex apply` 的命令行参数：目标任务 id，外加通用的 `-c key=value` 配置覆写。
#[derive(Debug, Parser)]
pub struct ApplyCommand {
    pub task_id: String,

    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,
}
/// 命令入口：加载配置（含 CLI 覆写）→ 拉取任务 → 应用其 diff。
pub async fn run_apply_command(
    apply_cli: ApplyCommand,
    cwd: Option<PathBuf>,
) -> anyhow::Result<()> {
    let config = Config::load_with_cli_overrides(
        apply_cli
            .config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?,
    )
    .await?;

    let task_response = get_task(&config, apply_cli.task_id).await?;
    apply_diff_from_task(task_response, cwd).await
}

/// 从任务响应中抽取「PR 输出项」里的 diff 并应用。
/// 缺少 diff turn 或没有 PR 输出项时直接 `bail!` 报错（说明该任务没有可应用的补丁）。
pub async fn apply_diff_from_task(
    task_response: GetTaskResponse,
    cwd: Option<PathBuf>,
) -> anyhow::Result<()> {
    let diff_turn = match task_response.current_diff_task_turn {
        Some(turn) => turn,
        None => anyhow::bail!("No diff turn found"),
    };
    let output_diff = diff_turn.output_items.iter().find_map(|item| match item {
        OutputItem::Pr(PrOutputItem { output_diff }) => Some(output_diff),
        _ => None,
    });
    match output_diff {
        Some(output_diff) => apply_diff(&output_diff.diff, cwd).await,
        None => anyhow::bail!("No PR output item found"),
    }
}

// 在 `cwd`（默认当前目录，取不到则退到临时目录）执行 `git apply`，
// 退出码非 0 时把 applied/skipped/conflicts 数量及 stdout/stderr 一并报出便于排查冲突。
async fn apply_diff(diff: &str, cwd: Option<PathBuf>) -> anyhow::Result<()> {
    let cwd = cwd.unwrap_or(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
    let req = ApplyGitRequest {
        cwd,
        diff: diff.to_string(),
        revert: false,
        preflight: false,
    };
    let res = apply_git_patch(&req)?;
    if res.exit_code != 0 {
        anyhow::bail!(
            "Git apply failed (applied={}, skipped={}, conflicts={})\nstdout:\n{}\nstderr:\n{}",
            res.applied_paths.len(),
            res.skipped_paths.len(),
            res.conflicted_paths.len(),
            res.stdout,
            res.stderr
        );
    }
    println!("Successfully applied diff");
    Ok(())
}
