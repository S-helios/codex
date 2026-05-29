#![deny(clippy::print_stdout, clippy::print_stderr)]
//! 【文件职责】app-server 的「进程/服务总入口与编排骨架」。负责加载配置、初始化
//! 遥测/日志、按 transport（stdio / unix / ws / 远程控制）拉起接入点，并启动两个长驻
//! tokio 任务——「处理器循环」与「出站路由循环」，把整套服务的生命周期（启动、优雅
//! 重启 drain、关闭）串起来。
//!
//! 【架构位置】
//!   层级：前端集成层（app-server）· 顶层装配
//!   上游：CLI（`codex app-server` / daemon）通过 `run_main*` 进入
//!   下游：`transport`（接入与字节流）、`MessageProcessor`（请求分发）、`OutgoingMessageSender`（出站）
//!
//! 【两循环模型 · 本文件的核心设计】见 `OutboundControlEvent` 文档：
//!   · 处理器循环：消费 `TransportEvent`，做 JSON-RPC 分发、连接初始化握手、关闭信号 drain；
//!   · 出站循环：消费 `OutgoingEnvelope`，执行「可能很慢」的逐连接写。
//!   两者经由 `OutboundControlEvent`（连接开/关/全断）协调，不直接共享可变连接状态。
//!
//! 【阅读建议】主入口是 `run_main_with_transport_options`（巨型装配函数）。读法：
//!   ① 前半段——配置加载 / 遥测 / 状态库 / 告警收集；
//!   ② 中段——按 transport 启动 acceptor、启动远程控制；
//!   ③ 后半段——`tokio::spawn` 出站循环 + 处理器循环（含 `tokio::select!` 主循环）。
//!   `ShutdownState`（优雅重启状态机）与 `OutboundControlEvent` 是理解生命周期的两把钥匙；
//!   顶部一批 `config_*_warning` / `*_location` 是配置告警的格式化 helper，可后看。
//!   对照 learn_docs/5_前端_集成_协议/20_app_server_layer.md §6、§7。

use codex_arg0::Arg0DispatchPaths;
use codex_config::ConfigLayerStackOrdering;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::RemoteThreadConfigLoader;
use codex_config::ThreadConfigLoader;
use codex_core::config::Config;
use codex_core::resolve_installation_id;
use codex_login::AuthManager;
use codex_utils_cli::CliConfigOverrides;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;

use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::message_processor::MessageProcessor;
use crate::message_processor::MessageProcessorArgs;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::QueuedOutgoingMessage;
use crate::transport::CHANNEL_CAPACITY;
use crate::transport::ConnectionState;
use crate::transport::OutboundConnectionState;
use crate::transport::RemoteControlStartConfig;
use crate::transport::TransportEvent;
use crate::transport::acquire_app_server_startup_lock;
use crate::transport::app_server_startup_lock_path;
use crate::transport::auth::policy_from_settings;
use crate::transport::prepare_control_socket_path;
use crate::transport::route_outgoing_envelope;
use crate::transport::start_control_socket_acceptor;
use crate::transport::start_remote_control;
use crate::transport::start_stdio_connection;
use crate::transport::start_websocket_acceptor;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TextPosition as AppTextPosition;
use codex_app_server_protocol::TextRange as AppTextRange;
use codex_config::ConfigLoadError;
use codex_config::TextRange as CoreTextRange;
use codex_core::ExecPolicyError;
use codex_core::check_execpolicy_for_warnings;
use codex_core::config::find_codex_home;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_rollout::state_db as rollout_state_db;
use codex_state::log_db;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;

mod analytics_utils;
mod app_server_tracing;
mod attestation;
mod bespoke_event_handling;
mod command_exec;
mod config;
mod config_manager;
mod config_manager_service;
mod connection_rpc_gate;
mod dynamic_tools;
mod error_code;
mod extensions;
mod filters;
mod fs_watch;
mod fuzzy_file_search;
pub mod in_process;
mod mcp_refresh;
mod message_processor;
mod models;
mod outgoing_message;
mod request_processors;
mod request_serialization;
mod server_request_error;
mod skills_watcher;
mod thread_state;
mod thread_status;
mod transport;

pub use crate::error_code::INPUT_TOO_LARGE_ERROR_CODE;
pub use crate::error_code::INVALID_PARAMS_ERROR_CODE;
pub use crate::transport::AppServerTransport;
pub use crate::transport::app_server_control_socket_path;
pub use crate::transport::auth::AppServerWebsocketAuthArgs;
pub use crate::transport::auth::AppServerWebsocketAuthSettings;
pub use crate::transport::auth::WebsocketAuthCliMode;

const LOG_FORMAT_ENV_VAR: &str = "LOG_FORMAT";
const OTEL_SERVICE_NAME: &str = "codex-app-server";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogFormat {
    Default,
    Json,
}

type StderrLogLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

fn configured_thread_config_loader(config: &Config) -> Arc<dyn ThreadConfigLoader> {
    match config.experimental_thread_config_endpoint.as_deref() {
        Some(endpoint) => Arc::new(RemoteThreadConfigLoader::new(endpoint)),
        None => Arc::new(NoopThreadConfigLoader),
    }
}

