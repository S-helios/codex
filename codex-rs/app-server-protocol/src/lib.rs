//! 【文件职责】`codex-app-server-protocol` crate 的根入口：声明各子模块并把
//! 协议类型统一 re-export 到 crate 顶层，对外提供一个扁平的命名空间。
//!
//! 【架构位置】
//!   层级：app-server 的「线缆协议（wire protocol）定义层」
//!   上游：app-server 服务端与各类客户端（VS Code 扩展、Codex Cloud、CLI 等）
//!         都依赖本 crate 收发 JSON-RPC 消息。
//!   下游：`codex-protocol`（核心领域类型，如 ThreadId / TokenUsage / ResponseItem）。
//!
//! 【模块构成】
//!   - `jsonrpc_lite`：JSON-RPC 信封（`JSONRPCRequest` / `Response` / `Notification` /
//!     `Error` 及 `RequestId`）。注意：这里是「精简版」JSON-RPC，刻意不带
//!     `"jsonrpc": "2.0"` 字段，详见该文件首部说明。
//!   - `protocol::common`：请求/响应/通知的「总目录」——用宏集中定义
//!     `ClientRequest` / `ServerRequest` / `ServerNotification` 等枚举。
//!   - `protocol::v1` / `protocol::v2`：两代具体的 params/response 类型；v1 多为
//!     已弃用 API，v2 是当前主力（线程、轮次、工具、配置等）。
//!   - `export` / `schema_fixtures`：把上述类型导出为 TypeScript 定义与 JSON Schema，
//!     供前端/外部客户端生成类型，是「单一事实来源」的落地手段。
//!
//! 【阅读建议】本文件只是 re-export 清单，无逻辑；要理解协议先看
//!   `protocol/common.rs` 的几个 `*_definitions!` 宏调用（请求/通知目录），
//!   再按需深入 `protocol/v2/` 下的具体类型文件。
mod experimental_api;
mod export;
mod jsonrpc_lite;
mod protocol;
mod schema_fixtures;

pub use experimental_api::*;
pub use export::GenerateTsOptions;
pub use export::generate_internal_json_schema;
pub use export::generate_json;
pub use export::generate_json_with_experimental;
pub use export::generate_ts;
pub use export::generate_ts_with_options;
pub use export::generate_types;
pub use jsonrpc_lite::*;
pub use protocol::common::*;
pub use protocol::event_mapping::*;
pub use protocol::item_builders::*;
pub use protocol::thread_history::*;
pub use protocol::v1::ApplyPatchApprovalParams;
pub use protocol::v1::ApplyPatchApprovalResponse;
pub use protocol::v1::ClientInfo;
pub use protocol::v1::ConversationGitInfo;
pub use protocol::v1::ConversationSummary;
pub use protocol::v1::ExecCommandApprovalParams;
pub use protocol::v1::ExecCommandApprovalResponse;
pub use protocol::v1::GetAuthStatusParams;
pub use protocol::v1::GetAuthStatusResponse;
pub use protocol::v1::GetConversationSummaryParams;
pub use protocol::v1::GetConversationSummaryResponse;
pub use protocol::v1::GitDiffToRemoteParams;
pub use protocol::v1::GitDiffToRemoteResponse;
pub use protocol::v1::GitSha;
pub use protocol::v1::InitializeCapabilities;
pub use protocol::v1::InitializeParams;
pub use protocol::v1::InitializeResponse;
pub use protocol::v1::InterruptConversationResponse;
pub use protocol::v1::LoginApiKeyParams;
pub use protocol::v1::SandboxSettings;
pub use protocol::v1::Tools;
pub use protocol::v1::UserSavedConfig;
pub use protocol::v2::*;
pub use schema_fixtures::SchemaFixtureOptions;
#[doc(hidden)]
pub use schema_fixtures::generate_typescript_schema_fixture_subtree_for_tests;
pub use schema_fixtures::read_schema_fixture_subtree;
pub use schema_fixtures::read_schema_fixture_tree;
pub use schema_fixtures::write_schema_fixtures;
pub use schema_fixtures::write_schema_fixtures_with_options;
