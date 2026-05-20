//! 线程（thread，旧称 conversation）的顶层管理器。
//!
//! 【文件职责】
//! - 维护「内存中的所有活跃线程」`HashMap<ThreadId, Arc<CodexThread>>`；
//! - 提供 start / resume / fork / spawn_subagent / shutdown 全套生命周期 API；
//! - 把 `CodexSpawnArgs` 喂给 `Codex::spawn` 拉起 Session，并把首个
//!   `SessionConfigured` 事件转成 `NewThread` 返回给上层。
//!
//! 【架构位置】
//! ```text
//! ApplicationServer / CLI
//!   └─ ThreadManager           ← 本文件：线程级总控
//!         └─ Arc<ThreadManagerState>   ← 共享内部状态（threads / managers）
//!               └─ spawn_thread_*      ← 最终的 spawn 主路径
//!                     └─ Codex::spawn  ← session/mod.rs
//! ```
//!
//! 【阅读建议】
//!   1. 先看 `NewThread` / `ThreadManager` / `ThreadManagerState` 结构定义；
//!   2. 再看入口三件套：`start_thread_with_options` / `resume_thread_from_rollout`
//!      / `fork_thread`；
//!   3. 最后看公共主路径 `spawn_thread_with_source` → `finalize_thread_spawn`；
//!   4. `ForkSnapshot` 和 `fork_history_from_snapshot` 是 fork 历史截断的算法。

use crate::SkillsManager;
use crate::agent::AgentControl;
use crate::attestation::AttestationProvider;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::ThreadStoreConfig;
use crate::environment_selection::default_thread_environment_selections;
use crate::environment_selection::resolve_environment_selections;
use crate::mcp::McpManager;
use crate::rollout::truncation;
use crate::session::Codex;
use crate::session::CodexSpawnArgs;
use crate::session::CodexSpawnOk;
use crate::session::INITIAL_SUBMIT_ID;
use crate::shell_snapshot::ShellSnapshot;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::TurnStatus;
use codex_core_plugins::PluginsManager;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::empty_extension_registry;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
#[cfg(test)]
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::W3cTraceContext;
use codex_rollout::state_db::StateDbHandle;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadMetadataPatch;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::UpdateThreadMetadataParams;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::warn;

// ═══════════════════════════════════════════════════════════════
// Constants & Test toggles  ·  常量与测试开关
// ═══════════════════════════════════════════════════════════════

/// 线程创建广播通道容量：1024 足够吸收瞬时尖峰；如有订阅者掉线导致积压超过
/// 此值，最早的通知会被 `broadcast::channel` 静默丢弃，订阅者将收到 `Lagged`。
const THREAD_CREATED_CHANNEL_CAPACITY: usize = 1024;

/// Test-only override for enabling thread-manager behaviors used by integration
/// tests.
///
/// In production builds this value should remain at its default (`false`) and
/// must not be toggled.
/// 仅集成测试使用的全局开关：开启后启用 `ops_log` 等仅测试用的旁路行为。
/// 生产环境必须保持默认 false。
/// [引用范围] 由 `set_thread_manager_test_mode_for_tests` 写、由
/// `should_use_test_thread_manager_behavior` 读、由 `ThreadManager::new` 和
/// 一些 `_for_tests` 构造器在判断是否初始化 `ops_log` 时间接消费。
static FORCE_TEST_THREAD_MANAGER_BEHAVIOR: AtomicBool = AtomicBool::new(false);

type CapturedOps = Vec<(ThreadId, Op)>;
type SharedCapturedOps = Arc<std::sync::Mutex<CapturedOps>>;

pub(crate) fn set_thread_manager_test_mode_for_tests(enabled: bool) {
    FORCE_TEST_THREAD_MANAGER_BEHAVIOR.store(enabled, Ordering::Relaxed);
}

fn should_use_test_thread_manager_behavior() -> bool {
    FORCE_TEST_THREAD_MANAGER_BEHAVIOR.load(Ordering::Relaxed)
}

/// RAII：测试用的临时 codex_home 目录在 Drop 时递归删除。
/// 出错时静默忽略（测试结束 fs 状态不影响判定）。
struct TempCodexHomeGuard {
    path: PathBuf,
}

impl Drop for TempCodexHomeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ═══════════════════════════════════════════════════════════════
// 公开数据类型
// ═══════════════════════════════════════════════════════════════

/// Represents a newly created Codex thread (formerly called a conversation), including the first event
/// (which is [`EventMsg::SessionConfigured`]).
/// 新创建的 Codex 线程的返回包：thread_id + Arc<CodexThread> + 首个事件。
/// 首事件保证是 `SessionConfigured`，由 `finalize_thread_spawn` 校验。
pub struct NewThread {
    pub thread_id: ThreadId,
    pub thread: Arc<CodexThread>,
    pub session_configured: SessionConfiguredEvent,
}

// TODO(ccunningham): Add an explicit non-interrupting live-turn snapshot once
// core can represent sampling boundaries directly instead of relying on
// whichever items happened to be persisted mid-turn.
//
// Two likely future variants:
// - `TruncateToLastSamplingBoundary` for callers that want a coherent fork from
//   the last stable model boundary without synthesizing an interrupt.
// - `WaitUntilNextSamplingBoundary` (or similar) for callers that prefer to
//   fork after the next sampling boundary rather than interrupting immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkSnapshot {
    /// Fork a committed prefix ending strictly before the nth user message.
    ///
    /// When `n` is within range, this cuts before that 0-based user-message
    /// boundary. When `n` is out of range and the source thread is currently
    /// mid-turn, this instead cuts before the active turn's opening boundary
    /// so the fork drops the unfinished turn suffix. When `n` is out of range
    /// and the source thread is already at a turn boundary, this returns the
    /// full committed history unchanged.
    /// 在「第 n 条 user 消息」边界严格之前切一刀做 fork。
    ///
    /// - n 在范围内 → 切到该 0 基索引边界之前；
    /// - n 越界 + 源线程 mid-turn → 切到当前活跃 Turn 的开始边界之前
    ///   （丢弃未完成的尾部）；
    /// - n 越界 + 源线程已到 Turn 边界 → 返回完整 committed history。
    TruncateBeforeNthUserMessage(usize),

    /// Fork the current persisted history as if the source thread had been
    /// interrupted now.
    ///
    /// If the persisted snapshot ends mid-turn, this appends the same
    /// `<turn_aborted>` marker produced by a real interrupt. If the snapshot is
    /// already at a turn boundary, this returns the current persisted history
    /// unchanged.
    /// 把当前持久化历史「视为刚被中断」来 fork。
    ///
    /// snapshot 在 mid-turn 状态时追加和真实 interrupt 一致的
    /// `<turn_aborted>` 标记，让 fork 出来的历史里中断点是合法 Turn 边界；
    /// 已经在 Turn 边界时则原样返回。
    Interrupted,
}

/// Preserve legacy `fork_thread(usize, ...)` callsites by mapping them to the
/// existing truncate-before-nth-user-message snapshot mode.
/// 兼容老 API：`fork_thread(usize, ...)` 自动映射到
/// `TruncateBeforeNthUserMessage` 模式。
impl From<usize> for ForkSnapshot {
    fn from(value: usize) -> Self {
        Self::TruncateBeforeNthUserMessage(value)
    }
}

/// `shutdown_all_threads_bounded` 的返回：按结果分桶给出 thread_id 列表。
/// 调用方可按 `submit_failed` / `timed_out` 做重试或诊断。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThreadShutdownReport {
    pub completed: Vec<ThreadId>,
    pub submit_failed: Vec<ThreadId>,
    pub timed_out: Vec<ThreadId>,
}

enum ShutdownOutcome {
    Complete,
    SubmitFailed,
    TimedOut,
}