/// Control-plane messages from the processor/transport side to the outbound router task.
///
/// `run_main_with_transport_options` uses two loops/tasks:
/// - processor loop: handles incoming JSON-RPC and request dispatch
/// - outbound loop: performs potentially slow writes to per-connection writers
///
/// `OutboundControlEvent` keeps those loops coordinated without sharing mutable
/// connection state directly. In particular, the outbound loop needs to know
/// when a connection opens/closes so it can route messages correctly.
/// 【控制面事件】处理器循环 → 出站循环的单向控制消息。出站循环维护 `connection_id → 写端`
/// 的路由表，但它本身不监听 transport，因此连接的开/关必须由处理器循环「告知」它——
/// 这就是本枚举存在的原因：用消息传递替代共享可变状态，避免两个任务争抢同一份连接表。
enum OutboundControlEvent {
    /// Register a new writer for an opened connection.
    /// 新连接打开：把它的写端 + 一组共享标志（是否已初始化 / 是否启用实验 API /
    /// 退订的通知方法集）登记进出站路由表。这些 `Arc` 标志在处理器侧更新、出站侧读取，
    /// 使出站循环能据此过滤/路由而无需回查处理器状态。
    Opened {
        connection_id: ConnectionId,
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        disconnect_sender: Option<CancellationToken>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    },
    /// Remove state for a closed/disconnected connection.
    /// 连接关闭：从出站路由表移除其状态。
    Closed { connection_id: ConnectionId },
    /// Disconnect all connection-oriented clients during graceful restart.
    /// 优雅重启：主动断开所有面向连接的客户端（出站侧触发各连接的 disconnect token）。
    DisconnectAll,
}

/// 优雅重启的状态机（只在多客户端 transport 且启用信号处理时生效）。
/// 语义：收到首个关闭信号后进入「drain」——停止接受新连接但允许现有 assistant turn 跑完；
/// 收到第二个可强制信号则 `forced`，不再等待直接收尾。`last_logged_running_turn_count`
/// 用于「turn 数变化才打日志」，避免 drain 期间刷屏。
#[derive(Default)]
struct ShutdownState {
    requested: bool,
    forced: bool,
    last_logged_running_turn_count: Option<usize>,
}

/// `update()` 的决策结果：`Noop` 继续 drain；`Finish` 立即收尾（停 acceptor、断全部连接）。
enum ShutdownAction {
    Noop,
    Finish,
}

/// 关闭信号的两种性质：`Forceable`（SIGTERM/Ctrl-C，可二次触发强制）；
/// `GracefulOnly`（SIGHUP，仅触发优雅 drain，二次也不强制）——仅 unix 有此区分。
#[derive(Clone, Copy)]
enum ShutdownSignal {
    Forceable,
    #[cfg(unix)]
    GracefulOnly,
}

async fn shutdown_signal() -> IoResult<ShutdownSignal> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;

        let mut term = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            ctrl_c_result = tokio::signal::ctrl_c() => ctrl_c_result.map(|_| ShutdownSignal::Forceable),
            _ = term.recv() => Ok(ShutdownSignal::Forceable),
            _ = hangup.recv() => Ok(ShutdownSignal::GracefulOnly),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map(|_| ShutdownSignal::Forceable)
    }
}

impl ShutdownState {
    fn requested(&self) -> bool {
        self.requested
    }

    fn forced(&self) -> bool {
        self.forced
    }

    /// 收到关闭信号时推进状态：首次置 `requested` 进入 drain；若 drain 期间又收到可强制信号
    /// 则升级为 `forced`（第二次 Ctrl-C 强杀语义）。仅 `Forceable` 能触发 `forced`，
    /// `GracefulOnly`（SIGHUP）重复到达不会强制。
    fn on_signal(
        &mut self,
        signal: ShutdownSignal,
        connection_count: usize,
        running_turn_count: usize,
    ) {
        if self.requested {
            if matches!(signal, ShutdownSignal::Forceable) {
                self.forced = true;
            }
            return;
        }

        self.requested = true;
        self.last_logged_running_turn_count = None;
        info!(
            "received shutdown signal; entering graceful restart drain (connections={}, runningAssistantTurns={}, requests still accepted until no assistant turns are running)",
            connection_count, running_turn_count,
        );
    }

    /// 在处理器主循环每轮开头被调用，根据「是否已请求关闭 / 是否强制 / 还有几个 turn 在跑」
    /// 决定是否收尾。收尾条件：已强制，或正在运行的 assistant turn 归零。否则返回 `Noop`
    /// 继续等待（turn 数变化时补一行日志）。
    fn update(&mut self, running_turn_count: usize, connection_count: usize) -> ShutdownAction {
        if !self.requested {
            return ShutdownAction::Noop;
        }

        if self.forced || running_turn_count == 0 {
            if self.forced {
                info!(
                    "received second shutdown signal; forcing restart with {running_turn_count} running assistant turn(s) and {connection_count} connection(s)"
                );
            } else {
                info!(
                    "shutdown signal restart: no assistant turns running; stopping acceptor and disconnecting {connection_count} connection(s)"
                );
            }
            return ShutdownAction::Finish;
        }

        if self.last_logged_running_turn_count != Some(running_turn_count) {
            info!(
                "shutdown signal restart: waiting for {running_turn_count} running assistant turn(s) to finish"
            );
            self.last_logged_running_turn_count = Some(running_turn_count);
        }

        ShutdownAction::Noop
    }
}

