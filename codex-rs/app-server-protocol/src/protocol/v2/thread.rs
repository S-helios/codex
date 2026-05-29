//! 【文件职责】v2 协议中「线程（Thread）」相关的请求参数、响应与通知类型。
//! 一个 Thread 是一段可持久化、可恢复、可派生的会话，本文件定义围绕其
//! 生命周期与元数据的全部 wire 类型。
//!
//! 【架构位置】
//!   层级：协议定义层 · v2 子模块（被 `protocol::v2` 以 `pub use thread::*;` 导出）
//!   上游：`protocol::common` 的 `client_request_definitions!` / `server_notification
//!         _definitions!` 调用，把这里的类型登记为具体方法的 params/response/payload。
//!   下游：`codex_protocol`（核心领域类型，如 `ThreadGoal` / `TokenUsage` /
//!         `ThreadMemoryMode`），本文件多处提供 `From<core 类型>` 的转换桥接。
//!
//! 【类型分组（按出现顺序）】
//!   - 启动/恢复/派生：`ThreadStartParams` / `ThreadResumeParams` / `ThreadForkParams`
//!     及各自 Response——三者共享大量「配置覆盖」字段（model、sandbox、审批策略等）。
//!     Resume/Fork 顶部的 `///` 详述了 thread_id / history / path 三种来源的优先级。
//!   - 设置与归档：`ThreadSettingsUpdate*` / `ThreadArchive*` / `ThreadUnarchive*` /
//!     `ThreadUnsubscribe*`。
//!   - 目标模式（Goal）：`ThreadGoal*`，对应 goal-mode 的目标/预算/状态。
//!   - 元数据/记忆/压缩/回滚：`ThreadMetadataUpdate*` / `ThreadMemoryMode*` /
//!     `ThreadCompactStart*` / `ThreadRollback*`。
//!   - 列举与读取：`ThreadList*` / `ThreadSearch*` / `ThreadLoadedList*` /
//!     `ThreadRead*` / `ThreadTurns(Items)List*`（含分页游标语义）。
//!   - 通知：`Thread*Notification`（started / statusChanged / archived / tokenUsage 等）。
//!
//! 【贯穿设计 · double-option 字段】不少可选字段用 `Option<Option<T>>` 配合
//!   `serde_helpers::*_double_option`，以在 JSON 上区分「字段缺省＝不改」与
//!   「显式 null＝清空」两种语义——阅读 `service_tier`、`git_info` 等字段时注意这点。
//!
//! 【实验性标记】`#[experimental("...")]` 标注的字段/类型属实验性 API，可能随
//!   版本变动；schema 导出时会据此做门控过滤。
//!
//! 【阅读建议】先看 `ThreadStartParams` / `ThreadStartResponse` 建立「一个线程由
//!   哪些配置构成」的整体印象，其余 Resume/Fork 基本是其变体；列举类只需关注
//!   游标（cursor / backwards_cursor）的成对语义。本文件为纯类型定义，无运行逻辑。
use super::ActivePermissionProfile;
use super::ApprovalsReviewer;
use super::AskForApproval;
use super::SandboxMode;
use super::SandboxPolicy;
use super::Thread;
use super::ThreadItem;
use super::ThreadSource;
use super::Turn;
use super::TurnEnvironmentParams;
use super::TurnItemsView;
use super::shared::v2_enum_from_core;
use codex_experimental_api_macros::ExperimentalApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadGoalStatus as CoreThreadGoalStatus;
use codex_protocol::protocol::TokenUsage as CoreTokenUsage;
use codex_protocol::protocol::TokenUsageInfo as CoreTokenUsageInfo;
use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;
use ts_rs::TS;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadStartSource {
    Startup,
    Clear,
}

/// 由客户端在 `thread/start` 时注入的「动态工具」声明：让宿主侧自定义一个可被模型
/// 调用的工具（名称、描述、入参 JSON Schema）。`defer_loading` 为 true 时表示该工具
/// 默认不暴露给模型上下文、需要时再加载。注意此结构「只 `Serialize` 派生」，反序列化
/// 走下方手写 `Deserialize` 以兼容旧字段。
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DynamicToolSpec {
    #[ts(optional)]
    pub namespace: Option<String>,
    pub name: String,
    pub description: String,
    pub input_schema: JsonValue,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer_loading: bool,
}

/// 反序列化中转结构：比 `DynamicToolSpec` 多了一个已弃用的 `expose_to_context` 字段，
/// 用于在手写 `Deserialize` 中做新旧字段的兼容映射（见下方 impl）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DynamicToolSpecDe {
    namespace: Option<String>,
    name: String,
    description: String,
    input_schema: JsonValue,
    defer_loading: Option<bool>,
    expose_to_context: Option<bool>,
}