// ═══════════════════════════════════════════════════════════════
// ThreadManager  ·  顶层管理器
// ═══════════════════════════════════════════════════════════════

/// [`ThreadManager`] is responsible for creating threads and maintaining
/// them in memory.
/// `ThreadManager` 负责创建线程并在内存中维护它们。
///
/// 真实状态都在 `Arc<ThreadManagerState>` 里：用 Arc 是为了让 `AgentControl`
/// 能以 `Weak` 引用回它而无需让所有内部函数都接收 `Arc<&Self>`。
pub struct ThreadManager {
    state: Arc<ThreadManagerState>,
    /// 仅测试场景持有：临时 codex_home 目录的 RAII 句柄，Drop 时清理。
    _test_codex_home_guard: Option<TempCodexHomeGuard>,
}

/// 启动新线程的参数集合。聚成一个 struct 是为了避免函数签名爆炸（曾经是 14 个
/// 散参数）。所有 `Option` 字段都有合理的默认推导逻辑（见 `start_thread_with_options`）。
pub struct StartThreadOptions {
    pub config: Config,
    pub initial_history: InitialHistory,
    pub session_source: Option<SessionSource>,
    pub thread_source: Option<ThreadSource>,
    pub dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
    pub persist_extended_history: bool,
    pub metrics_service_name: Option<String>,
    pub parent_trace: Option<W3cTraceContext>,
    pub environments: Vec<TurnEnvironmentSelection>,
}

pub(crate) struct ResumeThreadWithHistoryOptions {
    pub(crate) config: Config,
    pub(crate) initial_history: InitialHistory,
    pub(crate) agent_control: AgentControl,
    pub(crate) session_source: SessionSource,
    pub(crate) inherited_shell_snapshot: Option<Arc<ShellSnapshot>>,
    pub(crate) inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
}

/// Shared, `Arc`-owned state for [`ThreadManager`]. This `Arc` is required to have a single
/// `Arc` reference that can be downgraded to by `AgentControl` while preventing every single
/// function to require an `Arc<&Self>`.
/// `ThreadManager` 的共享内部状态。
///
/// 用 `Arc<ThreadManagerState>` 是有意为之 —— `AgentControl` 需要能向下转
/// `Weak<ThreadManagerState>` 反向引用，而不让外层每个函数都背一个
/// `Arc<&Self>`（这会污染所有签名）。
///
/// 【字段分类】
///   - 状态：`threads`、`thread_created_tx`、`ops_log`
///   - 依赖管理器：`auth_manager` / `models_manager` / `environment_manager`
///     / `skills_manager` / `plugins_manager` / `mcp_manager`
///   - 平台资源：`extensions` / `thread_store` / `attestation_provider`
///     / `state_db` / `analytics_events_client`
///   - 元数据：`session_source` / `installation_id`
pub(crate) struct ThreadManagerState {
    threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>,
    thread_created_tx: broadcast::Sender<ThreadId>,
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
    environment_manager: Arc<EnvironmentManager>,
    skills_manager: Arc<SkillsManager>,
    plugins_manager: Arc<PluginsManager>,
    mcp_manager: Arc<McpManager>,
    extensions: Arc<ExtensionRegistry<Config>>,
    thread_store: Arc<dyn ThreadStore>,
    attestation_provider: Option<Arc<dyn AttestationProvider>>,
    session_source: SessionSource,
    installation_id: String,
    analytics_events_client: Option<AnalyticsEventsClient>,
    state_db: Option<StateDbHandle>,
    // Captures submitted ops for testing purpose when test mode is enabled.
    // 测试模式下记录所有 submit 过的 Op；生产环境此字段为 None，零开销。
    ops_log: Option<SharedCapturedOps>,
}

// ───────────────────────────────────────────────────────────────
// 顶层工厂函数
// ───────────────────────────────────────────────────────────────

/// 根据 Config 构造 `SharedModelsManager`：用 model_provider 创建 provider，
/// 再委托其 `models_manager` 工厂方法。
pub fn build_models_manager(
    config: &Config,
    auth_manager: Arc<AuthManager>,
) -> SharedModelsManager {
    let provider = create_model_provider(config.model_provider.clone(), Some(auth_manager));
    provider.models_manager(
        config.codex_home.to_path_buf(),
        config.model_catalog.clone(),
    )
}

/// 根据 Config 选择并构造 ThreadStore 实现：
///   - `Local` → 落盘 SQLite + JSONL 的 `LocalThreadStore`；
///   - `InMemory { id }` → 共享的 in-memory 单例（按 id 复用）。
pub fn thread_store_from_config(
    config: &Config,
    state_db: Option<StateDbHandle>,
) -> Arc<dyn ThreadStore> {
    match &config.experimental_thread_store {
        ThreadStoreConfig::Local => Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(config),
            state_db,
        )),
        ThreadStoreConfig::InMemory { id } => InMemoryThreadStore::for_id(id),
    }
}