fn config_warning_from_error(
    summary: impl Into<String>,
    err: &std::io::Error,
) -> ConfigWarningNotification {
    let (path, range) = match config_error_location(err) {
        Some((path, range)) => (Some(path), Some(range)),
        None => (None, None),
    };
    ConfigWarningNotification {
        summary: summary.into(),
        details: Some(err.to_string()),
        path,
        range,
    }
}

fn config_error_location(err: &std::io::Error) -> Option<(String, AppTextRange)> {
    err.get_ref()
        .and_then(|err| err.downcast_ref::<ConfigLoadError>())
        .map(|err| {
            let config_error = err.config_error();
            (
                config_error.path.to_string_lossy().to_string(),
                app_text_range(&config_error.range),
            )
        })
}

fn exec_policy_warning_location(err: &ExecPolicyError) -> (Option<String>, Option<AppTextRange>) {
    match err {
        ExecPolicyError::ParsePolicy { path, source } => {
            if let Some(location) = source.location() {
                let range = AppTextRange {
                    start: AppTextPosition {
                        line: location.range.start.line,
                        column: location.range.start.column,
                    },
                    end: AppTextPosition {
                        line: location.range.end.line,
                        column: location.range.end.column,
                    },
                };
                return (Some(location.path), Some(range));
            }
            (Some(path.clone()), None)
        }
        _ => (None, None),
    }
}

fn app_text_range(range: &CoreTextRange) -> AppTextRange {
    AppTextRange {
        start: AppTextPosition {
            line: range.start.line,
            column: range.start.column,
        },
        end: AppTextPosition {
            line: range.end.line,
            column: range.end.column,
        },
    }
}

fn project_config_warning(config: &Config) -> Option<ConfigWarningNotification> {
    let mut disabled_folders = Vec::new();

    for layer in config.config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ true,
    ) {
        let ConfigLayerSource::Project { dot_codex_folder } = &layer.name else {
            continue;
        };
        let Some(disabled_reason) = &layer.disabled_reason else {
            continue;
        };
        disabled_folders.push((
            dot_codex_folder.as_path().display().to_string(),
            disabled_reason.clone(),
        ));
    }

    if disabled_folders.is_empty() {
        return None;
    }

    let mut message = concat!(
        "Project-local config, hooks, and exec policies are disabled in the following folders ",
        "until the project is trusted, but skills still load.\n",
    )
    .to_string();
    for (index, (folder, reason)) in disabled_folders.iter().enumerate() {
        let display_index = index + 1;
        message.push_str(&format!("    {display_index}. {folder}\n"));
        message.push_str(&format!("       {reason}\n"));
    }

    Some(ConfigWarningNotification {
        summary: message,
        details: None,
        path: None,
        range: None,
    })
}

impl LogFormat {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase) {
            Some(value) if value == "json" => Self::Json,
            _ => Self::Default,
        }
    }
}

fn log_format_from_env() -> LogFormat {
    let value = std::env::var(LOG_FORMAT_ENV_VAR).ok();
    LogFormat::from_env_value(value.as_deref())
}

/// 默认入口：以「stdio transport + VSCode 来源 + 默认运行时选项」启动 app-server，
/// 是最常见的 TUI/桌面端子进程接入路径。需要 unix/ws/远程控制等其它形态时，
/// 直接调用 `run_main_with_transport_options` 自定义参数。
pub async fn run_main(
    arg0_paths: Arg0DispatchPaths,
    cli_config_overrides: CliConfigOverrides,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    default_analytics_enabled: bool,
) -> IoResult<()> {
    run_main_with_transport_options(
        arg0_paths,
        cli_config_overrides,
        loader_overrides,
        strict_config,
        default_analytics_enabled,
        AppServerTransport::Stdio,
        SessionSource::VSCode,
        AppServerWebsocketAuthSettings::default(),
        AppServerRuntimeOptions::default(),
    )
    .await
}

/// 是否在启动时跑「插件预热任务」。嵌入式/测试场景可用 `Skip` 跳过，避免无谓的后台预热。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStartupTasks {
    Start,
    Skip,
}

/// app-server 运行时开关集合。三者分别控制：插件预热、是否启用远程控制接入、
/// 是否安装系统关闭信号处理器（进而决定能否走「优雅重启 drain」，见 `ShutdownState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppServerRuntimeOptions {
    pub plugin_startup_tasks: PluginStartupTasks,
    pub remote_control_enabled: bool,
    pub install_shutdown_signal_handler: bool,
}

