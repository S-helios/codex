//! 【文件职责】定义 ChatGPT 后端「获取任务」(`/wham/tasks/{id}`) 响应的反序列化类型，
//! 并提供 `get_task` 拉取函数。被 `apply_command` 用来取出任务的 PR diff。
//!
//! 这些结构体只保留抽取 diff 所需的最小字段（其余字段忽略，见 `OutputItem::Other`）。
use codex_core::config::Config;
use serde::Deserialize;

use crate::chatgpt_client::chatgpt_get_request;

/// 任务响应：当前只关心「含 diff 的那一轮」(`current_diff_task_turn`)，可能为空。
#[derive(Debug, Deserialize)]
pub struct GetTaskResponse {
    pub current_diff_task_turn: Option<AssistantTurn>,
}

// Only relevant fields for our extraction
#[derive(Debug, Deserialize)]
pub struct AssistantTurn {
    pub output_items: Vec<OutputItem>,
}

/// 任务一轮的输出项，按 JSON `type` 字段区分。
/// 这里只解析 `pr`（携带 diff）；其余所有类型用 `#[serde(other)]` 兜底为 `Other`，
/// 既能向前兼容后端新增类型，又能在过滤时简单跳过。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "pr")]
    Pr(PrOutputItem),

    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct PrOutputItem {
    pub output_diff: OutputDiff,
}

#[derive(Debug, Deserialize)]
pub struct OutputDiff {
    pub diff: String,
}

/// 按 task id 调 `/wham/tasks/{id}` 拉取任务详情（认证 GET，错误归一由客户端处理）。
pub(crate) async fn get_task(config: &Config, task_id: String) -> anyhow::Result<GetTaskResponse> {
    let path = format!("/wham/tasks/{task_id}");
    chatgpt_get_request(config, path).await
}