impl ThreadManager {
    // ───────────────────────────────────────────────────────────────
    // 构造（生产 + 测试）
    // ───────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Config,
        auth_manager: Arc<AuthManager>,
        session_source: SessionSource,
        environment_manager: Arc<EnvironmentManager>,
        extensions: Arc<ExtensionRegistry<Config>>,
        analytics_events_client: Option<AnalyticsEventsClient>,
        thread_store: Arc<dyn ThreadStore>,
        state_db: Option<StateDbHandle>,
        installation_id: String,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
    ) -> Self {
        let codex_home = config.codex_home.clone();
        let restriction_product = session_source.restriction_product();
        let (thread_created_tx, _) = broadcast::channel(THREAD_CREATED_CHANNEL_CAPACITY);
        let plugins_manager = Arc::new(PluginsManager::new_with_restriction_product(
            codex_home.to_path_buf(),
            restriction_product,
        ));
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&plugins_manager)));
        let skills_manager = Arc::new(SkillsManager::new_with_restriction_product(
            codex_home,
            config.bundled_skills_enabled(),
            restriction_product,
        ));
        Self {
            state: Arc::new(ThreadManagerState {
                threads: Arc::new(RwLock::new(HashMap::new())),
                thread_created_tx,
                models_manager: build_models_manager(config, auth_manager.clone()),
                environment_manager,
                skills_manager,
                plugins_manager,
                mcp_manager,
                extensions,
                thread_store,
                attestation_provider,
                auth_manager,
                session_source,
                installation_id,
                analytics_events_client,
                state_db,
                ops_log: should_use_test_thread_manager_behavior()
                    .then(|| Arc::new(std::sync::Mutex::new(Vec::new()))),
            }),
            _test_codex_home_guard: None,
        }
    }

    /// Construct with a dummy AuthManager containing the provided CodexAuth.
    /// Used for integration tests: should not be used by ordinary business logic.
    /// 集成测试专用构造器：用临时 codex_home + dummy AuthManager。
    /// 普通业务代码禁止使用。
    pub(crate) fn with_models_provider_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
    ) -> Self {
        set_thread_manager_test_mode_for_tests(/*enabled*/ true);
        let codex_home = std::env::temp_dir().join(format!(
            "codex-thread-manager-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&codex_home)
            .unwrap_or_else(|err| panic!("temp codex home dir create failed: {err}"));
        let mut manager = Self::with_models_provider_and_home_for_tests(
            auth,
            provider,
            codex_home.clone(),
            Arc::new(EnvironmentManager::default_for_tests()),
        );
        // 注入 RAII guard，让 manager Drop 时清理临时目录。
        manager._test_codex_home_guard = Some(TempCodexHomeGuard { path: codex_home });
        manager
    }

    /// Construct with a dummy AuthManager containing the provided CodexAuth and codex home.
    /// Used for integration tests: should not be used by ordinary business logic.
    pub(crate) fn with_models_provider_and_home_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
        codex_home: PathBuf,
        environment_manager: Arc<EnvironmentManager>,
    ) -> Self {
        Self::with_models_provider_home_and_state_for_tests(
            auth,
            provider,
            codex_home,
            environment_manager,
            /*state_db*/ None,
        )
    }

    pub(crate) fn with_models_provider_home_and_state_for_tests(
        auth: CodexAuth,
        provider: ModelProviderInfo,
        codex_home: PathBuf,
        environment_manager: Arc<EnvironmentManager>,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        set_thread_manager_test_mode_for_tests(/*enabled*/ true);
        let auth_manager = AuthManager::from_auth_for_testing(auth);
        let installation_id = uuid::Uuid::new_v4().to_string();
        let skills_codex_home = match AbsolutePathBuf::from_absolute_path_checked(&codex_home) {
            Ok(codex_home) => codex_home,
            Err(err) => panic!("test codex_home should be absolute: {err}"),
        };
        let (thread_created_tx, _) = broadcast::channel(THREAD_CREATED_CHANNEL_CAPACITY);
        let restriction_product = SessionSource::Exec.restriction_product();
        let plugins_manager = Arc::new(PluginsManager::new_with_restriction_product(
            codex_home.clone(),
            restriction_product,
        ));
        let mcp_manager = Arc::new(McpManager::new(Arc::clone(&plugins_manager)));
        let skills_manager = Arc::new(SkillsManager::new_with_restriction_product(
            skills_codex_home,
            /*bundled_skills_enabled*/ true,
            restriction_product,
        ));
        // This test constructor has no Config input. Tests that need a non-local
        // process store should construct ThreadManager::new with an explicit store.
        // 本测试构造器没有 Config 入参；想换非 Local 的 store 要走 `new`。
        let thread_store: Arc<dyn ThreadStore> = Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: codex_home.clone(),
                sqlite_home: codex_home.clone(),
                default_model_provider_id: OPENAI_PROVIDER_ID.to_string(),
            },
            state_db.clone(),
        ));
        Self {
            state: Arc::new(ThreadManagerState {
                threads: Arc::new(RwLock::new(HashMap::new())),
                thread_created_tx,
                models_manager: create_model_provider(provider, Some(auth_manager.clone()))
                    .models_manager(codex_home, /*config_model_catalog*/ None),
                environment_manager,
                skills_manager,
                plugins_manager,
                mcp_manager,
                extensions: empty_extension_registry(),
                thread_store,
                attestation_provider: None,
                auth_manager,
                session_source: SessionSource::Exec,
                installation_id,
                analytics_events_client: None,
                state_db,
                ops_log: should_use_test_thread_manager_behavior()
                    .then(|| Arc::new(std::sync::Mutex::new(Vec::new()))),
            }),
            _test_codex_home_guard: None,
        }
    }

    // ───────────────────────────────────────────────────────────────
    // 子组件只读访问器
    // ───────────────────────────────────────────────────────────────

    pub fn session_source(&self) -> SessionSource {
        self.state.session_source.clone()
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.state.auth_manager.clone()
    }

    pub fn skills_manager(&self) -> Arc<SkillsManager> {
        self.state.skills_manager.clone()
    }

    pub fn plugins_manager(&self) -> Arc<PluginsManager> {
        self.state.plugins_manager.clone()
    }

    pub fn mcp_manager(&self) -> Arc<McpManager> {
        self.state.mcp_manager.clone()
    }

    pub fn environment_manager(&self) -> Arc<EnvironmentManager> {
        self.state.environment_manager.clone()
    }

    pub fn default_environment_selections(
        &self,
        cwd: &AbsolutePathBuf,
    ) -> Vec<TurnEnvironmentSelection> {
        default_thread_environment_selections(self.state.environment_manager.as_ref(), cwd)
    }

    pub fn validate_environment_selections(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> CodexResult<()> {
        resolve_environment_selections(self.state.environment_manager.as_ref(), environments)
            .map(|_| ())
    }

    pub fn get_models_manager(&self) -> SharedModelsManager {
        self.state.models_manager.clone()
    }

    pub async fn list_models(&self, refresh_strategy: RefreshStrategy) -> Vec<ModelPreset> {
        self.state
            .models_manager
            .list_models(refresh_strategy)
            .await
    }

    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.state.models_manager.list_collaboration_modes()
    }

    pub async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.state.list_thread_ids().await
    }

    /// 订阅「线程创建」广播。新订阅者只收到订阅后创建的线程；积压超过
    /// `THREAD_CREATED_CHANNEL_CAPACITY` 时会丢最早的，发回 `Lagged`。
    pub fn subscribe_thread_created(&self) -> broadcast::Receiver<ThreadId> {
        self.state.thread_created_tx.subscribe()
    }

    pub async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        self.state.get_thread(thread_id).await
    }

    /// Updates metadata for loaded and cold threads through one entrypoint.
    ///
    /// Loaded threads route through `CodexThread`/`LiveThread`, so metadata changes stay ordered
    /// with live rollout writes. Cold threads go directly to the store, which owns unloaded JSONL
    /// compatibility and SQLite metadata updates.
    /// 统一入口：无论线程是 loaded（在 ThreadManager 内存里）还是 cold（仅在
    /// 持久化层）都通过此方法改 metadata。
    ///
    /// - loaded → 经 `CodexThread`/`LiveThread`，保证元数据修改与 rollout 写
    ///   操作的顺序；
    /// - cold   → 直接经 thread_store，由它处理 JSONL 兼容性和 SQLite 元数据。
    pub async fn update_thread_metadata(
        &self,
        thread_id: ThreadId,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> CodexResult<StoredThread> {
        if let Ok(thread) = self.get_thread(thread_id).await {
            // ephemeral 线程不持久化，无 metadata 概念，直接拒绝。
            if thread.config_snapshot().await.ephemeral {
                return Err(CodexErr::InvalidRequest(format!(
                    "ephemeral thread does not support metadata updates: {thread_id}"
                )));
            }
            return thread
                .update_thread_metadata(patch, include_archived)
                .await
                .map_err(|err| thread_store_metadata_update_error(thread_id, err));
        }
        // 走 cold 路径：thread 不在内存，直接打到 store。
        self.state
            .thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id,
                patch,
                include_archived,
            })
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                err => thread_store_metadata_update_error(thread_id, err),
            })
    }

    /// List `thread_id` plus all known descendants in its spawn subtree.
    /// 列出 thread_id 自身 + spawn 子树中所有已知 descendant。
    ///
    /// 数据合并两路：
    ///   1. state_db 中持久化的 thread spawn 图（Open + Closed 两种边状态）；
    ///   2. AgentControl 在内存中维护的活跃 agent 子树。
    /// 用 HashSet 去重，最终顺序为「自身 → DB descendant → 内存 descendant」。
    pub async fn list_agent_subtree_thread_ids(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let thread = self.state.get_thread(thread_id).await?;

        let mut subtree_thread_ids = Vec::new();
        let mut seen_thread_ids = HashSet::new();
        subtree_thread_ids.push(thread_id);
        seen_thread_ids.insert(thread_id);

        if let Some(state_db_ctx) = thread.state_db() {
            for status in [
                DirectionalThreadSpawnEdgeStatus::Open,
                DirectionalThreadSpawnEdgeStatus::Closed,
            ] {
                for descendant_id in state_db_ctx
                    .list_thread_spawn_descendants_with_status(thread_id, status)
                    .await
                    .map_err(|err| {
                        CodexErr::Fatal(format!("failed to load thread-spawn descendants: {err}"))
                    })?
                {
                    if seen_thread_ids.insert(descendant_id) {
                        subtree_thread_ids.push(descendant_id);
                    }
                }
            }
        }

        for descendant_id in thread
            .codex
            .session
            .services
            .agent_control
            .list_live_agent_subtree_thread_ids(thread_id)
            .await?
        {
            if seen_thread_ids.insert(descendant_id) {
                subtree_thread_ids.push(descendant_id);
            }
        }

        Ok(subtree_thread_ids)
    }

    // ───────────────────────────────────────────────────────────────
    // 启动 / 恢复 / fork 线程的入口
    // ───────────────────────────────────────────────────────────────
    // 这几个 public 方法都是「便利封装」，最终都汇到
    // `ThreadManagerState::spawn_thread_with_source`。
    //
    // 注意：所有这里的 Box::pin(...) 不是无用的——它把巨大的 spawn future
    // 装箱，避免内联到每个调用方的 async state machine，否则栈占用爆炸。

    pub async fn start_thread(&self, config: Config) -> CodexResult<NewThread> {
        // Box delegated thread-spawn futures so these convenience wrappers do
        // not inline the full spawn path into every caller's async state.
        Box::pin(self.start_thread_with_tools(
            config,
            Vec::new(),
            /*persist_extended_history*/ false,
        ))
        .await
    }

    pub async fn start_thread_with_tools(
        &self,
        config: Config,
        dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
        persist_extended_history: bool,
    ) -> CodexResult<NewThread> {
        let environments = default_thread_environment_selections(
            self.state.environment_manager.as_ref(),
            &config.cwd,
        );
        Box::pin(self.start_thread_with_options(StartThreadOptions {
            config,
            initial_history: InitialHistory::New,
            session_source: None,
            thread_source: None,
            dynamic_tools,
            persist_extended_history,
            metrics_service_name: None,
            parent_trace: None,
            environments,
        }))
        .await
    }

    pub async fn start_thread_with_options(
        &self,
        options: StartThreadOptions,
    ) -> CodexResult<NewThread> {
        let session_source = options
            .session_source
            .unwrap_or_else(|| self.state.session_source.clone());
        // thread_source 优先用调用方传入，否则从 initial_history 推断
        // （resume 历史里通常带有原 thread_source）。
        let thread_source = options
            .thread_source
            .or_else(|| options.initial_history.get_resumed_thread_source());
        Box::pin(self.state.spawn_thread_with_source(
            options.config,
            options.initial_history,
            Arc::clone(&self.state.auth_manager),
            self.agent_control(),
            session_source,
            thread_source,
            options.dynamic_tools,
            options.persist_extended_history,
            options.metrics_service_name,
            /*inherited_shell_snapshot*/ None,
            /*inherited_exec_policy*/ None,
            options.parent_trace,
            options.environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    // TODO(jif) merge with fork_agent
    /// Spawn a subagent by forking persisted history from `forked_from_thread_id`.
    /// 从指定 thread 派生子 agent：先刷盘父线程 rollout、再读完整持久化历史、
    /// 按 `ForkSnapshot::Interrupted` 截断、然后作为新线程的 initial_history 启动。
    pub async fn spawn_subagent(
        &self,
        forked_from_thread_id: ThreadId,
        mut options: StartThreadOptions,
    ) -> CodexResult<NewThread> {
        let fork_source = self.get_thread(forked_from_thread_id).await?;
        // Persist queued rollout updates before reading the fork snapshot.
        // 读 fork snapshot 前必须刷盘：rollout 在内存里有排队的写操作，不刷
        // 就读会得到不一致的历史。
        fork_source.ensure_rollout_materialized().await;
        fork_source.flush_rollout().await?;
        let stored_thread = fork_source
            .read_thread(
                /*include_archived*/ true, /*include_history*/ true,
            )
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to read subagent fork source {forked_from_thread_id}: {err}"
                ))
            })?;
        let history = stored_thread_to_initial_history(stored_thread, fork_source.rollout_path())?;
        options.initial_history = fork_history_from_snapshot(
            ForkSnapshot::Interrupted,
            history,
            InterruptedTurnHistoryMarker::from_config(&options.config),
        );
        self.start_thread_with_options(options).await
    }

    pub async fn resume_thread_from_rollout(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
        parent_trace: Option<W3cTraceContext>,
    ) -> CodexResult<NewThread> {
        let initial_history = self.initial_history_from_rollout_path(rollout_path).await?;
        Box::pin(self.resume_thread_with_history(
            config,
            initial_history,
            auth_manager,
            /*persist_extended_history*/ false,
            parent_trace,
        ))
        .await
    }

    pub async fn resume_thread_with_history(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        persist_extended_history: bool,
        parent_trace: Option<W3cTraceContext>,
    ) -> CodexResult<NewThread> {
        let environments = default_thread_environment_selections(
            self.state.environment_manager.as_ref(),
            &config.cwd,
        );
        let thread_source = initial_history.get_resumed_thread_source();
        Box::pin(self.state.spawn_thread(
            config,
            initial_history,
            auth_manager,
            self.agent_control(),
            thread_source,
            Vec::new(),
            persist_extended_history,
            /*metrics_service_name*/ None,
            parent_trace,
            environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    pub(crate) async fn start_thread_with_user_shell_override_for_tests(
        &self,
        config: Config,
        user_shell_override: crate::shell::Shell,
    ) -> CodexResult<NewThread> {
        let environments = default_thread_environment_selections(
            self.state.environment_manager.as_ref(),
            &config.cwd,
        );
        Box::pin(self.state.spawn_thread(
            config,
            InitialHistory::New,
            Arc::clone(&self.state.auth_manager),
            self.agent_control(),
            /*thread_source*/ None,
            Vec::new(),
            /*persist_extended_history*/ false,
            /*metrics_service_name*/ None,
            /*parent_trace*/ None,
            environments,
            /*user_shell_override*/ Some(user_shell_override),
        ))
        .await
    }

    pub(crate) async fn resume_thread_from_rollout_with_user_shell_override_for_tests(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
        user_shell_override: crate::shell::Shell,
    ) -> CodexResult<NewThread> {
        let initial_history = self.initial_history_from_rollout_path(rollout_path).await?;
        let environments = default_thread_environment_selections(
            self.state.environment_manager.as_ref(),
            &config.cwd,
        );
        let thread_source = initial_history.get_resumed_thread_source();
        Box::pin(self.state.spawn_thread(
            config,
            initial_history,
            auth_manager,
            self.agent_control(),
            thread_source,
            Vec::new(),
            /*persist_extended_history*/ false,
            /*metrics_service_name*/ None,
            /*parent_trace*/ None,
            environments,
            /*user_shell_override*/ Some(user_shell_override),
        ))
        .await
    }

    // ───────────────────────────────────────────────────────────────
    // 移除 / 关闭线程
    // ───────────────────────────────────────────────────────────────

    /// Removes the thread from the manager's internal map, though the thread is stored
    /// as `Arc<CodexThread>`, it is possible that other references to it exist elsewhere.
    /// Returns the thread if the thread was found and removed.
    /// 从 manager 的内存表里移除线程。注意：thread 是 `Arc<CodexThread>`，
    /// 其他地方持有的引用不会失效；本方法只是"从注册表里登记取消"。
    pub async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        self.state.threads.write().await.remove(thread_id)
    }

    /// Tries to shut down all tracked threads concurrently within the provided timeout.
    /// Threads that complete shutdown are removed from the manager; incomplete shutdowns
    /// remain tracked so callers can retry or inspect them later.
    /// 在 timeout 内并发关闭所有线程。
    ///
    /// 结果分三桶：
    ///   - completed   → 已干净退出，从 manager 移除
    ///   - submit_failed → submit Shutdown 这步失败，仍保留在 manager 里
    ///   - timed_out   → 超时未结束，仍保留在 manager 里（调用方可重试）
    ///
    /// 三个列表按 thread_id 字符串排序后返回，便于测试做确定性断言。
    pub async fn shutdown_all_threads_bounded(&self, timeout: Duration) -> ThreadShutdownReport {
        // ── Step 1：快照线程列表（只持读锁，避免后续 await 时阻塞写入者）──
        let threads = {
            let threads = self.state.threads.read().await;
            threads
                .iter()
                .map(|(thread_id, thread)| (*thread_id, Arc::clone(thread)))
                .collect::<Vec<_>>()
        };

        // ── Step 2：并发 shutdown，记录每个线程的结果 ────────────────────
        let mut shutdowns = threads
            .into_iter()
            .map(|(thread_id, thread)| async move {
                let outcome = match tokio::time::timeout(timeout, thread.shutdown_and_wait()).await
                {
                    Ok(Ok(())) => ShutdownOutcome::Complete,
                    Ok(Err(_)) => ShutdownOutcome::SubmitFailed,
                    Err(_) => ShutdownOutcome::TimedOut,
                };
                (thread_id, outcome)
            })
            .collect::<FuturesUnordered<_>>();
        let mut report = ThreadShutdownReport::default();

        while let Some((thread_id, outcome)) = shutdowns.next().await {
            match outcome {
                ShutdownOutcome::Complete => report.completed.push(thread_id),
                ShutdownOutcome::SubmitFailed => report.submit_failed.push(thread_id),
                ShutdownOutcome::TimedOut => report.timed_out.push(thread_id),
            }
        }

        // ── Step 3：把"已完成"的线程从注册表里移除（写锁，最小作用域）──
        let mut tracked_threads = self.state.threads.write().await;
        for thread_id in &report.completed {
            tracked_threads.remove(thread_id);
        }

        report
            .completed
            .sort_by_key(std::string::ToString::to_string);
        report
            .submit_failed
            .sort_by_key(std::string::ToString::to_string);
        report
            .timed_out
            .sort_by_key(std::string::ToString::to_string);
        report
    }

    // ───────────────────────────────────────────────────────────────
    // Fork 线程
    // ───────────────────────────────────────────────────────────────

    /// Fork an existing thread by snapshotting rollout history according to
    /// `snapshot` and starting a new thread with identical configuration
    /// (unless overridden by the caller's `config`). The new thread will have
    /// a fresh id.
    /// 按 `snapshot` 截断 rollout 历史后，启动一个新线程作为 fork。
    /// 新线程拥有新 thread_id，但默认继承原线程配置（可被 `config` 覆盖）。
    ///
    /// 泛型 `S: Into<ForkSnapshot>` 是为了兼容老接口 `fork_thread(usize, ...)`
    /// —— `From<usize>` 实现把 usize 映射到 `TruncateBeforeNthUserMessage`。
    pub async fn fork_thread<S>(
        &self,
        snapshot: S,
        config: Config,
        path: PathBuf,
        thread_source: Option<ThreadSource>,
        persist_extended_history: bool,
        parent_trace: Option<W3cTraceContext>,
    ) -> CodexResult<NewThread>
    where
        S: Into<ForkSnapshot>,
    {
        let snapshot = snapshot.into();
        let history = self.initial_history_from_rollout_path(path).await?;
        self.fork_thread_from_history(
            snapshot,
            config,
            history,
            thread_source,
            persist_extended_history,
            parent_trace,
        )
        .await
    }

    /// 把 rollout 路径解析为 `InitialHistory::Resumed`：从 store 读完整线程
    /// （含历史），再投影为 InitialHistory。
    async fn initial_history_from_rollout_path(
        &self,
        rollout_path: PathBuf,
    ) -> CodexResult<InitialHistory> {
        let requested_rollout_path = rollout_path.clone();
        let stored_thread = self
            .state
            .thread_store
            .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                rollout_path,
                include_archived: true,
                include_history: true,
            })
            .await
            .map_err(thread_store_rollout_read_error)?;
        stored_thread_to_initial_history(stored_thread, Some(requested_rollout_path))
    }

    /// Fork an existing thread from already-loaded store history.
    /// 从已加载的 store 历史 fork —— 节省一次 store IO，适用于调用方已经
    /// 拿到 history 的场景（如 spawn_subagent）。
    pub async fn fork_thread_from_history<S>(
        &self,
        snapshot: S,
        config: Config,
        history: InitialHistory,
        thread_source: Option<ThreadSource>,
        persist_extended_history: bool,
        parent_trace: Option<W3cTraceContext>,
    ) -> CodexResult<NewThread>
    where
        S: Into<ForkSnapshot>,
    {
        self.fork_thread_with_initial_history(
            snapshot.into(),
            config,
            history,
            thread_source,
            persist_extended_history,
            parent_trace,
        )
        .await
    }

    async fn fork_thread_with_initial_history(
        &self,
        snapshot: ForkSnapshot,
        config: Config,
        history: InitialHistory,
        thread_source: Option<ThreadSource>,
        persist_extended_history: bool,
        parent_trace: Option<W3cTraceContext>,
    ) -> CodexResult<NewThread> {
        let interrupted_marker = InterruptedTurnHistoryMarker::from_config(&config);
        let history = fork_history_from_snapshot(snapshot, history, interrupted_marker);
        let environments = default_thread_environment_selections(
            self.state.environment_manager.as_ref(),
            &config.cwd,
        );
        Box::pin(self.state.spawn_thread(
            config,
            history,
            Arc::clone(&self.state.auth_manager),
            self.agent_control(),
            thread_source,
            Vec::new(),
            persist_extended_history,
            /*metrics_service_name*/ None,
            parent_trace,
            environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    /// 把 `ThreadManagerState` 弱引用包成 `AgentControl`，子 agent 通过它
    /// 反向调用 manager 而不形成强循环引用。
    pub(crate) fn agent_control(&self) -> AgentControl {
        AgentControl::new(Arc::downgrade(&self.state))
    }

    #[cfg(test)]
    pub(crate) fn captured_ops(&self) -> Vec<(ThreadId, Op)> {
        self.state
            .ops_log
            .as_ref()
            .and_then(|ops_log| ops_log.lock().ok().map(|log| log.clone()))
            .unwrap_or_default()
    }
}

// ═══════════════════════════════════════════════════════════════
// ThreadManagerState  ·  内部状态实现
// ═══════════════════════════════════════════════════════════════

impl ThreadManagerState {
    pub(crate) fn state_db(&self) -> Option<StateDbHandle> {
        self.state_db.clone()
    }

    /// 列出所有「对外可见」的线程 id —— 过滤掉 internal session_source
    /// 的线程（如内部辅助 agent），它们对调用方不应可见。
    pub(crate) async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.threads
            .read()
            .await
            .iter()
            .filter_map(|(thread_id, thread)| {
                (!thread.session_source.is_internal()).then_some(*thread_id)
            })
            .collect()
    }

    /// Fetch a thread by ID or return ThreadNotFound.
    /// 内部 / external 线程在数据上同表，但取线程时要过滤掉 internal——对
    /// 外部调用方而言它们应表现为「不存在」而非「拒绝访问」。
    pub(crate) async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        let threads = self.threads.read().await;
        match threads.get(&thread_id) {
            Some(thread) if !thread.session_source.is_internal() => Ok(thread.clone()),
            Some(_) | None => Err(CodexErr::ThreadNotFound(thread_id)),
        }
    }

    /// 直接从 store 读 cold thread；错误类型经过转换：
    ///   - ThreadNotFound 透传；
    ///   - "no rollout found for thread id" 这种特定 InvalidRequest 也判为
    ///     ThreadNotFound（store 实现细节，不该泄漏到上层）；
    ///   - 其余包装为 Fatal。
    pub(crate) async fn read_stored_thread(
        &self,
        params: ReadThreadParams,
    ) -> CodexResult<StoredThread> {
        let thread_id = params.thread_id;
        self.thread_store
            .read_thread(params)
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                ThreadStoreError::InvalidRequest { message } => {
                    if message.starts_with("no rollout found for thread id ") {
                        CodexErr::ThreadNotFound(thread_id)
                    } else {
                        CodexErr::Fatal(format!(
                            "failed to read stored thread {thread_id}: invalid thread-store request: {message}"
                        ))
                    }
                }
                err => CodexErr::Fatal(format!("failed to read stored thread {thread_id}: {err}")),
            })
    }

    /// Send an operation to a thread by ID.
    /// 给指定线程投递 Op；测试模式下顺便把 (thread_id, op) 记到 ops_log。
    pub(crate) async fn send_op(&self, thread_id: ThreadId, op: Op) -> CodexResult<String> {
        let thread = self.get_thread(thread_id).await?;
        if let Some(ops_log) = &self.ops_log
            && let Ok(mut log) = ops_log.lock()
        {
            log.push((thread_id, op.clone()));
        }
        thread.submit(op).await
    }

    #[cfg(test)]
    /// Append a prebuilt message to a thread by ID outside the normal user-input path.
    pub(crate) async fn append_message(
        &self,
        thread_id: ThreadId,
        message: ResponseItem,
    ) -> CodexResult<String> {
        let thread = self.get_thread(thread_id).await?;
        thread.append_message(message).await
    }

    pub(crate) async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        self.threads.write().await.remove(thread_id)
    }

    // ───────────────────────────────────────────────────────────────
    // Spawn 主路径（多个便利封装最终都汇聚到 spawn_thread_with_source）
    // ───────────────────────────────────────────────────────────────

    pub(crate) async fn spawn_new_thread(
        &self,
        config: Config,
        agent_control: AgentControl,
    ) -> CodexResult<NewThread> {
        Box::pin(self.spawn_new_thread_with_source(
            config,
            agent_control,
            self.session_source.clone(),
            /*thread_source*/ None,
            /*persist_extended_history*/ false,
            /*metrics_service_name*/ None,
            /*inherited_shell_snapshot*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        ))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_new_thread_with_source(
        &self,
        config: Config,
        agent_control: AgentControl,
        session_source: SessionSource,
        thread_source: Option<ThreadSource>,
        persist_extended_history: bool,
        metrics_service_name: Option<String>,
        inherited_shell_snapshot: Option<Arc<ShellSnapshot>>,
        inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
        environments: Option<Vec<TurnEnvironmentSelection>>,
    ) -> CodexResult<NewThread> {
        let environments = environments.unwrap_or_else(|| {
            default_thread_environment_selections(self.environment_manager.as_ref(), &config.cwd)
        });
        Box::pin(self.spawn_thread_with_source(
            config,
            InitialHistory::New,
            Arc::clone(&self.auth_manager),
            agent_control,
            session_source,
            thread_source,
            Vec::new(),
            persist_extended_history,
            metrics_service_name,
            inherited_shell_snapshot,
            inherited_exec_policy,
            /*parent_trace*/ None,
            environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    pub(crate) async fn resume_thread_with_history_with_source(
        &self,
        options: ResumeThreadWithHistoryOptions,
    ) -> CodexResult<NewThread> {
        let ResumeThreadWithHistoryOptions {
            config,
            initial_history,
            agent_control,
            session_source,
            inherited_shell_snapshot,
            inherited_exec_policy,
        } = options;
        let environments =
            default_thread_environment_selections(self.environment_manager.as_ref(), &config.cwd);
        let thread_source = initial_history.get_resumed_thread_source();
        Box::pin(self.spawn_thread_with_source(
            config,
            initial_history,
            Arc::clone(&self.auth_manager),
            agent_control,
            session_source,
            thread_source,
            Vec::new(),
            /*persist_extended_history*/ false,
            /*metrics_service_name*/ None,
            inherited_shell_snapshot,
            inherited_exec_policy,
            /*parent_trace*/ None,
            environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn fork_thread_with_source(
        &self,
        config: Config,
        initial_history: InitialHistory,
        agent_control: AgentControl,
        session_source: SessionSource,
        thread_source: Option<ThreadSource>,
        persist_extended_history: bool,
        inherited_shell_snapshot: Option<Arc<ShellSnapshot>>,
        inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
        environments: Option<Vec<TurnEnvironmentSelection>>,
    ) -> CodexResult<NewThread> {
        let environments = environments.unwrap_or_else(|| {
            default_thread_environment_selections(self.environment_manager.as_ref(), &config.cwd)
        });
        Box::pin(self.spawn_thread_with_source(
            config,
            initial_history,
            Arc::clone(&self.auth_manager),
            agent_control,
            session_source,
            thread_source,
            Vec::new(),
            persist_extended_history,
            /*metrics_service_name*/ None,
            inherited_shell_snapshot,
            inherited_exec_policy,
            /*parent_trace*/ None,
            environments,
            /*user_shell_override*/ None,
        ))
        .await
    }

    /// Spawn a new thread with optional history and register it with the manager.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_thread(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        agent_control: AgentControl,
        thread_source: Option<ThreadSource>,
        dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
        persist_extended_history: bool,
        metrics_service_name: Option<String>,
        parent_trace: Option<W3cTraceContext>,
        environments: Vec<TurnEnvironmentSelection>,
        user_shell_override: Option<crate::shell::Shell>,
    ) -> CodexResult<NewThread> {
        Box::pin(self.spawn_thread_with_source(
            config,
            initial_history,
            auth_manager,
            agent_control,
            self.session_source.clone(),
            thread_source,
            dynamic_tools,
            persist_extended_history,
            metrics_service_name,
            /*inherited_shell_snapshot*/ None,
            /*inherited_exec_policy*/ None,
            parent_trace,
            environments,
            user_shell_override,
        ))
        .await
    }

    /// spawn 主路径 —— 所有公开入口最终都汇聚到这里。
    ///
    /// 关键步骤：
    ///   1. 若是 resumed thread 且同 thread_id 已运行：复用已有线程（幂等）；
    ///      若同 id 已死，先从表中移除再继续；
    ///   2. 解析 environments；准备 parent rollout trace（仅 sub-agent 场景）；
    ///   3. 调 `Codex::spawn` 拉起 Session（这是真正的重活）；
    ///   4. `finalize_thread_spawn`：等首事件、登记表、构造 `CodexThread`；
    ///   5. 若是 resume，发出 resume 生命周期事件并 apply goal runtime 效果。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_thread_with_source(
        &self,
        config: Config,
        initial_history: InitialHistory,
        auth_manager: Arc<AuthManager>,
        agent_control: AgentControl,
        session_source: SessionSource,
        thread_source: Option<ThreadSource>,
        dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
        persist_extended_history: bool,
        metrics_service_name: Option<String>,
        inherited_shell_snapshot: Option<Arc<ShellSnapshot>>,
        inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
        parent_trace: Option<W3cTraceContext>,
        environments: Vec<TurnEnvironmentSelection>,
        user_shell_override: Option<crate::shell::Shell>,
    ) -> CodexResult<NewThread> {
        let is_resumed_thread = matches!(&initial_history, InitialHistory::Resumed(_));
        // ── Step 1：resume 幂等性检查 ───────────────────────────────────
        // 若调用方 resume 一个已经在内存中运行的线程，复用而非新开；
        // 若同 id 线程已死，先从表中移除再继续 spawn 流程。
        if let InitialHistory::Resumed(resumed) = &initial_history {
            let mut threads = self.threads.write().await;
            if let Some(thread) = threads.get(&resumed.conversation_id).cloned() {
                if thread.is_running() {
                    // rollout 路径不一致 → 拒绝（避免静默替换历史）。
                    if let Some(requested_rollout_path) = resumed.rollout_path.as_deref()
                        && thread.rollout_path().as_deref() != Some(requested_rollout_path)
                    {
                        return Err(CodexErr::InvalidRequest(format!(
                            "thread {} is already running with a different rollout path",
                            resumed.conversation_id
                        )));
                    }
                    return Ok(NewThread {
                        thread_id: resumed.conversation_id,
                        session_configured: thread.session_configured(),
                        thread,
                    });
                }
                threads.remove(&resumed.conversation_id);
            }
        }
        let environment_selections =
            resolve_environment_selections(self.environment_manager.as_ref(), &environments)?;
        let parent_rollout_thread_trace = self
            .parent_rollout_thread_trace_for_source(&session_source, &initial_history)
            .await;
        let tracked_session_source = session_source.clone();
        // ── Step 2：真正拉起 Session ────────────────────────────────────
        let CodexSpawnOk {
            codex, thread_id, ..
        } = Codex::spawn(CodexSpawnArgs {
            config,
            installation_id: self.installation_id.clone(),
            auth_manager,
            models_manager: Arc::clone(&self.models_manager),
            environment_manager: Arc::clone(&self.environment_manager),
            skills_manager: Arc::clone(&self.skills_manager),
            plugins_manager: Arc::clone(&self.plugins_manager),
            mcp_manager: Arc::clone(&self.mcp_manager),
            extensions: Arc::clone(&self.extensions),
            conversation_history: initial_history,
            session_source,
            thread_source,
            agent_control,
            dynamic_tools,
            persist_extended_history,
            metrics_service_name,
            inherited_shell_snapshot,
            inherited_exec_policy,
            parent_rollout_thread_trace,
            user_shell_override,
            parent_trace,
            environment_selections,
            analytics_events_client: self.analytics_events_client.clone(),
            thread_store: Arc::clone(&self.thread_store),
            attestation_provider: self.attestation_provider.clone(),
        })
        .await?;
        // ── Step 3：等首事件 + 登记表 + 构造 `CodexThread` ───────────────
        let new_thread = self
            .finalize_thread_spawn(codex, thread_id, tracked_session_source)
            .await?;
        // ── Step 4：resume 场景的善后处理 ───────────────────────────────
        if is_resumed_thread {
            new_thread.thread.emit_thread_resume_lifecycle().await;
            if let Err(err) = new_thread.thread.apply_goal_resume_runtime_effects().await {
                // resume 时 goal runtime 失败不致命，只 warn 不阻断恢复流程。
                warn!("failed to apply goal resume runtime effects: {err}");
            }
        }
        Ok(new_thread)
    }

    /// Codex spawn 完成后的收尾：
    ///   1. 取下一个事件，要求必须是 INITIAL_SUBMIT_ID 的 `SessionConfigured`；
    ///   2. 在写锁下检查 thread_id 是否已被占用：
    ///      - 空位 → 插入新 `CodexThread`，返回 NewThread；
    ///      - 已占 → 关闭新创建的 codex 释放资源、返回 InvalidRequest。
    async fn finalize_thread_spawn(
        &self,
        codex: Codex,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<NewThread> {
        let event = codex.next_event().await?;
        let session_configured = match event {
            Event {
                id,
                msg: EventMsg::SessionConfigured(session_configured),
            } if id == INITIAL_SUBMIT_ID => session_configured,
            _ => {
                // 协议契约：spawn 后的首事件必须是 SessionConfigured。
                // 否则上游逻辑（如等首事件再发欢迎语）会全错位。
                return Err(CodexErr::SessionConfiguredNotFirstEvent);
            }
        };

        {
            let mut threads = self.threads.write().await;
            if let std::collections::hash_map::Entry::Vacant(e) = threads.entry(thread_id) {
                let thread = Arc::new(CodexThread::new(
                    codex,
                    session_configured.clone(),
                    session_configured.rollout_path.clone(),
                    session_source,
                ));
                e.insert(thread.clone());
                return Ok(NewThread {
                    thread_id,
                    thread,
                    session_configured,
                });
            }
        }

        // 已占位：把刚刚 spawn 的 codex 干净地关掉，避免资源泄漏。
        if let Err(err) = codex.shutdown_and_wait().await {
            warn!("failed to shut down duplicate thread {thread_id}: {err}");
        }
        Err(CodexErr::InvalidRequest(format!(
            "thread {thread_id} is already running"
        )))
    }

    pub(crate) fn notify_thread_created(&self, thread_id: ThreadId) {
        // 无订阅者时 send 返回 Err 是正常的，忽略即可。
        let _ = self.thread_created_tx.send(thread_id);
    }

    /// 计算 sub-agent 场景下，新线程要继承的「父线程 rollout trace context」。
    ///
    /// 仅在 `SessionSource::SubAgent::ThreadSpawn` 且非 resume 时返回有效 trace；
    /// resume 路径下父线程已写过 `ThreadStarted` 事件，再继承会重复写、导致
    /// rollout 不可重放。父线程查找失败也只回 disabled trace —— tracing 是
    /// 诊断信息，不该阻塞子线程创建。
    async fn parent_rollout_thread_trace_for_source(
        &self,
        session_source: &SessionSource,
        initial_history: &InitialHistory,
    ) -> codex_rollout_trace::ThreadTraceContext {
        // A fresh v2 child belongs to the same rollout tree as its parent, so
        // session startup derives its child trace from the parent's thread
        // context. Resumed children already have a prior `ThreadStarted` event
        // for this thread id; deriving a child trace during resume would write
        // that start event again and make the bundle unreplayable.
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = session_source
        else {
            return codex_rollout_trace::ThreadTraceContext::disabled();
        };
        if matches!(initial_history, InitialHistory::Resumed(_)) {
            return codex_rollout_trace::ThreadTraceContext::disabled();
        }
        // Parent lookup can fail if the parent was closed or released between
        // spawn preparation and session construction. Tracing is diagnostic, so
        // that race should not block child creation; the child simply starts
        // without a parent rollout trace.
        self.get_thread(*parent_thread_id)
            .await
            .ok()
            .map(|thread| thread.codex.session.services.rollout_thread_trace.clone())
            .unwrap_or_else(codex_rollout_trace::ThreadTraceContext::disabled)
    }
}

// ═══════════════════════════════════════════════════════════════
// 模块级辅助函数
// ═══════════════════════════════════════════════════════════════

/// 把 `StoredThread` 转换为 `InitialHistory::Resumed`。
/// 若 `stored_thread.history` 缺失（理论上不该发生）则返回 Fatal。
/// rollout_path 优先用调用方提供的值，缺省回退到 stored_thread 自身记录的。
fn stored_thread_to_initial_history(
    stored_thread: StoredThread,
    rollout_path: Option<PathBuf>,
) -> CodexResult<InitialHistory> {
    let thread_id = stored_thread.thread_id;
    let history = stored_thread.history.ok_or_else(|| {
        CodexErr::Fatal(format!(
            "thread {thread_id} did not include persisted history"
        ))
    })?;
    Ok(InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: history.items,
        rollout_path: rollout_path.or(stored_thread.rollout_path),
    }))
}

/// rollout 读路径上的统一错误转换：ThreadNotFound / InvalidRequest 透传，
/// 其他视为 Fatal。
fn thread_store_rollout_read_error(err: ThreadStoreError) -> CodexErr {
    match err {
        ThreadStoreError::ThreadNotFound { thread_id } => CodexErr::ThreadNotFound(thread_id),
        ThreadStoreError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        err => CodexErr::Fatal(format!("failed to read thread by rollout path: {err}")),
    }
}

/// metadata 更新路径上的统一错误转换：
/// `Unsupported` 转 `UnsupportedOperation`（如 InMemory store 不支持元数据更新）。
fn thread_store_metadata_update_error(thread_id: ThreadId, err: ThreadStoreError) -> CodexErr {
    match err {
        ThreadStoreError::ThreadNotFound { thread_id } => CodexErr::ThreadNotFound(thread_id),
        ThreadStoreError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        ThreadStoreError::Unsupported { operation } => CodexErr::UnsupportedOperation(format!(
            "thread metadata update is not supported by this store: {operation}"
        )),
        err => CodexErr::Fatal(format!(
            "failed to update thread metadata {thread_id}: {err}"
        )),
    }
}

// ───────────────────────────────────────────────────────────────
// Fork 历史截断算法
// ───────────────────────────────────────────────────────────────

/// Return a fork snapshot cut strictly before the nth user message (0-based).
///
/// Out-of-range values keep the full committed history at a turn boundary, but
/// when the source thread is currently mid-turn they fall back to cutting
/// before the active turn's opening boundary so the fork omits the unfinished
/// suffix entirely.
/// 返回一个「在第 n 条 user 消息严格之前」切的 fork snapshot（n 0 基）。
///
/// 越界处理：
///   - 源线程已在 Turn 边界 → 返回完整 committed history；
///   - 源线程 mid-turn → 退而切到当前活跃 Turn 开始边界之前，丢弃未完成尾部。
///
/// 返回 `InitialHistory::New`（rolled 为空）或 `InitialHistory::Forked(rolled)`。
fn truncate_before_nth_user_message(
    history: InitialHistory,
    n: usize,
    snapshot_state: &SnapshotTurnState,
) -> InitialHistory {
    let items: Vec<RolloutItem> = history.get_rollout_items();
    let user_positions = truncation::user_message_positions_in_rollout(&items);
    let rolled = if snapshot_state.ends_mid_turn && n >= user_positions.len() {
        // 越界 + mid-turn：以活跃 Turn 开始边界为切点；若没记到，退到最后
        // 一条 user 消息位置；都没有就保留全量。
        if let Some(cut_idx) = snapshot_state
            .active_turn_start_index
            .or_else(|| user_positions.last().copied())
        {
            items[..cut_idx].to_vec()
        } else {
            items
        }
    } else {
        truncation::truncate_rollout_before_nth_user_message_from_start(&items, n)
    };

    if rolled.is_empty() {
        InitialHistory::New
    } else {
        InitialHistory::Forked(rolled)
    }
}

/// 历史快照的「Turn 状态摘要」：是否止于 mid-turn、活跃 Turn 的 id 和起点 idx。
/// 由 `snapshot_turn_state` 计算，供 fork 算法决定如何切点。
#[derive(Debug, Eq, PartialEq)]
struct SnapshotTurnState {
    ends_mid_turn: bool,
    active_turn_id: Option<String>,
    active_turn_start_index: Option<usize>,
}

/// 扫描 history 的 rollout items，计算它在 Turn 维度上的「结束位置」。
///
/// 两条路径：
///   1. 有显式 Turn 生命周期事件（builder 报告 has_active_turn 且 active_turn_id
///      存在）→ 直接读 builder 的状态。
///   2. 没有显式 Turn 事件（如合成 fork/resume 历史）→ 看「最后一条 user
///      消息之后」是否出现 `TurnComplete` / `TurnAborted` 边界事件；没有就
///      判定为 mid-turn。
fn snapshot_turn_state(history: &InitialHistory) -> SnapshotTurnState {
    let rollout_items = history.get_rollout_items();
    let mut builder = ThreadHistoryBuilder::new();
    for item in &rollout_items {
        builder.handle_rollout_item(item);
    }
    let active_turn_id = builder.active_turn_id_if_explicit();
    if builder.has_active_turn() && active_turn_id.is_some() {
        let active_turn_snapshot = builder.active_turn_snapshot();
        // 活跃 Turn 已完成（status != InProgress）→ 视为非 mid-turn。
        if active_turn_snapshot
            .as_ref()
            .is_some_and(|turn| turn.status != TurnStatus::InProgress)
        {
            return SnapshotTurnState {
                ends_mid_turn: false,
                active_turn_id: None,
                active_turn_start_index: None,
            };
        }

        return SnapshotTurnState {
            ends_mid_turn: true,
            active_turn_id,
            active_turn_start_index: builder.active_turn_start_index(),
        };
    }

    let Some(last_user_position) = truncation::user_message_positions_in_rollout(&rollout_items)
        .last()
        .copied()
    else {
        // 完全没有 user 消息 → 不算 mid-turn。
        return SnapshotTurnState {
            ends_mid_turn: false,
            active_turn_id: None,
            active_turn_start_index: None,
        };
    };

    // Synthetic fork/resume histories can contain user/assistant response items
    // without explicit turn lifecycle events. If the persisted snapshot has no
    // terminating boundary after its last user message, treat it as mid-turn.
    // 合成历史（fork/resume）可能没有显式 Turn 生命周期事件——若最后一条
    // user 消息之后没有 TurnComplete / TurnAborted 边界，就当 mid-turn 处理。
    SnapshotTurnState {
        ends_mid_turn: !rollout_items[last_user_position + 1..].iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
            )
        }),
        active_turn_id: None,
        active_turn_start_index: None,
    }
}