impl Default for AppServerRuntimeOptions {
    fn default() -> Self {
        Self {
            plugin_startup_tasks: PluginStartupTasks::Start,
            remote_control_enabled: false,
            install_shutdown_signal_handler: true,
        }
    }
}

/// 【主入口·巨型装配函数】完整拉起一个 app-server 实例并阻塞到其退出。
///
/// 职责（按执行顺序）：建三条 mpsc 信道 → 解析配置/初始化遥测与状态库 → 收集配置告警
/// → 装好 tracing 订阅者 → 按 `transport` 启动 acceptor 与远程控制 → spawn「出站循环」
/// 与「处理器循环」→ 等两循环结束后做收尾（取消 token、回收 acceptor、关闭遥测）。
///
/// 副作用：安装全局 tracing 订阅者、启动多个长驻 tokio 任务、（unix socket 形态）持有
/// 启动锁与创建控制 socket。返回 `Ok(())` 表示服务正常退出（连接耗尽 / 收到关闭信号 /
/// 信道关闭）。`strict_config` 为真时配置错误会直接向上抛 `Err`，否则降级用默认配置并告警。
#[allow(clippy::too_many_arguments)]
pub async fn run_main_with_transport_options(
    arg0_paths: Arg0DispatchPaths,
    cli_config_overrides: CliConfigOverrides,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    default_analytics_enabled: bool,
    transport: AppServerTransport,
    session_source: SessionSource,
    auth: AppServerWebsocketAuthSettings,
    runtime_options: AppServerRuntimeOptions,
) -> IoResult<()> {
    // ── Step 1：建立三条核心 mpsc 信道 ──────────────────────────────────
    // transport_event：transport → 处理器循环（入站事件）
    // outgoing       ：发射器 → 出站循环（出站信封）
    // outbound_control：处理器循环 → 出站循环（连接开/关/全断的控制面）
    let (transport_event_tx, mut transport_event_rx) =
        mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(CHANNEL_CAPACITY);
    let (outbound_control_tx, mut outbound_control_rx) =
        mpsc::channel::<OutboundControlEvent>(CHANNEL_CAPACITY);

    // Parse CLI overrides once and derive the base Config eagerly so later
    // components do not need to work with raw TOML values.
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let codex_home = find_codex_home()?;
    let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
        arg0_paths.codex_self_exe.clone(),
        arg0_paths.codex_linux_sandbox_exe.clone(),
    )?;
    let environment_manager = if loader_overrides.ignore_user_config {
        EnvironmentManager::from_env(Some(local_runtime_paths)).await
    } else {
        EnvironmentManager::from_codex_home(codex_home.clone(), Some(local_runtime_paths)).await
    }
    .map(Arc::new)
    .map_err(std::io::Error::other)?;
    let config_manager = ConfigManager::new(
        codex_home.to_path_buf(),
        cli_kv_overrides.clone(),
        loader_overrides,
        strict_config,
        Default::default(),
        arg0_paths.clone(),
        Arc::new(NoopThreadConfigLoader),
    );
    match config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await
    {
        Ok(config) => {
            let discovered_thread_config_loader = configured_thread_config_loader(&config);
            config_manager
                .replace_thread_config_loader(Arc::clone(&discovered_thread_config_loader));
            let auth_manager =
                AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await;
            config_manager.replace_cloud_requirements_loader(auth_manager, config.chatgpt_base_url);
        }
        Err(err) => {
            warn!(error = %err, "Failed to preload config for cloud requirements");
            // TODO(gt): Make cloud requirements preload failures blocking once we can fail-closed.
        }
    };
    let mut config_warnings = Vec::new();
    let (mut config, should_run_personality_migration) = match config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await
    {
        Ok(config) => (config, true),
        Err(err) => {
            if strict_config {
                return Err(err);
            }

            let message = config_warning_from_error("Invalid configuration; using defaults.", &err);
            config_warnings.push(message);
            (
                config_manager.load_default_config().await.map_err(|e| {
                    std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!("error loading default config after config error: {e}"),
                    )
                })?,
                false,
            )
        }
    };

    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        Some(OTEL_SERVICE_NAME),
        default_analytics_enabled,
    )
    .map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("error loading otel config: {e}"),
        )
    })?;
    codex_core::otel_init::record_process_start(otel.as_ref(), OTEL_SERVICE_NAME);
    codex_core::otel_init::install_sqlite_telemetry(otel.as_ref(), OTEL_SERVICE_NAME);
    let unix_socket_startup_lock = match &transport {
        AppServerTransport::UnixSocket { socket_path } => {
            let startup_lock_path = app_server_startup_lock_path(&codex_home)?;
            let startup_lock = acquire_app_server_startup_lock(startup_lock_path).await?;
            prepare_control_socket_path(socket_path.as_path()).await?;
            Some(startup_lock)
        }
        _ => None,
    };
    let state_db = match rollout_state_db::try_init(&config).await {
        Ok(state_db) => Some(state_db),
        Err(err) => {
            return Err(std::io::Error::other(format!(
                "failed to initialize sqlite state runtime under {}: {err}",
                config.sqlite_home.display()
            )));
        }
    };

    if should_run_personality_migration {
        let effective_toml = config.config_layer_stack.effective_config();
        match effective_toml.try_into() {
            Ok(config_toml) => {
                match codex_core::personality_migration::maybe_migrate_personality(
                    &config.codex_home,
                    &config_toml,
                    state_db.clone(),
                )
                .await
                {
                    Ok(codex_core::personality_migration::PersonalityMigrationStatus::Applied) => {
                        config = config_manager
                            .load_latest_config(/*fallback_cwd*/ None)
                            .await
                            .map_err(|err| {
                                std::io::Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "error reloading config after personality migration: {err}"
                                    ),
                                )
                            })?;
                    }
                    Ok(
                        codex_core::personality_migration::PersonalityMigrationStatus::SkippedMarker
                        | codex_core::personality_migration::PersonalityMigrationStatus::SkippedExplicitPersonality
                        | codex_core::personality_migration::PersonalityMigrationStatus::SkippedNoSessions,
                    ) => {}
                    Err(err) => {
                        warn!(error = %err, "Failed to run personality migration");
                    }
                }
            }
            Err(err) => {
                warn!(error = %err, "Failed to deserialize config for personality migration");
            }
        }
    }

    if let Ok(Some(err)) = check_execpolicy_for_warnings(&config.config_layer_stack).await {
        let (path, range) = exec_policy_warning_location(&err);
        let message = ConfigWarningNotification {
            summary: "Error parsing rules; custom rules not applied.".to_string(),
            details: Some(err.to_string()),
            path,
            range,
        };
        config_warnings.push(message);
    }

    if let Some(warning) = project_config_warning(&config) {
        config_warnings.push(warning);
    }
    for warning in &config.startup_warnings {
        config_warnings.push(ConfigWarningNotification {
            summary: warning.clone(),
            details: None,
            path: None,
            range: None,
        });
    }
    if let Some(warning) =
        codex_core::config::system_bwrap_warning(config.permissions.permission_profile())
    {
        config_warnings.push(ConfigWarningNotification {
            summary: warning,
            details: None,
            path: None,
            range: None,
        });
    }

    let feedback = CodexFeedback::new();

    // Install a simple subscriber so `tracing` output is visible. Users can
    // control the log level with `RUST_LOG` and switch to JSON logs with
    // `LOG_FORMAT=json`.
    let stderr_fmt: StderrLogLayer = match log_format_from_env() {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_filter(EnvFilter::from_default_env())
            .boxed(),
        LogFormat::Default => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_filter(EnvFilter::from_default_env())
            .boxed(),
    };

    let feedback_layer = feedback.logger_layer();
    let feedback_metadata_layer = feedback.metadata_layer();
    let log_db = state_db.clone().map(log_db::start);
    let log_db_layer = log_db
        .clone()
        .map(|layer| layer.with_filter(Targets::new().with_default(Level::TRACE)));
    let otel_logger_layer = otel.as_ref().and_then(|o| o.logger_layer());
    let otel_tracing_layer = otel.as_ref().and_then(|o| o.tracing_layer());
    let _ = tracing_subscriber::registry()
        .with(stderr_fmt)
        .with(feedback_layer)
        .with(feedback_metadata_layer)
        .with(log_db_layer)
        .with(otel_logger_layer)
        .with(otel_tracing_layer)
        .try_init();
    for warning in &config_warnings {
        match &warning.details {
            Some(details) => error!("{} {}", warning.summary, details),
            None => error!("{}", warning.summary),
        }
    }
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let transport_shutdown_token = CancellationToken::new();
    let mut transport_accept_handles = Vec::<JoinHandle<()>>::new();

    // ── Step 2：按 transport 形态推导生命周期策略 ───────────────────────
    // stdio 是「单客户端」模式（一个子进程对端）：最后一个连接关闭即退出整个进程；
    // 也正因为它生命周期绑定父进程，不启用「信号优雅重启」（那是多客户端守护场景的需求）。
    let single_client_mode = matches!(&transport, AppServerTransport::Stdio);
    let shutdown_when_no_connections = single_client_mode;
    let graceful_signal_restart_enabled =
        runtime_options.install_shutdown_signal_handler && !single_client_mode;
    let mut app_server_client_name_rx = None;

    // ── Step 3：按 transport 启动对应 acceptor，归一为统一的 TransportEvent 流 ──
    // 四种形态各起一种接入：stdio 直连父进程 stdin/stdout；unix/ws 起监听 acceptor；
    // Off 不监听（仅 in-process 或纯远程控制）。无论哪种，入站都汇成 `transport_event_tx`。
    match &transport {
        AppServerTransport::Stdio => {
            let (stdio_client_name_tx, stdio_client_name_rx) = oneshot::channel::<String>();
            app_server_client_name_rx = Some(stdio_client_name_rx);
            start_stdio_connection(
                transport_event_tx.clone(),
                &mut transport_accept_handles,
                stdio_client_name_tx,
            )
            .await?;
        }
        AppServerTransport::UnixSocket { socket_path } => {
            let accept_handle = start_control_socket_acceptor(
                socket_path.clone(),
                transport_event_tx.clone(),
                transport_shutdown_token.clone(),
            )
            .await?;
            transport_accept_handles.push(accept_handle);
        }
        AppServerTransport::WebSocket { bind_address } => {
            let accept_handle = start_websocket_acceptor(
                *bind_address,
                transport_event_tx.clone(),
                transport_shutdown_token.clone(),
                policy_from_settings(&auth)?,
            )
            .await?;
            transport_accept_handles.push(accept_handle);
        }
        AppServerTransport::Off => {}
    }
    drop(unix_socket_startup_lock);

    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await;

    let remote_control_requested = runtime_options.remote_control_enabled;
    let remote_control_enabled = remote_control_requested && state_db.is_some();
    if remote_control_requested && state_db.is_none() {
        error!("remote control disabled because sqlite state db is unavailable");
    }
    if transport_accept_handles.is_empty() && !remote_control_enabled {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            if remote_control_requested && state_db.is_none() {
                "no transport configured; remote control disabled because sqlite state db is unavailable"
            } else {
                "no transport configured; use --listen or enable remote control"
            },
        ));
    }

    let (remote_control_accept_handle, remote_control_handle) = start_remote_control(
        RemoteControlStartConfig {
            remote_control_url: config.chatgpt_base_url.clone(),
            installation_id: installation_id.clone(),
        },
        state_db.clone(),
        auth_manager.clone(),
        transport_event_tx.clone(),
        transport_shutdown_token.clone(),
        app_server_client_name_rx,
        remote_control_enabled,
    )
    .await?;
    transport_accept_handles.push(remote_control_accept_handle);

    // ── Step 4：spawn「出站路由循环」────────────────────────────────────
    // 维护本地的 `connection_id → OutboundConnectionState` 表，只做两件事：
    // ① 消费控制面事件（Opened/Closed/DisconnectAll）维护路由表；
    // ② 消费 `OutgoingEnvelope` 把消息写到目标连接（可能是慢写，故独立于处理器循环）。
    // `biased`：优先处理控制面，确保「连接已登记」先于「向其写消息」，避免丢路由。
    let outbound_handle = tokio::spawn(async move {
        let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
        loop {
            tokio::select! {
                    biased;
                    event = outbound_control_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        match event {
                            OutboundControlEvent::Opened {
                                connection_id,
                                writer,
                                disconnect_sender,
                                initialized,
                                experimental_api_enabled,
                                opted_out_notification_methods,
                            } => {
                                outbound_connections.insert(
                                    connection_id,
                                    OutboundConnectionState::new(
                                        writer,
                                        initialized,
                                        experimental_api_enabled,
                                        opted_out_notification_methods,
                                        disconnect_sender,
                                    ),
                                );
                            }
                            OutboundControlEvent::Closed { connection_id } => {
                                outbound_connections.remove(&connection_id);
                            }
                            OutboundControlEvent::DisconnectAll => {
                                info!(
                                    "disconnecting {} outbound websocket connection(s) for graceful restart",
                                    outbound_connections.len()
                                );
                                for connection_state in outbound_connections.values() {
                                    connection_state.request_disconnect();
                                }
                                outbound_connections.clear();
                            }
                        }
                    }
                    envelope = outgoing_rx.recv() => {
                    let Some(envelope) = envelope else {
                        break;
                    };
                    route_outgoing_envelope(&mut outbound_connections, envelope).await;
                }
            }
        }
        info!("outbound router task exited (channel closed)");
    });

    // ── Step 5：spawn「处理器循环」（服务主循环）─────────────────────────
    // 先一次性装配 `MessageProcessor`（聚合全部 Processor），再进入 `tokio::select!` 主循环：
    // 处理 transport 事件（连接开/关、入站 JSON-RPC 请求/响应/通知/错误）、远程控制状态变更、
    // 线程创建广播、以及关闭信号/turn 计数变化（优雅重启 drain）。循环退出后做线程收尾。
    let processor_handle = tokio::spawn({
        let auth_manager = Arc::clone(&auth_manager);
        let analytics_events_client =
            analytics_events_client_from_config(Arc::clone(&auth_manager), &config);
        let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            analytics_events_client.clone(),
        ));
        let initialize_notification_sender = outgoing_message_sender.clone();
        let outbound_control_tx = outbound_control_tx;
        let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
            outgoing: outgoing_message_sender,
            analytics_events_client,
            arg0_paths,
            config: Arc::new(config),
            config_manager,
            environment_manager,
            feedback: feedback.clone(),
            log_db,
            state_db: state_db.clone(),
            config_warnings,
            session_source,
            auth_manager,
            installation_id,
            rpc_transport: analytics_rpc_transport(&transport),
            remote_control_handle: Some(remote_control_handle.clone()),
            plugin_startup_tasks: runtime_options.plugin_startup_tasks,
        }));
        let mut thread_created_rx = processor.thread_created_receiver();
        let mut running_turn_count_rx = processor.subscribe_running_assistant_turn_count();
        let mut connections = HashMap::<ConnectionId, ConnectionState>::new();
        let mut remote_control_status_rx = remote_control_handle.status_receiver();
        let mut remote_control_status = remote_control_status_rx.borrow().clone();
        let transport_shutdown_token = transport_shutdown_token.clone();
        async move {
            let mut listen_for_threads = true;
            let mut shutdown_state = ShutdownState::default();
            loop {
                let running_turn_count = {
                    let running_turn_count = running_turn_count_rx.borrow();
                    *running_turn_count
                };
                if matches!(
                    shutdown_state.update(running_turn_count, connections.len()),
                    ShutdownAction::Finish
                ) {
                    transport_shutdown_token.cancel();
                    let _ = outbound_control_tx
                        .send(OutboundControlEvent::DisconnectAll)
                        .await;
                    break;
                }

                tokio::select! {
                    shutdown_signal_result = shutdown_signal(), if graceful_signal_restart_enabled && !shutdown_state.forced() => {
                        let signal = match shutdown_signal_result {
                            Ok(signal) => signal,
                            Err(err) => {
                                warn!("failed to listen for shutdown signal during graceful restart drain: {err}");
                                continue;
                            }
                        };
                        let running_turn_count = *running_turn_count_rx.borrow();
                        shutdown_state.on_signal(signal, connections.len(), running_turn_count);
                    }
                    changed = running_turn_count_rx.changed(), if graceful_signal_restart_enabled && shutdown_state.requested() => {
                        if changed.is_err() {
                            warn!("running-turn watcher closed during graceful restart drain");
                        }
                    }
                    event = transport_event_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        match event {
                            TransportEvent::ConnectionOpened {
                                connection_id,
                                origin,
                                writer,
                                disconnect_sender,
                            } => {
                                // 新连接：创建三份「处理器侧 ↔ 出站侧」共享标志（已初始化 /
                                // 启用实验 API / 退订通知方法），先把写端 + 标志 `Opened` 给出站
                                // 循环登记，再在处理器侧记一份 ConnectionState 持同样的标志。
                                // 两侧持同一组 `Arc`，状态更新对出站路由即时可见。
                                let outbound_initialized = Arc::new(AtomicBool::new(false));
                                let outbound_experimental_api_enabled =
                                    Arc::new(AtomicBool::new(false));
                                let outbound_opted_out_notification_methods =
                                    Arc::new(RwLock::new(HashSet::new()));
                                if outbound_control_tx
                                    .send(OutboundControlEvent::Opened {
                                        connection_id,
                                        writer,
                                        disconnect_sender,
                                        initialized: Arc::clone(&outbound_initialized),
                                        experimental_api_enabled: Arc::clone(
                                            &outbound_experimental_api_enabled,
                                        ),
                                        opted_out_notification_methods: Arc::clone(
                                            &outbound_opted_out_notification_methods,
                                        ),
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                connections.insert(
                                    connection_id,
                                    ConnectionState::new(
                                        origin,
                                        outbound_initialized,
                                        outbound_experimental_api_enabled,
                                        outbound_opted_out_notification_methods,
                                    ),
                                );
                            }
                            TransportEvent::ConnectionClosed { connection_id } => {
                                let Some(connection_state) = connections.remove(&connection_id) else {
                                    continue;
                                };
                                if outbound_control_tx
                                    .send(OutboundControlEvent::Closed { connection_id })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                processor.connection_closed(connection_id, &connection_state.session).await;
                                if shutdown_when_no_connections && connections.is_empty() {
                                    break;
                                }
                            }
                            TransportEvent::IncomingMessage { connection_id, message } => {
                                match message {
                                    JSONRPCMessage::Request(request) => {
                                        let Some(connection_state) = connections.get_mut(&connection_id) else {
                                            warn!("dropping request from unknown connection: {connection_id:?}");
                                            continue;
                                        };
                                        let was_initialized =
                                            connection_state.session.initialized();
                                        processor
                                            .process_request(
                                                connection_id,
                                                request,
                                                &transport,
                                                Arc::clone(&connection_state.session),
                                            )
                                            .await;
                                        let opted_out_notification_methods_snapshot = connection_state
                                            .session
                                            .opted_out_notification_methods();
                                        let experimental_api_enabled =
                                            connection_state.session.experimental_api_enabled();
                                        let is_initialized = connection_state.session.initialized();
                                        if let Ok(mut opted_out_notification_methods) = connection_state
                                            .outbound_opted_out_notification_methods
                                            .write()
                                        {
                                            *opted_out_notification_methods =
                                                opted_out_notification_methods_snapshot;
                                        } else {
                                            warn!(
                                                "failed to update outbound opted-out notifications"
                                            );
                                        }
                                        connection_state
                                            .outbound_experimental_api_enabled
                                            .store(
                                                experimental_api_enabled,
                                                std::sync::atomic::Ordering::Release,
                                            );
                                        // 这次请求恰好让连接从「未初始化」跃迁到「已初始化」
                                        // （即刚处理完 Initialize 握手）：此处才把连接级初始化
                                        // 通知 + 远程控制状态推给这条连接，随后标记 outbound 就绪。
                                        // 顺序很关键——先送初始通知、后置就绪位，避免下游误判
                                        // 「已就绪却还没收到初始状态」。对应 process_request 里传
                                        // `outbound_initialized = None` 的注释：就绪由这里收尾。
                                        if !was_initialized && is_initialized {
                                            processor
                                                .send_initialize_notifications_to_connection(
                                                    connection_id,
                                                )
                                                .await;
                                            initialize_notification_sender
                                                .send_server_notification_to_connections(
                                                    &[connection_id],
                                                    ServerNotification::RemoteControlStatusChanged(
                                                        remote_control_status.clone(),
                                                    ),
                                                )
                                                .await;
                                            processor
                                                .connection_initialized(
                                                    connection_id,
                                                    connection_state
                                                        .session
                                                        .request_attestation(),
                                                )
                                                .await;
                                            connection_state
                                                .outbound_initialized
                                                .store(true, std::sync::atomic::Ordering::Release);
                                        }
                                    }
                                    JSONRPCMessage::Response(response) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping response from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor.process_response(response).await;
                                    }
                                    JSONRPCMessage::Notification(notification) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping notification from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor.process_notification(notification).await;
                                    }
                                    JSONRPCMessage::Error(err) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping error from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor.process_error(err).await;
                                    }
                                }
                            }
                        }
                    }
                    changed = remote_control_status_rx.changed() => {
                        if changed.is_err() {
                            continue;
                        }
                        let status = remote_control_status_rx.borrow().clone();
                        if remote_control_status == status {
                            continue;
                        }
                        remote_control_status = status.clone();
                        let notification = ServerNotification::RemoteControlStatusChanged(status);
                        initialize_notification_sender
                            .send_server_notification(notification)
                            .await;
                    }
                    created = thread_created_rx.recv(), if listen_for_threads => {
                        match created {
                            Ok(thread_id) => {
                                let mut initialized_connection_ids = Vec::new();
                                for (connection_id, connection_state) in &connections {
                                    if connection_state.session.initialized() {
                                        initialized_connection_ids.push(*connection_id);
                                    }
                                }
                                processor
                                    .try_attach_thread_listener(
                                        thread_id,
                                        initialized_connection_ids,
                                    )
                                    .await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                // TODO(jif) handle lag.
                                // Assumes thread creation volume is low enough that lag never happens.
                                // If it does, we log and continue without resyncing to avoid attaching
                                // listeners for threads that should remain unsubscribed.
                                warn!("thread_created receiver lagged; skipping resync");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                listen_for_threads = false;
                            }
                        }
                    }
                }
            }

            // 收尾：仅在「非强制」退出时做有序清理——等各连接 rpc_gate 排空、跑完后台任务、
            // 关闭线程。强制退出（第二次信号）则跳过这些等待，尽快让位重启。
            if !shutdown_state.forced() {
                futures::future::join_all(
                    connections
                        .values()
                        .map(|connection_state| connection_state.session.rpc_gate.shutdown()),
                )
                .await;
                processor.drain_background_tasks().await;
                processor.shutdown_threads().await;
            }
            info!("processor task exited (channel closed)");
        }
    });

    // ── Step 6：等两循环退出并做进程级收尾 ─────────────────────────────
    // 先 drop 持有的 `transport_event_tx`：否则只要这一份发送端还在，处理器循环里的
    // `transport_event_rx.recv()` 永不返回 `None`，循环无法因「信道关闭」而退出。
    drop(transport_event_tx);

    let _ = processor_handle.await;
    let _ = outbound_handle.await;

    transport_shutdown_token.cancel();
    for handle in transport_accept_handles {
        let _ = handle.await;
    }

    if let Some(otel) = otel {
        otel.shutdown();
    }

    Ok(())
}