impl<'de> Deserialize<'de> for DynamicToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let DynamicToolSpecDe {
            namespace,
            name,
            description,
            input_schema,
            defer_loading,
            expose_to_context,
        } = DynamicToolSpecDe::deserialize(deserializer)?;

        Ok(Self {
            namespace,
            name,
            description,
            input_schema,
            // `defer_loading` 的取值优先级（兼容新旧客户端）：
            //   1. 显式给了 `deferLoading` → 直接用。
            //   2. 否则回退到旧字段 `exposeToContext`：语义相反，「暴露」即「不延迟加载」，
            //      故取其反值（visible=true → defer=false）。
            //   3. 两者都没给 → 默认 false（不延迟，即立即暴露给上下文）。
            defer_loading: defer_loading
                .unwrap_or_else(|| expose_to_context.map(|visible| !visible).unwrap_or(false)),
        })
    }
}

// === Threads, Turns, and Items ===
// Thread APIs
/// `thread/start` 的入参：创建一个新线程并指定其初始配置。字段几乎全部可选，
/// 缺省者落回 Codex 全局/默认配置。核心维度包括：模型与 provider、cwd 与运行时
/// 工作区根、审批策略（`approval_policy` / `approvals_reviewer`）、沙箱或命名权限
/// profile、基础/开发者指令、人格、是否临时（`ephemeral`，不落盘）等。
/// 标 `#[experimental(..)]` 的字段为实验性、可能变动。本类型是理解「一个线程由
/// 哪些配置构成」的入口，Resume / Fork 的参数基本是它的变体。
#[derive(
    Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadStartParams {
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub model_provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub service_tier: Option<Option<String>>,
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    /// Replace the thread's runtime workspace roots. Relative paths are
    /// resolved against the effective cwd for the thread.
    #[experimental("thread/start.runtimeWorkspaceRoots")]
    #[ts(optional = nullable)]
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    #[experimental(nested)]
    #[ts(optional = nullable)]
    pub approval_policy: Option<AskForApproval>,
    /// Override where approval requests are routed for review on this thread
    /// and subsequent turns.
    #[ts(optional = nullable)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    #[ts(optional = nullable)]
    pub sandbox: Option<SandboxMode>,
    /// Named profile id for this thread. Cannot be combined with `sandbox`.
    #[experimental("thread/start.permissions")]
    #[ts(optional = nullable)]
    pub permissions: Option<String>,
    #[ts(optional = nullable)]
    pub config: Option<HashMap<String, JsonValue>>,
    #[ts(optional = nullable)]
    pub service_name: Option<String>,
    #[ts(optional = nullable)]
    pub base_instructions: Option<String>,
    #[ts(optional = nullable)]
    pub developer_instructions: Option<String>,
    #[ts(optional = nullable)]
    pub personality: Option<Personality>,
    #[ts(optional = nullable)]
    pub ephemeral: Option<bool>,
    #[ts(optional = nullable)]
    pub session_start_source: Option<ThreadStartSource>,
    /// Optional client-supplied analytics source classification for this thread.
    #[ts(optional = nullable)]
    pub thread_source: Option<ThreadSource>,
    /// Optional sticky environments for this thread.
    ///
    /// Omitted selects the default environment when environment access is
    /// enabled. Empty disables environment access for turns that do not
    /// provide a turn override. Non-empty selects the first environment as the
    /// current turn environment.
    #[experimental("thread/start.environments")]
    #[ts(optional = nullable)]
    pub environments: Option<Vec<TurnEnvironmentParams>>,
    #[experimental("thread/start.dynamicTools")]
    #[ts(optional = nullable)]
    pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
    /// Test-only experimental field used to validate experimental gating and
    /// schema filtering behavior in a stable way.
    #[experimental("thread/start.mockExperimentalField")]
    #[ts(optional = nullable)]
    pub mock_experimental_field: Option<String>,
    /// If true, opt into emitting raw Responses API items on the event stream.
    /// This is for internal use only (e.g. Codex Cloud).
    #[experimental("thread/start.experimentalRawEvents")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub experimental_raw_events: bool,
    /// Deprecated and ignored by app-server. Kept only so older clients can
    /// continue sending the field while rollout persistence always uses the
    /// limited history policy.
    #[experimental("thread/start.persistFullHistory")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist_extended_history: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MockExperimentalMethodParams {
    /// Test-only payload field.
    #[ts(optional = nullable)]
    pub value: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MockExperimentalMethodResponse {
    /// Echoes the input `value`.
    pub echoed: Option<String>,
}

/// `thread/start` 的响应：回传新建线程的句柄（`thread`）及「实际生效」的配置——
/// 即把入参里的可选项与默认值合并后的最终结果（解析后的 cwd、运行时工作区根、
/// 加载到的指令源文件、生效的审批策略与沙箱策略等）。客户端据此知晓线程真实状态，
/// 而非自己传入的请求值。`ThreadResumeResponse` / `ThreadForkResponse` 结构几乎相同。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub service_tier: Option<String>,
    pub cwd: AbsolutePathBuf,
    /// Thread-scoped runtime workspace roots used to materialize
    /// `:workspace_roots`.
    #[experimental("thread/start.runtimeWorkspaceRoots")]
    #[serde(default)]
    pub runtime_workspace_roots: Vec<AbsolutePathBuf>,
    /// Instruction source files currently loaded for this thread.
    #[serde(default)]
    pub instruction_sources: Vec<AbsolutePathBuf>,
    #[experimental(nested)]
    pub approval_policy: AskForApproval,
    /// Reviewer currently used for approval requests on this thread.
    pub approvals_reviewer: ApprovalsReviewer,
    /// Legacy sandbox policy retained for compatibility. Experimental clients
    /// should prefer `activePermissionProfile` for profile provenance.
    pub sandbox: SandboxPolicy,
    /// Named or implicit built-in profile that produced the active
    /// permissions, when known.
    #[experimental("thread/start.activePermissionProfile")]
    #[serde(default)]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(
    Serialize, Deserialize, Debug, Default, Clone, PartialEq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSettingsUpdateParams {
    pub thread_id: String,
    /// Override the working directory for subsequent turns.
    #[ts(optional = nullable)]
    pub cwd: Option<PathBuf>,
    /// Override the approval policy for subsequent turns.
    #[experimental(nested)]
    #[ts(optional = nullable)]
    pub approval_policy: Option<AskForApproval>,
    /// Override where approval requests are routed for subsequent turns.
    #[ts(optional = nullable)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    /// Override the sandbox policy for subsequent turns.
    #[ts(optional = nullable)]
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Select a named permissions profile id for subsequent turns. Cannot be
    /// combined with `sandboxPolicy`.
    #[experimental("thread/settings/update.permissions")]
    #[ts(optional = nullable)]
    pub permissions: Option<String>,
    /// Override the model for subsequent turns.
    #[ts(optional = nullable)]
    pub model: Option<String>,
    /// Override the service tier for subsequent turns. `null` clears the
    /// current service tier; omission leaves it unchanged.
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub service_tier: Option<Option<String>>,
    /// Override the reasoning effort for subsequent turns.
    #[ts(optional = nullable)]
    pub effort: Option<ReasoningEffort>,
    /// Override the reasoning summary for subsequent turns.
    #[ts(optional = nullable)]
    pub summary: Option<ReasoningSummary>,
    /// EXPERIMENTAL - Set a pre-set collaboration mode for subsequent turns.
    ///
    /// For `collaboration_mode.settings.developer_instructions`, `null` means
    /// "use the built-in instructions for the selected mode".
    #[experimental("thread/settings/update.collaborationMode")]
    #[ts(optional = nullable)]
    pub collaboration_mode: Option<CollaborationMode>,
    /// Override the personality for subsequent turns.
    #[ts(optional = nullable)]
    pub personality: Option<Personality>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSettingsUpdateResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSettings {
    pub cwd: AbsolutePathBuf,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub sandbox_policy: SandboxPolicy,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub model: String,
    pub model_provider: String,
    pub service_tier: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub summary: Option<ReasoningSummary>,
    pub collaboration_mode: CollaborationMode,
    pub personality: Option<Personality>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSettingsUpdatedNotification {
    pub thread_id: String,
    pub thread_settings: ThreadSettings,
}

#[derive(
    Serialize, Deserialize, Debug, Default, Clone, PartialEq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// There are three ways to resume a thread:
/// 1. By thread_id: load the thread from disk by thread_id and resume it.
/// 2. By history: instantiate the thread from memory and resume it.
/// 3. By path: load the thread from disk by path and resume it.
///
/// For non-running threads, the precedence is: history > non-empty path > thread_id.
/// If using history or a non-empty path for a non-running thread, the thread_id
/// param will be ignored.
///
/// If thread_id identifies a running thread, app-server rejoins that thread and
/// treats a non-empty path as a consistency check against the active rollout path.
/// Empty string path values are treated as absent.
///
/// Prefer using thread_id whenever possible.
///
/// 恢复一个线程的三种来源及优先级（针对「非运行中」线程）：
///   history（内存历史，实验性，仅 Codex Cloud）＞ 非空 path（按磁盘路径加载）
///   ＞ thread_id（按 id 从磁盘加载）。用 history 或非空 path 时，thread_id 被忽略。
/// 若 thread_id 命中一个「正在运行」的线程，则 app-server 直接重新接入该线程，
/// 并把非空 path 当作「与当前 rollout 路径是否一致」的校验项。空字符串 path 视为未提供。
/// 实践中应尽量用 thread_id。其余字段与 `ThreadStartParams` 同义，为恢复后的配置覆盖。
pub struct ThreadResumeParams {
    pub thread_id: String,

    /// [UNSTABLE] FOR CODEX CLOUD - DO NOT USE.
    /// If specified, the thread will be resumed with the provided history
    /// instead of loaded from disk.
    #[experimental("thread/resume.history")]
    #[ts(optional = nullable)]
    pub history: Option<Vec<ResponseItem>>,

    /// [UNSTABLE] Specify the rollout path to resume from.
    /// If specified for a non-running thread, the thread_id param will be ignored.
    /// If thread_id identifies a running thread, the path must match the active
    /// rollout path.
    #[experimental("thread/resume.path")]
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_empty_path_as_none"
    )]
    #[ts(optional = nullable)]
    pub path: Option<PathBuf>,

    /// Configuration overrides for the resumed thread, if any.
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub model_provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub service_tier: Option<Option<String>>,
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    /// Replace the thread's runtime workspace roots. Relative paths are
    /// resolved against the effective cwd for the thread.
    #[experimental("thread/resume.runtimeWorkspaceRoots")]
    #[ts(optional = nullable)]
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    #[experimental(nested)]
    #[ts(optional = nullable)]
    pub approval_policy: Option<AskForApproval>,
    /// Override where approval requests are routed for review on this thread
    /// and subsequent turns.
    #[ts(optional = nullable)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    #[ts(optional = nullable)]
    pub sandbox: Option<SandboxMode>,
    /// Named profile id for the resumed thread. Cannot be combined with
    /// `sandbox`.
    #[experimental("thread/resume.permissions")]
    #[ts(optional = nullable)]
    pub permissions: Option<String>,
    #[ts(optional = nullable)]
    pub config: Option<HashMap<String, serde_json::Value>>,
    #[ts(optional = nullable)]
    pub base_instructions: Option<String>,
    #[ts(optional = nullable)]
    pub developer_instructions: Option<String>,
    #[ts(optional = nullable)]
    pub personality: Option<Personality>,
    /// When true, return only thread metadata and live-resume state without
    /// populating `thread.turns`. This is useful when the client plans to call
    /// `thread/turns/list` immediately after resuming.
    #[experimental("thread/resume.excludeTurns")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_turns: bool,
    /// When present, include a `thread/turns/list` page in the resume response
    /// so clients can bootstrap recent turns without a second request.
    #[experimental("thread/resume.initialTurnsPage")]
    #[ts(optional = nullable)]
    pub initial_turns_page: Option<ThreadResumeInitialTurnsPageParams>,
    /// Deprecated and ignored by app-server. Kept only so older clients can
    /// continue sending the field while rollout persistence always uses the
    /// limited history policy.
    #[experimental("thread/resume.persistFullHistory")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist_extended_history: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadResumeResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub service_tier: Option<String>,
    pub cwd: AbsolutePathBuf,
    /// Thread-scoped runtime workspace roots used to materialize
    /// `:workspace_roots`.
    #[experimental("thread/resume.runtimeWorkspaceRoots")]
    #[serde(default)]
    pub runtime_workspace_roots: Vec<AbsolutePathBuf>,
    /// Instruction source files currently loaded for this thread.
    #[serde(default)]
    pub instruction_sources: Vec<AbsolutePathBuf>,
    #[experimental(nested)]
    pub approval_policy: AskForApproval,
    /// Reviewer currently used for approval requests on this thread.
    pub approvals_reviewer: ApprovalsReviewer,
    /// Legacy sandbox policy retained for compatibility. Experimental clients
    /// should prefer `activePermissionProfile` for profile provenance.
    pub sandbox: SandboxPolicy,
    /// Named or implicit built-in profile that produced the active
    /// permissions, when known.
    #[experimental("thread/resume.activePermissionProfile")]
    #[serde(default)]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// `thread/turns/list` page returned when requested by `initialTurnsPage`.
    #[experimental("thread/resume.initialTurnsPage")]
    #[serde(default)]
    pub initial_turns_page: Option<TurnsPage>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadResumeInitialTurnsPageParams {
    /// Optional turn page size.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional turn pagination direction; defaults to descending.
    #[ts(optional = nullable)]
    pub sort_direction: Option<SortDirection>,
    /// How much item detail to include for each returned turn; defaults to summary.
    #[ts(optional = nullable)]
    pub items_view: Option<TurnItemsView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnsPage {
    pub data: Vec<Turn>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

impl From<ThreadTurnsListResponse> for TurnsPage {
    fn from(response: ThreadTurnsListResponse) -> Self {
        Self {
            data: response.data,
            next_cursor: response.next_cursor,
            backwards_cursor: response.backwards_cursor,
        }
    }
}

#[derive(
    Serialize, Deserialize, Debug, Default, Clone, PartialEq, JsonSchema, TS, ExperimentalApi,
)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// There are two ways to fork a thread:
/// 1. By thread_id: load the thread from disk by thread_id and fork it into a new thread.
/// 2. By path: load the thread from disk by path and fork it into a new thread.
///
/// If using a non-empty path, the thread_id param will be ignored.
/// Empty string path values are treated as absent.
///
/// Prefer using thread_id whenever possible.
///
/// 派生（fork）一个线程到「新线程」的两种来源：按 thread_id 从磁盘加载后 fork，
/// 或按非空 path 从磁盘加载后 fork（此时 thread_id 被忽略）。空字符串 path 视为未提供。
/// 与 resume 的区别：fork 产出的是一条全新线程（新 id），原线程不受影响。
pub struct ThreadForkParams {
    pub thread_id: String,

    /// [UNSTABLE] Specify the rollout path to fork from.
    /// If specified, the thread_id param will be ignored.
    #[experimental("thread/fork.path")]
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_empty_path_as_none"
    )]
    #[ts(optional = nullable)]
    pub path: Option<PathBuf>,

    /// Configuration overrides for the forked thread, if any.
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub model_provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub service_tier: Option<Option<String>>,
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    /// Replace the thread's runtime workspace roots. Relative paths are
    /// resolved against the effective cwd for the thread.
    #[experimental("thread/fork.runtimeWorkspaceRoots")]
    #[ts(optional = nullable)]
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    #[experimental(nested)]
    #[ts(optional = nullable)]
    pub approval_policy: Option<AskForApproval>,
    /// Override where approval requests are routed for review on this thread
    /// and subsequent turns.
    #[ts(optional = nullable)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    #[ts(optional = nullable)]
    pub sandbox: Option<SandboxMode>,
    /// Named profile id for the forked thread. Cannot be combined with
    /// `sandbox`.
    #[experimental("thread/fork.permissions")]
    #[ts(optional = nullable)]
    pub permissions: Option<String>,
    #[ts(optional = nullable)]
    pub config: Option<HashMap<String, serde_json::Value>>,
    #[ts(optional = nullable)]
    pub base_instructions: Option<String>,
    #[ts(optional = nullable)]
    pub developer_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    /// Optional client-supplied analytics source classification for this forked thread.
    #[ts(optional = nullable)]
    pub thread_source: Option<ThreadSource>,
    /// When true, return only thread metadata and live fork state without
    /// populating `thread.turns`. This is useful when the client plans to call
    /// `thread/turns/list` immediately after forking.
    #[experimental("thread/fork.excludeTurns")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_turns: bool,
    /// Deprecated and ignored by app-server. Kept only so older clients can
    /// continue sending the field while rollout persistence always uses the
    /// limited history policy.
    #[experimental("thread/fork.persistFullHistory")]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist_extended_history: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, ExperimentalApi)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadForkResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub service_tier: Option<String>,
    pub cwd: AbsolutePathBuf,
    /// Thread-scoped runtime workspace roots used to materialize
    /// `:workspace_roots`.
    #[experimental("thread/fork.runtimeWorkspaceRoots")]
    #[serde(default)]
    pub runtime_workspace_roots: Vec<AbsolutePathBuf>,
    /// Instruction source files currently loaded for this thread.
    #[serde(default)]
    pub instruction_sources: Vec<AbsolutePathBuf>,
    #[experimental(nested)]
    pub approval_policy: AskForApproval,
    /// Reviewer currently used for approval requests on this thread.
    pub approvals_reviewer: ApprovalsReviewer,
    /// Legacy sandbox policy retained for compatibility. Experimental clients
    /// should prefer `activePermissionProfile` for profile provenance.
    pub sandbox: SandboxPolicy,
    /// Named or implicit built-in profile that produced the active
    /// permissions, when known.
    #[experimental("thread/fork.activePermissionProfile")]
    #[serde(default)]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadArchiveParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadArchiveResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadUnsubscribeParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadUnsubscribeResponse {
    pub status: ThreadUnsubscribeStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ThreadUnsubscribeStatus {
    NotLoaded,
    NotSubscribed,
    Unsubscribed,
}

/// Parameters for `thread/increment_elicitation`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadIncrementElicitationParams {
    /// Thread whose out-of-band elicitation counter should be incremented.
    pub thread_id: String,
}

/// Response for `thread/increment_elicitation`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadIncrementElicitationResponse {
    /// Current out-of-band elicitation count after the increment.
    pub count: u64,
    /// Whether timeout accounting is paused after applying the increment.
    pub paused: bool,
}