/// fork 算法主入口：根据 `ForkSnapshot` 模式 + 历史 + 中断标记，产出最终的
/// fork InitialHistory。
fn fork_history_from_snapshot(
    snapshot: ForkSnapshot,
    history: InitialHistory,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let snapshot_state = snapshot_turn_state(&history);
    match snapshot {
        ForkSnapshot::TruncateBeforeNthUserMessage(nth_user_message) => {
            truncate_before_nth_user_message(history, nth_user_message, &snapshot_state)
        }
        ForkSnapshot::Interrupted => {
            // 先把外层包装统一成 Forked（Resumed 是为 resume 路径准备的，fork
            // 场景不应保留 ResumedHistory.conversation_id 等元信息）。
            let history = match history {
                InitialHistory::New => InitialHistory::New,
                InitialHistory::Cleared => InitialHistory::Cleared,
                InitialHistory::Forked(history) => InitialHistory::Forked(history),
                InitialHistory::Resumed(resumed) => InitialHistory::Forked(resumed.history),
            };
            if snapshot_state.ends_mid_turn {
                append_interrupted_boundary(
                    history,
                    snapshot_state.active_turn_id,
                    interrupted_marker,
                )
            } else {
                history
            }
        }
    }
}

/// Append the same persisted interrupt boundary used by the live interrupt path
/// to an existing fork snapshot after the source thread has been confirmed to
/// be mid-turn.
/// 在已确认源线程为 mid-turn 的 fork snapshot 上追加和实时 interrupt 路径
/// 一致的中断边界（先 marker、再 TurnAborted）。
///
/// 这样 fork 出的历史在中断点的形状与真实中断完全一致，下游做 replay /
/// 训练数据收集时无需区分两种来源。
fn append_interrupted_boundary(
    history: InitialHistory,
    turn_id: Option<String>,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let aborted_event = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id,
        reason: TurnAbortReason::Interrupted,
        completed_at: None,
        duration_ms: None,
    }));

    match history {
        InitialHistory::New | InitialHistory::Cleared => {
            let mut history = Vec::new();
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Forked(mut history) => {
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Resumed(mut resumed) => {
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                resumed.history.push(RolloutItem::ResponseItem(marker));
            }
            resumed.history.push(aborted_event);
            InitialHistory::Forked(resumed.history)
        }
    }
}

#[cfg(test)]
#[path = "thread_manager_tests.rs"]
mod tests;