fn analytics_rpc_transport(transport: &AppServerTransport) -> AppServerRpcTransport {
    match transport {
        AppServerTransport::Stdio => AppServerRpcTransport::Stdio,
        AppServerTransport::UnixSocket { .. }
        | AppServerTransport::WebSocket { .. }
        | AppServerTransport::Off => AppServerRpcTransport::Websocket,
    }
}

#[cfg(test)]
mod tests {
    use super::LogFormat;
    use pretty_assertions::assert_eq;

    #[test]
    fn log_format_from_env_value_matches_json_values_case_insensitively() {
        assert_eq!(LogFormat::from_env_value(Some("json")), LogFormat::Json);
        assert_eq!(LogFormat::from_env_value(Some("JSON")), LogFormat::Json);
        assert_eq!(LogFormat::from_env_value(Some("  Json  ")), LogFormat::Json);
    }

    #[test]
    fn log_format_from_env_value_defaults_for_non_json_values() {
        assert_eq!(
            LogFormat::from_env_value(/*value*/ None),
            LogFormat::Default
        );
        assert_eq!(LogFormat::from_env_value(Some("")), LogFormat::Default);
        assert_eq!(LogFormat::from_env_value(Some("text")), LogFormat::Default);
        assert_eq!(LogFormat::from_env_value(Some("jsonl")), LogFormat::Default);
    }
}
