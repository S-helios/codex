//! 【文件职责】`codex-chatgpt` crate 的入口，聚合「ChatGPT 账户后端」相关能力：
//! 拉取云端 Codex 任务的 diff 并应用（`apply_command`/`get_task`）、向 ChatGPT
//! 后端发认证 HTTP 请求（`chatgpt_client`，crate 私有）、列举可用连接器/Apps
//! （`connectors`）、查询工作区 beta 设置（`workspace_settings`）。
pub mod apply_command;
mod chatgpt_client;
pub mod connectors;
pub mod get_task;
pub mod workspace_settings;