/// Parameters for `thread/decrement_elicitation`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadDecrementElicitationParams {
    /// Thread whose out-of-band elicitation counter should be decremented.
    pub thread_id: String,
}

/// Response for `thread/decrement_elicitation`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadDecrementElicitationResponse {
    /// Current out-of-band elicitation count after the decrement.
    pub count: u64,
    /// Whether timeout accounting remains paused after applying the decrement.
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSetNameParams {
    pub thread_id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadUnarchiveParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSetNameResponse {}

// `v2_enum_from_core!` 宏：声明一个 wire 层枚举，并自动生成它与 `codex_protocol`
// 核心枚举（`CoreThreadGoalStatus`）之间的双向 `From` 转换，避免手写样板。
// 这里定义「目标状态」：活跃 / 暂停 / 受阻 / 用量受限 / 预算受限 / 已完成。
v2_enum_from_core! {
    pub enum ThreadGoalStatus from CoreThreadGoalStatus {
        Active,
        Paused,
        Blocked,
        UsageLimited,
        BudgetLimited,
        Complete,
    }
}

/// goal-mode 的「目标」：给线程设定一个客观目标与可选的 token 预算，并跟踪其
/// 消耗（tokens_used / time_used_seconds）与状态。时间戳/计数用 `i64` 并在 TS 侧
/// 映射为 `number`。下方 `From<core::ThreadGoal>` 负责从核心类型转换而来。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoal {
    pub thread_id: String,
    pub objective: String,
    pub status: ThreadGoalStatus,
    #[ts(type = "number | null")]
    pub token_budget: Option<i64>,
    #[ts(type = "number")]
    pub tokens_used: i64,
    #[ts(type = "number")]
    pub time_used_seconds: i64,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

impl From<codex_protocol::protocol::ThreadGoal> for ThreadGoal {
    fn from(value: codex_protocol::protocol::ThreadGoal) -> Self {
        Self {
            thread_id: value.thread_id.to_string(),
            objective: value.objective,
            status: value.status.into(),
            token_budget: value.token_budget,
            tokens_used: value.tokens_used,
            time_used_seconds: value.time_used_seconds,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalSetParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub objective: Option<String>,
    #[ts(optional = nullable)]
    pub status: Option<ThreadGoalStatus>,
    #[serde(
        default,
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable, type = "number | null")]
    pub token_budget: Option<Option<i64>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalSetResponse {
    pub goal: ThreadGoal,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalGetParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalGetResponse {
    pub goal: Option<ThreadGoal>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalClearParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalClearResponse {
    pub cleared: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMetadataUpdateParams {
    pub thread_id: String,
    /// Patch the stored Git metadata for this thread.
    /// Omit a field to leave it unchanged, set it to `null` to clear it, or
    /// provide a string to replace the stored value.
    #[ts(optional = nullable)]
    pub git_info: Option<ThreadMetadataGitInfoUpdateParams>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMetadataGitInfoUpdateParams {
    /// Omit to leave the stored commit unchanged, set to `null` to clear it,
    /// or provide a non-empty string to replace it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option"
    )]
    #[ts(optional = nullable, type = "string | null")]
    pub sha: Option<Option<String>>,
    /// Omit to leave the stored branch unchanged, set to `null` to clear it,
    /// or provide a non-empty string to replace it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option"
    )]
    #[ts(optional = nullable, type = "string | null")]
    pub branch: Option<Option<String>>,
    /// Omit to leave the stored origin URL unchanged, set to `null` to clear it,
    /// or provide a non-empty string to replace it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::protocol::serde_helpers::serialize_double_option",
        deserialize_with = "crate::protocol::serde_helpers::deserialize_double_option"
    )]
    #[ts(optional = nullable, type = "string | null")]
    pub origin_url: Option<Option<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMetadataUpdateResponse {
    pub thread: Thread,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ThreadMemoryMode {
    Enabled,
    Disabled,
}

impl ThreadMemoryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    pub fn to_core(self) -> codex_protocol::protocol::ThreadMemoryMode {
        match self {
            Self::Enabled => codex_protocol::protocol::ThreadMemoryMode::Enabled,
            Self::Disabled => codex_protocol::protocol::ThreadMemoryMode::Disabled,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMemoryModeSetParams {
    pub thread_id: String,
    pub mode: ThreadMemoryMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadMemoryModeSetResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryResetResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadUnarchiveResponse {
    pub thread: Thread,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadCompactStartParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadCompactStartResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadShellCommandParams {
    pub thread_id: String,
    /// Shell command string evaluated by the thread's configured shell.
    /// Unlike `command/exec`, this intentionally preserves shell syntax
    /// such as pipes, redirects, and quoting. This runs unsandboxed with full
    /// access rather than inheriting the thread sandbox policy.
    /// 由线程配置的 shell 直接求值的命令串。与 `command/exec` 不同，这里刻意保留
    /// 管道、重定向、引号等 shell 语法。⚠️ 安全注意：此命令「不进沙箱、以完整权限」
    /// 运行，不继承线程的 sandbox 策略——属于受信任路径，调用方需自行担保安全性。
    pub command: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadShellCommandResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadApproveGuardianDeniedActionParams {
    pub thread_id: String,
    /// Serialized `codex_protocol::protocol::GuardianAssessmentEvent`.
    pub event: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadApproveGuardianDeniedActionResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadBackgroundTerminalsCleanParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadBackgroundTerminalsCleanResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// `thread/rollback` 入参：从线程末尾回退 `num_turns` 个轮次。
pub struct ThreadRollbackParams {
    pub thread_id: String,
    /// The number of turns to drop from the end of the thread. Must be >= 1.
    ///
    /// This only modifies the thread's history and does not revert local file changes
    /// that have been made by the agent. Clients are responsible for reverting these changes.
    /// 要从线程尾部丢弃的轮次数，须 ≥ 1。⚠️ 仅修改线程「历史记录」，并不会回滚 agent
    /// 已对本地文件做出的改动——还原这些文件改动是「客户端的责任」，协议层不负责。
    pub num_turns: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRollbackResponse {
    /// The updated thread after applying the rollback, with `turns` populated.
    ///
    /// The ThreadItems stored in each Turn are lossy since we explicitly do not
    /// persist all agent interactions, such as command executions. This is the same
    /// behavior as `thread/resume`.
    pub thread: Thread,
}

/// `thread/list` 入参：分页列举线程，支持按 provider / 来源类型 / 是否归档 / cwd /
/// 标题子串等过滤，并可选排序键与方向。各字段均可选，缺省走服务端合理默认值。
/// 其中 `use_state_db_only` 控制是否「只查状态库、跳过扫描 JSONL rollout 修复元数据」，
/// 关闭它（默认）会保留更慢但更准的 scan-and-repair 行为。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadListParams {
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size; defaults to a reasonable server-side value.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional sort key; defaults to created_at.
    #[ts(optional = nullable)]
    pub sort_key: Option<ThreadSortKey>,
    /// Optional sort direction; defaults to descending (newest first).
    #[ts(optional = nullable)]
    pub sort_direction: Option<SortDirection>,
    /// Optional provider filter; when set, only sessions recorded under these
    /// providers are returned. When present but empty, includes all providers.
    #[ts(optional = nullable)]
    pub model_providers: Option<Vec<String>>,
    /// Optional source filter; when set, only sessions from these source kinds
    /// are returned. When omitted or empty, defaults to interactive sources.
    #[ts(optional = nullable)]
    pub source_kinds: Option<Vec<ThreadSourceKind>>,
    /// Optional archived filter; when set to true, only archived threads are returned.
    /// If false or null, only non-archived threads are returned.
    #[ts(optional = nullable)]
    pub archived: Option<bool>,
    /// Optional cwd filter or filters; when set, only threads whose session cwd
    /// exactly matches one of these paths are returned.
    #[ts(optional = nullable, type = "string | Array<string> | null")]
    pub cwd: Option<ThreadListCwdFilter>,
    /// If true, return from the state DB without scanning JSONL rollouts to
    /// repair thread metadata. Omitted or false preserves scan-and-repair
    /// behavior.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub use_state_db_only: bool,
    /// Optional substring filter for the extracted thread title.
    #[ts(optional = nullable)]
    pub search_term: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSearchParams {
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size; defaults to a reasonable server-side value.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional sort key; defaults to created_at.
    #[ts(optional = nullable)]
    pub sort_key: Option<ThreadSortKey>,
    /// Optional sort direction; defaults to descending (newest first).
    #[ts(optional = nullable)]
    pub sort_direction: Option<SortDirection>,
    /// Optional source filter; when set, only sessions from these source kinds
    /// are returned. When omitted or empty, defaults to interactive sources.
    #[ts(optional = nullable)]
    pub source_kinds: Option<Vec<ThreadSourceKind>>,
    /// Optional archived filter; when set to true, only archived threads are returned.
    /// If false or null, only non-archived threads are returned.
    #[ts(optional = nullable)]
    pub archived: Option<bool>,
    /// Required substring/full-text query for thread search.
    pub search_term: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum ThreadListCwdFilter {
    One(String),
    Many(Vec<String>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadSourceKind {
    Cli,
    #[serde(rename = "vscode")]
    #[ts(rename = "vscode")]
    VsCode,
    Exec,
    AppServer,
    SubAgent,
    SubAgentReview,
    SubAgentCompact,
    SubAgentThreadSpawn,
    SubAgentOther,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum ThreadSortKey {
    CreatedAt,
    UpdatedAt,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// `thread/list` 响应。本文件多处列举响应共用同一套「双向游标」分页约定，集中说明如下：
///   - `next_cursor`：不透明游标，传给下一次调用以「接着最后一项往后翻」；为 None 表示翻完。
///   - `backwards_cursor`：不透明游标，配合「相反的 sortDirection」用于反向翻页；仅当本页
///     至少有一条记录时才填充。对时间戳排序，它锚定在本页时间戳起点，以免漏掉同一秒内的更新。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadListResponse {
    pub data: Vec<Thread>,
    /// Opaque cursor to pass to the next call to continue after the last item.
    /// if None, there are no more items to return.
    pub next_cursor: Option<String>,
    /// Opaque cursor to pass as `cursor` when reversing `sortDirection`.
    /// This is only populated when the page contains at least one thread.
    /// Use it with the opposite `sortDirection`; for timestamp sorts it anchors
    /// at the start of the page timestamp so same-second updates are not skipped.
    pub backwards_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSearchResult {
    pub thread: Thread,
    pub snippet: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSearchResponse {
    pub data: Vec<ThreadSearchResult>,
    /// Opaque cursor to pass to the next call to continue after the last item.
    /// if None, there are no more items to return.
    pub next_cursor: Option<String>,
    /// Opaque cursor to pass as `cursor` when reversing `sortDirection`.
    /// This is only populated when the page contains at least one thread.
    /// Use it with the opposite `sortDirection`; for timestamp sorts it anchors
    /// at the start of the page timestamp so same-second updates are not skipped.
    pub backwards_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadLoadedListParams {
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size; defaults to no limit.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadLoadedListResponse {
    /// Thread ids for sessions currently loaded in memory.
    pub data: Vec<String>,
    /// Opaque cursor to pass to the next call to continue after the last item.
    /// if None, there are no more items to return.
    pub next_cursor: Option<String>,
}

/// 线程的运行状态（serde 以 `type` 字段做内部标记）：
///   - `NotLoaded`：未加载进内存。
///   - `Idle`：已加载且空闲，无进行中的轮次。
///   - `SystemError`：因系统错误处于异常态。
///   - `Active { active_flags }`：有进行中的轮次；`active_flags` 进一步说明它当前
///     在等待什么（等待审批 / 等待用户输入）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Active {
        active_flags: Vec<ThreadActiveFlag>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ThreadActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadReadParams {
    pub thread_id: String,
    /// When true, include turns and their items from rollout history.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_turns: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadReadResponse {
    pub thread: Thread,
}

/// `thread/inject_items` 入参：把一批「原始 Responses API items」直接追加进线程的
/// 模型可见历史，而「不」触发一个用户轮次。用于在不发起对话的前提下植入上下文。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadInjectItemsParams {
    pub thread_id: String,
    /// Raw Responses API items to append to the thread's model-visible history.
    pub items: Vec<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadInjectItemsResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTurnsListParams {
    pub thread_id: String,
    /// Opaque cursor to pass to the next call to continue after the last turn.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional turn page size.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional turn pagination direction; defaults to descending.
    #[ts(optional = nullable)]
    pub sort_direction: Option<SortDirection>,
    /// How much item detail to include for each returned turn; defaults to summary.
    #[ts(optional = nullable)]
    pub items_view: Option<TurnItemsView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTurnsListResponse {
    pub data: Vec<Turn>,
    /// Opaque cursor to pass to the next call to continue after the last turn.
    /// if None, there are no more turns to return.
    pub next_cursor: Option<String>,
    /// Opaque cursor to pass as `cursor` when reversing `sortDirection`.
    /// This is only populated when the page contains at least one turn.
    /// Use it with the opposite `sortDirection` to include the anchor turn again
    /// and catch updates to that turn.
    pub backwards_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTurnsItemsListParams {
    pub thread_id: String,
    pub turn_id: String,
    /// Opaque cursor to pass to the next call to continue after the last item.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional item page size.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional item pagination direction; defaults to ascending.
    #[ts(optional = nullable)]
    pub sort_direction: Option<SortDirection>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTurnsItemsListResponse {
    pub data: Vec<ThreadItem>,
    /// Opaque cursor to pass to the next call to continue after the last item.
    /// if None, there are no more items to return.
    pub next_cursor: Option<String>,
    /// Opaque cursor to pass as `cursor` when reversing `sortDirection`.
    /// This is only populated when the page contains at least one item.
    pub backwards_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTokenUsageUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

/// 线程的 token 用量：`total` 为累计、`last` 为最近一次请求的明细；
/// `model_context_window` 为当前模型的上下文窗口大小（暂为可选，见 TODO）。
/// 由核心类型 `CoreTokenUsageInfo` 经下方 `From` 转换得来，通过
/// `ThreadTokenUsageUpdatedNotification` 推送给客户端用于展示用量进度。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadTokenUsage {
    pub total: TokenUsageBreakdown,
    pub last: TokenUsageBreakdown,
    // TODO(aibrahim): make this not optional
    #[ts(type = "number | null")]
    pub model_context_window: Option<i64>,
}

impl From<CoreTokenUsageInfo> for ThreadTokenUsage {
    fn from(value: CoreTokenUsageInfo) -> Self {
        Self {
            total: value.total_token_usage.into(),
            last: value.last_token_usage.into(),
            model_context_window: value.model_context_window,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TokenUsageBreakdown {
    #[ts(type = "number")]
    pub total_tokens: i64,
    #[ts(type = "number")]
    pub input_tokens: i64,
    #[ts(type = "number")]
    pub cached_input_tokens: i64,
    #[ts(type = "number")]
    pub output_tokens: i64,
    #[ts(type = "number")]
    pub reasoning_output_tokens: i64,
}

impl From<CoreTokenUsage> for TokenUsageBreakdown {
    fn from(value: CoreTokenUsage) -> Self {
        Self {
            total_tokens: value.total_tokens,
            input_tokens: value.input_tokens,
            cached_input_tokens: value.cached_input_tokens,
            output_tokens: value.output_tokens,
            reasoning_output_tokens: value.reasoning_output_tokens,
        }
    }
}

// Thread/Turn lifecycle notifications and item progress events
// ── 线程生命周期通知 · 服务端单向推送（无需响应）─────────────────────────
// 以下 `Thread*Notification` 由服务端经 `ServerNotification` 推送给客户端，反映线程
// 的启动/状态变化/归档/关闭/改名/目标变更/token 用量更新等。结构都很直白，多为
// 「thread_id + 该事件的少量字段」，故不逐个加注释。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadStartedNotification {
    pub thread: Thread,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadStatusChangedNotification {
    pub thread_id: String,
    pub status: ThreadStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadArchivedNotification {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadUnarchivedNotification {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadClosedNotification {
    pub thread_id: String,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadNameUpdatedNotification {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub thread_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalUpdatedNotification {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub goal: ThreadGoal,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadGoalClearedNotification {
    pub thread_id: String,
}

/// Deprecated: Use `ContextCompaction` item type instead.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextCompactedNotification {
    pub thread_id: String,
    pub turn_id: String,
}
